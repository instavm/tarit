//! virtio-vsock host pump: a thread that owns the TX kick eventfd and moves
//! host-stream bytes into the guest RX queue.
//!
//! Guest -> host is event-driven: the TX QUEUE_NOTIFY ioeventfd wakes this
//! thread, which drains the eventfd and runs `process_tx_queue()` off the
//! seccomped vCPU thread. Host -> guest wakes through a private eventfd when the
//! controller writes an exec command; a modest poll timeout also flushes any
//! queued RX and lets the thread observe stop.
//!
//! Linux-only (the exec channel + IRQ delivery need the eventfd/KVM plumbing).

#![cfg(target_os = "linux")]

use crate::virtio::vsock::VirtioVsockMmio;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use vmm_sys_util::eventfd::EventFd;

const POLL_TIMEOUT_MS: libc::c_int = 250;

/// How long the paused pump sleeps between checks of the pause flag.
const PAUSE_POLL: std::time::Duration = std::time::Duration::from_micros(100);

/// Handle for the vsock pump thread. Dropping it stops + joins the thread.
pub struct VsockPump {
    stop: Arc<AtomicBool>,
    pause_req: Arc<AtomicBool>,
    pause_ack: Arc<AtomicBool>,
    wake_evt: EventFd,
    handle: Option<JoinHandle<()>>,
    pub device: Arc<VirtioVsockMmio>,
}

impl VsockPump {
    pub fn wake_evt(&self) -> io::Result<EventFd> {
        self.wake_evt.try_clone()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unblock a paused or polling pump so it observes stop promptly.
        self.pause_req.store(false, Ordering::SeqCst);
        let _ = self.wake_evt.write(1);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// Pause the pump and wait until it acknowledges: after this returns, the
    /// thread is parked and will not touch guest memory until
    /// [`Self::resume`]. Callers pause this thread *before* pausing the vCPU,
    /// so the handshake latency here never contributes to guest downtime.
    pub fn pause(&self) {
        if self.thread_gone() {
            return;
        }
        self.pause_req.store(true, Ordering::SeqCst);
        let _ = self.wake_evt.write(1);
        while !self.pause_ack.load(Ordering::SeqCst) {
            if self.thread_gone() {
                return;
            }
            std::thread::sleep(PAUSE_POLL);
        }
    }

    /// Release a pause. Does not wait: the thread re-enters its poll loop on
    /// its own within [`PAUSE_POLL`].
    pub fn resume(&self) {
        self.pause_req.store(false, Ordering::SeqCst);
    }

    /// A finished thread writes no guest memory, so it counts as paused.
    fn thread_gone(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }
}

impl Drop for VsockPump {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawn the pump. `tx_kick_fd` is the KVM ioeventfd for the TX queue's
/// QUEUE_NOTIFY register (datamatch=1), so guest kicks wake this thread instead
/// of trapping into the vCPU thread.
pub fn spawn_vsock_pump(device: Arc<VirtioVsockMmio>, tx_kick_fd: RawFd) -> io::Result<VsockPump> {
    let stop = Arc::new(AtomicBool::new(false));
    let pause_req = Arc::new(AtomicBool::new(false));
    let pause_ack = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let pause_req_t = pause_req.clone();
    let pause_ack_t = pause_ack.clone();
    let device_t = device.clone();
    let wake_evt = EventFd::new(libc::EFD_NONBLOCK)?;
    let wake_fd = wake_evt.as_raw_fd();

    let handle = std::thread::Builder::new()
        .name("virtio-vsock-pump".into())
        .spawn(move || {
            run(
                stop_t,
                pause_req_t,
                pause_ack_t,
                device_t,
                tx_kick_fd,
                wake_fd,
            );
        })?;

    Ok(VsockPump {
        stop,
        pause_req,
        pause_ack,
        wake_evt,
        handle: Some(handle),
        device,
    })
}

fn run(
    stop: Arc<AtomicBool>,
    pause_req: Arc<AtomicBool>,
    pause_ack: Arc<AtomicBool>,
    device: Arc<VirtioVsockMmio>,
    tx_kick_fd: RawFd,
    wake_fd: RawFd,
) {
    if let Err(e) = vmm_jailer::seccomp::SeccompProfile::vsock().install() {
        log::error!("vsock pump: seccomp install failed; refusing guest I/O: {e}");
        return;
    }

    while !stop.load(Ordering::Relaxed) {
        // Pause request: acknowledge and park. While parked this thread
        // performs no guest-memory writes — the live snapshot's final stop
        // relies on that to keep the memory image and device state coherent.
        if pause_req.load(Ordering::SeqCst) {
            pause_ack.store(true, Ordering::SeqCst);
            while pause_req.load(Ordering::SeqCst) && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(PAUSE_POLL);
            }
            pause_ack.store(false, Ordering::SeqCst);
            continue;
        }

        let mut pfds = [
            libc::pollfd {
                fd: tx_kick_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];

        // SAFETY: `pfds` is a valid writable pollfd array for the provided
        // length; poll does not retain the pointer and errors are handled.
        let n = unsafe {
            libc::poll(
                pfds.as_mut_ptr(),
                pfds.len() as libc::nfds_t,
                POLL_TIMEOUT_MS,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            log::warn!("vsock pump: poll failed: {err}");
            break;
        }

        if n == 0 {
            if let Err(error) = device.pump_host_streams() {
                log::error!("vsock pump: device failed: {error}");
                break;
            }
            continue;
        }

        if pfds[0].revents != 0 {
            drain_eventfd(tx_kick_fd, "tx_kick");
            if let Err(error) = device.process_tx_queue() {
                log::error!("vsock pump: TX device failure: {error}");
                break;
            }
        }
        if pfds[1].revents != 0 {
            drain_eventfd(wake_fd, "wake");
        }

        if let Err(error) = device.pump_host_streams() {
            log::error!("vsock pump: device failed: {error}");
            break;
        }
    }
}

fn drain_eventfd(fd: RawFd, label: &str) {
    let mut counter = [0u8; 8];
    // SAFETY: `counter` is a valid writable 8-byte eventfd counter buffer;
    // invalid or empty fds are reported by read and handled below.
    let rc = unsafe { libc::read(fd, counter.as_mut_ptr() as *mut libc::c_void, counter.len()) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if !matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) {
            log::warn!("vsock pump: {label} read failed: {err}");
        }
    }
}
