#![allow(dead_code)]

use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{CloseFrame as AxumCloseFrame, Message as AxumMessage, WebSocket, WebSocketUpgrade},
        ConnectInfo, FromRequestParts, State,
    },
    http::{
        header::{
            HeaderName, HeaderValue, AUTHORIZATION, CONNECTION, FORWARDED, HOST,
            PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL, UPGRADE,
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

use crate::{api::AppState, net::NetAlloc, shares, supervisor::NetworkLease};

type UpstreamWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const MAX_PENDING_PINGS: usize = 64;

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
struct TrustedForwarding {
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
enum GatewayError {
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
        .route("/", any(handle_request))
        .route("/{*path}", any(handle_request))
        .with_state(state)
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
        let token = bearer_token(request.headers())?;
        let share = shares::authorize_gateway(&state, &slug, token.as_deref()).await?;
        let forwarding = TrustedForwarding {
            peer,
            host: format!("{slug}.{domain}"),
        };
        let (mut parts, body) = request.into_parts();

        if is_websocket_request(&parts) {
            let protocols = requested_subprotocols(&parts.headers)?;
            let websocket = WebSocketUpgrade::from_request_parts(&mut parts, &state)
                .await
                .map_err(|_| GatewayError::NotFound)?;
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
        } else {
            parts.extensions.insert(forwarding);
            proxy_local_http(&state, &share, Request::from_parts(parts, body)).await
        }
    }
    .await;

    result.unwrap_or_else(IntoResponse::into_response)
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

fn bearer_token(headers: &axum::http::HeaderMap) -> Result<Option<String>, GatewayError> {
    let values = headers.get_all(AUTHORIZATION);
    if values.iter().count() > 1 {
        return Err(GatewayError::Unauthorized);
    }
    let Some(value) = values.iter().next() else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| GatewayError::Unauthorized)?;
    let token = value
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty() && !token.bytes().any(|byte| byte.is_ascii_whitespace()))
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
                    return Err(GatewayError::Unavailable);
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
        || name == AUTHORIZATION
        || name == PROXY_AUTHORIZATION
        || name == PROXY_AUTHENTICATE
        || name == FORWARDED
        || name.as_str().starts_with("x-forwarded-")
        || name.as_str() == "x-real-ip"
}

fn should_strip_response_header(
    name: &HeaderName,
    connection_headers: &HashSet<HeaderName>,
) -> bool {
    is_hop_by_hop(name, connection_headers)
        || name == PROXY_AUTHENTICATE
        || name == PROXY_AUTHORIZATION
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
    let protocol = negotiated_protocol(response.headers().get(SEC_WEBSOCKET_PROTOCOL), &protocols)?;
    let websocket = if let Some(protocol) = protocol {
        websocket.protocols([protocol])
    } else {
        websocket
    };
    Ok(websocket.on_upgrade(move |client| async move {
        let _lease = lease;
        bridge_websocket(client, upstream, idle_timeout).await;
    }))
}

fn negotiated_protocol(
    response_protocol: Option<&HeaderValue>,
    requested: &[String],
) -> Result<Option<String>, GatewayError> {
    let Some(response_protocol) = response_protocol else {
        return Ok(None);
    };
    let protocol = response_protocol
        .to_str()
        .ok()
        .filter(|protocol| protocol.bytes().all(is_websocket_token_byte))
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
        proxy_http_to_target, proxy_websocket_to_target, share_slug, TrustedForwarding,
        UpstreamTarget,
    };
    use axum::{
        body::{Body, Bytes},
        extract::{ws::WebSocketUpgrade, State},
        http::{
            header::{
                AUTHORIZATION, CONNECTION, COOKIE, FORWARDED, HOST, ORIGIN, PROXY_AUTHORIZATION,
                SEC_WEBSOCKET_PROTOCOL,
            },
            HeaderMap, HeaderValue, Method, Request, StatusCode,
        },
        response::Response,
        routing::{get, post},
        Router,
    };
    use futures_util::{SinkExt, StreamExt};
    use std::{
        convert::Infallible,
        net::SocketAddr,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::{
        net::TcpListener,
        sync::{mpsc, oneshot},
    };
    use tokio_tungstenite::{
        accept_hdr_async, connect_async,
        tungstenite::{client::IntoClientRequest, protocol::Message as TungsteniteMessage},
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
        assert!(headers.get(AUTHORIZATION).is_none());
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
            Duration::from_millis(25),
            None,
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bridges_websocket_frames_and_negotiates_the_upstream_protocol() {
        let (upstream, observed) = spawn_websocket_echo_upstream().await;
        let gateway = start_gateway_router(upstream, Duration::from_secs(1)).await;
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

        let (mut client, response) = connect_async(request).await.unwrap();
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
    }

    #[tokio::test]
    async fn websocket_idle_timeout_closes_an_inactive_bridge() {
        let (upstream, _observed) = spawn_websocket_echo_upstream().await;
        let gateway = start_gateway_router(upstream, Duration::from_millis(25)).await;
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
        let (mut client, _) = connect_async(request).await.unwrap();

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), client.next())
                .await
                .unwrap(),
            Some(Ok(TungsteniteMessage::Close(_))) | None
        ));
    }

    #[tokio::test]
    async fn upstream_eof_closes_client_without_waiting_for_idle_timeout() {
        let upstream = spawn_websocket_drop_upstream().await;
        let gateway = start_gateway_router(upstream, Duration::from_secs(1)).await;
        let (mut client, _) = connect_async(format!("ws://{gateway}/socket"))
            .await
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(100), client.next())
                .await
                .is_ok()
        );
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

    async fn start_gateway_router(upstream: SocketAddr, idle: Duration) -> SocketAddr {
        let app = Router::new().route(
            "/socket",
            get(move |headers: HeaderMap, ws: WebSocketUpgrade| async move {
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
                )
                .await
                .unwrap()
            }),
        );
        start_axum(app).await
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

    async fn spawn_websocket_drop_upstream() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
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
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(socket);
        });
        addr
    }
}
