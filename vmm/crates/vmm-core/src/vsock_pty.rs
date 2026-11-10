//! Host side of the interactive PTY vsock channel.
//!
//! The host initiates one virtio-vsock stream per PTY session to the guest
//! agent's PTY listener (port 1025). After sending the START stream frame this
//! module becomes a byte relay between the API UDS connection and the vsock
//! stream, waking the virtio-vsock pump after every host→guest write.

#![cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]

use crate::pty_stream::{self, PtyStart};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use vmm_devices::virtio::vsock::VirtioVsockMmio;
use vmm_sys_util::eventfd::EventFd;

/// Guest agent vsock port used for interactive PTY sessions.
pub const VMM_PTY_VSOCK_PORT: u32 = 1025;

/// Host endpoint for interactive PTY sessions over virtio-vsock.
pub struct VsockPtyChannel {
    device: Arc<VirtioVsockMmio>,
    pump_wake: Option<EventFd>,
    sessions: Mutex<Sessions>,
}

#[derive(Default)]
struct Sessions {
    stopped: bool,
    next_id: u64,
    streams: HashMap<u64, (UnixStream, UnixStream)>,
}

struct SessionGuard<'a> {
    channel: &'a VsockPtyChannel,
    id: u64,
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        self.channel
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .streams
            .remove(&self.id);
    }
}

impl VsockPtyChannel {
    /// Create a shared PTY channel bound to a virtio-vsock device.
    #[must_use]
    pub fn new(device: Arc<VirtioVsockMmio>, pump_wake: Option<EventFd>) -> Arc<Self> {
        Arc::new(Self {
            device,
            pump_wake,
            sessions: Mutex::new(Sessions::default()),
        })
    }

    pub fn attach(
        &self,
        host_stream: UnixStream,
        cols: u16,
        rows: u16,
        shell: Option<String>,
    ) -> Result<(), String> {
        if shell.as_ref().is_some_and(|s| s.len() > 4096) {
            return Err("PTY shell exceeds 4096 bytes".into());
        }
        let mut guest_stream = self
            .device
            .connect_guest_stream(VMM_PTY_VSOCK_PORT)
            .map_err(|e| format!("vsock pty connect: {e}"))?;

        let _session = self.register(&host_stream, &guest_stream)?;
        self.wake_pump();
        write_start(&mut guest_stream, &PtyStart { cols, rows, shell })
            .map_err(|e| format!("vsock pty START write: {e}"))?;
        self.wake_pump();

        relay_bidirectional(host_stream, guest_stream, self.clone_wake())
            .map_err(|e| format!("vsock pty relay: {e}"))
    }

    fn register(&self, host: &UnixStream, guest: &UnixStream) -> Result<SessionGuard<'_>, String> {
        let streams = (
            host.try_clone().map_err(|e| e.to_string())?,
            guest.try_clone().map_err(|e| e.to_string())?,
        );
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if sessions.stopped {
            return Err("PTY channel stopped".into());
        }
        let id = sessions.next_id;
        sessions.next_id = sessions.next_id.wrapping_add(1);
        sessions.streams.insert(id, streams);
        Ok(SessionGuard { channel: self, id })
    }

    /// Cancel active relays before stopping the device pump. Registration and
    /// shutdown share one lock, so an attachment racing stop cannot escape.
    pub fn stop(&self) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.stopped = true;
        for (_, (host, guest)) in sessions.streams.drain() {
            let _ = host.shutdown(Shutdown::Both);
            let _ = guest.shutdown(Shutdown::Both);
        }
    }

    fn wake_pump(&self) {
        if let Some(evt) = &self.pump_wake {
            let _ = evt.write(1);
        }
    }

    fn clone_wake(&self) -> Option<EventFd> {
        self.pump_wake.as_ref().and_then(|evt| evt.try_clone().ok())
    }
}

// Bound the entire START write, including retries after short writes/EINTR.
fn write_start(stream: &mut UnixStream, start: &PtyStart) -> io::Result<()> {
    write_start_with_timeout(stream, start, Duration::from_secs(5))
}

fn write_start_with_timeout(
    stream: &mut UnixStream,
    start: &PtyStart,
    timeout: Duration,
) -> io::Result<()> {
    let body = serde_json::to_vec(start).map_err(io::Error::other)?;
    if body.len() > 32 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PTY START exceeds 32 KiB",
        ));
    }
    let mut frame = Vec::with_capacity(body.len() + 5);
    frame.push(pty_stream::TYPE_START);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    let deadline = Instant::now() + timeout;
    let result = (|| {
        let mut remaining = frame.as_slice();
        while !remaining.is_empty() {
            let timeout = deadline
                .checked_duration_since(Instant::now())
                .filter(|duration| !duration.is_zero())
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "PTY START deadline"))?;
            stream.set_write_timeout(Some(timeout))?;
            match stream.write(remaining) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => remaining = &remaining[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })();
    stream.set_write_timeout(None)?;
    result
}

fn relay_bidirectional(
    host_stream: UnixStream,
    guest_stream: UnixStream,
    pump_wake: Option<EventFd>,
) -> io::Result<()> {
    let host_for_guest = host_stream.try_clone()?;
    let guest_for_host = guest_stream.try_clone()?;

    let cancel_host = host_stream.try_clone()?;
    let cancel_guest = guest_for_host.try_clone()?;
    let host_to_guest =
        spawn_copy_thread("pty-host-to-guest", host_for_guest, guest_stream, pump_wake)?;
    let guest_to_host =
        match spawn_copy_thread("pty-guest-to-host", guest_for_host, host_stream, None) {
            Ok(thread) => thread,
            Err(error) => {
                let _ = cancel_host.shutdown(Shutdown::Both);
                let _ = cancel_guest.shutdown(Shutdown::Both);
                let _ = join_copy_thread(host_to_guest);
                return Err(error);
            }
        };

    let host_result = join_copy_thread(host_to_guest);
    let guest_result = join_copy_thread(guest_to_host);
    host_result.and(guest_result)
}

fn spawn_copy_thread(
    name: &str,
    reader: UnixStream,
    writer: UnixStream,
    wake: Option<EventFd>,
) -> io::Result<JoinHandle<io::Result<u64>>> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || copy_stream(reader, writer, wake))
}

fn join_copy_thread(handle: JoinHandle<io::Result<u64>>) -> io::Result<()> {
    match handle.join() {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(io::Error::other("PTY relay thread panicked")),
    }
}

fn copy_stream(
    mut reader: UnixStream,
    mut writer: UnixStream,
    wake: Option<EventFd>,
) -> io::Result<u64> {
    let mut total = 0u64;
    let mut buf = [0u8; 8192];
    let result = loop {
        match reader.read(&mut buf) {
            Ok(0) => break Ok(total),
            Ok(n) => {
                if let Err(e) = writer.write_all(&buf[..n]).and_then(|_| writer.flush()) {
                    if is_shutdown_error(e.kind()) {
                        break Ok(total);
                    }
                    break Err(e);
                }
                total += n as u64;
                if let Some(evt) = &wake {
                    let _ = evt.write(1);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if is_shutdown_error(e.kind()) => break Ok(total),
            Err(e) => break Err(e),
        }
    };

    let _ = reader.shutdown(Shutdown::Both);
    let _ = writer.shutdown(Shutdown::Both);
    result
}

fn is_shutdown_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_cancels_cloned_relays_and_rejects_late_registration() {
        let channel = VsockPtyChannel::new(Arc::new(VirtioVsockMmio::new(5, 3)), None);
        let (host, mut client) = UnixStream::pair().unwrap();
        let (guest, mut agent) = UnixStream::pair().unwrap();
        let session = channel.register(&host, &guest).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let relay = std::thread::spawn(move || {
            done_tx
                .send(relay_bidirectional(host, guest, None))
                .unwrap();
        });
        client.write_all(b"input").unwrap();
        let mut input = [0; 5];
        agent.read_exact(&mut input).unwrap();
        assert_eq!(&input, b"input");
        channel.stop();
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        relay.join().unwrap();
        drop(session);
        let (host, _) = UnixStream::pair().unwrap();
        let (guest, _) = UnixStream::pair().unwrap();
        assert!(channel.register(&host, &guest).is_err());
        assert!(channel.sessions.lock().unwrap().streams.is_empty());
    }

    #[test]
    fn start_write_deadline_unblocks_a_stalled_guest() {
        let (mut writer, _reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let fill = [0; 8192];
        loop {
            match writer.write(&fill) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("fill send buffer: {e}"),
            }
        }
        writer.set_nonblocking(false).unwrap();
        let started = Instant::now();
        let error = write_start_with_timeout(
            &mut writer,
            &PtyStart {
                cols: 80,
                rows: 24,
                shell: None,
            },
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(writer.write_timeout().unwrap(), None);
    }

    #[test]
    fn start_frame_is_bounded_and_clears_its_handshake_timeout() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        write_start(
            &mut writer,
            &PtyStart {
                cols: 80,
                rows: 24,
                shell: Some("sh".into()),
            },
        )
        .unwrap();
        assert_eq!(writer.write_timeout().unwrap(), None);
        let mut header = [0; 5];
        reader.read_exact(&mut header).unwrap();
        assert_eq!(header[0], pty_stream::TYPE_START);
        let mut body = vec![0; u32::from_be_bytes(header[1..].try_into().unwrap()) as usize];
        reader.read_exact(&mut body).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["shell"],
            "sh"
        );
        assert!(write_start(
            &mut writer,
            &PtyStart {
                cols: 80,
                rows: 24,
                shell: Some("x".repeat(32768))
            }
        )
        .is_err());
    }
}
