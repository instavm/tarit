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
const QUIESCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// thread has drained guest TX and host RX once, then parked without
    /// touching guest memory until [`Self::resume`]. Callers must pause every
    /// vCPU first so the guest cannot publish another descriptor after this
    /// drain.
    pub fn pause(&self) -> io::Result<()> {
        if self.thread_gone() {
            self.fail_if_unexpected_exit();
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "vsock worker exited before quiescence",
            ));
        }
        self.pause_req.store(true, Ordering::SeqCst);
        self.wake_evt.write(1)?;
        let deadline = std::time::Instant::now() + QUIESCE_TIMEOUT;
        while !self.pause_ack.load(Ordering::SeqCst) {
            if self.thread_gone() {
                self.fail_if_unexpected_exit();
                self.pause_req.store(false, Ordering::SeqCst);
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "vsock worker exited during quiescence",
                ));
            }
            if std::time::Instant::now() >= deadline {
                self.pause_req.store(false, Ordering::SeqCst);
                self.device.fail_worker("vsock worker quiescence timed out");
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "vsock worker quiescence timed out",
                ));
            }
            std::thread::sleep(PAUSE_POLL);
        }
        Ok(())
    }

    /// Release a pause and wait until the worker has left its parked state.
    /// This acknowledgement prevents a rapid resume/pause cycle from
    /// mistaking the previous pause acknowledgement for the new request.
    pub fn resume(&self) -> io::Result<()> {
        if self.thread_gone() {
            self.fail_if_unexpected_exit();
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "vsock worker exited before resume",
            ));
        }
        self.pause_req.store(false, Ordering::SeqCst);
        self.wake_evt.write(1)?;
        let deadline = std::time::Instant::now() + QUIESCE_TIMEOUT;
        while self.pause_ack.load(Ordering::SeqCst) {
            if self.thread_gone() {
                self.fail_if_unexpected_exit();
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "vsock worker exited during resume",
                ));
            }
            if std::time::Instant::now() >= deadline {
                self.device
                    .fail_worker("vsock worker resume acknowledgement timed out");
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "vsock worker resume acknowledgement timed out",
                ));
            }
            std::thread::sleep(PAUSE_POLL);
        }
        Ok(())
    }

    fn thread_gone(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    fn fail_if_unexpected_exit(&self) {
        if !self.stop.load(Ordering::SeqCst) {
            self.device
                .fail_worker("vsock worker is not running during quiescence");
        }
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
    // SAFETY: F_GETFD inspects the descriptor without retaining it. The caller
    // owns the descriptor for the lifetime of the returned worker.
    if unsafe { libc::fcntl(tx_kick_fd, libc::F_GETFD) } < 0 {
        let source = io::Error::last_os_error();
        return Err(io::Error::new(
            source.kind(),
            format!("vsock queue kick descriptor is invalid: {source}"),
        ));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let pause_req = Arc::new(AtomicBool::new(false));
    let pause_ack = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let pause_req_t = pause_req.clone();
    let pause_ack_t = pause_ack.clone();
    let device_t = device.clone();
    let health_device = device.clone();
    let wake_evt = EventFd::new(libc::EFD_NONBLOCK)?;
    let wake_fd = wake_evt.as_raw_fd();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    let handle = std::thread::Builder::new()
        .name("virtio-vsock-pump".into())
        .spawn(move || {
            let stop_health = Arc::clone(&stop_t);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(
                    stop_t,
                    pause_req_t,
                    pause_ack_t,
                    device_t,
                    tx_kick_fd,
                    wake_fd,
                    ready_tx,
                );
            }));
            if !stop_health.load(Ordering::SeqCst) {
                let context = if outcome.is_err() {
                    "vsock worker panicked"
                } else {
                    "vsock worker exited unexpectedly"
                };
                health_device.fail_worker(context);
            }
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = handle.join();
            return Err(error);
        }
        Err(error) => {
            let _ = handle.join();
            return Err(io::Error::other(format!(
                "vsock worker exited during startup: {error}"
            )));
        }
    }

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
    ready_tx: std::sync::mpsc::SyncSender<io::Result<()>>,
) {
    if let Err(e) = vmm_jailer::seccomp::SeccompProfile::vsock().install() {
        let _ = ready_tx.send(Err(io::Error::other(format!(
            "install vsock worker sandbox: {e}"
        ))));
        return;
    }
    if ready_tx.send(Ok(())).is_err() {
        return;
    }

    while !stop.load(Ordering::Relaxed) {
        // Pause request: acknowledge and park. While parked this thread
        // performs no guest-memory writes — the live snapshot's final stop
        // relies on that to keep the memory image and device state coherent.
        if pause_req.load(Ordering::SeqCst) {
            // Eventfd counters are host state and are not serialized. Drain the
            // TX queue even when the counter is empty so a descriptor whose
            // kick raced with the pause cannot become permanently stranded in
            // a restored VM. vCPUs are already paused by the caller.
            drain_eventfd(tx_kick_fd, "tx_kick");
            if let Err(error) = device.process_tx_queue() {
                log::error!("vsock pump: TX drain before pause failed: {error}");
                break;
            }
            if let Err(error) = device.pump_host_streams() {
                log::error!("vsock pump: RX drain before pause failed: {error}");
                break;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_rejects_an_invalid_queue_descriptor() {
        let device = Arc::new(VirtioVsockMmio::new(7, 3));
        let error = match spawn_vsock_pump(device, -1) {
            Ok(mut worker) => {
                worker.stop();
                panic!("invalid queue descriptor unexpectedly started a vsock worker")
            }
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("descriptor is invalid"));
        assert!(message.contains(&io::Error::from_raw_os_error(libc::EBADF).to_string()));
    }

    #[test]
    fn resume_waits_for_the_pause_acknowledgement_to_clear() {
        let device = Arc::new(VirtioVsockMmio::new(7, 3));
        let kick = EventFd::new(libc::EFD_NONBLOCK).expect("queue kick");
        let mut worker = spawn_vsock_pump(device, kick.as_raw_fd()).expect("start vsock worker");

        worker.pause().expect("pause vsock worker");
        assert!(worker.pause_ack.load(Ordering::SeqCst));
        worker.resume().expect("resume vsock worker");
        assert!(!worker.pause_ack.load(Ordering::SeqCst));

        worker.pause().expect("pause vsock worker again");
        assert!(worker.pause_ack.load(Ordering::SeqCst));
        worker.resume().expect("resume vsock worker again");
        assert!(!worker.pause_ack.load(Ordering::SeqCst));
        worker.stop();
    }
}
