//! RPC dispatch over a Unix Domain Socket (1:1 model — no VM ids).
//!
//! The control socket is hardened before the accept loop starts: a socket
//! parent directory created by this process is mode `0700`, the socket node is
//! mode `0600`, and Linux clients must authenticate as root or the server's
//! own effective UID via peer credentials.

use crate::types::{ApiRequest, ApiResponse};
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};
use vmm_core::controller::VmmController;

/// Maximum accepted length-prefixed JSON frame size.
///
/// This is the single control-plane cap defined once in `tarit-proto`
/// (`MAX_API_FRAME_LEN`, 16 MiB). The VMM server, the orchestrator client, and
/// the integration docs all use it so the wire contract cannot drift.
pub const MAX_FRAME_BYTES: usize = tarit_proto::MAX_API_FRAME_LEN;
const SOCKET_DIR_MODE: u32 = 0o700;
const SOCKET_FILE_MODE: u32 = 0o600;
const CONTROL_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// A framed request: 4-byte big-endian length + JSON body.
pub fn read_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    read_frame_with_timeout(stream, CONTROL_IO_TIMEOUT)
}

fn read_frame_with_timeout(stream: &mut UnixStream, timeout: Duration) -> std::io::Result<Vec<u8>> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "timeout overflow"))?;
    let mut len_buf = [0u8; 4];
    read_exact_before(stream, &mut len_buf, deadline)?;
    let declared_len = u32::from_be_bytes(len_buf);
    let len = usize::try_from(declared_len).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame length does not fit usize",
        )
    })?;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    read_exact_before(stream, &mut body, deadline)?;
    Ok(body)
}

/// Write a framed JSON body.
pub fn write_frame(stream: &mut UnixStream, body: &[u8]) -> std::io::Result<()> {
    write_frame_with_timeout(stream, body, CONTROL_IO_TIMEOUT)
}

fn write_frame_with_timeout(
    stream: &mut UnixStream,
    body: &[u8],
    timeout: Duration,
) -> std::io::Result<()> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let len = u32::try_from(body.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "frame length does not fit u32",
        )
    })?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "timeout overflow"))?;
    write_all_before(stream, &len.to_be_bytes(), deadline)?;
    write_all_before(stream, body, deadline)?;
    Ok(())
}

fn read_exact_before(
    stream: &mut UnixStream,
    mut buf: &mut [u8],
    deadline: Instant,
) -> std::io::Result<()> {
    while !buf.is_empty() {
        let remaining = remaining_before(deadline, "read frame deadline exceeded")?;
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(buf) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "peer closed during frame",
                ))
            }
            Ok(read) => buf = &mut buf[read..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_all_before(
    stream: &mut UnixStream,
    mut buf: &[u8],
    deadline: Instant,
) -> std::io::Result<()> {
    while !buf.is_empty() {
        let remaining = remaining_before(deadline, "write frame deadline exceeded")?;
        stream.set_write_timeout(Some(remaining))?;
        match stream.write(buf) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "peer stopped accepting frame",
                ))
            }
            Ok(written) => buf = &buf[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn remaining_before(deadline: Instant, message: &'static str) -> std::io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, message))
    } else {
        Ok(remaining)
    }
}

fn clear_control_timeouts(stream: &UnixStream) -> std::io::Result<()> {
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)
}

/// Dispatch a single `ApiRequest` using the VMM controller.
pub fn dispatch(req: ApiRequest, controller: &VmmController) -> ApiResponse {
    match req {
        ApiRequest::Create(spec) => match controller.create_live(spec.config) {
            Ok(()) => ApiResponse::Ok,
            Err(e) => ApiResponse::Err {
                msg: format!("{e}"),
            },
        },
        ApiRequest::Pause => match controller.pause() {
            Ok(()) => ApiResponse::Ok,
            Err(e) => ApiResponse::Err {
                msg: format!("{e}"),
            },
        },
        ApiRequest::Suspend => match controller.suspend() {
            Ok(()) => ApiResponse::Ok,
            Err(e) => ApiResponse::Err {
                msg: format!("{e}"),
            },
        },
        ApiRequest::Resume => match controller.resume() {
            Ok(()) => ApiResponse::Ok,
            Err(e) => ApiResponse::Err {
                msg: format!("{e}"),
            },
        },
        ApiRequest::Snapshot { diff } => match controller.snapshot(diff) {
            Ok(path) => ApiResponse::Snapshot { path },
            Err(e) => ApiResponse::Err {
                msg: format!("{e}"),
            },
        },
        ApiRequest::ReleaseScratch { path, identity } => {
            match controller.release_scratch(&path, identity) {
                Ok(()) => ApiResponse::Ok,
                Err(e) => ApiResponse::Err {
                    msg: format!("{e}"),
                },
            }
        }
        ApiRequest::Restore {
            snapshot_path,
            overlay,
            net,
        } => match controller.restore_with_overrides(&snapshot_path, overlay, net) {
            Ok(()) => ApiResponse::Restored,
            Err(e) => ApiResponse::Err {
                msg: format!("{e}"),
            },
        },
        ApiRequest::Stop => match controller.stop() {
            Ok(()) => ApiResponse::Ok,
            Err(e) => ApiResponse::Err {
                msg: format!("{e}"),
            },
        },
        ApiRequest::Exec {
            command,
            timeout_ms,
        } => match controller.exec(&command, timeout_ms) {
            Ok((exit_code, stdout, duration_ms)) => ApiResponse::Exec {
                exit_code,
                stdout,
                stderr: String::new(),
                duration_ms,
            },
            Err(e) => ApiResponse::Err {
                msg: format!("{e}"),
            },
        },
        ApiRequest::AttachPty { .. } => ApiResponse::Err {
            msg: "AttachPty is a streaming request".into(),
        },
        ApiRequest::UpdateEgress {
            allowlist,
            allow_existing,
        } => {
            use vmm_net::egress::EgressPolicy;
            use vmm_net::live_egress::{compile_egress_update, EgressUpdate};

            let rules = match parse_egress_allowlist(&allowlist) {
                Ok(rules) => rules,
                Err(msg) => return ApiResponse::Err { msg },
            };

            let update = EgressUpdate {
                vm_id: String::new(),
                policy: EgressPolicy { rules },
                allow_existing,
            };
            // Only program real netfilter rules when we're isolated in a per-VM
            // network namespace (set by `serve --netns`); otherwise the output
            // hook's default-drop would take out the host's own egress. Without
            // enforcement we stay compile-only (validate + report) so the
            // orchestrator still gets a rule count and the call is a safe no-op.
            let enforce = std::env::var("VMM_EGRESS_ENFORCE").as_deref() == Ok("1");
            if enforce {
                match vmm_net::live_egress::apply_egress_update(&update) {
                    Ok(result) => {
                        log::info!("egress: applied {} rules via nft", result.rules_applied);
                        ApiResponse::EgressUpdated {
                            rules_applied: result.rules_applied,
                        }
                    }
                    Err(e) => {
                        log::error!("egress apply failed: {e}");
                        ApiResponse::Err {
                            msg: format!("egress apply failed: {e}"),
                        }
                    }
                }
            } else {
                let result = compile_egress_update(&update);
                log::info!(
                    "egress: compiled {} rules → {} nft commands (enforcement off: no netns, not applied)",
                    result.rules_applied,
                    result.nft_commands.len()
                );
                ApiResponse::EgressUpdated {
                    rules_applied: result.rules_applied,
                }
            }
        }
        ApiRequest::Status => match controller.status() {
            Ok(status) => ApiResponse::Status(status),
            Err(e) => ApiResponse::Err {
                msg: format!("{e}"),
            },
        },
    }
}

/// Run the API server on `socket_path`. Blocks the calling thread.
pub fn serve(socket_path: &str) -> std::io::Result<()> {
    let controller = VmmController::new();
    serve_with_controller(socket_path, controller)
}

/// Run the API server with a pre-existing controller.
pub fn serve_with_controller(socket_path: &str, controller: VmmController) -> std::io::Result<()> {
    let controller = std::sync::Arc::new(controller);
    remove_stale_socket(socket_path)?;
    ensure_socket_parent(socket_path)?;
    let listener = UnixListener::bind(socket_path)?;
    harden_socket_permissions(socket_path)?;
    // Handle SIGTERM/SIGINT: on shutdown, stop the VM cleanly (vCPU threads,
    // net loops, fds) and unlink the socket, so the orchestrator never leaves
    // an orphaned guest or a stale socket behind.
    install_shutdown_handler(controller.clone(), socket_path.to_string());
    log::info!("api listening on {socket_path}");
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                log::warn!("accept: {e}");
                continue;
            }
        };
        if let Err(e) = authorize_peer(&stream) {
            log::warn!("rejecting unauthorized api peer: {e}");
            continue;
        }
        let req_body = match read_frame(&mut stream) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("read_frame: {e}");
                continue;
            }
        };
        let req: ApiRequest = match serde_json::from_slice(&req_body) {
            Ok(r) => r,
            Err(e) => {
                let resp = ApiResponse::Err {
                    msg: format!("bad request: {e}"),
                };
                let _ = write_frame(&mut stream, &encode_response(&resp));
                continue;
            }
        };
        if let ApiRequest::AttachPty { cols, rows, shell } = req {
            // PTY sessions are intentionally long-lived. The absolute framing
            // deadline protects only their initial authenticated request.
            if let Err(e) = clear_control_timeouts(&stream) {
                log::warn!("clear AttachPty control timeout: {e}");
                continue;
            }
            let controller = controller.clone();
            if let Err(e) = std::thread::Builder::new()
                .name("api-attach-pty".into())
                .spawn(move || serve_attach_pty(stream, controller, cols, rows, shell))
            {
                log::warn!("attach_pty thread spawn: {e}");
            }
            continue;
        }
        // Isolate handler panics: a panic in dispatch must not crash the whole
        // `vmm serve` process (which would orphan the VM before the orchestrator
        // can stop it cleanly). Catch it and return a framed error instead.
        let resp =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(req, &controller)))
                .unwrap_or_else(|payload| {
                    let msg = payload
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    log::error!("request handler panicked: {msg}");
                    ApiResponse::Err {
                        msg: format!("internal error: {msg}"),
                    }
                });
        if let Err(e) = write_frame(&mut stream, &encode_response(&resp)) {
            log::warn!("write_frame: {e}");
        }
    }
    Ok(())
}

fn serve_attach_pty(
    stream: UnixStream,
    controller: std::sync::Arc<VmmController>,
    cols: u16,
    rows: u16,
    shell: Option<String>,
) {
    let mut error_stream = stream.try_clone().ok();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        controller.attach_pty(stream, cols, rows, shell)
    }))
    .unwrap_or_else(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        log::error!("attach_pty handler panicked: {msg}");
        Err(vmm_core::error::VmmError::Device(format!(
            "internal error: {msg}"
        )))
    });

    if let Err(e) = result {
        if let Some(stream) = error_stream.as_mut() {
            let _ = vmm_core::pty_stream::write_error_frame(stream, &format!("{e}"));
            let _ = stream.flush();
        }
    }
}

fn parse_egress_allowlist(
    allowlist: &[String],
) -> Result<Vec<vmm_net::egress::EgressRule>, String> {
    allowlist
        .iter()
        .map(|rule| parse_egress_rule(rule))
        .collect()
}

fn parse_egress_rule(rule: &str) -> Result<vmm_net::egress::EgressRule, String> {
    use vmm_net::egress::{EgressRule, Proto};

    if rule.is_empty() {
        return Err("bad egress allowlist rule \"\": empty rule".into());
    }

    let (cidr, port_proto) = match rule.split_once(':') {
        Some((cidr, rest)) => {
            if cidr.is_empty() {
                return Err(format!("bad egress allowlist rule {rule:?}: missing CIDR"));
            }
            (cidr, Some(rest))
        }
        None => (rule, None),
    };

    if cidr.is_empty() {
        return Err(format!("bad egress allowlist rule {rule:?}: missing CIDR"));
    }

    let Some(port_proto) = port_proto else {
        return Ok(EgressRule {
            cidr: cidr.into(),
            port: 0,
            proto: Proto::Any,
        });
    };

    let (port, proto) = if let Some((port, proto)) = port_proto.split_once('/') {
        let proto = match proto {
            "tcp" => Proto::Tcp,
            "udp" => Proto::Udp,
            _ => {
                return Err(format!(
                    "bad egress allowlist rule {rule:?}: unknown proto {proto:?}"
                ))
            }
        };
        (parse_egress_port(rule, port)?, proto)
    } else {
        (parse_egress_port(rule, port_proto)?, Proto::Tcp)
    };

    Ok(EgressRule {
        cidr: cidr.into(),
        port,
        proto,
    })
}

fn parse_egress_port(rule: &str, port: &str) -> Result<u16, String> {
    if port.is_empty() {
        return Err(format!("bad egress allowlist rule {rule:?}: missing port"));
    }
    port.parse::<u16>()
        .map_err(|e| format!("bad egress allowlist rule {rule:?}: invalid port {port:?}: {e}"))
}

fn remove_stale_socket(socket_path: &str) -> std::io::Result<()> {
    match std::fs::symlink_metadata(socket_path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(socket_path),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("refusing to remove non-socket path {socket_path}"),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn ensure_socket_parent(socket_path: &str) -> std::io::Result<()> {
    let Some(parent) = Path::new(socket_path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };

    match std::fs::metadata(parent) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("socket parent is not a directory: {}", parent.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(SOCKET_DIR_MODE))
        }
        Err(e) => Err(e),
    }
}

fn harden_socket_permissions(socket_path: &str) -> std::io::Result<()> {
    std::fs::set_permissions(
        socket_path,
        std::fs::Permissions::from_mode(SOCKET_FILE_MODE),
    )
}

#[cfg(target_os = "linux")]
fn authorize_peer(stream: &UnixStream) -> std::io::Result<()> {
    let peer = peer_credentials(stream)?;
    // SAFETY: `geteuid` has no preconditions and only returns the effective UID
    // of the current process.
    let server_uid = unsafe { libc::geteuid() };
    if peer_uid_is_authorized(peer.uid, server_uid) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "peer uid {} is not authorized for server euid {}",
                peer.uid, server_uid
            ),
        ))
    }
}

#[cfg(not(target_os = "linux"))]
fn authorize_peer(_stream: &UnixStream) -> std::io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn peer_uid_is_authorized(peer_uid: libc::uid_t, server_uid: libc::uid_t) -> bool {
    peer_uid == server_uid || peer_uid == 0
}

#[cfg(target_os = "linux")]
fn peer_credentials(stream: &UnixStream) -> std::io::Result<libc::ucred> {
    // SAFETY: `ucred` is plain old data and is immediately initialized by
    // `getsockopt` before any fields are read.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `stream.as_raw_fd()` is a valid connected Unix socket fd while
    // `stream` is borrowed, `credentials` and `len` are valid writable output
    // pointers, and their sizes match the `SO_PEERCRED` contract.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if len != std::mem::size_of::<libc::ucred>() as libc::socklen_t {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "short SO_PEERCRED response",
        ));
    }
    Ok(credentials)
}

/// Install a SIGTERM/SIGINT handler that tears the VM down cleanly.
///
/// The signals are blocked process-wide (so they never interrupt the accept
/// loop or a vCPU thread's KVM_RUN) and handled on a dedicated thread that
/// `sigwait`s for them. Threads spawned later (the vCPU + net-io threads)
/// inherit the block mask, so only this thread reacts to the signal.
fn install_shutdown_handler(controller: std::sync::Arc<VmmController>, socket_path: String) {
    let set = match shutdown_signal_set() {
        Ok(set) => set,
        Err(e) => {
            log::warn!("shutdown signal set: {e}");
            return;
        }
    };
    // SAFETY: `set` was initialized by `sigemptyset`/`sigaddset`, and the
    // third argument is null because the previous mask is intentionally ignored.
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    if rc != 0 {
        log::warn!("pthread_sigmask: {}", std::io::Error::from_raw_os_error(rc));
        return;
    }

    std::thread::Builder::new()
        .name("vmm-shutdown".into())
        .spawn(move || {
            let wait_set = match shutdown_signal_set() {
                Ok(set) => set,
                Err(e) => {
                    log::warn!("shutdown signal set: {e}");
                    return;
                }
            };
            let mut sig: libc::c_int = 0;
            // SAFETY: `wait_set` is a valid initialized signal set, and `sig`
            // is a valid out-pointer for the signal number returned by sigwait.
            let rc = unsafe { libc::sigwait(&wait_set, &mut sig) };
            if rc == 0 {
                log::info!("received signal {sig}; shutting down VM cleanly");
            } else {
                log::warn!("sigwait: {}", std::io::Error::from_raw_os_error(rc));
            }
            graceful_teardown(&controller, &socket_path);
            std::process::exit(0);
        })
        .ok();
}

fn shutdown_signal_set() -> std::io::Result<libc::sigset_t> {
    // SAFETY: `sigset_t` is immediately initialized with `sigemptyset` before
    // it is used by any other libc call.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is a valid mutable pointer to sigset_t storage.
    if unsafe { libc::sigemptyset(&mut set) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `set` is initialized, and SIGTERM is a valid signal constant.
    if unsafe { libc::sigaddset(&mut set, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `set` is initialized, and SIGINT is a valid signal constant.
    if unsafe { libc::sigaddset(&mut set, libc::SIGINT) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(set)
}

/// Stop the VM cleanly and remove the socket file. Split out from the signal
/// thread so it is unit-testable without raising a real signal.
fn graceful_teardown(controller: &VmmController, socket_path: &str) {
    if let Err(e) = controller.stop() {
        log::warn!("shutdown: stop returned: {e}");
    }
    if let Err(e) = remove_stale_socket(socket_path) {
        log::warn!("shutdown: socket cleanup returned: {e}");
    }
}

/// Serialize a response, falling back to a minimal error frame if (impossibly)
/// serialization fails — the serve loop must never panic on the response path.
fn encode_response(resp: &ApiResponse) -> Vec<u8> {
    serde_json::to_vec(resp).unwrap_or_else(|_| b"{\"status\":\"err\",\"msg\":\"encode\"}".to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_core::config::{KernelConfig, MemoryConfig, VcpuConfig, VmConfig};

    fn cfg() -> VmConfig {
        VmConfig {
            kernel: KernelConfig {
                path: "/k".into(),
                cmdline: "console=ttyS0".into(),
                initramfs: None,
            },
            memory: MemoryConfig { size_mib: 64 },
            vcpus: VcpuConfig { count: 1 },
            volumes: vec![],
            net: vec![],
        }
    }

    #[test]
    fn dispatch_create_returns_ok_or_err() {
        let controller = VmmController::new();
        let req = ApiRequest::Create(crate::types::VmSpec { config: cfg() });
        let resp = dispatch(req, &controller);
        assert!(matches!(resp, ApiResponse::Ok | ApiResponse::Err { .. }));
    }

    #[test]
    fn dispatch_pause_without_vm_returns_err() {
        let controller = VmmController::new();
        let resp = dispatch(ApiRequest::Pause, &controller);
        assert!(matches!(resp, ApiResponse::Err { .. }));
    }

    #[test]
    fn dispatch_suspend_without_vm_returns_err() {
        let controller = VmmController::new();
        let resp = dispatch(ApiRequest::Suspend, &controller);
        assert!(matches!(resp, ApiResponse::Err { .. }));
    }

    #[test]
    fn dispatch_stop_without_vm_returns_ok() {
        let controller = VmmController::new();
        let resp = dispatch(ApiRequest::Stop, &controller);
        assert!(matches!(resp, ApiResponse::Ok));
    }

    #[test]
    fn dispatch_status_without_vm_returns_err() {
        let controller = VmmController::new();
        let resp = dispatch(ApiRequest::Status, &controller);
        assert!(matches!(resp, ApiResponse::Err { .. }));
    }

    #[test]
    fn dispatch_status_after_create_reports_config() {
        let controller = VmmController::new();
        // Non-boot builds set state=Created without loading a kernel. Under the
        // `boot` feature create() actually boots and needs a real bootable
        // kernel, which this unit test does not provide, so skip it there.
        if controller.create(cfg()).is_err() {
            return;
        }
        let resp = dispatch(ApiRequest::Status, &controller);
        match resp {
            ApiResponse::Status(s) => {
                assert_eq!(s.mem_mib, 64);
                assert_eq!(s.vcpus, 1);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn release_scratch_requires_the_vmm_owned_identity() {
        let controller = VmmController::new();
        if controller.create(cfg()).is_err() {
            return;
        }
        let path = controller.snapshot(false).expect("create snapshot");
        let identity = vmm_core::gc::OwnedScratchFile::identity_for(Path::new(&path))
            .expect("snapshot identity");
        let mut wrong_identity = identity.clone();
        wrong_identity.inode = wrong_identity.inode.saturating_add(1);

        assert!(matches!(
            dispatch(
                ApiRequest::ReleaseScratch {
                    path: path.clone(),
                    identity: wrong_identity,
                },
                &controller
            ),
            ApiResponse::Err { .. }
        ));
        assert!(matches!(
            dispatch(
                ApiRequest::ReleaseScratch {
                    path: path.clone(),
                    identity
                },
                &controller
            ),
            ApiResponse::Ok
        ));

        controller.stop().expect("stop VM");
        assert!(
            Path::new(&path).exists(),
            "released snapshot survives VM stop"
        );
        std::fs::remove_file(path).expect("clean up released snapshot");
    }

    #[test]
    fn graceful_teardown_stops_vm_and_removes_socket() {
        let controller = VmmController::new();
        // See dispatch_status_after_create_reports_config: create() only
        // completes without a real kernel on the non-boot path.
        if controller.create(cfg()).is_err() {
            return;
        }
        assert!(controller.status().is_ok());

        let sock = std::env::current_dir()
            .unwrap()
            .join(format!(".vmm-teardown-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        assert!(sock.exists());

        graceful_teardown(&controller, sock.to_str().unwrap());

        // VM is stopped (status now errors) and the socket file is gone.
        assert!(controller.status().is_err());
        assert!(!sock.exists());
        drop(listener);
    }

    #[test]
    fn remove_stale_socket_refuses_regular_file() {
        let path = std::env::current_dir()
            .unwrap()
            .join(format!(".vmm-not-socket-{}", std::process::id()));
        std::fs::write(&path, b"x").unwrap();

        let err = remove_stale_socket(path.to_str().unwrap()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(path.exists());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn ensure_socket_parent_creates_missing_parent_with_private_mode() {
        let dir = std::env::current_dir()
            .unwrap()
            .join(format!(".vmm-api-sockdir-{}", std::process::id()));
        let sock = dir.join("api.sock");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir(&dir);

        ensure_socket_parent(sock.to_str().unwrap()).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_DIR_MODE);

        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn harden_socket_permissions_sets_socket_mode_0600() {
        let sock = std::env::current_dir()
            .unwrap()
            .join(format!(".vmm-api-perms-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        harden_socket_permissions(sock.to_str().unwrap()).unwrap();

        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_FILE_MODE);

        drop(listener);
        std::fs::remove_file(sock).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn peer_uid_authorization_allows_same_user_and_root_only() {
        assert!(peer_uid_is_authorized(1000, 1000));
        assert!(peer_uid_is_authorized(0, 1000));
        assert!(!peer_uid_is_authorized(1001, 1000));
    }

    #[test]
    fn authorize_peer_allows_current_process_peer() {
        let (stream, _peer) = UnixStream::pair().unwrap();

        authorize_peer(&stream).unwrap();
    }

    #[test]
    fn read_frame_rejects_declared_size_over_cap() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        let too_large = u32::try_from(MAX_FRAME_BYTES + 1).unwrap();
        writer.write_all(&too_large.to_be_bytes()).unwrap();

        let err = read_frame(&mut reader).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_frame_has_an_absolute_deadline_for_partial_bodies() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(&8u32.to_be_bytes()).unwrap();
        writer.write_all(b"x").unwrap();

        let started = Instant::now();
        let error = read_frame_with_timeout(&mut reader, Duration::from_millis(25)).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn write_frame_has_an_absolute_deadline_when_peer_does_not_drain() {
        let (mut writer, _reader) = UnixStream::pair().unwrap();
        let body = vec![0u8; 4 * 1024 * 1024];

        let started = Instant::now();
        let error =
            write_frame_with_timeout(&mut writer, &body, Duration::from_millis(25)).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn parse_egress_allowlist_rejects_bad_port_and_proto() {
        let bad_port = parse_egress_rule("10.0.0.0/8:not-a-port");
        assert!(bad_port.unwrap_err().contains("invalid port"));

        let bad_proto = parse_egress_rule("10.0.0.0/8:443/icmp");
        assert!(bad_proto.unwrap_err().contains("unknown proto"));
    }

    #[test]
    fn parse_egress_allowlist_preserves_valid_rules() {
        use vmm_net::egress::Proto;

        let cidr_only = parse_egress_rule("10.0.0.0/8").unwrap();
        assert_eq!(cidr_only.port, 0);
        assert_eq!(cidr_only.proto, Proto::Any);

        let tcp_default = parse_egress_rule("10.0.0.0/8:443").unwrap();
        assert_eq!(tcp_default.port, 443);
        assert_eq!(tcp_default.proto, Proto::Tcp);

        let udp = parse_egress_rule("10.0.0.0/8:53/udp").unwrap();
        assert_eq!(udp.port, 53);
        assert_eq!(udp.proto, Proto::Udp);
    }
}
