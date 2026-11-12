//! Threaded vCPU run loop — runs KVM_RUN in a background thread so the
//! main thread can pause/snapshot/resume the VM, and send serial input
//! to a running guest (for real exec, like `docker exec`).

#![cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]

use crate::error::{Result, VmmError};
use crate::vcpu::{drain_pending_io, PendingIo, VcpuId, PENDING_IO_REENTRY_BUDGET};
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
        Self::spawn_with_start_gate(id, vcpu, mmio_bus, serial, None, PENDING_IO_REENTRY_BUDGET)
    }

    pub(crate) fn spawn_parked(
        id: VcpuId,
        vcpu: VcpuFd,
        mmio_bus: Arc<MmioBus>,
        serial: Arc<vmm_devices::serial::Serial>,
        start: crate::vcpu_start::VcpuStartToken,
    ) -> Result<Self> {
        Self::spawn_with_start_gate(
            id,
            vcpu,
            mmio_bus,
            serial,
            Some(start),
            PENDING_IO_REENTRY_BUDGET,
        )
    }

    fn spawn_with_start_gate(
        id: VcpuId,
        mut vcpu: VcpuFd,
        mmio_bus: Arc<MmioBus>,
        serial: Arc<vmm_devices::serial::Serial>,
        start: Option<crate::vcpu_start::VcpuStartToken>,
        pending_io_budget: usize,
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
                        let mut io_complete = true;
                        if pending_io {
                            match complete_pending_io(
                                &mut vcpu,
                                &mmio_bus,
                                &serial_for_thread,
                                &stop_for_thread,
                                pending_io_budget,
                            )? {
                                PendingIo::Completed => pending_io = false,
                                PendingIo::Stopped => return Ok(()),
                                PendingIo::Incomplete => io_complete = false,
                            }
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
                        if io_complete {
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
                        } else {
                            // The instruction is still in flight, so registers
                            // would not match memory. Publish no capture: the
                            // waiting pause fails and its rollback resumes this
                            // thread, which stays alive; KVM finishes the
                            // instruction on the next ordinary entry.
                            *captured_for_thread
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) = None;
                            log::warn!(
                                "vCPU pause: in-flight I/O did not complete within \
                                 {pending_io_budget} re-entries; this pause captures no state"
                            );
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
            crate::vcpu::wait_for_vcpu_exit(&handle, || self.vcpu_alive(), || self.signal_vcpu());
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
        // Repeated kicks below cover both a missed signal and startup before
        // the worker publishes its TID. Never detach while VM memory is live.
        if let Some(handle) = self.handle.take() {
            crate::vcpu::wait_for_vcpu_exit(&handle, || self.vcpu_alive(), || self.signal_vcpu());
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
    budget: usize,
) -> Result<PendingIo> {
    vcpu.set_kvm_immediate_exit(1);
    let result = drain_pending_io(stop, budget, || match vcpu.run() {
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
        other => {
            log::warn!("vCPU exit {other:?} — stopping");
            return Err(VmmError::Kvm(format!("unhandled exit: {other:?}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Requires real KVM: explicitly ignored by ordinary unit runs rather than
    // silently counting an unavailable /dev/kvm as passing execution evidence.
    #[test]
    #[ignore = "requires /dev/kvm; run explicitly on the KVM fixture host"]
    fn kvm_pending_pio_mmio_complete_without_executing_next_instruction() {
        let kvm = kvm_ioctls::Kvm::new().unwrap();
        assert!(kvm.check_extension(kvm_ioctls::Cap::ImmediateExit));
        for code in [
            vec![0xe4, 0x80], // in al, 0x80
            vec![0xe6, 0x80], // out 0x80, al
            // Real-mode DS has a 64 KiB limit. Use an unmapped hole above
            // our 16 KiB RAM slot, but within that architectural limit.
            vec![0xa0, 0x00, 0x80], // mov al, byte [0x8000]
            vec![0xa2, 0x00, 0x80], // mov byte [0x8000], al
        ] {
            let vm = kvm.create_vm().unwrap();
            // Match production VM setup before entering real mode on VMX.
            vm.set_tss_address(0xfffbd000).unwrap();
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
                VcpuExit::InternalError => {
                    // SAFETY: this union member is selected by INTERNAL_ERROR.
                    let internal = unsafe { vcpu.get_kvm_run().__bindgen_anon_1.internal };
                    panic!("first KVM_RUN internal error: code={code:02x?} suberror={} data={:x?} regs={:?} sregs={:?}",
                        internal.suberror, &internal.data[..(internal.ndata as usize).min(internal.data.len())],
                        vcpu.get_regs().unwrap(), vcpu.get_sregs().unwrap());
                }
                other => panic!("expected first I/O exit for {code:02x?}: {other:?}"),
            }
            let bus = MmioBus::new();
            let serial = vmm_devices::serial::Serial::new(
                vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap(),
            );
            assert_eq!(
                complete_pending_io(
                    &mut vcpu,
                    &bus,
                    &serial,
                    &AtomicBool::new(false),
                    PENDING_IO_REENTRY_BUDGET
                )
                .unwrap(),
                PendingIo::Completed
            );
            let regs = vcpu.get_regs().unwrap();
            assert_eq!(regs.rip, 0x1000 + code.len() as u64);
            assert_eq!(regs.rax & 0xff, 0x5a, "next mov must not execute");
            assert_eq!(vcpu.get_kvm_run().immediate_exit, 0);
            assert!(matches!(vcpu.run().unwrap(), VcpuExit::Hlt));
            assert_eq!(vcpu.get_regs().unwrap().rax & 0xff, 0xcc);
        }
    }

    /// 16 KiB of real-mode RAM at 0 holding `program` at 0x1000. Addresses
    /// from 0x4000 up to the 64 KiB segment limit are an MMIO hole.
    fn real_mode_vm(
        kvm: &kvm_ioctls::Kvm,
        program: &[u8],
    ) -> (kvm_ioctls::VmFd, vmm_memory_backend::GuestMemory) {
        let vm = kvm.create_vm().unwrap();
        // Match production VM setup before entering real mode on VMX.
        vm.set_tss_address(0xfffbd000).unwrap();
        let mem = vmm_memory_backend::GuestMemory::new(0x4000).unwrap();
        mem.register(&vm, 0).unwrap();
        mem.write_phys(0x1000, program).unwrap();
        (vm, mem)
    }

    fn real_mode_vcpu(vm: &kvm_ioctls::VmFd) -> VcpuFd {
        let vcpu = vm.create_vcpu(0).unwrap();
        let mut sregs = vcpu.get_sregs().unwrap();
        sregs.cs.base = 0;
        sregs.cs.selector = 0;
        vcpu.set_sregs(&sregs).unwrap();
        let mut regs = vcpu.get_regs().unwrap();
        regs.rip = 0x1000;
        regs.rflags = 2;
        vcpu.set_regs(&regs).unwrap();
        vcpu
    }

    fn test_serial() -> Arc<vmm_devices::serial::Serial> {
        Arc::new(vmm_devices::serial::Serial::new(
            vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap(),
        ))
    }

    /// Records warnings logged on the calling thread while `f` runs. Records
    /// from other threads are dropped, so parallel tests do not interfere.
    fn capture_warnings<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
        struct ThreadWarnings;
        std::thread_local! {
            static WARNINGS: std::cell::RefCell<Option<Vec<String>>> =
                const { std::cell::RefCell::new(None) };
        }
        impl log::Log for ThreadWarnings {
            fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
                metadata.level() <= log::Level::Warn
            }
            fn log(&self, record: &log::Record<'_>) {
                if self.enabled(record.metadata()) {
                    WARNINGS.with(|warnings| {
                        if let Some(lines) = warnings.borrow_mut().as_mut() {
                            lines.push(record.args().to_string());
                        }
                    });
                }
            }
            fn flush(&self) {}
        }
        static LOGGER: ThreadWarnings = ThreadWarnings;
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            log::set_logger(&LOGGER).expect("no other logger in the vmm-core tests");
            log::set_max_level(log::LevelFilter::Warn);
        });
        WARNINGS.with(|warnings| *warnings.borrow_mut() = Some(Vec::new()));
        let value = f();
        let lines = WARNINGS.with(|warnings| warnings.borrow_mut().take().unwrap_or_default());
        (value, lines)
    }

    /// Main logged every exit the run loop cannot handle before the vCPU
    /// stopped; #124's shared handler had dropped that line. Needs the `kvm`
    /// feature on Linux, not /dev/kvm.
    #[test]
    fn unhandled_exit_is_logged_before_the_vcpu_stops() {
        let serial = test_serial();
        let (result, warnings) =
            capture_warnings(|| handle_io_exit(VcpuExit::Shutdown, &MmioBus::new(), &serial));
        assert!(result.is_err());
        assert!(
            warnings
                .iter()
                .any(|line| line.contains("Shutdown") && line.contains("stopping")),
            "{warnings:?}"
        );
    }

    /// `rep movsb` from MMIO exits once per byte, and KVM reaches its
    /// immediate-exit check only at each 1,024-iteration boundary. Finishing
    /// it needs hundreds of re-entries: the production budget must cover
    /// that, and a budget that runs out must leave the vCPU consistent and
    /// runnable rather than failed.
    #[test]
    #[ignore = "requires /dev/kvm; run explicitly on the KVM fixture host"]
    fn kvm_rep_mmio_string_completes_within_budget_and_exhaustion_is_not_fatal() {
        let kvm = kvm_ioctls::Kvm::new().unwrap();
        for budget in [PENDING_IO_REENTRY_BUDGET, 64] {
            // rep movsb (DS:SI in the MMIO hole, ES:DI in RAM); mov al,0xcc; hlt
            let (vm, _mem) = real_mode_vm(&kvm, &[0xf3, 0xa4, 0xb0, 0xcc, 0xf4]);
            let mut vcpu = real_mode_vcpu(&vm);
            let mut regs = vcpu.get_regs().unwrap();
            regs.rsi = 0x8000;
            regs.rdi = 0x2000;
            regs.rcx = 3000;
            regs.rax = 0x5a;
            vcpu.set_regs(&regs).unwrap();
            assert!(matches!(vcpu.run().unwrap(), VcpuExit::MmioRead(..)));
            let bus = MmioBus::new();
            let serial = test_serial();
            let outcome =
                complete_pending_io(&mut vcpu, &bus, &serial, &AtomicBool::new(false), budget)
                    .unwrap();
            assert_eq!(vcpu.get_kvm_run().immediate_exit, 0);
            if budget == PENDING_IO_REENTRY_BUDGET {
                assert_eq!(outcome, PendingIo::Completed);
                let regs = vcpu.get_regs().unwrap();
                assert!(
                    regs.rcx < 3000 && regs.rcx.is_multiple_of(1024),
                    "completion should stop at a 1,024-iteration boundary: rcx={}",
                    regs.rcx
                );
                assert_eq!(regs.rax & 0xff, 0x5a, "next mov must not execute");
            } else {
                // Registers are not consistent mid-instruction; only the
                // outcome and the run below are checked.
                assert_eq!(outcome, PendingIo::Incomplete);
            }
            loop {
                match vcpu.run() {
                    Ok(VcpuExit::Hlt) => break,
                    Ok(exit) => handle_io_exit(exit, &bus, &serial).unwrap(),
                    Err(error) if error.errno() == libc::EINTR => {}
                    Err(error) => panic!("KVM_RUN after completion: {error}"),
                }
            }
            let regs = vcpu.get_regs().unwrap();
            assert_eq!(regs.rcx, 0, "the copy must finish");
            assert_eq!(regs.rax & 0xff, 0xcc);
        }
    }

    /// A pause that lands inside a long MMIO string instruction and cannot
    /// finish it fails that pause only: no state is captured, the vCPU
    /// acknowledges and resumes, and its thread stays alive for the next
    /// pause. With the production budget every such pause succeeds.
    #[test]
    #[ignore = "requires /dev/kvm; run explicitly on the KVM fixture host"]
    fn kvm_pause_that_cannot_finish_io_fails_without_killing_the_vcpu() {
        let kvm = kvm_ioctls::Kvm::new().unwrap();
        // l: mov cx,4000; mov si,0x8000; mov di,0x2000; rep movsb; jmp l
        let program: [u8; 13] = [
            0xb9, 0xa0, 0x0f, 0xbe, 0x00, 0x80, 0xbf, 0x00, 0x20, 0xf3, 0xa4, 0xeb, 0xf3,
        ];
        for budget in [4, PENDING_IO_REENTRY_BUDGET] {
            let (vm, _mem) = real_mode_vm(&kvm, &program);
            let thread = VcpuThread::spawn_with_start_gate(
                VcpuId(0),
                real_mode_vcpu(&vm),
                Arc::new(MmioBus::new()),
                test_serial(),
                None,
                budget,
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
            let mut refused = 0;
            for _ in 0..20 {
                thread.request_snapshot_pause().unwrap();
                if let Err(error) = thread.wait_snapshot_paused() {
                    assert!(
                        error.to_string().contains("did not capture state"),
                        "budget {budget}: {error}"
                    );
                    refused += 1;
                }
                assert!(!thread.is_exited(), "budget {budget}: vCPU thread exited");
                thread.resume();
                thread.wait_snapshot_resumed().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if budget == PENDING_IO_REENTRY_BUDGET {
                assert_eq!(refused, 0, "the production budget must finish rep I/O");
            } else {
                assert!(refused > 0, "no pause landed inside the rep instruction");
            }
            assert!(!thread.is_exited());
            thread.stop().unwrap();
        }
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    /// Exercise actual KVM_RUN and the production stop/Drop paths. Keep the
    /// overall test bounded even if a regression leaves a worker blocked.
    #[test]
    #[ignore = "requires /dev/kvm; explicitly run on the KVM fixture host"]
    fn kvm_busy_vcpus_stop_and_drop_during_startup_and_execution() {
        let (send, recv) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let kvm = kvm_ioctls::Kvm::new().unwrap();
            for stop_explicitly in [true, false] {
                for allow_startup in [true, false] {
                    for _ in 0..8 {
                        let vm = kvm.create_vm().unwrap();
                        vm.set_tss_address(0xfffbd000).unwrap();
                        let mem = vmm_memory_backend::GuestMemory::new(0x4000).unwrap();
                        mem.register(&vm, 0).unwrap();
                        mem.write_phys(0x1000, &[0xeb, 0xfe]).unwrap(); // jmp $
                        let vcpu = vm.create_vcpu(0).unwrap();
                        let mut sregs = vcpu.get_sregs().unwrap();
                        sregs.cs.base = 0;
                        sregs.cs.selector = 0;
                        vcpu.set_sregs(&sregs).unwrap();
                        let mut regs = vcpu.get_regs().unwrap();
                        regs.rip = 0x1000;
                        regs.rflags = 2;
                        vcpu.set_regs(&regs).unwrap();
                        let serial = Arc::new(vmm_devices::serial::Serial::new(
                            vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap(),
                        ));
                        let thread =
                            VcpuThread::spawn(VcpuId(0), vcpu, Arc::new(MmioBus::new()), serial)
                                .unwrap();
                        let exited = thread.exited.clone();
                        if allow_startup {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        if stop_explicitly {
                            thread.stop().unwrap();
                        } else {
                            drop(thread);
                        }
                        assert!(
                            exited.load(Ordering::Acquire),
                            "stop/Drop must join before VM memory is released"
                        );
                    }
                }
            }
            send.send(()).unwrap();
        });
        recv.recv_timeout(std::time::Duration::from_secs(10))
            .expect("vCPU shutdown exceeded 10 seconds");
        worker.join().unwrap();
    }
}
