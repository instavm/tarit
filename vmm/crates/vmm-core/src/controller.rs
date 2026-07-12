//! VMM controller — manages the single VM lifecycle (1:1 model).
//!
//! One VMM process = one microVM. The controller owns at most one VM at a
//! time. Lifecycle: boot → (pause/resume)* → snapshot/restore → stop.

use crate::config::VmConfig;
use crate::error::{Result, VmmError};
use crate::gc::OwnedScratchFile;
use crate::state::VmState;
#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const EXEC_OUTPUT_CAP: usize = 16 * 1024 * 1024;
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const EXEC_OUTPUT_TRUNCATED: &[u8] = b"\n[VMM exec output truncated]\n";
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const EXEC_OUTPUT_PAYLOAD_CAP: usize = EXEC_OUTPUT_CAP - EXEC_OUTPUT_TRUNCATED.len();
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const EXEC_ACC_TAIL_CAP: usize = 64 * 1024;

/// State backing a VM whose vCPU is actively executing in a background thread.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
pub struct RunningVm {
    pub kvm_vm: crate::kvm::KvmVm,
    pub vcpu_thread: crate::vcpu_thread::VcpuThread,
    /// Application-processor (AP) vCPU threads for SMP (`vcpus.count > 1`).
    /// Empty for a uniprocessor VM. They share the BSP's `Serial` and MMIO bus,
    /// and are paused/resumed/stopped together with the BSP.
    pub ap_threads: Vec<crate::vcpu_thread::VcpuThread>,
    pub loaded_entry: u64,
    /// virtio-net host<->tap I/O loops. Each is dropped (which stops+joins the
    /// thread) before `keep_alive_fds`, whose EventFds the loops reference as
    /// their TX kick fd — so declaration order here is load-bearing.
    pub net_io_loops: Vec<vmm_devices::virtio::net_io_loop::NetIoLoop>,
    pub blk_devices: Vec<Arc<vmm_devices::virtio::blk_transport::VirtioBlkMmio>>,
    pub net_devices: Vec<Arc<vmm_devices::virtio::net_transport::VirtioNetMmio>>,
    /// TAP devices backing the virtio-net loops; closed after the loops stop.
    pub taps: Vec<vmm_net::tap::Tap>,
    /// virtio-vsock host pump thread (host→guest RX). Dropped before the irqfds.
    pub vsock_pump: Option<vmm_devices::virtio::vsock_io_loop::VsockPump>,
    /// Host side of the vsock exec channel (accepts the guest agent's dial).
    pub vsock_exec: Option<std::sync::Arc<crate::vsock_exec::VsockExecChannel>>,
    /// Host side of the interactive PTY channel (connects to guest port 1025).
    pub vsock_pty: Option<std::sync::Arc<crate::vsock_pty::VsockPtyChannel>>,
    /// irqfd EventFds that must stay open for the VM's lifetime. Owned here so
    /// they are closed when the VM stops — create/stop cycles must not leak fds
    /// (a PaaS churns through many thousands of VMs per host).
    pub keep_alive_fds: Vec<vmm_sys_util::eventfd::EventFd>,
}

/// A VM instance with its memory available for snapshot.
pub struct VmInstance {
    pub state: VmState,
    /// When this instance was created in-process, for uptime reporting.
    pub created_at: std::time::Instant,
    /// Path of the most recent snapshot of this VM (the parent for the next
    /// incremental diff). `None` until the first snapshot. Enables diff chains:
    /// each diff references its parent so restore can replay base + diffs.
    pub last_snapshot: Option<String>,
    /// VMM-owned scratch files removed when this VM stops or is dropped.
    pub transient_files: VmTransientFiles,
    /// True once KVM dirty-page logging has been enabled for this VM (after the
    /// first snapshot), so subsequent diff snapshots capture only changed pages.
    pub dirty_logging: bool,
    pub config: VmConfig,
    pub guest_mem: Option<vmm_memory_backend::GuestMemory>,
    pub state_blob: Option<Vec<u8>>,
    pub mem_dump: Option<Vec<u8>>,
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub lazy_restore: Option<vmm_memory_backend::LazyRestore>,
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub running: Option<RunningVm>,
}

impl Drop for VmInstance {
    fn drop(&mut self) {
        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        stop_running_vm(self);
        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        {
            self.lazy_restore = None;
        }
        self.transient_files.cleanup();
    }
}

/// Scratch files owned by one VM instance.
#[derive(Debug, Default)]
pub struct VmTransientFiles {
    live_snapshot: Option<OwnedScratchFile>,
    suspend_snapshot: Option<OwnedScratchFile>,
    owned_overlays: Vec<OwnedScratchFile>,
}

impl VmTransientFiles {
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    fn from_owned_overlay_paths(paths: Vec<PathBuf>) -> Self {
        Self {
            owned_overlays: paths.into_iter().map(OwnedScratchFile::remember).collect(),
            ..Self::default()
        }
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    fn set_live_snapshot_owned(&mut self, path: OwnedScratchFile) {
        if let Some(old) = self.live_snapshot.replace(path) {
            remove_owned_scratch_file(&old);
        }
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    fn set_suspend_snapshot(&mut self, path: PathBuf) {
        if let Some(old) = self
            .suspend_snapshot
            .replace(OwnedScratchFile::remember(path))
        {
            remove_owned_scratch_file(&old);
        }
    }

    fn cleanup(&mut self) {
        if let Some(path) = self.live_snapshot.take() {
            remove_owned_scratch_file(&path);
        }
        if let Some(path) = self.suspend_snapshot.take() {
            remove_owned_scratch_file(&path);
        }
        for path in self.owned_overlays.drain(..) {
            remove_owned_scratch_file(&path);
        }
    }
}

fn remove_owned_scratch_file(file: &OwnedScratchFile) {
    match file.remove() {
        Ok(true) => log::info!("removed VM scratch file {}", file.path().display()),
        Ok(false) => {}
        Err(e) => log::warn!("remove VM scratch file {}: {e}", file.path().display()),
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
struct OwnedOverlayGuard {
    paths: Vec<PathBuf>,
    armed: bool,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
impl OwnedOverlayGuard {
    fn new(config: &VmConfig) -> Self {
        Self {
            paths: vmm_created_overlay_candidates(config),
            armed: true,
        }
    }

    fn disarm(mut self) -> Vec<PathBuf> {
        self.armed = false;
        std::mem::take(&mut self.paths)
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
impl Drop for OwnedOverlayGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for path in &self.paths {
            remove_file_if_exists(path);
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn vmm_created_overlay_candidates(config: &VmConfig) -> Vec<PathBuf> {
    config
        .volumes
        .iter()
        .filter_map(|vol| vol.overlay.as_deref())
        .map(PathBuf::from)
        .filter(|path| crate::gc::is_owned_overlay_name(path) && !path.exists())
        .collect()
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn remove_file_if_exists(path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Ok(()) => log::info!("removed VM scratch file {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("remove VM scratch file {}: {e}", path.display()),
    }
}

/// The VMM controller — owns at most one VM (1:1 process model).
pub struct VmmController {
    vm: Arc<Mutex<Option<VmInstance>>>,
}

impl VmmController {
    pub fn new() -> Self {
        Self {
            vm: Arc::new(Mutex::new(None)),
        }
    }

    /// Lock the VM slot, recovering from a poisoned mutex. API handlers are
    /// panic-isolated by `catch_unwind` in the RPC layer, but a panic while the
    /// lock is held would otherwise poison it and turn every subsequent request
    /// into a `PoisonError` panic — bricking the whole VM process. Recovering
    /// the inner value keeps the process serviceable.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<VmInstance>> {
        self.vm.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Boot the single VM. Error if one already exists.
    pub fn create(&self, config: VmConfig) -> Result<()> {
        config.validate()?;
        let mut slot = self.lock();
        if slot.is_some() {
            return Err(VmmError::InvalidConfig(
                "VM already exists (1:1 model — stop first)".into(),
            ));
        }

        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        {
            let (guest_mem, state_blob) = self.boot_vm(&config)?;
            *slot = Some(VmInstance {
                state: VmState::Paused,
                created_at: std::time::Instant::now(),
                last_snapshot: None,
                transient_files: VmTransientFiles::default(),
                dirty_logging: false,
                config,
                guest_mem: Some(guest_mem),
                state_blob: Some(state_blob),
                mem_dump: None,
                lazy_restore: None,
                running: None,
            });
        }

        #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
        {
            *slot = Some(VmInstance {
                state: VmState::Created,
                created_at: std::time::Instant::now(),
                last_snapshot: None,
                transient_files: VmTransientFiles::default(),
                dirty_logging: false,
                config,
                guest_mem: None,
                state_blob: None,
                mem_dump: None,
                #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
                lazy_restore: None,
                #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
                running: None,
            });
        }

        log::info!("VM created");
        Ok(())
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    fn boot_vm(&self, config: &VmConfig) -> Result<(vmm_memory_backend::GuestMemory, Vec<u8>)> {
        use std::time::Instant;
        use vmm_loader::load;
        use vmm_memory_backend::GuestMemory;

        let t0 = Instant::now();
        let mem_size = config.memory.size_bytes()?;
        let mem = GuestMemory::new(mem_size).map_err(|e| VmmError::Memory(e.to_string()))?;
        let t_mem = t0.elapsed();

        let t1 = Instant::now();
        let cmdline = if config.kernel.cmdline.is_empty() {
            vmm_loader::default_cmdline()
        } else {
            config.kernel.cmdline.clone()
        };
        let kernel_path = PathBuf::from(&config.kernel.path);
        let initramfs_path = config.kernel.initramfs.as_ref().map(PathBuf::from);
        let loaded = load(
            &mem.inner,
            &kernel_path,
            &cmdline,
            initramfs_path.as_ref(),
            mem.size_bytes,
        )
        .map_err(|e| VmmError::Loader(e.to_string()))?;
        let t_load = t1.elapsed();

        let t2 = Instant::now();
        crate::vcpu_setup::write_gdt(&mem).map_err(|e| VmmError::Device(e.to_string()))?;
        let template = crate::cpu_template::CpuTemplate::bare();
        let vm = crate::kvm::KvmVm::new_with_options(mem.clone(), vec![], template, false)?;
        let mut vcpu = vm.create_vcpu(0)?;
        vm.setup_vcpu_for_bzimage_boot_full(&vcpu, &loaded, false)?;
        let t_setup = t2.elapsed();

        let t3 = Instant::now();
        vm.run_vcpu(&mut vcpu)?;
        let t_run = t3.elapsed();

        let vcpu_state = save_vcpu_state(&vcpu)?;
        let total = t0.elapsed();
        log::info!(
            "VM: boot perf — mem={:?} load={:?} setup={:?} run={:?} total={:?}",
            t_mem,
            t_load,
            t_setup,
            t_run,
            total
        );

        let state_blob = serialize_state_blob(loaded.entry, mem.size_bytes, &vcpu_state, config);
        Ok((mem, state_blob))
    }

    pub fn snapshot(&self, diff: bool) -> Result<String> {
        let mut slot = self.lock();
        let vm = slot
            .as_mut()
            .ok_or_else(|| VmmError::InvalidConfig("no VM (boot first)".into()))?;

        let path_buf = unique_scratch_snapshot_path("vmm-snap")?;
        let path = path_buf.to_string_lossy().into_owned();

        let state_before = vm.state;

        // Pause ALL vCPUs while we copy guest RAM so the dump is crash-consistent
        // — no vCPU (BSP or AP) can mutate memory mid-copy. Resumed right after,
        // so a snapshot never stops a running VM. (An idle guest is already
        // quiescent; this makes an actively-running guest's snapshot consistent.)
        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        let paused_here = pause_running_vcpus(vm);

        let snapshot_result = (|| -> Result<usize> {
            // Fold the vCPU state each thread captured during the pause above into
            // the stored state blob, so this snapshot is faithfully resumable (not
            // just a memory image). The boot-time blob already carries entry/
            // mem_size/kernel/cmdline/vcpus/volumes/net; we only attach the live
            // vCPU register+MSR+LAPIC state (BSP + each AP for SMP). If nothing was
            // captured (e.g. a VM that never ran), the blob is written unchanged.
            #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
            capture_live_state(vm);

            // Write guest memory. When `diff` is requested and we have a parent
            // snapshot + KVM dirty logging on, write an INCREMENTAL snapshot: only
            // the pages dirtied since the parent, plus a pointer to the parent so
            // restore can replay base + diffs. Otherwise write a full snapshot and
            // turn dirty logging on so the NEXT snapshot can be a small diff. Using a
            // raw pointer to the guest mapping avoids borrowing `vm` so the chain
            // fields can be updated afterwards.
            let (mem_ptr, mem_bytes): (*const u8, usize) = match vm.guest_mem.as_ref() {
                Some(g) => {
                    let mem_bytes = usize::try_from(g.size_bytes)
                        .map_err(|_| VmmError::Memory("guest memory too large".into()))?;
                    (g.as_ptr(), mem_bytes)
                }
                None => match vm.mem_dump.as_ref() {
                    Some(d) => (d.as_ptr(), d.len()),
                    None => (std::ptr::null(), 0),
                },
            };
            let mem_slice: &[u8] = if mem_ptr.is_null() {
                &[]
            } else {
                // SAFETY: ptr+len describe a live mapping/Vec owned by `vm` for this
                // whole function; we only read it, and we mutate only other `vm` fields.
                unsafe { std::slice::from_raw_parts(mem_ptr, mem_bytes) }
            };
            let state_blob: Vec<u8> = vm.state_blob.clone().unwrap_or_default();

            #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
            let want_diff = diff && vm.last_snapshot.is_some() && vm.dirty_logging;
            #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
            let want_diff = false;

            if want_diff {
                #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
                {
                    let Some(running) = vm.running.as_ref() else {
                        return Err(VmmError::Snapshot(
                            "diff snapshot requested without a running KVM VM".into(),
                        ));
                    };
                    let mut dirty = running.kvm_vm.read_dirty()?;
                    if let Some(guest_mem) = vm.guest_mem.as_ref() {
                        let host_dirty = guest_mem.drain_host_dirty();
                        dirty.merge(&host_dirty);
                    }
                    let parent = vm.last_snapshot.clone().unwrap_or_default();
                    write_scratch_diff_snapshot_file(&path, &parent, &state_blob, mem_slice, &dirty)
                }
                #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
                {
                    Ok(0)
                }
            } else {
                write_scratch_snapshot_file(&path, &state_blob, mem_slice, false)?;
                // Enable dirty logging (idempotent) + drain the initial bitmap so the
                // next snapshot can diff against this full baseline.
                #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
                {
                    let enabled = match vm.running.as_ref() {
                        Some(r) => {
                            let ok = r.kvm_vm.enable_dirty_logging().is_ok();
                            if ok {
                                let _ = r.kvm_vm.read_dirty();
                            }
                            ok
                        }
                        None => false,
                    };
                    if enabled {
                        if let Some(guest_mem) = vm.guest_mem.as_ref() {
                            let _ = guest_mem.drain_host_dirty();
                        }
                        vm.dirty_logging = true;
                    }
                }
                Ok(mem_slice.len())
            }
        })();

        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        if paused_here && state_before == VmState::Running {
            resume_running_vcpus(vm);
        }

        vm.state = state_before;
        let mem_len = snapshot_result?;
        vm.last_snapshot = Some(path.clone());
        log::info!("VM: snapshot saved to {path} ({mem_len} bytes mem, diff={diff})");
        Ok(path)
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn create_live(&self, config: VmConfig) -> Result<()> {
        use crate::vcpu_thread::VcpuThread;
        use vmm_loader::load;
        use vmm_memory_backend::GuestMemory;

        config.validate()?;
        let mut slot = self.lock();
        if slot.is_some() {
            return Err(VmmError::InvalidConfig(
                "VM already exists (1:1 model — stop first)".into(),
            ));
        }

        let mem_size = config.memory.size_bytes()?;
        let mem = GuestMemory::new(mem_size).map_err(|e| VmmError::Memory(e.to_string()))?;
        let cmdline = if config.kernel.cmdline.is_empty() {
            vmm_loader::default_cmdline()
        } else {
            config.kernel.cmdline.clone()
        };

        // Running VMs always need an in-kernel IRQCHIP + PIT. Without it KVM has
        // no LAPIC to service the guest timer, so HLT exits to userspace and the
        // vCPU thread busy-spins at 100% CPU instead of blocking while idle
        // (confirmed against the KVM API docs §4.24/§4.38/§4.46: the irqchip
        // must exist before the guest relies on TSC-deadline timers).
        let full_boot = true;
        let overlay_guard = OwnedOverlayGuard::new(&config);

        // Build virtio-blk (+ virtio-net) devices via the shared helper so the
        // create path and the restore path (build_running_vm) can never drift
        // on the device / IRQ / MMIO / ACPI layout. virtio devices are
        // discovered via the ACPI DSDT only (a single `_CRS` Interrupt
        // descriptor), NOT the cmdline `virtio_mmio.device=`: the ACPI path
        // gives the guest a proper interrupt mapping so request_irq() succeeds
        // (a raw cmdline IRQ is not a mapped virq → request_irq -22; and
        // advertising via both makes the guest bind twice → -16 EBUSY).
        let WiredDevices {
            devices,
            acpi_devices,
            blks,
            blk_irq_evts,
            nets,
            rng_irq,
            vsock,
        } = build_devices(&config, &mem)?;
        let mut irq_evts: Vec<vmm_sys_util::eventfd::EventFd> = blk_irq_evts;

        let kernel_path = PathBuf::from(&config.kernel.path);
        if !kernel_path.exists() {
            return Err(VmmError::InvalidConfig(format!(
                "kernel not found: {}",
                kernel_path.display()
            )));
        }
        let loaded = load(
            &mem.inner,
            &kernel_path,
            &cmdline,
            config.kernel.initramfs.as_ref().map(PathBuf::from).as_ref(),
            mem.size_bytes,
        )
        .map_err(|e| VmmError::Loader(e.to_string()))?;
        crate::vcpu_setup::write_gdt(&mem).map_err(|e| VmmError::Device(e.to_string()))?;

        // Write ACPI tables (MADT + FADT + DSDT with virtio-mmio device entries).
        if full_boot {
            crate::vcpu_setup::write_acpi_tables_with_devices(
                &mem,
                config.vcpus.count,
                &acpi_devices,
            )?;
        }

        let template = crate::cpu_template::CpuTemplate::bare();
        let kvm_vm =
            crate::kvm::KvmVm::new_with_options(mem.clone(), devices, template, full_boot)?;

        // Register irqfds (no ioeventfds for block — QUEUE_NOTIFY traps to userspace).
        for (i, evt) in irq_evts.iter().enumerate() {
            let irq = 5 + i as u32;
            if let Err(e) = kvm_vm.register_irqfd(evt, irq) {
                log::warn!("irqfd for volume {i} (gsi={irq}): {e}");
            }
        }

        // i8042 irqfd for full boot. Kept alive in RunningVm.keep_alive_fds so
        // it survives for the VM's lifetime and is closed when the VM stops.
        if full_boot {
            let i8042_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
                .map_err(|e| VmmError::Kvm(format!("EventFd: {e}")))?;
            let _ = kvm_vm.register_irqfd(&i8042_evt, 1);
            irq_evts.push(i8042_evt);
        }

        let serial_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| VmmError::Kvm(format!("EventFd: {e}")))?;
        if let Err(e) = kvm_vm.register_irqfd(&serial_evt, 4) {
            log::warn!("serial irqfd (gsi=4): {e}");
        }
        let serial = Arc::new(vmm_devices::serial::Serial::new(
            serial_evt
                .try_clone()
                .map_err(|e| VmmError::Kvm(format!("EventFd clone: {e}")))?,
        ));
        irq_evts.push(serial_evt);

        // virtio-net: register an irqfd + a TX-queue ioeventfd per device and
        // spawn its host<->tap I/O loop. Unlike block (whose QUEUE_NOTIFY traps
        // to userspace), net uses an ioeventfd so the guest's TX kick lands on
        // the io thread instead of exiting the vCPU. NetIoLoop + Tap are kept
        // alive in RunningVm and dropped (loop first) on stop.
        let mut net_io_loops = Vec::new();
        let mut net_devices = Vec::new();
        let mut taps = Vec::new();
        for net in nets {
            let WiredNet {
                dev,
                tap,
                irq_evt,
                io_evt,
                irq,
                mmio_base,
            } = net;
            if let Err(e) = kvm_vm.register_irqfd(&irq_evt, irq) {
                log::warn!("net irqfd (gsi={irq}): {e}");
            }
            if let Err(e) = kvm_vm.register_ioeventfd(mmio_base + 0x50, &io_evt) {
                log::warn!("net ioeventfd at 0x{:x}: {e}", mmio_base + 0x50);
            }
            let tap_fd = tap.fd;
            let kick_fd = {
                use std::os::fd::AsRawFd;
                io_evt.as_raw_fd()
            };
            match vmm_devices::virtio::net_io_loop::spawn_net_io_loop(dev.clone(), tap_fd, kick_fd)
            {
                Ok(l) => net_io_loops.push(l),
                Err(e) => log::warn!("net io loop: {e}"),
            }
            net_devices.push(dev);
            irq_evts.push(irq_evt);
            irq_evts.push(io_evt);
            taps.push(tap);
        }

        // Register the virtio-rng completion irqfd (kept alive with the rest).
        if let Some((irq, evt)) = rng_irq {
            if let Err(e) = kvm_vm.register_irqfd(&evt, irq) {
                log::warn!("rng irqfd (gsi={irq}): {e}");
            }
            irq_evts.push(evt);
        }

        // Wire the virtio-vsock exec channel: register its irqfd, bind the
        // control socket the guest agent dials into, and start the host→guest
        // pump. Best-effort — on any failure exec transparently uses serial.
        let (vsock_pump, vsock_exec, vsock_pty) = match vsock {
            Some(wv) => {
                if let Err(e) = kvm_vm.register_irqfd(&wv.irq_evt, wv.irq) {
                    log::warn!("vsock irqfd (gsi={}): {e}", wv.irq);
                }
                irq_evts.push(wv.irq_evt);
                // TX QUEUE_NOTIFY → ioeventfd, so the guest's kick runs the TX
                // path (host socket connect/write) on the pump thread rather than
                // the seccomped vCPU thread (which would SIGSYS on connect()).
                // datamatch=1 = QUEUE_TX: only the TX kick routes here; RX/EVENT
                // (values 0/2) still trap to the vCPU, where they do no host I/O.
                if let Err(e) =
                    kvm_vm.register_ioeventfd_datamatch(wv.mmio_base + 0x50, &wv.io_evt, 1)
                {
                    log::warn!("vsock ioeventfd at 0x{:x}: {e}", wv.mmio_base + 0x50);
                }
                use std::os::fd::AsRawFd;
                let tx_kick_fd = wv.io_evt.as_raw_fd();
                let device = wv.device;
                let pump = vmm_devices::virtio::vsock_io_loop::spawn_vsock_pump(
                    device.clone(),
                    tx_kick_fd,
                )
                .ok();
                let pump_wake = pump.as_ref().and_then(|p| p.wake_evt().ok());
                let pty_wake = pump.as_ref().and_then(|p| p.wake_evt().ok());
                irq_evts.push(wv.io_evt);
                let exec = match crate::vsock_exec::VsockExecChannel::bind_with_pump_wake(
                    &wv.control_socket,
                    pump_wake,
                ) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        log::warn!("vsock exec bind {}: {e}", wv.control_socket.display());
                        None
                    }
                };
                let pty = pump
                    .as_ref()
                    .map(|_| crate::vsock_pty::VsockPtyChannel::new(device, pty_wake));
                (pump, exec, pty)
            }
            None => (None, None, None),
        };

        let vcpu = kvm_vm.create_vcpu(0)?;
        kvm_vm.setup_vcpu_for_bzimage_boot_full(&vcpu, &loaded, full_boot)?;
        let vcpu_thread = VcpuThread::spawn(vcpu, kvm_vm.mmio_bus.clone(), serial.clone());

        // SMP: create the application processors (vCPU ids 1..count). Each AP
        // gets CPUID (with its APIC id) + MP_STATE=UNINITIALIZED and its own
        // thread sharing the BSP's serial + MMIO bus; the guest BSP brings them
        // online via INIT/SIPI (handled by the in-kernel LAPIC).
        let mut ap_threads = Vec::new();
        for id in 1..config.vcpus.count {
            let ap = kvm_vm.create_vcpu(id)?;
            kvm_vm.setup_ap_vcpu(&ap, id)?;
            ap_threads.push(VcpuThread::spawn(
                ap,
                kvm_vm.mmio_bus.clone(),
                serial.clone(),
            ));
            log::info!("SMP: AP vCPU {id} created (UNINITIALIZED, awaiting SIPI)");
        }

        let state_blob = serialize_state_blob(
            loaded.entry,
            mem.size_bytes,
            &VcpuStateSave::default(),
            &config,
        );

        *slot = Some(VmInstance {
            state: VmState::Running,
            created_at: std::time::Instant::now(),
            last_snapshot: None,
            transient_files: VmTransientFiles::from_owned_overlay_paths(overlay_guard.disarm()),
            dirty_logging: false,
            config,
            guest_mem: Some(mem),
            state_blob: Some(state_blob),
            mem_dump: None,
            lazy_restore: None,
            running: Some(RunningVm {
                kvm_vm,
                vcpu_thread,
                ap_threads,
                loaded_entry: loaded.entry,
                net_io_loops,
                blk_devices: blks,
                net_devices,
                taps,
                vsock_pump,
                vsock_exec,
                vsock_pty,
                // Own the irqfd EventFds so they're closed on stop (no fd leak).
                keep_alive_fds: irq_evts,
            }),
        });

        log::info!("VM: created (live, full_boot={full_boot}) — vCPU executing in background");
        Ok(())
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
    pub fn create_live(&self, _config: VmConfig) -> Result<()> {
        Err(VmmError::Kvm("create_live needs Linux+KVM+boot".into()))
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn live_snapshot(
        &self,
        snapshot_config: crate::live_snapshot::LiveSnapshotConfig,
    ) -> Result<crate::live_snapshot::LiveSnapshotResult> {
        let (running, mem, state_blob) = {
            let mut slot = self.lock();
            let vm = slot
                .as_mut()
                .ok_or_else(|| VmmError::InvalidConfig("no VM".into()))?;
            // Phase A: live snapshot is uniprocessor-only (see snapshot()).
            if vm.config.vcpus.count > 1 {
                return Err(VmmError::Snapshot(
                    "SMP live snapshot (vcpus.count > 1) not yet supported".into(),
                ));
            }
            let running = vm
                .running
                .take()
                .ok_or_else(|| VmmError::InvalidConfig("VM not running".into()))?;
            let mem = vm
                .guest_mem
                .clone()
                .ok_or_else(|| VmmError::Memory("no guest memory".into()))?;
            let state_blob = vm.state_blob.clone().unwrap_or_default();
            (running, mem, state_blob)
        };

        let snap_result = crate::live_snapshot::live_snapshot(
            &running.kvm_vm.vm_fd,
            &mem,
            &running.kvm_vm.slots,
            &running.vcpu_thread,
            &snapshot_config,
        );

        {
            let mut slot = self.lock();
            if let Some(vm) = slot.as_mut() {
                vm.running = Some(running);
            }
        }

        let mut result = snap_result?;
        let live_path = unique_scratch_snapshot_path("vmm-live")?;
        let live_path_s = live_path.to_string_lossy().into_owned();
        if let Err(e) =
            write_scratch_snapshot_file(&live_path_s, &state_blob, &result.mem_snapshot, false)
        {
            let _ = std::fs::remove_file(&live_path);
            return Err(e);
        }
        result.snapshot_path = live_path_s.clone();
        let owned_live_path = OwnedScratchFile::remember(&live_path);
        {
            let mut slot = self.lock();
            if let Some(vm) = slot.as_mut() {
                vm.transient_files.set_live_snapshot_owned(owned_live_path);
            } else {
                remove_owned_scratch_file(&owned_live_path);
            }
        }
        log::info!(
            "VM: live snapshot — {} rounds, {} pages, {:?}",
            result.rounds,
            result.pages_copied,
            result.elapsed
        );
        Ok(result)
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn restore(&self, snapshot_path: &str, overlay: Option<String>) -> Result<()> {
        use std::time::Instant;

        let start = Instant::now();
        let restored = match try_lazy_restore_full_snapshot(snapshot_path) {
            Ok(Some(restored)) => restored,
            Ok(None) => {
                log::info!("restore: diff-chain tip detected; using eager snapshot replay");
                let (mem, state_blob) = load_snapshot_chain(snapshot_path)?;
                RestoredSnapshot {
                    mem,
                    state_blob,
                    lazy_restore: None,
                }
            }
            Err(e) => {
                log::warn!("restore: lazy restore unavailable ({e}); falling back to eager");
                let (mem, state_blob) = load_snapshot_chain(snapshot_path)?;
                RestoredSnapshot {
                    mem,
                    state_blob,
                    lazy_restore: None,
                }
            }
        };
        let RestoredSnapshot {
            mem,
            state_blob,
            lazy_restore,
        } = restored;
        let mem_len = usize::try_from(mem.size_bytes)
            .map_err(|_| VmmError::Memory("restored memory too large".into()))?;

        // Deserialize the state blob (shared owned `StateBlob`) to recover the
        // kernel path/cmdline/vcpus, the attached volumes/net, and any captured
        // vCPU state for a faithful resume.
        let mut saved = postcard::from_bytes::<StateBlob>(&state_blob).ok();

        let (kernel_path, cmdline, vcpus, volumes, net) = saved
            .as_ref()
            .map(|s| {
                (
                    s.kernel_path.clone(),
                    s.cmdline.clone(),
                    s.vcpus,
                    s.volumes.clone(),
                    s.net.clone(),
                )
            })
            .unwrap_or_else(|| (String::new(), String::new(), 1, vec![], vec![]));
        let vcpus = u8::try_from(vcpus).map_err(|_| {
            VmmError::InvalidConfig(format!("snapshot vcpu count too large: {vcpus}"))
        })?;

        // Recover the boot entry + the captured vCPU state (if this was a live
        // snapshot). With the full state we can reconstruct a *running* VM that
        // resumes exactly where it paused; without it we restore a paused,
        // memory-only image (the fast-boot / exec-via-fresh-boot fallback).
        let entry = saved.as_ref().map(|s| s.entry).unwrap_or(0);
        let vcpu_full: Option<crate::vcpu_setup::VcpuFullState> = saved
            .as_ref()
            .and_then(|s| s.vcpu_full.as_ref())
            .and_then(|bytes| postcard::from_bytes(bytes).ok());
        let vm_full: Option<crate::kvm::VmFullState> = saved
            .as_ref()
            .and_then(|s| s.vm_full.as_ref())
            .and_then(|bytes| postcard::from_bytes(bytes).ok());
        // AP vCPU states (SMP restore, phase B). Each entry is a postcard
        // VcpuFullState for AP id 1..N; empty for a uniprocessor snapshot.
        let ap_states: Vec<crate::vcpu_setup::VcpuFullState> = saved
            .as_ref()
            .map(|s| {
                s.vcpu_full_aps
                    .iter()
                    .filter_map(|bytes| postcard::from_bytes(bytes).ok())
                    .collect()
            })
            .unwrap_or_default();
        // UART register programming, so the restored serial re-arms the guest's
        // RX interrupt (post-restore exec fix). Default for pre-serial blobs.
        let serial_state = saved.as_ref().map(|s| s.serial.clone()).unwrap_or_default();
        let virtio_blk = saved
            .as_ref()
            .map(|s| s.virtio_blk.clone())
            .unwrap_or_default();
        let virtio_net = saved
            .as_ref()
            .map(|s| s.virtio_net.clone())
            .unwrap_or_default();
        let vsock_state = saved.as_ref().and_then(|s| s.vsock.clone());

        let mib = usize::try_from(crate::config::MIB).expect("MiB fits in usize");
        let mut config = VmConfig {
            kernel: crate::config::KernelConfig {
                path: kernel_path,
                cmdline,
                initramfs: None,
            },
            memory: crate::config::MemoryConfig {
                size_mib: u64::try_from(mem_len / mib).map_err(|_| {
                    VmmError::InvalidConfig("snapshot memory size too large".into())
                })?,
            },
            vcpus: crate::config::VcpuConfig { count: vcpus },
            volumes,
            net,
        };
        let overlay_seed = restore_overlay_seed(&config, overlay.as_deref())?;
        apply_restore_overlay(&mut config, overlay)?;
        config.validate()?;
        let overlay_guard = OwnedOverlayGuard::new(&config);
        if let Some((golden_overlay, clone_overlay)) = overlay_seed {
            seed_restore_overlay(&golden_overlay, &clone_overlay)?;
        }

        // Keep the owned state blob aligned with the restored config. Future
        // snapshots start from this blob and only patch in live device/vCPU
        // state, so leaving the golden overlay here would make clone snapshots
        // point back at the shared golden upper layer.
        let state_blob = if let Some(saved) = saved.as_mut() {
            saved.volumes = config.volumes.clone();
            postcard::to_allocvec(saved).unwrap_or_else(|_| state_blob.clone())
        } else {
            state_blob
        };

        // With captured vCPU state, rebuild a *running* VM (fresh KVM VM over the
        // restored memory + devices, the vCPU state re-applied, and the vCPU
        // thread resumed). Fall back to a paused image on any error so restore
        // never hard-fails.
        let (running, state) = match vcpu_full.as_ref() {
            Some(fs) => {
                match build_running_vm(
                    mem.clone(),
                    &config,
                    fs,
                    &ap_states,
                    vm_full.as_ref(),
                    &serial_state,
                    &virtio_blk,
                    &virtio_net,
                    vsock_state.as_ref(),
                    entry,
                ) {
                    Ok(r) => (Some(r), VmState::Running),
                    Err(e) => {
                        log::warn!(
                            "restore: could not reconstruct running VM ({e}); restoring paused"
                        );
                        (None, VmState::Paused)
                    }
                }
            }
            None => (None, VmState::Paused),
        };
        let resumed = running.is_some();

        let mut slot = self.lock();
        *slot = Some(VmInstance {
            state,
            created_at: std::time::Instant::now(),
            last_snapshot: None,
            transient_files: VmTransientFiles::from_owned_overlay_paths(overlay_guard.disarm()),
            dirty_logging: false,
            config,
            guest_mem: Some(mem),
            state_blob: Some(state_blob),
            mem_dump: None,
            lazy_restore,
            running,
        });
        log::info!(
            "VM restored in {:?} ({})",
            start.elapsed(),
            if resumed { "running" } else { "paused" }
        );
        Ok(())
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
    pub fn restore(&self, _snapshot_path: &str, _overlay: Option<String>) -> Result<()> {
        Err(VmmError::Snapshot("restore needs Linux+KVM+boot".into()))
    }

    pub fn suspend(&self) -> Result<()> {
        let mut slot = self.lock();
        let vm = slot
            .as_mut()
            .ok_or_else(|| VmmError::InvalidConfig("no VM".into()))?;

        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        {
            suspend_vm_in_place(vm)?;
            log::info!("VM suspended");
            Ok(())
        }

        #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
        {
            let _ = vm;
            Err(VmmError::Snapshot("suspend needs Linux+KVM+boot".into()))
        }
    }

    pub fn pause(&self) -> Result<()> {
        let mut slot = self.lock();
        let vm = slot
            .as_mut()
            .ok_or_else(|| VmmError::InvalidConfig("no VM".into()))?;
        // Actually stop the guest vCPU, not just flip the state enum — a paused
        // VM must stop consuming host CPU (a PaaS pauses idle VMs by the
        // thousand). snapshot() drives the thread directly; the API must too.
        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        pause_running_vcpus(vm);
        vm.state = VmState::Paused;
        log::info!("VM paused");
        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        let mut slot = self.lock();
        let vm = slot
            .as_mut()
            .ok_or_else(|| VmmError::InvalidConfig("no VM".into()))?;
        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        resume_running_vcpus(vm);
        vm.state = VmState::Running;
        log::info!("VM resumed");
        Ok(())
    }

    /// Return a cheap health/info snapshot of the VM (no guest interaction).
    /// Errors if no VM exists — that error is itself a valid response proving
    /// the serve process is alive to the orchestrator's health check.
    pub fn status(&self) -> Result<crate::state::VmStatus> {
        let slot = self.lock();
        let vm = slot
            .as_ref()
            .ok_or_else(|| VmmError::InvalidConfig("no VM (boot first)".into()))?;

        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        let vcpu_alive = vm
            .running
            .as_ref()
            .map(|r| !r.vcpu_thread.is_exited() && r.ap_threads.iter().all(|ap| !ap.is_exited()))
            .unwrap_or(false);
        #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
        let vcpu_alive = false;

        Ok(crate::state::VmStatus {
            state: vm.state,
            uptime_ms: vm.created_at.elapsed().as_millis() as u64,
            vcpus: vm.config.vcpus.count,
            mem_mib: vm.config.memory.size_mib,
            volumes: vm.config.volumes.len(),
            nets: vm.config.net.len(),
            kernel: vm.config.kernel.path.clone(),
            vcpu_alive,
        })
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn attach_pty(
        &self,
        host_stream: std::os::unix::net::UnixStream,
        cols: u16,
        rows: u16,
        shell: Option<String>,
    ) -> Result<()> {
        let pty = {
            let slot = self.lock();
            slot.as_ref()
                .and_then(|vm| vm.running.as_ref())
                .and_then(|r| r.vsock_pty.clone())
        }
        .ok_or_else(|| VmmError::Device("vsock PTY channel unavailable".into()))?;

        pty.attach(host_stream, cols, rows, shell)
            .map_err(VmmError::Device)
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
    pub fn attach_pty(
        &self,
        _host_stream: std::os::unix::net::UnixStream,
        _cols: u16,
        _rows: u16,
        _shell: Option<String>,
    ) -> Result<()> {
        Err(VmmError::Kvm("AttachPty needs Linux+KVM+boot".into()))
    }

    ///
    /// The VM must have been created with a rootfs that runs the VMM guest
    /// agent (which reads from /dev/ttyS0). The controller sends the command
    /// via the serial channel and waits for the `VMM_EXEC_EXIT=` marker.
    ///
    /// If the VM is not running (no background vCPU thread), falls back to
    /// booting a fresh VM with the command in the cmdline.
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn exec(&self, command: &str, timeout_ms: u64) -> Result<(i32, String, u64)> {
        use std::time::{Duration, Instant};

        let start = Instant::now();
        let timeout = Duration::from_millis(if timeout_ms > 0 { timeout_ms } else { 30000 });

        // Prefer the vsock exec channel when the guest agent has dialed in: it's
        // a dedicated framed stream, so exec never desyncs against ttyS0 console
        // output the way serial does under concurrent IRQ load (validated: 25/25
        // rapid execs + multi-line output clean over vsock). Falls back to serial
        // when the guest hasn't dialed vsock (older agent / no device) or on any
        // vsock error. Opt out with VMM_VSOCK_EXEC=0. Clone the Arc and drop the
        // controller lock before the (blocking) exec so other API calls aren't
        // stalled.
        if std::env::var("VMM_VSOCK_EXEC").as_deref() != Ok("0") {
            let vsock_channel = {
                let slot = self.lock();
                slot.as_ref()
                    .and_then(|vm| vm.running.as_ref())
                    .and_then(|r| r.vsock_exec.clone())
            };
            if let Some(vx) = vsock_channel {
                match vx.exec(command, timeout) {
                    Some(Ok(r)) => {
                        log::info!("exec '{command}' via vsock → exit={}", r.0);
                        return Ok(r);
                    }
                    Some(Err(e)) => {
                        log::warn!("vsock exec failed ({e}); falling back to serial");
                    }
                    None => {} // no guest connection / disabled → serial
                }
            }
        }

        // Check if we have a running VM with a serial channel.
        let serial_handle = {
            let slot = self.lock();
            slot.as_ref()
                .and_then(|vm| vm.running.as_ref())
                .map(|r| r.vcpu_thread.serial.clone())
        };

        if let Some(serial) = serial_handle {
            // Discard any stale output left in the channel by a previous exec
            // (e.g. one that timed out) so it can't be misread as this command's
            // response.
            let _ = serial.drain_output();

            // Real exec: send command to the running VM's serial port.
            let cmd = format!("VMM_EXEC:{command}");
            serial.send(cmd.as_bytes());

            // Wait for VMM_EXEC_START, then capture until VMM_EXEC_EXIT=.
            //
            // Accumulate raw bytes and only parse *complete* lines (up to the
            // last newline), keeping any partial trailing line for the next
            // iteration. drain_output() returns whatever bytes are buffered, so
            // for a slow or chatty command the VMM_EXEC_EXIT= marker can be
            // split across two drains; parsing each chunk independently would
            // miss it and hang until timeout.
            let mut acc: Vec<u8> = Vec::new();
            let mut output = Vec::new();
            let mut truncated = false;
            let mut started = false;

            while start.elapsed() < timeout {
                // Poll the serial output buffer frequently while an exec is in
                // flight so the VMM_EXEC_EXIT= marker is detected promptly. This
                // loop only runs during an active serial-fallback exec, so the
                // tighter interval does not affect idle CPU.
                std::thread::sleep(Duration::from_millis(2));
                let chunk = serial.drain_output();
                if chunk.is_empty() {
                    continue;
                }
                acc.extend_from_slice(&chunk);
                while let Some(pos) = acc.iter().position(|&b| b == b'\n') {
                    let mut line: Vec<u8> = acc.drain(..=pos).collect();
                    line.pop(); // drop the '\n'
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    let line_str = String::from_utf8_lossy(&line);
                    if line_str == "VMM_EXEC_START" {
                        started = true;
                        continue;
                    }
                    if let Some(code) = line_str.strip_prefix("VMM_EXEC_EXIT=") {
                        let exit_code: i32 = code.trim().parse().unwrap_or(0);
                        let duration_ms = start.elapsed().as_millis() as u64;
                        let output_str = finish_exec_output(output, truncated);
                        log::info!("exec: '{command}' → exit={exit_code} {duration_ms}ms");
                        return Ok((exit_code, output_str, duration_ms));
                    }
                    if started {
                        append_exec_output(&mut output, &line, &mut truncated);
                        append_exec_output(&mut output, b"\n", &mut truncated);
                    }
                }
                trim_exec_accumulator(&mut acc, started, &mut output, &mut truncated);
            }

            let duration_ms = start.elapsed().as_millis() as u64;
            let output_str = finish_exec_output(output, truncated);
            if started {
                log::warn!("exec: timed out after {duration_ms}ms");
                return Ok((-1, output_str, duration_ms));
            }
            return Err(VmmError::Kvm(format!(
                "exec timed out after {timeout:?} — no response from guest agent"
            )));
        }

        // Fallback: boot a fresh VM with the command in cmdline.
        self.exec_fresh_boot(command, timeout_ms)
    }

    /// Fallback exec: boot a fresh VM with the command baked into cmdline.
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    fn exec_fresh_boot(&self, command: &str, timeout_ms: u64) -> Result<(i32, String, u64)> {
        use std::time::Instant;
        use vmm_loader::load;
        use vmm_memory_backend::GuestMemory;

        let config = {
            let slot = self.lock();
            slot.as_ref()
                .map(|vm| vm.config.clone())
                .ok_or_else(|| VmmError::InvalidConfig("no VM (create first)".into()))?
        };
        config.validate()?;

        let start = Instant::now();
        let timeout_secs = if timeout_ms > 0 {
            timeout_ms / 1000
        } else {
            10
        };
        std::env::set_var("VMM_BOOT_TIMEOUT", timeout_secs.to_string());

        let mem_size = config.memory.size_bytes()?;
        let mem = GuestMemory::new(mem_size).map_err(|e| VmmError::Memory(e.to_string()))?;
        let mut cmdline = if config.kernel.cmdline.is_empty() {
            vmm_loader::default_cmdline()
        } else {
            config.kernel.cmdline.clone()
        };
        cmdline.push_str(&format!(" vmm.cmd=\"{command}\""));

        let kernel_path = PathBuf::from(&config.kernel.path);
        let initramfs_path = config.kernel.initramfs.as_ref().map(PathBuf::from);
        let loaded = load(
            &mem.inner,
            &kernel_path,
            &cmdline,
            initramfs_path.as_ref(),
            mem.size_bytes,
        )
        .map_err(|e| VmmError::Loader(e.to_string()))?;
        crate::vcpu_setup::write_gdt(&mem).map_err(|e| VmmError::Device(e.to_string()))?;
        let template = crate::cpu_template::CpuTemplate::bare();
        let vm = crate::kvm::KvmVm::new_with_options(mem.clone(), vec![], template, false)?;
        let mut vcpu = vm.create_vcpu(0)?;
        vm.setup_vcpu_for_bzimage_boot_full(&vcpu, &loaded, false)?;
        vm.run_vcpu(&mut vcpu)?;

        let duration_ms = start.elapsed().as_millis() as u64;
        log::info!("exec (fresh boot): '{command}' → {duration_ms}ms");
        Ok((0, String::new(), duration_ms))
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
    pub fn exec(&self, _command: &str, _timeout_ms: u64) -> Result<(i32, String, u64)> {
        Err(VmmError::Kvm("exec needs Linux+KVM+boot".into()))
    }

    /// Stop the VM and clear the slot.
    pub fn stop(&self) -> Result<()> {
        let mut slot = self.lock();
        if let Some(vm) = slot.as_mut() {
            #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
            stop_running_vm(vm);
            vm.state = VmState::Stopped;
            log::info!("VM stopped");
        }
        *slot = None;
        Ok(())
    }
}

impl Default for VmmController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn append_exec_output(output: &mut Vec<u8>, bytes: &[u8], truncated: &mut bool) {
    if *truncated || bytes.is_empty() {
        return;
    }
    let remaining = EXEC_OUTPUT_PAYLOAD_CAP.saturating_sub(output.len());
    if bytes.len() <= remaining {
        output.extend_from_slice(bytes);
        return;
    }
    output.extend_from_slice(&bytes[..remaining]);
    *truncated = true;
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn trim_exec_accumulator(
    acc: &mut Vec<u8>,
    started: bool,
    output: &mut Vec<u8>,
    truncated: &mut bool,
) {
    if acc.len() <= EXEC_ACC_TAIL_CAP {
        return;
    }
    let drain_len = acc.len() - EXEC_ACC_TAIL_CAP;
    let drained: Vec<u8> = acc.drain(..drain_len).collect();
    if started {
        append_exec_output(output, &drained, truncated);
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn finish_exec_output(mut output: Vec<u8>, truncated: bool) -> String {
    if truncated {
        output.extend_from_slice(EXEC_OUTPUT_TRUNCATED);
    }
    String::from_utf8_lossy(&output).to_string()
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn stop_running_vm(vm: &mut VmInstance) {
    if let Some(mut running) = vm.running.take() {
        if let Err(e) = running.vcpu_thread.stop() {
            log::warn!("VM: vCPU thread stop returned: {e}");
        }
        // Stop the AP vCPU threads (SMP). Draining consumes each thread,
        // which signals + joins it.
        for ap in running.ap_threads.drain(..) {
            if let Err(e) = ap.stop() {
                log::warn!("VM: AP vCPU thread stop returned: {e}");
            }
        }
        // Stop the net I/O threads before their EventFds/taps drop.
        for l in running.net_io_loops.iter_mut() {
            l.stop();
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn pause_running_vcpus(vm: &VmInstance) -> bool {
    if let Some(r) = vm.running.as_ref() {
        let mut any = false;
        if !r.vcpu_thread.is_exited() {
            r.vcpu_thread.pause();
            any = true;
        }
        for ap in &r.ap_threads {
            if !ap.is_exited() {
                ap.pause();
                any = true;
            }
        }
        any
    } else {
        false
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn resume_running_vcpus(vm: &VmInstance) {
    if let Some(r) = vm.running.as_ref() {
        if !r.vcpu_thread.is_exited() {
            r.vcpu_thread.resume();
        }
        for ap in &r.ap_threads {
            if !ap.is_exited() {
                ap.resume();
            }
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn capture_live_state(vm: &mut VmInstance) {
    let captured = vm.running.as_ref().and_then(|r| {
        r.vcpu_thread
            .captured_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    });
    let ap_captured: Option<Vec<Vec<u8>>> = vm.running.as_ref().and_then(|r| {
        let mut v = Vec::with_capacity(r.ap_threads.len());
        for ap in &r.ap_threads {
            match ap
                .captured_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                Some(s) => v.push(s),
                None => return None,
            }
        }
        Some(v)
    });
    let vm_state = vm
        .running
        .as_ref()
        .and_then(|r| r.kvm_vm.capture_vm_state().ok())
        .and_then(|s| postcard::to_allocvec(&s).ok());
    let serial_state = vm
        .running
        .as_ref()
        .map(|r| vmm_devices::persist::Persist::save(&*r.vcpu_thread.serial));
    let (virtio_blk, virtio_net) = vm
        .running
        .as_ref()
        .map(|r| {
            (
                capture_virtio_blk_states(&r.blk_devices),
                capture_virtio_net_states(&r.net_devices),
            )
        })
        .unwrap_or_default();
    let vsock_state = vm
        .running
        .as_ref()
        .and_then(|r| r.vsock_pump.as_ref())
        .map(|p| vmm_devices::persist::Persist::save(&*p.device));
    if let Some(full) = captured {
        if let Some(existing) = vm.state_blob.as_deref() {
            if let Ok(mut b) = postcard::from_bytes::<StateBlob>(existing) {
                b.vcpu_full = Some(full);
                b.vcpu_full_aps = ap_captured.unwrap_or_default();
                b.vm_full = vm_state;
                if let Some(s) = serial_state {
                    b.serial = s;
                }
                b.virtio_blk = virtio_blk;
                b.virtio_net = virtio_net;
                b.vsock = vsock_state;
                vm.state_blob = Some(postcard::to_allocvec(&b).unwrap_or_default());
            }
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn suspend_vm_in_place(vm: &mut VmInstance) -> Result<()> {
    if vm.state == VmState::Suspended {
        return Ok(());
    }

    let state_before = vm.state;
    let paused_here = pause_running_vcpus(vm);
    vm.state = VmState::Paused;
    let result = (|| -> Result<()> {
        capture_live_state(vm);

        let guest_mem = vm
            .guest_mem
            .as_ref()
            .ok_or_else(|| VmmError::Memory("no guest memory to suspend".into()))?;
        let mem_ptr = guest_mem.as_ptr() as *mut u8;
        let mem_len: usize = guest_mem
            .size_bytes
            .try_into()
            .map_err(|_| VmmError::Memory("guest memory too large".into()))?;
        let host_dirty = guest_mem.host_dirty_tracker();
        let state_blob = vm.state_blob.clone().unwrap_or_default();
        let state_len = u64::try_from(state_blob.len())
            .map_err(|_| VmmError::Snapshot("state blob too large".into()))?;
        let mem_len_u64 = u64::try_from(mem_len)
            .map_err(|_| VmmError::Snapshot("memory image too large".into()))?;
        let layout = full_snapshot_layout_for_lengths(state_len, mem_len_u64)
            .ok_or_else(|| VmmError::Snapshot("suspend file layout overflow".into()))?;
        let path = unique_suspend_snapshot_path()?;

        let mem_slice = {
            // SAFETY: guest_mem owns this mmap for the lifetime of `vm`; the vCPUs are
            // paused, and this read also resolves any older lazy-restore faults before
            // we unregister the previous UFFD below.
            unsafe { std::slice::from_raw_parts(mem_ptr.cast_const(), mem_len) }
        };
        if let Err(e) = write_scratch_snapshot_file(&path, &state_blob, mem_slice, false) {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }

        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(VmmError::Snapshot(format!("open {path}: {e}")));
            }
        };
        if let Err(e) = file.sync_all() {
            let _ = std::fs::remove_file(&path);
            return Err(VmmError::Snapshot(format!("sync {path}: {e}")));
        }

        // The previous lazy restore (from restore or an older suspend) must stay
        // active until the full memory image has been copied above. Drop it only now
        // so the same range can be registered on the suspend image.
        vm.lazy_restore = None;

        let lazy_restore = match vmm_memory_backend::start_lazy_restore_in_place(
            mem_ptr,
            mem_len,
            &file,
            layout.mem_offset,
            layout.mem_len,
            Some(host_dirty),
        ) {
            Ok(lazy_restore) => lazy_restore,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(VmmError::Snapshot(format!("UFFD suspend restore: {e}")));
            }
        };

        if let Err(e) = vmm_memory_backend::madvise_dontneed(mem_ptr, mem_len) {
            drop(lazy_restore);
            let _ = std::fs::remove_file(&path);
            return Err(VmmError::Snapshot(format!("release guest RAM: {e}")));
        }
        drop_file_cache(&file, layout.mem_offset, layout.mem_len);
        let tracked_path = std::fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
        vm.transient_files.set_suspend_snapshot(tracked_path);
        vm.lazy_restore = Some(lazy_restore);
        vm.state = VmState::Suspended;

        log::info!(
            "VM: suspend image armed at {path} ({} bytes guest RAM released)",
            layout.mem_len
        );
        Ok(())
    })();

    if result.is_err() {
        vm.state = state_before;
        if paused_here && state_before == VmState::Running {
            resume_running_vcpus(vm);
        }
    }
    result
}

fn private_runtime_dir() -> Result<PathBuf> {
    // Use the system temp dir (disk-backed, large enough for multi-hundred-MB
    // snapshots), under a private per-process 0700 subdir. Never the CWD/source
    // tree, and never a small runtime tmpfs like XDG_RUNTIME_DIR (/run/user),
    // which fills up when large snapshots are written to it.
    let dir = std::env::temp_dir()
        .join(".vmm-runtime")
        .join(format!("vmm-{}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| VmmError::Snapshot(format!("create runtime dir {}: {e}", dir.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(&dir)
            .map_err(|e| VmmError::Snapshot(format!("stat runtime dir {}: {e}", dir.display())))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(VmmError::Snapshot(format!(
                "runtime path is not a directory: {}",
                dir.display()
            )));
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| VmmError::Snapshot(format!("chmod runtime dir {}: {e}", dir.display())))?;
    }
    Ok(dir)
}

fn unique_scratch_snapshot_path(prefix: &str) -> Result<PathBuf> {
    unique_runtime_file_path(prefix, "snap")
}

fn unique_runtime_file_path(prefix: &str, suffix: &str) -> Result<PathBuf> {
    static SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(private_runtime_dir()?.join(format!(
        "{prefix}-{}-{ts}-{seq}.{suffix}",
        std::process::id()
    )))
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn unique_suspend_snapshot_path() -> Result<String> {
    Ok(unique_scratch_snapshot_path(".vmm-suspend")?
        .to_string_lossy()
        .into_owned())
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn full_snapshot_layout_for_lengths(state_len: u64, mem_len: u64) -> Option<FullSnapshotLayout> {
    let mem_offset = FULL_SNAPSHOT_HEADER_LEN.checked_add(state_len)?;
    mem_offset.checked_add(mem_len)?;
    Some(FullSnapshotLayout {
        state_offset: FULL_SNAPSHOT_HEADER_LEN,
        state_len,
        mem_offset,
        mem_len,
    })
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn drop_file_cache(file: &std::fs::File, offset: u64, len: u64) {
    use std::os::fd::AsRawFd;

    let Ok(offset) = libc::off_t::try_from(offset) else {
        log::warn!("suspend: cannot fadvise file cache; offset too large");
        return;
    };
    let Ok(len) = libc::off_t::try_from(len) else {
        log::warn!("suspend: cannot fadvise file cache; length too large");
        return;
    };
    // SAFETY: `file.as_raw_fd()` is a valid open fd, and offset/len were checked
    // to fit `off_t`; posix_fadvise does not dereference Rust pointers.
    let rc =
        unsafe { libc::posix_fadvise(file.as_raw_fd(), offset, len, libc::POSIX_FADV_DONTNEED) };
    if rc != 0 {
        log::warn!(
            "suspend: POSIX_FADV_DONTNEED failed: {}",
            std::io::Error::from_raw_os_error(rc)
        );
    }
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn apply_restore_overlay(config: &mut VmConfig, overlay: Option<String>) -> Result<()> {
    let Some(overlay) = overlay else {
        return Ok(());
    };

    let Some(volume) = select_restore_overlay_volume(&mut config.volumes) else {
        return Err(VmmError::InvalidConfig(
            "restore overlay requested but snapshot has no volumes".into(),
        ));
    };

    // A snapshot records the golden VM's VolumeConfig. For a restored clone,
    // never reuse that saved upper layer: keep `path` as the immutable lower
    // image and install the per-restore sparse CoW overlay as the writable upper
    // layer. If the golden used a direct rw disk, open_cow still reopens `path`
    // read-only and redirects clone writes into this fresh overlay.
    volume.overlay = Some(overlay);
    Ok(())
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn restore_overlay_seed(
    config: &VmConfig,
    overlay: Option<&str>,
) -> Result<Option<(PathBuf, PathBuf)>> {
    let Some(target) = overlay else {
        return Ok(None);
    };
    let Some(index) = restore_overlay_volume_index(&config.volumes) else {
        return Err(VmmError::InvalidConfig(
            "restore overlay requested but snapshot has no volumes".into(),
        ));
    };
    let Some(source) = config.volumes[index].overlay.as_deref() else {
        return Ok(None);
    };
    if source == target {
        return Ok(None);
    }
    Ok(Some((PathBuf::from(source), PathBuf::from(target))))
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn seed_restore_overlay(source: &Path, target: &Path) -> Result<()> {
    if source == target {
        return Ok(());
    }

    copy_restore_overlay(source, target).map_err(|e| {
        let _ = std::fs::remove_file(target);
        VmmError::Snapshot(format!(
            "seed restore overlay {} -> {}: {e}",
            source.display(),
            target.display()
        ))
    })
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn copy_restore_overlay(source: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        copy_sparse_restore_overlay(source, target)
    }

    #[cfg(not(target_os = "linux"))]
    {
        copy_dense_restore_overlay(source, target)
    }
}

#[cfg(all(
    not(target_os = "linux"),
    any(test, all(target_arch = "x86_64", feature = "boot"))
))]
fn copy_dense_restore_overlay(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};

    let mut source = std::fs::File::open(source)?;
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    source.seek(SeekFrom::Start(0))?;
    target.seek(SeekFrom::Start(0))?;
    std::io::copy(&mut source, &mut target)?;
    target.sync_all()
}

#[cfg(all(
    target_os = "linux",
    any(test, all(target_arch = "x86_64", feature = "boot"))
))]
fn copy_sparse_restore_overlay(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;

    let mut source = std::fs::File::open(source)?;
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let length = source.metadata()?.len();
    target.set_len(length)?;

    let mut offset = 0u64;
    while offset < length {
        let data = unsafe {
            libc::lseek(
                source.as_raw_fd(),
                offset.try_into().map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "overlay offset too large",
                    )
                })?,
                libc::SEEK_DATA,
            )
        };
        if data < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            if matches!(
                error.raw_os_error(),
                Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
            ) {
                target.set_len(0)?;
                source.seek(SeekFrom::Start(0))?;
                target.seek(SeekFrom::Start(0))?;
                std::io::copy(&mut source, &mut target)?;
                return target.sync_all();
            }
            return Err(error);
        }
        let data = u64::try_from(data).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "negative overlay data offset",
            )
        })?;
        let hole = unsafe {
            libc::lseek(
                source.as_raw_fd(),
                data.try_into().map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "overlay offset too large",
                    )
                })?,
                libc::SEEK_HOLE,
            )
        };
        if hole < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let hole = u64::try_from(hole).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "negative overlay hole offset",
            )
        })?;
        if hole < data || hole > length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid overlay data extent",
            ));
        }

        source.seek(SeekFrom::Start(data))?;
        target.seek(SeekFrom::Start(data))?;
        let mut remaining = hole.saturating_sub(data);
        let mut buffer = [0u8; 64 * 1024];
        while remaining > 0 {
            let wanted = usize::try_from(remaining.min(buffer.len() as u64))
                .expect("bounded copy length fits usize");
            let read = source.read(&mut buffer[..wanted])?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "overlay data extent ended early",
                ));
            }
            target.write_all(&buffer[..read])?;
            remaining -= read as u64;
        }
        offset = hole;
    }
    target.sync_all()
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn select_restore_overlay_volume(
    volumes: &mut [crate::config::VolumeConfig],
) -> Option<&mut crate::config::VolumeConfig> {
    let index = restore_overlay_volume_index(volumes)?;
    volumes.get_mut(index)
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn restore_overlay_volume_index(volumes: &[crate::config::VolumeConfig]) -> Option<usize> {
    // If the golden was already CoW, that volume is the rootfs upper layer we
    // must replace. Otherwise fall back to volume 0: configs attach rootfs first.
    let index = volumes
        .iter()
        .position(|vol| vol.overlay.is_some())
        .unwrap_or(0);
    volumes.get(index).map(|_| index)
}

/// Write a snapshot file with CRC32 integrity.
///
/// Layout: `[4B magic "VMSN"][2B version LE][2B flags LE][8B state_len LE]
/// [4B state_crc LE][8B mem_len LE][4B mem_crc LE][state_blob][mem_dump]`.
#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
#[allow(dead_code)]
pub(crate) fn write_snapshot_file(
    path: &str,
    state_blob: &[u8],
    mem_dump: &[u8],
    diff: bool,
) -> Result<()> {
    write_snapshot_file_with_mode(
        path,
        state_blob,
        mem_dump,
        diff,
        SnapshotCreateMode::Truncate,
    )
}

fn write_scratch_snapshot_file(
    path: &str,
    state_blob: &[u8],
    mem_dump: &[u8],
    diff: bool,
) -> Result<()> {
    write_snapshot_file_with_mode(
        path,
        state_blob,
        mem_dump,
        diff,
        SnapshotCreateMode::CreateNewPrivate,
    )
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum SnapshotCreateMode {
    Truncate,
    CreateNewPrivate,
}

fn open_snapshot_output(path: &str, mode: SnapshotCreateMode) -> Result<std::fs::File> {
    match mode {
        SnapshotCreateMode::Truncate => std::fs::File::create(path)
            .map_err(|e| VmmError::Snapshot(format!("create {path}: {e}"))),
        SnapshotCreateMode::CreateNewPrivate => {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            options
                .open(std::path::Path::new(path))
                .map_err(|e| VmmError::Snapshot(format!("create {path}: {e}")))
        }
    }
}

fn write_snapshot_file_with_mode(
    path: &str,
    state_blob: &[u8],
    mem_dump: &[u8],
    diff: bool,
    mode: SnapshotCreateMode,
) -> Result<()> {
    use std::io::Write;
    const MAGIC: &[u8; 4] = b"VMSN";
    const VERSION: u16 = 1;

    let state_crc = crc32fast::hash(state_blob);
    let mem_crc = crc32fast::hash(mem_dump);
    let flags: u16 = if diff { 1 } else { 0 };
    let state_len = u64::try_from(state_blob.len())
        .map_err(|_| VmmError::Snapshot("state blob too large".into()))?;
    let mem_len = u64::try_from(mem_dump.len())
        .map_err(|_| VmmError::Snapshot("memory image too large".into()))?;

    let mut file = open_snapshot_output(path, mode)?;
    file.write_all(MAGIC)
        .map_err(|e| VmmError::Snapshot(e.to_string()))?;
    file.write_all(&VERSION.to_le_bytes())
        .map_err(|e| VmmError::Snapshot(e.to_string()))?;
    file.write_all(&flags.to_le_bytes())
        .map_err(|e| VmmError::Snapshot(e.to_string()))?;
    file.write_all(&state_len.to_le_bytes())
        .map_err(|e| VmmError::Snapshot(e.to_string()))?;
    file.write_all(&state_crc.to_le_bytes())
        .map_err(|e| VmmError::Snapshot(e.to_string()))?;
    file.write_all(&mem_len.to_le_bytes())
        .map_err(|e| VmmError::Snapshot(e.to_string()))?;
    file.write_all(&mem_crc.to_le_bytes())
        .map_err(|e| VmmError::Snapshot(e.to_string()))?;
    file.write_all(state_blob)
        .map_err(|e| VmmError::Snapshot(e.to_string()))?;
    file.write_all(mem_dump)
        .map_err(|e| VmmError::Snapshot(e.to_string()))?;
    file.flush()
        .map_err(|e| VmmError::Snapshot(e.to_string()))?;
    Ok(())
}

/// Write an incremental (diff) snapshot: only the pages dirtied since the parent
/// snapshot, plus a pointer to the parent so restore can replay base + diffs.
///
/// Layout: `[4B "VMSD"][2B version][4B parent_len][parent path]
/// [8B state_len][4B state_crc][state_blob][4B n_pages]
/// (n_pages × [8B gpa][4B len][page bytes])`. Returns the diff payload size.
#[cfg(all(test, target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn write_diff_snapshot_file(
    path: &str,
    parent: &str,
    state_blob: &[u8],
    mem: &[u8],
    dirty: &vmm_memory_backend::dirty::DirtyBitmap,
) -> Result<usize> {
    write_diff_snapshot_file_with_mode(
        path,
        parent,
        state_blob,
        mem,
        dirty,
        SnapshotCreateMode::Truncate,
    )
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn write_scratch_diff_snapshot_file(
    path: &str,
    parent: &str,
    state_blob: &[u8],
    mem: &[u8],
    dirty: &vmm_memory_backend::dirty::DirtyBitmap,
) -> Result<usize> {
    write_diff_snapshot_file_with_mode(
        path,
        parent,
        state_blob,
        mem,
        dirty,
        SnapshotCreateMode::CreateNewPrivate,
    )
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn write_diff_snapshot_file_with_mode(
    path: &str,
    parent: &str,
    state_blob: &[u8],
    mem: &[u8],
    dirty: &vmm_memory_backend::dirty::DirtyBitmap,
    mode: SnapshotCreateMode,
) -> Result<usize> {
    use std::io::Write;
    use vmm_snapshot::diff::build_diff;
    const MAGIC: &[u8; 4] = b"VMSD";
    const VERSION: u16 = 1;

    let diff = build_diff(mem, dirty, Vec::new());
    let mut file = open_snapshot_output(path, mode)?;
    let mut wr = |b: &[u8]| -> Result<()> {
        file.write_all(b)
            .map_err(|e| VmmError::Snapshot(e.to_string()))
    };
    wr(MAGIC)?;
    wr(&VERSION.to_le_bytes())?;
    let pbytes = parent.as_bytes();
    let parent_len = u32::try_from(pbytes.len())
        .map_err(|_| VmmError::Snapshot("parent path too long".into()))?;
    let state_len = u64::try_from(state_blob.len())
        .map_err(|_| VmmError::Snapshot("state blob too large".into()))?;
    let page_count = u32::try_from(diff.pages.len())
        .map_err(|_| VmmError::Snapshot("too many diff pages".into()))?;
    wr(&parent_len.to_le_bytes())?;
    wr(pbytes)?;
    wr(&state_len.to_le_bytes())?;
    wr(&crc32fast::hash(state_blob).to_le_bytes())?;
    wr(state_blob)?;
    wr(&page_count.to_le_bytes())?;
    let mut total = 0usize;
    for p in &diff.pages {
        wr(&p.gpa.to_le_bytes())?;
        let page_len = u32::try_from(p.bytes.len())
            .map_err(|_| VmmError::Snapshot("diff page too large".into()))?;
        wr(&page_len.to_le_bytes())?;
        wr(&p.bytes)?;
        total = total
            .checked_add(p.bytes.len())
            .ok_or_else(|| VmmError::Snapshot("diff payload length overflow".into()))?;
    }
    file.flush()
        .map_err(|e| VmmError::Snapshot(e.to_string()))?;
    Ok(total)
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
const FULL_SNAPSHOT_HEADER_LEN: u64 = 32;
#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
const FULL_SNAPSHOT_REST_HEADER_LEN: usize = 28;
#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
const FULL_SNAPSHOT_DIFF_FLAG: u16 = 1;
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const SNAPSHOT_VERSION: u16 = 1;
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const MAX_SNAPSHOT_STATE_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const MAX_DIFF_PARENT_PATH_BYTES: u64 = 4096;
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const MAX_DIFF_PAGE_BYTES: u64 = 1024 * 1024;
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const MAX_DIFF_PAGES: usize = 1 << 20;
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const MAX_DIFF_CHAIN_DEPTH: usize = 1024;
#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
const MAX_EAGER_DIFF_BYTES: u64 = 256 * 1024 * 1024;

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn validate_diff_page_range(gpa: u64, len: usize, mem_bytes: usize) -> Result<()> {
    let start =
        usize::try_from(gpa).map_err(|_| VmmError::Snapshot("diff page GPA too large".into()))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| VmmError::Snapshot("diff page range overflow".into()))?;
    if end > mem_bytes {
        return Err(VmmError::Snapshot(format!(
            "diff page outside base guest memory: 0x{start:x}..0x{end:x} > 0x{mem_bytes:x}"
        )));
    }
    Ok(())
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn validate_diff_payload_budget(total_so_far: u64, next_len: usize) -> Result<u64> {
    let next =
        u64::try_from(next_len).map_err(|_| VmmError::Snapshot("diff page too large".into()))?;
    let total = total_so_far
        .checked_add(next)
        .ok_or_else(|| VmmError::Snapshot("diff payload length overflow".into()))?;
    if total > MAX_EAGER_DIFF_BYTES {
        return Err(VmmError::Snapshot(format!(
            "diff payload too large: {total} bytes > {MAX_EAGER_DIFF_BYTES}"
        )));
    }
    Ok(total)
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FullSnapshotLayout {
    state_offset: u64,
    state_len: u64,
    mem_offset: u64,
    mem_len: u64,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FullSnapshotHeader {
    flags: u16,
    layout: FullSnapshotLayout,
    state_crc: u32,
    mem_crc: u32,
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotFileKind {
    LazyFull(FullSnapshotLayout),
    EagerOnly,
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn full_snapshot_layout_from_header(
    hdr: &[u8; FULL_SNAPSHOT_REST_HEADER_LEN],
) -> FullSnapshotLayout {
    let state_len = u64::from_le_bytes(
        hdr[4..12]
            .try_into()
            .expect("VMSN state_len field is 8 bytes"),
    );
    let mem_len = u64::from_le_bytes(
        hdr[16..24]
            .try_into()
            .expect("VMSN mem_len field is 8 bytes"),
    );
    full_snapshot_layout_for_lengths(state_len, mem_len)
        .expect("snapshot header lengths should not overflow")
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn parse_full_snapshot_header(
    hdr: &[u8; FULL_SNAPSHOT_REST_HEADER_LEN],
    file_len: u64,
    path: &str,
) -> Result<FullSnapshotHeader> {
    let version = u16::from_le_bytes(hdr[0..2].try_into().expect("VMSN version field is 2 bytes"));
    if version != SNAPSHOT_VERSION {
        return Err(VmmError::Snapshot(format!(
            "unsupported VMSN version {version} in {path}"
        )));
    }

    let flags = u16::from_le_bytes(hdr[2..4].try_into().expect("VMSN flags field is 2 bytes"));
    if flags & !FULL_SNAPSHOT_DIFF_FLAG != 0 {
        return Err(VmmError::Snapshot(format!(
            "unsupported VMSN flags 0x{flags:x} in {path}"
        )));
    }

    let state_len = u64::from_le_bytes(
        hdr[4..12]
            .try_into()
            .expect("VMSN state_len field is 8 bytes"),
    );
    let state_crc = u32::from_le_bytes(
        hdr[12..16]
            .try_into()
            .expect("VMSN state_crc field is 4 bytes"),
    );
    let mem_len = u64::from_le_bytes(
        hdr[16..24]
            .try_into()
            .expect("VMSN mem_len field is 8 bytes"),
    );
    let mem_crc = u32::from_le_bytes(
        hdr[24..28]
            .try_into()
            .expect("VMSN mem_crc field is 4 bytes"),
    );
    validate_snapshot_lengths(state_len, mem_len, path)?;
    let layout = full_snapshot_layout_for_lengths(state_len, mem_len)
        .ok_or_else(|| VmmError::Snapshot(format!("full snapshot length overflow in {path}")))?;
    let expected_len = layout
        .mem_offset
        .checked_add(layout.mem_len)
        .ok_or_else(|| VmmError::Snapshot(format!("full snapshot length overflow in {path}")))?;
    if file_len < expected_len {
        return Err(VmmError::Snapshot(format!(
            "truncated full snapshot: need {expected_len} bytes, got {file_len}"
        )));
    }
    Ok(FullSnapshotHeader {
        flags,
        layout,
        state_crc,
        mem_crc,
    })
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn validate_snapshot_lengths(state_len: u64, mem_len: u64, path: &str) -> Result<()> {
    if state_len > MAX_SNAPSHOT_STATE_BYTES {
        return Err(VmmError::Snapshot(format!(
            "state blob too large in {path}: {state_len} bytes"
        )));
    }
    if mem_len < crate::config::MIB || mem_len > crate::config::MAX_MEMORY_BYTES {
        return Err(VmmError::Snapshot(format!(
            "memory image size out of range in {path}: {mem_len} bytes"
        )));
    }
    if mem_len % crate::config::MIB != 0 {
        return Err(VmmError::Snapshot(format!(
            "memory image size is not MiB-aligned in {path}: {mem_len} bytes"
        )));
    }
    Ok(())
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn snapshot_file_kind_from_header(
    magic: &[u8; 4],
    full_header: Option<&[u8; FULL_SNAPSHOT_REST_HEADER_LEN]>,
) -> std::result::Result<SnapshotFileKind, &'static str> {
    match magic {
        b"VMSN" => {
            let hdr = full_header.ok_or("missing VMSN header")?;
            let flags =
                u16::from_le_bytes(hdr[2..4].try_into().expect("VMSN flags field is 2 bytes"));
            if flags & FULL_SNAPSHOT_DIFF_FLAG != 0 {
                Ok(SnapshotFileKind::EagerOnly)
            } else {
                Ok(SnapshotFileKind::LazyFull(
                    full_snapshot_layout_from_header(hdr),
                ))
            }
        }
        b"VMSD" => Ok(SnapshotFileKind::EagerOnly),
        _ => Err("bad snapshot magic"),
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn crc32_file_range(
    file: &mut std::fs::File,
    path: &str,
    offset: u64,
    len: u64,
    what: &str,
) -> Result<u32> {
    use std::io::{Read, Seek, SeekFrom};

    file.seek(SeekFrom::Start(offset))
        .map_err(|e| VmmError::Snapshot(format!("seek {what} in {path}: {e}")))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut remaining = len;
    let mut buf = [0u8; 64 * 1024];
    while remaining > 0 {
        let want = usize::try_from(remaining.min(buf.len() as u64))
            .map_err(|_| VmmError::Snapshot(format!("{what} length too large in {path}")))?;
        file.read_exact(&mut buf[..want])
            .map_err(|e| VmmError::Snapshot(format!("read {what} in {path}: {e}")))?;
        hasher.update(&buf[..want]);
        remaining -= u64::try_from(want)
            .map_err(|_| VmmError::Snapshot(format!("{what} length overflow in {path}")))?;
    }
    Ok(hasher.finalize())
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
struct RestoredSnapshot {
    mem: vmm_memory_backend::GuestMemory,
    state_blob: Vec<u8>,
    lazy_restore: Option<vmm_memory_backend::LazyRestore>,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn try_lazy_restore_full_snapshot(path: &str) -> Result<Option<RestoredSnapshot>> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file =
        std::fs::File::open(path).map_err(|e| VmmError::Snapshot(format!("open {path}: {e}")))?;
    let file_len = file
        .metadata()
        .map_err(|e| VmmError::Snapshot(format!("stat {path}: {e}")))?
        .len();
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .map_err(|e| VmmError::Snapshot(format!("read magic in {path}: {e}")))?;

    if &magic != b"VMSN" {
        snapshot_file_kind_from_header(&magic, None)
            .map_err(|e| VmmError::Snapshot(format!("{e} in {path}")))?;
        return Ok(None);
    };

    let mut hdr = [0u8; FULL_SNAPSHOT_REST_HEADER_LEN];
    file.read_exact(&mut hdr)
        .map_err(|e| VmmError::Snapshot(format!("read header in {path}: {e}")))?;
    let header = parse_full_snapshot_header(&hdr, file_len, path)?;
    if header.flags & FULL_SNAPSHOT_DIFF_FLAG != 0 {
        return Ok(None);
    }
    let layout = header.layout;

    let state_len: usize = layout
        .state_len
        .try_into()
        .map_err(|_| VmmError::Snapshot("state blob too large".into()))?;
    let mem_len: usize = layout
        .mem_len
        .try_into()
        .map_err(|_| VmmError::Snapshot("memory image too large".into()))?;

    file.seek(SeekFrom::Start(layout.state_offset))
        .map_err(|e| VmmError::Snapshot(format!("seek state in {path}: {e}")))?;
    let mut state_blob = vec![0u8; state_len];
    file.read_exact(&mut state_blob)
        .map_err(|e| VmmError::Snapshot(format!("read state in {path}: {e}")))?;
    let actual_state_crc = crc32fast::hash(&state_blob);
    if actual_state_crc != header.state_crc {
        return Err(VmmError::Snapshot(format!(
            "state CRC mismatch in {path}: got {actual_state_crc:#010x}, expected {:#010x}",
            header.state_crc
        )));
    }
    let actual_mem_crc =
        crc32_file_range(&mut file, path, layout.mem_offset, layout.mem_len, "mem")?;
    if actual_mem_crc != header.mem_crc {
        return Err(VmmError::Snapshot(format!(
            "memory CRC mismatch in {path}: got {actual_mem_crc:#010x}, expected {:#010x}",
            header.mem_crc
        )));
    }

    let mem = vmm_memory_backend::GuestMemory::new(layout.mem_len)
        .map_err(|e| VmmError::Memory(e.to_string()))?;
    let lazy_restore = vmm_memory_backend::start_lazy_restore(
        mem.as_ptr() as *mut u8,
        mem_len,
        &file,
        layout.mem_offset,
        layout.mem_len,
        Some(mem.host_dirty_tracker()),
    )
    .map_err(|e| VmmError::Snapshot(format!("UFFD lazy restore: {e}")))?;

    log::info!(
        "restore: UFFD lazy full snapshot armed (mem_offset={}, mem_len={})",
        layout.mem_offset,
        layout.mem_len
    );
    Ok(Some(RestoredSnapshot {
        mem,
        state_blob,
        lazy_restore: Some(lazy_restore),
    }))
}

/// One snapshot file's contents: a full base image, or a diff (parent + pages).
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
enum SnapshotContent {
    Full {
        mem: vmm_memory_backend::GuestMemory,
        state: Vec<u8>,
    },
    Diff {
        parent: PathBuf,
        state: Vec<u8>,
        pages: Vec<(u64, Vec<u8>)>,
    },
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn ensure_file_has_bytes(
    file_len: u64,
    offset: u64,
    len: u64,
    what: &str,
    path: &str,
) -> Result<()> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| VmmError::Snapshot(format!("{what} length overflow in {path}")))?;
    if end > file_len {
        return Err(VmmError::Snapshot(format!(
            "truncated {what} in {path}: need {end} bytes, got {file_len}"
        )));
    }
    Ok(())
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn canonical_snapshot_tip(path: &str) -> Result<(PathBuf, PathBuf)> {
    let tip = std::fs::canonicalize(path)
        .map_err(|e| VmmError::Snapshot(format!("canonicalize {path}: {e}")))?;
    let root = tip
        .parent()
        .ok_or_else(|| VmmError::Snapshot(format!("snapshot has no parent dir: {path}")))?
        .to_path_buf();
    Ok((tip, root))
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn resolve_snapshot_parent(parent: &str, snapshot_root: &Path) -> Result<PathBuf> {
    if parent.is_empty() {
        return Err(VmmError::Snapshot("empty diff parent path".into()));
    }
    let parent_path = Path::new(parent);
    let candidate = if parent_path.is_absolute() {
        parent_path.to_path_buf()
    } else {
        snapshot_root.join(parent_path)
    };
    let canonical = std::fs::canonicalize(&candidate).map_err(|e| {
        VmmError::Snapshot(format!(
            "canonicalize diff parent {}: {e}",
            candidate.display()
        ))
    })?;
    if !canonical.starts_with(snapshot_root) {
        return Err(VmmError::Snapshot(format!(
            "diff parent escapes snapshot root: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn read_snapshot(path: &Path, snapshot_root: &Path) -> Result<SnapshotContent> {
    use std::io::{Read, Seek, SeekFrom};

    let path_display = path.display().to_string();
    let mut file = std::fs::File::open(path)
        .map_err(|e| VmmError::Snapshot(format!("open {path_display}: {e}")))?;
    let file_len = file
        .metadata()
        .map_err(|e| VmmError::Snapshot(format!("stat {path_display}: {e}")))?
        .len();
    let rd = |f: &mut std::fs::File, buf: &mut [u8], what: &str| -> Result<()> {
        f.read_exact(buf)
            .map_err(|e| VmmError::Snapshot(format!("{what} in {path_display}: {e}")))
    };
    let mut magic = [0u8; 4];
    rd(&mut file, &mut magic, "read magic")?;

    if &magic == b"VMSN" {
        let mut hdr = [0u8; FULL_SNAPSHOT_REST_HEADER_LEN];
        rd(&mut file, &mut hdr, "read header")?;
        let header = parse_full_snapshot_header(&hdr, file_len, &path_display)?;
        let layout = header.layout;
        let state_len = usize::try_from(layout.state_len)
            .map_err(|_| VmmError::Snapshot("state blob too large".into()))?;
        let mem_len = usize::try_from(layout.mem_len)
            .map_err(|_| VmmError::Snapshot("memory image too large".into()))?;
        let mut state = vec![0u8; state_len];
        file.seek(SeekFrom::Start(layout.state_offset))
            .map_err(|e| VmmError::Snapshot(format!("seek state in {path_display}: {e}")))?;
        rd(&mut file, &mut state, "read state")?;
        let actual_state_crc = crc32fast::hash(&state);
        if actual_state_crc != header.state_crc {
            return Err(VmmError::Snapshot(format!(
                "state CRC mismatch in {path_display}: got {actual_state_crc:#010x}, expected {:#010x}",
                header.state_crc
            )));
        }
        let actual_mem_crc = crc32_file_range(
            &mut file,
            &path_display,
            layout.mem_offset,
            layout.mem_len,
            "mem",
        )?;
        if actual_mem_crc != header.mem_crc {
            return Err(VmmError::Snapshot(format!(
                "memory CRC mismatch in {path_display}: got {actual_mem_crc:#010x}, expected {:#010x}",
                header.mem_crc
            )));
        }
        let mem = vmm_memory_backend::GuestMemory::new(layout.mem_len)
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        let mem_slice: &mut [u8] = {
            // SAFETY: `mem` was just allocated with `mem_len` bytes and is owned here.
            unsafe { std::slice::from_raw_parts_mut(mem.as_ptr() as *mut u8, mem_len) }
        };
        file.seek(SeekFrom::Start(layout.mem_offset))
            .map_err(|e| VmmError::Snapshot(format!("seek mem in {path_display}: {e}")))?;
        rd(&mut file, mem_slice, "read mem")?;
        Ok(SnapshotContent::Full { mem, state })
    } else if &magic == b"VMSD" {
        let mut u16b = [0u8; 2];
        rd(&mut file, &mut u16b, "read version")?;
        let version = u16::from_le_bytes(u16b);
        if version != SNAPSHOT_VERSION {
            return Err(VmmError::Snapshot(format!(
                "unsupported VMSD version {version} in {path_display}"
            )));
        }
        let mut u32b = [0u8; 4];
        rd(&mut file, &mut u32b, "read parent_len")?;
        let parent_len = u64::from(u32::from_le_bytes(u32b));
        if parent_len > MAX_DIFF_PARENT_PATH_BYTES {
            return Err(VmmError::Snapshot("parent path too long".into()));
        }
        ensure_file_has_bytes(
            file_len,
            file.stream_position().unwrap_or(file_len),
            parent_len,
            "parent path",
            &path_display,
        )?;
        let parent_len = usize::try_from(parent_len)
            .map_err(|_| VmmError::Snapshot("parent path too long".into()))?;
        let mut pbuf = vec![0u8; parent_len];
        rd(&mut file, &mut pbuf, "read parent")?;
        let parent = std::str::from_utf8(&pbuf).map_err(|e| {
            VmmError::Snapshot(format!("parent path is not UTF-8 in {path_display}: {e}"))
        })?;
        let parent = resolve_snapshot_parent(parent, snapshot_root)?;
        let mut u64b = [0u8; 8];
        rd(&mut file, &mut u64b, "read state_len")?;
        let state_len = u64::from_le_bytes(u64b);
        if state_len > MAX_SNAPSHOT_STATE_BYTES {
            return Err(VmmError::Snapshot(format!(
                "state blob too large in {path_display}: {state_len} bytes"
            )));
        }
        rd(&mut file, &mut u32b, "read state_crc")?;
        let state_crc = u32::from_le_bytes(u32b);
        ensure_file_has_bytes(
            file_len,
            file.stream_position().unwrap_or(file_len),
            state_len,
            "state blob",
            &path_display,
        )?;
        let state_len = usize::try_from(state_len)
            .map_err(|_| VmmError::Snapshot("state blob too large".into()))?;
        let mut state = vec![0u8; state_len];
        rd(&mut file, &mut state, "read state")?;
        let actual_state_crc = crc32fast::hash(&state);
        if actual_state_crc != state_crc {
            return Err(VmmError::Snapshot(format!(
                "state CRC mismatch in {path_display}: got {actual_state_crc:#010x}, expected {state_crc:#010x}"
            )));
        }
        rd(&mut file, &mut u32b, "read n_pages")?;
        let n_pages = usize::try_from(u32::from_le_bytes(u32b))
            .map_err(|_| VmmError::Snapshot("too many diff pages".into()))?;
        if n_pages > MAX_DIFF_PAGES {
            return Err(VmmError::Snapshot(format!(
                "too many diff pages in {path_display}: {n_pages}"
            )));
        }
        let min_page_headers = u64::try_from(n_pages)
            .ok()
            .and_then(|n| n.checked_mul(12))
            .ok_or_else(|| VmmError::Snapshot("diff page header length overflow".into()))?;
        ensure_file_has_bytes(
            file_len,
            file.stream_position().unwrap_or(file_len),
            min_page_headers,
            "diff page headers",
            &path_display,
        )?;
        let mut pages = Vec::with_capacity(n_pages);
        let mut total_page_bytes: u64 = 0;
        for _ in 0..n_pages {
            rd(&mut file, &mut u64b, "read page gpa")?;
            let gpa = u64::from_le_bytes(u64b);
            rd(&mut file, &mut u32b, "read page len")?;
            let len = u64::from(u32::from_le_bytes(u32b));
            if len > MAX_DIFF_PAGE_BYTES {
                return Err(VmmError::Snapshot("diff page too large".into()));
            }
            ensure_file_has_bytes(
                file_len,
                file.stream_position().unwrap_or(file_len),
                len,
                "diff page bytes",
                &path_display,
            )?;
            let end = gpa
                .checked_add(len)
                .ok_or_else(|| VmmError::Snapshot("diff page GPA overflow".into()))?;
            if end > crate::config::MAX_MEMORY_BYTES {
                return Err(VmmError::Snapshot(format!(
                    "diff page outside maximum guest memory in {path_display}: end={end}"
                )));
            }
            let len = usize::try_from(len)
                .map_err(|_| VmmError::Snapshot("diff page too large".into()))?;
            total_page_bytes = validate_diff_payload_budget(total_page_bytes, len)?;
            let mut bytes = vec![0u8; len];
            rd(&mut file, &mut bytes, "read page bytes")?;
            pages.push((gpa, bytes));
        }
        Ok(SnapshotContent::Diff {
            parent,
            state,
            pages,
        })
    } else {
        Err(VmmError::Snapshot(format!("bad magic in {path_display}")))
    }
}

/// Reconstruct the guest memory + tip state for a snapshot that may be the tip
/// of a diff chain: follow parent pointers to the base full snapshot, load it,
/// then apply each diff's dirty pages in base→tip order. Returns the memory and
/// the tip snapshot's state blob (so restore uses the checkpoint's vCPU state).
/// Iterative (not recursive) so a chain of hundreds of diffs can't overflow.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn load_snapshot_chain(path: &str) -> Result<(vmm_memory_backend::GuestMemory, Vec<u8>)> {
    // Follow the chain tip→base, collecting each diff's (state, pages).
    let mut diffs: Vec<(Vec<u8>, Vec<(u64, Vec<u8>)>)> = Vec::new();
    let mut chain_page_bytes = 0u64;
    let (mut cur, snapshot_root) = canonical_snapshot_tip(path)?;
    let mut seen = std::collections::HashSet::new();
    let (mem, base_state) = loop {
        if seen.len() >= MAX_DIFF_CHAIN_DEPTH {
            return Err(VmmError::Snapshot("snapshot chain too deep".into()));
        }
        if !seen.insert(cur.clone()) {
            return Err(VmmError::Snapshot("snapshot chain cycle".into()));
        }
        match read_snapshot(&cur, &snapshot_root)? {
            SnapshotContent::Full { mem, state } => break (mem, state),
            SnapshotContent::Diff {
                parent,
                state,
                pages,
            } => {
                for (_, bytes) in &pages {
                    chain_page_bytes = validate_diff_payload_budget(chain_page_bytes, bytes.len())?;
                }
                diffs.push((state, pages));
                cur = parent;
            }
        }
    };
    // The tip is the first diff collected (or the base if no diffs).
    let tip_state = diffs.first().map(|(s, _)| s.clone()).unwrap_or(base_state);
    // Apply diffs base→tip = reverse of the tip→base collection order.
    let mem_bytes = usize::try_from(mem.size_bytes)
        .map_err(|_| VmmError::Memory("restored memory too large".into()))?;
    let mem_slice: &mut [u8] = {
        // SAFETY: `mem` is owned here and sized `mem_bytes`; we only write in-range.
        unsafe { std::slice::from_raw_parts_mut(mem.as_ptr() as *mut u8, mem_bytes) }
    };
    for (_, pages) in diffs.iter().rev() {
        for (gpa, bytes) in pages {
            validate_diff_page_range(*gpa, bytes.len(), mem_bytes)?;
            let start = usize::try_from(*gpa)
                .map_err(|_| VmmError::Snapshot("diff page GPA too large".into()))?;
            let end = start + bytes.len();
            mem_slice[start..end].copy_from_slice(bytes);
        }
    }
    Ok((mem, tip_state))
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct VcpuStateSave {
    pub rip: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub rsi: u64,
    pub cr0: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub efer: u64,
    pub apic_base: u64,
}

/// On-disk snapshot state blob (postcard-encoded). A single owned definition is
/// used for both serialize (snapshot) and deserialize (restore) so the two
/// halves cannot silently drift out of sync.
///
/// `vcpu_full` carries the postcard-serialized [`crate::vcpu_setup::VcpuFullState`]
/// captured from the running vCPU while paused (REGS/SREGS/FPU/XSAVE/XCRS/MSRS/
/// LAPIC/MP_STATE/VCPU_EVENTS). It is `None` for a fast-boot blob (no vCPU has
/// run yet) and `Some` for a snapshot of a live VM, which is what lets restore
/// reconstruct a *running* guest rather than just its memory.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StateBlob {
    pub entry: u64,
    pub mem_size: u64,
    pub vcpu: VcpuStateSave,
    pub kernel_path: String,
    pub cmdline: String,
    pub vcpus: u64,
    pub volumes: Vec<crate::config::VolumeConfig>,
    pub net: Vec<crate::config::NetConfig>,
    pub vcpu_full: Option<Vec<u8>>,
    /// Postcard-serialized [`crate::vcpu_setup::VcpuFullState`] for each AP vCPU
    /// (id 1..N) in id order — SMP snapshot (phase B). Empty for a uniprocessor
    /// VM, so old single-vCPU blobs restore unchanged.
    #[serde(default)]
    pub vcpu_full_aps: Vec<Vec<u8>>,
    /// Postcard-serialized [`crate::kvm::VmFullState`] — the in-kernel IRQCHIP
    /// (PIC+IOAPIC), PIT, and kvmclock. `None` for a fast-boot blob.
    pub vm_full: Option<Vec<u8>>,
    /// 16550 UART register state (IER/LCR/divisor/...), so a restored serial
    /// re-arms the guest's RX interrupt and `exec` works after restore.
    #[serde(default)]
    pub serial: vmm_devices::serial::SerialState,
    #[serde(default)]
    pub virtio_blk: Vec<Vec<u8>>,
    #[serde(default)]
    pub virtio_net: Vec<Vec<u8>>,
    /// virtio-vsock transport state and active stream metadata. Streams are not
    /// resurrected; restore injects RSTs so the guest agent re-dials.
    #[serde(default)]
    pub vsock: Option<vmm_devices::virtio::vsock::VirtioVsockMmioState>,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
#[allow(dead_code)] // retained for the fast-boot path; live snapshots use the vCPU thread capture
fn save_vcpu_state(vcpu: &kvm_ioctls::VcpuFd) -> Result<VcpuStateSave> {
    let regs = vcpu
        .get_regs()
        .map_err(|e| VmmError::Kvm(format!("KVM_GET_REGS for snapshot: {e}")))?;
    let sregs = vcpu
        .get_sregs()
        .map_err(|e| VmmError::Kvm(format!("KVM_GET_SREGS for snapshot: {e}")))?;
    Ok(VcpuStateSave {
        rip: regs.rip,
        rflags: regs.rflags,
        rsp: regs.rsp,
        rsi: regs.rsi,
        cr0: sregs.cr0,
        cr3: sregs.cr3,
        cr4: sregs.cr4,
        efer: sregs.efer,
        apic_base: sregs.apic_base,
    })
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn capture_virtio_blk_states(
    devs: &[Arc<vmm_devices::virtio::blk_transport::VirtioBlkMmio>],
) -> Vec<Vec<u8>> {
    devs.iter()
        .map(|dev| vmm_devices::persist::Persist::save(&**dev))
        .filter_map(
            |state: vmm_devices::virtio::blk_transport::VirtioBlkMmioState| {
                postcard::to_allocvec(&state).ok()
            },
        )
        .collect()
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn capture_virtio_net_states(
    devs: &[Arc<vmm_devices::virtio::net_transport::VirtioNetMmio>],
) -> Vec<Vec<u8>> {
    devs.iter()
        .map(|dev| vmm_devices::persist::Persist::save(&**dev))
        .filter_map(
            |state: vmm_devices::virtio::net_transport::VirtioNetMmioState| {
                postcard::to_allocvec(&state).ok()
            },
        )
        .collect()
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn restore_virtio_blk_states(
    devs: &mut [Arc<vmm_devices::virtio::blk_transport::VirtioBlkMmio>],
    states: &[Vec<u8>],
) {
    if states.is_empty() {
        return;
    }
    if states.len() != devs.len() {
        log::warn!(
            "restore: virtio-blk state count {} != device count {}; using fresh blk devices",
            states.len(),
            devs.len()
        );
        return;
    }
    for (i, (dev, bytes)) in devs.iter_mut().zip(states.iter()).enumerate() {
        match postcard::from_bytes::<vmm_devices::virtio::blk_transport::VirtioBlkMmioState>(bytes)
        {
            Ok(state) => vmm_devices::persist::Persist::restore(dev, state),
            Err(e) => log::warn!("restore: virtio-blk state {i}: {e}; using fresh state"),
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn restore_virtio_net_states(
    devs: &mut [Arc<vmm_devices::virtio::net_transport::VirtioNetMmio>],
    states: &[Vec<u8>],
) {
    if states.is_empty() {
        return;
    }
    if states.len() != devs.len() {
        log::warn!(
            "restore: virtio-net state count {} != device count {}; using fresh net devices",
            states.len(),
            devs.len()
        );
        return;
    }
    for (i, (dev, bytes)) in devs.iter_mut().zip(states.iter()).enumerate() {
        match postcard::from_bytes::<vmm_devices::virtio::net_transport::VirtioNetMmioState>(bytes)
        {
            Ok(state) => vmm_devices::persist::Persist::restore(dev, state),
            Err(e) => log::warn!("restore: virtio-net state {i}: {e}; using fresh state"),
        }
    }
}

/// Reconstruct a *running* VM from restored guest memory + a captured vCPU
/// state. Mirrors `create_live`'s KVM/device plumbing, but re-applies the saved
/// vCPU state instead of setting up a fresh boot entry, and does NOT rewrite the
/// kernel/GDT/ACPI tables (they are already present in the restored memory).
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn build_running_vm(
    mem: vmm_memory_backend::GuestMemory,
    config: &VmConfig,
    full_state: &crate::vcpu_setup::VcpuFullState,
    ap_states: &[crate::vcpu_setup::VcpuFullState],
    vm_state: Option<&crate::kvm::VmFullState>,
    serial_state: &vmm_devices::serial::SerialState,
    virtio_blk_states: &[Vec<u8>],
    virtio_net_states: &[Vec<u8>],
    vsock_state: Option<&vmm_devices::virtio::vsock::VirtioVsockMmioState>,
    entry: u64,
) -> Result<RunningVm> {
    use crate::vcpu_thread::VcpuThread;

    let full_boot = true;
    // Recreate the same devices at the same deterministic MMIO/IRQ layout
    // create_live used, so they line up with the ACPI tables already baked into
    // the restored guest memory. The ACPI table list is ignored here (the
    // restored memory already contains it) — only the devices + fds matter.
    let WiredDevices {
        devices,
        acpi_devices: _,
        mut blks,
        blk_irq_evts,
        nets,
        rng_irq,
        vsock,
    } = build_devices(config, &mem)?;
    restore_virtio_blk_states(&mut blks, virtio_blk_states);
    let mut net_devices: Vec<_> = nets.iter().map(|n| n.dev.clone()).collect();
    restore_virtio_net_states(&mut net_devices, virtio_net_states);
    let mut irq_evts: Vec<vmm_sys_util::eventfd::EventFd> = blk_irq_evts;

    let template = crate::cpu_template::CpuTemplate::bare();
    let kvm_vm = crate::kvm::KvmVm::new_with_options(mem, devices, template, full_boot)?;

    // Re-apply the guest's in-kernel IRQCHIP/PIT/clock over the freshly-created
    // ones, so the restored guest keeps its interrupt routing (a fresh IOAPIC
    // would be masked/default and post-restore device I/O would stall waiting
    // for interrupts). Must happen after the irqchip/PIT exist (new_with_options
    // created them) and before the vCPU runs.
    if let Some(vm_state) = vm_state {
        kvm_vm.restore_vm_state(vm_state)?;
    }

    for (i, evt) in irq_evts.iter().enumerate() {
        let irq = 5 + i as u32;
        if let Err(e) = kvm_vm.register_irqfd(evt, irq) {
            log::warn!("irqfd for volume {i} (gsi={irq}): {e}");
        }
    }
    // i8042 irqfd (gsi 1), matching create_live's full-boot path.
    let i8042_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
        .map_err(|e| VmmError::Kvm(format!("EventFd: {e}")))?;
    let _ = kvm_vm.register_irqfd(&i8042_evt, 1);
    irq_evts.push(i8042_evt);

    let serial_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
        .map_err(|e| VmmError::Kvm(format!("EventFd: {e}")))?;
    if let Err(e) = kvm_vm.register_irqfd(&serial_evt, 4) {
        log::warn!("serial irqfd (gsi=4): {e}");
    }
    let mut serial_dev = vmm_devices::serial::Serial::new(
        serial_evt
            .try_clone()
            .map_err(|e| VmmError::Kvm(format!("EventFd clone: {e}")))?,
    );
    // Replay the guest's captured UART programming so the restored serial
    // re-arms its RX interrupt; without this, host→guest bytes (exec commands)
    // raise no IRQ and the guest agent never wakes — exec hangs post-restore.
    vmm_devices::persist::Persist::restore(&mut serial_dev, serial_state.clone());
    let serial = Arc::new(serial_dev);
    irq_evts.push(serial_evt);

    // virtio-net: mirror create_live — irqfd + TX ioeventfd + host<->tap loop
    // per device, kept alive (loop before fds) in the returned RunningVm.
    let mut net_io_loops = Vec::new();
    let mut taps = Vec::new();
    for net in nets {
        let WiredNet {
            dev,
            tap,
            irq_evt,
            io_evt,
            irq,
            mmio_base,
        } = net;
        if let Err(e) = kvm_vm.register_irqfd(&irq_evt, irq) {
            log::warn!("net irqfd (gsi={irq}): {e}");
        }
        if let Err(e) = kvm_vm.register_ioeventfd(mmio_base + 0x50, &io_evt) {
            log::warn!("net ioeventfd at 0x{:x}: {e}", mmio_base + 0x50);
        }
        let tap_fd = tap.fd;
        let kick_fd = {
            use std::os::fd::AsRawFd;
            io_evt.as_raw_fd()
        };
        match vmm_devices::virtio::net_io_loop::spawn_net_io_loop(dev, tap_fd, kick_fd) {
            Ok(l) => net_io_loops.push(l),
            Err(e) => log::warn!("net io loop: {e}"),
        }
        irq_evts.push(irq_evt);
        irq_evts.push(io_evt);
        taps.push(tap);
    }

    // Register the virtio-rng completion irqfd, matching create_live.
    if let Some((irq, evt)) = rng_irq {
        if let Err(e) = kvm_vm.register_irqfd(&evt, irq) {
            log::warn!("rng irqfd (gsi={irq}): {e}");
        }
        irq_evts.push(evt);
    }

    // Wire the virtio-vsock exec channel (matching create_live), so a restored
    // VM re-establishes exec-over-vsock when the guest agent re-dials.
    let (vsock_pump, vsock_exec, vsock_pty, vsock_reset) = match vsock {
        Some(wv) => {
            if let Err(e) = kvm_vm.register_irqfd(&wv.irq_evt, wv.irq) {
                log::warn!("vsock irqfd (gsi={}): {e}", wv.irq);
            }
            irq_evts.push(wv.irq_evt);
            if let Err(e) = kvm_vm.register_ioeventfd_datamatch(wv.mmio_base + 0x50, &wv.io_evt, 1)
            {
                log::warn!("vsock ioeventfd at 0x{:x}: {e}", wv.mmio_base + 0x50);
            }
            use std::os::fd::AsRawFd;
            let tx_kick_fd = wv.io_evt.as_raw_fd();
            let device = wv.device;
            // Restore the transport (queue addrs/cursors, features) now so the
            // pump can service the guest, but DEFER injecting connection RSTs
            // until the vCPU is resumed (below). An RST delivered while the vCPU
            // is still paused raises an RX completion interrupt the paused LAPIC
            // drops, so the guest never sees the reset and never re-dials.
            let reset = vsock_state.map(|state| {
                device.restore_transport_state(state);
                (device.clone(), state.connections.clone())
            });
            let pump =
                vmm_devices::virtio::vsock_io_loop::spawn_vsock_pump(device.clone(), tx_kick_fd)
                    .ok();
            let pump_wake = pump.as_ref().and_then(|p| p.wake_evt().ok());
            let pty_wake = pump.as_ref().and_then(|p| p.wake_evt().ok());
            irq_evts.push(wv.io_evt);
            let exec = crate::vsock_exec::VsockExecChannel::bind_with_pump_wake(
                &wv.control_socket,
                pump_wake,
            )
            .ok();
            let pty = pump
                .as_ref()
                .map(|_| crate::vsock_pty::VsockPtyChannel::new(device, pty_wake));
            (pump, exec, pty, reset)
        }
        None => (None, None, None, None),
    };

    let vcpu = kvm_vm.create_vcpu(0)?;
    // CPUID must be set on the fresh vCPU before the saved MSRs/state are
    // applied; the captured LAPIC already carries the LVT config, so we do not
    // re-run set_lint here.
    kvm_vm.setup_cpuid(&vcpu)?;
    crate::vcpu_setup::restore_vcpu_full_state(&vcpu, full_state)?;
    kvm_vm.apply_cpu_template_msrs(&vcpu)?;
    let vcpu_thread = VcpuThread::spawn(vcpu, kvm_vm.mmio_bus.clone(), serial.clone());

    // SMP restore (phase B): recreate each AP (id 1..N) and re-apply its captured
    // state (which includes its RUNNABLE MP_STATE + LAPIC), so the restored VM
    // comes back with all vCPUs online. Per-AP CPUID (with its APIC id) is set
    // before the saved state, matching create_live.
    let mut ap_threads = Vec::with_capacity(ap_states.len());
    for (i, ap_state) in ap_states.iter().enumerate() {
        let id = (i + 1) as u8;
        let ap = kvm_vm.create_vcpu(id)?;
        kvm_vm.apply_boot_cpuid(&ap, id)?;
        crate::vcpu_setup::restore_vcpu_full_state(&ap, ap_state)?;
        kvm_vm.apply_cpu_template_msrs(&ap)?;
        ap_threads.push(VcpuThread::spawn(
            ap,
            kvm_vm.mmio_bus.clone(),
            serial.clone(),
        ));
    }

    // The guest vCPU(s) are live again. Inject an RST for each connection that
    // was open at snapshot time so the guest's vsock layer tears the stale
    // stream down and the agent re-dials the host exec channel (the host side
    // already re-accepts the new connection). Done here, post-resume, so the RX
    // completion interrupt lands on a running vCPU instead of a paused LAPIC.
    if let Some((dev, conns)) = vsock_reset {
        let resets = dev.reset_restored_connections(&conns);
        if resets > 0 {
            log::info!("vsock restore: injected RST for {resets} restored stream(s)");
        }
    }

    Ok(RunningVm {
        kvm_vm,
        vcpu_thread,
        ap_threads,
        loaded_entry: entry,
        net_io_loops,
        blk_devices: blks,
        net_devices,
        taps,
        vsock_pump,
        vsock_exec,
        vsock_pty,
        keep_alive_fds: irq_evts,
    })
}

/// Build the virtio device list (block + net) shared by `create_live` and
/// `build_running_vm`, at a single deterministic MMIO/IRQ/ACPI layout so the
/// two paths cannot drift. Block devices come first at GSI 5.., then net at
/// GSI 5+volumes.len().., each at MMIO base 0xd000_0000 + slot*0x1000.
///
/// Returns the boxed devices (for `KvmVm::new_with_options`), the ACPI DSDT
/// entries (base,len,gsi) create_live bakes into guest memory, the per-volume
/// completion irqfds (GSI 5.., registered by the caller), and the net wiring
/// (device + tap + irqfd/ioeventfd EventFds) the caller registers + spawns.
/// (virtio-rng is intentionally not wired here yet; see build_devices.)
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
struct WiredDevices {
    devices: Vec<(
        vmm_devices::bus::MmioRange,
        Box<dyn vmm_devices::bus::MmioDevice>,
    )>,
    acpi_devices: Vec<(u64, u64, u32)>,
    blks: Vec<Arc<vmm_devices::virtio::blk_transport::VirtioBlkMmio>>,
    blk_irq_evts: Vec<vmm_sys_util::eventfd::EventFd>,
    nets: Vec<WiredNet>,
    /// virtio-rng completion irqfd (gsi, EventFd), registered by the caller.
    rng_irq: Option<(u32, vmm_sys_util::eventfd::EventFd)>,
    /// virtio-vsock exec device (pump + control-socket accept wired by caller).
    vsock: Option<WiredVsock>,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
struct WiredNet {
    dev: Arc<vmm_devices::virtio::net_transport::VirtioNetMmio>,
    tap: vmm_net::tap::Tap,
    irq_evt: vmm_sys_util::eventfd::EventFd,
    io_evt: vmm_sys_util::eventfd::EventFd,
    irq: u32,
    mmio_base: u64,
}

/// A wired virtio-vsock device: the exec channel between host and guest. The
/// guest agent dials the host over vsock; the device bridges that to
/// `control_socket`, which the controller accepts on to run exec commands.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
struct WiredVsock {
    device: Arc<vmm_devices::virtio::vsock::VirtioVsockMmio>,
    irq_evt: vmm_sys_util::eventfd::EventFd,
    irq: u32,
    /// ioeventfd for the TX QUEUE_NOTIFY, so the guest's kick lands on the pump
    /// thread instead of trapping to the (seccomped) vCPU thread.
    io_evt: vmm_sys_util::eventfd::EventFd,
    mmio_base: u64,
    control_socket: std::path::PathBuf,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn build_devices(config: &VmConfig, mem: &vmm_memory_backend::GuestMemory) -> Result<WiredDevices> {
    use vmm_devices::bus::{MmioDevice, MmioRange};
    use vmm_devices::virtio::blk_transport::VirtioBlkMmio;
    use vmm_devices::virtio::net_transport::VirtioNetMmio;
    use vmm_devices::virtio::rng::VirtioRng;
    use vmm_devices::virtio::rng_transport::VirtioRngMmio;

    const MMIO_START: u64 = 0xd000_0000;
    let gm = mem.inner.clone();
    let host_dirty = mem.host_dirty_tracker();
    let mut devices: Vec<(MmioRange, Box<dyn MmioDevice>)> = Vec::new();
    let mut acpi_devices: Vec<(u64, u64, u32)> = Vec::new();
    let mut blks: Vec<Arc<VirtioBlkMmio>> = Vec::new();
    let mut blk_irq_evts: Vec<vmm_sys_util::eventfd::EventFd> = Vec::new();
    let mut nets: Vec<WiredNet> = Vec::new();

    for (i, vol) in config.volumes.iter().enumerate() {
        let irq = 5 + i as u32;
        let mmio_base = MMIO_START + (i as u64) * 0x1000;
        let backend = crate::volume::open_volume_backend(vol)
            .map_err(|e| VmmError::Device(format!("blk backend {}: {e}", vol.path)))?;
        let transport = Arc::new(VirtioBlkMmio::new(irq, backend));
        transport.set_guest_memory(gm.clone());
        transport.set_guest_dirty_tracker(host_dirty.clone());
        let irq_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| VmmError::Kvm(format!("EventFd: {e}")))?;
        transport.set_irq_evt(
            irq_evt
                .try_clone()
                .map_err(|e| VmmError::Kvm(format!("EventFd clone: {e}")))?,
        );
        // No ioeventfd for block — QUEUE_NOTIFY must trap to userspace so
        // process_queue() runs synchronously.
        devices.push((
            MmioRange::new(mmio_base, 0x1000),
            Box::new(transport.clone()),
        ));
        acpi_devices.push((mmio_base, 0x1000, irq));
        blks.push(transport);
        blk_irq_evts.push(irq_evt);
        log::info!("volume {i}: {} at mmio 0x{mmio_base:x} irq {irq}", vol.path);
    }

    for (j, net) in config.net.iter().enumerate() {
        let slot = config.volumes.len() + j;
        let irq = 5 + slot as u32;
        let mmio_base = MMIO_START + (slot as u64) * 0x1000;
        let mac = parse_guest_mac(net.guest_mac.as_deref(), j);
        let tap = vmm_net::tap::Tap::create(&net.tap)
            .map_err(|e| VmmError::Device(format!("tap {}: {e}", net.tap)))?;
        let dev = Arc::new(VirtioNetMmio::new(irq, mac));
        dev.set_guest_memory(gm.clone());
        dev.set_guest_dirty_tracker(host_dirty.clone());
        dev.set_tap_fd(tap.fd);
        let irq_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| VmmError::Kvm(format!("EventFd: {e}")))?;
        let io_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| VmmError::Kvm(format!("EventFd: {e}")))?;
        dev.set_irq_evt(
            irq_evt
                .try_clone()
                .map_err(|e| VmmError::Kvm(format!("EventFd clone: {e}")))?,
        );
        devices.push((MmioRange::new(mmio_base, 0x1000), Box::new(dev.clone())));
        acpi_devices.push((mmio_base, 0x1000, irq));
        log::info!(
            "net {j}: tap={} mac={:02x?} at mmio 0x{mmio_base:x} irq {irq}",
            net.tap,
            mac
        );
        nets.push(WiredNet {
            dev,
            tap,
            irq_evt,
            io_evt,
            irq,
            mmio_base,
        });
    }

    // virtio-rng at the slot after all block + net devices (entropy for
    // restored/cloned guests to reseed their CRNG). Like block, its QUEUE_NOTIFY
    // traps to userspace and is serviced synchronously on the MMIO bus, so it
    // needs only a completion irqfd. Now safe to probe: the backend fills
    // entropy via getrandom (openat was killing the vCPU under seccomp).
    let rng_slot = config.volumes.len() + config.net.len();
    let rng_irq_num = 5 + rng_slot as u32;
    let rng_mmio = MMIO_START + (rng_slot as u64) * 0x1000;
    let rng_dev = VirtioRngMmio::new(rng_irq_num, VirtioRng::new());
    rng_dev.set_guest_memory(gm.clone());
    rng_dev.set_guest_dirty_tracker(host_dirty.clone());
    let rng_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
        .map_err(|e| VmmError::Kvm(format!("EventFd: {e}")))?;
    rng_dev.set_irq_evt(
        rng_evt
            .try_clone()
            .map_err(|e| VmmError::Kvm(format!("EventFd clone: {e}")))?,
    );
    devices.push((MmioRange::new(rng_mmio, 0x1000), Box::new(rng_dev)));
    acpi_devices.push((rng_mmio, 0x1000, rng_irq_num));
    log::info!("virtio-rng at mmio 0x{rng_mmio:x} irq {rng_irq_num}");

    // virtio-vsock: the exec channel. Placed at the slot after rng. The guest
    // agent dials the host (CID 2) over vsock; the device bridges that to a
    // per-VM control socket the controller accepts on, giving exec its own
    // framed stream (no ttyS0 console interleaving, clean reconnect on restore).
    use vmm_devices::virtio::vsock::VirtioVsockMmio;
    const VSOCK_GUEST_CID: u64 = 3;
    let vsock_slot = rng_slot + 1;
    let vsock_irq = 5 + vsock_slot as u32;
    let vsock_mmio = MMIO_START + (vsock_slot as u64) * 0x1000;
    let control_socket = unique_runtime_file_path("vmm-vsock", "sock")?;
    let _ = std::fs::remove_file(&control_socket);
    let vsock_dev = Arc::new(VirtioVsockMmio::new(vsock_irq, VSOCK_GUEST_CID));
    vsock_dev.set_guest_memory(gm.clone());
    vsock_dev.set_guest_dirty_tracker(host_dirty.clone());
    if let Err(e) = vsock_dev.connect_uds(&control_socket) {
        log::warn!("vsock connect_uds({}): {e}", control_socket.display());
    }
    let vsock_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
        .map_err(|e| VmmError::Kvm(format!("EventFd: {e}")))?;
    vsock_dev.set_irq_evt(
        vsock_evt
            .try_clone()
            .map_err(|e| VmmError::Kvm(format!("EventFd clone: {e}")))?,
    );
    let vsock_io_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
        .map_err(|e| VmmError::Kvm(format!("EventFd: {e}")))?;
    devices.push((
        MmioRange::new(vsock_mmio, 0x1000),
        Box::new(vsock_dev.clone()),
    ));
    acpi_devices.push((vsock_mmio, 0x1000, vsock_irq));
    log::info!(
        "virtio-vsock at mmio 0x{vsock_mmio:x} irq {vsock_irq} guest_cid {VSOCK_GUEST_CID} → {}",
        control_socket.display()
    );

    Ok(WiredDevices {
        devices,
        acpi_devices,
        blks,
        blk_irq_evts,
        nets,
        rng_irq: Some((rng_irq_num, rng_evt)),
        vsock: Some(WiredVsock {
            device: vsock_dev,
            irq_evt: vsock_evt,
            irq: vsock_irq,
            io_evt: vsock_io_evt,
            mmio_base: vsock_mmio,
            control_socket,
        }),
    })
}

/// Parse a `xx:xx:xx:xx:xx:xx` MAC, falling back to a deterministic locally
/// administered address (02:00:00:00:00:NN) if absent or malformed.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn parse_guest_mac(spec: Option<&str>, index: usize) -> [u8; 6] {
    let default = [0x02, 0x00, 0x00, 0x00, 0x00, (index as u8).wrapping_add(1)];
    let Some(s) = spec else {
        return default;
    };
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        log::warn!("net mac {s:?}: expected 6 colon-separated bytes; using default");
        return default;
    }
    let mut mac = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        match u8::from_str_radix(p.trim(), 16) {
            Ok(b) => mac[i] = b,
            Err(_) => {
                log::warn!("net mac {s:?}: bad byte {p:?}; using default");
                return default;
            }
        }
    }
    mac
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn serialize_state_blob(
    entry: u64,
    mem_size: u64,
    vcpu_state: &VcpuStateSave,
    config: &VmConfig,
) -> Vec<u8> {
    let blob = StateBlob {
        entry,
        mem_size,
        vcpu: vcpu_state.clone(),
        kernel_path: config.kernel.path.clone(),
        cmdline: config.kernel.cmdline.clone(),
        vcpus: config.vcpus.count as u64,
        volumes: config.volumes.clone(),
        net: config.net.clone(),
        vcpu_full: None,
        vcpu_full_aps: Vec::new(),
        vm_full: None,
        serial: Default::default(),
        virtio_blk: Vec::new(),
        virtio_net: Vec::new(),
        vsock: None,
    };

    postcard::to_allocvec(&blob).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{KernelConfig, MemoryConfig, VcpuConfig, VmConfig, VolumeConfig};

    fn cfg() -> VmConfig {
        VmConfig {
            kernel: KernelConfig {
                path: "/vmlinux".into(),
                cmdline: "console=ttyS0".into(),
                initramfs: None,
            },
            memory: MemoryConfig { size_mib: 128 },
            vcpus: VcpuConfig { count: 2 },
            volumes: vec![],
            net: vec![],
        }
    }

    #[test]
    fn status_without_vm_errors() {
        let c = VmmController::new();
        assert!(c.status().is_err());
    }

    #[cfg(not(feature = "boot"))]
    #[test]
    fn status_reports_config_after_create() {
        let c = VmmController::new();
        // create() populates the slot on any target (the non-boot path sets
        // state=Created); it does not need KVM.
        c.create(cfg()).unwrap();
        let s = c.status().unwrap();
        assert_eq!(s.vcpus, 2);
        assert_eq!(s.mem_mib, 128);
        assert_eq!(s.volumes, 0);
        assert_eq!(s.nets, 0);
        assert_eq!(s.kernel, "/vmlinux");
        // No live vCPU thread on the non-boot path.
        assert!(!s.vcpu_alive);
    }

    #[cfg(not(feature = "boot"))]
    #[test]
    fn status_errors_again_after_stop() {
        let c = VmmController::new();
        c.create(cfg()).unwrap();
        assert!(c.status().is_ok());
        c.stop().unwrap();
        assert!(c.status().is_err());
    }

    #[test]
    fn restore_overlay_replaces_saved_golden_overlay() {
        let mut config = cfg();
        config.volumes = vec![VolumeConfig {
            path: "/base/rootfs.ext4".into(),
            read_only: true,
            overlay: Some("/golden/rootfs.overlay".into()),
        }];

        assert_eq!(
            restore_overlay_seed(&config, Some("/clones/a.overlay"))
                .expect("derive golden overlay seed"),
            Some((
                PathBuf::from("/golden/rootfs.overlay"),
                PathBuf::from("/clones/a.overlay")
            ))
        );
        apply_restore_overlay(&mut config, Some("/clones/a.overlay".into())).unwrap();

        assert_eq!(config.volumes[0].path, "/base/rootfs.ext4");
        assert_eq!(
            config.volumes[0].overlay.as_deref(),
            Some("/clones/a.overlay")
        );
    }

    #[test]
    fn restore_overlay_seed_is_a_private_copy_of_the_golden_upper() {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        let dir = private_runtime_dir().expect("private test runtime");
        let unique = format!(
            "restore-overlay-seed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        );
        let golden = dir.join(format!("{unique}-golden.cow"));
        let clone = dir.join(format!("{unique}-clone.cow"));
        let cleanup = [golden.clone(), clone.clone()];
        let golden_bytes = b"golden writable upper state";

        std::fs::write(&golden, golden_bytes).expect("write golden upper");
        seed_restore_overlay(&golden, &clone).expect("seed clone upper");

        assert_eq!(
            std::fs::read(&clone).expect("read clone upper"),
            golden_bytes,
            "a clone must start from the golden writable upper state"
        );
        #[cfg(unix)]
        assert_ne!(
            std::fs::metadata(&golden).expect("golden metadata").ino(),
            std::fs::metadata(&clone).expect("clone metadata").ino(),
            "the clone must not share the golden writable backing file"
        );

        std::fs::write(&clone, b"clone-private-state").expect("mutate clone upper");
        assert_eq!(
            std::fs::read(&golden).expect("reread golden upper"),
            golden_bytes,
            "clone writes must not modify the reusable golden upper state"
        );

        for path in cleanup {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn restore_overlay_wraps_direct_rw_volume() {
        let mut config = cfg();
        config.volumes = vec![VolumeConfig {
            path: "/base/rootfs.ext4".into(),
            read_only: false,
            overlay: None,
        }];

        apply_restore_overlay(&mut config, Some("/clones/b.overlay".into())).unwrap();

        assert_eq!(config.volumes[0].path, "/base/rootfs.ext4");
        assert_eq!(
            config.volumes[0].overlay.as_deref(),
            Some("/clones/b.overlay")
        );
    }

    #[test]
    fn restore_overlay_errors_without_snapshot_volume() {
        let mut config = cfg();
        let err = apply_restore_overlay(&mut config, Some("/clones/orphan.overlay".into()))
            .expect_err("overlay without a disk should fail");

        assert!(matches!(err, VmmError::InvalidConfig(_)));
    }

    #[test]
    fn full_snapshot_layout_places_memory_after_header_and_state() {
        let mut hdr = [0u8; FULL_SNAPSHOT_REST_HEADER_LEN];
        hdr[0..2].copy_from_slice(&1u16.to_le_bytes());
        hdr[2..4].copy_from_slice(&0u16.to_le_bytes());
        hdr[4..12].copy_from_slice(&123u64.to_le_bytes());
        hdr[16..24].copy_from_slice(&(64 * 1024 * 1024u64).to_le_bytes());

        let layout = full_snapshot_layout_from_header(&hdr);

        assert_eq!(layout.state_offset, 32);
        assert_eq!(layout.state_len, 123);
        assert_eq!(layout.mem_offset, 32 + 123);
        assert_eq!(layout.mem_len, 64 * 1024 * 1024);
    }

    #[test]
    fn snapshot_kind_detects_lazy_full_vs_diff_fallback() {
        let mut hdr = [0u8; FULL_SNAPSHOT_REST_HEADER_LEN];
        hdr[0..2].copy_from_slice(&1u16.to_le_bytes());
        hdr[4..12].copy_from_slice(&7u64.to_le_bytes());
        hdr[16..24].copy_from_slice(&4096u64.to_le_bytes());

        let full = snapshot_file_kind_from_header(b"VMSN", Some(&hdr)).unwrap();
        assert!(matches!(full, SnapshotFileKind::LazyFull(_)));

        hdr[2..4].copy_from_slice(&FULL_SNAPSHOT_DIFF_FLAG.to_le_bytes());
        let diff_flagged = snapshot_file_kind_from_header(b"VMSN", Some(&hdr)).unwrap();
        assert_eq!(diff_flagged, SnapshotFileKind::EagerOnly);

        let diff_chain_tip = snapshot_file_kind_from_header(b"VMSD", None).unwrap();
        assert_eq!(diff_chain_tip, SnapshotFileKind::EagerOnly);
    }

    #[test]
    fn diff_page_range_validation_rejects_pages_outside_base_memory() {
        validate_diff_page_range(0, 4096, 4096).unwrap();

        let err = validate_diff_page_range(4096, 1, 4096).unwrap_err();
        assert!(err.to_string().contains("outside base guest memory"));

        let err = validate_diff_page_range(u64::MAX, 2, 4096).unwrap_err();
        assert!(
            err.to_string().contains("too large") || err.to_string().contains("overflow"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn diff_payload_budget_rejects_large_eager_restore_inputs() {
        let total = validate_diff_payload_budget(0, MAX_EAGER_DIFF_BYTES as usize).unwrap();
        assert_eq!(total, MAX_EAGER_DIFF_BYTES);

        let err = validate_diff_payload_budget(MAX_EAGER_DIFF_BYTES, 1).unwrap_err();
        assert!(err.to_string().contains("diff payload too large"));
    }

    #[test]
    fn suspend_layout_reuses_full_snapshot_memory_payload() {
        let layout = full_snapshot_layout_for_lengths(123, 2 * 4096).unwrap();

        assert_eq!(layout.state_offset, FULL_SNAPSHOT_HEADER_LEN);
        assert_eq!(layout.state_len, 123);
        assert_eq!(layout.mem_offset, FULL_SNAPSHOT_HEADER_LEN + 123);
        assert_eq!(layout.mem_len, 2 * 4096);
    }

    #[test]
    fn suspend_image_path_is_process_local_and_private() {
        let path = PathBuf::from(unique_suspend_snapshot_path().unwrap());

        let name = path.file_name().and_then(|s| s.to_str()).unwrap();
        assert!(name.starts_with(".vmm-suspend-"));
        assert!(name.ends_with(".snap"));
        assert!(path.is_absolute());
        assert!(path.components().any(|c| c.as_os_str() == ".vmm-runtime"));
    }

    // Incremental diff-chain round trip. Boot-gated because it uses the
    // GuestMemory-backed snapshot helpers; runs on Linux+KVM (c8i).
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    #[test]
    fn incremental_snapshot_chain_reconstructs_memory() {
        use vmm_memory_backend::dirty::DirtyBitmap;
        let dir = private_runtime_dir().unwrap();
        let base = dir.join(format!("t-base-{}.snap", std::process::id()));
        let d1 = dir.join(format!("t-d1-{}.snap", std::process::id()));
        let d2 = dir.join(format!("t-d2-{}.snap", std::process::id()));
        let (base_s, d1_s, d2_s) = (
            base.to_str().unwrap(),
            d1.to_str().unwrap(),
            d2.to_str().unwrap(),
        );

        // Full base: 1 MiB of 0xAA (matches restore validation minimum).
        let mem: Vec<u8> = vec![0xAA; usize::try_from(crate::config::MIB).expect("MiB fits usize")];
        write_snapshot_file(base_s, b"basestate", &mem, false).unwrap();

        // Diff 1 off the base: page 1 -> 0xBB.
        let mut m1 = mem.clone();
        m1[4096..8192].fill(0xBB);
        let mut dirty1 = DirtyBitmap::new();
        dirty1.mark(0x1000);
        write_diff_snapshot_file(d1_s, base_s, b"state1", &m1, &dirty1).unwrap();

        // Diff 2 off diff 1: page 2 -> 0xCC (a two-deep chain).
        let mut m2 = m1.clone();
        m2[8192..12288].fill(0xCC);
        let mut dirty2 = DirtyBitmap::new();
        dirty2.mark(0x2000);
        write_diff_snapshot_file(d2_s, d1_s, b"state2", &m2, &dirty2).unwrap();

        // Restoring the tip (diff 2) must reproduce base+diff1+diff2 byte-for-byte
        // and yield the tip's state blob.
        let (gm, state) = load_snapshot_chain(d2_s).unwrap();
        assert_eq!(state, b"state2");
        let gm_len = usize::try_from(gm.size_bytes).expect("test memory size fits usize");
        // SAFETY: `gm` owns `gm_len` bytes for the duration of this assertion.
        let recon: &[u8] = unsafe { std::slice::from_raw_parts(gm.as_ptr(), gm_len) };
        assert_eq!(&recon[0..4096], &[0xAA; 4096][..], "page 0 from base");
        assert_eq!(&recon[4096..8192], &[0xBB; 4096][..], "page 1 from diff1");
        assert_eq!(&recon[8192..12288], &[0xCC; 4096][..], "page 2 from diff2");

        // Restoring an intermediate checkpoint (diff 1) reproduces only up to it.
        let (gm1, state1) = load_snapshot_chain(d1_s).unwrap();
        assert_eq!(state1, b"state1");
        let gm1_len = usize::try_from(gm1.size_bytes).expect("test memory size fits usize");
        // SAFETY: `gm1` owns `gm1_len` bytes for the duration of this assertion.
        let recon1: &[u8] = unsafe { std::slice::from_raw_parts(gm1.as_ptr(), gm1_len) };
        assert_eq!(
            &recon1[8192..12288],
            &[0xAA; 4096][..],
            "diff1 has original page 2"
        );

        for p in [base_s, d1_s, d2_s] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    #[test]
    fn diff_snapshot_restore_rejects_page_outside_base_memory() {
        use std::io::Write;

        let dir = private_runtime_dir().unwrap();
        let base = dir.join(format!("t-base-oob-{}.snap", std::process::id()));
        let diff = dir.join(format!("t-diff-oob-{}.snap", std::process::id()));
        let (base_s, diff_s) = (base.to_str().unwrap(), diff.to_str().unwrap());

        let mem: Vec<u8> = vec![0xAA; usize::try_from(crate::config::MIB).expect("MiB fits usize")];
        write_snapshot_file(base_s, b"basestate", &mem, false).unwrap();

        let mut file = std::fs::File::create(diff_s).unwrap();
        file.write_all(b"VMSD").unwrap();
        file.write_all(&SNAPSHOT_VERSION.to_le_bytes()).unwrap();
        file.write_all(&(base.file_name().unwrap().to_string_lossy().len() as u32).to_le_bytes())
            .unwrap();
        file.write_all(base.file_name().unwrap().to_string_lossy().as_bytes())
            .unwrap();
        file.write_all(&(b"state".len() as u64).to_le_bytes())
            .unwrap();
        file.write_all(&crc32fast::hash(b"state").to_le_bytes())
            .unwrap();
        file.write_all(b"state").unwrap();
        file.write_all(&1u32.to_le_bytes()).unwrap();
        file.write_all(&(crate::config::MIB + 4096).to_le_bytes())
            .unwrap();
        file.write_all(&4096u32.to_le_bytes()).unwrap();
        file.write_all(&vec![0xDD; 4096]).unwrap();
        drop(file);

        let err = match load_snapshot_chain(diff_s) {
            Ok(_) => panic!("out-of-bounds diff must fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("outside base guest memory"));

        let _ = std::fs::remove_file(base_s);
        let _ = std::fs::remove_file(diff_s);
    }
}
