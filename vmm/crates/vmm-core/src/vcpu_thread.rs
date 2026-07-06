//! Threaded vCPU run loop — runs KVM_RUN in a background thread so the
//! main thread can pause/snapshot/resume the VM, and send serial input
//! to a running guest (for real exec, like `docker exec`).

#![cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]

use crate::error::{Result, VmmError};
use kvm_ioctls::{VcpuExit, VcpuFd};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use vmm_devices::bus::MmioBus;

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
    /// Spawn a vCPU thread that runs KVM_RUN in a loop.
    pub fn spawn(
        mut vcpu: VcpuFd,
        mmio_bus: Arc<MmioBus>,
        serial: Arc<vmm_devices::serial::Serial>,
    ) -> Self {
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

        let handle = thread::spawn(move || -> Result<()> {
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

            // Warm up glibc's allocator BEFORE seccomp closes off openat. On the
            // first large (mmap-backed) allocation glibc lazily reads
            // /proc/sys/vm/overcommit_memory; if that first big allocation
            // happens later in the run loop (e.g. buffering a chatty guest's
            // serial output), the openat is rejected with SIGSYS and the vCPU
            // thread is killed mid-boot. Forcing the read here, while syscalls
            // are unrestricted, makes glibc cache the result so the steady-state
            // run loop never needs openat.
            {
                let mut warm: Vec<u8> = Vec::with_capacity(8 * 1024 * 1024);
                warm.resize(8 * 1024 * 1024, 0);
                warm[0] = 1;
                warm[8 * 1024 * 1024 - 1] = 1;
                std::hint::black_box(&warm);
                drop(warm);
            }

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
            let result = (|| -> Result<()> {
                loop {
                    // Stop requested — exit cleanly (no error).
                    if stop_for_thread.load(Ordering::Relaxed) {
                        log::info!("vCPU thread stopping");
                        return Ok(());
                    }
                    // Check control channel.
                    if ctrl.load(Ordering::Relaxed) {
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
                        paus.store(true, Ordering::Relaxed);
                        log::info!("vCPU thread pausing");
                        // Spin-wait for control to clear (resume) or for stop.
                        while ctrl.load(Ordering::Relaxed)
                            && !stop_for_thread.load(Ordering::Relaxed)
                        {
                            thread::sleep(std::time::Duration::from_millis(10));
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
                            // With in-kernel IRQCHIP, HLT should be handled
                            // by KVM internally (no exit to userspace). If we
                            // DO get a HLT exit, the kernel is idle — yield
                            // to let other threads (serial, net) run.
                            std::thread::yield_now();
                        }
                        Ok(VcpuExit::MmioWrite(addr, data)) => {
                            let mut val: u64 = 0;
                            for (i, &b) in data.iter().enumerate().take(8) {
                                val |= (b as u64) << (i * 8);
                            }
                            let _ = mmio_bus.write(addr, val, data.len() as u8);
                        }
                        Ok(VcpuExit::MmioRead(addr, data)) => {
                            if let Ok(val) = mmio_bus.read(addr, data.len() as u8) {
                                let bytes = val.to_le_bytes();
                                for (i, slot) in data.iter_mut().enumerate().take(8) {
                                    *slot = bytes[i];
                                }
                            }
                        }
                        Ok(VcpuExit::IoOut(port, data)) => {
                            if (0x3f8..=0x3ff).contains(&port) {
                                serial_for_thread.write((port - 0x3f8) as u8, data[0]);
                            }
                        }
                        Ok(VcpuExit::IoIn(port, data)) => {
                            if (0x3f8..=0x3ff).contains(&port) {
                                data[0] = serial_for_thread.read((port - 0x3f8) as u8);
                            }
                        }
                        Ok(other) => {
                            log::warn!("vCPU exit {other:?} — stopping");
                            return Err(VmmError::Kvm(format!("unhandled exit: {other:?}")));
                        }
                        Err(e) => {
                            // EINTR = a control-thread tgkill(SIGALRM) fired
                            // to interrupt this blocking KVM_RUN so we re-check
                            // the control channel. EAGAIN can be returned by KVM
                            // while an AP vCPU progresses through its INIT/SIPI
                            // bringup state machine — it is retryable, not
                            // fatal. Anything else is fatal.
                            let errno = e.errno();
                            if errno != libc::EINTR && errno != libc::EAGAIN {
                                return Err(VmmError::Kvm(format!("KVM_RUN: {e}")));
                            }
                        }
                    }
                }
            })();
            result
        });

        Self {
            handle: Some(handle),
            control,
            stop_flag,
            paused,
            exited,
            tid,
            serial,
            captured_state,
        }
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
    pub fn pause(&self) {
        if self.exited.load(Ordering::Relaxed) {
            return;
        }
        self.control.store(true, Ordering::Relaxed);
        // Poke the vCPU thread until it pauses. The kick is re-sent every
        // tick because KVM may re-enter the guest before we observe `paused`.
        while !self.paused.load(Ordering::Relaxed) && !self.exited.load(Ordering::Relaxed) {
            // If the vCPU thread died abruptly (a seccomp SIGSYS terminates it
            // without running the exit guard), it can never set paused/exited.
            // A zero-signal liveness probe lets pause() — and therefore
            // snapshot()/stop() — abort instead of spinning forever.
            if !self.vcpu_alive() {
                log::warn!("vCPU thread is gone; aborting pause (guest already dead)");
                self.exited.store(true, Ordering::Relaxed);
                self.paused.store(true, Ordering::Relaxed);
                return;
            }
            self.signal_vcpu();
            thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Resume the vCPU from a pause.
    pub fn resume(&self) {
        self.control.store(false, Ordering::Relaxed);
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
