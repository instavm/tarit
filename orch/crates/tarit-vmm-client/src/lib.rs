//! Unix-domain client for the Tarit VMM control API (length-prefixed JSON).

pub use tarit_proto::*;

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VmmError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("vmm: {0}")]
    Api(String),

    /// The VMM refused the request without changing the VM, because the host
    /// was too busy to finish it in time; the same request can be sent again.
    #[error("vmm busy: {0}")]
    Busy(String),
}

#[cfg(test)]
fn read_api_frame(stream: &mut UnixStream) -> Result<Vec<u8>, VmmError> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_API_FRAME_LEN {
        return Err(VmmError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        )));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    Ok(body)
}

fn checked_api_frame_len(len: usize) -> Result<u32, VmmError> {
    if len > MAX_API_FRAME_LEN {
        return Err(VmmError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "request frame too large",
        )));
    }
    u32::try_from(len).map_err(|_| {
        VmmError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "request frame length exceeds wire format",
        ))
    })
}

/// Default total request budget. Explicit request timeouts take precedence;
/// exec requests receive at least their guest execution budget plus five seconds.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn write_api_frame(stream: &mut UnixStream, body: &[u8]) -> Result<(), VmmError> {
    let len = checked_api_frame_len(body.len())?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn request_timeout_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, "VMM request timed out")
}

fn request_phase_timeout(
    deadline: Option<Instant>,
    now: Instant,
    fallback: Duration,
) -> std::io::Result<Duration> {
    match deadline {
        Some(deadline) => {
            let remaining = deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                Err(request_timeout_error())
            } else {
                Ok(remaining.min(fallback))
            }
        }
        None => Ok(fallback),
    }
}

fn map_deadline_io_error(error: std::io::Error, deadline: Instant) -> VmmError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) || Instant::now() >= deadline
    {
        VmmError::Io(request_timeout_error())
    } else {
        VmmError::Io(error)
    }
}

fn phase_poll_expired(error: &std::io::Error, deadline: Instant) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) && Instant::now() < deadline
}

/// Block until `stream` is ready for `events` or the request deadline passes.
/// Deadline-bounded I/O polls readiness instead of re-arming `SO_RCVTIMEO` /
/// `SO_SNDTIMEO` around every syscall: macOS `setsockopt` intermittently
/// fails with EINVAL once the peer has shut the socket down, which turned a
/// completed VMM response into a spurious client error.
fn wait_for_stream_ready(
    stream: &UnixStream,
    events: libc::c_short,
    deadline: Instant,
) -> Result<(), VmmError> {
    loop {
        let remaining = request_phase_timeout(Some(deadline), Instant::now(), Duration::MAX)?;
        let timeout_ms = remaining
            .as_millis()
            .min(i32::MAX as u128)
            .try_into()
            .expect("bounded poll timeout fits i32");
        let mut poll_fd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if ready == 0 {
            return Err(VmmError::Io(request_timeout_error()));
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(VmmError::Io(error));
        }
        return Ok(());
    }
}

fn write_all_until(
    stream: &mut UnixStream,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), VmmError> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        wait_for_stream_ready(stream, libc::POLLOUT, deadline)?;
        let written = match stream.write(remaining) {
            Ok(written) => written,
            Err(error) if phase_poll_expired(&error, deadline) => continue,
            Err(error) => return Err(map_deadline_io_error(error, deadline)),
        };
        if written == 0 {
            return Err(VmmError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write VMM request",
            )));
        }
        remaining = &remaining[written..];
    }
    Ok(())
}

fn read_exact_until(
    stream: &mut UnixStream,
    bytes: &mut [u8],
    deadline: Instant,
) -> Result<(), VmmError> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        // A VMM op that dumps guest RAM (suspend, snapshot) is silent for far
        // longer than any per-read stream timeout; only the request deadline
        // fails it.
        wait_for_stream_ready(stream, libc::POLLIN, deadline)?;
        let read = match stream.read(remaining) {
            Ok(read) => read,
            Err(error) if phase_poll_expired(&error, deadline) => continue,
            Err(error) => return Err(map_deadline_io_error(error, deadline)),
        };
        if read == 0 {
            return Err(VmmError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "VMM response ended before its frame completed",
            )));
        }
        remaining = &mut remaining[read..];
    }
    Ok(())
}

fn retry_connect_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    ) || error.raw_os_error() == Some(libc::EAGAIN)
}

fn write_api_frame_until(
    stream: &mut UnixStream,
    body: &[u8],
    deadline: Instant,
) -> Result<(), VmmError> {
    let len = checked_api_frame_len(body.len())?;
    write_all_until(stream, &len.to_be_bytes(), deadline)?;
    write_all_until(stream, body, deadline)?;
    stream
        .flush()
        .map_err(|error| map_deadline_io_error(error, deadline))
}

fn read_api_frame_until(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>, VmmError> {
    let mut len_buf = [0u8; 4];
    read_exact_until(stream, &mut len_buf, deadline)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_API_FRAME_LEN {
        return Err(VmmError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        )));
    }
    let mut body = vec![0u8; len];
    read_exact_until(stream, &mut body, deadline)?;
    Ok(body)
}

fn unix_socket_address(path: &Path) -> Result<(libc::sockaddr_un, libc::socklen_t), VmmError> {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(VmmError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid Unix socket path",
        )));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            address.sun_path.as_mut_ptr().cast(),
            bytes.len(),
        );
    }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path)
        .checked_add(bytes.len() + 1)
        .and_then(|length| libc::socklen_t::try_from(length).ok())
        .ok_or_else(|| {
            VmmError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Unix socket path is too long",
            ))
        })?;
    Ok((address, length))
}

fn wait_for_nonblocking_connect(stream: &UnixStream, deadline: Instant) -> Result<(), VmmError> {
    loop {
        let remaining = request_phase_timeout(Some(deadline), Instant::now(), Duration::MAX)?;
        let timeout_ms = remaining
            .as_millis()
            .min(i32::MAX as u128)
            .try_into()
            .expect("bounded poll timeout fits i32");
        let mut poll_fd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if ready == 0 {
            continue;
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(VmmError::Io(error));
        }

        let mut socket_error: libc::c_int = 0;
        let mut length = libc::socklen_t::try_from(std::mem::size_of_val(&socket_error))
            .expect("socket error length fits socklen_t");
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut socket_error as *mut libc::c_int).cast(),
                &mut length,
            )
        };
        if result < 0 {
            return Err(VmmError::Io(std::io::Error::last_os_error()));
        }
        if socket_error == 0 {
            return Ok(());
        }
        return Err(VmmError::Io(std::io::Error::from_raw_os_error(
            socket_error,
        )));
    }
}

fn connect_nonblocking(path: &Path, deadline: Instant) -> Result<UnixStream, VmmError> {
    let (address, length) = unix_socket_address(path)?;
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(VmmError::Io(std::io::Error::last_os_error()));
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let fd_flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFD) };
    if fd_flags < 0 {
        return Err(VmmError::Io(std::io::Error::last_os_error()));
    }
    if unsafe {
        libc::fcntl(
            stream.as_raw_fd(),
            libc::F_SETFD,
            fd_flags | libc::FD_CLOEXEC,
        )
    } < 0
    {
        return Err(VmmError::Io(std::io::Error::last_os_error()));
    }
    stream.set_nonblocking(true)?;
    let result = unsafe { libc::connect(stream.as_raw_fd(), (&raw const address).cast(), length) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        let code = error.raw_os_error();
        if code == Some(libc::EINPROGRESS) || code == Some(libc::EALREADY) {
            wait_for_nonblocking_connect(&stream, deadline)?;
        } else {
            return Err(VmmError::Io(error));
        }
    }
    request_phase_timeout(Some(deadline), Instant::now(), Duration::MAX)?;
    // Readiness only promises that some bytes can be transferred. Keep the
    // socket nonblocking so a write larger than the available send buffer
    // returns control to the deadline-aware polling loop.
    Ok(stream)
}

/// Cancellation of one request's transport, used only when its VM is being
/// deleted. This does not promise to cancel guest side effects by itself.
#[derive(Debug, Default)]
pub struct RequestCancellation {
    state: Mutex<(bool, Option<UnixStream>)>,
}

impl RequestCancellation {
    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.0 = true;
        if let Some(stream) = &state.1 {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn register(&self, stream: &UnixStream) -> Result<(), VmmError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.0 {
            return Err(VmmError::Io(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "VM execution cancelled for deletion",
            )));
        }
        state.1 = Some(stream.try_clone()?);
        Ok(())
    }
}

/// Client for a single VMM instance (one UDS, one VM).
pub struct VmmClient {
    socket_path: String,
    connect_timeout: Duration,
    request_timeout: Option<Duration>,
    cancellation: Option<Arc<RequestCancellation>>,
}

impl VmmClient {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().display().to_string(),
            connect_timeout: Duration::from_secs(5),
            request_timeout: None,
            cancellation: None,
        }
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Override the default total connect, write, and response-read deadline.
    /// A timeout does not establish whether a mutating request was applied;
    /// callers must not blindly replay non-idempotent operations.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    pub fn with_cancellation(mut self, cancellation: Arc<RequestCancellation>) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }

    fn connect(&self) -> Result<UnixStream, VmmError> {
        let path = Path::new(&self.socket_path);
        let started = std::time::Instant::now();
        loop {
            match UnixStream::connect(path) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(self.connect_timeout))?;
                    stream.set_write_timeout(Some(self.connect_timeout))?;
                    return Ok(stream);
                }
                Err(e) if started.elapsed() < self.connect_timeout => {
                    std::thread::sleep(Duration::from_millis(50));
                    if e.kind() == std::io::ErrorKind::NotFound
                        || e.kind() == std::io::ErrorKind::ConnectionRefused
                    {
                        continue;
                    }
                    return Err(VmmError::Io(e));
                }
                Err(e) => return Err(VmmError::Io(e)),
            }
        }
    }

    fn connect_for_request(&self, deadline: Option<Instant>) -> Result<UnixStream, VmmError> {
        let Some(deadline) = deadline else {
            return self.connect();
        };
        let path = Path::new(&self.socket_path);
        let started = Instant::now();
        let connect_deadline = started
            .checked_add(self.connect_timeout)
            .map_or(deadline, |connect| deadline.min(connect));

        loop {
            request_phase_timeout(Some(connect_deadline), Instant::now(), Duration::MAX)?;
            match connect_nonblocking(path, connect_deadline) {
                Ok(stream) => return Ok(stream),
                Err(VmmError::Io(error)) if retry_connect_error(&error) => {
                    let sleep = request_phase_timeout(
                        Some(connect_deadline),
                        Instant::now(),
                        Duration::from_millis(50),
                    )?;
                    std::thread::sleep(sleep);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn request_budget(&self, req: &ApiRequest) -> Duration {
        self.request_timeout.unwrap_or_else(|| match req {
            ApiRequest::Exec { timeout_ms, .. } => Duration::from_millis(*timeout_ms)
                .saturating_add(Duration::from_secs(5))
                .max(DEFAULT_REQUEST_TIMEOUT),
            _ => DEFAULT_REQUEST_TIMEOUT,
        })
    }

    pub fn request(&self, req: &ApiRequest) -> Result<ApiResponse, VmmError> {
        let body = serde_json::to_vec(req)?;
        checked_api_frame_len(body.len())?;
        let deadline = Instant::now()
            .checked_add(self.request_budget(req))
            .ok_or_else(|| {
                VmmError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "request deadline exceeds monotonic clock range",
                ))
            })?;
        let mut stream = self.connect_for_request(Some(deadline))?;
        if let Some(cancellation) = &self.cancellation {
            cancellation.register(&stream)?;
        }
        write_api_frame_until(&mut stream, &body, deadline)?;
        let resp_body = read_api_frame_until(&mut stream, deadline)?;
        let resp: ApiResponse = serde_json::from_slice(&resp_body)?;
        Ok(resp)
    }

    pub fn request_ok(&self, req: &ApiRequest) -> Result<ApiResponse, VmmError> {
        match self.request(req)? {
            ApiResponse::Err { msg } => Err(match msg.strip_prefix(RETRYABLE_ERROR_PREFIX) {
                Some(reason) => VmmError::Busy(reason.to_string()),
                None => VmmError::Api(msg),
            }),
            other => Ok(other),
        }
    }

    pub fn create(&self, config: VmConfig) -> Result<(), VmmError> {
        match self.request_ok(&ApiRequest::Create(VmSpec { config }))? {
            ApiResponse::Ok => Ok(()),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    pub fn stop(&self) -> Result<(), VmmError> {
        match self.request_ok(&ApiRequest::Stop)? {
            ApiResponse::Ok => Ok(()),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    pub fn pause(&self) -> Result<(), VmmError> {
        match self.request_ok(&ApiRequest::Pause)? {
            ApiResponse::Ok => Ok(()),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    /// Pause vCPUs and release resident guest RAM into the VMM's private lazy
    /// suspend image. This is distinct from `pause`, which retains RAM.
    pub fn suspend(&self) -> Result<(), VmmError> {
        match self.request_ok(&ApiRequest::Suspend)? {
            ApiResponse::Ok => Ok(()),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    pub fn resume(&self) -> Result<(), VmmError> {
        match self.request_ok(&ApiRequest::Resume)? {
            ApiResponse::Ok => Ok(()),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    pub fn snapshot(&self, diff: bool) -> Result<String, VmmError> {
        let path = self.snapshot_unreleased(diff)?;
        let identity = Self::scratch_identity(Path::new(&path))?;
        self.release_scratch(&path, identity)?;
        Ok(path)
    }

    /// Take a snapshot while the VMM retains cleanup ownership.
    ///
    /// Callers that need to transfer the artifact must capture its identity and
    /// explicitly call `release_scratch` only after they hold local ownership.
    pub fn snapshot_unreleased(&self, diff: bool) -> Result<String, VmmError> {
        self.snapshot_unreleased_with(diff, None, false)
    }

    /// [`Self::snapshot_unreleased`] with the VMM's checkpoint options:
    /// `base`, a VMM-visible full snapshot this VM last wrote or was restored
    /// from, lets the VMM pause only for pages changed since (the result is a
    /// full snapshot either way); `resume` resumes a VM the caller paused as
    /// soon as RAM is captured.
    pub fn snapshot_unreleased_with(
        &self,
        diff: bool,
        base: Option<&str>,
        resume: bool,
    ) -> Result<String, VmmError> {
        // The orchestrator manages disk uppers itself, so it asks for a
        // memory-only snapshot and rejects any disk artifact.
        match self.request_ok(&ApiRequest::Snapshot {
            diff,
            live: false,
            capture_disks: false,
            base: base.map(str::to_string),
            resume,
        })? {
            ApiResponse::Snapshot {
                path,
                overlay_path: None,
                volume_paths,
                integrity_path: None,
                live_stats: None,
            } if volume_paths.is_empty() => Ok(path),
            ApiResponse::Snapshot {
                integrity_path: Some(_),
                ..
            } => Err(VmmError::Api(
                "non-live snapshot unexpectedly returned a live artifact".into(),
            )),
            ApiResponse::Snapshot {
                integrity_path: None,
                ..
            } => Err(VmmError::Api(
                "non-live snapshot unexpectedly returned a disk artifact".into(),
            )),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    /// Take an atomic live snapshot while the VMM retains cleanup ownership of
    /// both RAM and the optional reflinked disk upper.
    pub fn live_snapshot_unreleased(
        &self,
    ) -> Result<
        (
            String,
            Option<String>,
            String,
            tarit_proto::LiveSnapshotStats,
        ),
        VmmError,
    > {
        match self.request_ok(&ApiRequest::Snapshot {
            diff: false,
            live: true,
            capture_disks: false,
            base: None,
            resume: false,
        })? {
            ApiResponse::Snapshot {
                path,
                overlay_path,
                integrity_path: Some(integrity_path),
                live_stats: Some(live_stats),
                ..
            } => Ok((path, overlay_path, integrity_path, live_stats)),
            ApiResponse::Snapshot {
                integrity_path: None,
                ..
            } => Err(VmmError::Api(
                "live snapshot did not return an integrity sidecar".into(),
            )),
            ApiResponse::Snapshot {
                live_stats: None, ..
            } => Err(VmmError::Api(
                "live snapshot did not return structured pre-copy statistics".into(),
            )),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    /// Disarm VMM cleanup for exactly one scratch artifact it still owns.
    pub fn release_scratch(&self, path: &str, identity: ScratchIdentity) -> Result<(), VmmError> {
        match self.request_ok(&ApiRequest::ReleaseScratch {
            path: path.to_string(),
            identity,
        })? {
            ApiResponse::Ok => Ok(()),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    pub fn exec(
        &self,
        command: &str,
        timeout_ms: u64,
    ) -> Result<(i32, String, String, u64), VmmError> {
        match self.request_ok(&ApiRequest::Exec {
            command: command.to_string(),
            timeout_ms,
        })? {
            ApiResponse::Exec {
                exit_code,
                stdout,
                stderr,
                duration_ms,
            } => Ok((exit_code, stdout, stderr, duration_ms)),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    pub fn update_egress(
        &self,
        allowlist: Vec<String>,
        allow_existing: bool,
    ) -> Result<usize, VmmError> {
        match self.request_ok(&ApiRequest::UpdateEgress {
            allowlist,
            allow_existing,
        })? {
            ApiResponse::EgressUpdated { rules_applied, .. } => Ok(rules_applied),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    pub fn set_balloon(&self, target_mib: u64) -> Result<(u64, u64, u32, u32), VmmError> {
        match self.request_ok(&ApiRequest::SetBalloon { target_mib })? {
            ApiResponse::Balloon {
                target_mib,
                actual_mib,
                target_pages,
                actual_pages,
            } => Ok((target_mib, actual_mib, target_pages, actual_pages)),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    pub fn balloon(&self) -> Result<(u64, u64, u32, u32), VmmError> {
        match self.request_ok(&ApiRequest::Balloon)? {
            ApiResponse::Balloon {
                target_mib,
                actual_mib,
                target_pages,
                actual_pages,
            } => Ok((target_mib, actual_mib, target_pages, actual_pages)),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    fn scratch_identity(path: &Path) -> Result<ScratchIdentity, VmmError> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(VmmError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a regular scratch file", path.display()),
            )));
        }
        let (created_secs, created_nanos) = metadata
            .created()
            .ok()
            .and_then(|created| {
                created
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .ok()
                    .and_then(|duration| {
                        i64::try_from(duration.as_secs())
                            .ok()
                            .map(|seconds| (Some(seconds), Some(duration.subsec_nanos())))
                    })
            })
            .unwrap_or((None, None));
        Ok(ScratchIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            created_secs,
            created_nanos,
        })
    }

    /// Fetch a health/info snapshot of the VM (state, uptime, vCPUs, etc.).
    pub fn status(&self) -> Result<VmStatus, VmmError> {
        match self.request_ok(&ApiRequest::Status)? {
            ApiResponse::Status(s) => Ok(s),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    /// Attach to a PTY. This opens a fresh UDS connection, sends one JSON
    /// `AttachPty` request frame, then returns the stream in raw STREAM mode.
    pub fn attach_pty(
        &self,
        cols: u16,
        rows: u16,
        shell: Option<String>,
    ) -> Result<UnixStream, VmmError> {
        let mut stream = self.connect()?;
        let body = serde_json::to_vec(&ApiRequest::AttachPty { cols, rows, shell })?;
        write_api_frame(&mut stream, &body)?;
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)?;
        Ok(stream)
    }

    /// Restore a VM from a snapshot file into this (freshly spawned) `vmm serve`
    /// process, resuming it to a running state.
    pub fn restore(&self, snapshot_path: &str, overlay: Option<String>) -> Result<(), VmmError> {
        self.restore_with_options(snapshot_path, overlay, None, RestoreMemoryPolicy::Auto)
    }

    /// Restore while replacing every host-network binding saved in the
    /// snapshot. Orchestrators must use this with `Some`, including an empty
    /// vector, so a restore can never silently reuse a stale tap or guest IP.
    pub fn restore_with_network_override(
        &self,
        snapshot_path: &str,
        overlay: Option<String>,
        net: Option<Vec<NetConfig>>,
    ) -> Result<(), VmmError> {
        self.restore_with_options(snapshot_path, overlay, net, RestoreMemoryPolicy::Auto)
    }

    /// Restore with an explicit memory backend policy. `auto` preserves the
    /// historical behavior; `eager` and `lazy` force that exact restore path.
    pub fn restore_with_options(
        &self,
        snapshot_path: &str,
        overlay: Option<String>,
        net: Option<Vec<NetConfig>>,
        memory_policy: RestoreMemoryPolicy,
    ) -> Result<(), VmmError> {
        self.restore_with_integrity(snapshot_path, overlay, net, memory_policy, None)
    }

    pub fn restore_with_integrity(
        &self,
        snapshot_path: &str,
        overlay: Option<String>,
        net: Option<Vec<NetConfig>>,
        memory_policy: RestoreMemoryPolicy,
        memory_integrity: Option<MemoryIntegrity>,
    ) -> Result<(), VmmError> {
        self.restore_with_resource_overrides(
            snapshot_path,
            overlay,
            net,
            None,
            memory_policy,
            memory_integrity,
        )
    }

    pub fn restore_with_resource_overrides(
        &self,
        snapshot_path: &str,
        overlay: Option<String>,
        net: Option<Vec<NetConfig>>,
        volumes: Option<Vec<VolumeConfig>>,
        memory_policy: RestoreMemoryPolicy,
        memory_integrity: Option<MemoryIntegrity>,
    ) -> Result<(), VmmError> {
        self.restore_with_memory_target(
            snapshot_path,
            overlay,
            net,
            volumes,
            memory_policy,
            memory_integrity,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)] // Preserve the existing restore resource-override API.
    pub fn restore_with_memory_target(
        &self,
        snapshot_path: &str,
        overlay: Option<String>,
        net: Option<Vec<NetConfig>>,
        volumes: Option<Vec<VolumeConfig>>,
        memory_policy: RestoreMemoryPolicy,
        memory_integrity: Option<MemoryIntegrity>,
        target_memory_mib: Option<u64>,
    ) -> Result<(), VmmError> {
        match self.request_ok(&ApiRequest::Restore {
            target_memory_mib,
            snapshot_path: snapshot_path.to_string(),
            overlay,
            net,
            volumes,
            memory_policy,
            memory_integrity,
        })? {
            ApiResponse::Restored | ApiResponse::Ok => Ok(()),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    pub fn repair_guest_network(&self, network: GuestNetworkRepair) -> Result<(), VmmError> {
        match self.request_ok(&ApiRequest::RepairGuestNetwork { network })? {
            ApiResponse::GuestNetworkRepaired | ApiResponse::Ok => Ok(()),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }
}

/// Poll until the socket file exists (used after spawning `vmm serve`).
pub fn wait_for_socket(path: &Path, timeout: Duration) -> Result<(), VmmError> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(VmmError::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("socket {} did not appear", path.display()),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    static NEXT_SOCKET_ID: AtomicUsize = AtomicUsize::new(0);

    struct SocketPath(std::path::PathBuf);

    impl Drop for SocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn socket_path() -> SocketPath {
        let id = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
        SocketPath(std::path::PathBuf::from(format!(
            ".tarit-vmm-client-{}-{id}.sock",
            std::process::id()
        )))
    }

    #[test]
    fn cancellation_before_connect_sends_no_exec_request() {
        let socket =
            std::env::temp_dir().join(format!("tarit-cancel-before-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let cancellation = Arc::new(RequestCancellation::default());
        cancellation.cancel();
        let result = VmmClient::new(&socket)
            .with_request_timeout(Duration::from_secs(1))
            .with_cancellation(cancellation)
            .exec("must not run", 3_600_000);
        assert!(result.is_err());
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn request_timeout_defaults_and_explicit_exec_budgets() {
        let client = VmmClient::new("/vmm.sock");
        assert_eq!(
            client.request_budget(&ApiRequest::Status),
            DEFAULT_REQUEST_TIMEOUT
        );
        let exec = ApiRequest::Exec {
            command: "sleep 60".into(),
            timeout_ms: 60000,
        };
        assert_eq!(client.request_budget(&exec), Duration::from_secs(65));
        assert_eq!(
            client
                .with_request_timeout(Duration::from_secs(1))
                .request_budget(&exec),
            Duration::from_secs(1)
        );
        assert_eq!(
            VmmClient::new("/vmm.sock")
                .with_request_timeout(Duration::from_millis(200))
                .request_timeout,
            Some(Duration::from_millis(200))
        );
    }

    #[test]
    fn restore_request_round_trips_with_lazy_memory_policy() {
        let req = ApiRequest::Restore {
            target_memory_mib: None,
            snapshot_path: "/snapshots/golden.snap".into(),
            memory_integrity: None,
            overlay: None,
            net: None,
            volumes: None,
            memory_policy: RestoreMemoryPolicy::Lazy,
        };

        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "op": "restore",
                "snapshot_path": "/snapshots/golden.snap",
                "memory_policy": "lazy",
            })
        );
    }

    #[test]
    fn request_phase_timeout_uses_the_remaining_request_budget() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(200);

        assert_eq!(
            request_phase_timeout(Some(deadline), now, Duration::from_secs(1))
                .expect("request has time remaining"),
            Duration::from_millis(200)
        );
        assert_eq!(
            request_phase_timeout(Some(deadline), now, Duration::from_millis(50))
                .expect("request has time remaining"),
            Duration::from_millis(50)
        );
        assert_eq!(
            request_phase_timeout(
                Some(deadline),
                now + Duration::from_millis(150),
                Duration::from_secs(1)
            )
            .expect("request has time remaining"),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn expired_request_deadline_returns_a_timeout() {
        let now = Instant::now();
        let error = request_phase_timeout(Some(now), now, Duration::from_secs(1))
            .expect_err("an expired request must not start another phase");

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn full_accept_queue_error_retries_connection() {
        assert!(retry_connect_error(&std::io::Error::from_raw_os_error(
            libc::EAGAIN
        )));
        assert!(retry_connect_error(&std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused
        )));
        assert!(!retry_connect_error(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn extreme_timeouts_do_not_overflow_monotonic_deadlines() {
        let socket = socket_path();
        let error = VmmClient::new(&socket.0)
            .with_connect_timeout(Duration::MAX)
            .with_request_timeout(Duration::from_millis(10))
            .status()
            .unwrap_err();
        assert!(
            matches!(error, VmmError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut)
        );
        let error = VmmClient::new(&socket.0)
            .with_request_timeout(Duration::MAX)
            .status()
            .unwrap_err();
        assert!(
            matches!(error, VmmError::Io(error) if error.kind() == std::io::ErrorKind::InvalidInput)
        );
    }

    #[test]
    fn request_timeout_covers_connect_retry() {
        let socket = socket_path();

        let error = VmmClient::new(&socket.0)
            .with_request_timeout(Duration::from_millis(100))
            .status()
            .expect_err("a missing VMM socket must time out");

        assert!(matches!(
            error,
            VmmError::Io(ref error) if error.kind() == std::io::ErrorKind::TimedOut
        ));
    }

    #[test]
    fn request_deadline_covers_partial_response_header_and_body() {
        for partial in [&[0_u8, 0][..], &[0, 0, 0, 4, b'{'][..]] {
            let socket = socket_path();
            let listener = std::os::unix::net::UnixListener::bind(&socket.0).unwrap();
            let (release_tx, release_rx) = mpsc::channel();
            let partial = partial.to_vec();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                read_api_frame(&mut stream).unwrap();
                stream.write_all(&partial).unwrap();
                release_rx.recv().unwrap();
            });
            let result = VmmClient::new(&socket.0)
                .with_request_timeout(Duration::from_millis(100))
                .status();
            release_tx.send(()).unwrap();
            server.join().unwrap();
            assert!(
                matches!(result, Err(VmmError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut)
            );
        }
    }

    #[test]
    fn cancellation_interrupts_a_request_under_the_default_deadline() {
        // Requests without an explicit timeout now always take the deadline
        // path; delete-time cancellation must still cut them short.
        let socket = socket_path();
        let listener = std::os::unix::net::UnixListener::bind(&socket.0).unwrap();
        let (received_tx, received_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_api_frame(&mut stream).unwrap();
            received_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        let cancellation = Arc::new(RequestCancellation::default());
        let client = VmmClient::new(&socket.0).with_cancellation(Arc::clone(&cancellation));
        let started = Instant::now();
        let request = std::thread::spawn(move || client.exec("sleep 60", 60_000));
        received_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("exec request reached the VMM");
        cancellation.cancel();
        let result = request.join().unwrap();
        let elapsed = started.elapsed();
        release_tx.send(()).unwrap();
        server.join().unwrap();
        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_secs(10),
            "cancelled exec waited {elapsed:?}"
        );
    }

    #[test]
    fn default_request_deadline_bounds_a_silent_peer() {
        let socket = socket_path();
        let listener = std::os::unix::net::UnixListener::bind(&socket.0).unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_api_frame(&mut stream).unwrap();
            release_rx.recv().unwrap();
        });
        let started = Instant::now();
        let result = VmmClient::new(&socket.0).status();
        release_tx.send(()).unwrap();
        server.join().unwrap();
        assert!(
            matches!(result, Err(VmmError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut)
        );
        assert!(started.elapsed() < DEFAULT_REQUEST_TIMEOUT + Duration::from_secs(5));
    }

    #[test]
    fn oversized_request_frames_are_rejected_before_writing() {
        assert!(checked_api_frame_len(usize::MAX).is_err());
        let body = vec![0; MAX_API_FRAME_LEN + 1];
        for bounded in [false, true] {
            let (mut writer, mut reader) = UnixStream::pair().unwrap();
            let result = if bounded {
                write_api_frame_until(&mut writer, &body, Instant::now() + Duration::from_secs(1))
            } else {
                write_api_frame(&mut writer, &body)
            };
            assert!(
                matches!(result, Err(VmmError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidInput)
            );
            reader.set_nonblocking(true).unwrap();
            assert_eq!(
                reader.read(&mut [0; 1]).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }

    #[test]
    fn request_timeout_covers_the_response_read() {
        let socket = socket_path();
        let listener =
            std::os::unix::net::UnixListener::bind(&socket.0).expect("bind test VMM socket");
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            read_api_frame(&mut stream).expect("read request");
            release_rx.recv().expect("release server");
        });

        let error = VmmClient::new(&socket.0)
            .with_request_timeout(Duration::from_millis(100))
            .status()
            .expect_err("a response that never arrives must time out");

        assert!(matches!(
            error,
            VmmError::Io(ref error) if error.kind() == std::io::ErrorKind::TimedOut
        ));
        release_tx.send(()).expect("release server");
        server.join().expect("join server");
    }

    #[test]
    fn request_timeout_covers_a_peer_that_stops_reading() {
        let socket = socket_path();
        let listener =
            std::os::unix::net::UnixListener::bind(&socket.0).expect("bind test VMM socket");
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().expect("accept client");
            // Keep the connection open without consuming the request. A large
            // command must fill the socket send buffer and exercise backpressure.
            release_rx.recv().expect("release server");
        });
        let (result_tx, result_rx) = mpsc::channel();
        let path = socket.0.clone();
        let client = std::thread::spawn(move || {
            let result = VmmClient::new(path)
                .with_request_timeout(Duration::from_millis(100))
                .exec(&"x".repeat(8 * 1024 * 1024), 1000);
            result_tx.send(result).expect("send request result");
        });

        let result = result_rx.recv_timeout(Duration::from_secs(2));
        // Always release and join the peer, including when the regression
        // leaves the client stuck in a blocking write past its deadline.
        release_tx.send(()).expect("release server");
        server.join().expect("join server");
        client.join().expect("join client");

        let error = result
            .expect("the request must time out without the peer closing")
            .expect_err("a peer that does not read must time out");
        assert!(matches!(
            error,
            VmmError::Io(ref error) if error.kind() == std::io::ErrorKind::TimedOut
        ));
    }

    #[test]
    fn large_request_survives_write_backpressure_within_deadline() {
        let socket = socket_path();
        let listener =
            std::os::unix::net::UnixListener::bind(&socket.0).expect("bind test VMM socket");
        let command = "x".repeat(8 * 1024 * 1024);
        let expected_command = command.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            std::thread::sleep(Duration::from_millis(100));
            let request: ApiRequest =
                serde_json::from_slice(&read_api_frame(&mut stream).expect("read request"))
                    .expect("decode request");
            assert!(matches!(
                request,
                ApiRequest::Exec { command, .. } if command == expected_command
            ));
            let body = serde_json::to_vec(&ApiResponse::Ok).expect("encode response");
            write_api_frame(&mut stream, &body).expect("write response");
        });

        let result = VmmClient::new(&socket.0)
            .with_request_timeout(Duration::from_secs(5))
            .request_ok(&ApiRequest::Exec {
                command,
                timeout_ms: 1000,
            });
        server.join().expect("join server");
        assert!(matches!(result.expect("complete request"), ApiResponse::Ok));
    }

    #[test]
    fn slow_response_within_request_budget_survives_read_poll_intervals() {
        let socket = socket_path();
        let listener =
            std::os::unix::net::UnixListener::bind(&socket.0).expect("bind test VMM socket");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            read_api_frame(&mut stream).expect("read request");
            // Stay silent for several connect-timeout poll intervals, like a
            // suspend copying guest RAM, then answer within the request budget.
            std::thread::sleep(Duration::from_millis(400));
            let body = serde_json::to_vec(&ApiResponse::Ok).expect("encode response");
            write_api_frame(&mut stream, &body).expect("write response");
        });

        VmmClient::new(&socket.0)
            .with_connect_timeout(Duration::from_millis(100))
            .with_request_timeout(Duration::from_secs(5))
            .suspend()
            .expect("a slow suspend within the request budget must succeed");
        server.join().expect("join server");
    }

    #[test]
    fn restore_request_round_trips_without_overlay() {
        let req = ApiRequest::Restore {
            target_memory_mib: None,
            snapshot_path: "/snapshots/golden.snap".into(),
            memory_integrity: None,
            overlay: None,
            net: None,
            volumes: None,
            memory_policy: RestoreMemoryPolicy::Auto,
        };

        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "op": "restore",
                "snapshot_path": "/snapshots/golden.snap",
            })
        );

        let decoded: ApiRequest = serde_json::from_value(value).unwrap();
        match decoded {
            ApiRequest::Restore {
                target_memory_mib: None,
                snapshot_path,
                memory_integrity,
                overlay,
                net,
                volumes,
                memory_policy,
            } => {
                assert_eq!(snapshot_path, "/snapshots/golden.snap");
                assert!(memory_integrity.is_none());
                assert_eq!(overlay, None);
                assert!(net.is_none());
                assert!(volumes.is_none());
                assert_eq!(memory_policy, RestoreMemoryPolicy::Auto);
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn restore_request_round_trips_with_overlay() {
        let req = ApiRequest::Restore {
            target_memory_mib: None,
            snapshot_path: "/snapshots/golden.snap".into(),
            memory_integrity: None,
            overlay: Some("/overlays/clone.cow".into()),
            net: None,
            volumes: None,
            memory_policy: RestoreMemoryPolicy::Auto,
        };

        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "op": "restore",
                "snapshot_path": "/snapshots/golden.snap",
                "overlay": "/overlays/clone.cow",
            })
        );

        let decoded: ApiRequest = serde_json::from_value(value).unwrap();
        match decoded {
            ApiRequest::Restore {
                target_memory_mib: None,
                snapshot_path,
                memory_integrity,
                overlay,
                net,
                volumes,
                memory_policy,
            } => {
                assert_eq!(snapshot_path, "/snapshots/golden.snap");
                assert!(memory_integrity.is_none());
                assert_eq!(overlay, Some("/overlays/clone.cow".into()));
                assert!(net.is_none());
                assert!(volumes.is_none());
                assert_eq!(memory_policy, RestoreMemoryPolicy::Auto);
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    fn memory_snapshot_response(path: &str) -> ApiResponse {
        ApiResponse::Snapshot {
            path: path.into(),
            overlay_path: None,
            volume_paths: Vec::new(),
            integrity_path: None,
            live_stats: None,
        }
    }

    /// Serve one request on a fresh VMM socket and return the request's wire
    /// form together with the client's result.
    fn serve_one_snapshot<T>(
        response: ApiResponse,
        call: impl FnOnce(&VmmClient) -> T,
    ) -> (serde_json::Value, T) {
        let socket = socket_path();
        let listener =
            std::os::unix::net::UnixListener::bind(&socket.0).expect("bind test VMM socket");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            let request: serde_json::Value =
                serde_json::from_slice(&read_api_frame(&mut stream).expect("read request"))
                    .expect("decode request");
            let body = serde_json::to_vec(&response).expect("encode response");
            write_api_frame(&mut stream, &body).expect("write response");
            request
        });
        let result = call(&VmmClient::new(&socket.0).with_request_timeout(Duration::from_secs(5)));
        (server.join().expect("join server"), result)
    }

    #[test]
    fn snapshot_unreleased_with_sends_checkpoint_options_only_when_set() {
        let cases = [
            (
                Some("/snapshots/base.snap"),
                true,
                serde_json::json!({
                    "op": "snapshot",
                    "diff": false,
                    "live": false,
                    "capture_disks": false,
                    "base": "/snapshots/base.snap",
                    "resume": true,
                }),
            ),
            (
                Some("/snapshots/base.snap"),
                false,
                serde_json::json!({
                    "op": "snapshot",
                    "diff": false,
                    "live": false,
                    "capture_disks": false,
                    "base": "/snapshots/base.snap",
                }),
            ),
            (
                None,
                true,
                serde_json::json!({
                    "op": "snapshot",
                    "diff": false,
                    "live": false,
                    "capture_disks": false,
                    "resume": true,
                }),
            ),
            (
                None,
                false,
                serde_json::json!({
                    "op": "snapshot",
                    "diff": false,
                    "live": false,
                    "capture_disks": false,
                }),
            ),
        ];
        for (base, resume, expected) in cases {
            let (request, result) =
                serve_one_snapshot(memory_snapshot_response("/tmp/vmm-snap-1.snap"), |client| {
                    client.snapshot_unreleased_with(false, base, resume)
                });
            assert_eq!(request, expected, "base {base:?}, resume {resume}");
            assert_eq!(
                result.expect("memory-only snapshot"),
                "/tmp/vmm-snap-1.snap"
            );
        }
    }

    #[test]
    fn snapshot_unreleased_sends_no_base_and_no_resume() {
        for diff in [false, true] {
            let (request, result) =
                serve_one_snapshot(memory_snapshot_response("/tmp/vmm-snap-2.snap"), |client| {
                    client.snapshot_unreleased(diff)
                });
            assert_eq!(
                request,
                serde_json::json!({
                    "op": "snapshot",
                    "diff": diff,
                    "live": false,
                    "capture_disks": false,
                })
            );
            assert_eq!(
                result.expect("memory-only snapshot"),
                "/tmp/vmm-snap-2.snap"
            );
        }
    }

    #[test]
    fn snapshot_unreleased_with_rejects_a_disk_artifact() {
        let (_, result) = serve_one_snapshot(
            ApiResponse::Snapshot {
                path: "/tmp/vmm-snap-3.snap".into(),
                overlay_path: Some("/tmp/vmm-snap-3.cow".into()),
                volume_paths: Vec::new(),
                integrity_path: None,
                live_stats: None,
            },
            |client| client.snapshot_unreleased_with(false, Some("/snapshots/base.snap"), true),
        );
        let error = result.expect_err("a memory-only request must not accept a disk artifact");
        assert!(error.to_string().contains("disk artifact"), "{error}");
    }

    #[test]
    fn a_retryable_vmm_error_is_reported_as_busy() {
        let (_, result) = serve_one_snapshot(
            ApiResponse::retryable_err("quiesce block I/O worker: timed out"),
            |client| client.snapshot_unreleased_with(false, None, true),
        );
        assert!(matches!(
            result,
            Err(VmmError::Busy(ref reason)) if reason == "quiesce block I/O worker: timed out"
        ));
    }

    #[test]
    fn snapshot_unreleased_with_surfaces_a_vmm_error() {
        let (request, result) = serve_one_snapshot(
            ApiResponse::Err {
                msg: "snapshot failed".into(),
            },
            |client| client.snapshot_unreleased_with(false, Some("/snapshots/base.snap"), true),
        );
        assert_eq!(request["base"], "/snapshots/base.snap");
        assert!(matches!(
            result,
            Err(VmmError::Api(ref message)) if message == "snapshot failed"
        ));
    }
}
