//! Host side of the vsock exec channel.
//!
//! The virtio-vsock device bridges the guest agent's outbound connection (guest
//! -> host CID 2, port 1024) to a per-VM Unix control socket. This module binds
//! that socket, accepts the guest's connection, and runs exec commands over it
//! using the same `VMM_EXEC:` / `VMM_EXEC_EXIT=` marker protocol as serial, but
//! on a dedicated framed stream so exec output never interleaves with the ttyS0
//! console and a connection dropped by a restore is transparently re-accepted.

#![cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]

use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use vmm_sys_util::eventfd::EventFd;

const EXEC_OUTPUT_CAP: usize = 16 * 1024 * 1024;
const EXEC_OUTPUT_TRUNCATED: &[u8] = b"\n[VMM exec output truncated]\n";
const EXEC_OUTPUT_PAYLOAD_CAP: usize = EXEC_OUTPUT_CAP - EXEC_OUTPUT_TRUNCATED.len();
const EXEC_ACC_TAIL_CAP: usize = 64 * 1024;

/// A live exec channel over vsock. Holds the accepted guest connection (if the
/// agent has dialed) and re-accepts on reconnect.
pub struct VsockExecChannel {
    stream: Arc<Mutex<Option<UnixStream>>>,
    stop: Arc<AtomicBool>,
    /// Set after a failed exec exchange: the vsock data path isn't working for
    /// this VM, so skip it and use serial (avoids a per-exec timeout + the
    /// guest agent reconnect thrash that dropping the stream would cause).
    disabled: Arc<AtomicBool>,
    pump_wake: Option<EventFd>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl VsockExecChannel {
    /// Bind `control_socket` and spawn a thread that accepts the guest agent's
    /// connection (the device connects here when the guest dials vsock) and
    /// keeps the newest stream for exec.
    pub fn bind(control_socket: &Path) -> std::io::Result<Arc<Self>> {
        Self::bind_with_pump_wake(control_socket, None)
    }

    /// Like [`Self::bind`], but also wakes the vsock pump after host→guest
    /// writes so commands do not wait for the pump's stop/RX timeout.
    pub fn bind_with_pump_wake(
        control_socket: &Path,
        pump_wake: Option<EventFd>,
    ) -> std::io::Result<Arc<Self>> {
        let _ = std::fs::remove_file(control_socket);
        let listener = UnixListener::bind(control_socket)?;
        listener.set_nonblocking(true)?;

        let stream: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let stream_t = stream.clone();
        let stop_t = stop.clone();

        // The accept thread blocks in poll() until the guest agent connects
        // (0 idle CPU) and wakes immediately on connect — no fixed sleep, so a
        // freshly-booted or freshly-restored guest is picked up with no accept
        // quantization latency. The 250ms timeout only bounds how often we
        // re-check the stop flag. The listener stays non-blocking so accept()
        // never blocks after a spurious wake.
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
                    if unsafe { libc::poll(&mut pfd, 1, 250) } <= 0 {
                        continue; // timeout or EINTR -> re-check the stop flag
                    }
                    match listener.accept() {
                        Ok((s, _)) => {
                            log::info!("vsock exec: guest agent connected");
                            // Blocking with a short read timeout for the exec loop.
                            let _ = s.set_nonblocking(false);
                            let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                            // Newest connection wins (guest re-dials after restore).
                            *stream_t.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(_) => std::thread::sleep(Duration::from_millis(50)),
                    }
                }
            })?;

        Ok(Arc::new(Self {
            stream,
            stop,
            disabled: Arc::new(AtomicBool::new(false)),
            pump_wake,
            handle: Mutex::new(Some(handle)),
        }))
    }

    /// True once the guest agent has dialed and a stream is available.
    pub fn is_connected(&self) -> bool {
        self.stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Run `command` over vsock. Returns `None` when no guest connection exists
    /// or the channel has been disabled after a prior failure (caller falls back
    /// to serial); `Some(Ok(..))` on success; `Some(Err(..))` on a failure, after
    /// which the channel disables itself so later execs go straight to serial.
    pub fn exec(
        &self,
        command: &str,
        timeout: Duration,
    ) -> Option<Result<(i32, String, u64), String>> {
        if self.disabled.load(Ordering::Relaxed) {
            return None;
        }
        let mut guard = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        let stream = guard.as_mut()?;
        let result = { run_exec(stream, command, timeout, self.pump_wake.as_ref()) };
        if result.is_err() {
            // Keep the stream (dropping it would make the agent reconnect and
            // we'd retry+fail every exec); just stop using vsock for this VM.
            self.disabled.store(true, Ordering::Relaxed);
        }
        Some(result)
    }
}

impl Drop for VsockExecChannel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
    }
}

/// Send one command and read back its output up to the `VMM_EXEC_EXIT=` marker.
fn run_exec(
    stream: &mut UnixStream,
    command: &str,
    timeout: Duration,
    pump_wake: Option<&EventFd>,
) -> Result<(i32, String, u64), String> {
    let start = Instant::now();
    let msg = format!("VMM_EXEC:{command}\n");
    stream
        .write_all(msg.as_bytes())
        .and_then(|_| stream.flush())
        .map_err(|e| format!("vsock exec write: {e}"))?;
    if let Some(evt) = pump_wake {
        let _ = evt.write(1);
    }

    let mut acc: Vec<u8> = Vec::new();
    let mut output: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut started = false;
    let mut buf = [0u8; 4096];

    while start.elapsed() < timeout {
        match stream.read(&mut buf) {
            Ok(0) => return Err("vsock exec: peer closed".into()),
            Ok(n) => acc.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("vsock exec read: {e}")),
        }
        while let Some(pos) = acc.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = acc.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let s = String::from_utf8_lossy(&line);
            if s == "VMM_AGENT_READY" {
                continue;
            }
            if s == "VMM_EXEC_START" {
                started = true;
                continue;
            }
            if let Some(code) = s.strip_prefix("VMM_EXEC_EXIT=") {
                let exit_code: i32 = code.trim().parse().unwrap_or(0);
                let output_str = finish_exec_output(output, truncated);
                return Ok((exit_code, output_str, start.elapsed().as_millis() as u64));
            }
            if started {
                append_exec_output(&mut output, &line, &mut truncated);
                append_exec_output(&mut output, b"\n", &mut truncated);
            }
        }
        trim_exec_accumulator(&mut acc, started, &mut output, &mut truncated);
    }
    Err("vsock exec: timed out".into())
}

fn append_exec_output(output: &mut Vec<u8>, bytes: &[u8], truncated: &mut bool) {
    if *truncated || bytes.is_empty() {
        return;
    }
    let remaining = EXEC_OUTPUT_PAYLOAD_CAP.saturating_sub(output.len());
    if bytes.len() <= remaining {
        output.extend_from_slice(bytes);
        return;
    }
    output.extend_from_slice(&bytes[..remaining]);
    *truncated = true;
}

fn trim_exec_accumulator(
    acc: &mut Vec<u8>,
    started: bool,
    output: &mut Vec<u8>,
    truncated: &mut bool,
) {
    if acc.len() <= EXEC_ACC_TAIL_CAP {
        return;
    }
    let drain_len = acc.len() - EXEC_ACC_TAIL_CAP;
    let drained: Vec<u8> = acc.drain(..drain_len).collect();
    if started {
        append_exec_output(output, &drained, truncated);
    }
}

fn finish_exec_output(mut output: Vec<u8>, truncated: bool) -> String {
    if truncated {
        output.extend_from_slice(EXEC_OUTPUT_TRUNCATED);
    }
    String::from_utf8_lossy(&output).to_string()
}
