use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Extension, Path, Query, State,
    },
    http::StatusCode,
    response::Response,
    Json,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tarit_types::OrchError;
use tarit_vmm_client::{
    PtyResize, PtyStreamFrame, MAX_FRAME_LEN, TYPE_DATA, TYPE_ERROR, TYPE_EXIT, TYPE_RESIZE,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

use crate::{
    api::{ensure_vm_access, ApiError, AppState},
    cluster::{self, Owner},
    config::ApiIdentity,
    ops,
};

#[derive(Debug, Default)]
pub(crate) struct PtyRegistry {
    sessions: Mutex<HashMap<Uuid, PtySession>>,
}

const CONNECT_TOKEN_TTL_SECS: i64 = 60;

#[derive(Debug, Clone)]
pub(crate) struct PtySession {
    pty_id: Uuid,
    vm_id: Uuid,
    cols: u16,
    rows: u16,
    shell: Option<String>,
    created_at: DateTime<Utc>,
    /// Unguessable, per-session token issued to the authenticated caller that
    /// created this session. The WebSocket upgrade authenticates with this
    /// token instead of the caller's long-lived API key, so the account
    /// credential never appears in a URL, proxy log, or browser history.
    connect_token: String,
    /// Identity of the caller that created the session, used to re-check VM
    /// access when the WebSocket connects.
    owner: ApiIdentity,
    connect_token_expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreatePtySessionRequest {
    cols: u16,
    rows: u16,
    #[serde(default)]
    shell: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResizePtySessionRequest {
    cols: u16,
    rows: u16,
}

#[derive(Debug, Serialize)]
pub(crate) struct CreatePtySessionResponse {
    pty_id: Uuid,
    cols: u16,
    rows: u16,
    /// Short-lived token to authenticate the WebSocket upgrade for this
    /// session. Pass it as the `token` query parameter on the connect URL.
    connect_token: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct PtyResizeResponse {
    pty_id: Uuid,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Serialize)]
pub(crate) struct PtySessionResponse {
    pty_id: Uuid,
    vm_id: Uuid,
    cols: u16,
    rows: u16,
    shell: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListPtySessionsResponse {
    sessions: Vec<PtySessionResponse>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WsQuery {
    /// Per-session PTY connect token returned by the create-session call.
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsControl {
    Resize { cols: u16, rows: u16 },
}

impl PtyRegistry {
    pub(crate) fn create(
        &self,
        vm_id: Uuid,
        cols: u16,
        rows: u16,
        shell: Option<String>,
        owner: ApiIdentity,
    ) -> Result<PtySession, OrchError> {
        validate_dimensions(cols, rows)?;
        let session = PtySession {
            pty_id: Uuid::new_v4(),
            vm_id,
            cols,
            rows,
            shell,
            created_at: Utc::now(),
            connect_token: generate_connect_token(),
            owner,
            connect_token_expires_at: Utc::now() + ChronoDuration::seconds(CONNECT_TOKEN_TTL_SECS),
        };
        self.sessions
            .lock()
            .map_err(|_| OrchError::Internal("pty registry lock".into()))?
            .insert(session.pty_id, session.clone());
        Ok(session)
    }

    pub(crate) fn list(&self, vm_id: Uuid) -> Vec<PtySession> {
        self.sessions
            .lock()
            .map(|sessions| {
                sessions
                    .values()
                    .filter(|session| session.vm_id == vm_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn get(&self, vm_id: Uuid, pty_id: Uuid) -> Result<PtySession, OrchError> {
        self.sessions
            .lock()
            .map_err(|_| OrchError::Internal("pty registry lock".into()))?
            .get(&pty_id)
            .filter(|session| session.vm_id == vm_id)
            .cloned()
            .ok_or_else(|| OrchError::NotFound(format!("pty session {pty_id} not found")))
    }

    pub(crate) fn consume_connect_token(
        &self,
        vm_id: Uuid,
        pty_id: Uuid,
        provided: &str,
    ) -> Result<PtySession, OrchError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| OrchError::Internal("pty registry lock".into()))?;
        let Some(session) = sessions
            .get(&pty_id)
            .filter(|session| session.vm_id == vm_id)
        else {
            return Err(OrchError::Unauthorized);
        };
        if Utc::now() > session.connect_token_expires_at {
            sessions.remove(&pty_id);
            return Err(OrchError::Unauthorized);
        }
        if provided.is_empty() || !connect_token_matches(provided, &session.connect_token) {
            return Err(OrchError::Unauthorized);
        }
        Ok(sessions
            .remove(&pty_id)
            .expect("session exists after token validation"))
    }

    pub(crate) fn delete(&self, vm_id: Uuid, pty_id: Uuid) -> Result<(), OrchError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| OrchError::Internal("pty registry lock".into()))?;
        match sessions.get(&pty_id) {
            Some(session) if session.vm_id == vm_id => {
                sessions.remove(&pty_id);
                Ok(())
            }
            _ => Err(OrchError::NotFound(format!(
                "pty session {pty_id} not found"
            ))),
        }
    }

    pub(crate) fn resize(
        &self,
        vm_id: Uuid,
        pty_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> Result<PtySession, OrchError> {
        validate_dimensions(cols, rows)?;
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| OrchError::Internal("pty registry lock".into()))?;
        let session = sessions
            .get_mut(&pty_id)
            .filter(|session| session.vm_id == vm_id)
            .ok_or_else(|| OrchError::NotFound(format!("pty session {pty_id} not found")))?;
        session.cols = cols;
        session.rows = rows;
        Ok(session.clone())
    }
}

pub(crate) async fn create_session(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(vm_id): Path<Uuid>,
    Json(req): Json<CreatePtySessionRequest>,
) -> Result<(StatusCode, Json<CreatePtySessionResponse>), ApiError> {
    ensure_local_vm_for_pty(&state, vm_id, &identity).await?;
    let session =
        state
            .pty_registry
            .create(vm_id, req.cols, req.rows, req.shell, identity.clone())?;
    Ok((
        StatusCode::CREATED,
        Json(CreatePtySessionResponse {
            pty_id: session.pty_id,
            cols: session.cols,
            rows: session.rows,
            connect_token: session.connect_token,
        }),
    ))
}

pub(crate) async fn list_sessions(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(vm_id): Path<Uuid>,
) -> Result<Json<ListPtySessionsResponse>, ApiError> {
    ensure_local_vm_for_pty(&state, vm_id, &identity).await?;
    let sessions = state
        .pty_registry
        .list(vm_id)
        .iter()
        .map(PtySessionResponse::from)
        .collect();
    Ok(Json(ListPtySessionsResponse { sessions }))
}

pub(crate) async fn get_session(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path((vm_id, pty_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<PtySessionResponse>, ApiError> {
    ensure_local_vm_for_pty(&state, vm_id, &identity).await?;
    let session = state.pty_registry.get(vm_id, pty_id)?;
    Ok(Json(PtySessionResponse::from(&session)))
}

pub(crate) async fn delete_session(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path((vm_id, pty_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    ensure_local_vm_for_pty(&state, vm_id, &identity).await?;
    state.pty_registry.delete(vm_id, pty_id)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn resize_session(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path((vm_id, pty_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<ResizePtySessionRequest>,
) -> Result<Json<PtyResizeResponse>, ApiError> {
    ensure_local_vm_for_pty(&state, vm_id, &identity).await?;
    let session = state
        .pty_registry
        .resize(vm_id, pty_id, req.cols, req.rows)?;
    Ok(Json(PtyResizeResponse {
        pty_id: session.pty_id,
        cols: session.cols,
        rows: session.rows,
    }))
}

pub(crate) async fn connect_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path((vm_id, pty_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<WsQuery>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state, vm_id, pty_id, query.token))
}

async fn handle_ws(
    mut socket: WebSocket,
    state: AppState,
    vm_id: Uuid,
    pty_id: Uuid,
    token: Option<String>,
) {
    let provided = token.unwrap_or_default();
    let session = match state
        .pty_registry
        .consume_connect_token(vm_id, pty_id, &provided)
    {
        Ok(session) => session,
        Err(OrchError::Unauthorized) => {
            close_ws(&mut socket, 4401, "unauthorized").await;
            return;
        }
        Err(e) => {
            close_ws(&mut socket, 1008, &e.to_string()).await;
            return;
        }
    };

    // Authenticate the upgrade with a one-time per-session connect token, not the
    // caller's long-lived API key. The token was handed to the authenticated
    // creator of this session, so we authorize against that recorded identity.
    let identity = session.owner.clone();

    if let Err(e) = ensure_local_vm_for_pty(&state, vm_id, &identity).await {
        close_ws(&mut socket, 1013, &e.to_string()).await;
        return;
    }

    let sup = Arc::clone(&state.supervisor);
    let shell = session.shell.clone();
    let attach = tokio::task::spawn_blocking(move || {
        sup.attach_pty(vm_id, session.cols, session.rows, shell)
    })
    .await;

    let stream = match attach {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            close_ws(&mut socket, 1011, &e.to_string()).await;
            return;
        }
        Err(e) => {
            close_ws(&mut socket, 1011, &format!("attach join: {e}")).await;
            return;
        }
    };

    if let Err(e) = stream.set_nonblocking(true) {
        close_ws(&mut socket, 1011, &format!("set nonblocking: {e}")).await;
        return;
    }
    let stream = match tokio::net::UnixStream::from_std(stream) {
        Ok(stream) => stream,
        Err(e) => {
            close_ws(&mut socket, 1011, &format!("wrap stream: {e}")).await;
            return;
        }
    };

    let (mut vmm_reader, mut vmm_writer) = stream.into_split();
    let (mut ws_sender, mut ws_receiver) = socket.split();

    let mut vmm_to_ws = tokio::spawn(async move {
        loop {
            let frame = read_vmm_stream_frame(&mut vmm_reader).await?;
            match frame.frame_type {
                TYPE_DATA => {
                    ws_sender
                        .send(Message::Binary(frame.payload.into()))
                        .await?;
                }
                TYPE_EXIT => {
                    let exit_code = decode_exit_code(&frame.payload)?;
                    let text =
                        serde_json::json!({ "type": "exit", "exit_code": exit_code }).to_string();
                    ws_sender.send(Message::Text(text.into())).await?;
                    let _ = ws_sender.send(Message::Close(None)).await;
                    break;
                }
                TYPE_ERROR => {
                    let msg = decode_error_message(frame.payload)?;
                    let _ = ws_sender.send(close_message(1011, &msg)).await;
                    break;
                }
                TYPE_RESIZE => {}
                _ => {}
            }
        }
        Ok::<(), BridgeError>(())
    });

    let mut ws_to_vmm = tokio::spawn(async move {
        while let Some(message) = ws_receiver.next().await {
            match message? {
                Message::Binary(data) => {
                    write_vmm_stream_frame(&mut vmm_writer, &data_frame(data.to_vec())).await?;
                }
                Message::Text(text) => {
                    if let Ok(WsControl::Resize { cols, rows }) =
                        serde_json::from_str::<WsControl>(text.as_str())
                    {
                        write_vmm_stream_frame(&mut vmm_writer, &resize_frame(cols, rows)).await?;
                    }
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
        Ok::<(), BridgeError>(())
    });

    tokio::select! {
        _ = &mut vmm_to_ws => {
            ws_to_vmm.abort();
        }
        _ = &mut ws_to_vmm => {
            vmm_to_ws.abort();
        }
    }
}

impl From<&PtySession> for PtySessionResponse {
    fn from(value: &PtySession) -> Self {
        Self {
            pty_id: value.pty_id,
            vm_id: value.vm_id,
            cols: value.cols,
            rows: value.rows,
            shell: value.shell.clone(),
            created_at: value.created_at,
        }
    }
}

async fn ensure_local_vm_for_pty(
    state: &AppState,
    vm_id: Uuid,
    identity: &ApiIdentity,
) -> Result<(), OrchError> {
    match cluster::resolve_owner(state, vm_id).await? {
        Owner::Local => {
            let vm = ops::get_local(state, vm_id)?;
            ensure_vm_access(identity, &vm)
        }
        // TODO: proxy the WebSocket to the peer returned here once the internal
        // peer API grows an upgrade/stream forwarding path.
        Owner::Remote(_) => Err(OrchError::Conflict(
            "PTY peer forwarding is not implemented yet; connect to the owning node".into(),
        )),
    }
}

fn validate_dimensions(cols: u16, rows: u16) -> Result<(), OrchError> {
    if cols == 0 || rows == 0 {
        return Err(OrchError::BadRequest(
            "PTY cols and rows must be greater than zero".into(),
        ));
    }
    Ok(())
}

/// Generate an unguessable per-session PTY connect token. Two v4 UUIDs give
/// roughly 244 bits of CSPRNG-backed entropy (uuid draws from `getrandom`),
/// rendered as hex without hyphens.
fn generate_connect_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Constant-time comparison of PTY connect tokens (compare fixed-length SHA-256
/// digests so neither length nor an early mismatch leaks through timing).
fn connect_token_matches(provided: &str, expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    let a = Sha256::digest(provided.as_bytes());
    let b = Sha256::digest(expected.as_bytes());
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn close_ws(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket.send(close_message(code, reason)).await;
}

fn close_message(code: u16, reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: truncate_close_reason(reason).into(),
    }))
}

fn truncate_close_reason(reason: &str) -> String {
    reason.chars().take(120).collect()
}

async fn read_vmm_stream_frame<R>(reader: &mut R) -> Result<PtyStreamFrame, BridgeError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 5];
    reader.read_exact(&mut header).await?;
    let frame_type = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(BridgeError::Protocol("stream frame too large".into()));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(PtyStreamFrame {
        frame_type,
        payload,
    })
}

async fn write_vmm_stream_frame<W>(
    writer: &mut W,
    frame: &PtyStreamFrame,
) -> Result<(), BridgeError>
where
    W: AsyncWrite + Unpin,
{
    if frame.payload.len() > u32::MAX as usize {
        return Err(BridgeError::Protocol("stream frame too large".into()));
    }
    let mut header = [0u8; 5];
    header[0] = frame.frame_type;
    header[1..5].copy_from_slice(&(frame.payload.len() as u32).to_be_bytes());
    writer.write_all(&header).await?;
    writer.write_all(&frame.payload).await?;
    writer.flush().await?;
    Ok(())
}

fn data_frame(data: Vec<u8>) -> PtyStreamFrame {
    PtyStreamFrame {
        frame_type: TYPE_DATA,
        payload: data,
    }
}

fn resize_frame(cols: u16, rows: u16) -> PtyStreamFrame {
    let payload =
        serde_json::to_vec(&PtyResize { cols, rows }).expect("serializing PtyResize cannot fail");
    PtyStreamFrame {
        frame_type: TYPE_RESIZE,
        payload,
    }
}

#[derive(Deserialize)]
struct ExitPayload {
    exit_code: i32,
}

fn decode_exit_code(payload: &[u8]) -> Result<i32, BridgeError> {
    serde_json::from_slice::<ExitPayload>(payload)
        .map(|exit| exit.exit_code)
        .map_err(|e| BridgeError::Protocol(format!("invalid exit frame: {e}")))
}

fn decode_error_message(payload: Vec<u8>) -> Result<String, BridgeError> {
    String::from_utf8(payload)
        .map_err(|e| BridgeError::Protocol(format!("invalid error frame: {e}")))
}

#[derive(Debug, thiserror::Error)]
enum BridgeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("websocket: {0}")]
    WebSocket(#[from] axum::Error),

    #[error("protocol: {0}")]
    Protocol(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiRole;

    fn test_identity() -> ApiIdentity {
        ApiIdentity {
            tenant: "tenant-a".into(),
            role: ApiRole::User,
            max_vms: None,
            api_key_id: "key-1".into(),
        }
    }

    #[test]
    fn registry_creates_resizes_lists_and_deletes_sessions() {
        let registry = PtyRegistry::default();
        let vm_id = Uuid::new_v4();
        let other_vm_id = Uuid::new_v4();

        let session = registry
            .create(vm_id, 80, 24, Some("/bin/bash".into()), test_identity())
            .unwrap();
        assert_eq!(session.cols, 80);
        assert_eq!(session.rows, 24);
        assert_eq!(registry.list(vm_id).len(), 1);
        assert!(registry.list(other_vm_id).is_empty());

        let resized = registry.resize(vm_id, session.pty_id, 120, 40).unwrap();
        assert_eq!(resized.cols, 120);
        assert_eq!(resized.rows, 40);

        registry.delete(vm_id, session.pty_id).unwrap();
        assert!(registry.get(vm_id, session.pty_id).is_err());
    }

    #[test]
    fn registry_rejects_zero_dimensions() {
        let registry = PtyRegistry::default();
        assert!(matches!(
            registry.create(Uuid::new_v4(), 0, 24, None, test_identity()),
            Err(OrchError::BadRequest(_))
        ));
    }

    #[test]
    fn connect_token_is_unguessable_and_matches_constant_time() {
        let registry = PtyRegistry::default();
        let vm_id = Uuid::new_v4();
        let session = registry
            .create(vm_id, 80, 24, None, test_identity())
            .unwrap();
        // A fresh token is long, hex, and unique per session.
        assert_eq!(session.connect_token.len(), 64);
        let other = registry
            .create(vm_id, 80, 24, None, test_identity())
            .unwrap();
        assert_ne!(session.connect_token, other.connect_token);
        // The matcher accepts the real token and rejects anything else.
        assert!(connect_token_matches(
            &session.connect_token,
            &session.connect_token
        ));
        assert!(!connect_token_matches("", &session.connect_token));
        assert!(!connect_token_matches(
            &other.connect_token,
            &session.connect_token
        ));
    }

    #[test]
    fn connect_token_can_only_be_consumed_once() {
        let registry = PtyRegistry::default();
        let vm_id = Uuid::new_v4();
        let session = registry
            .create(vm_id, 80, 24, None, test_identity())
            .unwrap();

        assert!(registry
            .consume_connect_token(vm_id, session.pty_id, &session.connect_token)
            .is_ok());
        assert!(matches!(
            registry.consume_connect_token(vm_id, session.pty_id, &session.connect_token),
            Err(OrchError::Unauthorized)
        ));
    }

    #[test]
    fn expired_connect_token_is_rejected_and_removed() {
        let registry = PtyRegistry::default();
        let vm_id = Uuid::new_v4();
        let session = registry
            .create(vm_id, 80, 24, None, test_identity())
            .unwrap();
        registry
            .sessions
            .lock()
            .unwrap()
            .get_mut(&session.pty_id)
            .unwrap()
            .connect_token_expires_at = Utc::now() - chrono::Duration::seconds(1);

        assert!(matches!(
            registry.consume_connect_token(vm_id, session.pty_id, &session.connect_token),
            Err(OrchError::Unauthorized)
        ));
        assert!(matches!(
            registry.get(vm_id, session.pty_id),
            Err(OrchError::NotFound(_))
        ));
    }
}
