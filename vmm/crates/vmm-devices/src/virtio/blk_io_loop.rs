//! Event-driven virtio-blk queue worker.
//!
//! KVM routes queue-0 notifications to an eventfd owned by this thread. Host
//! storage latency therefore cannot stall the vCPU/MMIO dispatch path. Before
//! snapshot capture, the worker drains the queue and parks so neither device
//! state nor guest memory can change while it is serialized.

#![cfg(target_os = "linux")]

use crate::virtio::blk_transport::VirtioBlkMmio;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use vmm_sys_util::eventfd::EventFd;

const POLL_TIMEOUT_MS: libc::c_int = 100;
const PAUSE_POLL: std::time::Duration = std::time::Duration::from_micros(100);
const QUIESCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Handle for one volume's queue worker. Dropping it stops and joins the
/// worker before the eventfds and backing storage are released.
pub struct BlkIoLoop {
    stop: Arc<AtomicBool>,
    pause_req: Arc<AtomicBool>,
    pause_ack: Arc<AtomicBool>,
    wake_evt: EventFd,
    handle: Option<JoinHandle<()>>,
    pub device: Arc<VirtioBlkMmio>,
}

impl BlkIoLoop {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.pause_req.store(false, Ordering::SeqCst);
        let _ = self.wake_evt.write(1);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Drain all work published by stopped vCPUs, then park without touching
    /// guest memory or device state until resumed.
    pub fn pause(&self) -> io::Result<()> {
        if self.thread_gone() {
            self.fail_if_unexpected_exit();
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "block I/O worker exited before quiescence",
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
                    "block I/O worker exited during quiescence",
                ));
            }
            if std::time::Instant::now() >= deadline {
                // A slow backing operation can legitimately outlive the
                // snapshot quiescence budget. Abort this capture boundary,
                // but leave the healthy worker and its in-flight descriptor
                // intact so the running source can finish the request.
                self.pause_req.store(false, Ordering::SeqCst);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "block I/O worker quiescence timed out",
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
                "block I/O worker exited before resume",
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
                    "block I/O worker exited during resume",
                ));
            }
            if std::time::Instant::now() >= deadline {
                self.device
                    .fail_worker("block I/O worker resume acknowledgement timed out");
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "block I/O worker resume acknowledgement timed out",
                ));
            }
            std::thread::sleep(PAUSE_POLL);
        }
        Ok(())
    }

    fn thread_gone(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(|handle| handle.is_finished())
    }

    fn fail_if_unexpected_exit(&self) {
        if !self.stop.load(Ordering::SeqCst) {
            self.device
                .fail_worker("block I/O worker is not running during quiescence");
        }
    }
}

impl Drop for BlkIoLoop {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawn a queue worker. `kick_fd` must be the non-blocking eventfd registered
/// for queue 0 at this device's QUEUE_NOTIFY MMIO address.
pub fn spawn_blk_io_loop(device: Arc<VirtioBlkMmio>, kick_fd: RawFd) -> io::Result<BlkIoLoop> {
    // SAFETY: F_GETFD inspects the descriptor without retaining it. The
    // controller owns the descriptor for the returned worker's lifetime.
    if unsafe { libc::fcntl(kick_fd, libc::F_GETFD) } < 0 {
        let source = io::Error::last_os_error();
        return Err(io::Error::new(
            source.kind(),
            format!("block queue kick descriptor is invalid: {source}"),
        ));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let pause_req = Arc::new(AtomicBool::new(false));
    let pause_ack = Arc::new(AtomicBool::new(false));
    let wake_evt = EventFd::new(libc::EFD_NONBLOCK)?;

    let stop_t = Arc::clone(&stop);
    let pause_req_t = Arc::clone(&pause_req);
    let pause_ack_t = Arc::clone(&pause_ack);
    let device_t = Arc::clone(&device);
    let health_device = Arc::clone(&device);
    let wake_fd = wake_evt.as_raw_fd();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let handle = std::thread::Builder::new()
        .name("virtio-blk-io".into())
        .spawn(move || {
            if let Err(error) = vmm_jailer::seccomp::SeccompProfile::block().install() {
                let _ = ready_tx.send(Err(error));
                return;
            }
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            let stop_health = Arc::clone(&stop_t);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(stop_t, pause_req_t, pause_ack_t, device_t, kick_fd, wake_fd);
            }));
            if !stop_health.load(Ordering::SeqCst) {
                let context = if outcome.is_err() {
                    "block I/O worker panicked"
                } else {
                    "block I/O worker exited unexpectedly"
                };
                health_device.fail_worker(context);
            }
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = handle.join();
            return Err(io::Error::other(format!(
                "install block I/O worker sandbox: {error}"
            )));
        }
        Err(error) => {
            let _ = handle.join();
            return Err(io::Error::other(format!(
                "block I/O worker exited during startup: {error}"
            )));
        }
    }

    Ok(BlkIoLoop {
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
    device: Arc<VirtioBlkMmio>,
    kick_fd: RawFd,
    wake_fd: RawFd,
) {
    while !stop.load(Ordering::Relaxed) {
        if pause_req.load(Ordering::SeqCst) {
            // Eventfd counters are host-only state. Always inspect the queue
            // after draining the counter so a kick racing with quiescence can
            // never disappear from a restored VM.
            drain_eventfd(kick_fd, "queue kick");
            if let Err(error) = device.process_queue(0) {
                log::error!("block I/O worker: queue drain before pause failed: {error}");
                break;
            }
            pause_ack.store(true, Ordering::SeqCst);
            while pause_req.load(Ordering::SeqCst) && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(PAUSE_POLL);
            }
            pause_ack.store(false, Ordering::SeqCst);
            continue;
        }

        let mut pollfds = [
            libc::pollfd {
                fd: kick_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `pollfds` is live and writable for the duration of poll.
        let ready = unsafe {
            libc::poll(
                pollfds.as_mut_ptr(),
                pollfds.len() as libc::nfds_t,
                POLL_TIMEOUT_MS,
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("block I/O worker: poll failed: {error}");
            break;
        }
        if pollfds[1].revents != 0 {
            drain_eventfd(wake_fd, "wake");
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if ready == 0 || pollfds[0].revents != 0 {
            drain_eventfd(kick_fd, "queue kick");
            if let Err(error) = device.process_queue(0) {
                log::error!("block I/O worker: device failure: {error}");
                break;
            }
        }
    }
}

fn drain_eventfd(fd: RawFd, label: &str) {
    let mut counter = [0_u8; 8];
    // SAFETY: `counter` is a valid eventfd-sized output buffer. The fd is
    // non-blocking and remains owned by the controller for the worker lifetime.
    let result = unsafe { libc::read(fd, counter.as_mut_ptr().cast(), counter.len()) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if !matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) {
            log::warn!("block I/O worker: {label} read failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_rejects_an_invalid_queue_descriptor() {
        let device = Arc::new(VirtioBlkMmio::new_stub(5, 2));
        let error = match spawn_blk_io_loop(device, -1) {
            Ok(mut worker) => {
                worker.stop();
                panic!("invalid queue descriptor unexpectedly started a block worker")
            }
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("descriptor is invalid"));
        assert!(message.contains(&io::Error::from_raw_os_error(libc::EBADF).to_string()));
    }

    #[test]
    fn resume_waits_for_the_pause_acknowledgement_to_clear() {
        let device = Arc::new(VirtioBlkMmio::new_stub(5, 2));
        let kick = EventFd::new(libc::EFD_NONBLOCK).expect("queue kick");
        let mut worker = spawn_blk_io_loop(device, kick.as_raw_fd()).expect("start block worker");

        worker.pause().expect("pause block worker");
        assert!(worker.pause_ack.load(Ordering::SeqCst));
        worker.resume().expect("resume block worker");
        assert!(!worker.pause_ack.load(Ordering::SeqCst));

        worker.pause().expect("pause block worker again");
        assert!(worker.pause_ack.load(Ordering::SeqCst));
        worker.resume().expect("resume block worker again");
        assert!(!worker.pause_ack.load(Ordering::SeqCst));
        worker.stop();
    }
}
