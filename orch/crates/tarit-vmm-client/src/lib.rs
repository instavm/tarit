//! Unix-domain client for the Tarit VMM control API (length-prefixed JSON).

pub use tarit_proto::*;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;
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

/// Client for a single VMM instance (one UDS, one VM).
pub struct VmmClient {
    socket_path: String,
    connect_timeout: Duration,
}

impl VmmClient {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().display().to_string(),
            connect_timeout: Duration::from_secs(5),
        }
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
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

    pub fn request(&self, req: &ApiRequest) -> Result<ApiResponse, VmmError> {
        let mut stream = self.connect()?;
        let body = serde_json::to_vec(req)?;
        write_api_frame(&mut stream, &body)?;
        let resp_body = read_api_frame(&mut stream)?;
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
