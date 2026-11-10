//! Threaded vCPU run loop — runs KVM_RUN in a background thread so the
//! main thread can pause/snapshot/resume the VM, and send serial input
//! to a running guest (for real exec, like `docker exec`).

#![cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]

use crate::error::{Result, VmmError};
use crate::vcpu::VcpuId;
use kvm_ioctls::{VcpuExit, VcpuFd};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use vmm_devices::bus::MmioBus;

const SNAPSHOT_VCPU_TRANSITION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Commands the control thread sends to the vCPU thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcpuCommand {
    /// Keep running.
    Run,
    /// Pause (stop KVM_RUN, wait for Resume).
    Pause,
    /// Stop permanently (the VM is being destroyed).
    Stop,
}

/// A handle to a running vCPU thread.
/// Sets the paused+exited flags when the vCPU thread's scope ends — including on
/// a panic unwind — so the controller never spins in pause() waiting on a thread
/// that is gone. (A seccomp SIGSYS bypasses Drop; pause() has a separate
/// liveness probe for that.)
struct VcpuExitGuard {
    paused: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
}

impl Drop for VcpuExitGuard {
    fn drop(&mut self) {
        self.paused.store(true, Ordering::Relaxed);
        self.exited.store(true, Ordering::Relaxed);
    }
}

pub struct VcpuThread {
    handle: Option<JoinHandle<Result<()>>>,
    pub control: Arc<AtomicBool>,
    pub stop_flag: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    tid: Arc<AtomicI32>,
    /// Shared 16550 UART for host↔guest communication.
    pub serial: Arc<vmm_devices::serial::Serial>,
    /// Postcard-serialized `VcpuFullState`, captured by the vCPU thread the
    /// moment it enters a pause (guest stopped). The controller reads this
    /// after `pause()` to build a faithful, resumable snapshot.
    pub captured_state: Arc<Mutex<Option<Vec<u8>>>>,
}

impl VcpuThread {
    /// Spawn a vCPU thread, named after `id` (see [`VcpuId::thread_name`]),
    /// that runs KVM_RUN in a loop.
    pub fn spawn(
        id: VcpuId,
        vcpu: VcpuFd,
        mmio_bus: Arc<MmioBus>,
        serial: Arc<vmm_devices::serial::Serial>,
    ) -> Result<Self> {
        Self::spawn_with_start_gate(id, vcpu, mmio_bus, serial, None)
    }

    pub(crate) fn spawn_parked(
        id: VcpuId,
        vcpu: VcpuFd,
        mmio_bus: Arc<MmioBus>,
        serial: Arc<vmm_devices::serial::Serial>,
        start: crate::vcpu_start::VcpuStartToken,
    ) -> Result<Self> {
        Self::spawn_with_start_gate(id, vcpu, mmio_bus, serial, Some(start))
    }

    fn spawn_with_start_gate(
        id: VcpuId,
        mut vcpu: VcpuFd,
        mmio_bus: Arc<MmioBus>,
        serial: Arc<vmm_devices::serial::Serial>,
        start: Option<crate::vcpu_start::VcpuStartToken>,
    ) -> Result<Self> {
        let control = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let tid = Arc::new(AtomicI32::new(0));
        let ctrl = control.clone();
        let stop_for_thread = stop_flag.clone();
        let paus = paused.clone();
        let exited_for_thread = exited.clone();
        let tid_for_thread = tid.clone();
        let serial_for_thread = serial.clone();
        let captured_state = Arc::new(Mutex::new(None));
        let captured_for_thread = captured_state.clone();

        let handle = id.spawn_thread(move || -> Result<()> {
            // Install a no-op SIGALRM handler so the signal interrupts
            // KVM_RUN (returns EINTR) without killing the process.
            // SAFETY: sigaction is called with a valid signal number and a
            // process-local no-op handler; errors are harmless for this best-effort
            // KVM_RUN interrupt mechanism.
            unsafe {
                extern "C" fn noop_handler(_: libc::c_int) {}
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = noop_handler as *const () as usize;
                libc::sigaction(libc::SIGALRM, &sa, std::ptr::null_mut());
            }

            // Publish our TID so the controller can target us with tgkill.
            // alarm() targets the *process*, so on a multi-threaded process
            // (which we now are — there is a control thread) the signal can
            // land on the wrong thread and KVM_RUN keeps blocking. tgkill
            // directs the signal at exactly this thread.
            // SAFETY: syscall(SYS_gettid) has no preconditions and returns the
            // current Linux thread id.
            let my_tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
            tid_for_thread.store(my_tid, Ordering::SeqCst);

            // glibc may lazily open /proc files from inside malloc/free (the
            // first non-main arena heap trim reads overcommit_memory). The
            // seccomp profiles answer path opens with EACCES rather than
            // SIGSYS, so no allocator warm-up is needed here.

            // Install the seccomp filter for this dedicated vCPU
            // thread. The filter is per-thread; the controller's thread is
            // unaffected so it can still call open/snapshot/etc. The
            // sigaction above and the TID publish above must run BEFORE
            // this point — rt_sigaction is not in the vCPU allowlist.
            #[cfg(target_os = "linux")]
            {
                let profile = vmm_jailer::seccomp::SeccompProfile::vcpu();
                if let Err(e) = profile.install() {
                    log::error!(
                        "seccomp install failed — refusing to run vCPU without seccomp: {e}"
                    );
                    return Err(VmmError::Kvm(format!("seccomp install: {e}")));
                }
            }

            // A drop guard sets `paused`+`exited` when this scope ends — even on
            // a panic unwind — so the controller never spins in pause() on a
            // thread that has gone away. (A seccomp SIGSYS terminates the thread
            // without unwinding, bypassing Drop; pause() has a separate liveness
            // probe to cover that case.)
            let _exit_guard = VcpuExitGuard {
                paused: paus.clone(),
                exited: exited_for_thread.clone(),
            };
            if let Some(start) = start {
                if !start.wait(&stop_for_thread) {
                    return Ok(());
                }
            }
            let result = (|| -> Result<()> {
                let mut pending_io = false;
                loop {
                    // Stop requested — exit cleanly (no error).
                    if stop_for_thread.load(Ordering::Relaxed) {
                        log::info!("vCPU thread stopping");
                        return Ok(());
                    }
                    // Check control channel.
                    if ctrl.load(Ordering::Relaxed) {
                        if pending_io {
                            if !complete_pending_io(
                                &mut vcpu,
                                &mmio_bus,
                                &serial_for_thread,
                                &stop_for_thread,
                            )? {
                                return Ok(());
                            }
                            pending_io = false;
                        }
                        // Cancellation/stop can race completion. Never publish a
                        // capture after stop, or park after pause was withdrawn.
                        if stop_for_thread.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        if !ctrl.load(Ordering::Acquire) {
                            continue;
                        }
                        // Tell KVM that userspace deliberately paused this
                        // vCPU. A kvm-clock guest consumes the flag to avoid a
                        // false watchdog soft-lockup after a long pause. The
                        // ioctl is optional for guests using another clock
                        // source, so rejection is diagnostic rather than fatal.
                        if let Err(error) = vcpu.kvmclock_ctrl() {
                            log::debug!("KVM_KVMCLOCK_CTRL on pause unavailable: {error}");
                        }
                        // Guest is stopped here — capture the full vCPU state
                        // for a faithful snapshot. All the ioctls (KVM_GET_*),
                        // the futex (Mutex), and the allocation (mmap/brk) used
                        // below are in the vCPU seccomp allow-list, so this is
                        // safe under the installed filter.
                        match crate::vcpu_setup::capture_vcpu_full_state(&vcpu) {
                            Ok(st) => match postcard::to_allocvec(&st) {
                                Ok(bytes) => {
                                    *captured_for_thread
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner()) = Some(bytes);
                                }
                                Err(e) => log::warn!("serialize vCPU state: {e}"),
                            },
                            Err(e) => log::warn!("capture vCPU state: {e}"),
                        }
                        paus.store(true, Ordering::Release);
                        log::info!("vCPU thread pausing");
                        // Wait for control to clear (resume) or for stop.
                        // Adaptive backoff: a live-snapshot final stop resumes
                        // within microseconds, so spin briefly and poll at µs
                        // granularity first — a coarse sleep here would become
                        // guest downtime. Long pauses (suspend, full snapshot)
                        // back off to 10ms ticks after the first 5ms.
                        let park_start = std::time::Instant::now();
                        while ctrl.load(Ordering::Relaxed)
                            && !stop_for_thread.load(Ordering::Relaxed)
                        {
                            let parked = park_start.elapsed();
                            if parked < std::time::Duration::from_micros(200) {
                                std::hint::spin_loop();
                            } else if parked < std::time::Duration::from_millis(5) {
                                thread::sleep(std::time::Duration::from_micros(10));
                            } else {
                                thread::sleep(std::time::Duration::from_millis(10));
                            }
                        }
                        if stop_for_thread.load(Ordering::Relaxed) {
                            log::info!("vCPU thread stopping (from pause)");
                            return Ok(());
                        }
                        paus.store(false, Ordering::Relaxed);
                        log::info!("vCPU thread resuming");
                    }

                    match vcpu.run() {
                        Ok(VcpuExit::Hlt) => {
                            pending_io = false;
                            // With in-kernel IRQCHIP, HLT should be handled
                            // by KVM internally (no exit to userspace). If we
                            // DO get a HLT exit, the kernel is idle — yield
                            // to let other threads (serial, net) run.
                            std::thread::yield_now();
                        }
                        Ok(exit) => {
                            handle_io_exit(exit, &mmio_bus, &serial_for_thread)?;
                            pending_io = true;
                        }
                        Err(e) => {
                            // EINTR = a control-thread tgkill(SIGALRM) fired
                            // to interrupt this blocking KVM_RUN so we re-check
                            // the control channel. EAGAIN can be returned by KVM
                            // while an AP vCPU progresses through its INIT/SIPI
                            // bringup state machine — it is retryable, not
                            // fatal. Anything else is fatal.
                            let errno = e.errno();
                            // KVM completes a previous I/O operation before
                            // checking signals. EAGAIN makes no such promise.
                            if errno == libc::EINTR {
                                pending_io = false;
                            }
                            if errno != libc::EINTR && errno != libc::EAGAIN {
                                return Err(VmmError::Kvm(format!("KVM_RUN: {e}")));
                            }
                        }
                    }
                }
            })();
            result
        });
        let handle =
            handle.map_err(|e| VmmError::Kvm(format!("spawn {}: {e}", id.thread_name())))?;

        Ok(Self {
            handle: Some(handle),
            control,
            stop_flag,
            paused,
            exited,
            tid,
            serial,
            captured_state,
        })
    }

    /// True once the vCPU run loop has exited (clean stop, guest crash, etc.).
    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }

    /// Send SIGALRM directly to the vCPU thread (via tgkill) to interrupt
    /// a blocking KVM_RUN. No-op if the TID hasn't been published yet.
    fn signal_vcpu(&self) {
        let tid = self.tid.load(Ordering::SeqCst);
        if tid == 0 {
            return;
        }
        // SAFETY: getpid has no preconditions and cannot invalidate memory.
        let pid = unsafe { libc::getpid() };
        // SAFETY: tgkill is a thin syscall wrapper; SIGALRM is handled by the
        // vCPU thread's no-op handler (installed on entry to the thread).
        unsafe {
            libc::syscall(libc::SYS_tgkill, pid, tid, libc::SIGALRM);
        }
    }

    /// True if the vCPU thread is still alive, probed with a zero signal
    /// (existence check only). Returns true if the TID isn't published yet
    /// (the thread is still starting up).
    fn vcpu_alive(&self) -> bool {
        let tid = self.tid.load(Ordering::SeqCst);
        if tid == 0 {
            return true;
        }
        // SAFETY: getpid has no preconditions and cannot invalidate memory.
        let pid = unsafe { libc::getpid() };
        // SAFETY: tgkill with signal 0 performs no delivery; it only checks
        // whether the target thread exists (returns 0) or not (-1/ESRCH).
        let r = unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, 0) };
        r == 0
    }

    /// Request the vCPU to pause. Blocks until the vCPU is actually paused
    /// — or returns immediately if the run loop has already exited.
    ///
    /// The wait is µs-granular: live snapshots measure their guest blackout
    /// across this handshake, so a coarse poll here would put a floor under
    /// the achievable downtime. Spin first, then sleep in 10µs steps, and
    /// re-kick/liveness-probe the vCPU only every ~200µs.
    pub fn pause(&self) {
        if self.exited.load(Ordering::Relaxed) {
            return;
        }
        self.control.store(true, Ordering::Relaxed);
        self.signal_vcpu();
        let start = std::time::Instant::now();
        let mut next_kick = std::time::Duration::from_micros(200);
        while !self.paused.load(Ordering::Relaxed) && !self.exited.load(Ordering::Relaxed) {
            let elapsed = start.elapsed();
            if elapsed >= next_kick {
                // If the vCPU thread died abruptly (a seccomp SIGSYS terminates
                // it without running the exit guard), it can never set
                // paused/exited. A zero-signal liveness probe lets pause() —
                // and therefore snapshot()/stop() — abort instead of spinning
                // forever. The kick is re-sent because KVM may re-enter the
                // guest before we observe `paused`.
                if !self.vcpu_alive() {
                    log::warn!("vCPU thread is gone; aborting pause (guest already dead)");
                    self.exited.store(true, Ordering::Relaxed);
                    self.paused.store(true, Ordering::Relaxed);
                    return;
                }
                self.signal_vcpu();
                next_kick += std::time::Duration::from_micros(200);
            }
            if elapsed < std::time::Duration::from_micros(100) {
                std::hint::spin_loop();
            } else {
                thread::sleep(std::time::Duration::from_micros(10));
            }
        }
    }

    /// Arm this vCPU for a coherent multi-vCPU snapshot pause without waiting.
    ///
    /// Call this for every vCPU before waiting on any one of them. That keeps
    /// the inter-vCPU stop skew to the KVM exit/acknowledgement interval instead
    /// of allowing later vCPUs to run while the controller waits on the first.
    pub(crate) fn request_snapshot_pause(&self) -> Result<()> {
        if self.exited.load(Ordering::Relaxed) || !self.vcpu_alive() {
            return Err(VmmError::Snapshot(
                "vCPU exited before snapshot pause".into(),
            ));
        }
        *self
            .captured_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.control.store(true, Ordering::Release);
        self.signal_vcpu();
        Ok(())
    }

    /// Wait for a previously armed snapshot pause and require newly captured
    /// state. A timeout or dead thread fails the snapshot instead of allowing a
    /// stale capture from an earlier pause to be published.
    pub(crate) fn wait_snapshot_paused(&self) -> Result<()> {
        let start = std::time::Instant::now();
        let mut next_kick = std::time::Duration::from_micros(200);
        while !self.paused.load(Ordering::Acquire) {
            if self.exited.load(Ordering::Relaxed) || !self.vcpu_alive() {
                self.exited.store(true, Ordering::Relaxed);
                return Err(VmmError::Snapshot(
                    "vCPU exited during snapshot pause".into(),
                ));
            }
            let elapsed = start.elapsed();
            if elapsed >= SNAPSHOT_VCPU_TRANSITION_TIMEOUT {
                return Err(VmmError::Snapshot(format!(
                    "vCPU snapshot pause timed out after {:?}",
                    SNAPSHOT_VCPU_TRANSITION_TIMEOUT
                )));
            }
            if elapsed >= next_kick {
                self.signal_vcpu();
                next_kick += std::time::Duration::from_micros(200);
            }
            if elapsed < std::time::Duration::from_micros(100) {
                std::hint::spin_loop();
            } else {
                thread::sleep(std::time::Duration::from_micros(10));
            }
        }
        if self.exited.load(Ordering::Relaxed) {
            return Err(VmmError::Snapshot(
                "vCPU exited while acknowledging snapshot pause".into(),
            ));
        }
        if self
            .captured_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none()
        {
            return Err(VmmError::Snapshot(
                "vCPU snapshot pause did not capture state".into(),
            ));
        }
        Ok(())
    }

    /// Resume the vCPU from a pause.
    pub fn resume(&self) {
        self.control.store(false, Ordering::Relaxed);
    }

    /// Block until the vCPU thread has left its pause park (or exited). Used
    /// by callers that measure guest blackout: `resume()` only requests the
    /// resume, this observes the vCPU actually committing to re-enter the
    /// guest.
    pub fn wait_resumed(&self) {
        let start = std::time::Instant::now();
        let mut next_probe = std::time::Duration::from_micros(200);
        while self.paused.load(Ordering::Relaxed) && !self.exited.load(Ordering::Relaxed) {
            let elapsed = start.elapsed();
            if elapsed >= next_probe {
                // Same failure mode as pause(): a signal-terminated vCPU
                // thread never clears `paused` (VcpuExitGuard doesn't run on
                // SIGSYS), so probe liveness instead of spinning forever.
                if !self.vcpu_alive() {
                    log::warn!("vCPU thread is gone; aborting wait_resumed");
                    self.exited.store(true, Ordering::Relaxed);
                    return;
                }
                next_probe += std::time::Duration::from_micros(200);
            }
            if elapsed < std::time::Duration::from_micros(100) {
                std::hint::spin_loop();
            } else {
                thread::sleep(std::time::Duration::from_micros(10));
            }
        }
    }

    /// Bounded counterpart to [`Self::wait_resumed`] for snapshot publication.
    pub(crate) fn wait_snapshot_resumed(&self) -> Result<()> {
        let start = std::time::Instant::now();
        while self.paused.load(Ordering::Acquire) {
            if self.exited.load(Ordering::Relaxed) || !self.vcpu_alive() {
                self.exited.store(true, Ordering::Relaxed);
                return Err(VmmError::Snapshot(
                    "vCPU exited while resuming from snapshot pause".into(),
                ));
            }
            if start.elapsed() >= SNAPSHOT_VCPU_TRANSITION_TIMEOUT {
                return Err(VmmError::Snapshot(format!(
                    "vCPU snapshot resume timed out after {:?}",
                    SNAPSHOT_VCPU_TRANSITION_TIMEOUT
                )));
            }
            if start.elapsed() < std::time::Duration::from_micros(100) {
                std::hint::spin_loop();
            } else {
                thread::sleep(std::time::Duration::from_micros(10));
            }
        }
        if self.exited.load(Ordering::Relaxed) {
            return Err(VmmError::Snapshot(
                "vCPU exited after snapshot pause".into(),
            ));
        }
        Ok(())
    }

    /// Stop the vCPU permanently (joins the thread).
    pub fn stop(mut self) -> Result<()> {
        // Order matters: set stop *before* control so the pause spin-loop
        // observes stop on its next tick and exits cleanly.
        self.stop_flag.store(true, Ordering::Relaxed);
        self.control.store(true, Ordering::Relaxed);
        // Wake the vCPU out of any blocking KVM_RUN so it observes the flags.
        self.signal_vcpu();
        if let Some(handle) = self.handle.take() {
            // A thread terminated by a signal (e.g. a seccomp SIGSYS) cannot be
            // joined cleanly — the std thread lifecycle panics on join. Isolate
            // the join with catch_unwind and treat an unwinding join as an
            // abnormal (signal) exit rather than letting it poison callers.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.join())) {
                Ok(Ok(inner)) => inner?,
                Ok(Err(_)) => return Err(VmmError::Kvm("vCPU thread panicked".into())),
                Err(_) => log::warn!("vCPU thread terminated abnormally (signal); detached"),
            }
        }
        Ok(())
    }
}

impl Drop for VcpuThread {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        self.control.store(true, Ordering::Relaxed);
        // Best-effort signal so a Drop in error paths doesn't hang the
        // process when the vCPU is mid-KVM_RUN.
        let tid = self.tid.load(Ordering::SeqCst);
        if tid != 0 {
            // SAFETY: getpid has no preconditions and cannot invalidate memory.
            let pid = unsafe { libc::getpid() };
            // SAFETY: tgkill targets the published vCPU TID with SIGALRM to wake
            // KVM_RUN during Drop; the result is intentionally ignored.
            unsafe {
                libc::syscall(libc::SYS_tgkill, pid, tid, libc::SIGALRM);
            }
        }
        if let Some(handle) = self.handle.take() {
            // Never panic in Drop — a panic during an unwind aborts the whole
            // process. Joining a signal-killed thread panics, so isolate it.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = handle.join();
            }));
        }
    }
}

/// Finish the instruction that caused the last userspace I/O exit before any
/// KVM_GET_* capture. KVM API §5 guarantees immediate_exit completes pending
/// operations without executing further guest instructions. Only this vCPU
/// thread accesses kvm_run; device handlers run without holding controller locks.
fn complete_pending_io(
    vcpu: &mut VcpuFd,
    bus: &MmioBus,
    serial: &vmm_devices::serial::Serial,
    stop: &AtomicBool,
) -> Result<bool> {
    vcpu.set_kvm_immediate_exit(1);
    let result = drain_pending_io(stop, || match vcpu.run() {
        Err(error) if error.errno() == libc::EINTR => Ok(true),
        Err(error) if error.errno() == libc::EAGAIN => Ok(false),
        Err(error) => Err(VmmError::Snapshot(format!(
            "complete pending vCPU I/O: {error}"
        ))),
        // One instruction may need multiple exits (e.g. split MMIO). Service
        // those with immediate_exit still set, until KVM confirms completion.
        Ok(exit) => {
            handle_io_exit(exit, bus, serial)?;
            Ok(false)
        }
    });
    vcpu.set_kvm_immediate_exit(0);
    result
}

fn drain_pending_io(stop: &AtomicBool, mut step: impl FnMut() -> Result<bool>) -> Result<bool> {
    // A broken backend must fail the snapshot rather than spin forever while
    // the controller holds a lifecycle lock. Each run has immediate_exit set.
    for _ in 0..64 {
        if stop.load(Ordering::Acquire) {
            return Ok(false);
        }
        if step()? {
            return Ok(true);
        }
        std::thread::yield_now();
    }
    Err(VmmError::Snapshot(
        "pending vCPU I/O did not complete in 64 reentries".into(),
    ))
}

fn handle_io_exit(
    exit: VcpuExit<'_>,
    bus: &MmioBus,
    serial: &vmm_devices::serial::Serial,
) -> Result<()> {
    match exit {
        VcpuExit::MmioWrite(addr, data) => {
            let mut bytes = [0; 8];
            for (dst, src) in bytes.iter_mut().zip(data.iter()) {
                *dst = *src;
            }
            let _ = bus.write(addr, u64::from_le_bytes(bytes), data.len() as u8);
        }
        VcpuExit::MmioRead(addr, data) => {
            if let Ok(value) = bus.read(addr, data.len() as u8) {
                for (dst, src) in data.iter_mut().zip(value.to_le_bytes()) {
                    *dst = src;
                }
            }
        }
        VcpuExit::IoOut(port, data) => {
            if (0x3f8..=0x3ff).contains(&port) {
                serial.write((port - 0x3f8) as u8, data[0]);
            }
        }
        VcpuExit::IoIn(port, data) => {
            if (0x3f8..=0x3ff).contains(&port) {
                data[0] = serial.read((port - 0x3f8) as u8);
            }
        }
        other => return Err(VmmError::Kvm(format!("unhandled exit: {other:?}"))),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_retries_abort_on_fault_and_observe_stop_before_reentry() {
        let stop = AtomicBool::new(false);
        let mut attempts = 0;
        assert!(drain_pending_io(&stop, || {
            attempts += 1;
            Ok(attempts == 3)
        })
        .unwrap());
        assert_eq!(attempts, 3);
        assert!(drain_pending_io(&stop, || Err(VmmError::Snapshot(
            "injected completion fault".into()
        )))
        .is_err());
        assert!(!drain_pending_io(&stop, || {
            stop.store(true, Ordering::Release);
            Ok(false)
        })
        .unwrap());
        assert!(!drain_pending_io(&stop, || panic!("stop must precede reentry")).unwrap());
    }

    #[test]
    fn completion_retry_budget_is_bounded() {
        let stop = AtomicBool::new(false);
        let mut attempts = 0;
        assert!(drain_pending_io(&stop, || {
            attempts += 1;
            Ok(false)
        })
        .is_err());
        assert_eq!(attempts, 64);
    }

    // Requires real KVM: explicitly ignored by ordinary unit runs rather than
    // silently counting an unavailable /dev/kvm as passing execution evidence.
    #[test]
    #[ignore = "requires /dev/kvm; run explicitly on the KVM fixture host"]
    fn kvm_pending_pio_mmio_complete_without_executing_next_instruction() {
        let kvm = kvm_ioctls::Kvm::new().unwrap();
        assert!(kvm.check_extension(kvm_ioctls::Cap::ImmediateExit));
        for code in [
            vec![0xe4, 0x80],                // in al, 0x80
            vec![0xe6, 0x80],                // out 0x80, al
            vec![0x67, 0xa0, 0, 0, 0, 0xf0], // mov al, byte [0xf0000000]
            vec![0x67, 0xa2, 0, 0, 0, 0xf0], // mov byte [0xf0000000], al
        ] {
            let vm = kvm.create_vm().unwrap();
            let mem = vmm_memory_backend::GuestMemory::new(0x4000).unwrap();
            mem.register(&vm, 0).unwrap();
            let mut program = code.clone();
            program.extend_from_slice(&[0xb0, 0xcc, 0xf4]); // mov al,0xcc; hlt
            mem.write_phys(0x1000, &program).unwrap();
            let mut vcpu = vm.create_vcpu(0).unwrap();
            let mut sregs = vcpu.get_sregs().unwrap();
            sregs.cs.base = 0;
            sregs.cs.selector = 0;
            vcpu.set_sregs(&sregs).unwrap();
            let mut regs = vcpu.get_regs().unwrap();
            regs.rip = 0x1000;
            regs.rflags = 2;
            regs.rax = 0x5a;
            vcpu.set_regs(&regs).unwrap();
            match vcpu.run().unwrap() {
                VcpuExit::IoIn(_, data) | VcpuExit::MmioRead(_, data) => data[0] = 0x5a,
                VcpuExit::IoOut(_, _) | VcpuExit::MmioWrite(_, _) => {}
                other => panic!("expected I/O exit: {other:?}"),
            }
            let bus = MmioBus::new();
            let serial = vmm_devices::serial::Serial::new(
                vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap(),
            );
            assert!(
                complete_pending_io(&mut vcpu, &bus, &serial, &AtomicBool::new(false)).unwrap()
            );
            let regs = vcpu.get_regs().unwrap();
            assert_eq!(regs.rip, 0x1000 + code.len() as u64);
            assert_eq!(regs.rax & 0xff, 0x5a, "next mov must not execute");
            assert_eq!(vcpu.get_kvm_run().immediate_exit, 0);
            assert!(matches!(vcpu.run().unwrap(), VcpuExit::Hlt));
            assert_eq!(vcpu.get_regs().unwrap().rax & 0xff, 0xcc);
        }
    }
}
