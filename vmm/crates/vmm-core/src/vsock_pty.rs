//! Host side of the interactive PTY vsock channel.
//!
//! The host initiates one virtio-vsock stream per PTY session to the guest
//! agent's PTY listener (port 1025). After sending the START stream frame this
//! module becomes a byte relay between the API UDS connection and the vsock
//! stream, waking the virtio-vsock pump after every host→guest write.

#![cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]

use crate::pty_stream::{self, PtyStart};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread::JoinHandle;
use vmm_devices::virtio::vsock::VirtioVsockMmio;
use vmm_sys_util::eventfd::EventFd;

/// Guest agent vsock port used for interactive PTY sessions.
pub const VMM_PTY_VSOCK_PORT: u32 = 1025;

/// Host endpoint for interactive PTY sessions over virtio-vsock.
pub struct VsockPtyChannel {
    device: Arc<VirtioVsockMmio>,
    pump_wake: Option<EventFd>,
}

impl VsockPtyChannel {
    /// Create a shared PTY channel bound to a virtio-vsock device.
    #[must_use]
    pub fn new(device: Arc<VirtioVsockMmio>, pump_wake: Option<EventFd>) -> Arc<Self> {
        Arc::new(Self { device, pump_wake })
    }

    pub fn attach(
        &self,
        host_stream: UnixStream,
        cols: u16,
        rows: u16,
        shell: Option<String>,
    ) -> Result<(), String> {
        let mut guest_stream = self
            .device
            .connect_guest_stream(VMM_PTY_VSOCK_PORT)
            .map_err(|e| format!("vsock pty connect: {e}"))?;

        pty_stream::write_json_frame(
            &mut guest_stream,
            pty_stream::TYPE_START,
            &PtyStart { cols, rows, shell },
        )
        .and_then(|_| guest_stream.flush())
        .map_err(|e| format!("vsock pty START write: {e}"))?;
        self.wake_pump();

        relay_bidirectional(host_stream, guest_stream, self.clone_wake())
            .map_err(|e| format!("vsock pty relay: {e}"))
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

fn relay_bidirectional(
    host_stream: UnixStream,
    guest_stream: UnixStream,
    pump_wake: Option<EventFd>,
) -> io::Result<()> {
    let host_for_guest = host_stream.try_clone()?;
    let guest_for_host = guest_stream.try_clone()?;

    let host_to_guest =
        spawn_copy_thread("pty-host-to-guest", host_for_guest, guest_stream, pump_wake)?;
    let guest_to_host = spawn_copy_thread("pty-guest-to-host", guest_for_host, host_stream, None)?;

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
