#![allow(dead_code)]

use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{CloseFrame as AxumCloseFrame, Message as AxumMessage, WebSocket, WebSocketUpgrade},
        ConnectInfo, FromRequestParts, State,
    },
    http::{
        header::{
            HeaderName, HeaderValue, CONNECTION, FORWARDED, HOST, PROXY_AUTHENTICATE,
            PROXY_AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL, UPGRADE,
        },
        Request, StatusCode, Uri,
    },
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use futures_util::{
    stream::{self, SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use std::{
    collections::{HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tarit_types::{ErrorBody, OrchError, ShareRecord};
use tokio::{
    net::TcpStream,
    sync::watch,
    time::{self, Instant},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        protocol::{CloseFrame as TungsteniteCloseFrame, Message as TungsteniteMessage},
    },
    MaybeTlsStream, WebSocketStream,
};

use crate::{
    api::AppState,
    cluster::{self, Owner},
    config::{ApiIdentity, ApiRole},
    net::NetAlloc,
    shares,
    supervisor::NetworkLease,
};

type UpstreamWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const MAX_PENDING_PINGS: usize = 64;
const SHARE_TOKEN_HEADER: &str = "x-tarit-share-token";

#[derive(Default)]
struct PendingPings {
    values: VecDeque<Bytes>,
}

impl PendingPings {
    fn track(&mut self, payload: Bytes) {
        if self.values.len() == MAX_PENDING_PINGS {
            self.values.pop_front();
        }
        self.values.push_back(payload);
    }

    fn consume(&mut self, payload: &Bytes) -> bool {
        let Some(position) = self.values.iter().position(|pending| pending == payload) else {
            return false;
        };
        self.values.remove(position);
        true
    }

    fn len(&self) -> usize {
        self.values.len()
    }
}

#[derive(Clone)]
pub(crate) struct TrustedForwarding {
    peer: Option<SocketAddr>,
    host: String,
}

#[derive(Clone, Copy)]
struct UpstreamTarget {
    ip: IpAddr,
    port: u16,
}

impl UpstreamTarget {
    fn new(ip: IpAddr, port: u16) -> Self {
        Self { ip, port }
    }

    fn from_net(net: &NetAlloc, port: u16) -> Result<Self, GatewayError> {
        let ip = net
            .guest_ip
            .parse()
            .map_err(|_| GatewayError::Unavailable)?;
        (port != 0)
            .then_some(Self::new(ip, port))
            .ok_or(GatewayError::Unavailable)
    }

    fn uri(self, scheme: &str, request_uri: &Uri) -> Result<String, GatewayError> {
        if request_uri.scheme().is_some() || request_uri.authority().is_some() {
            return Err(GatewayError::NotFound);
        }
        let path_and_query = request_uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        if !path_and_query.starts_with('/') {
            return Err(GatewayError::NotFound);
        }
        Ok(format!(
            "{scheme}://{}{}",
            SocketAddr::new(self.ip, self.port),
            path_and_query
        ))
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum GatewayError {
    Unauthorized,
    NotFound,
    Unavailable,
}

impl From<OrchError> for GatewayError {
    fn from(error: OrchError) -> Self {
        match error {
            OrchError::Unauthorized => Self::Unauthorized,
            OrchError::NotFound(_) | OrchError::BadRequest(_) | OrchError::Conflict(_) => {
                Self::NotFound
            }
            OrchError::Forbidden(_)
            | OrchError::Internal(_)
            | OrchError::Vmm(_)
            | OrchError::Overloaded { .. } => Self::Unavailable,
        }
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found"),
            Self::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "share unavailable"),
        };
        (
            status,
            axum::Json(ErrorBody {
                error: error.into(),
            }),
        )
            .into_response()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/internal/v1", any(reject_internal_path))
        .route("/internal/v1/{*path}", any(reject_internal_path))
        .route("/", any(handle_request))
        .route("/{*path}", any(handle_request))
        .with_state(state)
}

pub(crate) async fn resolve_share_owner(
    state: &AppState,
    id: uuid::Uuid,
) -> Result<Owner, OrchError> {
    cluster::resolve_owner(state, id).await
}

async fn reject_internal_path() -> Response {
    GatewayError::NotFound.into_response()
}

async fn handle_request(State(state): State<AppState>, request: Request<Body>) -> Response {
    let result = async {
        let domain = state
            .config
            .share_domain
            .as_deref()
            .ok_or(GatewayError::Unavailable)?;
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(peer)| *peer);
        let slug = share_slug_from_headers(request.headers(), domain)?;
        let token = share_token(request.headers())?;
        let share = shares::authorize_gateway(&state, &slug, token.as_deref()).await?;
        let forwarding = TrustedForwarding {
            peer,
            host: format!("{slug}.{domain}"),
        };
        let (mut parts, body) = request.into_parts();
        let owner = resolve_share_owner(&state, share.vm_id)
            .await
            .map_err(|error| {
                tracing::warn!(share_id = %share.id, %error, "share owner resolution failed");
                GatewayError::Unavailable
            })?;

        if is_websocket_request(&parts) {
            let protocols = requested_subprotocols(&parts.headers)?;
            let websocket = WebSocketUpgrade::from_request_parts(&mut parts, &state)
                .await
                .map_err(|_| GatewayError::NotFound)?;
            match owner {
                Owner::Local => {
                    proxy_local_websocket(
                        &state,
                        &share,
                        &parts.uri,
                        websocket,
                        protocols,
                        &parts.headers,
                        &forwarding,
                        connect_timeout(&state),
                        idle_timeout(&state),
                    )
                    .await
                }
                Owner::Remote(rpc_addr) => {
                    proxy_remote_websocket(
                        &state,
                        &share,
                        &rpc_addr,
                        &parts.uri,
                        websocket,
                        protocols,
                        &parts.headers,
                    )
                    .await
                }
            }
        } else {
            parts.extensions.insert(forwarding);
            let request = Request::from_parts(parts, body);
            match owner {
                Owner::Local => proxy_local_http(&state, &share, request).await,
                Owner::Remote(rpc_addr) => {
                    proxy_remote_http(&state, &share, &rpc_addr, request).await
                }
            }
        }
    }
    .await;

    result.unwrap_or_else(IntoResponse::into_response)
}

fn share_identity(share: &ShareRecord) -> ApiIdentity {
    ApiIdentity {
        tenant: share.owner_key.clone(),
        role: ApiRole::User,
        max_vms: None,
        api_key_id: share_peer_identity_id(share),
    }
}

pub(crate) fn share_peer_identity_id(share: &ShareRecord) -> String {
    format!("share:{}:{}", share.id, share.token_version)
}

async fn proxy_remote_http(
    state: &AppState,
    share: &ShareRecord,
    rpc_addr: &str,
    request: Request<Body>,
) -> Result<Response, GatewayError> {
    let identity = share_identity(share);
    state
        .peer
        .proxy_share_http(rpc_addr, share.id, &identity, request)
        .await
        .map_err(|error| {
            tracing::warn!(share_id = %share.id, %error, "owner share HTTP proxy failed");
            GatewayError::Unavailable
        })
}

async fn proxy_remote_websocket(
    state: &AppState,
    share: &ShareRecord,
    rpc_addr: &str,
    request_uri: &Uri,
    websocket: WebSocketUpgrade,
    protocols: Vec<String>,
    headers: &axum::http::HeaderMap,
) -> Result<Response, GatewayError> {
    let identity = share_identity(share);
    let (upstream, response_protocol) = time::timeout(
        connect_timeout(state),
        state.peer.connect_share_websocket(
            rpc_addr,
            share.id,
            &identity,
            request_uri,
            headers,
            &protocols,
        ),
    )
    .await
    .map_err(|_| GatewayError::Unavailable)?
    .map_err(|error| {
        tracing::warn!(share_id = %share.id, %error, "owner share WebSocket proxy failed");
        GatewayError::Unavailable
    })?;
    let protocol = negotiated_protocol(response_protocol.as_deref(), &protocols)?;
    let websocket = if let Some(protocol) = protocol {
        websocket.protocols([protocol])
    } else {
        websocket
    };
    let idle_timeout = idle_timeout(state);
    let metrics = Arc::clone(&state.metrics);
    Ok(websocket.on_upgrade(move |client| async move {
        let _active_websocket = metrics.track_share_websocket();
        bridge_websocket(client, upstream, idle_timeout).await;
    }))
}

/// Handle the owner-side peer request after the internal router has loaded and
/// authorized the authoritative share record. The peer URL is never exposed to
/// this function; it only derives a guest target from the local supervisor.
pub(crate) async fn proxy_authoritative_local_share(
    state: &AppState,
    share: &ShareRecord,
    request: Request<Body>,
) -> Result<Response, GatewayError> {
    let domain = state
        .config
        .share_domain
        .as_deref()
        .ok_or(GatewayError::Unavailable)?;
    let forwarding = TrustedForwarding {
        peer: None,
        host: format!("{}.{}", share.slug, domain),
    };
    let (mut parts, body) = request.into_parts();
    if is_websocket_request(&parts) {
        let protocols = requested_subprotocols(&parts.headers)?;
        let websocket = WebSocketUpgrade::from_request_parts(&mut parts, state)
            .await
            .map_err(|_| GatewayError::Unavailable)?;
        proxy_local_websocket(
            state,
            share,
            &parts.uri,
            websocket,
            protocols,
            &parts.headers,
            &forwarding,
            connect_timeout(state),
            idle_timeout(state),
        )
        .await
    } else {
        parts.extensions.insert(forwarding);
        proxy_local_http(state, share, Request::from_parts(parts, body)).await
    }
}

fn is_websocket_request(parts: &axum::http::request::Parts) -> bool {
    parts
        .headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim().eq_ignore_ascii_case("upgrade"))
        && parts
            .headers
            .get(UPGRADE)
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
}

fn connect_timeout(state: &AppState) -> Duration {
    Duration::from_millis(state.config.share_connect_timeout_ms)
}

fn idle_timeout(state: &AppState) -> Duration {
    Duration::from_secs(state.config.share_idle_timeout_secs)
}

fn share_slug_from_headers(
    headers: &axum::http::HeaderMap,
    domain: &str,
) -> Result<String, GatewayError> {
    let values = headers.get_all(HOST);
    if values.iter().count() != 1 {
        return Err(GatewayError::NotFound);
    }
    let host = values
        .iter()
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or(GatewayError::NotFound)?;
    share_slug(host, domain).map_err(|_| GatewayError::NotFound)
}

fn share_slug(host: &str, domain: &str) -> Result<String, ()> {
    let authority = host.parse::<axum::http::uri::Authority>().map_err(|_| ())?;
    let hostname = authority.host().to_ascii_lowercase();
    let suffix = format!(".{domain}");
    let slug = hostname.strip_suffix(&suffix).ok_or(())?;
    if slug.is_empty()
        || slug.len() > 63
        || slug.contains('.')
        || slug.starts_with('-')
        || slug.ends_with('-')
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(());
    }
    Ok(slug.into())
}

fn share_token(headers: &axum::http::HeaderMap) -> Result<Option<String>, GatewayError> {
    let values = headers.get_all(SHARE_TOKEN_HEADER);
    if values.iter().count() > 1 {
        return Err(GatewayError::Unauthorized);
    }
    let Some(value) = values.iter().next() else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| GatewayError::Unauthorized)?;
    let token = (!value.is_empty() && !value.bytes().any(|byte| byte.is_ascii_whitespace()))
        .then_some(value)
        .ok_or(GatewayError::Unauthorized)?;
    Ok(Some(token.into()))
}

fn requested_subprotocols(headers: &axum::http::HeaderMap) -> Result<Vec<String>, GatewayError> {
    let mut protocols = Vec::new();
    for value in headers.get_all(SEC_WEBSOCKET_PROTOCOL) {
        let value = value.to_str().map_err(|_| GatewayError::NotFound)?;
        for protocol in value.split(',').map(str::trim) {
            if protocol.is_empty()
                || !protocol.bytes().all(is_websocket_token_byte)
                || protocols.iter().any(|known| known == protocol)
            {
                return Err(GatewayError::NotFound);
            }
            protocols.push(protocol.into());
        }
    }
    Ok(protocols)
}

fn is_websocket_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

async fn proxy_local_http(
    state: &AppState,
    share: &ShareRecord,
    request: Request<Body>,
) -> Result<Response, GatewayError> {
    let (target, lease) = local_target(state, share)?;
    proxy_http_to_target(
        target,
        request,
        connect_timeout(state),
        idle_timeout(state),
        Some(lease),
    )
    .await
}

fn local_target(
    state: &AppState,
    share: &ShareRecord,
) -> Result<(UpstreamTarget, NetworkLease), GatewayError> {
    let lease = state
        .supervisor
        .acquire_network_lease(share.vm_id)
        .map_err(|_| GatewayError::Unavailable)?;
    let target = UpstreamTarget::from_net(lease.allocation(), share.guest_port)?;
    Ok((target, lease))
}

async fn proxy_http_to_target(
    target: UpstreamTarget,
    request: Request<Body>,
    connect_timeout: Duration,
    idle_timeout: Duration,
    lease: Option<NetworkLease>,
) -> Result<Response, GatewayError> {
    let upstream = target.uri("http", request.uri())?;
    let forwarding = request
        .extensions()
        .get::<TrustedForwarding>()
        .cloned()
        .ok_or(GatewayError::Unavailable)?;
    let (parts, body) = request.into_parts();
    let headers = sanitized_request_headers(&parts.headers, &forwarding)?;
    let (upload_done_tx, mut upload_done_rx) = watch::channel(false);
    let client = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| GatewayError::Unavailable)?;
    let request = client
        .request(parts.method, upstream)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(request_body_stream(
            body,
            idle_timeout,
            upload_done_tx,
        )))
        .send();
    tokio::pin!(request);
    let response = loop {
        if *upload_done_rx.borrow() {
            break time::timeout(idle_timeout, &mut request)
                .await
                .map_err(|_| GatewayError::Unavailable)?
                .map_err(|_| GatewayError::Unavailable)?;
        }
        tokio::select! {
            response = &mut request => break response.map_err(|_| GatewayError::Unavailable)?,
            changed = upload_done_rx.changed() => {
                if changed.is_err() {
                    break time::timeout(idle_timeout, &mut request)
                        .await
                        .map_err(|_| GatewayError::Unavailable)?
                        .map_err(|_| GatewayError::Unavailable)?;
                }
            }
        }
    };
    let status = response.status();
    let headers = sanitized_response_headers(response.headers());
    let response_stream = response_body_stream(response, idle_timeout, lease);
    let mut builder = axum::http::Response::builder().status(status);
    *builder.headers_mut().ok_or(GatewayError::Unavailable)? = headers;
    builder
        .body(Body::from_stream(response_stream))
        .map_err(|_| GatewayError::Unavailable)
}

fn request_body_stream(
    body: Body,
    idle_timeout: Duration,
    upload_done_tx: watch::Sender<bool>,
) -> impl futures_util::Stream<Item = Result<Bytes, io::Error>> {
    stream::try_unfold(
        (Box::pin(body.into_data_stream()), upload_done_tx),
        move |(mut stream, upload_done_tx)| async move {
            match time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(Ok(chunk))) => Ok(Some((chunk, (stream, upload_done_tx)))),
                Ok(Some(Err(error))) => Err(io::Error::other(error.to_string())),
                Ok(None) => {
                    upload_done_tx.send_replace(true);
                    Ok(None)
                }
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "request body idle timeout",
                )),
            }
        },
    )
}

fn response_body_stream(
    response: reqwest::Response,
    idle_timeout: Duration,
    lease: Option<NetworkLease>,
) -> impl futures_util::Stream<Item = Result<Bytes, io::Error>> {
    stream::try_unfold(
        (Box::pin(response.bytes_stream()), lease),
        move |(mut stream, lease)| async move {
            match time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(Ok(chunk))) => Ok(Some((chunk, (stream, lease)))),
                Ok(Some(Err(error))) => Err(io::Error::other(error.to_string())),
                Ok(None) => Ok(None),
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "response body idle timeout",
                )),
            }
        },
    )
}

fn sanitized_request_headers(
    headers: &axum::http::HeaderMap,
    forwarding: &TrustedForwarding,
) -> Result<axum::http::HeaderMap, GatewayError> {
    let connection_headers = connection_headers(headers);
    let mut sanitized = axum::http::HeaderMap::new();
    for (name, value) in headers {
        if should_strip_request_header(name, &connection_headers) {
            continue;
        }
        sanitized.append(name.clone(), value.clone());
    }
    if let Some(peer) = forwarding.peer {
        sanitized.insert(
            "x-forwarded-for",
            HeaderValue::from_str(&peer.ip().to_string()).map_err(|_| GatewayError::Unavailable)?,
        );
    }
    sanitized.insert(
        "x-forwarded-host",
        HeaderValue::from_str(&forwarding.host).map_err(|_| GatewayError::Unavailable)?,
    );
    sanitized.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    let forwarded = match forwarding.peer {
        Some(peer) if peer.ip().is_ipv4() => {
            format!("for={};host={};proto=http", peer.ip(), forwarding.host)
        }
        Some(peer) => format!(
            "for=\"[{}]\";host={};proto=http",
            peer.ip(),
            forwarding.host
        ),
        None => format!("host={};proto=http", forwarding.host),
    };
    sanitized.insert(
        FORWARDED,
        HeaderValue::from_str(&forwarded).map_err(|_| GatewayError::Unavailable)?,
    );
    Ok(sanitized)
}

fn sanitized_response_headers(headers: &axum::http::HeaderMap) -> axum::http::HeaderMap {
    let connection_headers = connection_headers(headers);
    headers
        .iter()
        .filter(|(name, _)| !should_strip_response_header(name, &connection_headers))
        .fold(
            axum::http::HeaderMap::new(),
            |mut sanitized, (name, value)| {
                sanitized.append(name.clone(), value.clone());
                sanitized
            },
        )
}

fn connection_headers(headers: &axum::http::HeaderMap) -> HashSet<HeaderName> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|value| HeaderName::from_str(value.trim()).ok())
        .collect()
}

fn should_strip_request_header(
    name: &HeaderName,
    connection_headers: &HashSet<HeaderName>,
) -> bool {
    is_hop_by_hop(name, connection_headers)
        || name == HOST
        || name.as_str() == SHARE_TOKEN_HEADER
        || name == PROXY_AUTHORIZATION
        || name == PROXY_AUTHENTICATE
        || name == FORWARDED
        || name.as_str().starts_with("x-forwarded-")
        || name.as_str() == "x-real-ip"
        || name.as_str() == "x-peer-secret"
        || name.as_str().starts_with("x-tarit-")
}

fn should_strip_response_header(
    name: &HeaderName,
    connection_headers: &HashSet<HeaderName>,
) -> bool {
    is_hop_by_hop(name, connection_headers)
        || name == PROXY_AUTHENTICATE
        || name == PROXY_AUTHORIZATION
        || name == FORWARDED
        || name.as_str().starts_with("x-forwarded-")
        || name.as_str() == "x-real-ip"
        || name.as_str().starts_with("x-tarit-")
}

fn is_hop_by_hop(name: &HeaderName, connection_headers: &HashSet<HeaderName>) -> bool {
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
        )
}

async fn proxy_local_websocket(
    state: &AppState,
    share: &ShareRecord,
    request_uri: &Uri,
    websocket: WebSocketUpgrade,
    protocols: Vec<String>,
    headers: &axum::http::HeaderMap,
    forwarding: &TrustedForwarding,
    connect_timeout: Duration,
    idle_timeout: Duration,
) -> Result<Response, GatewayError> {
    let (target, lease) = local_target(state, share)?;
    proxy_websocket_to_target(
        target,
        request_uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/"),
        websocket,
        protocols,
        headers,
        forwarding,
        connect_timeout,
        idle_timeout,
        Some(lease),
        Arc::clone(&state.metrics),
    )
    .await
}

async fn proxy_websocket_to_target(
    target: UpstreamTarget,
    path_and_query: &str,
    websocket: WebSocketUpgrade,
    protocols: Vec<String>,
    headers: &axum::http::HeaderMap,
    forwarding: &TrustedForwarding,
    connect_timeout: Duration,
    idle_timeout: Duration,
    lease: Option<NetworkLease>,
    metrics: Arc<crate::metrics::Metrics>,
) -> Result<Response, GatewayError> {
    let request_uri = path_and_query
        .parse::<Uri>()
        .map_err(|_| GatewayError::NotFound)?;
    let upstream = target.uri("ws", &request_uri)?;
    let mut upstream_request = upstream
        .into_client_request()
        .map_err(|_| GatewayError::Unavailable)?;
    let headers = sanitized_websocket_headers(headers, forwarding)?;
    for (name, value) in &headers {
        upstream_request
            .headers_mut()
            .append(name.clone(), value.clone());
    }
    if !protocols.is_empty() {
        upstream_request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_str(&protocols.join(", ")).map_err(|_| GatewayError::NotFound)?,
        );
    }

    fn sanitized_websocket_headers(
        headers: &axum::http::HeaderMap,
        forwarding: &TrustedForwarding,
    ) -> Result<axum::http::HeaderMap, GatewayError> {
        let mut headers = sanitized_request_headers(headers, forwarding)?;
        let handshake_headers = headers
            .keys()
            .filter(|name| name.as_str().starts_with("sec-websocket-"))
            .cloned()
            .collect::<Vec<_>>();
        for name in handshake_headers {
            headers.remove(name);
        }
        Ok(headers)
    }
    let (upstream, response) = time::timeout(connect_timeout, connect_async(upstream_request))
        .await
        .map_err(|_| GatewayError::Unavailable)?
        .map_err(|_| GatewayError::Unavailable)?;
    let protocol = negotiated_protocol(
        response
            .headers()
            .get(SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok()),
        &protocols,
    )?;
    let websocket = if let Some(protocol) = protocol {
        websocket.protocols([protocol])
    } else {
        websocket
    };
    Ok(websocket.on_upgrade(move |client| async move {
        let _lease = lease;
        let _active_websocket = metrics.track_share_websocket();
        bridge_websocket(client, upstream, idle_timeout).await;
    }))
}

fn negotiated_protocol(
    response_protocol: Option<&str>,
    requested: &[String],
) -> Result<Option<String>, GatewayError> {
    let Some(response_protocol) = response_protocol else {
        return Ok(None);
    };
    let protocol = response_protocol
        .bytes()
        .all(is_websocket_token_byte)
        .then_some(response_protocol)
        .filter(|protocol| requested.iter().any(|requested| requested == protocol))
        .ok_or(GatewayError::Unavailable)?;
    Ok(Some(protocol.into()))
}

async fn bridge_websocket(client: WebSocket, upstream: UpstreamWebSocket, idle_timeout: Duration) {
    let (client_tx, client_rx) = client.split();
    let (upstream_tx, upstream_rx) = upstream.split();
    let (activity_tx, activity_rx) = watch::channel(Instant::now());
    let (client_close_tx, client_close_rx) = watch::channel(false);
    let client_closed = Arc::new(AtomicBool::new(false));
    let upstream_closed = Arc::new(AtomicBool::new(false));
    let client_pings = Arc::new(Mutex::new(PendingPings::default()));
    let mut client_to_upstream = tokio::spawn(forward_client_to_upstream(
        client_rx,
        upstream_tx,
        activity_tx.clone(),
        activity_rx.clone(),
        idle_timeout,
        Arc::clone(&client_closed),
        Arc::clone(&upstream_closed),
        client_close_tx.clone(),
        Arc::clone(&client_pings),
    ));
    let mut upstream_to_client = tokio::spawn(forward_upstream_to_client(
        upstream_rx,
        client_tx,
        activity_tx,
        activity_rx,
        idle_timeout,
        client_closed,
        upstream_closed,
        client_close_rx,
        client_pings,
    ));

    tokio::select! {
        _ = &mut client_to_upstream => {
            let _ = time::timeout(idle_timeout, &mut upstream_to_client).await;
            upstream_to_client.abort();
        }
        _ = &mut upstream_to_client => {
            let _ = time::timeout(idle_timeout, &mut client_to_upstream).await;
            client_to_upstream.abort();
        }
    }
}

async fn forward_client_to_upstream(
    mut source: SplitStream<WebSocket>,
    mut sink: SplitSink<UpstreamWebSocket, TungsteniteMessage>,
    activity_tx: watch::Sender<Instant>,
    mut activity_rx: watch::Receiver<Instant>,
    idle_timeout: Duration,
    client_closed: Arc<AtomicBool>,
    upstream_closed: Arc<AtomicBool>,
    client_close_tx: watch::Sender<bool>,
    client_pings: Arc<Mutex<PendingPings>>,
) {
    loop {
        let deadline = *activity_rx.borrow() + idle_timeout;
        tokio::select! {
            _ = time::sleep_until(deadline) => {
                let _ = sink.send(TungsteniteMessage::Close(None)).await;
                return;
            }
            changed = activity_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                let _ = sink.flush().await;
            }
            message = source.next() => {
                let Some(Ok(message)) = message else {
                    let _ = sink.send(TungsteniteMessage::Close(None)).await;
                    return;
                };
                let close = matches!(message, AxumMessage::Close(_));
                if close {
                    client_closed.store(true, Ordering::Release);
                    client_close_tx.send_replace(true);
                    if upstream_closed.load(Ordering::Acquire) {
                        return;
                    }
                }
                if let AxumMessage::Ping(payload) = &message {
                    if let Ok(mut pending) = client_pings.lock() {
                        pending.track(payload.clone());
                    }
                }
                let message = client_message(message);
                if !matches!(time::timeout(idle_timeout, sink.send(message)).await, Ok(Ok(()))) {
                    return;
                }
                activity_tx.send_replace(Instant::now());
                if close {
                    return;
                }
            }
        }
    }
}

async fn forward_upstream_to_client(
    mut source: SplitStream<UpstreamWebSocket>,
    mut sink: SplitSink<WebSocket, AxumMessage>,
    activity_tx: watch::Sender<Instant>,
    mut activity_rx: watch::Receiver<Instant>,
    idle_timeout: Duration,
    client_closed: Arc<AtomicBool>,
    upstream_closed: Arc<AtomicBool>,
    mut client_close_rx: watch::Receiver<bool>,
    client_pings: Arc<Mutex<PendingPings>>,
) {
    loop {
        let deadline = *activity_rx.borrow() + idle_timeout;
        tokio::select! {
            _ = time::sleep_until(deadline) => {
                let _ = sink.send(AxumMessage::Close(None)).await;
                return;
            }
            changed = client_close_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                if *client_close_rx.borrow_and_update() {
                    let _ = sink.flush().await;
                }
            }
            changed = activity_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                let _ = sink.flush().await;
            }
            message = source.next() => {
                let Some(Ok(message)) = message else {
                    let _ = sink.send(AxumMessage::Close(None)).await;
                    return;
                };
                if let TungsteniteMessage::Pong(payload) = &message {
                    if consume_pending_ping(&client_pings, payload) {
                        activity_tx.send_replace(Instant::now());
                        continue;
                    }
                }
                let close = matches!(message, TungsteniteMessage::Close(_));
                if close {
                    upstream_closed.store(true, Ordering::Release);
                    if client_closed.load(Ordering::Acquire) {
                        let _ = sink.flush().await;
                        return;
                    }
                }
                let Some(message) = upstream_message(message) else {
                    continue;
                };
                if !matches!(time::timeout(idle_timeout, sink.send(message)).await, Ok(Ok(()))) {
                    return;
                }
                activity_tx.send_replace(Instant::now());
                if close {
                    return;
                }
            }
        }
    }
}

fn consume_pending_ping(pings: &Mutex<PendingPings>, payload: &Bytes) -> bool {
    let Ok(mut pings) = pings.lock() else {
        return false;
    };
    pings.consume(payload)
}

fn client_message(message: AxumMessage) -> TungsteniteMessage {
    match message {
        AxumMessage::Text(text) => TungsteniteMessage::Text(text.to_string().into()),
        AxumMessage::Binary(bytes) => TungsteniteMessage::Binary(bytes),
        AxumMessage::Ping(bytes) => TungsteniteMessage::Ping(bytes),
        AxumMessage::Pong(bytes) => TungsteniteMessage::Pong(bytes),
        AxumMessage::Close(frame) => {
            TungsteniteMessage::Close(frame.map(|frame| TungsteniteCloseFrame {
                code: frame.code.into(),
                reason: frame.reason.to_string().into(),
            }))
        }
    }
}

fn upstream_message(message: TungsteniteMessage) -> Option<AxumMessage> {
    match message {
        TungsteniteMessage::Text(text) => Some(AxumMessage::Text(text.to_string().into())),
        TungsteniteMessage::Binary(bytes) => Some(AxumMessage::Binary(bytes)),
        TungsteniteMessage::Ping(bytes) => Some(AxumMessage::Ping(bytes)),
        TungsteniteMessage::Pong(bytes) => Some(AxumMessage::Pong(bytes)),
        TungsteniteMessage::Close(frame) => {
            Some(AxumMessage::Close(frame.map(|frame| AxumCloseFrame {
                code: frame.code.into(),
                reason: frame.reason.to_string().into(),
            })))
        }
        TungsteniteMessage::Frame(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        proxy_http_to_target, proxy_websocket_to_target, router as gateway_router, share_slug,
        TrustedForwarding, UpstreamTarget,
    };
    use axum::{
        body::{Body, Bytes},
        extract::{ws::WebSocketUpgrade, State},
        http::{
            header::{
                AUTHORIZATION, CONNECTION, COOKIE, FORWARDED, HOST, ORIGIN, PROXY_AUTHORIZATION,
                SEC_WEBSOCKET_PROTOCOL,
            },
            HeaderMap, HeaderValue, Method, Request, StatusCode, Uri,
        },
        response::Response,
        routing::{get, post},
        Router,
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use chrono::Utc;
    use futures_util::{SinkExt, StreamExt};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::{
        collections::HashMap,
        convert::Infallible,
        net::SocketAddr,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tarit_types::{ShareRecord, ShareVisibility};
    use tokio::{
        net::TcpListener,
        sync::{mpsc, oneshot},
    };
    use tokio_tungstenite::{
        accept_hdr_async, connect_async,
        tungstenite::{client::IntoClientRequest, protocol::Message as TungsteniteMessage},
    };
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::{
        api::AppState,
        audit::LocalAuditOutbox,
        config::{ApiKeyRegistry, ApiRole, AutoscaleConfig, Config, WarmPoolConfig},
        metrics::Metrics,
        net::NetAlloc,
        peer::PeerClient,
        pty::PtyRegistry,
        scheduler::Scheduler,
        shares::{ShareRepository, ShareTokenSigner},
        supervisor::VmmSupervisor,
    };

    const SHARE_HOST: &str = "calm-red-fox.shares.example.com";

    #[test]
    fn extracts_only_one_slug_label() {
        assert_eq!(
            share_slug(SHARE_HOST, "shares.example.com").unwrap(),
            "calm-red-fox"
        );
        for host in [
            "shares.example.com",
            "a.b.shares.example.com",
            "calm-red-fox.shares.example.com.",
            "-bad.shares.example.com",
            "bad-.shares.example.com",
        ] {
            assert!(share_slug(host, "shares.example.com").is_err(), "{host}");
        }
    }

    #[test]
    fn pending_pings_are_bounded() {
        let mut pings = super::PendingPings::default();
        for value in 0..=super::MAX_PENDING_PINGS {
            pings.track(Bytes::from(vec![value as u8]));
        }

        assert_eq!(pings.len(), super::MAX_PENDING_PINGS);
        assert!(!pings.consume(&Bytes::from_static(&[0])));
        assert!(pings.consume(&Bytes::from_static(&[super::MAX_PENDING_PINGS as u8])));
    }

    #[test]
    fn header_sanitation_preserves_basic_and_bearer_application_credentials() {
        let forwarding = TrustedForwarding {
            peer: Some("203.0.113.9:443".parse().unwrap()),
            host: SHARE_HOST.into(),
        };

        for authorization in ["Basic YXBwbGljYXRpb246c2VjcmV0", "Bearer application-token"] {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, HeaderValue::from_str(authorization).unwrap());
            headers.insert(
                "x-tarit-share-token",
                HeaderValue::from_static("share-token"),
            );

            let sanitized = super::sanitized_request_headers(&headers, &forwarding).unwrap();

            assert_eq!(sanitized.get(AUTHORIZATION).unwrap(), authorization);
            assert!(sanitized.get("x-tarit-share-token").is_none());
        }
    }

    #[test]
    fn share_token_rejects_multiple_credential_headers() {
        let mut headers = HeaderMap::new();
        headers.append(
            "x-tarit-share-token",
            HeaderValue::from_static("first-token"),
        );
        headers.append(
            "x-tarit-share-token",
            HeaderValue::from_static("second-token"),
        );

        assert!(super::share_token(&headers).is_err());
    }

    #[tokio::test]
    async fn gateway_router_rejects_ambiguous_hosts_before_target_resolution() {
        let state = gateway_test_state();
        let mut request = Request::builder()
            .uri("/")
            .header(HOST, SHARE_HOST)
            .body(Body::empty())
            .unwrap();
        request
            .headers_mut()
            .append(HOST, HeaderValue::from_static("other.shares.example.com"));

        let response = gateway_router(state).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn gateway_router_requires_the_dedicated_private_share_token_header() {
        let state = gateway_test_state();
        let share = gateway_share(Uuid::new_v4(), 8080, ShareVisibility::Private);
        state.shares.insert(&share).await.unwrap();
        let token = ShareTokenSigner::new([7; 32], Duration::from_secs(300))
            .issue(&share, Utc::now())
            .unwrap();
        assert!(
            crate::shares::authorize_gateway(&state, &share.slug, Some(&token))
                .await
                .is_ok()
        );
        let request = Request::builder()
            .uri("/")
            .header(HOST, SHARE_HOST)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();

        let response = gateway_router(state).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn gateway_router_does_not_expose_internal_paths() {
        let upstream = start_axum(Router::new().route(
            "/{*path}",
            get(|| async { Response::new(Body::from("guest internal endpoint")) }),
        ))
        .await;
        let state = gateway_test_state();
        install_gateway_share(&state, upstream, ShareVisibility::Public).await;
        let request = Request::builder()
            .uri("/internal/v1/shares/not-a-tarit-route")
            .header(HOST, SHARE_HOST)
            .body(Body::empty())
            .unwrap();

        let response = gateway_router(state).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn gateway_router_uses_supervisor_target_and_sanitizes_share_credentials() {
        let (upstream, received) = spawn_inspecting_http_upstream().await;
        let state = gateway_test_state();
        let share = install_gateway_share(&state, upstream, ShareVisibility::Private).await;
        assert_eq!(
            state
                .supervisor
                .network_allocation(share.vm_id)
                .unwrap()
                .guest_ip,
            upstream.ip().to_string()
        );
        let (target, _lease) = super::local_target(&state, &share).unwrap();
        assert_eq!(
            target.uri("http", &"/inspect".parse().unwrap()).unwrap(),
            format!("http://{upstream}/inspect")
        );
        let token = ShareTokenSigner::new([7; 32], Duration::from_secs(300))
            .issue(&share, Utc::now())
            .unwrap();
        assert!(
            crate::shares::authorize_gateway(&state, &share.slug, Some(&token))
                .await
                .is_ok()
        );
        let request = Request::builder()
            .uri("/inspect?unrelated=keep")
            .header(HOST, SHARE_HOST)
            .header(AUTHORIZATION, "Bearer application-credential")
            .header("x-tarit-share-token", token)
            .header(COOKIE, "session=application")
            .header(CONNECTION, "keep-alive, x-smuggled")
            .header("x-smuggled", "remove-me")
            .body(Body::empty())
            .unwrap();

        let response = gateway_router(state).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (headers, uri) = received.await.unwrap();
        assert_eq!(uri, "/inspect?unrelated=keep");
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap(),
            "Bearer application-credential"
        );
        assert_eq!(headers.get(COOKIE).unwrap(), "session=application");
        assert!(headers.get("x-tarit-share-token").is_none());
        assert!(headers.get("x-smuggled").is_none());
    }

    #[tokio::test]
    async fn streams_bodies_and_rebuilds_trusted_headers() {
        let (addr, received_headers) = spawn_streaming_http_upstream().await;
        let target = UpstreamTarget::new(addr.ip(), addr.port());
        let body = Body::from_stream(futures_util::stream::iter([
            Ok::<_, Infallible>(Bytes::from_static(b"one")),
            Ok::<_, Infallible>(Bytes::from_static(b"two")),
        ]));
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/stream?keep=this")
            .header(HOST, SHARE_HOST)
            .header(CONNECTION, "keep-alive, x-smuggled")
            .header("x-smuggled", "drop-me")
            .header("proxy-connection", "keep-alive")
            .header(PROXY_AUTHORIZATION, "Basic c2VjcmV0")
            .header(AUTHORIZATION, "Bearer private-share-token")
            .header("x-tarit-share-token", "share-secret")
            .header(FORWARDED, "for=untrusted")
            .header("x-forwarded-for", "198.51.100.7")
            .header("x-forwarded-host", "untrusted.example")
            .header("x-forwarded-proto", "https")
            .body(body)
            .unwrap();
        request.extensions_mut().insert(TrustedForwarding {
            peer: Some("203.0.113.9:443".parse().unwrap()),
            host: SHARE_HOST.into(),
        });

        let response = proxy_http_to_target(
            target,
            request,
            Duration::from_secs(1),
            Duration::from_secs(1),
            None,
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(CONNECTION).is_none());
        let chunks = response
            .into_body()
            .into_data_stream()
            .map(|chunk| chunk.unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            chunks,
            vec![Bytes::from_static(b"one"), Bytes::from_static(b"two")]
        );

        let headers = received_headers.await.unwrap();
        assert!(headers.get(PROXY_AUTHORIZATION).is_none());
        assert!(headers.get(AUTHORIZATION).is_some());
        assert!(headers.get("x-tarit-share-token").is_none());
        assert!(headers.get("x-smuggled").is_none());
        assert!(headers.get("proxy-connection").is_none());
        assert!(headers.get(FORWARDED).is_some());
        assert_eq!(
            headers.get(FORWARDED).unwrap(),
            "for=203.0.113.9;host=calm-red-fox.shares.example.com;proto=http"
        );
        assert_eq!(headers.get("x-forwarded-for").unwrap(), "203.0.113.9");
        assert_eq!(
            headers.get("x-forwarded-host").unwrap(),
            "calm-red-fox.shares.example.com"
        );
        assert_eq!(headers.get("x-forwarded-proto").unwrap(), "http");
    }

    #[tokio::test]
    async fn active_uploads_do_not_hit_the_response_idle_timeout() {
        let addr = spawn_upload_draining_upstream().await;
        let target = UpstreamTarget::new(addr.ip(), addr.port());
        let body = Body::from_stream(futures_util::stream::unfold(0, |chunk| async move {
            if chunk == 5 {
                None
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Some((Ok::<_, Infallible>(Bytes::from_static(b"chunk")), chunk + 1))
            }
        }));
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/upload")
            .header(HOST, SHARE_HOST)
            .body(body)
            .unwrap();
        request.extensions_mut().insert(TrustedForwarding {
            peer: Some("203.0.113.9:443".parse().unwrap()),
            host: SHARE_HOST.into(),
        });

        let response = proxy_http_to_target(
            target,
            request,
            Duration::from_secs(1),
            Duration::from_millis(100),
            None,
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn gateway_router_exposes_the_first_response_chunk_before_upstream_completes() {
        let (upstream, release_second_chunk, mut upstream_complete) =
            spawn_delayed_streaming_http_upstream().await;
        let state = gateway_test_state();
        let share = install_gateway_share(&state, upstream, ShareVisibility::Public).await;
        assert_eq!(
            state
                .supervisor
                .network_allocation(share.vm_id)
                .unwrap()
                .guest_ip,
            upstream.ip().to_string()
        );
        let request = Request::builder()
            .uri("/stream")
            .header(HOST, SHARE_HOST)
            .body(Body::empty())
            .unwrap();

        let response = tokio::time::timeout(
            Duration::from_millis(500),
            gateway_router(state).oneshot(request),
        )
        .await
        .expect("gateway should return response headers before upstream completes")
        .unwrap();
        let mut body = response.into_body().into_data_stream();
        let first = tokio::time::timeout(Duration::from_millis(500), body.next())
            .await
            .expect("first chunk should be observable")
            .unwrap()
            .unwrap();
        assert_eq!(first, Bytes::from_static(b"first"));
        assert!(
            upstream_complete.try_recv().is_err(),
            "upstream must still be waiting to produce its delayed second chunk"
        );

        release_second_chunk.send(()).unwrap();
        let second = tokio::time::timeout(Duration::from_millis(500), body.next())
            .await
            .expect("second chunk should arrive after release")
            .unwrap()
            .unwrap();
        assert_eq!(second, Bytes::from_static(b"second"));
        assert!(
            tokio::time::timeout(Duration::from_millis(500), &mut upstream_complete)
                .await
                .expect("upstream should complete after its second chunk")
                .is_ok()
        );
    }

    #[tokio::test]
    async fn bridges_websocket_frames_and_negotiates_the_upstream_protocol() {
        let (upstream, observed) = spawn_websocket_echo_upstream().await;
        let (gateway, metrics) = start_gateway_router(upstream, Duration::from_secs(1)).await;
        let mut request = format!("ws://{gateway}/socket?keep=this")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("chat, alternate"),
        );
        request
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://client.example"));
        request
            .headers_mut()
            .insert(COOKIE, HeaderValue::from_static("session=guest"));
        request
            .headers_mut()
            .insert(FORWARDED, HeaderValue::from_static("for=untrusted"));
        request
            .headers_mut()
            .insert("x-forwarded-for", HeaderValue::from_static("198.51.100.7"));
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer application-credential"),
        );
        request.headers_mut().insert(
            "x-tarit-share-token",
            HeaderValue::from_static("share-token"),
        );

        let (mut client, response) = connect_async(request).await.unwrap();
        wait_for_websocket_gauge(&metrics, 1).await;
        assert_eq!(
            response.headers().get(SEC_WEBSOCKET_PROTOCOL).unwrap(),
            "chat"
        );

        client
            .send(TungsteniteMessage::Text("hello".into()))
            .await
            .unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            TungsteniteMessage::Text("hello".into())
        );

        client
            .send(TungsteniteMessage::Binary(Bytes::from_static(b"bin")))
            .await
            .unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            TungsteniteMessage::Binary(Bytes::from_static(b"bin"))
        );

        client
            .send(TungsteniteMessage::Ping(Bytes::from_static(b"ping")))
            .await
            .unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            TungsteniteMessage::Pong(Bytes::from_static(b"ping"))
        );
        client
            .send(TungsteniteMessage::Pong(Bytes::from_static(b"pong")))
            .await
            .unwrap();

        client.send(TungsteniteMessage::Close(None)).await.unwrap();
        assert!(matches!(
            client.next().await.unwrap().unwrap(),
            TungsteniteMessage::Close(_)
        ));

        let observed = observed.await.unwrap();
        assert!(observed.contains(&"text"));
        assert!(observed.contains(&"binary"));
        assert!(observed.contains(&"ping"));
        assert!(observed.contains(&"pong"));
        assert!(observed.contains(&"close"));
        wait_for_websocket_gauge(&metrics, 0).await;
    }

    #[tokio::test]
    async fn websocket_idle_timeout_closes_an_inactive_bridge() {
        let (upstream, _observed) = spawn_websocket_echo_upstream().await;
        let (gateway, metrics) = start_gateway_router(upstream, Duration::from_millis(100)).await;
        let mut request = format!("ws://{gateway}/socket")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://client.example"));
        request
            .headers_mut()
            .insert(COOKIE, HeaderValue::from_static("session=guest"));
        request
            .headers_mut()
            .insert(FORWARDED, HeaderValue::from_static("for=untrusted"));
        request
            .headers_mut()
            .insert("x-forwarded-for", HeaderValue::from_static("198.51.100.7"));
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer application-credential"),
        );
        request.headers_mut().insert(
            "x-tarit-share-token",
            HeaderValue::from_static("share-token"),
        );
        let (mut client, _) = connect_async(request).await.unwrap();
        wait_for_websocket_gauge(&metrics, 1).await;

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), client.next())
                .await
                .unwrap(),
            Some(Ok(TungsteniteMessage::Close(_))) | None
        ));
        wait_for_websocket_gauge(&metrics, 0).await;
    }

    #[tokio::test]
    async fn upstream_eof_closes_client_without_waiting_for_idle_timeout() {
        let (upstream, drop_upstream) = spawn_websocket_drop_upstream().await;
        let (gateway, metrics) = start_gateway_router(upstream, Duration::from_millis(100)).await;
        let (mut client, _) = connect_async(format!("ws://{gateway}/socket"))
            .await
            .unwrap();
        wait_for_websocket_gauge(&metrics, 1).await;
        drop_upstream.send(()).unwrap();

        let close = tokio::time::timeout(Duration::from_secs(1), client.next())
            .await
            .expect("client should observe upstream EOF");
        assert!(matches!(
            close,
            Some(Ok(TungsteniteMessage::Close(_))) | None
        ));
        wait_for_websocket_gauge(&metrics, 0).await;
    }

    #[tokio::test]
    async fn client_disconnect_releases_the_active_websocket_gauge() {
        let (upstream, _observed) = spawn_websocket_echo_upstream().await;
        let (gateway, metrics) = start_gateway_router(upstream, Duration::from_secs(1)).await;
        let mut request = format!("ws://{gateway}/socket")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("chat, alternate"),
        );
        request
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://client.example"));
        request
            .headers_mut()
            .insert(COOKIE, HeaderValue::from_static("session=guest"));
        request
            .headers_mut()
            .insert(FORWARDED, HeaderValue::from_static("for=untrusted"));
        request
            .headers_mut()
            .insert("x-forwarded-for", HeaderValue::from_static("198.51.100.7"));
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer application-credential"),
        );
        request.headers_mut().insert(
            "x-tarit-share-token",
            HeaderValue::from_static("share-token"),
        );
        let (client, _) = connect_async(request).await.unwrap();
        wait_for_websocket_gauge(&metrics, 1).await;

        drop(client);

        wait_for_websocket_gauge(&metrics, 0).await;
    }

    #[tokio::test]
    async fn remote_owner_preserves_stream_chunks_and_identity() {
        let mut cluster = TestShareCluster::start().await;

        let response = cluster
            .request_through_non_owner_with_header(
                "/stream?keep=this",
                "x-tarit-tenant",
                "attacker-tenant",
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);

        let mut body = response.bytes_stream();
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(500), body.next())
                .await
                .expect("owner should stream the first chunk promptly")
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"first")
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(500), cluster.owner_tenant())
                .await
                .expect("owner should receive the forwarding identity"),
            "tenant-a"
        );

        cluster.release_second_chunk();
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(500), body.next())
                .await
                .expect("owner should stream the delayed chunk")
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"second")
        );
    }

    #[tokio::test]
    async fn remote_owner_forwards_the_exact_root_path() {
        let (upstream, received) = spawn_root_inspecting_http_upstream().await;
        let cluster =
            TestShareCluster::start_with_upstream(upstream, ShareVisibility::Public).await;

        let response = cluster.request_through_non_owner("/").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(received.await.unwrap(), "/");
    }

    #[tokio::test]
    async fn remote_owner_forwards_root_query_strings() {
        let (upstream, received) = spawn_root_inspecting_http_upstream().await;
        let cluster =
            TestShareCluster::start_with_upstream(upstream, ShareVisibility::Public).await;

        let response = cluster.request_through_non_owner("/?x=preserve").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(received.await.unwrap(), "/?x=preserve");
    }

    #[tokio::test]
    async fn remote_owner_streams_upload_chunks_before_the_client_finishes() {
        let (upstream, first_chunk) = spawn_first_chunk_observing_upstream().await;
        let cluster =
            TestShareCluster::start_with_upstream(upstream, ShareVisibility::Public).await;
        let (release_second_chunk_tx, release_second_chunk_rx) = oneshot::channel();
        let body = futures_util::stream::unfold(
            (false, Some(release_second_chunk_rx)),
            |(sent_first, release_second_chunk_rx)| async move {
                if !sent_first {
                    return Some((
                        Ok::<_, std::io::Error>(Bytes::from_static(b"first")),
                        (true, release_second_chunk_rx),
                    ));
                }
                let release_second_chunk_rx = release_second_chunk_rx?;
                let _ = release_second_chunk_rx.await;
                Some((Ok(Bytes::from_static(b"second")), (true, None)))
            },
        );
        let request = reqwest::Client::new()
            .post(format!("http://{}/upload", cluster.client_addr))
            .header(HOST, SHARE_HOST)
            .body(reqwest::Body::wrap_stream(body))
            .send();
        let request = tokio::spawn(request);

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(500), first_chunk)
                .await
                .expect("owner upstream should receive the first delayed upload chunk")
                .unwrap(),
            Bytes::from_static(b"first")
        );
        assert!(
            !request.is_finished(),
            "the client must still be waiting to produce its second upload chunk"
        );

        release_second_chunk_tx.send(()).unwrap();
        let response = request.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.bytes().await.unwrap(),
            Bytes::from_static(b"firstsecond")
        );
    }

    #[tokio::test]
    async fn peer_share_route_rejects_missing_or_forged_identity() {
        let cluster = TestShareCluster::start().await;

        let missing_secret = reqwest::Client::new()
            .get(cluster.owner_share_url("/stream"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing_secret.status(), StatusCode::SERVICE_UNAVAILABLE);

        let forged_admin = reqwest::Client::new()
            .get(cluster.owner_share_url("/stream"))
            .header("x-peer-secret", "peer-secret")
            .header("x-tarit-tenant", "tenant-b")
            .header("x-tarit-role", "admin")
            .header("x-tarit-api-key-id", "forged")
            .send()
            .await
            .unwrap();
        assert_eq!(forged_admin.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn peer_share_identity_cannot_be_replayed() {
        let upstream = start_axum(
            Router::new().route("/stream", get(|| async { Response::new(Body::from("ok")) })),
        )
        .await;
        let cluster =
            TestShareCluster::start_with_upstream(upstream, ShareVisibility::Public).await;
        let headers = signed_share_identity_headers(&cluster.share);

        let first = reqwest::Client::new()
            .get(cluster.owner_share_url("/stream"))
            .headers(headers.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let replay = reqwest::Client::new()
            .get(cluster.owner_share_url("/stream"))
            .headers(headers)
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn remote_owner_enforces_private_share_auth_and_strips_share_tokens() {
        let (upstream, received) = spawn_inspecting_http_upstream().await;
        let cluster =
            TestShareCluster::start_with_upstream(upstream, ShareVisibility::Private).await;
        let token = ShareTokenSigner::new([7; 32], Duration::from_secs(300))
            .issue(&cluster.share, Utc::now())
            .unwrap();

        let missing_token = cluster.request_through_non_owner("/inspect").await;
        assert_eq!(missing_token.status(), StatusCode::UNAUTHORIZED);

        let response = cluster
            .request_through_non_owner_with_header(
                "/inspect?preserve=this",
                "x-tarit-share-token",
                &token,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let (headers, uri) = received.await.unwrap();
        assert_eq!(uri, "/inspect?preserve=this");
        assert!(headers.get("x-tarit-share-token").is_none());
    }

    #[tokio::test]
    async fn remote_owner_streams_large_uploads_and_query_strings() {
        let (upstream, received) = spawn_large_echo_upstream().await;
        let cluster =
            TestShareCluster::start_with_upstream(upstream, ShareVisibility::Public).await;
        let payload = vec![b'x'; 2 * 1024 * 1024];
        let response = reqwest::Client::new()
            .post(format!("http://{}/upload?large=yes", cluster.client_addr))
            .header(HOST, SHARE_HOST)
            .body(reqwest::Body::wrap_stream(futures_util::stream::iter([
                Ok::<_, std::io::Error>(Bytes::copy_from_slice(&payload[..512 * 1024])),
                Ok(Bytes::copy_from_slice(&payload[512 * 1024..])),
            ])))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().as_ref(), payload.as_slice());
        let (uri, received_len) = received.await.unwrap();
        assert_eq!(uri, "/upload?large=yes");
        assert_eq!(received_len, payload.len());
    }

    #[tokio::test]
    async fn remote_owner_bridges_text_binary_ping_pong_and_graceful_close() {
        let (upstream, observed) = spawn_cross_node_websocket_upstream().await;
        let cluster =
            TestShareCluster::start_with_upstream(upstream, ShareVisibility::Public).await;
        let mut request = format!("ws://{}/socket?keep=this", cluster.client_addr)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert(HOST, HeaderValue::from_static(SHARE_HOST));
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("chat, alternate"),
        );
        request
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://client.example"));
        request
            .headers_mut()
            .insert(COOKIE, HeaderValue::from_static("session=guest"));
        request
            .headers_mut()
            .insert(AUTHORIZATION, HeaderValue::from_static("******"));
        request.headers_mut().insert(
            "x-tarit-share-token",
            HeaderValue::from_static("must-not-reach-guest"),
        );

        let (mut client, response) = connect_async(request).await.unwrap();
        assert_eq!(
            response.headers().get(SEC_WEBSOCKET_PROTOCOL).unwrap(),
            "chat"
        );
        wait_for_websocket_gauge(&cluster.client_metrics, 1).await;
        wait_for_websocket_gauge(&cluster.owner_metrics, 1).await;

        client
            .send(TungsteniteMessage::Text("through-owner".into()))
            .await
            .unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            TungsteniteMessage::Text("through-owner".into())
        );
        client
            .send(TungsteniteMessage::Binary(Bytes::from_static(
                b"through-owner",
            )))
            .await
            .unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            TungsteniteMessage::Binary(Bytes::from_static(b"through-owner"))
        );
        client
            .send(TungsteniteMessage::Ping(Bytes::from_static(b"ping")))
            .await
            .unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            TungsteniteMessage::Pong(Bytes::from_static(b"ping"))
        );
        client
            .send(TungsteniteMessage::Pong(Bytes::from_static(b"pong")))
            .await
            .unwrap();
        client.send(TungsteniteMessage::Close(None)).await.unwrap();
        assert!(matches!(
            client.next().await.unwrap().unwrap(),
            TungsteniteMessage::Close(_)
        ));

        wait_for_websocket_gauge(&cluster.client_metrics, 0).await;
        wait_for_websocket_gauge(&cluster.owner_metrics, 0).await;
        let observed = observed.await.unwrap();
        for frame in ["text", "binary", "ping", "pong", "close"] {
            assert!(
                observed.contains(&frame),
                "upstream did not receive {frame}"
            );
        }
    }

    #[tokio::test]
    async fn remote_owner_abrupt_disconnect_releases_both_node_gauges() {
        let (upstream, observed) = spawn_permissive_websocket_echo_upstream().await;
        let cluster =
            TestShareCluster::start_with_upstream(upstream, ShareVisibility::Public).await;
        let mut request = format!("ws://{}/socket", cluster.client_addr)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert(HOST, HeaderValue::from_static(SHARE_HOST));
        request
            .headers_mut()
            .insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("chat"));

        let (client, _) = connect_async(request).await.unwrap();
        wait_for_websocket_gauge(&cluster.client_metrics, 1).await;
        wait_for_websocket_gauge(&cluster.owner_metrics, 1).await;
        drop(client);

        wait_for_websocket_gauge(&cluster.client_metrics, 0).await;
        wait_for_websocket_gauge(&cluster.owner_metrics, 0).await;
        let observed = observed.await.unwrap();
        assert!(
            observed.contains(&"close") || observed.contains(&"disconnect"),
            "upstream did not observe bridge termination: {observed:?}"
        );
    }

    #[tokio::test]
    async fn remote_owner_rejects_stale_local_state_after_authoritative_reassignment() {
        let cluster = TestShareCluster::start().await;
        assert!(
            cluster
                .owner_state
                .supervisor
                .is_running(cluster.share.vm_id),
            "the stale owner must still appear locally running"
        );
        crate::cluster::set_test_authoritative_owner(
            "owner",
            cluster.share.vm_id,
            "http://127.0.0.1:9".into(),
        );

        let response = cluster.request_through_non_owner("/stream").await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn remote_owner_rechecks_the_authoritative_share_version() {
        let cluster = TestShareCluster::start().await;
        let mut updated = cluster.share.clone();
        updated.token_version += 1;
        cluster.owner_shares.update(&updated).await.unwrap();

        let response = cluster.request_through_non_owner("/stream").await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn remote_share_rejects_dot_segment_paths() {
        let cluster = TestShareCluster::start().await;
        for path in ["/../../vms", "/%2e%2e/%2E%2e/vms"] {
            let response = gateway_router(cluster.client_state.clone())
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(HOST, SHARE_HOST)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
    }

    struct TestShareCluster {
        client_state: AppState,
        owner_state: AppState,
        client_addr: SocketAddr,
        owner_addr: SocketAddr,
        share_id: Uuid,
        share: ShareRecord,
        owner_shares: ShareRepository,
        client_metrics: Arc<Metrics>,
        owner_metrics: Arc<Metrics>,
        owner_tenants: mpsc::UnboundedReceiver<String>,
        release_second_chunk: Option<oneshot::Sender<()>>,
    }

    impl TestShareCluster {
        async fn start() -> Self {
            let (upstream, release_second_chunk, _upstream_complete) =
                spawn_delayed_streaming_http_upstream().await;
            Self::start_with(
                upstream,
                ShareVisibility::Public,
                Some(release_second_chunk),
            )
            .await
        }

        async fn start_with_upstream(upstream: SocketAddr, visibility: ShareVisibility) -> Self {
            Self::start_with(upstream, visibility, None).await
        }

        async fn start_with(
            upstream: SocketAddr,
            visibility: ShareVisibility,
            release_second_chunk: Option<oneshot::Sender<()>>,
        ) -> Self {
            let owner = gateway_test_state_for_host("owner");
            let owner_state = owner.clone();
            let owner_metrics = Arc::clone(&owner.metrics);
            let owner_shares = owner.shares.clone();
            let share = install_gateway_share(&owner, upstream, visibility).await;
            let (owner_tenant_tx, owner_tenants) = mpsc::unbounded_channel();
            let owner_addr = start_axum(crate::internal::internal_router(owner).layer(
                axum::middleware::from_fn(
                    move |request: Request<Body>, next: axum::middleware::Next| {
                        let owner_tenant_tx = owner_tenant_tx.clone();
                        async move {
                            if let Some(tenant) = request
                                .headers()
                                .get("x-tarit-tenant")
                                .and_then(|value| value.to_str().ok())
                            {
                                let _ = owner_tenant_tx.send(tenant.to_string());
                            }
                            next.run(request).await
                        }
                    },
                ),
            ))
            .await;

            let client = gateway_test_state_for_host("non-owner");
            let client_metrics = Arc::clone(&client.metrics);
            client.shares.insert(&share).await.unwrap();
            let client_addr = start_axum(gateway_router(client.clone())).await;
            crate::cluster::set_test_authoritative_owner(
                "non-owner",
                share.vm_id,
                format!("http://{owner_addr}"),
            );

            Self {
                client_state: client,
                owner_state,
                client_addr,
                owner_addr,
                share_id: share.id,
                share,
                owner_shares,
                client_metrics,
                owner_metrics,
                owner_tenants,
                release_second_chunk,
            }
        }

        async fn request_through_non_owner(&self, path: &str) -> reqwest::Response {
            self.request_through_non_owner_with_header(path, "x-request-id", "test")
                .await
        }

        async fn request_through_non_owner_with_header(
            &self,
            path: &str,
            header: &str,
            value: &str,
        ) -> reqwest::Response {
            reqwest::Client::new()
                .get(format!("http://{}{}", self.client_addr, path))
                .header(HOST, SHARE_HOST)
                .header(header, value)
                .send()
                .await
                .unwrap()
        }

        async fn owner_tenant(&mut self) -> String {
            self.owner_tenants.recv().await.unwrap()
        }

        fn owner_share_url(&self, path: &str) -> String {
            format!(
                "http://{}/internal/v1/shares/{}{}",
                self.owner_addr, self.share_id, path
            )
        }

        fn release_second_chunk(&mut self) {
            self.release_second_chunk.take().unwrap().send(()).unwrap();
        }
    }

    fn gateway_test_state() -> AppState {
        gateway_test_state_for_host("test-host")
    }

    fn signed_share_identity_headers(share: &ShareRecord) -> HeaderMap {
        let issued_at = Utc::now().timestamp();
        let nonce = Uuid::new_v4().to_string();
        let identity_id = super::share_peer_identity_id(share);
        let mut mac = Hmac::<Sha256>::new_from_slice(b"peer-secret").unwrap();
        mac.update(b"tarit-peer-identity-v1\n");
        mac.update(issued_at.to_string().as_bytes());
        mac.update(b"\n");
        mac.update(nonce.as_bytes());
        mac.update(b"\n");
        mac.update(share.owner_key.as_bytes());
        mac.update(b"\nuser\n");
        mac.update(identity_id.as_bytes());
        let mut headers = HeaderMap::new();
        headers.insert("x-peer-secret", HeaderValue::from_static("peer-secret"));
        headers.insert(
            "x-tarit-tenant",
            HeaderValue::from_str(&share.owner_key).unwrap(),
        );
        headers.insert("x-tarit-role", HeaderValue::from_static("user"));
        headers.insert(
            "x-tarit-api-key-id",
            HeaderValue::from_str(&identity_id).unwrap(),
        );
        headers.insert(
            "x-tarit-identity-timestamp",
            HeaderValue::from_str(&issued_at.to_string()).unwrap(),
        );
        headers.insert(
            "x-tarit-identity-nonce",
            HeaderValue::from_str(&nonce).unwrap(),
        );
        headers.insert(
            "x-tarit-identity-signature",
            HeaderValue::from_str(&URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())).unwrap(),
        );
        headers
    }

    fn gateway_test_state_for_host(host_id: &str) -> AppState {
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            api_keys: ApiKeyRegistry::from_plaintext_entries(vec![(
                "test-key".into(),
                "tenant-a".into(),
                ApiRole::Admin,
                0,
            )])
            .unwrap(),
            host_id: host_id.into(),
            vmm_bin: PathBuf::from("target/taritd-gateway-test/vmm"),
            kernel: PathBuf::from("target/taritd-gateway-test/kernel"),
            rootfs: PathBuf::from("target/taritd-gateway-test/rootfs"),
            socket_dir: PathBuf::from("target/taritd-gateway-test/sockets"),
            db_path: PathBuf::from("target/taritd-gateway-test/fleet.db"),
            net_state_path: PathBuf::from("target/taritd-gateway-test/net-state.json"),
            images_dir: PathBuf::from("target/taritd-gateway-test/images"),
            max_vms: 4,
            max_vcpus: 4,
            max_memory_mib: 1024,
            peer_secret: "peer-secret".into(),
            database_url: None,
            rpc_addr: "http://127.0.0.1:0".into(),
            enable_net: false,
            rootfs_read_only: false,
            metrics_expose_tenant_labels: false,
            vm_cgroup_parent: None,
            vm_cgroup_pids_max: 1024,
            warm_pool: WarmPoolConfig::default(),
            admission_timeout_ms: 1,
            reap_on_shutdown: true,
            region: "local".into(),
            zone: "local".into(),
            cloud: "onprem".into(),
            autoscale: AutoscaleConfig::default(),
            ssh_gateway_enabled: false,
            ssh_gateway_addr: "127.0.0.1:0".parse().unwrap(),
            ssh_gateway_host_key_path: PathBuf::from("target/taritd-gateway-test/ssh_host"),
            share_listen: None,
            share_domain: Some("shares.example.com".into()),
            share_token_key: Some([7; 32]),
            share_token_ttl_secs: 300,
            share_connect_timeout_ms: 1_000,
            share_idle_timeout_secs: 1,
        };
        let store = Arc::new(Mutex::new(tarit_store::Store::open(":memory:").unwrap()));
        let shares = ShareRepository::new(Arc::clone(&store), None);
        let (store_tx, _store_rx) = tokio::sync::mpsc::unbounded_channel();
        let peer = std::thread::spawn(|| PeerClient::new("peer-secret".into()))
            .join()
            .unwrap();
        AppState {
            config: config.clone(),
            audit_outbox: Arc::new(LocalAuditOutbox::new(Arc::clone(&store))),
            store,
            exec_cache: Arc::new(std::sync::RwLock::new(HashMap::new())),
            vm_cache: Arc::new(std::sync::RwLock::new(HashMap::new())),
            store_tx,
            pty_registry: Arc::new(PtyRegistry::default()),
            supervisor: Arc::new(VmmSupervisor::new(config.clone())),
            scheduler: Arc::new(Scheduler::new(config)),
            peer: Arc::new(peer),
            shares,
            fleet: None,
            metrics: Arc::new(Metrics::default()),
        }
    }

    fn gateway_share(vm_id: Uuid, guest_port: u16, visibility: ShareVisibility) -> ShareRecord {
        let now = Utc::now();
        ShareRecord {
            id: Uuid::new_v4(),
            slug: "calm-red-fox".into(),
            owner_key: "tenant-a".into(),
            vm_id,
            guest_port,
            visibility,
            token_version: 0,
            revoked_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    async fn install_gateway_share(
        state: &AppState,
        upstream: SocketAddr,
        visibility: ShareVisibility,
    ) -> ShareRecord {
        let vm_id = Uuid::new_v4();
        let share = gateway_share(vm_id, upstream.port(), visibility);
        state.supervisor.install_test_network_allocation(
            vm_id,
            NetAlloc {
                idx: 0,
                vm_id,
                tap: "test-tap".into(),
                host_ip: "127.0.0.1".into(),
                guest_ip: upstream.ip().to_string(),
                prefix: 30,
            },
        );
        state.shares.insert(&share).await.unwrap();
        share
    }

    async fn wait_for_websocket_gauge(metrics: &Metrics, expected: u64) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while metrics.active_share_websockets() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "active share websocket gauge did not reach {expected}; got {}",
                metrics.active_share_websockets()
            )
        });
    }

    async fn spawn_inspecting_http_upstream() -> (SocketAddr, oneshot::Receiver<(HeaderMap, Uri)>) {
        let (received_tx, received_rx) = oneshot::channel();
        let received_tx = Arc::new(Mutex::new(Some(received_tx)));
        let app = Router::new()
            .route(
                "/inspect",
                get(
                    |State(tx): State<Arc<Mutex<Option<oneshot::Sender<(HeaderMap, Uri)>>>>>,
                     headers: HeaderMap,
                     uri: Uri| async move {
                        if let Some(tx) = tx.lock().unwrap().take() {
                            let _ = tx.send((headers, uri));
                        }
                        Response::new(Body::from("ok"))
                    },
                ),
            )
            .with_state(received_tx);
        (start_axum(app).await, received_rx)
    }

    async fn spawn_root_inspecting_http_upstream() -> (SocketAddr, oneshot::Receiver<String>) {
        let (received_tx, received_rx) = oneshot::channel();
        let received_tx = Arc::new(Mutex::new(Some(received_tx)));
        let app =
            Router::new()
                .route(
                    "/",
                    get(
                        |State(tx): State<Arc<Mutex<Option<oneshot::Sender<String>>>>>,
                         uri: Uri| async move {
                            if let Some(tx) = tx.lock().unwrap().take() {
                                let _ = tx.send(uri.to_string());
                            }
                            Response::new(Body::from("ok"))
                        },
                    ),
                )
                .with_state(received_tx);
        (start_axum(app).await, received_rx)
    }

    async fn spawn_first_chunk_observing_upstream() -> (SocketAddr, oneshot::Receiver<Bytes>) {
        let (first_chunk_tx, first_chunk_rx) = oneshot::channel();
        let first_chunk_tx = Arc::new(Mutex::new(Some(first_chunk_tx)));
        let app =
            Router::new()
                .route(
                    "/upload",
                    post(
                        |State(tx): State<Arc<Mutex<Option<oneshot::Sender<Bytes>>>>>,
                         body: Body| async move {
                            let mut body = body.into_data_stream();
                            let first = body.next().await.unwrap().unwrap();
                            if let Some(tx) = tx.lock().unwrap().take() {
                                let _ = tx.send(first.clone());
                            }
                            let mut uploaded = first.to_vec();
                            while let Some(chunk) = body.next().await {
                                uploaded.extend_from_slice(&chunk.unwrap());
                            }
                            Response::new(Body::from(uploaded))
                        },
                    ),
                )
                .with_state(first_chunk_tx);
        (start_axum(app).await, first_chunk_rx)
    }

    async fn spawn_delayed_streaming_http_upstream(
    ) -> (SocketAddr, oneshot::Sender<()>, oneshot::Receiver<()>) {
        let (release_tx, release_rx) = oneshot::channel();
        let (complete_tx, complete_rx) = oneshot::channel();
        let controls = Arc::new(Mutex::new(Some((release_rx, complete_tx))));
        let app = Router::new().route(
            "/stream",
            get({
                let controls = Arc::clone(&controls);
                move || {
                    let controls = Arc::clone(&controls);
                    async move {
                        let (release_rx, complete_tx) = controls.lock().unwrap().take().unwrap();
                        let (chunk_tx, chunk_rx) = mpsc::channel::<Bytes>(2);
                        tokio::spawn(async move {
                            let _ = chunk_tx.send(Bytes::from_static(b"first")).await;
                            let _ = release_rx.await;
                            let _ = chunk_tx.send(Bytes::from_static(b"second")).await;
                            let _ = complete_tx.send(());
                        });
                        Response::new(Body::from_stream(futures_util::stream::unfold(
                            chunk_rx,
                            |mut chunk_rx| async move {
                                chunk_rx
                                    .recv()
                                    .await
                                    .map(|chunk| (Ok::<_, Infallible>(chunk), chunk_rx))
                            },
                        )))
                    }
                }
            }),
        );
        (start_axum(app).await, release_tx, complete_rx)
    }

    async fn spawn_streaming_http_upstream() -> (SocketAddr, oneshot::Receiver<HeaderMap>) {
        let (headers_tx, headers_rx) = oneshot::channel();
        let headers_tx = Arc::new(Mutex::new(Some(headers_tx)));
        let app = Router::new()
            .route(
                "/stream",
                post(
                    |State(tx): State<Arc<Mutex<Option<oneshot::Sender<HeaderMap>>>>>,
                     headers: HeaderMap,
                     body: Body| async move {
                        if let Some(tx) = tx.lock().unwrap().take() {
                            let _ = tx.send(headers);
                        }
                        let mut body = body.into_data_stream();
                        let (tx, rx) = mpsc::channel::<Bytes>(2);
                        tokio::spawn(async move {
                            while let Some(chunk) = body.next().await {
                                if let Ok(chunk) = chunk {
                                    if tx.send(chunk).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        });
                        Response::new(Body::from_stream(futures_util::stream::unfold(
                            rx,
                            |mut rx| async move {
                                rx.recv()
                                    .await
                                    .map(|chunk| (Ok::<_, Infallible>(chunk), rx))
                            },
                        )))
                    },
                ),
            )
            .with_state(headers_tx);
        (start_axum(app).await, headers_rx)
    }

    async fn spawn_large_echo_upstream() -> (SocketAddr, oneshot::Receiver<(Uri, usize)>) {
        let (received_tx, received_rx) = oneshot::channel();
        let received_tx = Arc::new(Mutex::new(Some(received_tx)));
        let app = Router::new()
            .route(
                "/upload",
                post(
                    |State(tx): State<Arc<Mutex<Option<oneshot::Sender<(Uri, usize)>>>>>,
                     uri: Uri,
                     body: Body| async move {
                        let mut body = body.into_data_stream();
                        let mut bytes = Vec::new();
                        while let Some(chunk) = body.next().await {
                            bytes.extend_from_slice(&chunk.unwrap());
                        }
                        if let Some(tx) = tx.lock().unwrap().take() {
                            let _ = tx.send((uri, bytes.len()));
                        }
                        Response::new(Body::from(bytes))
                    },
                ),
            )
            .with_state(received_tx);
        (start_axum(app).await, received_rx)
    }

    async fn spawn_upload_draining_upstream() -> SocketAddr {
        let app = Router::new().route(
            "/upload",
            post(|body: Body| async move {
                let mut body = body.into_data_stream();
                while let Some(chunk) = body.next().await {
                    chunk.unwrap();
                }
                Response::new(Body::from("ok"))
            }),
        );
        start_axum(app).await
    }

    async fn start_gateway_router(
        upstream: SocketAddr,
        idle: Duration,
    ) -> (SocketAddr, Arc<Metrics>) {
        let metrics = Arc::new(Metrics::default());
        let app = Router::new().route(
            "/socket",
            get({
                let metrics = Arc::clone(&metrics);
                move |headers: HeaderMap, ws: WebSocketUpgrade| {
                    let metrics = Arc::clone(&metrics);
                    async move {
                        proxy_websocket_to_target(
                            UpstreamTarget::new(upstream.ip(), upstream.port()),
                            "/echo?keep=this",
                            ws,
                            vec!["chat".into(), "alternate".into()],
                            &headers,
                            &TrustedForwarding {
                                peer: Some("203.0.113.9:443".parse().unwrap()),
                                host: SHARE_HOST.into(),
                            },
                            Duration::from_secs(1),
                            idle,
                            None,
                            metrics,
                        )
                        .await
                        .unwrap()
                    }
                }
            }),
        );
        (start_axum(app).await, metrics)
    }

    async fn start_axum(app: Router) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    async fn spawn_websocket_echo_upstream() -> (SocketAddr, oneshot::Receiver<Vec<&'static str>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (observed_tx, observed_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_hdr_async(
                stream,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                assert_eq!(
                    request
                        .headers()
                        .get("sec-websocket-protocol")
                        .unwrap(),
                    "chat, alternate"
                );
                assert_eq!(
                    request.headers().get(ORIGIN).unwrap(),
                    "https://client.example"
                );
                assert_eq!(request.headers().get(COOKIE).unwrap(), "session=guest");
                assert_eq!(
                    request.headers().get(AUTHORIZATION).unwrap(),
                    "Bearer application-credential"
                );
                assert!(request.headers().get("x-tarit-share-token").is_none());
                assert_eq!(
                    request.headers().get(FORWARDED).unwrap(),
                    "for=203.0.113.9;host=calm-red-fox.shares.example.com;proto=http"
                );
                assert_eq!(
                    request.headers().get("x-forwarded-for").unwrap(),
                    "203.0.113.9"
                );
                response.headers_mut().insert(
                    "sec-websocket-protocol",
                    tokio_tungstenite::tungstenite::http::HeaderValue::from_static("chat"),
                );
                Ok(response)
            },
            )
            .await
            .unwrap();
            let mut observed = Vec::new();
            while let Some(message) = socket.next().await {
                match message.unwrap() {
                    TungsteniteMessage::Text(text) => {
                        observed.push("text");
                        socket.send(TungsteniteMessage::Text(text)).await.unwrap();
                    }
                    TungsteniteMessage::Binary(bytes) => {
                        observed.push("binary");
                        socket
                            .send(TungsteniteMessage::Binary(bytes))
                            .await
                            .unwrap();
                    }
                    TungsteniteMessage::Ping(bytes) => {
                        observed.push("ping");
                        let _ = bytes;
                    }
                    TungsteniteMessage::Pong(_) => observed.push("pong"),
                    TungsteniteMessage::Close(frame) => {
                        observed.push("close");
                        let _ = frame;
                        socket.flush().await.unwrap();
                        break;
                    }
                    _ => {}
                }
            }
            let _ = observed_tx.send(observed);
        });
        (addr, observed_rx)
    }

    async fn spawn_cross_node_websocket_upstream(
    ) -> (SocketAddr, oneshot::Receiver<Vec<&'static str>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (observed_tx, observed_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_hdr_async(
                stream,
                |_request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        tokio_tungstenite::tungstenite::http::HeaderValue::from_static("chat"),
                    );
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let mut observed = Vec::new();
            while let Some(message) = socket.next().await {
                match message.unwrap() {
                    TungsteniteMessage::Text(text) => {
                        observed.push("text");
                        socket.send(TungsteniteMessage::Text(text)).await.unwrap();
                    }
                    TungsteniteMessage::Binary(bytes) => {
                        observed.push("binary");
                        socket
                            .send(TungsteniteMessage::Binary(bytes))
                            .await
                            .unwrap();
                    }
                    TungsteniteMessage::Ping(bytes) => {
                        observed.push("ping");
                        socket.send(TungsteniteMessage::Pong(bytes)).await.unwrap();
                    }
                    TungsteniteMessage::Pong(_) => observed.push("pong"),
                    TungsteniteMessage::Close(frame) => {
                        observed.push("close");
                        let _ = frame;
                        socket.flush().await.unwrap();
                        break;
                    }
                    _ => {}
                }
            }
            let _ = observed_tx.send(observed);
        });
        (addr, observed_rx)
    }

    async fn spawn_permissive_websocket_echo_upstream(
    ) -> (SocketAddr, oneshot::Receiver<Vec<&'static str>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (observed_tx, observed_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_hdr_async(
                stream,
                |_request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        tokio_tungstenite::tungstenite::http::HeaderValue::from_static("chat"),
                    );
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let mut observed = Vec::new();
            while let Some(message) = socket.next().await {
                let message = match message {
                    Ok(message) => message,
                    Err(_) => {
                        observed.push("disconnect");
                        break;
                    }
                };
                match message {
                    TungsteniteMessage::Binary(bytes) => {
                        observed.push("binary");
                        socket
                            .send(TungsteniteMessage::Binary(bytes))
                            .await
                            .unwrap();
                    }
                    TungsteniteMessage::Close(frame) => {
                        observed.push("close");
                        let _ = socket.send(TungsteniteMessage::Close(frame)).await;
                        break;
                    }
                    _ => {}
                }
            }
            let _ = observed_tx.send(observed);
        });
        (addr, observed_rx)
    }

    async fn spawn_websocket_drop_upstream() -> (SocketAddr, oneshot::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (drop_tx, drop_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = accept_hdr_async(
                stream,
                |_request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        tokio_tungstenite::tungstenite::http::HeaderValue::from_static("chat"),
                    );
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let _ = drop_rx.await;
            drop(socket);
        });
        (addr, drop_tx)
    }
}
