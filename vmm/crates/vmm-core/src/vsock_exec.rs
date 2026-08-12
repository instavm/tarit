//! Host side of the vsock exec channel.
//!
//! The virtio-vsock device bridges the guest agent's outbound connection (guest
//! -> host CID 2, port 1024) to a per-VM Unix control socket. This module binds
//! that socket, accepts the guest's connection, and runs exec commands over it.
//! Newer guests advertise a chunked protocol with explicit stdout/stderr frames;
//! older guests stay on the legacy line protocol.

#![cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]

use crate::gc::OwnedScratchFile;
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use vmm_sys_util::eventfd::EventFd;

const READY_LINE: &str = "VMM_AGENT_READY";
const CHUNKED_CAPABILITY_LINE: &str = "VMM_VSOCK_EXEC_PROTO=2";
const ACCEPT_POLL_TIMEOUT: i32 = 250;
const CONNECT_READY_TIMEOUT: Duration = Duration::from_millis(500);
const CAPABILITY_PROBE_TIMEOUT: Duration = Duration::from_millis(25);
const EXEC_IO_TIMEOUT: Duration = Duration::from_millis(200);
const EXEC_ACC_TAIL_CAP: usize = 64 * 1024;
const EXEC_FRAME_MAGIC: &[u8; 4] = b"VEX2";
const EXEC_FRAME_VERSION: u8 = 2;
const EXEC_FRAME_HEADER_LEN: usize = 10;
const EXEC_FRAME_MAX_PAYLOAD: usize = 1024 * 1024;
const EXEC_CHUNK_MAX_BYTES: usize = 64 * 1024;
const EXEC_SPOOL_MEMORY_CAP: usize = 512 * 1024;
const EXEC_OUTPUT_MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecProtocol {
    MarkerV1,
    ChunkedV2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ExecFrameKind {
    Request = 1,
    Start = 2,
    Stdout = 3,
    Stderr = 4,
    Exit = 5,
    Error = 6,
}

impl ExecFrameKind {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Request),
            2 => Some(Self::Start),
            3 => Some(Self::Stdout),
            4 => Some(Self::Stderr),
            5 => Some(Self::Exit),
            6 => Some(Self::Error),
            _ => None,
        }
    }
}

struct ExecConnection {
    id: u64,
    protocol: ExecProtocol,
    stream: UnixStream,
}

/// A live exec channel over vsock. Holds the accepted guest connection (if the
/// agent has dialed) and re-accepts on reconnect.
pub struct VsockExecChannel {
    stream: Arc<Mutex<Option<ExecConnection>>>,
    stop: Arc<AtomicBool>,
    pump_wake: Option<EventFd>,
    handle: Mutex<Option<JoinHandle<()>>>,
    exec_gate: Mutex<()>,
    next_request_id: AtomicU64,
}

/// Why a vsock exec did not return a result. The split matters because exec is
/// not replay-safe: once the command line reached the guest, re-sending it on
/// another channel risks running it twice.
#[derive(Debug)]
pub enum VsockExecError {
    /// The command line was not delivered. Retrying on serial is safe.
    NotDelivered(String),
    /// The command was (or may have been) delivered but the exchange did not
    /// complete. The guest may still run it; do not re-send.
    Ambiguous(String),
}

impl std::fmt::Display for VsockExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotDelivered(e) => write!(f, "not delivered: {e}"),
            Self::Ambiguous(e) => write!(f, "ambiguous after dispatch: {e}"),
        }
    }
}

enum RunExecOutcome {
    Completed((i32, String, String, u64)),
    TimedOut {
        started: bool,
        stdout: String,
        stderr: String,
        duration_ms: u64,
    },
    WriteFailed(String),
    TransportFailed(String),
}

struct ExecFrame {
    kind: ExecFrameKind,
    payload: Vec<u8>,
}

struct OutputSpool {
    _owned: OwnedScratchFile,
    writer: File,
}

struct OutputSink {
    memory: Vec<u8>,
    spool: Option<OutputSpool>,
}

struct ExecOutputs {
    stdout: OutputSink,
    stderr: OutputSink,
    total_bytes: usize,
}

impl OutputSink {
    fn new() -> Self {
        Self {
            memory: Vec::new(),
            spool: None,
        }
    }

    fn append(&mut self, label: &str, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.spool.is_none()
            && self.memory.len().saturating_add(bytes.len()) > EXEC_SPOOL_MEMORY_CAP
        {
            let path = crate::controller::unique_runtime_file_path("vsock-exec", label)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            let owned = OwnedScratchFile::create_new(path)?;
            let mut writer = owned.file().try_clone()?;
            writer.write_all(&self.memory)?;
            self.memory.clear();
            self.spool = Some(OutputSpool {
                _owned: owned,
                writer,
            });
        }
        if let Some(spool) = self.spool.as_mut() {
            spool.writer.write_all(bytes)?;
        } else {
            self.memory.extend_from_slice(bytes);
        }
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<String> {
        if let Some(mut spool) = self.spool.take() {
            spool.writer.flush()?;
            spool.writer.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            spool.writer.read_to_end(&mut bytes)?;
            drop(spool);
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        } else {
            Ok(String::from_utf8_lossy(&self.memory).into_owned())
        }
    }
}

impl ExecOutputs {
    fn new() -> Self {
        Self {
            stdout: OutputSink::new(),
            stderr: OutputSink::new(),
            total_bytes: 0,
        }
    }

    fn stdout(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.account(bytes.len())?;
        self.stdout.append("stdout", bytes)
    }

    fn stderr(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.account(bytes.len())?;
        self.stderr.append("stderr", bytes)
    }

    fn account(&mut self, len: usize) -> std::io::Result<()> {
        self.total_bytes = self
            .total_bytes
            .checked_add(len)
            .ok_or_else(|| std::io::Error::other("exec output length overflow"))?;
        if self.total_bytes > EXEC_OUTPUT_MAX_BYTES {
            return Err(std::io::Error::other(format!(
                "exec output exceeds {} MiB limit",
                EXEC_OUTPUT_MAX_BYTES / (1024 * 1024)
            )));
        }
        Ok(())
    }

    fn finish(self) -> Result<(String, String), String> {
        let stdout = self.stdout.finish().map_err(|error| error.to_string())?;
        let stderr = self.stderr.finish().map_err(|error| error.to_string())?;
        Ok((stdout, stderr))
    }
}

impl VsockExecChannel {
    pub fn bind(control_socket: &Path) -> std::io::Result<Arc<Self>> {
        Self::bind_with_pump_wake(control_socket, None)
    }

    pub fn bind_with_pump_wake(
        control_socket: &Path,
        pump_wake: Option<EventFd>,
    ) -> std::io::Result<Arc<Self>> {
        let _ = std::fs::remove_file(control_socket);
        let listener = UnixListener::bind(control_socket)?;
        listener.set_nonblocking(true)?;

        let stream = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let next_connection_id = Arc::new(AtomicU64::new(1));
        let stream_t = Arc::clone(&stream);
        let stop_t = Arc::clone(&stop);
        let next_connection_id_t = Arc::clone(&next_connection_id);

        let listener_fd = listener.as_raw_fd();
        let handle = std::thread::Builder::new()
            .name("vsock-exec-accept".into())
            .spawn(move || {
                while !stop_t.load(Ordering::Relaxed) {
                    let mut pfd = libc::pollfd {
                        fd: listener_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: `pfd` points to one initialized pollfd and the
                    // listener fd remains open for the lifetime of this thread.
                    if unsafe { libc::poll(&mut pfd, 1, ACCEPT_POLL_TIMEOUT) } <= 0 {
                        continue;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let id = next_connection_id_t.fetch_add(1, Ordering::Relaxed);
                            match prepare_connection(stream, id) {
                                Ok(connection) => {
                                    log::info!(
                                        "vsock exec: guest agent connected (protocol={:?}, conn={})",
                                        connection.protocol,
                                        connection.id
                                    );
                                    *stream_t.lock().unwrap_or_else(|e| e.into_inner()) = Some(connection);
                                }
                                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                                Err(error) => {
                                    log::warn!("vsock exec: rejected guest connection: {error}");
                                }
                            }
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                        Err(_) => std::thread::sleep(Duration::from_millis(50)),
                    }
                }
            })?;

        Ok(Arc::new(Self {
            stream,
            stop,
            pump_wake,
            handle: Mutex::new(Some(handle)),
            exec_gate: Mutex::new(()),
            next_request_id: AtomicU64::new(1),
        }))
    }

    pub fn is_connected(&self) -> bool {
        self.stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub fn exec(
        &self,
        command: &str,
        timeout: Duration,
    ) -> Option<Result<(i32, String, String, u64), VsockExecError>> {
        let _exec_guard = self.exec_gate.lock().unwrap_or_else(|e| e.into_inner());
        let (connection_id, protocol, mut stream) = {
            let guard = self.stream.lock().unwrap_or_else(|e| e.into_inner());
            let connection = guard.as_ref()?;
            let connection_id = connection.id;
            let stream = match connection.stream.try_clone() {
                Ok(stream) => stream,
                Err(error) => {
                    drop(guard);
                    self.clear_connection(connection_id);
                    return Some(Err(VsockExecError::Ambiguous(format!(
                        "clone exec stream: {error}"
                    ))));
                }
            };
            (connection_id, connection.protocol, stream)
        };
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let outcome = run_exec(
            &mut stream,
            protocol,
            request_id,
            command,
            timeout,
            self.pump_wake.as_ref(),
        );
        let (result, stream_intact) = match outcome {
            RunExecOutcome::Completed(result) => (Ok(result), true),
            RunExecOutcome::TimedOut {
                started: true,
                stdout,
                stderr,
                duration_ms,
            } => (Ok((-1, stdout, stderr, duration_ms)), false),
            RunExecOutcome::TimedOut { started: false, .. } => (
                Err(VsockExecError::Ambiguous(
                    "timed out before the guest acknowledged the command".into(),
                )),
                false,
            ),
            RunExecOutcome::WriteFailed(error) => (Err(VsockExecError::NotDelivered(error)), false),
            RunExecOutcome::TransportFailed(error) => {
                (Err(VsockExecError::Ambiguous(error)), false)
            }
        };
        if !stream_intact {
            self.clear_connection(connection_id);
        }
        Some(result)
    }

    fn clear_connection(&self, connection_id: u64) {
        let mut guard = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        if guard
            .as_ref()
            .is_some_and(|connection| connection.id == connection_id)
        {
            *guard = None;
        }
    }
}

impl Drop for VsockExecChannel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }
}

fn prepare_connection(
    mut stream: UnixStream,
    connection_id: u64,
) -> std::io::Result<ExecConnection> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(CONNECT_READY_TIMEOUT));
    let ready = read_text_line(&mut stream, CONNECT_READY_TIMEOUT)?
        .ok_or_else(|| std::io::Error::new(ErrorKind::TimedOut, "guest agent ready timeout"))?;
    if ready != READY_LINE {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("unexpected vsock exec banner: {ready:?}"),
        ));
    }
    let _ = stream.set_read_timeout(Some(CAPABILITY_PROBE_TIMEOUT));
    let protocol = match read_text_line(&mut stream, CAPABILITY_PROBE_TIMEOUT)? {
        Some(line) if line == CHUNKED_CAPABILITY_LINE => ExecProtocol::ChunkedV2,
        Some(line) => {
            log::warn!("vsock exec: ignoring unexpected capability line {line:?}");
            ExecProtocol::MarkerV1
        }
        None => ExecProtocol::MarkerV1,
    };
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(None);
    Ok(ExecConnection {
        id: connection_id,
        protocol,
        stream,
    })
}

fn run_exec(
    stream: &mut UnixStream,
    protocol: ExecProtocol,
    request_id: u64,
    command: &str,
    timeout: Duration,
    pump_wake: Option<&EventFd>,
) -> RunExecOutcome {
    let _ = stream.set_read_timeout(Some(EXEC_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(EXEC_IO_TIMEOUT));
    match protocol {
        ExecProtocol::MarkerV1 => run_exec_marker_v1(stream, command, timeout, pump_wake),
        ExecProtocol::ChunkedV2 => {
            run_exec_chunked_v2(stream, request_id, command, timeout, pump_wake)
        }
    }
}

fn run_exec_marker_v1(
    stream: &mut UnixStream,
    command: &str,
    timeout: Duration,
    pump_wake: Option<&EventFd>,
) -> RunExecOutcome {
    let start = Instant::now();
    let deadline = start.checked_add(timeout).unwrap_or(start);
    let msg = format!("VMM_EXEC:{command}\n");
    if let Err(error) =
        write_all_before(stream, msg.as_bytes(), deadline).and_then(|_| stream.flush())
    {
        return RunExecOutcome::WriteFailed(format!("vsock exec write: {error}"));
    }
    if let Some(evt) = pump_wake {
        let _ = evt.write(1);
    }

    let mut acc = Vec::new();
    let mut outputs = ExecOutputs::new();
    let mut started = false;
    let mut buf = [0u8; 4096];

    while start.elapsed() < timeout {
        match stream.read(&mut buf) {
            Ok(0) => return RunExecOutcome::TransportFailed("vsock exec: peer closed".into()),
            Ok(read) => acc.extend_from_slice(&buf[..read]),
            Err(error)
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut =>
            {
                continue
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                return RunExecOutcome::TransportFailed(format!("vsock exec read: {error}"));
            }
        }
        while let Some(pos) = acc.iter().position(|&byte| byte == b'\n') {
            let mut line: Vec<u8> = acc.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let text = String::from_utf8_lossy(&line);
            if text == READY_LINE || text == CHUNKED_CAPABILITY_LINE {
                continue;
            }
            if text == "VMM_EXEC_START" {
                started = true;
                continue;
            }
            if let Some(code) = text.strip_prefix("VMM_EXEC_EXIT=") {
                let exit_code: i32 = code.trim().parse().unwrap_or(0);
                let (stdout, stderr) = match outputs.finish() {
                    Ok(outputs) => outputs,
                    Err(error) => return RunExecOutcome::TransportFailed(error),
                };
                return RunExecOutcome::Completed((
                    exit_code,
                    stdout,
                    stderr,
                    start.elapsed().as_millis() as u64,
                ));
            }
            if started {
                if let Err(error) = outputs.stdout(&line).and_then(|()| outputs.stdout(b"\n")) {
                    return RunExecOutcome::TransportFailed(error.to_string());
                }
            }
        }
        if let Err(error) = trim_exec_accumulator(&mut acc, started, &mut outputs) {
            return RunExecOutcome::TransportFailed(error.to_string());
        }
    }
    let (stdout, stderr) = match outputs.finish() {
        Ok(outputs) => outputs,
        Err(error) => return RunExecOutcome::TransportFailed(error),
    };
    RunExecOutcome::TimedOut {
        started,
        stdout,
        stderr,
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

fn run_exec_chunked_v2(
    stream: &mut UnixStream,
    request_id: u64,
    command: &str,
    timeout: Duration,
    pump_wake: Option<&EventFd>,
) -> RunExecOutcome {
    let start = Instant::now();
    let deadline = start.checked_add(timeout).unwrap_or(start);
    let mut payload = Vec::with_capacity(12 + command.len());
    payload.extend_from_slice(&request_id.to_be_bytes());
    let command_len = match u32::try_from(command.len()) {
        Ok(length) => length,
        Err(_) => {
            return RunExecOutcome::WriteFailed("vsock exec command too large for protocol".into())
        }
    };
    payload.extend_from_slice(&command_len.to_be_bytes());
    payload.extend_from_slice(command.as_bytes());
    if let Err(error) = write_exec_frame_before(stream, ExecFrameKind::Request, &payload, deadline)
    {
        return RunExecOutcome::WriteFailed(format!("vsock exec frame write: {error}"));
    }
    if let Some(evt) = pump_wake {
        let _ = evt.write(1);
    }

    let mut outputs = ExecOutputs::new();
    let mut started = false;

    loop {
        let frame = match read_exec_frame_before(stream, deadline) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                let (stdout, stderr) = match outputs.finish() {
                    Ok(outputs) => outputs,
                    Err(error) => return RunExecOutcome::TransportFailed(error),
                };
                return RunExecOutcome::TimedOut {
                    started,
                    stdout,
                    stderr,
                    duration_ms: start.elapsed().as_millis() as u64,
                };
            }
            Err(error) => return RunExecOutcome::TransportFailed(error),
        };
        match frame.kind {
            ExecFrameKind::Request => {
                return RunExecOutcome::TransportFailed("guest echoed an exec request frame".into())
            }
            ExecFrameKind::Start => {
                let frame_request_id = match parse_request_only_frame(&frame.payload) {
                    Ok(request_id) => request_id,
                    Err(error) => return RunExecOutcome::TransportFailed(error),
                };
                if frame_request_id != request_id {
                    return RunExecOutcome::TransportFailed(format!(
                        "unexpected exec start for request {frame_request_id} (wanted {request_id})"
                    ));
                }
                started = true;
            }
            ExecFrameKind::Stdout => {
                let (frame_request_id, bytes) = match parse_chunk_frame(&frame.payload) {
                    Ok(parsed) => parsed,
                    Err(error) => return RunExecOutcome::TransportFailed(error),
                };
                if frame_request_id != request_id {
                    return RunExecOutcome::TransportFailed(format!(
                        "unexpected stdout frame for request {frame_request_id} (wanted {request_id})"
                    ));
                }
                if !started {
                    return RunExecOutcome::TransportFailed(
                        "stdout chunk arrived before exec start".into(),
                    );
                }
                if let Err(error) = outputs.stdout(bytes) {
                    return RunExecOutcome::TransportFailed(error.to_string());
                }
            }
            ExecFrameKind::Stderr => {
                let (frame_request_id, bytes) = match parse_chunk_frame(&frame.payload) {
                    Ok(parsed) => parsed,
                    Err(error) => return RunExecOutcome::TransportFailed(error),
                };
                if frame_request_id != request_id {
                    return RunExecOutcome::TransportFailed(format!(
                        "unexpected stderr frame for request {frame_request_id} (wanted {request_id})"
                    ));
                }
                if !started {
                    return RunExecOutcome::TransportFailed(
                        "stderr chunk arrived before exec start".into(),
                    );
                }
                if let Err(error) = outputs.stderr(bytes) {
                    return RunExecOutcome::TransportFailed(error.to_string());
                }
            }
            ExecFrameKind::Exit => {
                let (frame_request_id, exit_code) = match parse_exit_frame(&frame.payload) {
                    Ok(parsed) => parsed,
                    Err(error) => return RunExecOutcome::TransportFailed(error),
                };
                if frame_request_id != request_id {
                    return RunExecOutcome::TransportFailed(format!(
                        "unexpected exec exit for request {frame_request_id} (wanted {request_id})"
                    ));
                }
                let (stdout, stderr) = match outputs.finish() {
                    Ok(outputs) => outputs,
                    Err(error) => return RunExecOutcome::TransportFailed(error),
                };
                return RunExecOutcome::Completed((
                    exit_code,
                    stdout,
                    stderr,
                    start.elapsed().as_millis() as u64,
                ));
            }
            ExecFrameKind::Error => {
                let (frame_request_id, message) = match parse_error_frame(&frame.payload) {
                    Ok(parsed) => parsed,
                    Err(error) => return RunExecOutcome::TransportFailed(error),
                };
                if frame_request_id != request_id {
                    return RunExecOutcome::TransportFailed(format!(
                        "unexpected exec error for request {frame_request_id} (wanted {request_id})"
                    ));
                }
                return RunExecOutcome::TransportFailed(format!(
                    "guest vsock exec error: {message}"
                ));
            }
        }
    }
}

fn trim_exec_accumulator(
    acc: &mut Vec<u8>,
    started: bool,
    outputs: &mut ExecOutputs,
) -> std::io::Result<()> {
    if acc.len() <= EXEC_ACC_TAIL_CAP {
        return Ok(());
    }
    let drain_len = acc.len() - EXEC_ACC_TAIL_CAP;
    let drained: Vec<u8> = acc.drain(..drain_len).collect();
    if started {
        outputs.stdout(&drained)?;
    }
    Ok(())
}

fn read_text_line(stream: &mut UnixStream, timeout: Duration) -> std::io::Result<Option<String>> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "peer closed while reading line",
                ))
            }
            Ok(1) => {
                if byte[0] == b'\n' {
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
                }
                line.push(byte[0]);
                if line.len() > EXEC_ACC_TAIL_CAP {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        "line exceeds vsock exec control limit",
                    ));
                }
            }
            Ok(_) => unreachable!("single-byte reads return at most one byte"),
            Err(error)
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut =>
            {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
fn write_exec_frame(
    stream: &mut UnixStream,
    kind: ExecFrameKind,
    payload: &[u8],
) -> std::io::Result<()> {
    if payload.len() > EXEC_FRAME_MAX_PAYLOAD {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "vsock exec frame exceeds payload cap",
        ));
    }
    let len = u32::try_from(payload.len()).map_err(|_| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            "vsock exec payload length overflow",
        )
    })?;
    let mut header = [0u8; EXEC_FRAME_HEADER_LEN];
    header[0..4].copy_from_slice(EXEC_FRAME_MAGIC);
    header[4] = EXEC_FRAME_VERSION;
    header[5] = kind as u8;
    header[6..10].copy_from_slice(&len.to_be_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)?;
    stream.flush()
}

fn write_exec_frame_before(
    stream: &mut UnixStream,
    kind: ExecFrameKind,
    payload: &[u8],
    deadline: Instant,
) -> std::io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "exec frame too large"))?;
    let mut header = [0u8; EXEC_FRAME_HEADER_LEN];
    header[0..4].copy_from_slice(EXEC_FRAME_MAGIC);
    header[4] = EXEC_FRAME_VERSION;
    header[5] = kind as u8;
    header[6..10].copy_from_slice(&len.to_be_bytes());
    write_all_before(stream, &header, deadline)?;
    write_all_before(stream, payload, deadline)?;
    stream.flush()
}

fn write_all_before(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let now = Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "exec write deadline exceeded",
            ));
        }
        stream.set_write_timeout(Some(
            deadline.saturating_duration_since(now).min(EXEC_IO_TIMEOUT),
        ))?;
        match stream.write(bytes) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "exec stream accepted zero bytes",
                ))
            }
            Ok(written) => bytes = &bytes[written..],
            Err(error)
                if error.kind() == ErrorKind::WouldBlock
                    || error.kind() == ErrorKind::TimedOut
                    || error.kind() == ErrorKind::Interrupted =>
            {
                continue
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn read_exec_frame_before(
    stream: &mut UnixStream,
    deadline: Instant,
) -> Result<Option<ExecFrame>, String> {
    let mut header = [0u8; EXEC_FRAME_HEADER_LEN];
    if !read_exact_before(stream, &mut header, deadline)? {
        return Ok(None);
    }
    if &header[0..4] != EXEC_FRAME_MAGIC {
        return Err("bad vsock exec frame magic".into());
    }
    if header[4] != EXEC_FRAME_VERSION {
        return Err(format!(
            "unsupported vsock exec frame version {}",
            header[4]
        ));
    }
    let kind = ExecFrameKind::from_u8(header[5])
        .ok_or_else(|| format!("unknown vsock exec frame kind {}", header[5]))?;
    let len = usize::try_from(u32::from_be_bytes(header[6..10].try_into().unwrap()))
        .map_err(|_| "vsock exec frame length overflow".to_string())?;
    if len > EXEC_FRAME_MAX_PAYLOAD {
        return Err(format!("vsock exec frame payload too large: {len}"));
    }
    let mut payload = vec![0u8; len];
    if !read_exact_before(stream, &mut payload, deadline)? {
        return Ok(None);
    }
    Ok(Some(ExecFrame { kind, payload }))
}

fn read_exact_before(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> Result<bool, String> {
    while !bytes.is_empty() {
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        let remaining = deadline.saturating_duration_since(now);
        stream
            .set_read_timeout(Some(remaining.min(EXEC_IO_TIMEOUT)))
            .map_err(|error| format!("set vsock exec read timeout: {error}"))?;
        match stream.read(bytes) {
            Ok(0) => return Err("vsock exec: peer closed".into()),
            Ok(read) => bytes = &mut bytes[read..],
            Err(error)
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut =>
            {
                continue
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("vsock exec read: {error}")),
        }
    }
    Ok(true)
}

fn parse_request_only_frame(payload: &[u8]) -> Result<u64, String> {
    if payload.len() != 8 {
        return Err(format!(
            "unexpected control payload length: {}",
            payload.len()
        ));
    }
    Ok(u64::from_be_bytes(payload.try_into().unwrap()))
}

fn parse_chunk_frame(payload: &[u8]) -> Result<(u64, &[u8]), String> {
    if payload.len() < 8 {
        return Err("chunk payload too short".into());
    }
    let request_id = u64::from_be_bytes(payload[0..8].try_into().unwrap());
    let bytes = &payload[8..];
    if bytes.len() > EXEC_CHUNK_MAX_BYTES {
        return Err(format!("exec chunk too large: {}", bytes.len()));
    }
    Ok((request_id, bytes))
}

fn parse_exit_frame(payload: &[u8]) -> Result<(u64, i32), String> {
    if payload.len() != 12 {
        return Err(format!(
            "exit payload has invalid length: {}",
            payload.len()
        ));
    }
    let request_id = u64::from_be_bytes(payload[0..8].try_into().unwrap());
    let exit_code = i32::from_be_bytes(payload[8..12].try_into().unwrap());
    Ok((request_id, exit_code))
}

fn parse_error_frame(payload: &[u8]) -> Result<(u64, String), String> {
    if payload.len() < 8 {
        return Err("error payload too short".into());
    }
    let request_id = u64::from_be_bytes(payload[0..8].try_into().unwrap());
    let message = String::from_utf8_lossy(&payload[8..]).into_owned();
    Ok((request_id, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn exec_request(stream: &mut UnixStream) -> (u64, String) {
        let frame = read_exec_frame_before(stream, Instant::now() + Duration::from_secs(1))
            .expect("frame read")
            .expect("frame available");
        assert_eq!(frame.kind, ExecFrameKind::Request);
        let request_id = u64::from_be_bytes(frame.payload[0..8].try_into().unwrap());
        let len = u32::from_be_bytes(frame.payload[8..12].try_into().unwrap()) as usize;
        let command = String::from_utf8(frame.payload[12..12 + len].to_vec()).unwrap();
        (request_id, command)
    }

    fn write_start(stream: &mut UnixStream, request_id: u64) {
        write_exec_frame(stream, ExecFrameKind::Start, &request_id.to_be_bytes()).unwrap();
    }

    fn write_chunk(stream: &mut UnixStream, kind: ExecFrameKind, request_id: u64, bytes: &[u8]) {
        let mut payload = Vec::with_capacity(8 + bytes.len());
        payload.extend_from_slice(&request_id.to_be_bytes());
        payload.extend_from_slice(bytes);
        write_exec_frame(stream, kind, &payload).unwrap();
    }

    fn write_exit(stream: &mut UnixStream, request_id: u64, exit_code: i32) {
        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&request_id.to_be_bytes());
        payload.extend_from_slice(&exit_code.to_be_bytes());
        write_exec_frame(stream, ExecFrameKind::Exit, &payload).unwrap();
    }

    #[test]
    fn chunked_exec_keeps_stdout_and_stderr_separate() {
        let (mut host, guest) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut guest = guest;
            let (request_id, command) = exec_request(&mut guest);
            assert_eq!(command, "echo separated");
            write_start(&mut guest, request_id);
            write_chunk(&mut guest, ExecFrameKind::Stdout, request_id, b"out\n");
            write_chunk(&mut guest, ExecFrameKind::Stderr, request_id, b"err\n");
            write_exit(&mut guest, request_id, 7);
        });

        let outcome = run_exec_chunked_v2(
            &mut host,
            11,
            "echo separated",
            Duration::from_secs(1),
            None,
        );
        match outcome {
            RunExecOutcome::Completed((exit, stdout, stderr, _)) => {
                assert_eq!(exit, 7);
                assert_eq!(stdout, "out\n");
                assert_eq!(stderr, "err\n");
            }
            _ => panic!("unexpected outcome"),
        }
        server.join().unwrap();
    }

    #[test]
    fn chunked_exec_streams_more_than_sixteen_mib_losslessly() {
        let (mut host, guest) = UnixStream::pair().unwrap();
        let total = 17 * 1024 * 1024;
        let server = std::thread::spawn(move || {
            let mut guest = guest;
            let (request_id, _) = exec_request(&mut guest);
            write_start(&mut guest, request_id);
            let chunk = vec![b'x'; EXEC_CHUNK_MAX_BYTES];
            let mut sent = 0usize;
            while sent < total {
                let remaining = total - sent;
                let bytes = &chunk[..remaining.min(chunk.len())];
                write_chunk(&mut guest, ExecFrameKind::Stdout, request_id, bytes);
                sent += bytes.len();
            }
            write_exit(&mut guest, request_id, 0);
        });

        let outcome = run_exec_chunked_v2(&mut host, 12, "emit-big", Duration::from_secs(5), None);
        match outcome {
            RunExecOutcome::Completed((exit, stdout, stderr, _)) => {
                assert_eq!(exit, 0);
                assert_eq!(stdout.len(), total);
                assert!(stdout.bytes().all(|byte| byte == b'x'));
                assert!(stderr.is_empty());
            }
            _ => panic!("unexpected outcome"),
        }
        server.join().unwrap();
    }

    #[test]
    fn marker_exec_reports_timeout_with_partial_output() {
        let (mut host, mut guest) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut request = [0u8; 64];
            let _ = guest.read(&mut request).unwrap();
            guest.write_all(b"VMM_EXEC_START\npartial").unwrap();
            std::thread::sleep(Duration::from_millis(80));
        });

        let outcome = run_exec_marker_v1(&mut host, "sleep", Duration::from_millis(40), None);
        match outcome {
            RunExecOutcome::TimedOut {
                started,
                stdout,
                stderr,
                ..
            } => {
                assert!(started);
                assert_eq!(stdout, "partial");
                assert!(stderr.is_empty());
            }
            _ => panic!("unexpected outcome"),
        }
        server.join().unwrap();
    }

    #[test]
    fn chunked_exec_detects_disconnects() {
        let (mut host, guest) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut guest = guest;
            let _ = exec_request(&mut guest);
        });

        let outcome =
            run_exec_chunked_v2(&mut host, 13, "disconnect", Duration::from_secs(1), None);
        assert!(
            matches!(outcome, RunExecOutcome::TransportFailed(message) if message.contains("peer closed"))
        );
        server.join().unwrap();
    }

    #[test]
    fn chunked_exec_rejects_reconnect_reply_with_wrong_request_id() {
        let (mut host, guest) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut guest = guest;
            let (request_id, _) = exec_request(&mut guest);
            write_start(&mut guest, request_id + 1);
        });

        let outcome = run_exec_chunked_v2(&mut host, 14, "wrong-id", Duration::from_secs(1), None);
        assert!(
            matches!(outcome, RunExecOutcome::TransportFailed(message) if message.contains("unexpected exec start"))
        );
        server.join().unwrap();
    }
}
