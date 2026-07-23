//! Host-to-host RPC over HTTP using `reqwest` (MIT OR Apache-2.0).
//!
//! Requests are only ever sent to a peer `rpc_addr` that came from the fleet
//! registry (never a user-supplied URL), redirects are disabled, and every
//! call carries a short-lived, replay-protected request HMAC. The shared key is
//! never transmitted, so a poisoned fleet RPC address cannot exfiltrate a
//! cluster-wide bearer credential.

use axum::{
    body::Body,
    http::{
        header::{HeaderName, HeaderValue, CONNECTION, SEC_WEBSOCKET_PROTOCOL},
        HeaderMap, Request, Response, Uri,
    },
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, io, str::FromStr, time::Duration};
use tarit_types::{CreateVmRequest, EgressUpdateRequest, OrchError, VmRecord};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async, tungstenite::client::IntoClientRequest, MaybeTlsStream, WebSocketStream,
};
use uuid::Uuid;

use crate::{cluster::PeerTarget, config::ApiIdentity};

pub struct PeerClient {
    secret: String,
    source_host_id: String,
    allow_insecure_http: bool,
    http: Client,
    stream_http: reqwest::Client,
}

pub type PeerWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const IDENTITY_SIGNATURE_VERSION: &str = "tarit-peer-identity-v1";
const REQUEST_SIGNATURE_VERSION: &str = "tarit-peer-request-v1";
const STREAMING_PAYLOAD: &str = "STREAMING-UNSIGNED-PAYLOAD";

struct RequestSignature<'a> {
    target: &'a PeerTarget,
    method: &'a str,
    canonical_path: &'a str,
    payload_hash: &'a str,
}

pub struct ShareWebSocketRequest<'a> {
    pub request_uri: &'a Uri,
    pub headers: &'a HeaderMap,
    pub protocols: &'a [String],
    pub trusted_proto: &'static str,
}

#[derive(Serialize)]
struct RemoteExecRequest {
    command: String,
    timeout_ms: u64,
}

#[derive(Deserialize)]
struct RemoteExecResponse {
    exit_code: i32,
    stdout: String,
    stderr: String,
    duration_ms: u64,
}

#[derive(Serialize)]
struct RemoteSnapshotRequest {
    diff: bool,
}

impl PeerClient {
    #[cfg(test)]
    pub fn new(secret: String) -> Self {
        Self::new_for_host(secret, true, "test-source".into())
    }

    pub fn new_for_host(secret: String, allow_insecure_http: bool, source_host_id: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(5))
            // SSRF hardening: never follow redirects to an attacker-chosen host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let stream_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            // The owner-side proxy enforces per-direction idle timeouts.
            // Deliberately keep reqwest's default of no total deadline so
            // healthy long-lived streams (SSE/downloads) are not truncated.
            .build()
            .expect("streaming reqwest client");
        Self {
            secret,
            source_host_id,
            allow_insecure_http,
            http,
            stream_http,
        }
    }

    fn validate_rpc_addr(&self, rpc_addr: &str) -> Result<(), OrchError> {
        let url = reqwest::Url::parse(rpc_addr)
            .map_err(|error| OrchError::Internal(format!("invalid peer RPC address: {error}")))?;
        let valid_origin = matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.port_or_known_default().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(url.path(), "" | "/");
        if !valid_origin {
            return Err(OrchError::Internal(
                "peer RPC address must be a normalized HTTP(S) origin".into(),
            ));
        }
        if url.scheme() != "https" && !self.allow_insecure_http {
            return Err(OrchError::Internal(
                "refusing plaintext peer RPC transport".into(),
            ));
        }
        Ok(())
    }

    fn peer_url(rpc_addr: &str, path: &str) -> String {
        let base = rpc_addr.trim_end_matches('/');
        format!("{base}{path}")
    }

    fn payload_hash(body: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(body))
    }

    fn canonical_path(url: &reqwest::Url) -> String {
        match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        }
    }

    fn request_signature(
        &self,
        method: &str,
        canonical_path: &str,
        payload_hash: &str,
        issued_at: i64,
        nonce: &str,
        target_host_id: &str,
    ) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_bytes())
            .expect("HMAC accepts arbitrary key lengths");
        for component in [
            REQUEST_SIGNATURE_VERSION,
            method,
            canonical_path,
            payload_hash,
            &issued_at.to_string(),
            nonce,
            &self.source_host_id,
            target_host_id,
        ] {
            mac.update(component.as_bytes());
            mac.update(b"\n");
        }
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    fn with_request_signature(
        &self,
        request: reqwest::blocking::RequestBuilder,
        target: &PeerTarget,
        method: &str,
        canonical_path: &str,
        payload_hash: &str,
    ) -> reqwest::blocking::RequestBuilder {
        let issued_at = Utc::now().timestamp();
        let nonce = Uuid::new_v4().to_string();
        request
            .header("X-Tarit-Peer-Version", REQUEST_SIGNATURE_VERSION)
            .header("X-Tarit-Peer-Source", &self.source_host_id)
            .header("X-Tarit-Peer-Target", &target.host_id)
            .header("X-Tarit-Peer-Timestamp", issued_at)
            .header("X-Tarit-Peer-Nonce", &nonce)
            .header("X-Tarit-Peer-Body-SHA256", payload_hash)
            .header(
                "X-Tarit-Peer-Signature",
                self.request_signature(
                    method,
                    canonical_path,
                    payload_hash,
                    issued_at,
                    &nonce,
                    &target.host_id,
                ),
            )
    }

    fn insert_request_signature_headers(
        &self,
        headers: &mut HeaderMap,
        target: &PeerTarget,
        method: &str,
        canonical_path: &str,
        payload_hash: &str,
    ) -> Result<(), OrchError> {
        let issued_at = Utc::now().timestamp();
        let nonce = Uuid::new_v4().to_string();
        let signature = self.request_signature(
            method,
            canonical_path,
            payload_hash,
            issued_at,
            &nonce,
            &target.host_id,
        );
        for (name, value) in [
            ("x-tarit-peer-version", REQUEST_SIGNATURE_VERSION),
            ("x-tarit-peer-source", self.source_host_id.as_str()),
            ("x-tarit-peer-target", target.host_id.as_str()),
            ("x-tarit-peer-nonce", nonce.as_str()),
            ("x-tarit-peer-body-sha256", payload_hash),
            ("x-tarit-peer-signature", signature.as_str()),
        ] {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_str(value)
                    .map_err(|_| OrchError::Internal("invalid peer HMAC header".into()))?,
            );
        }
        headers.insert(
            HeaderName::from_static("x-tarit-peer-timestamp"),
            HeaderValue::from_str(&issued_at.to_string())
                .map_err(|_| OrchError::Internal("invalid peer HMAC timestamp".into()))?,
        );
        Ok(())
    }

    fn share_url(rpc_addr: &str, share_id: Uuid, request_uri: &Uri) -> Result<String, OrchError> {
        if request_uri.scheme().is_some() || request_uri.authority().is_some() {
            return Err(OrchError::BadRequest(
                "share request URI must be origin-form".into(),
            ));
        }
        let path_and_query = request_uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        if !path_and_query.starts_with('/') {
            return Err(OrchError::BadRequest(
                "share request path must start with '/'".into(),
            ));
        }
        if request_uri.path().contains('\\')
            || request_uri.path().split('/').any(is_dot_path_segment)
        {
            return Err(OrchError::BadRequest(
                "share request path contains a dot segment".into(),
            ));
        }
        let route = format!("/internal/v1/shares/{share_id}");
        let peer_path = if request_uri.path() == "/" {
            route
        } else {
            format!("{route}{path_and_query}")
        };
        let peer_path = match request_uri.query() {
            Some(query) if request_uri.path() == "/" => format!("{peer_path}?{query}"),
            _ => peer_path,
        };
        let url = Self::peer_url(rpc_addr, &peer_path);
        let base = reqwest::Url::parse(rpc_addr)
            .map_err(|error| OrchError::Internal(format!("invalid peer RPC address: {error}")))?;
        if base.query().is_some() || base.fragment().is_some() {
            return Err(OrchError::Internal(
                "peer RPC address cannot contain a query or fragment".into(),
            ));
        }
        let parsed = reqwest::Url::parse(&url)
            .map_err(|error| OrchError::Internal(format!("invalid peer RPC address: {error}")))?;
        let expected_prefix = format!(
            "{}/internal/v1/shares/{share_id}",
            base.path().trim_end_matches('/')
        );
        if parsed.path() != expected_prefix
            && !parsed
                .path()
                .strip_prefix(&expected_prefix)
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            return Err(OrchError::BadRequest(
                "share request path escaped the peer route".into(),
            ));
        }
        Ok(url)
    }

    /// Forward an already-authorized share request to the trusted VM owner.
    ///
    /// `rpc_addr` is supplied by the fleet ownership resolver, never by the
    /// external request. The returned body is a live stream tied to the peer
    /// response, so dropping it cancels the peer read.
    pub async fn proxy_share_http(
        &self,
        target: &PeerTarget,
        share_id: Uuid,
        identity: &ApiIdentity,
        request: Request<Body>,
        trusted_proto: &'static str,
    ) -> Result<Response<Body>, OrchError> {
        self.validate_rpc_addr(&target.rpc_addr)?;
        let (parts, body) = request.into_parts();
        let url = Self::share_url(&target.rpc_addr, share_id, &parts.uri)?;
        let parsed_url = reqwest::Url::parse(&url)
            .map_err(|error| OrchError::Internal(format!("invalid peer share URL: {error}")))?;
        let canonical_path = Self::canonical_path(&parsed_url);
        let headers = self.share_headers(
            &parts.headers,
            identity,
            trusted_proto,
            RequestSignature {
                target,
                method: parts.method.as_str(),
                canonical_path: &canonical_path,
                payload_hash: STREAMING_PAYLOAD,
            },
        )?;
        let response = self
            .stream_http
            .request(parts.method, url)
            .headers(headers)
            .body(reqwest::Body::wrap_stream(body.into_data_stream().map(
                |result| result.map_err(|error| io::Error::other(error.to_string())),
            )))
            .send()
            .await
            .map_err(|error| OrchError::Internal(format!("peer share request: {error}")))?;
        let status = response.status();
        let headers = Self::sanitized_share_response_headers(response.headers());
        let mut builder = Response::builder().status(status);
        *builder
            .headers_mut()
            .ok_or_else(|| OrchError::Internal("peer share response headers".into()))? = headers;
        builder
            .body(Body::from_stream(response.bytes_stream().map(|result| {
                result.map_err(|error| io::Error::other(error.to_string()))
            })))
            .map_err(|error| OrchError::Internal(format!("peer share response: {error}")))
    }

    /// Open the peer half of a share WebSocket bridge. The outer gateway owns
    /// the client upgrade and bridges its frames to this authenticated stream.
    pub async fn connect_share_websocket(
        &self,
        target: &PeerTarget,
        share_id: Uuid,
        identity: &ApiIdentity,
        request: ShareWebSocketRequest<'_>,
    ) -> Result<(PeerWebSocket, Option<String>), OrchError> {
        self.validate_rpc_addr(&target.rpc_addr)?;
        let ShareWebSocketRequest {
            request_uri,
            headers,
            protocols,
            trusted_proto,
        } = request;
        let url = Self::share_url(&target.rpc_addr, share_id, request_uri)?;
        let mut url = reqwest::Url::parse(&url)
            .map_err(|error| OrchError::Internal(format!("invalid peer WebSocket URL: {error}")))?;
        match url.scheme() {
            "http" => {
                url.set_scheme("ws").expect("ws is a valid URL scheme");
            }
            "https" => {
                url.set_scheme("wss").expect("wss is a valid URL scheme");
            }
            scheme => {
                return Err(OrchError::Internal(format!(
                    "peer WebSocket URL has unsupported scheme {scheme}"
                )));
            }
        }
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|error| OrchError::Internal(format!("peer WebSocket request: {error}")))?;
        let canonical_path = Self::canonical_path(&url);
        let empty_hash = Self::payload_hash(&[]);
        let mut peer_headers = self.share_headers(
            headers,
            identity,
            trusted_proto,
            RequestSignature {
                target,
                method: "GET",
                canonical_path: &canonical_path,
                payload_hash: &empty_hash,
            },
        )?;
        let handshake_headers = peer_headers
            .keys()
            .filter(|name| name.as_str().starts_with("sec-websocket-"))
            .cloned()
            .collect::<Vec<_>>();
        for name in handshake_headers {
            peer_headers.remove(name);
        }
        for (name, value) in &peer_headers {
            request.headers_mut().append(name.clone(), value.clone());
        }
        if !protocols.is_empty() {
            request.headers_mut().insert(
                SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_str(&protocols.join(", "))
                    .map_err(|_| OrchError::BadRequest("invalid WebSocket protocols".into()))?,
            );
        }
        let (socket, response) = connect_async(request)
            .await
            .map_err(|error| OrchError::Internal(format!("peer WebSocket connect: {error}")))?;
        let protocol = response
            .headers()
            .get(SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Ok((socket, protocol))
    }

    fn share_headers(
        &self,
        headers: &HeaderMap,
        identity: &ApiIdentity,
        trusted_proto: &'static str,
        signature: RequestSignature<'_>,
    ) -> Result<HeaderMap, OrchError> {
        let connection_headers = Self::connection_headers(headers);
        let mut sanitized = HeaderMap::new();
        for (name, value) in headers {
            if Self::is_share_hop_header(name, &connection_headers) {
                continue;
            }
            sanitized.append(name.clone(), value.clone());
        }
        sanitized.insert("x-forwarded-proto", HeaderValue::from_static(trusted_proto));
        self.insert_request_signature_headers(
            &mut sanitized,
            signature.target,
            signature.method,
            signature.canonical_path,
            signature.payload_hash,
        )?;
        self.insert_signed_identity_headers(&mut sanitized, identity)?;
        Ok(sanitized)
    }

    fn post_json<B: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        target: &PeerTarget,
        path: &str,
        body: &B,
        identity: Option<&ApiIdentity>,
        what: &str,
    ) -> Result<R, OrchError> {
        self.validate_rpc_addr(&target.rpc_addr)?;
        let body = serde_json::to_vec(body)
            .map_err(|e| OrchError::Internal(format!("peer {what} encode: {e}")))?;
        let payload_hash = Self::payload_hash(&body);
        let req = self
            .http
            .post(Self::peer_url(&target.rpc_addr, path))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        let req = self.with_request_signature(req, target, "POST", path, &payload_hash);
        let resp = self
            .with_identity(req, identity)
            .send()
            .map_err(|e| OrchError::Internal(format!("peer {what} request: {e}")))?;
        Self::decode(resp, what)
    }

    fn empty_request(
        &self,
        target: &PeerTarget,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::blocking::RequestBuilder, OrchError> {
        self.validate_rpc_addr(&target.rpc_addr)?;
        let payload_hash = Self::payload_hash(&[]);
        let request = self
            .http
            .request(method.clone(), Self::peer_url(&target.rpc_addr, path));
        Ok(self.with_request_signature(request, target, method.as_str(), path, &payload_hash))
    }

    fn decode<R: for<'de> Deserialize<'de>>(
        resp: reqwest::blocking::Response,
        what: &str,
    ) -> Result<R, OrchError> {
        let status = resp.status();
        if !status.is_success() {
            let retry_after_secs = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1);
            let body = resp.text().unwrap_or_default();
            if status.as_u16() == 429 {
                return Err(OrchError::Overloaded {
                    message: format!("peer {what}: {body}"),
                    retry_after_secs,
                });
            }
            if status.as_u16() == 409 {
                return Err(OrchError::Conflict(format!("peer {what}: {body}")));
            }
            if status.as_u16() == 404 {
                return Err(OrchError::NotFound(format!("peer {what}: {body}")));
            }
            if status.as_u16() == 403 {
                return Err(OrchError::Forbidden(format!("peer {what}: {body}")));
            }
            if status.as_u16() == 401 {
                return Err(OrchError::Unauthorized);
            }
            return Err(OrchError::Internal(format!(
                "peer {what} HTTP {status}: {body}"
            )));
        }
        resp.json::<R>()
            .map_err(|e| OrchError::Internal(format!("peer {what} decode: {e}")))
    }

    /// Place a new VM on a peer (cross-node placement when local is at capacity).
    pub fn create_remote(
        &self,
        target: &PeerTarget,
        req: &CreateVmRequest,
        identity: &ApiIdentity,
    ) -> Result<VmRecord, OrchError> {
        self.post_json(target, "/internal/v1/vms", req, Some(identity), "create")
    }

    /// Restore a snapshot on the peer that holds its file (node-local restore).
    pub fn restore_remote(
        &self,
        target: &PeerTarget,
        req: &tarit_types::RestoreRequest,
        identity: &ApiIdentity,
    ) -> Result<VmRecord, OrchError> {
        self.post_json(
            target,
            "/internal/v1/restore",
            req,
            Some(identity),
            "restore",
        )
    }

    pub fn exec_remote(
        &self,
        target: &PeerTarget,
        vm_id: Uuid,
        command: &str,
        timeout_ms: u64,
        identity: &ApiIdentity,
    ) -> Result<(i32, String, String, u64), OrchError> {
        let body: RemoteExecResponse = self.post_json(
            target,
            &format!("/internal/v1/vms/{vm_id}/exec"),
            &RemoteExecRequest {
                command: command.to_string(),
                timeout_ms,
            },
            Some(identity),
            "exec",
        )?;
        Ok((body.exit_code, body.stdout, body.stderr, body.duration_ms))
    }

    pub fn pause_remote(
        &self,
        target: &PeerTarget,
        vm_id: Uuid,
        identity: &ApiIdentity,
    ) -> Result<VmRecord, OrchError> {
        self.post_json(
            target,
            &format!("/internal/v1/vms/{vm_id}/pause"),
            &serde_json::json!({}),
            Some(identity),
            "pause",
        )
    }

    pub fn suspend_remote(
        &self,
        target: &PeerTarget,
        vm_id: Uuid,
        identity: &ApiIdentity,
    ) -> Result<VmRecord, OrchError> {
        self.post_json(
            target,
            &format!("/internal/v1/vms/{vm_id}/suspend"),
            &serde_json::json!({}),
            Some(identity),
            "suspend",
        )
    }

    pub fn resume_remote(
        &self,
        target: &PeerTarget,
        vm_id: Uuid,
        identity: &ApiIdentity,
    ) -> Result<VmRecord, OrchError> {
        self.post_json(
            target,
            &format!("/internal/v1/vms/{vm_id}/resume"),
            &serde_json::json!({}),
            Some(identity),
            "resume",
        )
    }

    pub fn snapshot_remote(
        &self,
        target: &PeerTarget,
        vm_id: Uuid,
        diff: bool,
        identity: &ApiIdentity,
    ) -> Result<serde_json::Value, OrchError> {
        self.post_json(
            target,
            &format!("/internal/v1/vms/{vm_id}/snapshot"),
            &RemoteSnapshotRequest { diff },
            Some(identity),
            "snapshot",
        )
    }

    pub fn egress_remote(
        &self,
        target: &PeerTarget,
        vm_id: Uuid,
        body: &EgressUpdateRequest,
        identity: &ApiIdentity,
    ) -> Result<serde_json::Value, OrchError> {
        self.validate_rpc_addr(&target.rpc_addr)?;
        let path = format!("/internal/v1/vms/{vm_id}/egress");
        let encoded = serde_json::to_vec(body)
            .map_err(|e| OrchError::Internal(format!("peer egress encode: {e}")))?;
        let payload_hash = Self::payload_hash(&encoded);
        let req = self
            .http
            .patch(Self::peer_url(&target.rpc_addr, &path))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(encoded);
        let req = self.with_request_signature(req, target, "PATCH", &path, &payload_hash);
        let resp = self
            .with_identity(req, Some(identity))
            .send()
            .map_err(|e| OrchError::Internal(format!("peer egress request: {e}")))?;
        Self::decode(resp, "egress")
    }

    pub fn get_remote(
        &self,
        target: &PeerTarget,
        vm_id: Uuid,
        identity: &ApiIdentity,
    ) -> Result<VmRecord, OrchError> {
        let path = format!("/internal/v1/vms/{vm_id}");
        let req = self.empty_request(target, reqwest::Method::GET, &path)?;
        let resp = self
            .with_identity(req, Some(identity))
            .send()
            .map_err(|e| OrchError::Internal(format!("peer get request: {e}")))?;
        Self::decode(resp, "get")
    }

    pub fn status_remote(
        &self,
        target: &PeerTarget,
        vm_id: Uuid,
        identity: &ApiIdentity,
    ) -> Result<serde_json::Value, OrchError> {
        let path = format!("/internal/v1/vms/{vm_id}/status");
        let req = self.empty_request(target, reqwest::Method::GET, &path)?;
        let resp = self
            .with_identity(req, Some(identity))
            .send()
            .map_err(|e| OrchError::Internal(format!("peer status request: {e}")))?;
        Self::decode(resp, "status")
    }

    pub fn stop_remote(
        &self,
        target: &PeerTarget,
        vm_id: Uuid,
        identity: &ApiIdentity,
    ) -> Result<(), OrchError> {
        let path = format!("/internal/v1/vms/{vm_id}");
        let req = self.empty_request(target, reqwest::Method::DELETE, &path)?;
        let resp = self
            .with_identity(req, Some(identity))
            .send()
            .map_err(|e| OrchError::Internal(format!("peer stop request: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            if status.as_u16() == 404 {
                return Err(OrchError::NotFound("peer stop: vm not found".into()));
            }
            if status.as_u16() == 403 {
                return Err(OrchError::Forbidden("peer stop: forbidden".into()));
            }
            return Err(OrchError::Internal(format!("peer stop HTTP {status}")));
        }
        Ok(())
    }

    fn with_identity(
        &self,
        req: reqwest::blocking::RequestBuilder,
        identity: Option<&ApiIdentity>,
    ) -> reqwest::blocking::RequestBuilder {
        if let Some(identity) = identity {
            let issued_at = Utc::now().timestamp();
            let nonce = Uuid::new_v4().to_string();
            req.header("X-Tarit-Tenant", &identity.tenant)
                .header("X-Tarit-Role", identity.role.as_str())
                .header("X-Tarit-Api-Key-Id", &identity.api_key_id)
                .header("X-Tarit-Identity-Timestamp", issued_at)
                .header("X-Tarit-Identity-Nonce", &nonce)
                .header(
                    "X-Tarit-Identity-Signature",
                    self.identity_signature(identity, issued_at, &nonce),
                )
        } else {
            req
        }
    }

    fn insert_signed_identity_headers(
        &self,
        headers: &mut HeaderMap,
        identity: &ApiIdentity,
    ) -> Result<(), OrchError> {
        let issued_at = Utc::now().timestamp();
        let nonce = Uuid::new_v4().to_string();
        headers.insert(
            "x-tarit-tenant",
            HeaderValue::from_str(&identity.tenant)
                .map_err(|_| OrchError::Internal("invalid peer tenant header".into()))?,
        );
        headers.insert(
            "x-tarit-role",
            HeaderValue::from_static(identity.role.as_str()),
        );
        headers.insert(
            "x-tarit-api-key-id",
            HeaderValue::from_str(&identity.api_key_id)
                .map_err(|_| OrchError::Internal("invalid peer identity header".into()))?,
        );
        headers.insert(
            "x-tarit-identity-timestamp",
            HeaderValue::from_str(&issued_at.to_string())
                .map_err(|_| OrchError::Internal("invalid peer identity timestamp".into()))?,
        );
        headers.insert(
            "x-tarit-identity-nonce",
            HeaderValue::from_str(&nonce)
                .map_err(|_| OrchError::Internal("invalid peer identity nonce".into()))?,
        );
        headers.insert(
            "x-tarit-identity-signature",
            HeaderValue::from_str(&self.identity_signature(identity, issued_at, &nonce))
                .map_err(|_| OrchError::Internal("invalid peer identity signature".into()))?,
        );
        Ok(())
    }

    fn identity_signature(&self, identity: &ApiIdentity, issued_at: i64, nonce: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_bytes())
            .expect("HMAC accepts arbitrary key lengths");
        mac.update(IDENTITY_SIGNATURE_VERSION.as_bytes());
        mac.update(b"\n");
        mac.update(self.source_host_id.as_bytes());
        mac.update(b"\n");
        mac.update(issued_at.to_string().as_bytes());
        mac.update(b"\n");
        mac.update(nonce.as_bytes());
        mac.update(b"\n");
        mac.update(identity.tenant.as_bytes());
        mac.update(b"\n");
        mac.update(identity.role.as_str().as_bytes());
        mac.update(b"\n");
        mac.update(identity.api_key_id.as_bytes());
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    fn connection_headers(headers: &HeaderMap) -> HashSet<HeaderName> {
        headers
            .get_all(CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .filter_map(|value| HeaderName::from_str(value.trim()).ok())
            .collect()
    }

    fn is_share_hop_header(name: &HeaderName, connection_headers: &HashSet<HeaderName>) -> bool {
        connection_headers.contains(name)
            || matches!(
                name.as_str(),
                "connection"
                    | "keep-alive"
                    | "proxy-connection"
                    | "proxy-authenticate"
                    | "proxy-authorization"
                    | "te"
                    | "trailer"
                    | "transfer-encoding"
                    | "upgrade"
                    | "host"
                    | "forwarded"
                    | "x-real-ip"
                    | "x-api-key"
                    | "x-peer-secret"
                    | "x-tarit-share-token"
            )
            || name.as_str().starts_with("x-forwarded-")
            || name.as_str().starts_with("x-tarit-")
    }

    fn sanitized_share_response_headers(headers: &HeaderMap) -> HeaderMap {
        let connection_headers = Self::connection_headers(headers);
        headers
            .iter()
            .filter(|(name, _)| {
                !connection_headers.contains(*name)
                    && !matches!(
                        name.as_str(),
                        "connection"
                            | "keep-alive"
                            | "proxy-connection"
                            | "proxy-authenticate"
                            | "proxy-authorization"
                            | "te"
                            | "trailer"
                            | "transfer-encoding"
                            | "upgrade"
                            | "forwarded"
                            | "x-real-ip"
                    )
                    && !name.as_str().starts_with("x-forwarded-")
                    && !name.as_str().starts_with("x-tarit-")
            })
            .fold(HeaderMap::new(), |mut sanitized, (name, value)| {
                sanitized.append(name.clone(), value.clone());
                sanitized
            })
    }
}

fn is_dot_path_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let Some(high) = bytes.get(index + 1).and_then(|byte| hex_value(*byte)) else {
                return true;
            };
            let Some(low) = bytes.get(index + 2).and_then(|byte| hex_value(*byte)) else {
                return true;
            };
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    matches!(decoded.as_slice(), b"." | b"..")
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiRole;

    #[test]
    fn share_url_rejects_backslash_traversal() {
        let uri = r"/\..\..\vms".parse::<Uri>().unwrap();

        assert!(PeerClient::share_url("http://127.0.0.1:8080", Uuid::new_v4(), &uri).is_err());
    }

    #[test]
    fn share_url_uses_the_exact_root_route_and_preserves_queries() {
        let id = Uuid::nil();

        assert_eq!(
            PeerClient::share_url("http://owner.example/", id, &"/".parse::<Uri>().unwrap())
                .unwrap(),
            format!("http://owner.example/internal/v1/shares/{id}")
        );
        assert_eq!(
            PeerClient::share_url(
                "http://owner.example/",
                id,
                &"/?x=preserve".parse::<Uri>().unwrap(),
            )
            .unwrap(),
            format!("http://owner.example/internal/v1/shares/{id}?x=preserve")
        );
        assert_eq!(
            PeerClient::share_url(
                "http://owner.example/",
                id,
                &"/nested/path?x=preserve".parse::<Uri>().unwrap(),
            )
            .unwrap(),
            format!("http://owner.example/internal/v1/shares/{id}/nested/path?x=preserve")
        );
    }

    #[test]
    fn share_request_sanitization_strips_client_api_key() {
        let client = PeerClient::new("peer-secret".into());
        let identity = ApiIdentity {
            tenant: "tenant-a".into(),
            role: ApiRole::User,
            max_vms: None,
            api_key_id: "key-id".into(),
        };
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("client-secret"));
        headers.insert("accept", HeaderValue::from_static("text/plain"));

        let target = PeerTarget {
            host_id: "owner".into(),
            rpc_addr: "https://owner.example".into(),
        };
        let sanitized = client
            .share_headers(
                &headers,
                &identity,
                "https",
                RequestSignature {
                    target: &target,
                    method: "GET",
                    canonical_path: "/internal/v1/shares/test",
                    payload_hash: STREAMING_PAYLOAD,
                },
            )
            .unwrap();

        assert!(!sanitized.contains_key("x-api-key"));
        assert!(!sanitized.contains_key("x-peer-secret"));
        assert!(!sanitized
            .values()
            .any(|value| value.as_bytes() == b"peer-secret"));
        assert!(sanitized.contains_key("x-tarit-peer-signature"));
        assert_eq!(sanitized.get("accept").unwrap(), "text/plain");
    }

    #[test]
    fn identity_signature_is_bound_to_source_host() {
        let identity = ApiIdentity {
            tenant: "tenant-a".into(),
            role: ApiRole::User,
            max_vms: None,
            api_key_id: "key-id".into(),
        };
        let a = PeerClient::new_for_host("peer-secret".into(), true, "node-a".into());
        let b = PeerClient::new_for_host("peer-secret".into(), true, "node-b".into());
        assert_ne!(
            a.identity_signature(&identity, 123, "nonce"),
            b.identity_signature(&identity, 123, "nonce")
        );
    }
}
