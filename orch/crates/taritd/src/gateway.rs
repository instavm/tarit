use anyhow::{Context, Result};
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{self, Msg, Server as _, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tarit_types::{openssh_sha256_fingerprint, OrchError, SshKeyRecord, VmRecord};
use tarit_vmm_client::{
    PtyResize, PtyStreamFrame, MAX_FRAME_LEN, TYPE_DATA, TYPE_ERROR, TYPE_EXIT, TYPE_RESIZE,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::api::{store_err, AppState};
use crate::cluster::{self, Owner};

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

pub(crate) async fn run(state: AppState) -> Result<()> {
    let host_key = load_or_generate_host_key(&state.config.ssh_gateway_host_key_path)?;
    let mut methods = MethodSet::empty();
    methods.push(MethodKind::PublicKey);
    let config = Arc::new(server::Config {
        methods,
        auth_rejection_time: std::time::Duration::from_secs(1),
        auth_rejection_time_initial: Some(std::time::Duration::from_millis(0)),
        keys: vec![host_key],
        inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
        ..Default::default()
    });

    let listener = TcpListener::bind(state.config.ssh_gateway_addr)
        .await
        .with_context(|| format!("bind SSH gateway {}", state.config.ssh_gateway_addr))?;
    tracing::info!(
        addr = %state.config.ssh_gateway_addr,
        host_key = %state.config.ssh_gateway_host_key_path.display(),
        "SSH gateway listening"
    );

    let mut server = GatewayServer { state };
    server
        .run_on_socket(config, &listener)
        .await
        .context("SSH gateway server")
}

fn load_or_generate_host_key(path: &Path) -> Result<PrivateKey> {
    if path.exists() {
        let key = PrivateKey::read_openssh_file(path).with_context(|| {
            format!(
                "read SSH gateway host key {}: only Ed25519 OpenSSH private keys are supported; RSA host keys are not supported",
                path.display()
            )
        })?;
        if key.algorithm() != Algorithm::Ed25519 {
            anyhow::bail!(
                "SSH gateway host key {} must use Ed25519; RSA host keys are not supported",
                path.display()
            );
        }
        return Ok(key);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create SSH gateway host key dir {}", parent.display()))?;
    }

    let mut rng = russh::keys::key::safe_rng();
    let key = PrivateKey::random(&mut rng, Algorithm::Ed25519).context("generate SSH host key")?;
    key.write_openssh_file(path, russh::keys::ssh_key::LineEnding::LF)
        .with_context(|| format!("write SSH gateway host key {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(key)
}

struct GatewayServer {
    state: AppState,
}

impl server::Server for GatewayServer {
    type Handler = GatewayHandler;

    fn new_client(&mut self, peer_addr: Option<SocketAddr>) -> Self::Handler {
        GatewayHandler::new(self.state.clone(), peer_addr)
    }

    fn handle_session_error(&mut self, error: <Self::Handler as server::Handler>::Error) {
        tracing::debug!("SSH gateway session closed: {error}");
    }
}

struct GatewayHandler {
    state: AppState,
    peer_addr: Option<SocketAddr>,
    auth: Option<Authenticated>,
    channels: HashMap<ChannelId, ChannelState>,
}

impl GatewayHandler {
    fn new(state: AppState, peer_addr: Option<SocketAddr>) -> Self {
        Self {
            state,
            peer_addr,
            auth: None,
            channels: HashMap::new(),
        }
    }

    async fn start_pty(
        &mut self,
        channel: ChannelId,
        shell: Option<String>,
        session: &mut Session,
    ) -> Result<(), russh::Error> {
        tracing::debug!(?channel, "gateway: start_pty enter");
        if self
            .channels
            .get(&channel)
            .map(|state| state.started)
            .unwrap_or(false)
        {
            let _ = session.channel_failure(channel);
            return Ok(());
        }

        let Some(auth) = self.auth.clone() else {
            fail_channel_request(session, channel, "SSH session is not authenticated")?;
            return Ok(());
        };
        let (cols, rows) = self
            .channels
            .get(&channel)
            .map(|state| (state.cols, state.rows))
            .unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));

        let stream = match attach_authorized_pty(&self.state, &auth, cols, rows, shell).await {
            Ok(stream) => stream,
            Err(msg) => {
                tracing::warn!(?channel, vm_id = %auth.vm_id, "gateway: attach failed: {msg}");
                fail_channel_request(session, channel, &msg)?;
                return Ok(());
            }
        };
        tracing::debug!(?channel, vm_id = %auth.vm_id, "gateway: attach ok");

        if let Err(e) = stream.set_nonblocking(true) {
            fail_channel_request(
                session,
                channel,
                &format!("set PTY stream nonblocking: {e}"),
            )?;
            return Ok(());
        }
        let stream = match tokio::net::UnixStream::from_std(stream) {
            Ok(stream) => stream,
            Err(e) => {
                fail_channel_request(session, channel, &format!("wrap PTY stream: {e}"))?;
                return Ok(());
            }
        };

        session.channel_success(channel)?;
        let (reader, writer) = stream.into_split();
        let writer = Arc::new(AsyncMutex::new(writer));
        let handle = session.handle();
        let vm_id = auth.vm_id;
        let task = tokio::spawn(async move {
            if let Err(e) = vmm_to_ssh(reader, handle, channel).await {
                tracing::debug!(%vm_id, ?channel, "SSH gateway VMM->SSH bridge ended: {e}");
            }
        });

        let entry = self.channels.entry(channel).or_default();
        entry.started = true;
        entry.writer = Some(writer);
        entry.vmm_task = Some(task);
        Ok(())
    }

    async fn write_to_vmm(
        &mut self,
        channel: ChannelId,
        frame: PtyStreamFrame,
        session: &mut Session,
    ) -> Result<(), russh::Error> {
        let writer = self
            .channels
            .get(&channel)
            .and_then(|state| state.writer.clone());
        let Some(writer) = writer else {
            return Ok(());
        };

        if let Err(e) = write_vmm_stream_frame_locked(&writer, &frame).await {
            send_gateway_error(
                session,
                channel,
                &format!("PTY stream write failed: {e}"),
                1,
            )?;
            self.remove_channel(channel);
        }
        Ok(())
    }

    fn remove_channel(&mut self, channel: ChannelId) {
        if let Some(mut state) = self.channels.remove(&channel) {
            if let Some(task) = state.vmm_task.take() {
                task.abort();
            }
        }
    }
}

impl Drop for GatewayHandler {
    fn drop(&mut self) {
        for (_, mut state) in self.channels.drain() {
            if let Some(task) = state.vmm_task.take() {
                task.abort();
            }
        }
    }
}

impl server::Handler for GatewayHandler {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        let vm_id = match parse_target_vm_id(user) {
            Ok(vm_id) => vm_id,
            Err(msg) => {
                tracing::info!(user, peer = ?self.peer_addr, "SSH gateway auth rejected: {msg}");
                return Ok(server::Auth::reject());
            }
        };
        let fingerprint = match public_key_fingerprint_sha256(key) {
            Ok(fingerprint) => fingerprint,
            Err(e) => {
                tracing::warn!(user, peer = ?self.peer_addr, "SSH gateway key fingerprint failed: {e}");
                return Ok(server::Auth::reject());
            }
        };

        match lookup_active_key(&self.state, fingerprint.clone()).await {
            Ok(Some(record)) => {
                let owner_key = record.owner_key.clone();
                self.auth = Some(Authenticated {
                    owner_key: record.owner_key,
                    vm_id,
                    fingerprint: fingerprint.clone(),
                });
                crate::audit::record_parts(
                    &self.state,
                    "",
                    &owner_key,
                    tarit_types::audit_action::SSH_ATTEMPT,
                    Some(vm_id),
                    tarit_types::audit_outcome::OK,
                    Some(fingerprint),
                );
                Ok(server::Auth::Accept)
            }
            Ok(None) => {
                crate::audit::record_parts(
                    &self.state,
                    "",
                    "",
                    tarit_types::audit_action::SSH_ATTEMPT,
                    Some(vm_id),
                    tarit_types::audit_outcome::DENIED,
                    Some(fingerprint),
                );
                Ok(server::Auth::reject())
            }
            Err(e) => {
                tracing::warn!(user, peer = ?self.peer_addr, "SSH gateway key lookup failed: {e}");
                Ok(server::Auth::reject())
            }
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.channels.insert(channel.id(), ChannelState::default());
        Ok(true)
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        tracing::debug!(?channel, col_width, row_height, "gateway: pty_request");
        // OpenSSH sends 0x0 when stdin is not a real terminal (e.g. batch mode
        // with -tt, or piped stdin). Treat 0 as "no size hint" and fall back to
        // a sane default rather than rejecting, which would derail the session.
        let cols = pty_dim(col_width, DEFAULT_COLS);
        let rows = pty_dim(row_height, DEFAULT_ROWS);
        let entry = self.channels.entry(channel).or_default();
        entry.cols = cols;
        entry.rows = rows;
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        tracing::debug!(?channel, "gateway: shell_request");
        self.start_pty(channel, None, session).await
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).to_string();
        self.start_pty(channel, Some(command), session).await
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        tracing::debug!(?channel, n = data.len(), "gateway: ssh->vmm data");
        self.write_to_vmm(channel, data_frame(data.to_vec()), session)
            .await
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let cols = pty_dim(col_width, DEFAULT_COLS);
        let rows = pty_dim(row_height, DEFAULT_ROWS);
        let entry = self.channels.entry(channel).or_default();
        entry.cols = cols;
        entry.rows = rows;
        self.write_to_vmm(channel, resize_frame(cols, rows), session)
            .await?;
        let _ = session.channel_success(channel);
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(state) = self.channels.get_mut(&channel) {
            state.writer = None;
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.remove_channel(channel);
        Ok(())
    }
}

#[derive(Clone)]
struct Authenticated {
    owner_key: String,
    vm_id: Uuid,
    fingerprint: String,
}

struct ChannelState {
    cols: u16,
    rows: u16,
    started: bool,
    writer: Option<Arc<AsyncMutex<OwnedWriteHalf>>>,
    vmm_task: Option<tokio::task::JoinHandle<()>>,
}

impl Default for ChannelState {
    fn default() -> Self {
        Self {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            started: false,
            writer: None,
            vmm_task: None,
        }
    }
}

fn parse_target_vm_id(user: &str) -> Result<Uuid, String> {
    Uuid::parse_str(user).map_err(|_| "SSH username must be the target VM UUID".to_string())
}

fn pty_dim(value: u32, default: u16) -> u16 {
    if value == 0 {
        default
    } else {
        value.min(u16::MAX as u32) as u16
    }
}

fn public_key_fingerprint_sha256(key: &PublicKey) -> Result<String, russh::keys::ssh_key::Error> {
    Ok(openssh_sha256_fingerprint(&key.to_bytes()?))
}

async fn lookup_active_key(
    state: &AppState,
    fingerprint: String,
) -> Result<Option<SshKeyRecord>, OrchError> {
    let store = Arc::clone(&state.store);
    tokio::task::spawn_blocking(move || {
        let store = store
            .lock()
            .map_err(|_| OrchError::Internal("store lock".into()))?;
        store
            .get_active_ssh_key_by_fingerprint(&fingerprint)
            .map_err(store_err)
    })
    .await
    .map_err(|e| OrchError::Internal(format!("join: {e}")))?
}

async fn attach_authorized_pty(
    state: &AppState,
    auth: &Authenticated,
    cols: u16,
    rows: u16,
    shell: Option<String>,
) -> Result<std::os::unix::net::UnixStream, String> {
    match cluster::resolve_owner(state, auth.vm_id).await {
        Ok(Owner::Local) => {}
        Ok(Owner::Remote(rpc)) => {
            return Err(format!(
                "VM {} is owned by remote peer {rpc}; SSH peer forwarding is not implemented yet, connect to the owning node",
                auth.vm_id
            ));
        }
        Err(e) => return Err(e.to_string()),
    }

    let record = local_vm_record(state, auth.vm_id).map_err(|e| e.to_string())?;
    if record.owner_key.as_deref() != Some(auth.owner_key.as_str()) {
        tracing::warn!(
            vm_id = %auth.vm_id,
            fingerprint = %auth.fingerprint,
            "SSH gateway ownership check failed"
        );
        return Err("authenticated SSH key does not own the requested VM".into());
    }

    let supervisor = Arc::clone(&state.supervisor);
    let vm_id = auth.vm_id;
    tokio::task::spawn_blocking(move || supervisor.attach_pty(vm_id, cols, rows, shell))
        .await
        .map_err(|e| format!("attach PTY join: {e}"))?
        .map_err(|e| e.to_string())
}

fn local_vm_record(state: &AppState, vm_id: Uuid) -> Result<VmRecord, OrchError> {
    if let Some(record) = state
        .vm_cache
        .read()
        .ok()
        .and_then(|cache| cache.get(&vm_id).cloned())
    {
        return Ok(record);
    }

    let store = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock".into()))?;
    store.get_vm(vm_id).map_err(store_err)
}

async fn vmm_to_ssh(
    mut reader: OwnedReadHalf,
    handle: server::Handle,
    channel: ChannelId,
) -> Result<(), BridgeError> {
    loop {
        let frame = read_vmm_stream_frame(&mut reader).await?;
        match frame.frame_type {
            TYPE_DATA => {
                tracing::debug!(?channel, n = frame.payload.len(), "gateway: vmm->ssh data");
                handle
                    .data(channel, frame.payload)
                    .await
                    .map_err(|_| BridgeError::SshClosed)?;
            }
            TYPE_EXIT => {
                let exit_code = decode_exit_code(&frame.payload)?;
                let status = if exit_code < 0 { 255 } else { exit_code as u32 };
                let _ = handle.exit_status_request(channel, status).await;
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
                break;
            }
            TYPE_ERROR => {
                let msg = decode_error_message(frame.payload)?;
                let _ = handle
                    .data(channel, format!("taritd ssh gateway: {msg}\r\n"))
                    .await;
                let _ = handle.exit_status_request(channel, 1).await;
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
                break;
            }
            TYPE_RESIZE => {}
            _ => {}
        }
    }
    Ok(())
}

async fn write_vmm_stream_frame_locked(
    writer: &Arc<AsyncMutex<OwnedWriteHalf>>,
    frame: &PtyStreamFrame,
) -> Result<(), BridgeError> {
    let mut writer = writer.lock().await;
    write_vmm_stream_frame(&mut *writer, frame).await
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

fn fail_channel_request(
    session: &mut Session,
    channel: ChannelId,
    message: &str,
) -> Result<(), russh::Error> {
    let _ = session.channel_failure(channel);
    send_gateway_error(session, channel, message, 1)
}

fn send_gateway_error(
    session: &mut Session,
    channel: ChannelId,
    message: &str,
    exit_status: u32,
) -> Result<(), russh::Error> {
    session.data(channel, format!("taritd ssh gateway: {message}\r\n"))?;
    session.exit_status_request(channel, exit_status)?;
    session.eof(channel)?;
    session.close(channel)?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum BridgeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol: {0}")]
    Protocol(String),

    #[error("SSH channel closed")]
    SshClosed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiKeyRegistry, ApiRole, AutoscaleConfig, Config, WarmPoolConfig};
    use crate::metrics::Metrics;
    use crate::peer::PeerClient;
    use crate::pty::PtyRegistry;
    use crate::scheduler::Scheduler;
    use crate::supervisor::VmmSupervisor;
    use chrono::Utc;
    use russh::keys::HashAlg;
    use russh::server::Handler as _;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Mutex, RwLock};
    use tarit_store::Store;

    const ED25519_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g test@example";
    const ED25519_FINGERPRINT: &str = "SHA256:mKqU+0K8OhKmA8bBQi9Rz0Q5l7/g160hIP+rJYSTNj4";
    const RSA_KEY: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQCm9bEVScNevvQHZGzV9bBzwzbEaFSmK2QwY/t/FRS1MJMbaCrdfrY0aLpoN9f744JI5lvzilCHlnbCqcrKBmUeGtrVEDpyY6gAF8YfN8C6os/NOxTQHukjwlgHi01FjuiZyAnhxXnBrWE+ZXxIX1up13YC+DQhJBSPHaFuD3pdGUN/MCESh+enLcge0qSnBpAjdd2yxGn9sRs+6u1i7TOHvBBGmtoHFZIChbxJJyyFBquUHJOa5K+njQA5CLP1VX02qm/Efoy7WWuygKF2rHsbnkIzeWGvbv4tpW36412uRvj4/JAh2rSk1a1Dp57VCV4RGZCyB1jEhyB2qD4Q3YX5 tarit-test";

    #[test]
    fn parses_vm_id_username() {
        let vm_id = Uuid::new_v4();

        assert_eq!(parse_target_vm_id(&vm_id.to_string()).unwrap(), vm_id);
        assert!(parse_target_vm_id("root").is_err());
        assert!(parse_target_vm_id("").is_err());
    }

    #[test]
    fn russh_public_key_fingerprint_matches_shared_helper() {
        let key = PublicKey::from_openssh(ED25519_KEY).unwrap();
        let blob = key.to_bytes().unwrap();

        assert_eq!(openssh_sha256_fingerprint(&blob), ED25519_FINGERPRINT);
        assert_eq!(
            public_key_fingerprint_sha256(&key).unwrap(),
            key.fingerprint(HashAlg::Sha256).to_string()
        );
    }

    #[test]
    fn generates_ed25519_host_key() {
        let dir = std::path::PathBuf::from("target")
            .join(format!("taritd-gateway-test-{}", Uuid::new_v4()));
        let path = dir.join("ssh_host");

        let key = load_or_generate_host_key(&path).unwrap();

        let _ = std::fs::remove_dir_all(dir);
        assert_eq!(key.algorithm(), Algorithm::Ed25519);
    }

    #[test]
    fn accepts_registered_ed25519_public_key() {
        let state = test_state();
        let key = PublicKey::from_openssh(ED25519_KEY).unwrap();
        let fingerprint = public_key_fingerprint_sha256(&key).unwrap();
        state
            .store
            .lock()
            .unwrap()
            .insert_ssh_key(&SshKeyRecord {
                id: Uuid::new_v4(),
                owner_key: "test-owner".into(),
                fingerprint,
                public_key: ED25519_KEY.into(),
                key_type: "ssh-ed25519".into(),
                created_at: Utc::now(),
                is_active: true,
            })
            .unwrap();
        let mut handler = GatewayHandler::new(state, None);
        let vm_id = Uuid::new_v4();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let auth = runtime
            .block_on(handler.auth_publickey(&vm_id.to_string(), &key))
            .unwrap();

        assert!(matches!(auth, server::Auth::Accept));
        assert_eq!(handler.auth.as_ref().unwrap().owner_key, "test-owner");
    }

    #[test]
    fn rejects_rsa_host_key_file_with_actionable_error() {
        let dir = std::path::PathBuf::from("target")
            .join(format!("taritd-gateway-test-{}", Uuid::new_v4()));
        let path = dir.join("ssh_host");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, RSA_KEY).unwrap();

        let err = load_or_generate_host_key(&path).unwrap_err();

        let _ = std::fs::remove_dir_all(dir);
        assert!(err.to_string().contains("Ed25519"), "{err:#}");
        assert!(err.to_string().contains("RSA"), "{err:#}");
    }

    fn test_state() -> AppState {
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            api_keys: ApiKeyRegistry::from_plaintext_entries(vec![
                ("tenant-a-key".into(), "tenant-a".into(), ApiRole::User, 1),
                ("admin-key".into(), "admin".into(), ApiRole::Admin, 0),
            ])
            .unwrap(),
            host_id: "test-host".into(),
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
        };
        let store = Store::open(":memory:").unwrap();
        let (store_tx, _store_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState {
            config: config.clone(),
            store: Arc::new(Mutex::new(store)),
            exec_cache: Arc::new(RwLock::new(HashMap::new())),
            vm_cache: Arc::new(RwLock::new(HashMap::new())),
            store_tx,
            pty_registry: Arc::new(PtyRegistry::default()),
            supervisor: Arc::new(VmmSupervisor::new(config.clone())),
            scheduler: Arc::new(Scheduler::new(config)),
            peer: Arc::new(PeerClient::new("peer-secret".into())),
            fleet: None,
            metrics: Arc::new(Metrics::default()),
        }
    }
}
