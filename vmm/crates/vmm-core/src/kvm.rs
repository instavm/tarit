//! KVM VM + vCPU run loop.
//!
//! Open `/dev/kvm`, `KVM_CREATE_VM`, set up guest memory regions,
//! create vCPUs, set registers/CPUID, launch vCPU threads into `KVM_RUN`.
//! MMIO exits are dispatched into the `MmioBus` from `vmm-devices`.
//!
//! Linux+KVM only; behind the `kvm` feature. The non-KVM logic (config,
//! state machine) lives in the other modules and is host-agnostic.

#![cfg(all(feature = "kvm", target_os = "linux"))]

use crate::cpu_template::CpuTemplate;
use crate::error::{Result, VmmError};
use kvm_ioctls::{IoEventAddress, VcpuFd, VmFd};
use std::sync::Arc;
use vmm_devices::bus::{MmioBus, MmioRange};
use vmm_loader::LoadedKernel;
use vmm_memory_backend::GuestMemory;
use vmm_sys_util::eventfd::EventFd;

/// Cached KVM fd — opened once, reused for every VM.
static CACHED_KVM: std::sync::Mutex<Option<kvm_ioctls::Kvm>> = std::sync::Mutex::new(None);

/// A live KVM VM: the VM fd + the guest memory + the MMIO bus.
pub struct KvmVm {
    pub vm_fd: VmFd,
    pub mem: GuestMemory,
    pub mmio_bus: Arc<MmioBus>,
    pub slots: Vec<u32>,
    pub template: CpuTemplate,
    /// GSI routing entries accumulated for KVM_SET_GSI_ROUTING.
    routing_entries: std::sync::Mutex<Vec<kvm_bindings::kvm_irq_routing_entry>>,
}

/// VM-level (not per-vCPU) KVM state captured for a faithful resume: the
/// in-kernel PIC master/slave + IOAPIC (`irqchips`), the PIT (`pit`), and the
/// kvmclock (`clock`). All stored as raw POD struct bytes — `kvm_irqchip` wraps
/// a bindgen union that is not serde-derivable, and raw bytes keep all three
/// uniform and version-locked to the running binary (snapshots are ephemeral).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct VmFullState {
    pub irqchips: Vec<Vec<u8>>,
    pub pit: Vec<u8>,
    pub clock: Vec<u8>,
}

/// Copy a POD KVM struct to a byte vector.
///
/// # Safety
/// `T` must be a plain-old-data `#[repr(C)]` struct with no padding-sensitive
/// invariants (the KVM ioctl structs qualify): reading its bytes is well-defined.
unsafe fn pod_to_bytes<T: Copy>(v: &T) -> Vec<u8> {
    // SAFETY: caller guarantees `T` is POD and `v` is valid for `size_of::<T>()`
    // bytes; the slice is copied immediately into an owned Vec.
    unsafe {
        std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()).to_vec()
    }
}

/// Reconstruct a POD KVM struct from bytes produced by [`pod_to_bytes`].
///
/// # Safety
/// `bytes` must be exactly `size_of::<T>()` long and originally produced from a
/// valid `T` (checked for length here); the resulting value is a bitwise copy.
unsafe fn pod_from_bytes<T: Copy>(bytes: &[u8]) -> Result<T> {
    if bytes.len() != std::mem::size_of::<T>() {
        return Err(VmmError::Kvm(format!(
            "VM state blob size mismatch: got {}, want {}",
            bytes.len(),
            std::mem::size_of::<T>()
        )));
    }
    // SAFETY: caller guarantees `T` is a POD KVM struct for which all-zero bytes
    // are a valid temporary destination before the exact byte copy below.
    let mut v = unsafe { std::mem::zeroed::<T>() };
    // SAFETY: `bytes` length was checked to exactly match `T`, and `v` points to
    // writable storage for that many bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), &mut v as *mut T as *mut u8, bytes.len());
    }
    Ok(v)
}

impl KvmVm {
    /// Open `/dev/kvm`, create the VM, register the pre-loaded guest memory
    /// (with the kernel already in it), install the MMIO bus.
    pub fn new(
        mem: GuestMemory,
        mmio_devices: Vec<(MmioRange, Box<dyn vmm_devices::bus::MmioDevice>)>,
        template: CpuTemplate,
    ) -> Result<Self> {
        Self::new_with_options(mem, mmio_devices, template, false)
    }

    /// Create a VM with optional IRQCHIP + PIT (needed for full kernel boot
    /// to init; not needed for fast cold-boot-to-HLT benchmarks).
    pub fn new_with_options(
        mem: GuestMemory,
        mmio_devices: Vec<(MmioRange, Box<dyn vmm_devices::bus::MmioDevice>)>,
        template: CpuTemplate,
        full_boot: bool,
    ) -> Result<Self> {
        let vm_fd = {
            let mut guard = CACHED_KVM.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_none() {
                *guard = Some(
                    kvm_ioctls::Kvm::new()
                        .map_err(|e| VmmError::Kvm(format!("open /dev/kvm: {e}")))?,
                );
            }
            let kvm = guard
                .as_ref()
                .expect("cached KVM fd is initialized before use");
            kvm.create_vm_with_type(0)
                .map_err(|e| VmmError::Kvm(format!("KVM_CREATE_VM: {e}")))?
        };

        // Set TSS address (required by KVM for x86_64).
        vm_fd
            .set_tss_address(0xfffbd000)
            .map_err(|e| VmmError::Kvm(format!("KVM_SET_TSS_ADDR: {e}")))?;

        if full_boot {
            // Create in-kernel IRQCHIP for LAPIC (timer) + IOAPIC (irqfd).
            // Standard setup order: TSS_ADDR → CREATE_IRQCHIP → CREATE_PIT2.
            // Do NOT call KVM_SET_IDENTITY_MAP_ADDR or KVM_SET_CLOCK —
            // they are not needed here and may interfere with nested virt.
            vm_fd
                .create_irq_chip()
                .map_err(|e| VmmError::Kvm(format!("KVM_CREATE_IRQCHIP: {e}")))?;
            log::info!("KVM_CREATE_IRQCHIP: ok");
            use kvm_bindings::{kvm_pit_config, KVM_PIT_SPEAKER_DUMMY};
            let pit_config = kvm_pit_config {
                flags: KVM_PIT_SPEAKER_DUMMY,
                ..Default::default()
            };
            vm_fd
                .create_pit2(pit_config)
                .map_err(|e| VmmError::Kvm(format!("KVM_CREATE_PIT2: {e}")))?;
        }

        // Register guest memory.
        let slots = mem
            .register(&vm_fd, 0)
            .map_err(|e| VmmError::Memory(e.to_string()))?;

        // Build the MMIO bus with the supplied devices.
        let mut bus = MmioBus::new();
        for (range, dev) in mmio_devices {
            bus.insert(range, dev)
                .map_err(|e| VmmError::Device(format!("mmio bus: {e}")))?;
        }

        Ok(Self {
            vm_fd,
            mem,
            mmio_bus: Arc::new(bus),
            slots,
            template,
            routing_entries: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Create a vCPU with the given ID. The caller sets registers/CPUID
    /// (see [`VcpuRunner::set_initial_regs`]) then calls `run`.
    pub fn create_vcpu(&self, id: u8) -> Result<VcpuFd> {
        self.vm_fd
            .create_vcpu(id as u64)
            .map_err(|e| VmmError::Kvm(format!("create_vcpu({id}): {e}")))
    }

    /// Apply this VM's configured CPU template CPUID to a vCPU.
    pub fn setup_cpuid(&self, vcpu: &VcpuFd) -> Result<()> {
        crate::vcpu_setup::setup_cpuid_with_template(vcpu, &self.template)
    }

    /// Apply this VM's configured boot CPUID template for the given APIC id.
    pub fn apply_boot_cpuid(&self, vcpu: &VcpuFd, apic_id: u8) -> Result<()> {
        crate::vcpu_setup::apply_boot_cpuid_with_template(vcpu, apic_id, &self.template)
    }

    /// Apply this VM's configured CPU-template MSR clearing.
    pub fn apply_cpu_template_msrs(&self, vcpu: &VcpuFd) -> Result<()> {
        crate::vcpu_setup::apply_msr_template(vcpu, &self.template)
    }

    /// Configure the boot vCPU using this VM's configured CPU template.
    pub fn setup_vcpu_for_bzimage_boot_full(
        &self,
        vcpu: &VcpuFd,
        loaded: &LoadedKernel,
        full_boot: bool,
    ) -> Result<()> {
        crate::vcpu_setup::setup_vcpu_for_bzimage_boot_full_with_template(
            vcpu,
            loaded,
            full_boot,
            Some(&self.mem),
            &self.template,
        )
    }

    /// Configure an AP vCPU using this VM's configured CPU template.
    pub fn setup_ap_vcpu(&self, vcpu: &VcpuFd, apic_id: u8) -> Result<()> {
        crate::vcpu_setup::setup_ap_vcpu_with_template(vcpu, apic_id, &self.template)
    }

    /// Re-register guest memory with `KVM_MEM_LOG_DIRTY_PAGES` so KVM tracks
    /// guest writes. Enables incremental (diff) snapshots: each checkpoint then
    /// persists only the pages dirtied since the previous one. Same slots/addrs,
    /// so it does not disturb the running guest. Costs some write-protect
    /// overhead while enabled.
    pub fn enable_dirty_logging(&self) -> Result<()> {
        self.mem
            .register_with_dirty_logging(&self.vm_fd)
            .map(|_| ())
            .map_err(|e| VmmError::Memory(format!("enable dirty logging: {e}")))
    }

    /// Read and reset the dirty-page bitmap accumulated since the last read
    /// (KVM clears its internal bitmap on `KVM_GET_DIRTY_LOG`), so the next read
    /// captures only pages dirtied afterwards — exactly the diff-snapshot
    /// semantics. Call with the vCPU paused for a crash-consistent diff.
    pub fn read_dirty(&self) -> Result<vmm_memory_backend::dirty::DirtyBitmap> {
        vmm_memory_backend::read_dirty_log(&self.vm_fd, &self.mem, &self.slots)
            .map_err(|e| VmmError::Memory(format!("read dirty log: {e}")))
    }

    /// Register an ioeventfd for a MMIO address.
    pub fn register_ioeventfd(&self, mmio_addr: u64, evt: &EventFd) -> Result<()> {
        self.vm_fd
            .register_ioevent(evt, &IoEventAddress::Mmio(mmio_addr), 0u32)
            .map_err(|e| VmmError::Kvm(format!("register_ioevent(0x{mmio_addr:x}): {e}")))?;
        log::debug!("ioeventfd registered: mmio=0x{mmio_addr:x}");
        Ok(())
    }

    /// Like [`register_ioeventfd`], but only fires when the guest writes the
    /// exact 32-bit `datamatch` value to `mmio_addr`. Needed for a shared
    /// QUEUE_NOTIFY register where each queue index is a different value (e.g.
    /// route only the TX-queue kick to a specific eventfd/thread).
    pub fn register_ioeventfd_datamatch(
        &self,
        mmio_addr: u64,
        evt: &EventFd,
        datamatch: u32,
    ) -> Result<()> {
        self.vm_fd
            .register_ioevent(evt, &IoEventAddress::Mmio(mmio_addr), datamatch)
            .map_err(|e| {
                VmmError::Kvm(format!(
                    "register_ioevent(0x{mmio_addr:x}, datamatch={datamatch}): {e}"
                ))
            })?;
        log::debug!("ioeventfd registered: mmio=0x{mmio_addr:x} datamatch={datamatch}");
        Ok(())
    }

    /// Register a PIO ioeventfd. When the guest writes to `pio_port`,
    /// KVM fires the EventFd in-kernel (no userspace exit). This is
    /// used for the i8042 keyboard controller (port 0x60/0x64) so
    /// the kernel's i8042 driver doesn't exit to userspace on every
    /// write — L0 handles the reads internally.
    pub fn register_pio_ioeventfd(&self, pio_port: u64, evt: &EventFd) -> Result<()> {
        self.vm_fd
            .register_ioevent(evt, &IoEventAddress::Pio(pio_port), 0u32)
            .map_err(|e| VmmError::Kvm(format!("register_pio_ioevent(0x{pio_port:x}): {e}")))?;
        log::debug!("pio ioeventfd registered: port=0x{pio_port:x}");
        Ok(())
    }

    /// Register an existing EventFd as an irqfd for a GSI.
    /// Also adds a GSI routing entry routing the GSI to the in-kernel
    /// IOAPIC.
    pub fn register_irqfd(&self, evt: &EventFd, gsi: u32) -> Result<()> {
        self.vm_fd
            .register_irqfd(evt, gsi)
            .map_err(|e| VmmError::Kvm(format!("register_irqfd({gsi}): {e}")))?;

        // Add GSI → IOAPIC routing entry.
        let mut entry = kvm_bindings::kvm_irq_routing_entry {
            gsi,
            type_: kvm_bindings::KVM_IRQ_ROUTING_IRQCHIP,
            ..Default::default()
        };
        entry.u.irqchip.irqchip = kvm_bindings::KVM_IRQCHIP_IOAPIC;
        entry.u.irqchip.pin = gsi;

        let mut entries = self
            .routing_entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        entries.push(entry);
        self.apply_gsi_routing(&entries)?;
        log::info!("irqfd+routing registered: gsi={gsi} → IOAPIC pin {gsi}");
        Ok(())
    }

    /// Capture VM-level (not per-vCPU) state for a faithful resume: the
    /// in-kernel interrupt controllers (PIC master/slave + IOAPIC), the PIT, and
    /// the kvmclock. Without these, a restored VM gets a *fresh* IOAPIC whose
    /// redirection table is masked/default, so the guest's programmed interrupt
    /// routing is lost and any post-restore device I/O (disk, serial) stalls
    /// waiting for interrupts that never arrive. Call with the vCPU paused.
    pub fn capture_vm_state(&self) -> Result<VmFullState> {
        // PIC master (0), PIC slave (1), IOAPIC (2). Stored as raw struct bytes
        // because kvm_irqchip contains a bindgen union that is not serde-derivable.
        let mut irqchips = Vec::with_capacity(3);
        for chip_id in [
            kvm_bindings::KVM_IRQCHIP_PIC_MASTER,
            kvm_bindings::KVM_IRQCHIP_PIC_SLAVE,
            kvm_bindings::KVM_IRQCHIP_IOAPIC,
        ] {
            let mut chip = kvm_bindings::kvm_irqchip {
                chip_id,
                ..Default::default()
            };
            self.vm_fd
                .get_irqchip(&mut chip)
                .map_err(|e| VmmError::Kvm(format!("KVM_GET_IRQCHIP({chip_id}): {e}")))?;
            // SAFETY: kvm_irqchip is a KVM POD ioctl struct captured from KVM.
            irqchips.push(unsafe { pod_to_bytes(&chip) });
        }
        let pit = self
            .vm_fd
            .get_pit2()
            .map_err(|e| VmmError::Kvm(format!("KVM_GET_PIT2: {e}")))?;
        let clock = self
            .vm_fd
            .get_clock()
            .map_err(|e| VmmError::Kvm(format!("KVM_GET_CLOCK: {e}")))?;
        Ok(VmFullState {
            irqchips,
            // SAFETY: kvm_pit_state2 is a KVM POD ioctl struct captured from KVM.
            pit: unsafe { pod_to_bytes(&pit) },
            // SAFETY: kvm_clock_data is a KVM POD ioctl struct captured from KVM.
            clock: unsafe { pod_to_bytes(&clock) },
        })
    }

    /// Re-apply VM-level state captured by [`Self::capture_vm_state`]. Call after
    /// the fresh in-kernel IRQCHIP + PIT have been created (they must exist
    /// before KVM_SET_IRQCHIP/PIT2 succeed) and before the vCPU runs.
    pub fn restore_vm_state(&self, s: &VmFullState) -> Result<()> {
        for bytes in &s.irqchips {
            // SAFETY: snapshot validation guarantees this blob came from the same
            // binary's `pod_to_bytes` and `pod_from_bytes` checks its length.
            let chip: kvm_bindings::kvm_irqchip = unsafe { pod_from_bytes(bytes)? };
            self.vm_fd
                .set_irqchip(&chip)
                .map_err(|e| VmmError::Kvm(format!("KVM_SET_IRQCHIP: {e}")))?;
        }
        if !s.pit.is_empty() {
            // SAFETY: `pod_from_bytes` verifies the serialized PIT blob length.
            let pit: kvm_bindings::kvm_pit_state2 = unsafe { pod_from_bytes(&s.pit)? };
            self.vm_fd
                .set_pit2(&pit)
                .map_err(|e| VmmError::Kvm(format!("KVM_SET_PIT2: {e}")))?;
        }
        if !s.clock.is_empty() {
            // SAFETY: `pod_from_bytes` verifies the serialized clock blob length.
            let clock: kvm_bindings::kvm_clock_data = unsafe { pod_from_bytes(&s.clock)? };
            self.vm_fd
                .set_clock(&clock)
                .map_err(|e| VmmError::Kvm(format!("KVM_SET_CLOCK: {e}")))?;
        }
        Ok(())
    }

    /// Apply the accumulated GSI routing table to KVM.
    fn apply_gsi_routing(&self, entries: &[kvm_bindings::kvm_irq_routing_entry]) -> Result<()> {
        // Build a kvm_irq_routing FAM struct manually. Layout:
        //   u32 nr_entries, u32 flags, kvm_irq_routing_entry[nr_entries]
        use std::mem;
        let nr = entries.len() as u32;
        let entry_size = mem::size_of::<kvm_bindings::kvm_irq_routing_entry>();
        let total_size = 8 + nr as usize * entry_size;
        let mut buf = vec![0u8; total_size];
        buf[0..4].copy_from_slice(&nr.to_le_bytes());
        for (i, entry) in entries.iter().enumerate() {
            let off = 8 + i * entry_size;
            // SAFETY: `entry` points to a valid KVM routing entry and the slice is
            // copied immediately into the byte buffer passed to KVM.
            let entry_bytes =
                unsafe { std::slice::from_raw_parts(entry as *const _ as *const u8, entry_size) };
            buf[off..off + entry_size].copy_from_slice(entry_bytes);
        }
        // SAFETY: buf contains a valid kvm_irq_routing struct.
        let routing = unsafe { &*(buf.as_ptr() as *const kvm_bindings::kvm_irq_routing) };
        self.vm_fd
            .set_gsi_routing(routing)
            .map_err(|e| VmmError::Kvm(format!("KVM_SET_GSI_ROUTING: {e}")))?;
        Ok(())
    }

    /// Get the current thread's OS TID (for CPU pinning via `cpuset`).
    /// Call this from within a vCPU thread to get its TID.
    pub fn current_tid() -> u32 {
        // SAFETY: gettid has no preconditions and returns the current thread id.
        unsafe { libc::gettid() as u32 }
    }

    /// One vCPU's run loop: call `KVM_RUN`, handle exits (MMIO → bus, IO →
    /// serial, HLT → pause), repeat. Runs until the VM halts or an
    /// unrecoverable exit occurs.
    pub fn run_vcpu(&self, vcpu_fd: &mut VcpuFd) -> Result<()> {
        self.run_vcpu_with_i8042(vcpu_fd, None)
    }

    /// Run the vCPU loop with optional i8042 IRQ injection.
    /// When `i8042_irq` is set, writing to port 0x60 triggers an IRQ
    /// so the kernel's i8042 driver can read the ACK response.
    pub fn run_vcpu_with_i8042(
        &self,
        vcpu_fd: &mut VcpuFd,
        i8042_irq: Option<&EventFd>,
    ) -> Result<()> {
        use kvm_ioctls::VcpuExit;
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let timeout = Duration::from_secs(
            std::env::var("VMM_BOOT_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(10), // 10s default — kernel boots in <1s, panic+reboot in ~2s
        );
        let mut hlt_count = 0u32;
        let mut io_out_count = 0u64;

        // Install a no-op SIGALRM handler so the signal interrupts KVM_RUN
        // (returns EINTR) without killing the process. SIG_IGN does NOT
        // cause EINTR — we need a real (empty) handler function.
        //
        // `alarm()` posts SIGALRM at the *process* level, so on a
        // multi-threaded process (libtest worker, embedded VMM, etc.) the
        // kernel may deliver the signal to a thread that isn't this one —
        // and our KVM_RUN keeps blocking forever. We spawn a tiny watchdog
        // thread that sleeps for the timeout and then `tgkill`s our exact
        // TID, so EINTR lands on the thread executing KVM_RUN. The
        // watchdog drops out early via a shared atomic when the vCPU
        // returns (HLT, error, etc.).
        //
        // IMPORTANT: sigaction + thread spawn must happen BEFORE the
        // seccomp filter is installed — both `rt_sigaction` and `clone`
        // are not in the vCPU allowlist (intentionally — the steady-state
        // run loop never needs them).
        // SAFETY: sigaction is called with a valid signal number and a no-op
        // handler to interrupt KVM_RUN; errors are non-fatal and ignored here.
        unsafe {
            extern "C" fn noop_handler(_: libc::c_int) {}
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = noop_handler as *const () as usize;
            libc::sigaction(libc::SIGALRM, &sa, std::ptr::null_mut());
        }
        // SAFETY: syscall(SYS_gettid) has no preconditions and returns this TID.
        let my_tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let watchdog_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watchdog_done_thread = watchdog_done.clone();
        let timeout_secs = timeout.as_secs();
        let watchdog = Some(std::thread::spawn(move || {
            // SAFETY: getpid has no preconditions and cannot invalidate memory.
            let pid = unsafe { libc::getpid() };
            let total_ms = timeout_secs * 1000;
            for ms in 0..total_ms {
                if watchdog_done_thread.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                // Fire SIGALRM every 10ms to interrupt KVM_RUN when the
                // guest HLTs (in-kernel IRQCHIP keeps HLT in-kernel, but
                // on nested virt the SIGALRM provides a safety net).
                if ms % 10 == 0 && ms > 0 {
                    // SAFETY: tgkill targets the current process and vCPU TID
                    // with SIGALRM; the watchdog intentionally ignores errors.
                    unsafe {
                        libc::syscall(libc::SYS_tgkill, pid, my_tid, libc::SIGALRM);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if watchdog_done_thread.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            // Final timeout — set done flag THEN fire SIGALRM so the
            // EINTR handler sees done=true and exits the run loop.
            watchdog_done_thread.store(true, std::sync::atomic::Ordering::Relaxed);
            // SAFETY: tgkill targets the current process and vCPU TID with
            // SIGALRM to force KVM_RUN to return on timeout.
            unsafe {
                libc::syscall(libc::SYS_tgkill, pid, my_tid, libc::SIGALRM);
            }
        }));

        // NOTE: seccomp is installed on the *dedicated* vCPU thread
        // (vcpu_thread.rs), not here. `run_vcpu` runs on the caller's
        // thread — the controller invokes it from boot_vm on the same
        // thread that later does snapshot/restore/stop, and seccomp is
        // sticky for the thread's lifetime, so installing here would
        // poison snapshot's `openat` and similar syscalls.
        // RAII guard: setting `done = true` + joining the watchdog ensures
        // no stale tgkill can fire after `run_vcpu` returns.
        struct WatchdogGuard {
            done: std::sync::Arc<std::sync::atomic::AtomicBool>,
            handle: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for WatchdogGuard {
            fn drop(&mut self) {
                self.done.store(true, std::sync::atomic::Ordering::Relaxed);
                if let Some(h) = self.handle.take() {
                    let _ = h.join();
                }
            }
        }
        let _guard = WatchdogGuard {
            done: watchdog_done.clone(),
            handle: watchdog,
        };
        let cancel_timer = || {
            watchdog_done.store(true, std::sync::atomic::Ordering::Relaxed);
        };

        // Counter for MMIO exits.
        let mmio_count: std::cell::Cell<u64> = std::cell::Cell::new(0);

        loop {
            // Hard timeout guard: the watchdog thread sets `watchdog_done`
            // after the boot timeout. Check it every iteration so a guest that
            // HLT-spins — KVM_RUN returns Ok(Hlt) immediately without blocking,
            // so it never yields EINTR — can't run this loop forever at 100% CPU.
            if watchdog_done.load(std::sync::atomic::Ordering::Relaxed) {
                log::info!(
                    "vCPU run: watchdog timeout after {:?} — {} PIO, {} HLTs, {} MMIO",
                    start.elapsed(),
                    io_out_count,
                    hlt_count,
                    mmio_count.get()
                );
                cancel_timer();
                return Ok(());
            }
            match vcpu_fd.run() {
                Ok(exit) => {
                    match exit {
                        VcpuExit::MmioWrite(addr, data) => {
                            mmio_count.set(mmio_count.get() + 1);
                            let mut val: u64 = 0;
                            for (i, &b) in data.iter().enumerate().take(8) {
                                val |= (b as u64) << (i * 8);
                            }
                            log::debug!("MMIO write 0x{addr:x} len={} val=0x{val:x}", data.len());
                            if self.mmio_bus.write(addr, val, data.len() as u8).is_err() {
                                log::warn!("unhandled MMIO write at 0x{addr:x}");
                            }
                        }
                        VcpuExit::MmioRead(addr, data) => {
                            mmio_count.set(mmio_count.get() + 1);
                            match self.mmio_bus.read(addr, data.len() as u8) {
                                Ok(val) => {
                                    log::debug!(
                                        "MMIO read 0x{addr:x} len={} → 0x{val:x}",
                                        data.len()
                                    );
                                    let bytes = val.to_le_bytes();
                                    for (i, slot) in data.iter_mut().enumerate().take(8) {
                                        *slot = bytes[i];
                                    }
                                }
                                Err(_) => log::warn!("unhandled MMIO read at 0x{addr:x}"),
                            }
                        }
                        VcpuExit::Hlt => {
                            // HLT should be handled in-kernel by the LAPIC
                            // timer. If it exits to userspace, the LAPIC timer
                            // is not firing (CPUID TSC-Deadline bit may be
                            // masked off). Log and continue — the watchdog
                            // SIGALRM will re-enter KVM_RUN.
                            hlt_count += 1;
                            if hlt_count <= 3 {
                                log::warn!("HLT exit to userspace (#{hlt_count}) — LAPIC timer not firing?");
                            }
                        }
                        VcpuExit::IoOut(port, data) => {
                            io_out_count += 1;
                            if port == 0x3f8 {
                                use std::io::Write;
                                let _ = std::io::stdout().write_all(data);
                                let _ = std::io::stdout().flush();
                            }
                            // i8042: after writing to port 0x60, inject IRQ 1
                            if port == 0x60 {
                                if let Some(evt) = i8042_irq {
                                    let _ = evt.write(1);
                                }
                            }
                            // i8042 reset: writing 0xFE to port 0x64 triggers a
                            // system reset. The kernel does this on panic with
                            // reboot=k. Stop immediately instead of waiting for
                            // the watchdog timeout.
                            if port == 0x64 && data.contains(&0xfe) {
                                log::info!("i8042 reset (port 0x64, 0xFE) — guest rebooting");
                                cancel_timer();
                                return Ok(());
                            }
                        }
                        VcpuExit::IoIn(port, data) => {
                            // Serial port 0x3f8: TX data (we already write it out).
                            // Serial port 0x3fd: line status register (TX ready)
                            if port == 0x3fd {
                                data[0] = 0x60; // LSR: THR empty + data ready
                            }
                            if port == 0x3fa {
                                data[0] = 0; // FCR: no FIFO
                            }
                            // VGA ports: return 0xff
                            if port == 0x3d4 || port == 0x3d5 || port == 0x3da {
                                for b in data.iter_mut() {
                                    *b = 0xff;
                                }
                            }
                            // PS/2 controller 0x60 (data) / 0x64 (status)
                            if port == 0x60 {
                                data[0] = 0xfa; // ACK response
                            }
                            if port == 0x64 {
                                data[0] = 0x14;
                            }
                        }
                        VcpuExit::Shutdown => {
                            // Shutdown = triple fault (kernel panic + reboot).
                            log::info!("KVM_EXIT_SHUTDOWN — guest rebooted/reset");
                            cancel_timer();
                            return Ok(());
                        }
                        other => {
                            log::warn!("KVM_RUN exit {other:?} — stopping");
                            cancel_timer();
                            return Err(VmmError::Kvm(format!("unhandled exit: {other:?}")));
                        }
                    }
                }
                Err(e) => {
                    if e.errno() == libc::EINTR {
                        if watchdog_done.load(std::sync::atomic::Ordering::Relaxed) {
                            log::info!(
                                "vCPU run timed out after {:?} — {} PIO, {} HLTs, {} MMIO",
                                start.elapsed(),
                                io_out_count,
                                hlt_count,
                                mmio_count.get()
                            );
                            cancel_timer();
                            self.dump_guest_memory_diagnostic();
                            return Ok(());
                        }
                        continue;
                    }
                    cancel_timer();
                    return Err(VmmError::Kvm(format!("KVM_RUN: {e}")));
                }
            }
        }
    }

    /// Diagnostic: dump guest memory to a private runtime file and scan for
    /// kernel log strings. Called when the boot timeout fires with 0 exits —
    /// tells us whether the kernel is actually running (just not producing
    /// PIO exits due to nested-virt coalescing) or truly stuck.
    fn dump_guest_memory_diagnostic(&self) {
        let size = self.mem.size_bytes as usize;
        let ptr = self.mem.as_ptr();
        if ptr.is_null() {
            log::warn!("diagnostic: guest memory pointer is null");
            return;
        }
        // SAFETY: ptr is valid for size_bytes reads (GuestMemory contract).
        let slice = unsafe { std::slice::from_raw_parts(ptr, size) };

        // Scan for kernel log strings anywhere in guest memory.
        let needles = [
            "Linux version",
            "Command line:",
            "Kernel command line:",
            "BIOS-provided physical RAM map:",
            "Memory:",
            "NR_CPUS",
            "CPU0",
            "console [ttyS0] enabled",
            "Kernel panic",
            "VFS: Cannot open root",
            "microVM booted",
            "Hello from the guest",
            "request_module",
            " virtio-mmio",
            "console=ttyS0",
        ];
        let mut found = false;
        for needle in &needles {
            if let Some(pos) = find_substring(slice, needle.as_bytes()) {
                found = true;
                // Show up to 200 bytes of context starting a bit before the match.
                let ctx_start = pos.saturating_sub(10);
                let ctx_end = (pos + 250).min(size);
                let ctx = &slice[ctx_start..ctx_end];
                // Trim at first NUL to avoid dumping garbage.
                let trim_end = ctx.iter().position(|&b| b == 0).unwrap_or(ctx.len());
                let text = String::from_utf8_lossy(&ctx[..trim_end]);
                log::info!("diagnostic: found '{needle}' at GPA 0x{pos:x}:\n{text}");
            }
        }
        if !found {
            log::warn!(
                "diagnostic: no kernel log strings found in guest memory ({size} bytes scanned)"
            );
            // Dump the first 256 bytes of the kernel load address to see if
            // the decompression even started.
            let kstart = 0x200000usize;
            if kstart + 64 <= size {
                let bytes = &slice[kstart..kstart + 64];
                log::info!(
                    "diagnostic: bytes at kernel load (0x200000): {}",
                    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
                );
            }
        }

        // Write the full memory dump for offline analysis.
        match write_private_guest_memory_dump(slice) {
            Ok(dump_path) => log::info!(
                "diagnostic: guest memory dumped to {} ({size} bytes)",
                dump_path.display()
            ),
            Err(e) => log::warn!("diagnostic: failed to dump guest memory: {e}"),
        }
    }
}

fn write_private_guest_memory_dump(slice: &[u8]) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    // Mirror controller::private_runtime_dir: a private 0700 subdir under the
    // system temp dir (disk-backed), never the CWD/source tree or a small
    // runtime tmpfs like XDG_RUNTIME_DIR.
    let dir = std::env::temp_dir()
        .join(".vmm-runtime")
        .join(format!("vmm-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("vmm-guest-mem-{}-{ts}.bin", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&path)?;
    file.write_all(slice)?;
    file.flush()?;
    Ok(path)
}

/// Boyer-Moore-free naive substring search (the haystack is large but the
/// needle is short, so this is fine for a one-shot diagnostic).
fn find_substring(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let last = haystack.len() - needle.len();
    (0..=last).find(|&i| haystack[i..i + needle.len()] == *needle)
}
