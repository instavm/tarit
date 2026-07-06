//! Host-to-host RPC over HTTP using `reqwest` (MIT OR Apache-2.0).
//!
//! Requests are only ever sent to a peer `rpc_addr` that came from the fleet
//! registry (never a user-supplied URL), redirects are disabled, and every
//! call carries the shared `X-Peer-Secret`. That keeps the forward path free of
//! SSRF and unauthenticated cross-node control.

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tarit_types::{CreateVmRequest, EgressUpdateRequest, OrchError, VmRecord};
use uuid::Uuid;

use crate::config::ApiIdentity;

pub struct PeerClient {
    secret: String,
    http: Client,
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
    pub fn new(secret: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(5))
            // SSRF hardening: never follow redirects to an attacker-chosen host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        Self { secret, http }
    }

    fn peer_url(rpc_addr: &str, path: &str) -> String {
        let base = rpc_addr.trim_end_matches('/');
        format!("{base}{path}")
    }

    fn post_json<B: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        rpc_addr: &str,
        path: &str,
        body: &B,
        identity: Option<&ApiIdentity>,
        what: &str,
    ) -> Result<R, OrchError> {
        let req = self
            .http
            .post(Self::peer_url(rpc_addr, path))
            .header("X-Peer-Secret", &self.secret)
            .json(body);
        let resp = self
            .with_identity(req, identity)
            .send()
            .map_err(|e| OrchError::Internal(format!("peer {what} request: {e}")))?;
        Self::decode(resp, what)
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
        rpc_addr: &str,
        req: &CreateVmRequest,
        identity: &ApiIdentity,
    ) -> Result<VmRecord, OrchError> {
        self.post_json(rpc_addr, "/internal/v1/vms", req, Some(identity), "create")
    }

    /// Restore a snapshot on the peer that holds its file (node-local restore).
    pub fn restore_remote(
        &self,
        rpc_addr: &str,
        req: &tarit_types::RestoreRequest,
        identity: &ApiIdentity,
    ) -> Result<VmRecord, OrchError> {
        self.post_json(
            rpc_addr,
            "/internal/v1/restore",
            req,
            Some(identity),
            "restore",
        )
    }

    pub fn exec_remote(
        &self,
        rpc_addr: &str,
        vm_id: Uuid,
        command: &str,
        timeout_ms: u64,
        identity: &ApiIdentity,
    ) -> Result<(i32, String, String, u64), OrchError> {
        let body: RemoteExecResponse = self.post_json(
            rpc_addr,
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
        rpc_addr: &str,
        vm_id: Uuid,
        identity: &ApiIdentity,
    ) -> Result<VmRecord, OrchError> {
        self.post_json(
            rpc_addr,
            &format!("/internal/v1/vms/{vm_id}/pause"),
            &serde_json::json!({}),
            Some(identity),
            "pause",
        )
    }

    pub fn resume_remote(
        &self,
        rpc_addr: &str,
        vm_id: Uuid,
        identity: &ApiIdentity,
    ) -> Result<VmRecord, OrchError> {
        self.post_json(
            rpc_addr,
            &format!("/internal/v1/vms/{vm_id}/resume"),
            &serde_json::json!({}),
            Some(identity),
            "resume",
        )
    }

    pub fn snapshot_remote(
        &self,
        rpc_addr: &str,
        vm_id: Uuid,
        diff: bool,
        identity: &ApiIdentity,
    ) -> Result<serde_json::Value, OrchError> {
        self.post_json(
            rpc_addr,
            &format!("/internal/v1/vms/{vm_id}/snapshot"),
            &RemoteSnapshotRequest { diff },
            Some(identity),
            "snapshot",
        )
    }

    pub fn egress_remote(
        &self,
        rpc_addr: &str,
        vm_id: Uuid,
        body: &EgressUpdateRequest,
        identity: &ApiIdentity,
    ) -> Result<serde_json::Value, OrchError> {
        let req = self
            .http
            .patch(Self::peer_url(
                rpc_addr,
                &format!("/internal/v1/vms/{vm_id}/egress"),
            ))
            .header("X-Peer-Secret", &self.secret)
            .json(body);
        let resp = self
            .with_identity(req, Some(identity))
            .send()
            .map_err(|e| OrchError::Internal(format!("peer egress request: {e}")))?;
        Self::decode(resp, "egress")
    }

    pub fn get_remote(
        &self,
        rpc_addr: &str,
        vm_id: Uuid,
        identity: &ApiIdentity,
    ) -> Result<VmRecord, OrchError> {
        let req = self
            .http
            .get(Self::peer_url(
                rpc_addr,
                &format!("/internal/v1/vms/{vm_id}"),
            ))
            .header("X-Peer-Secret", &self.secret);
        let resp = self
            .with_identity(req, Some(identity))
            .send()
            .map_err(|e| OrchError::Internal(format!("peer get request: {e}")))?;
        Self::decode(resp, "get")
    }

    pub fn status_remote(
        &self,
        rpc_addr: &str,
        vm_id: Uuid,
        identity: &ApiIdentity,
    ) -> Result<serde_json::Value, OrchError> {
        let req = self
            .http
            .get(Self::peer_url(
                rpc_addr,
                &format!("/internal/v1/vms/{vm_id}/status"),
            ))
            .header("X-Peer-Secret", &self.secret);
        let resp = self
            .with_identity(req, Some(identity))
            .send()
            .map_err(|e| OrchError::Internal(format!("peer status request: {e}")))?;
        Self::decode(resp, "status")
    }

    pub fn stop_remote(
        &self,
        rpc_addr: &str,
        vm_id: Uuid,
        identity: &ApiIdentity,
    ) -> Result<(), OrchError> {
        let req = self
            .http
            .delete(Self::peer_url(
                rpc_addr,
                &format!("/internal/v1/vms/{vm_id}"),
            ))
            .header("X-Peer-Secret", &self.secret);
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
            req.header("X-Tarit-Tenant", &identity.tenant)
                .header("X-Tarit-Role", identity.role.as_str())
                .header("X-Tarit-Api-Key-Id", &identity.api_key_id)
        } else {
            req
        }
    }
}
