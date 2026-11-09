//! Event-driven virtio-blk queue worker.
//!
//! KVM routes queue-0 notifications to an eventfd owned by this thread. Host
//! storage latency therefore cannot stall the vCPU/MMIO dispatch path. Before
//! snapshot capture, the worker finishes its current bounded pass and parks.
//! Queued requests remain in guest memory with snapshot-persisted cursors;
//! resume and startup inspect them even without another guest notification.
//!
//! A guest FLUSH is served between passes, holding no device state, and can
//! take seconds on a busy NBD-backed disk. Pause does not wait for it: the
//! FLUSH stays unconsumed on the avail ring until the flush has finished and
//! the worker is running again, so a snapshot captures it as pending and the
//! restored VM serves it again.

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
    /// Set while the worker waits on a host flush (see the module docs). It
    /// touches no device state or guest memory then, and checks for a pause
    /// before it does again, so it counts as parked.
    in_host_flush: Arc<AtomicBool>,
    wake_evt: EventFd,
    handle: Option<JoinHandle<()>>,
    startup: Option<super::worker_startup::PendingStartup>,
    pub device: Arc<VirtioBlkMmio>,
}

impl BlkIoLoop {
    /// Wait for a worker from [`start_blk_io_loop`] to report that its
    /// sandbox is installed. Required before the guest runs.
    pub fn wait_ready(&mut self) -> io::Result<()> {
        match self.startup.take() {
            Some(startup) => startup.wait(&mut self.handle),
            None => Ok(()),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.pause_req.store(false, Ordering::SeqCst);
        let _ = self.wake_evt.write(1);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Finish the current bounded pass, then park without touching guest
    /// memory or device state. Unconsumed descriptors remain snapshot-visible.
    /// A host flush in progress does not delay it. A single blocking host read
    /// or write can still exceed the timeout.
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
        // Both flags are SeqCst, as are the worker's clear of `in_host_flush`
        // and its next read of `pause_req`: if this sees the flush still in
        // progress, the worker sees this pause when the flush ends.
        while !self.pause_ack.load(Ordering::SeqCst) && !self.in_host_flush.load(Ordering::SeqCst) {
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
    let mut worker = start_blk_io_loop(device, kick_fd)?;
    worker.wait_ready()?;
    Ok(worker)
}

/// [`spawn_blk_io_loop`] without waiting for the worker's startup report;
/// call [`BlkIoLoop::wait_ready`] before the guest runs.
pub fn start_blk_io_loop(device: Arc<VirtioBlkMmio>, kick_fd: RawFd) -> io::Result<BlkIoLoop> {
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
    let in_host_flush = Arc::new(AtomicBool::new(false));
    let wake_evt = EventFd::new(libc::EFD_NONBLOCK)?;

    let stop_t = Arc::clone(&stop);
    let pause_req_t = Arc::clone(&pause_req);
    let pause_ack_t = Arc::clone(&pause_ack);
    let in_host_flush_t = Arc::clone(&in_host_flush);
    let device_t = Arc::clone(&device);
    let health_device = Arc::clone(&device);
    let wake_fd = wake_evt.as_raw_fd();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let handle = std::thread::Builder::new()
        .name("virtio-blk-io".into())
        .spawn(move || {
            if let Err(error) = vmm_jailer::seccomp::SeccompProfile::block().install() {
                let _ = ready_tx.send(Err(io::Error::other(format!(
                    "install block I/O worker sandbox: {error}"
                ))));
                return;
            }
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            let stop_health = Arc::clone(&stop_t);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(
                    stop_t,
                    pause_req_t,
                    pause_ack_t,
                    in_host_flush_t,
                    device_t,
                    kick_fd,
                    wake_fd,
                );
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

    Ok(BlkIoLoop {
        stop,
        pause_req,
        pause_ack,
        in_host_flush,
        wake_evt,
        handle: Some(handle),
        startup: Some(super::worker_startup::PendingStartup::new(
            ready_rx,
            "block I/O worker",
        )),
        device,
    })
}

fn run(
    stop: Arc<AtomicBool>,
    pause_req: Arc<AtomicBool>,
    pause_ack: Arc<AtomicBool>,
    in_host_flush: Arc<AtomicBool>,
    device: Arc<VirtioBlkMmio>,
    kick_fd: RawFd,
    wake_fd: RawFd,
) {
    serve_queue(
        stop,
        pause_req,
        pause_ack,
        &in_host_flush,
        kick_fd,
        wake_fd,
        || device.process_queue_pass(),
        || device.host_flush_wanted(),
        || device.service_host_flush(),
    );
}

/// [`run_queue`] for passes that may stop at a guest FLUSH. The host flush
/// runs between passes with `in_host_flush` set, so a pause need not wait
/// for it. Once it returns the worker reaches its pause check before the
/// next pass, which completes the FLUSH.
#[allow(clippy::too_many_arguments)]
fn serve_queue<P, W, F>(
    stop: Arc<AtomicBool>,
    pause_req: Arc<AtomicBool>,
    pause_ack: Arc<AtomicBool>,
    in_host_flush: &AtomicBool,
    kick_fd: RawFd,
    wake_fd: RawFd,
    mut pass: P,
    mut flush_wanted: W,
    mut host_flush: F,
) where
    P: FnMut() -> Result<bool, crate::bus::MmioError>,
    W: FnMut() -> bool,
    F: FnMut() -> Result<(), crate::bus::MmioError>,
{
    run_queue(stop, pause_req, pause_ack, kick_fd, wake_fd, || {
        let more = pass()?;
        if !flush_wanted() {
            return Ok(more);
        }
        in_host_flush.store(true, Ordering::SeqCst);
        let flushed = host_flush();
        in_host_flush.store(false, Ordering::SeqCst);
        flushed.map(|()| true)
    });
}

fn run_queue<F>(
    stop: Arc<AtomicBool>,
    pause_req: Arc<AtomicBool>,
    pause_ack: Arc<AtomicBool>,
    kick_fd: RawFd,
    wake_fd: RawFd,
    mut process: F,
) where
    F: FnMut() -> Result<bool, crate::bus::MmioError>,
{
    // Eventfd counters are host-only state and are not in a snapshot. Always
    // inspect the queue on startup, after resume, and after a budget yield.
    let mut pending = true;
    while !stop.load(Ordering::Relaxed) {
        if pause_req.load(Ordering::SeqCst) {
            pause_ack.store(true, Ordering::SeqCst);
            while pause_req.load(Ordering::SeqCst) && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(PAUSE_POLL);
            }
            pending = true;
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
                if pending { 0 } else { POLL_TIMEOUT_MS },
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
        if pause_req.load(Ordering::SeqCst) {
            continue;
        }
        if pending || ready == 0 || pollfds[0].revents != 0 {
            drain_eventfd(kick_fd, "queue kick");
            match process() {
                Ok(more) => pending = more,
                Err(error) => {
                    log::error!("block I/O worker: device failure: {error}");
                    break;
                }
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
    fn continuous_refill_yields_to_pause_and_resumes_without_a_new_kick() {
        use super::super::vqueue::{
            AvailRing, Descriptor, QueueConfig, QueueWorkBudget, VirtQueueProcessor,
        };
        use std::sync::atomic::AtomicUsize;
        use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

        let mem = Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10_000)]).unwrap());
        for slot in 0..8u64 {
            mem.write_obj(
                Descriptor {
                    addr: 0x4000,
                    len: 1,
                    flags: 0,
                    next: 0,
                },
                GuestAddress(0x1000 + slot * 16),
            )
            .unwrap();
            mem.write_obj(slot as u16, GuestAddress(0x2000 + 4 + slot * 2))
                .unwrap();
        }
        mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(0x2000))
            .unwrap();
        let mut queue = VirtQueueProcessor::new(QueueConfig {
            size: 8,
            desc_table_addr: 0x1000,
            avail_ring_addr: 0x2000,
            used_ring_addr: 0x3000,
            ready: true,
        });
        queue.set_event_idx(true);
        let kick = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let wake_evt = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let kick_fd = kick.as_raw_fd();
        let wake_fd = wake_evt.as_raw_fd();
        let stop = Arc::new(AtomicBool::new(false));
        let pause_req = Arc::new(AtomicBool::new(false));
        let pause_ack = Arc::new(AtomicBool::new(false));
        let refill = Arc::new(AtomicBool::new(true));
        let completed = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (resumed_tx, resumed_rx) = std::sync::mpsc::channel();
        let handle = {
            let (stop, pause_req, pause_ack, refill, completed, mem) = (
                stop.clone(),
                pause_req.clone(),
                pause_ack.clone(),
                refill.clone(),
                completed.clone(),
                mem.clone(),
            );
            std::thread::spawn(move || {
                let stop_during_io = stop.clone();
                let pause_during_io = pause_req.clone();
                let mut published = 1u16;
                run_queue(stop, pause_req, pause_ack, kick_fd, wake_fd, || {
                    let result = queue.process_queue_chains_dirty(
                        &mem,
                        None,
                        Some(QueueWorkBudget {
                            max_chains: 32,
                            max_bytes: u64::MAX,
                            max_duration: std::time::Duration::from_secs(60),
                        }),
                        |_| {
                            if completed.load(Ordering::SeqCst) == 64 {
                                // Three passes must run with no kick. Hold a
                                // request until pause is pending, then keep the
                                // queue continuously nonempty through this pass.
                                entered_tx.send(()).unwrap();
                                while !pause_during_io.load(Ordering::SeqCst)
                                    && !stop_during_io.load(Ordering::SeqCst)
                                {
                                    std::thread::yield_now();
                                }
                            }
                            if refill.load(Ordering::SeqCst)
                                && !stop_during_io.load(Ordering::SeqCst)
                            {
                                published = published.wrapping_add(1);
                                mem.write_obj(published, GuestAddress(0x2002)).unwrap();
                            }
                            completed.fetch_add(1, Ordering::SeqCst);
                            Some((0, 0))
                        },
                    );
                    if !refill.load(Ordering::SeqCst) && result.processed > 0 {
                        resumed_tx.send(queue.used_idx()).unwrap();
                    }
                    Ok(result.budget_exhausted)
                });
            })
        };
        let mut worker = BlkIoLoop {
            stop,
            pause_req,
            pause_ack,
            in_host_flush: Arc::new(AtomicBool::new(false)),
            wake_evt,
            handle: Some(handle),
            startup: None,
            device: Arc::new(VirtioBlkMmio::new_stub(5, 2)),
        };
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("queue did not self-reschedule");
        worker
            .pause()
            .expect("continuously refilled queue must yield to pause");
        assert_eq!(completed.load(Ordering::SeqCst), 96);
        assert_eq!(mem.read_obj::<u16>(GuestAddress(0x3002)).unwrap(), 96);
        assert_eq!(mem.read_obj::<u16>(GuestAddress(0x2002)).unwrap(), 97);
        assert_eq!(
            mem.read_obj::<u16>(GuestAddress(0x3000 + 4 + 8 * 8))
                .unwrap(),
            96
        );
        refill.store(false, Ordering::SeqCst);
        worker.resume().unwrap();
        assert_eq!(
            resumed_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            97
        );
        worker.pause().unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 97);
        worker.stop();
    }

    #[test]
    fn pause_does_not_wait_for_a_host_flush_and_the_worker_parks_after_it() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::mpsc;

        let kick = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let wake_evt = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let (kick_fd, wake_fd) = (kick.as_raw_fd(), wake_evt.as_raw_fd());
        let stop = Arc::new(AtomicBool::new(false));
        let pause_req = Arc::new(AtomicBool::new(false));
        let pause_ack = Arc::new(AtomicBool::new(false));
        let in_host_flush = Arc::new(AtomicBool::new(false));
        // The first pass stops at a FLUSH; its host flush blocks until the
        // test releases it, as a flush of a busy NBD device does.
        let wanted = Arc::new(AtomicBool::new(false));
        let passes = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let handle = {
            let (stop, pause_req, pause_ack, in_host_flush, wanted, passes) = (
                stop.clone(),
                pause_req.clone(),
                pause_ack.clone(),
                in_host_flush.clone(),
                wanted.clone(),
                passes.clone(),
            );
            std::thread::spawn(move || {
                let flush_wanted = wanted.clone();
                serve_queue(
                    stop,
                    pause_req,
                    pause_ack,
                    &in_host_flush,
                    kick_fd,
                    wake_fd,
                    || {
                        if passes.fetch_add(1, Ordering::SeqCst) == 0 {
                            wanted.store(true, Ordering::SeqCst);
                        }
                        Ok(false)
                    },
                    || flush_wanted.load(Ordering::SeqCst),
                    || {
                        flush_wanted.store(false, Ordering::SeqCst);
                        entered_tx.send(()).unwrap();
                        let _ = release_rx.recv();
                        Ok(())
                    },
                );
            })
        };
        let mut worker = BlkIoLoop {
            stop,
            pause_req,
            pause_ack: pause_ack.clone(),
            in_host_flush,
            wake_evt,
            handle: Some(handle),
            startup: None,
            device: Arc::new(VirtioBlkMmio::new_stub(5, 2)),
        };
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("worker never started the host flush");
        let started = std::time::Instant::now();
        worker
            .pause()
            .expect("a host flush in progress must not block the pause");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let passes_at_pause = passes.load(Ordering::SeqCst);

        // The flush ends during the pause: the worker parks without another
        // pass, so it cannot complete the FLUSH into a snapshot being taken.
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !pause_ack.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "worker did not park");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(passes.load(Ordering::SeqCst), passes_at_pause);

        // Resumed, it completes the FLUSH without another guest kick.
        worker.resume().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while passes.load(Ordering::SeqCst) == passes_at_pause {
            assert!(std::time::Instant::now() < deadline, "no pass after resume");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        worker.stop();
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
