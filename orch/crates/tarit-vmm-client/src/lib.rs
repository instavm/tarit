//! Unix-domain client for the Tarit VMM control API (length-prefixed JSON).

pub use tarit_proto::*;

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
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
}

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

fn write_api_frame(stream: &mut UnixStream, body: &[u8]) -> Result<(), VmmError> {
    let len = body.len() as u32;
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

fn retry_connect_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    ) || error.raw_os_error() == Some(libc::EAGAIN)
}

fn write_all_until(
    stream: &mut UnixStream,
    bytes: &[u8],
    deadline: Instant,
    fallback: Duration,
) -> Result<(), VmmError> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        stream.set_write_timeout(Some(request_phase_timeout(
            Some(deadline),
            Instant::now(),
            fallback,
        )?))?;
        let written = stream
            .write(remaining)
            .map_err(|error| map_deadline_io_error(error, deadline))?;
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
    fallback: Duration,
) -> Result<(), VmmError> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        stream.set_read_timeout(Some(request_phase_timeout(
            Some(deadline),
            Instant::now(),
            fallback,
        )?))?;
        let read = stream
            .read(remaining)
            .map_err(|error| map_deadline_io_error(error, deadline))?;
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

fn write_api_frame_until(
    stream: &mut UnixStream,
    body: &[u8],
    deadline: Instant,
    fallback: Duration,
) -> Result<(), VmmError> {
    let len = body.len() as u32;
    write_all_until(stream, &len.to_be_bytes(), deadline, fallback)?;
    write_all_until(stream, body, deadline, fallback)?;
    stream.set_write_timeout(Some(request_phase_timeout(
        Some(deadline),
        Instant::now(),
        fallback,
    )?))?;
    stream
        .flush()
        .map_err(|error| map_deadline_io_error(error, deadline))
}

fn read_api_frame_until(
    stream: &mut UnixStream,
    deadline: Instant,
    fallback: Duration,
) -> Result<Vec<u8>, VmmError> {
    let mut len_buf = [0u8; 4];
    read_exact_until(stream, &mut len_buf, deadline, fallback)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_API_FRAME_LEN {
        return Err(VmmError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        )));
    }
    let mut body = vec![0u8; len];
    read_exact_until(stream, &mut body, deadline, fallback)?;
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
    stream.set_nonblocking(false)?;
    Ok(stream)
}

/// Client for a single VMM instance (one UDS, one VM).
pub struct VmmClient {
    socket_path: String,
    connect_timeout: Duration,
    request_timeout: Option<Duration>,
}

impl VmmClient {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().display().to_string(),
            connect_timeout: Duration::from_secs(5),
            request_timeout: None,
        }
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Limit a request's total connect, write, and response-read time.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
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
        let connect_deadline = deadline.min(started + self.connect_timeout);

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

    pub fn request(&self, req: &ApiRequest) -> Result<ApiResponse, VmmError> {
        let body = serde_json::to_vec(req)?;
        let deadline = self.request_timeout.map(|timeout| Instant::now() + timeout);
        let mut stream = self.connect_for_request(deadline)?;
        let resp_body = if let Some(deadline) = deadline {
            write_api_frame_until(&mut stream, &body, deadline, self.connect_timeout)?;
            read_api_frame_until(&mut stream, deadline, self.connect_timeout)?
        } else {
            write_api_frame(&mut stream, &body)?;
            read_api_frame(&mut stream)?
        };
        let resp: ApiResponse = serde_json::from_slice(&resp_body)?;
        Ok(resp)
    }

    pub fn request_ok(&self, req: &ApiRequest) -> Result<ApiResponse, VmmError> {
        match self.request(req)? {
            ApiResponse::Err { msg } => Err(VmmError::Api(msg)),
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

    pub fn resume(&self) -> Result<(), VmmError> {
        match self.request_ok(&ApiRequest::Resume)? {
            ApiResponse::Ok => Ok(()),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
    }

    pub fn snapshot(&self, diff: bool) -> Result<String, VmmError> {
        match self.request_ok(&ApiRequest::Snapshot { diff })? {
            ApiResponse::Snapshot { path } => Ok(path),
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
            ApiResponse::EgressUpdated { rules_applied } => Ok(rules_applied),
            other => Err(VmmError::Api(format!("unexpected response: {other:?}"))),
        }
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
        match self.request_ok(&ApiRequest::Restore {
            snapshot_path: snapshot_path.to_string(),
            overlay,
        })? {
            ApiResponse::Restored | ApiResponse::Ok => Ok(()),
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
    fn request_timeout_is_opt_in() {
        assert_eq!(VmmClient::new("/vmm.sock").request_timeout, None);
        assert_eq!(
            VmmClient::new("/vmm.sock")
                .with_request_timeout(Duration::from_millis(200))
                .request_timeout,
            Some(Duration::from_millis(200))
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
    fn restore_request_round_trips_without_overlay() {
        let req = ApiRequest::Restore {
            snapshot_path: "/snapshots/golden.snap".into(),
            overlay: None,
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
                snapshot_path,
                overlay,
            } => {
                assert_eq!(snapshot_path, "/snapshots/golden.snap");
                assert_eq!(overlay, None);
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn restore_request_round_trips_with_overlay() {
        let req = ApiRequest::Restore {
            snapshot_path: "/snapshots/golden.snap".into(),
            overlay: Some("/overlays/clone.cow".into()),
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
                snapshot_path,
                overlay,
            } => {
                assert_eq!(snapshot_path, "/snapshots/golden.snap");
                assert_eq!(overlay, Some("/overlays/clone.cow".into()));
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }
}
