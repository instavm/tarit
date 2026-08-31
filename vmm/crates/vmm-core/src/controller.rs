//! VMM controller — manages the single VM lifecycle (1:1 model).
//!
//! One VMM process = one microVM. The controller owns at most one VM at a
//! time. Lifecycle: boot → (pause/resume)* → snapshot/restore → stop.

use crate::config::VmConfig;
use crate::error::{Result, VmmError};
use crate::gc::OwnedScratchFile;
use crate::state::VmState;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tarit_proto::ScratchIdentity;

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
    /// Per-volume queue workers. Declared before their device/eventfd owners so
    /// they are stopped and joined before those resources are dropped.
    pub blk_io_loops: Vec<vmm_devices::virtio::blk_io_loop::BlkIoLoop>,
    /// virtio-net host<->tap I/O loops. Each is dropped (which stops+joins the
    /// thread) before `keep_alive_fds`, whose EventFds the loops reference as
    /// their TX kick fd — so declaration order here is load-bearing.
    pub net_io_loops: Vec<vmm_devices::virtio::net_io_loop::NetIoLoop>,
    pub blk_devices: Vec<Arc<vmm_devices::virtio::blk_transport::VirtioBlkMmio>>,
    pub net_devices: Vec<Arc<vmm_devices::virtio::net_transport::VirtioNetMmio>>,
    pub balloon_device: Option<Arc<vmm_devices::virtio::balloon::VirtioBalloonMmio>>,
    /// Reasserts the balloon's level IRQ after guest EOI when another virtio
    /// cause arrived during the interrupt-service window.
    pub balloon_irq_resample: Option<BalloonIrqResample>,
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
    /// Monotonic, process-unique id for this instance. Long-running operations
    /// that must release the controller lock (live snapshot) record it before
    /// and re-check it after, so a concurrent stop + create cannot get an old
    /// VM's `RunningVm` grafted onto the new instance in the slot.
    pub generation: u64,
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
        cleanup_private_runtime_dir();
    }
}

/// Scratch files owned by one VM instance.
#[derive(Debug, Default)]
pub struct VmTransientFiles {
    /// Live snapshots taken of this VM. Each one is kept until the VM stops or
    /// its ownership is transferred, so taking a second live snapshot never
    /// deletes the path an earlier call already handed to a caller.
    live_snapshots: Vec<OwnedScratchFile>,
    suspend_snapshot: Option<OwnedScratchFile>,
    snapshots: Vec<OwnedScratchFile>,
    owned_overlays: Vec<OwnedScratchFile>,
}

impl VmTransientFiles {
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    fn from_owned_overlays(owned_overlays: Vec<OwnedScratchFile>) -> Self {
        Self {
            owned_overlays,
            ..Self::default()
        }
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    fn add_live_snapshot_owned(&mut self, path: OwnedScratchFile) {
        self.live_snapshots.push(path);
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    fn set_suspend_snapshot(&mut self, path: OwnedScratchFile) {
        if let Some(old) = self.suspend_snapshot.replace(path) {
            remove_owned_scratch_file(&old);
        }
    }

    fn add_snapshot(&mut self, snapshot: OwnedScratchFile) {
        self.snapshots.push(snapshot);
    }

    fn release(&mut self, path: &str, identity: &ScratchIdentity) -> bool {
        let path = Path::new(path);
        let Some((kind, index)) = self.find_owned(path, identity) else {
            return false;
        };
        match kind {
            OwnedScratchKind::LiveSnapshot => {
                self.live_snapshots.remove(index);
            }
            OwnedScratchKind::SuspendSnapshot => {
                self.suspend_snapshot.take();
            }
            OwnedScratchKind::Snapshot => {
                self.snapshots.remove(index);
            }
            OwnedScratchKind::Overlay => {
                self.owned_overlays.remove(index);
            }
        }
        true
    }

    fn find_owned(
        &self,
        path: &Path,
        identity: &ScratchIdentity,
    ) -> Option<(OwnedScratchKind, usize)> {
        if let Some(index) = self
            .live_snapshots
            .iter()
            .position(|file| file.path() == path && file.matches_identity(identity))
        {
            return Some((OwnedScratchKind::LiveSnapshot, index));
        }
        if self
            .suspend_snapshot
            .as_ref()
            .is_some_and(|file| file.path() == path && file.matches_identity(identity))
        {
            return Some((OwnedScratchKind::SuspendSnapshot, 0));
        }
        if let Some(index) = self
            .snapshots
            .iter()
            .position(|file| file.path() == path && file.matches_identity(identity))
        {
            return Some((OwnedScratchKind::Snapshot, index));
        }
        self.owned_overlays
            .iter()
            .position(|file| file.path() == path && file.matches_identity(identity))
            .map(|index| (OwnedScratchKind::Overlay, index))
    }

    fn cleanup(&mut self) {
        for path in self.live_snapshots.drain(..) {
            remove_owned_scratch_file(&path);
        }
        if let Some(path) = self.suspend_snapshot.take() {
            remove_owned_scratch_file(&path);
        }
        for path in self.snapshots.drain(..) {
            remove_owned_scratch_file(&path);
        }
        for path in self.owned_overlays.drain(..) {
            remove_owned_scratch_file(&path);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum OwnedScratchKind {
    LiveSnapshot,
    SuspendSnapshot,
    Snapshot,
    Overlay,
}

/// Allocate the next process-unique VM generation.
fn next_vm_generation() -> u64 {
    static VM_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    VM_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn remove_owned_scratch_file(file: &OwnedScratchFile) {
    match file.remove() {
        Ok(true) => log::info!("removed VM scratch file {}", file.path().display()),
        Ok(false) => {}
        Err(e) => log::warn!("remove VM scratch file {}: {e}", file.path().display()),
    }
}

fn replay_consumed_dirty(vm: &VmInstance, dirty: Option<&vmm_memory_backend::dirty::DirtyBitmap>) {
    let (Some(guest_mem), Some(dirty)) = (vm.guest_mem.as_ref(), dirty) else {
        return;
    };
    for pfn in dirty.dirty_pfns() {
        guest_mem.mark_host_dirty(pfn.saturating_mul(4096), 4096);
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn replay_live_snapshot_dirty(
    controller: &VmmController,
    generation: u64,
    dirty: &vmm_memory_backend::dirty::DirtyBitmap,
) {
    let slot = controller.lock();
    if let Some(vm) = slot.as_ref().filter(|vm| vm.generation == generation) {
        replay_consumed_dirty(vm, Some(dirty));
    }
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
struct OwnedOverlayGuard {
    files: Vec<OwnedScratchFile>,
    armed: bool,
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
impl OwnedOverlayGuard {
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    fn create(config: &VmConfig) -> Result<Self> {
        let mut files = Vec::new();
        for path in config
            .volumes
            .iter()
            .filter_map(|volume| volume.overlay.as_deref())
            .map(PathBuf::from)
        {
            match OwnedScratchFile::create_new(&path) {
                Ok(file) => files.push(file),
                Err(error) => {
                    for file in files.drain(..) {
                        remove_owned_scratch_file(&file);
                    }
                    return Err(VmmError::Snapshot(format!(
                        "create private overlay {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        Ok(Self { files, armed: true })
    }

    fn from_created(files: Vec<OwnedScratchFile>) -> Self {
        Self { files, armed: true }
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    fn disarm(mut self) -> Vec<OwnedScratchFile> {
        self.armed = false;
        std::mem::take(&mut self.files)
    }
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
impl Drop for OwnedOverlayGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for file in &self.files {
            remove_owned_scratch_file(file);
        }
    }
}

/// The VMM controller — owns at most one VM (1:1 process model).
pub struct VmmController {
    vm: Arc<Mutex<Option<VmInstance>>>,
    lifecycle: Mutex<Option<LifecycleOp>>,
    /// Serialize every guest-agent request across both the framed vsock path
    /// and UART fallback.  The vsock channel has its own gate, but that alone
    /// does not prevent two callers from racing into UART while vsock is
    /// disconnected or reconnecting and splicing shell input in its 64-byte
    /// FIFO.
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    guest_agent: Mutex<()>,
}

#[cfg_attr(
    not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")),
    allow(dead_code)
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleOp {
    Create,
    Snapshot,
    LiveSnapshot,
    Restore,
    Suspend,
    Pause,
    Resume,
    Stop,
}

impl LifecycleOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Snapshot => "snapshot",
            Self::LiveSnapshot => "live_snapshot",
            Self::Restore => "restore",
            Self::Suspend => "suspend",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Stop => "stop",
        }
    }
}

#[derive(Debug)]
struct LifecycleGuard<'a> {
    state: &'a Mutex<Option<LifecycleOp>>,
    op: LifecycleOp,
}

impl Drop for LifecycleGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.as_ref() == Some(&self.op) {
            *state = None;
        }
    }
}

impl VmmController {
    /// Configure deterministic per-volume storage latency for Linux/KVM
    /// integration tests. This API is absent from production builds.
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        feature = "boot",
        feature = "test-failpoints"
    ))]
    pub fn set_test_block_service_delay(
        &self,
        volume_index: usize,
        delay: std::time::Duration,
    ) -> Result<()> {
        let slot = self.lock();
        let running = slot
            .as_ref()
            .and_then(|vm| vm.running.as_ref())
            .ok_or_else(|| VmmError::InvalidConfig("no running VM".into()))?;
        let device = running.blk_devices.get(volume_index).ok_or_else(|| {
            VmmError::InvalidConfig(format!("volume index {volume_index} is out of range"))
        })?;
        device
            .set_test_service_delay(delay)
            .map_err(|error| VmmError::Device(format!("set block service delay: {error}")))
    }

    /// Pause and immediately resume only the vCPUs. Used to prove that a slow
    /// storage backend cannot occupy the KVM execution/control thread.
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        feature = "boot",
        feature = "test-failpoints"
    ))]
    pub fn test_vcpu_pause_round_trip(&self) -> Result<()> {
        let slot = self.lock();
        let vm = slot
            .as_ref()
            .ok_or_else(|| VmmError::InvalidConfig("no running VM".into()))?;
        if pause_running_vcpus(vm)? {
            resume_running_vcpus(vm)?;
        }
        Ok(())
    }

    pub fn new() -> Self {
        Self {
            vm: Arc::new(Mutex::new(None)),
            lifecycle: Mutex::new(None),
            #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
            guest_agent: Mutex::new(()),
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

    fn begin_lifecycle(&self, op: LifecycleOp) -> Result<LifecycleGuard<'_>> {
        let mut state = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = *state {
            return Err(VmmError::InvalidConfig(format!(
                "lifecycle operation already in progress: {}",
                active.as_str()
            )));
        }
        *state = Some(op);
        drop(state);
        Ok(LifecycleGuard {
            state: &self.lifecycle,
            op,
        })
    }

    /// Boot the single VM. Error if one already exists.
    pub fn create(&self, config: VmConfig) -> Result<()> {
        let _lifecycle = self.begin_lifecycle(LifecycleOp::Create)?;
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
                generation: next_vm_generation(),
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
                generation: next_vm_generation(),
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
        let _lifecycle = self.begin_lifecycle(LifecycleOp::Snapshot)?;
        let mut slot = self.lock();
        let vm = slot
            .as_mut()
            .ok_or_else(|| VmmError::InvalidConfig("no VM (boot first)".into()))?;

        let path_buf = unique_scratch_snapshot_path("vmm-snap")?;
        let path = path_buf.to_string_lossy().into_owned();
        let mut owned_snapshot = staged_owned_output(&path_buf)?;

        let state_before = vm.state;

        // Stop every producer of guest-memory writes while we capture device state
        // and RAM. Pause vCPUs first so the guest cannot enqueue new net/vsock work
        // after an I/O pump has acknowledged its pause. The pumps are then parked
        // before capture begins. Resume in the inverse order: vCPUs first, then the
        // pumps, so a completion interrupt cannot be delivered to a paused LAPIC.
        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        let paused_here = if state_before == VmState::Running {
            match pause_running_vcpus(vm) {
                Ok(paused) => paused,
                Err(error) => {
                    remove_owned_scratch_file(&owned_snapshot);
                    return Err(error);
                }
            }
        } else {
            false
        };
        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        let io_paused_here = paused_here && pause_running_io(vm);

        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        let mut consumed_dirty = None;
        #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
        let consumed_dirty = None;
        let snapshot_result = (|| -> Result<usize> {
            // Fold the vCPU state each thread captured during the pause above into
            // the stored state blob, so this snapshot is faithfully resumable (not
            // just a memory image). The boot-time blob already carries entry/
            // mem_size/kernel/cmdline/vcpus/volumes/net; we only attach the live
            // vCPU register+MSR+LAPIC state (BSP + each AP for SMP). If nothing was
            // captured (e.g. a VM that never ran), the blob is written unchanged.
            #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
            capture_live_state(vm)?;

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
                    consumed_dirty = Some(dirty.clone());
                    let parent = vm.last_snapshot.clone().unwrap_or_default();
                    write_scratch_diff_snapshot_file(
                        &owned_snapshot,
                        &parent,
                        &state_blob,
                        mem_slice,
                        &dirty,
                    )
                }
                #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
                {
                    Ok(0)
                }
            } else {
                write_scratch_snapshot_file(&owned_snapshot, &state_blob, mem_slice, false)?;
                // Enable dirty logging (idempotent) + drain the initial bitmap so the
                // next snapshot can diff against this full baseline.
                #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
                {
                    let enabled = match vm.running.as_ref() {
                        Some(r) => {
                            let ok = r.kvm_vm.enable_dirty_logging().is_ok();
                            if ok {
                                if let Ok(mut dirty) = r.kvm_vm.read_dirty() {
                                    if let Some(guest_mem) = vm.guest_mem.as_ref() {
                                        let host_dirty = guest_mem.drain_host_dirty();
                                        dirty.merge(&host_dirty);
                                    }
                                    consumed_dirty = Some(dirty);
                                }
                            }
                            ok
                        }
                        None => false,
                    };
                    if enabled {
                        vm.dirty_logging = true;
                    }
                }
                Ok(mem_slice.len())
            }
        })();

        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        let resume_result = if paused_here && state_before == VmState::Running {
            resume_running_vcpus(vm)
        } else {
            Ok(())
        };
        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        if io_paused_here {
            resume_running_io(vm);
        }

        vm.state = state_before;
        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        let snapshot_result = match resume_result {
            Ok(()) => snapshot_result,
            Err(error) => Err(error),
        };
        let mem_len = match snapshot_result {
            Ok(mem_len) => mem_len,
            Err(error) => {
                replay_consumed_dirty(vm, consumed_dirty.as_ref());
                remove_owned_scratch_file(&owned_snapshot);
                return Err(error);
            }
        };
        if let Err(error) = persist_owned_output(&mut owned_snapshot, &path_buf) {
            replay_consumed_dirty(vm, consumed_dirty.as_ref());
            remove_owned_scratch_file(&owned_snapshot);
            return Err(error);
        }
        vm.last_snapshot = Some(path.clone());
        vm.transient_files.add_snapshot(owned_snapshot);
        log::info!("VM: snapshot saved to {path} ({mem_len} bytes mem, diff={diff})");
        Ok(path)
    }

    /// Transfer an exact, currently-owned scratch artifact to the caller.
    pub fn release_scratch(&self, path: &str, identity: ScratchIdentity) -> Result<()> {
        let mut slot = self.lock();
        let vm = slot
            .as_mut()
            .ok_or_else(|| VmmError::InvalidConfig("no VM (boot first)".into()))?;
        if vm.transient_files.release(path, &identity) {
            log::info!("released VM scratch file {path}");
            Ok(())
        } else {
            Err(VmmError::InvalidConfig(format!(
                "scratch artifact is not owned by this VM: {path}"
            )))
        }
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn create_live(&self, config: VmConfig) -> Result<()> {
        let _lifecycle = self.begin_lifecycle(LifecycleOp::Create)?;
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
        let overlay_guard = OwnedOverlayGuard::create(&config)?;

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
            blk_io_evts,
            blk_mmio_bases,
            nets,
            rng_irq,
            vsock,
            balloon,
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

        // Expose a fresh generation value from the first boot. The eventfd is
        // registered now and kept alive for the VM lifetime; it is triggered
        // only when a snapshot is restored into a new VM incarnation.
        let vmgenid = crate::vmgenid::VmGenId::new(&mem)?;
        kvm_vm.register_irqfd(vmgenid.eventfd(), crate::vmgenid::VMGENID_GSI)?;
        let vmgenid_evt = vmgenid.into_eventfd();

        // Route each block queue kick away from the vCPU and into its dedicated
        // storage worker. Failure is fatal: silently falling back to blocking
        // file I/O on the vCPU would violate control-plane latency isolation.
        for (i, evt) in irq_evts.iter().enumerate() {
            let irq = 5 + i as u32;
            kvm_vm.register_irqfd(evt, irq)?;
        }
        let mut blk_io_loops = Vec::with_capacity(blks.len());
        for (i, ((device, io_evt), mmio_base)) in
            blks.iter().zip(blk_io_evts).zip(blk_mmio_bases).enumerate()
        {
            kvm_vm.register_ioeventfd_datamatch(mmio_base + 0x50, &io_evt, 0)?;
            use std::os::fd::AsRawFd;
            let io_loop = vmm_devices::virtio::blk_io_loop::spawn_blk_io_loop(
                Arc::clone(device),
                io_evt.as_raw_fd(),
            )
            .map_err(|error| {
                VmmError::Device(format!("spawn block I/O worker for volume {i}: {error}"))
            })?;
            blk_io_loops.push(io_loop);
            irq_evts.push(io_evt);
        }
        // Keep the VM Generation ID eventfd alive, but only after the block
        // loop above. It has its own fixed GSI and must never be enumerated as
        // a volume interrupt.
        irq_evts.push(vmgenid_evt);

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
        // spawn its host<->tap I/O loop. The guest's TX kick lands on the I/O
        // thread instead of exiting the vCPU. NetIoLoop + Tap are kept alive
        // in RunningVm and dropped (loop first) on stop.
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

        let balloon_device = balloon.as_ref().map(|wired| wired.device.clone());
        let balloon_irq_resample = match balloon {
            Some(wired) => {
                kvm_vm.register_irqfd_with_resample(
                    &wired.irq_evt,
                    &wired.resample_evt,
                    wired.irq,
                )?;
                let loop_handle = BalloonIrqResample::spawn(
                    wired.device,
                    wired
                        .irq_evt
                        .try_clone()
                        .map_err(|error| VmmError::Kvm(format!("balloon irq clone: {error}")))?,
                    wired.resample_evt,
                )?;
                irq_evts.push(wired.irq_evt);
                Some(loop_handle)
            }
            None => None,
        };

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
            generation: next_vm_generation(),
            state: VmState::Running,
            created_at: std::time::Instant::now(),
            last_snapshot: None,
            transient_files: VmTransientFiles::from_owned_overlays(overlay_guard.disarm()),
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
                blk_io_loops,
                net_io_loops,
                blk_devices: blks,
                net_devices,
                balloon_device,
                balloon_irq_resample,
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
        let _lifecycle = self.begin_lifecycle(LifecycleOp::Create)?;
        Err(VmmError::Kvm("create_live needs Linux+KVM+boot".into()))
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn live_snapshot(
        &self,
        snapshot_config: crate::live_snapshot::LiveSnapshotConfig,
    ) -> Result<crate::live_snapshot::LiveSnapshotResult> {
        let _lifecycle = self.begin_lifecycle(LifecycleOp::LiveSnapshot)?;
        let (running, mem, base_blob, generation, overlay_source) = {
            let mut slot = self.lock();
            let vm = slot
                .as_mut()
                .ok_or_else(|| VmmError::InvalidConfig("no VM".into()))?;
            if vm.state != VmState::Running {
                return Err(VmmError::InvalidConfig(
                    "live snapshot requires a running VM".into(),
                ));
            }
            let mem = vm
                .guest_mem
                .clone()
                .ok_or_else(|| VmmError::Memory("no guest memory".into()))?;
            let base_blob = vm.state_blob.clone().unwrap_or_default();
            let running = vm
                .running
                .take()
                .ok_or_else(|| VmmError::InvalidConfig("VM not running".into()))?;
            let overlay_sources = vm
                .config
                .volumes
                .iter()
                .filter_map(|volume| volume.overlay.clone())
                .collect::<Vec<_>>();
            if overlay_sources.len() > 1 {
                vm.running = Some(running);
                return Err(VmmError::Snapshot(
                    "atomic live snapshot currently supports at most one writable disk overlay"
                        .into(),
                ));
            }
            (
                running,
                mem,
                base_blob,
                vm.generation,
                overlay_sources.into_iter().next(),
            )
        };

        // The controller lock is released for the whole pre-copy loop so other
        // operations are not blocked for seconds. `capture_state` runs inside
        // the final all-vCPU pause, which makes the state blob coherent
        // with the memory image (a blob captured before the loop would carry
        // boot-time registers against post-boot memory).
        //
        // `quiesce` parks every host device worker that can write guest memory
        // (block, network, and vsock) for the final stop, so no page can go
        // stale between the residual copy and device-state capture.
        let quiesce = |pause: bool| set_running_io_paused(&running, pause);
        let memory_stage_path = match unique_scratch_snapshot_path("vmm-live-memory") {
            Ok(path) => path,
            Err(error) => {
                let mut slot = self.lock();
                if let Some(vm) = slot
                    .as_mut()
                    .filter(|vm| vm.generation == generation && vm.running.is_none())
                {
                    vm.running = Some(running);
                }
                return Err(error);
            }
        };
        let memory_stage = match staged_owned_output(&memory_stage_path) {
            Ok(stage) => stage,
            Err(error) => {
                let mut slot = self.lock();
                if let Some(vm) = slot
                    .as_mut()
                    .filter(|vm| vm.generation == generation && vm.running.is_none())
                {
                    vm.running = Some(running);
                }
                return Err(error);
            }
        };
        if let Err(error) = prepare_live_snapshot_stage(memory_stage.file()) {
            remove_owned_scratch_file(&memory_stage);
            let mut slot = self.lock();
            if let Some(vm) = slot
                .as_mut()
                .filter(|vm| vm.generation == generation && vm.running.is_none())
            {
                vm.running = Some(running);
            }
            return Err(error);
        }
        let mut live_overlay = None;
        let vcpu_threads = std::iter::once(&running.vcpu_thread)
            .chain(running.ap_threads.iter())
            .collect::<Vec<_>>();
        let snap_result = crate::live_snapshot::live_snapshot(
            &running.kvm_vm,
            &mem,
            &vcpu_threads,
            &snapshot_config,
            memory_stage.file(),
            &quiesce,
            || {
                if let Some(source) = overlay_source.as_deref() {
                    live_overlay = Some(capture_live_overlay(Path::new(source))?);
                }
                capture_live_state_blob(&running, &base_blob)
            },
        );

        // Put the VM back only if the slot still holds the same instance. A
        // concurrent stop + create would otherwise get this VM's threads and
        // devices grafted onto an unrelated instance.
        let mut reclaimed = Some(running);
        {
            let mut slot = self.lock();
            match slot.as_mut() {
                Some(vm) if vm.generation == generation && vm.running.is_none() => {
                    vm.running = reclaimed.take();
                }
                _ => {
                    log::warn!("live_snapshot: VM replaced during the snapshot; discarding it");
                }
            }
        }
        // Dropping the stale `RunningVm` stops its vCPU and I/O threads.
        drop(reclaimed);

        let output = match snap_result {
            Ok(output) => output,
            Err(error) => {
                remove_owned_scratch_file(&memory_stage);
                if let Some(overlay) = live_overlay.as_ref() {
                    remove_owned_scratch_file(overlay);
                }
                return Err(error);
            }
        };
        if let Err(error) = inject_live_snapshot_controller_failure("precopy_complete") {
            replay_live_snapshot_dirty(self, generation, &output.consumed_dirty);
            remove_owned_scratch_file(&memory_stage);
            if let Some(overlay) = live_overlay.as_ref() {
                remove_owned_scratch_file(overlay);
            }
            return Err(error);
        }
        let mut result = output.result;
        result.overlay_path = live_overlay
            .as_ref()
            .map(|overlay| overlay.path().to_string_lossy().into_owned());

        let live_path = unique_scratch_snapshot_path("vmm-live").inspect_err(|_| {
            replay_live_snapshot_dirty(self, generation, &output.consumed_dirty);
            if let Some(overlay) = live_overlay.as_ref() {
                remove_owned_scratch_file(overlay);
            }
        })?;
        let live_path_s = live_path.to_string_lossy().into_owned();
        let mut owned_live_path = staged_owned_output(&live_path).inspect_err(|_| {
            replay_live_snapshot_dirty(self, generation, &output.consumed_dirty);
            if let Some(overlay) = live_overlay.as_ref() {
                remove_owned_scratch_file(overlay);
            }
        })?;
        prepare_live_snapshot_stage(owned_live_path.file()).inspect_err(|_| {
            replay_live_snapshot_dirty(self, generation, &output.consumed_dirty);
            remove_owned_scratch_file(&owned_live_path);
            if let Some(overlay) = live_overlay.as_ref() {
                remove_owned_scratch_file(overlay);
            }
        })?;
        let write = write_scratch_snapshot_file_from_memory_file(
            &owned_live_path,
            &output.state_blob,
            memory_stage.file(),
            result.mem_bytes,
            false,
        )
        .and_then(|manifest| {
            inject_live_snapshot_controller_failure("snapshot_written")?;
            Ok(manifest)
        });
        remove_owned_scratch_file(&memory_stage);
        let manifest = match write {
            Ok(manifest) => manifest,
            Err(error) => {
                replay_live_snapshot_dirty(self, generation, &output.consumed_dirty);
                remove_owned_scratch_file(&owned_live_path);
                if let Some(overlay) = live_overlay.as_ref() {
                    remove_owned_scratch_file(overlay);
                }
                return Err(error);
            }
        };
        if let Err(error) = persist_owned_output(&mut owned_live_path, &live_path) {
            replay_live_snapshot_dirty(self, generation, &output.consumed_dirty);
            remove_owned_scratch_file(&owned_live_path);
            if let Some(overlay) = live_overlay.as_ref() {
                remove_owned_scratch_file(overlay);
            }
            return Err(error);
        }
        if let Err(error) = inject_live_snapshot_controller_failure("snapshot_published") {
            replay_live_snapshot_dirty(self, generation, &output.consumed_dirty);
            remove_owned_scratch_file(&owned_live_path);
            if let Some(overlay) = live_overlay.as_ref() {
                remove_owned_scratch_file(overlay);
            }
            return Err(error);
        }
        result.snapshot_path.clone_from(&live_path_s);

        let integrity_path = match unique_scratch_snapshot_path("vmm-live-integrity") {
            Ok(path) => path,
            Err(error) => {
                replay_live_snapshot_dirty(self, generation, &output.consumed_dirty);
                remove_owned_scratch_file(&owned_live_path);
                if let Some(overlay) = live_overlay.as_ref() {
                    remove_owned_scratch_file(overlay);
                }
                return Err(error);
            }
        };
        let mut owned_integrity_path = match staged_owned_output(&integrity_path) {
            Ok(path) => path,
            Err(error) => {
                replay_live_snapshot_dirty(self, generation, &output.consumed_dirty);
                remove_owned_scratch_file(&owned_live_path);
                if let Some(overlay) = live_overlay.as_ref() {
                    remove_owned_scratch_file(overlay);
                }
                return Err(error);
            }
        };
        let encoded_manifest = manifest.encode().map_err(|error| {
            VmmError::Snapshot(format!("encode live snapshot integrity: {error}"))
        });
        let write_integrity = encoded_manifest.and_then(|encoded| {
            use std::io::Write as _;
            owned_integrity_path
                .file()
                .try_clone()
                .and_then(|mut file| file.write_all(&encoded).and_then(|()| file.sync_all()))
                .map_err(|error| {
                    VmmError::Snapshot(format!("write live snapshot integrity: {error}"))
                })?;
            inject_live_snapshot_controller_failure("integrity_written")
        });
        if let Err(error) = write_integrity
            .and_then(|()| persist_owned_output(&mut owned_integrity_path, &integrity_path))
        {
            replay_live_snapshot_dirty(self, generation, &output.consumed_dirty);
            remove_owned_scratch_file(&owned_integrity_path);
            remove_owned_scratch_file(&owned_live_path);
            if let Some(overlay) = live_overlay.as_ref() {
                remove_owned_scratch_file(overlay);
            }
            return Err(error);
        }
        if let Err(error) = inject_live_snapshot_controller_failure("integrity_published") {
            replay_live_snapshot_dirty(self, generation, &output.consumed_dirty);
            remove_owned_scratch_file(&owned_integrity_path);
            remove_owned_scratch_file(&owned_live_path);
            if let Some(overlay) = live_overlay.as_ref() {
                remove_owned_scratch_file(overlay);
            }
            return Err(error);
        }
        result.integrity_path = Some(integrity_path.to_string_lossy().into_owned());

        let mut owned_live_path = Some(owned_live_path);
        let mut owned_integrity_path = Some(owned_integrity_path);
        {
            let mut slot = self.lock();
            if let Some(vm) = slot.as_mut().filter(|vm| vm.generation == generation) {
                let owned_live_path = owned_live_path.take().ok_or_else(|| {
                    VmmError::Snapshot(
                        "live snapshot ownership disappeared before registration".into(),
                    )
                })?;
                vm.transient_files.add_live_snapshot_owned(owned_live_path);
                let owned_integrity_path = owned_integrity_path.take().ok_or_else(|| {
                    VmmError::Snapshot(
                        "live snapshot integrity ownership disappeared before registration".into(),
                    )
                })?;
                vm.transient_files
                    .add_live_snapshot_owned(owned_integrity_path);
                if let Some(overlay) = live_overlay.take() {
                    vm.transient_files.add_live_snapshot_owned(overlay);
                }
                // KVM_GET_DIRTY_LOG cleared every bit this snapshot consumed.
                // Replay them into the host-dirty tracker, which snapshot()
                // merges into the KVM dirty set, so a later diff snapshot still
                // carries the pages the live snapshot observed.
                if let Some(guest_mem) = vm.guest_mem.as_ref() {
                    for pfn in output.consumed_dirty.dirty_pfns() {
                        guest_mem.mark_host_dirty(
                            pfn.saturating_mul(crate::live_snapshot::PAGE_SIZE),
                            crate::live_snapshot::PAGE_SIZE,
                        );
                    }
                }
                // Dirty logging stays enabled after a live snapshot, exactly as
                // it does after a full snapshot. Record it so diff snapshots and
                // teardown see the real state of the VM.
                vm.dirty_logging = true;
            }
        }
        if let Some(owned_live_path) = owned_live_path {
            remove_owned_scratch_file(&owned_live_path);
        }
        if let Some(owned_integrity_path) = owned_integrity_path {
            remove_owned_scratch_file(&owned_integrity_path);
        }
        if let Some(overlay) = live_overlay.as_ref() {
            remove_owned_scratch_file(overlay);
        }
        log::info!(
            "VM: live snapshot — {} rounds, {} pages copied, {} residual, {:?} termination, {:?} downtime, {:?} total",
            result.rounds,
            result.pages_copied,
            result.final_dirty_pages,
            result.termination,
            result.downtime,
            result.elapsed
        );
        Ok(result)
    }

    /// API-facing live snapshot: run with the default config and return the
    /// on-disk snapshot path, mirroring what `snapshot()` returns.
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn live_snapshot_to_path(&self) -> Result<String> {
        self.live_snapshot(crate::live_snapshot::LiveSnapshotConfig::default())
            .map(|result| result.snapshot_path)
    }

    /// API-facing atomic live snapshot paths. When the VM has a writable disk
    /// upper it is reflinked during the same final stop as RAM and device state.
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn live_snapshot_to_paths(&self) -> Result<(String, Option<String>, Option<String>)> {
        self.live_snapshot(crate::live_snapshot::LiveSnapshotConfig::default())
            .map(|result| {
                (
                    result.snapshot_path,
                    result.overlay_path,
                    result.integrity_path,
                )
            })
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
    pub fn live_snapshot_to_path(&self) -> Result<String> {
        Err(VmmError::Kvm("live snapshot needs Linux+KVM+boot".into()))
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
    pub fn live_snapshot_to_paths(&self) -> Result<(String, Option<String>, Option<String>)> {
        Err(VmmError::Kvm("live snapshot needs Linux+KVM+boot".into()))
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn restore(&self, snapshot_path: &str, overlay: Option<String>) -> Result<()> {
        self.restore_with_overrides(
            snapshot_path,
            overlay,
            None,
            tarit_proto::RestoreMemoryPolicy::Auto,
        )
    }

    /// Restore a snapshot while explicitly replacing host-bound resources.
    /// Network bindings are never inferred or merged: a networked snapshot
    /// requires a same-cardinality replacement so stale tap/IP assignments
    /// cannot be reused on a different host.
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn restore_with_overrides(
        &self,
        snapshot_path: &str,
        overlay: Option<String>,
        net_override: Option<Vec<crate::config::NetConfig>>,
        memory_policy: tarit_proto::RestoreMemoryPolicy,
    ) -> Result<()> {
        self.restore_with_integrity(snapshot_path, overlay, net_override, memory_policy, None)
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn restore_with_integrity(
        &self,
        snapshot_path: &str,
        overlay: Option<String>,
        net_override: Option<Vec<crate::config::NetConfig>>,
        memory_policy: tarit_proto::RestoreMemoryPolicy,
        memory_integrity: Option<tarit_proto::MemoryIntegrity>,
    ) -> Result<()> {
        self.restore_with_resource_overrides(
            snapshot_path,
            overlay,
            net_override,
            None,
            memory_policy,
            memory_integrity,
        )
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn restore_with_resource_overrides(
        &self,
        snapshot_path: &str,
        overlay: Option<String>,
        net_override: Option<Vec<crate::config::NetConfig>>,
        volume_override: Option<Vec<crate::config::VolumeConfig>>,
        memory_policy: tarit_proto::RestoreMemoryPolicy,
        memory_integrity: Option<tarit_proto::MemoryIntegrity>,
    ) -> Result<()> {
        use std::time::Instant;

        let _lifecycle = self.begin_lifecycle(LifecycleOp::Restore)?;
        let start = Instant::now();
        let restored =
            restore_snapshot_with_policy(snapshot_path, memory_policy, memory_integrity.as_ref())?;
        let RestoredSnapshot {
            mem,
            state_blob,
            snapshot_version,
            lazy_restore,
        } = restored;
        let mem_len = usize::try_from(mem.size_bytes)
            .map_err(|_| VmmError::Memory("restored memory too large".into()))?;

        // Deserialize the state blob (shared owned `StateBlob`) to recover the
        // kernel path/cmdline/vcpus, the attached volumes/net, and any captured
        // vCPU state for a faithful resume.
        let (mut saved, balloon_state, compatibility) =
            decode_state_blob(&state_blob).ok_or_else(|| {
                VmmError::Snapshot("snapshot state blob is malformed or unsupported".into())
            })?;
        let has_compatibility_manifest = compatibility.is_some();
        validate_snapshot_compatibility(snapshot_version, compatibility.as_ref())?;

        let (kernel_path, cmdline, vcpus, volumes, net) = (
            saved.kernel_path.clone(),
            saved.cmdline.clone(),
            saved.vcpus,
            saved.volumes.clone(),
            saved.net.clone(),
        );
        let vcpus = u8::try_from(vcpus).map_err(|_| {
            VmmError::InvalidConfig(format!("snapshot vcpu count too large: {vcpus}"))
        })?;

        // Recover the boot entry + the captured vCPU state (if this was a live
        // snapshot). With the full state we can reconstruct a *running* VM that
        // resumes exactly where it paused; without it we restore a paused,
        // memory-only image (the fast-boot / exec-via-fresh-boot fallback).
        let entry = saved.entry;
        let vcpu_full: Option<crate::vcpu_setup::VcpuFullState> =
            decode_snapshot_component(saved.vcpu_full.as_deref(), "BSP vCPU")?;
        let vm_full: Option<crate::kvm::VmFullState> =
            decode_snapshot_component(saved.vm_full.as_deref(), "VM")?;
        if has_compatibility_manifest && vcpu_full.is_some() && saved.serial_runtime.is_none() {
            return Err(VmmError::Snapshot(
                "live snapshot is missing complete UART runtime state".into(),
            ));
        }
        // AP vCPU states (SMP restore, phase B). Each entry is a postcard
        // VcpuFullState for AP id 1..N; empty for a uniprocessor snapshot.
        let ap_states: Vec<crate::vcpu_setup::VcpuFullState> = saved
            .vcpu_full_aps
            .iter()
            .enumerate()
            .map(|(index, bytes)| {
                postcard::from_bytes(bytes).map_err(|error| {
                    VmmError::Snapshot(format!(
                        "snapshot AP vCPU {} state is malformed: {error}",
                        index + 1
                    ))
                })
            })
            .collect::<Result<_>>()?;
        validate_restored_runtime_shape(
            &saved,
            balloon_state.is_some(),
            vcpu_full.is_some(),
            vm_full.is_some(),
            ap_states.len(),
            vcpus,
        )?;
        // UART register programming, so the restored serial re-arms the guest's
        // RX interrupt (post-restore exec fix). Default for pre-serial blobs.
        let serial_state = saved.serial.clone();
        let virtio_blk = saved.virtio_blk.clone();
        let virtio_net = saved.virtio_net.clone();
        let vsock_state = saved.vsock.clone();

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
        apply_restore_network_override(&mut config, net_override)?;
        apply_restore_volume_override(&mut config, volume_override)?;
        let overlay_guard = match overlay.as_deref() {
            Some(target) => prepare_restore_overlay(&config, target)?,
            None => OwnedOverlayGuard::from_created(Vec::new()),
        };
        apply_restore_overlay(&mut config, overlay)?;
        config.validate()?;

        // Keep the owned state blob aligned with the restored config. Future
        // snapshots start from this blob and only patch in live device/vCPU
        // state, so leaving the golden overlay here would make clone snapshots
        // point back at the shared golden upper layer.
        saved.volumes = config.volumes.clone();
        saved.net = config.net.clone();
        let state_blob = encode_state_blob(&saved, balloon_state.as_ref()).map_err(|error| {
            VmmError::Snapshot(format!("re-encode restored snapshot state: {error}"))
        })?;

        // With captured vCPU state, rebuild a *running* VM (fresh KVM VM over the
        // restored memory + devices, the vCPU state re-applied, and the vCPU
        // thread resumed). A live snapshot must fail closed if reconstruction
        // fails; publishing a paused memory-only VM would hide incompatibility
        // and expose partially restored state.
        let (running, state) = match vcpu_full.as_ref() {
            Some(fs) => {
                let restored = RestoredRuntimeState {
                    vcpu: fs,
                    aps: &ap_states,
                    vm: vm_full.as_ref(),
                    serial: &serial_state,
                    serial_runtime: saved.serial_runtime.as_ref(),
                    virtio_blk: &virtio_blk,
                    virtio_net: &virtio_net,
                    vsock: vsock_state.as_ref(),
                    balloon: balloon_state.as_ref(),
                };
                let running = build_running_vm(mem.clone(), &config, restored, entry)?;
                (Some(running), VmState::Running)
            }
            None => (None, VmState::Paused),
        };
        let resumed = running.is_some();

        let mut slot = self.lock();
        *slot = Some(VmInstance {
            generation: next_vm_generation(),
            state,
            created_at: std::time::Instant::now(),
            last_snapshot: None,
            transient_files: VmTransientFiles::from_owned_overlays(overlay_guard.disarm()),
            dirty_logging: false,
            config,
            guest_mem: Some(mem),
            state_blob: Some(state_blob),
            mem_dump: None,
            lazy_restore,
            running,
        });
        drop(slot);
        if resumed {
            if let Err(error) = await_clone_repair_barrier(self) {
                // A clone whose kernel/userspace state was not repaired must
                // never become externally usable. Taking the instance drops
                // its vCPUs, devices, lazy handler, and private overlays.
                let failed = self.lock().take();
                drop(failed);
                return Err(error);
            }
        }
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

    #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
    pub fn restore_with_resource_overrides(
        &self,
        _snapshot_path: &str,
        _overlay: Option<String>,
        _net_override: Option<Vec<crate::config::NetConfig>>,
        _volume_override: Option<Vec<crate::config::VolumeConfig>>,
        _memory_policy: tarit_proto::RestoreMemoryPolicy,
        _memory_integrity: Option<tarit_proto::MemoryIntegrity>,
    ) -> Result<()> {
        Err(VmmError::Snapshot("restore needs Linux+KVM+boot".into()))
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
    pub fn restore_with_overrides(
        &self,
        _snapshot_path: &str,
        _overlay: Option<String>,
        _net_override: Option<Vec<crate::config::NetConfig>>,
        _memory_policy: tarit_proto::RestoreMemoryPolicy,
    ) -> Result<()> {
        Err(VmmError::Snapshot("restore needs Linux+KVM+boot".into()))
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
    pub fn restore_with_integrity(
        &self,
        _snapshot_path: &str,
        _overlay: Option<String>,
        _net_override: Option<Vec<crate::config::NetConfig>>,
        _memory_policy: tarit_proto::RestoreMemoryPolicy,
        _memory_integrity: Option<tarit_proto::MemoryIntegrity>,
    ) -> Result<()> {
        Err(VmmError::Snapshot("restore needs Linux+KVM+boot".into()))
    }

    pub fn suspend(&self) -> Result<()> {
        let _lifecycle = self.begin_lifecycle(LifecycleOp::Suspend)?;
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
        let _lifecycle = self.begin_lifecycle(LifecycleOp::Pause)?;
        let mut slot = self.lock();
        let vm = slot
            .as_mut()
            .ok_or_else(|| VmmError::InvalidConfig("no VM".into()))?;
        if vm.state == VmState::Paused {
            return Ok(());
        }
        if vm.state != VmState::Running {
            return Err(VmmError::InvalidConfig(format!(
                "cannot pause a VM in {:?} state",
                vm.state
            )));
        }
        // Actually stop the guest vCPU, not just flip the state enum — a paused
        // VM must stop consuming host CPU (a PaaS pauses idle VMs by the
        // thousand). snapshot() drives the thread directly; the API must too.
        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        pause_running_vcpus(vm)?;
        vm.state = VmState::Paused;
        log::info!("VM paused");
        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        let _lifecycle = self.begin_lifecycle(LifecycleOp::Resume)?;
        let mut slot = self.lock();
        let vm = slot
            .as_mut()
            .ok_or_else(|| VmmError::InvalidConfig("no VM".into()))?;
        if vm.state == VmState::Running {
            return Ok(());
        }
        if vm.state != VmState::Paused {
            return Err(VmmError::InvalidConfig(format!(
                "cannot resume a VM in {:?} state",
                vm.state
            )));
        }
        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        resume_running_vcpus(vm)?;
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
    pub fn set_balloon_target_mib(&self, target_mib: u64) -> Result<(u32, u32)> {
        let pages = target_mib
            .checked_mul(crate::config::MIB / 4096)
            .and_then(|pages| u32::try_from(pages).ok())
            .ok_or_else(|| VmmError::InvalidConfig("balloon target overflows u32 pages".into()))?;
        let slot = self.lock();
        let vm = slot
            .as_ref()
            .ok_or_else(|| VmmError::InvalidConfig("no VM".into()))?;
        let device = vm
            .running
            .as_ref()
            .and_then(|running| running.balloon_device.as_ref())
            .ok_or_else(|| VmmError::InvalidConfig("VM has no active balloon device".into()))?;
        device
            .set_target_pages(pages)
            .map_err(VmmError::InvalidConfig)?;
        Ok((device.target_pages(), device.actual_pages()))
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
    pub fn set_balloon_target_mib(&self, _target_mib: u64) -> Result<(u32, u32)> {
        Err(VmmError::Device(
            "virtio-balloon needs Linux+KVM+boot".into(),
        ))
    }

    pub fn balloon_state(&self) -> Result<(u32, u32)> {
        #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
        {
            let slot = self.lock();
            let vm = slot
                .as_ref()
                .ok_or_else(|| VmmError::InvalidConfig("no VM".into()))?;
            let device = vm
                .running
                .as_ref()
                .and_then(|running| running.balloon_device.as_ref())
                .ok_or_else(|| VmmError::InvalidConfig("VM has no active balloon device".into()))?;
            Ok((device.target_pages(), device.actual_pages()))
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
        Err(VmmError::Device(
            "virtio-balloon needs Linux+KVM+boot".into(),
        ))
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

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn repair_guest_network(&self, network: tarit_proto::GuestNetworkRepair) -> Result<()> {
        use std::net::Ipv4Addr;
        use std::time::{Duration, Instant};

        if network.dns_servers.len() > 4 {
            return Err(VmmError::InvalidConfig(
                "guest network repair supports at most 4 DNS servers".into(),
            ));
        }
        network.addr.parse::<Ipv4Addr>().map_err(|error| {
            VmmError::InvalidConfig(format!("invalid guest network address: {error}"))
        })?;
        network.gateway.parse::<Ipv4Addr>().map_err(|error| {
            VmmError::InvalidConfig(format!("invalid guest network gateway: {error}"))
        })?;
        if network.prefix > 32 {
            return Err(VmmError::InvalidConfig(
                "guest IPv4 network prefix must be at most 32".into(),
            ));
        }
        for dns in &network.dns_servers {
            dns.parse::<Ipv4Addr>().map_err(|error| {
                VmmError::InvalidConfig(format!("invalid guest DNS server: {error}"))
            })?;
        }
        let _guest_agent_guard = self.guest_agent.lock().unwrap_or_else(|e| e.into_inner());
        let start = Instant::now();
        let timeout = Duration::from_secs(5);
        let serial = {
            let slot = self.lock();
            slot.as_ref()
                .filter(|vm| vm.state == VmState::Running)
                .and_then(|vm| vm.running.as_ref())
                .map(|running| running.vcpu_thread.serial.clone())
        }
        .ok_or_else(|| VmmError::Device("guest network repair requires a running VM".into()))?;

        let _ = serial.drain_output();
        let payload = serde_json::to_string(&network)
            .map_err(|error| VmmError::Device(format!("encode guest network repair: {error}")))?;
        let request = format!("\nVMM_REPAIR_NET:{payload}");
        if !serial.send_within(request.as_bytes(), start + timeout) {
            return Err(VmmError::Kvm(
                "guest network repair request stalled on serial input".into(),
            ));
        }

        let mut acc = Vec::new();
        let mut output = Vec::new();
        let mut truncated = false;
        let mut started = false;
        while start.elapsed() < timeout {
            std::thread::sleep(Duration::from_millis(2));
            let chunk = serial.drain_output();
            if chunk.is_empty() {
                continue;
            }
            acc.extend_from_slice(&chunk);
            while let Some(pos) = acc.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = acc.drain(..=pos).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let line = String::from_utf8_lossy(&line);
                if line == "VMM_REPAIR_NET_START" {
                    started = true;
                    continue;
                }
                if started {
                    if let Some(code) = line.strip_prefix("VMM_REPAIR_NET_EXIT=") {
                        let exit_code: i32 = code.trim().parse().unwrap_or(1);
                        if exit_code == 0 {
                            return Ok(());
                        }
                        return Err(VmmError::Device(format!(
                            "guest network repair failed ({exit_code}): {}",
                            finish_exec_output(output, truncated)
                        )));
                    }
                }
                if started {
                    append_exec_output(&mut output, line.as_bytes(), &mut truncated);
                    append_exec_output(&mut output, b"\n", &mut truncated);
                }
            }
            trim_exec_accumulator(&mut acc, started, &mut output, &mut truncated);
        }

        Err(VmmError::Device(format!(
            "guest network repair timed out: {}",
            finish_exec_output(output, truncated)
        )))
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
    pub fn repair_guest_network(&self, _network: tarit_proto::GuestNetworkRepair) -> Result<()> {
        Err(VmmError::Kvm(
            "RepairGuestNetwork needs Linux+KVM+boot".into(),
        ))
    }

    ///
    /// The VM must have been created with a rootfs that runs the VMM guest
    /// agent (which reads from /dev/ttyS0). The controller sends the command
    /// via the serial channel and waits for the `VMM_EXEC_EXIT=` marker.
    ///
    /// If the VM is not running (no background vCPU thread), falls back to
    /// booting a fresh VM with the command in the cmdline.
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    pub fn exec(&self, command: &str, timeout_ms: u64) -> Result<(i32, String, String, u64)> {
        use std::time::{Duration, Instant};

        let _guest_agent_guard = self.guest_agent.lock().unwrap_or_else(|e| e.into_inner());
        let start = Instant::now();
        let timeout = Duration::from_millis(if timeout_ms > 0 { timeout_ms } else { 30000 });

        // Prefer the vsock exec channel when the guest agent has dialed in: it's
        // a dedicated framed stream, so exec never desyncs against ttyS0 console
        // output the way serial does under concurrent IRQ load (validated: 25/25
        // rapid execs + multi-line output clean over vsock). Falls back to serial
        // only when the guest hasn't dialed vsock (older agent / no device) or
        // when the command provably never reached the guest — exec is not
        // replay-safe, so anything ambiguous is surfaced as an error instead of
        // being re-sent on serial where it could run twice. Opt out with
        // VMM_VSOCK_EXEC=0. Clone the Arc and drop the controller lock before
        // the (blocking) exec so other API calls aren't stalled.
        if std::env::var("VMM_VSOCK_EXEC").as_deref() != Ok("0") {
            let vsock_channel = {
                let slot = self.lock();
                slot.as_ref()
                    .and_then(|vm| vm.running.as_ref())
                    .and_then(|r| r.vsock_exec.clone())
            };
            if let Some(vx) = vsock_channel {
                // A fresh guest announces serial readiness before its virtio
                // worker necessarily finishes dialing the framed vsock exec
                // channel. For the first real command only, allow that channel
                // a short bounded grace period. Empty commands are readiness
                // probes and deliberately keep using the short UART protocol.
                if !command.is_empty() {
                    let remaining = timeout.saturating_sub(start.elapsed());
                    let _ = vx.wait_for_initial_connection(remaining.min(Duration::from_secs(2)));
                }
                let remaining = timeout.saturating_sub(start.elapsed());
                match vx.exec(command, remaining) {
                    Some(Ok(r)) => {
                        log::info!("exec '{command}' via vsock → exit={}", r.0);
                        return Ok(r);
                    }
                    Some(Err(crate::vsock_exec::VsockExecError::NotDelivered(e))) => {
                        log::warn!("vsock exec not delivered ({e}); falling back to serial");
                    }
                    Some(Err(e @ crate::vsock_exec::VsockExecError::Ambiguous(_))) => {
                        log::warn!("vsock exec '{command}' failed: {e}");
                        return Err(VmmError::Device(format!("vsock exec failed: {e}")));
                    }
                    None => {} // no guest connection / disabled → serial
                }
            }
        }

        // Check if we have a running VM with a serial channel.
        let (serial_handle, state) = {
            let slot = self.lock();
            let vm = slot
                .as_ref()
                .ok_or_else(|| VmmError::InvalidConfig("no VM (create first)".into()))?;
            (
                vm.running.as_ref().map(|r| r.vcpu_thread.serial.clone()),
                vm.state,
            )
        };

        if let Some(serial) = serial_handle {
            // Discard any stale output left in the channel by a previous exec
            // (e.g. one that timed out) so it can't be misread as this command's
            // response.
            let _ = serial.drain_output();

            // A readiness probe must not depend on `/bin/sh` and must stay
            // short enough to cross the UART while a freshly booted 8250
            // driver switches modes. The nonce prevents a stale reply from a
            // prior timed-out probe from being accepted as current readiness.
            if command.is_empty() {
                static PROBE_SEQ: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let token = format!(
                    "{:08x}{:08x}",
                    std::process::id(),
                    PROBE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u32
                );
                let expected = format!("VMM_PROBE_OK:{token}");
                let probe = format!("\n\n\n\nVMM_PROBE:{token}\n");
                if !serial.send_within(probe.as_bytes(), start + timeout) {
                    return Err(VmmError::Kvm(
                        "readiness probe: guest serial input stalled".into(),
                    ));
                }
                let mut acc = Vec::new();
                while start.elapsed() < timeout {
                    std::thread::sleep(Duration::from_millis(2));
                    acc.extend_from_slice(&serial.drain_output());
                    while let Some(pos) = acc.iter().position(|&byte| byte == b'\n') {
                        let mut line: Vec<u8> = acc.drain(..=pos).collect();
                        line.pop();
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                        if line == expected.as_bytes() {
                            return Ok((
                                0,
                                String::new(),
                                String::new(),
                                start.elapsed().as_millis() as u64,
                            ));
                        }
                    }
                    if acc.len() > 4096 {
                        acc.drain(..acc.len() - 4096);
                    }
                }
                return Err(VmmError::Kvm(format!(
                    "readiness probe timed out after {timeout:?}"
                )));
            }

            // Tag the exec with a nonce echoed by the guest shell before the
            // command runs. A previously timed-out exec keeps running in the
            // guest and emits VMM_EXEC_START/VMM_EXEC_EXIT= markers later;
            // without the nonce those stale markers get attributed to this
            // exec (bogus instant completions with the wrong exit code) and
            // this exec's own markers to the next one, cascading. The leading
            // '\n' terminates any partially delivered stale line; the agent
            // ignores empty lines.
            static EXEC_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let nonce = format!(
                "VMM_EXEC_NONCE={}_{:x}",
                std::process::id(),
                EXEC_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            // Run the user command in its own `sh -c` so the nonce echo cannot
            // be swallowed by a parse error in the command itself; exit code
            // and output semantics are unchanged.
            let quoted = command.replace('\'', "'\\''");
            // A freshly opened Linux 8250 consumer can absorb a few pending RX
            // bytes while switching the tty to raw mode. Pad with ignored empty
            // lines so that startup loss cannot eat the protocol prefix (seen
            // on c8i as `M_EXEC:` and a guaranteed first-command timeout).
            let cmd = format!("\n\n\n\n\n\n\n\nVMM_EXEC:echo {nonce}; sh -c '{quoted}'");

            // The emulated UART RX FIFO holds 64 bytes; a one-shot enqueue
            // silently truncates longer commands (the un-terminated fragment
            // then splices with the next exec's bytes). Feed the FIFO as the
            // guest drains it, bounded by the exec deadline.
            if !serial.send_within(cmd.as_bytes(), start + timeout) {
                let err = "exec: guest did not accept the command (serial input stalled)";
                log::warn!("{err}");
                return Err(VmmError::Kvm(err.into()));
            }

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
            // Only lines after our nonce belong to this exec; anything before
            // it (including EXIT markers) is leftovers from an earlier exec.
            let mut confirmed = false;

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
                        continue;
                    }
                    if line_str == nonce {
                        confirmed = true;
                        continue;
                    }
                    if let Some(code) = line_str.strip_prefix("VMM_EXEC_EXIT=") {
                        if !confirmed {
                            log::warn!(
                                "exec: ignoring stale completion (exit={}) from an earlier timed-out exec",
                                code.trim()
                            );
                            continue;
                        }
                        let exit_code: i32 = code.trim().parse().unwrap_or(0);
                        let duration_ms = start.elapsed().as_millis() as u64;
                        let output_str = finish_exec_output(output, truncated);
                        log::info!("exec: '{command}' → exit={exit_code} {duration_ms}ms");
                        return Ok((exit_code, output_str, String::new(), duration_ms));
                    }
                    if confirmed {
                        append_exec_output(&mut output, &line, &mut truncated);
                        append_exec_output(&mut output, b"\n", &mut truncated);
                    }
                }
                trim_exec_accumulator(&mut acc, confirmed, &mut output, &mut truncated);
            }

            let duration_ms = start.elapsed().as_millis() as u64;
            let output_str = finish_exec_output(output, truncated);
            if confirmed {
                log::warn!("exec: timed out after {duration_ms}ms");
                return Ok((-1, output_str, String::new(), duration_ms));
            }
            log::warn!("exec: '{command}' got no response from guest agent after {duration_ms}ms");
            return Err(VmmError::Kvm(format!(
                "exec timed out after {timeout:?} — no response from guest agent"
            )));
        }

        if Self::fresh_boot_exec_allowed(state) {
            self.exec_fresh_boot(command, timeout_ms)
        } else {
            Err(VmmError::Kvm(format!(
                "exec unavailable without a live guest channel while VM is {state:?}"
            )))
        }
    }

    #[cfg(any(
        test,
        all(target_arch = "x86_64", target_os = "linux", feature = "boot")
    ))]
    fn fresh_boot_exec_allowed(state: VmState) -> bool {
        state == VmState::Created
    }

    #[cfg(any(
        test,
        all(target_arch = "x86_64", target_os = "linux", feature = "boot")
    ))]
    fn fresh_boot_timeout_secs(timeout_ms: u64) -> u64 {
        if timeout_ms == 0 {
            10
        } else {
            timeout_ms.div_ceil(1_000)
        }
    }

    /// Fallback exec: boot a fresh VM with the command baked into cmdline.
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    fn exec_fresh_boot(
        &self,
        command: &str,
        timeout_ms: u64,
    ) -> Result<(i32, String, String, u64)> {
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
        let timeout_secs = Self::fresh_boot_timeout_secs(timeout_ms);
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
        Ok((0, String::new(), String::new(), duration_ms))
    }

    #[cfg(not(all(target_arch = "x86_64", target_os = "linux", feature = "boot")))]
    pub fn exec(&self, _command: &str, _timeout_ms: u64) -> Result<(i32, String, String, u64)> {
        Err(VmmError::Kvm("exec needs Linux+KVM+boot".into()))
    }

    /// Stop the VM and clear the slot.
    pub fn stop(&self) -> Result<()> {
        let _lifecycle = self.begin_lifecycle(LifecycleOp::Stop)?;
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

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn build_clone_repair_v3_command(
    encoded_nonce: &str,
    host_realtime: std::time::Duration,
) -> String {
    use std::fmt::Write as _;

    const COMMAND_PREFIX: &str = "__TARIT_CLONE_REPAIR_V3__";
    let mut command = String::with_capacity(COMMAND_PREFIX.len() + encoded_nonce.len() + 24);
    command.push_str(COMMAND_PREFIX);
    command.push_str(encoded_nonce);
    write!(
        command,
        "{:016x}{:08x}",
        host_realtime.as_secs(),
        host_realtime.subsec_nanos()
    )
    .expect("writing timestamp hex to a String cannot fail");
    command
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn await_clone_repair_barrier(controller: &VmmController) -> Result<()> {
    use std::fmt::Write as _;
    use std::time::{Duration, Instant};

    const RESPONSE: &str = "TARIT_CLONE_REPAIR_V3_OK";
    const TIMEOUT: Duration = Duration::from_secs(30);

    // Generate this after restore on the host. Guest hwrng buffers and the
    // kernel CRNG are part of the captured RAM image, so neither is a safe
    // source of the first post-clone uniqueness input.
    let mut nonce = [0u8; 48];
    crate::vmgenid::fill_random(&mut nonce)?;
    // The public incarnation identifier doubles as the guest-visible boot ID.
    // Set RFC 4122 version/variant bits while retaining 122 random bits.
    nonce[32 + 6] = (nonce[32 + 6] & 0x0f) | 0x40;
    nonce[32 + 8] = (nonce[32 + 8] & 0x3f) | 0x80;
    let mut encoded_nonce = String::with_capacity(nonce.len() * 2);
    for byte in &nonce {
        write!(encoded_nonce, "{byte:02x}").expect("writing hex to a String cannot fail");
    }
    let expected_clone_id = encoded_nonce[encoded_nonce.len() - 32..].to_owned();
    nonce.fill(0);

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let channel = {
            let slot = controller.lock();
            slot.as_ref()
                .and_then(|vm| vm.running.as_ref())
                .and_then(|running| running.vsock_exec.clone())
        }
        .ok_or_else(|| VmmError::Device("restored VM has no clone repair channel".into()))?;

        if channel.is_connected() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let host_realtime = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| VmmError::Device("host realtime is before the Unix epoch".into()))?;
            let command = build_clone_repair_v3_command(&encoded_nonce, host_realtime);
            match channel.exec(&command, remaining) {
                Some(Ok((0, stdout, _, _))) => {
                    let mut fields = stdout.split_whitespace();
                    let marker = fields.next();
                    let clone_id = fields.next();
                    let valid_id = clone_id.is_some_and(|id| {
                        id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    });
                    if marker == Some(RESPONSE)
                        && valid_id
                        && clone_id == Some(expected_clone_id.as_str())
                        && fields.next().is_none()
                    {
                        log::info!("clone entropy repair barrier completed");
                        return Ok(());
                    }
                    return Err(VmmError::Device(
                        "guest agent returned an invalid clone repair acknowledgement".into(),
                    ));
                }
                Some(Ok((exit_code, _, stderr, _))) => {
                    let stage = if exit_code == -1 {
                        let diagnostic = controller
                            .exec(
                                "pid=$(cat /run/tarit/clone-workload.pid 2>/dev/null || true); \
                                 printf 'stage='; cat /run/tarit/clone-repair-stage 2>/dev/null || true; \
                                 printf 'clone_id='; cat /run/tarit/clone-id 2>/dev/null || true; \
                                 printf 'workload_repaired='; cat /run/tarit/clone-workload-repaired 2>/dev/null || true; \
                                 printf 'pid=%s\\n' \"$pid\"; \
                                 if [ -n \"$pid\" ] && kill -0 \"$pid\" 2>/dev/null; then \
                                   printf 'wchan='; cat \"/proc/$pid/wchan\" 2>/dev/null || true; printf '\\n'; \
                                   grep -E '^(State|voluntary_ctxt_switches|nonvoluntary_ctxt_switches):' \"/proc/$pid/status\" 2>/dev/null || true; \
                                 fi",
                                5000,
                            )
                            .ok()
                            .and_then(|(code, stdout, _, _)| (code == 0).then_some(stdout));
                        if let Some(diagnostic) = diagnostic.as_deref() {
                            log::warn!("clone repair timeout diagnostic: {}", diagnostic.trim());
                        }
                        diagnostic
                            .as_deref()
                            .and_then(|value| value.lines().next())
                            .and_then(|line| line.strip_prefix("stage="))
                            .filter(|value| !value.is_empty())
                            .unwrap_or("unavailable")
                            .to_owned()
                    } else {
                        "reported".into()
                    };
                    return Err(VmmError::Device(format!(
                        "guest clone repair failed with status {exit_code} at stage {stage}: {stderr}"
                    )));
                }
                Some(Err(error)) => {
                    return Err(VmmError::Device(format!(
                        "guest clone repair exchange failed: {error}"
                    )));
                }
                None => {}
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    Err(VmmError::Device(
        "guest did not complete clone entropy repair before admission".into(),
    ))
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
        for io_loop in running.blk_io_loops.iter_mut() {
            io_loop.stop();
        }
        for l in running.net_io_loops.iter_mut() {
            l.stop();
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn pause_running_vcpus(vm: &VmInstance) -> Result<bool> {
    if let Some(r) = vm.running.as_ref() {
        let vcpu_threads = std::iter::once(&r.vcpu_thread)
            .chain(r.ap_threads.iter())
            .collect::<Vec<_>>();
        if vcpu_threads.is_empty() {
            return Ok(false);
        }
        for vcpu_thread in &vcpu_threads {
            if let Err(error) = vcpu_thread.request_snapshot_pause() {
                for armed in &vcpu_threads {
                    armed.resume();
                }
                return Err(error);
            }
        }
        for vcpu_thread in &vcpu_threads {
            if let Err(error) = vcpu_thread.wait_snapshot_paused() {
                for armed in &vcpu_threads {
                    armed.resume();
                }
                return Err(error);
            }
        }
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Park or release every host I/O thread that can mutate guest memory without
/// running on a vCPU. The pause methods synchronously acknowledge, so once this
/// returns with `paused = true`, device state and RAM are stable as long as the
/// vCPUs are also paused.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn set_running_io_paused(running: &RunningVm, paused: bool) {
    if paused {
        for io_loop in &running.blk_io_loops {
            io_loop.pause();
        }
        for io_loop in &running.net_io_loops {
            io_loop.pause();
        }
        if let Some(pump) = running.vsock_pump.as_ref() {
            pump.pause();
        }
    } else {
        for io_loop in &running.blk_io_loops {
            io_loop.resume();
        }
        for io_loop in &running.net_io_loops {
            io_loop.resume();
        }
        if let Some(pump) = running.vsock_pump.as_ref() {
            pump.resume();
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn pause_running_io(vm: &VmInstance) -> bool {
    let Some(running) = vm.running.as_ref() else {
        return false;
    };
    set_running_io_paused(running, true);
    true
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn resume_running_io(vm: &VmInstance) {
    if let Some(running) = vm.running.as_ref() {
        set_running_io_paused(running, false);
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn resume_running_vcpus(vm: &VmInstance) -> Result<()> {
    if let Some(r) = vm.running.as_ref() {
        let vcpu_threads = std::iter::once(&r.vcpu_thread)
            .chain(r.ap_threads.iter())
            .collect::<Vec<_>>();
        for vcpu_thread in &vcpu_threads {
            vcpu_thread.resume();
        }
        for vcpu_thread in &vcpu_threads {
            vcpu_thread.wait_snapshot_resumed()?;
        }
    }
    Ok(())
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn capture_live_state(vm: &mut VmInstance) -> Result<()> {
    let Some(running) = vm.running.as_ref() else {
        return Ok(());
    };
    let existing = vm
        .state_blob
        .as_deref()
        .ok_or_else(|| VmmError::Snapshot("running VM is missing its base state blob".into()))?;
    vm.state_blob = Some(capture_live_state_blob(running, existing)?);
    Ok(())
}

/// Fold the vCPU registers and device state captured during the current pause
/// into `existing`, returning the updated state blob.
///
/// Call this with every vCPU paused: the returned blob is only coherent with a
/// memory image taken during the same pause. Every runtime component is
/// required: publishing an incomplete live snapshot only defers the failure to
/// restore time and risks treating partial runtime state as a memory-only VM.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn capture_live_state_blob(running: &RunningVm, existing: &[u8]) -> Result<Vec<u8>> {
    let captured = running
        .vcpu_thread
        .captured_state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| VmmError::Snapshot("live snapshot is missing BSP vCPU state".into()))?;
    let ap_captured: Vec<Vec<u8>> = running
        .ap_threads
        .iter()
        .enumerate()
        .map(|(index, ap)| {
            ap.captured_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .ok_or_else(|| {
                    VmmError::Snapshot(format!(
                        "live snapshot is missing AP vCPU {} state",
                        index + 1
                    ))
                })
        })
        .collect::<Result<_>>()?;
    let vm_state = running
        .kvm_vm
        .capture_vm_state()
        .map_err(|error| VmmError::Snapshot(format!("capture in-kernel VM state: {error}")))?;
    let vm_state = postcard::to_allocvec(&vm_state)
        .map_err(|error| VmmError::Snapshot(format!("serialize in-kernel VM state: {error}")))?;
    let serial_state = vmm_devices::persist::Persist::save(&*running.vcpu_thread.serial);
    let serial_runtime = running.vcpu_thread.serial.runtime_state();
    let virtio_blk = capture_virtio_blk_states(&running.blk_devices)?;
    let virtio_net = capture_virtio_net_states(&running.net_devices)?;
    let balloon = running
        .balloon_device
        .as_ref()
        .map(|device| vmm_devices::persist::Persist::save(&**device))
        .ok_or_else(|| VmmError::Snapshot("live snapshot is missing virtio-balloon".into()))?;
    let vsock_pump = running
        .vsock_pump
        .as_ref()
        .ok_or_else(|| VmmError::Snapshot("live snapshot is missing virtio-vsock".into()))?;
    let vsock_state = vmm_devices::persist::Persist::try_save(&*vsock_pump.device)
        .map_err(|error| VmmError::Snapshot(format!("capture virtio-vsock state: {error}")))?;

    let (mut b, _previous_balloon, compatibility) =
        decode_state_blob(existing).ok_or_else(|| {
            VmmError::Snapshot("running VM base state blob is malformed or unsupported".into())
        })?;
    if let Some(compatibility) = compatibility {
        compatibility.validate()?;
    }
    b.vcpu_full = Some(captured);
    b.vcpu_full_aps = ap_captured;
    b.vm_full = Some(vm_state);
    b.serial = serial_state;
    b.serial_runtime = Some(serial_runtime);
    b.virtio_blk = virtio_blk;
    b.virtio_net = virtio_net;
    b.vsock = Some(vsock_state);
    encode_state_blob(&b, Some(&balloon))
        .map_err(|error| VmmError::Snapshot(format!("serialize live device state: {error}")))
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn suspend_vm_in_place(vm: &mut VmInstance) -> Result<()> {
    if vm.state == VmState::Suspended {
        return Ok(());
    }

    let state_before = vm.state;
    let paused_here = if state_before == VmState::Running {
        pause_running_vcpus(vm)?
    } else {
        false
    };
    vm.state = VmState::Paused;
    let result = (|| -> Result<()> {
        capture_live_state(vm)?;

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
        let target_path = PathBuf::from(&path);
        let mut owned_snapshot = staged_owned_output(&target_path)?;

        let mem_slice = {
            // SAFETY: guest_mem owns this mmap for the lifetime of `vm`; the vCPUs are
            // paused, and this read also resolves any older lazy-restore faults before
            // we unregister the previous UFFD below.
            unsafe { std::slice::from_raw_parts(mem_ptr.cast_const(), mem_len) }
        };
        if let Err(e) = write_scratch_snapshot_file(&owned_snapshot, &state_blob, mem_slice, false)
        {
            remove_owned_scratch_file(&owned_snapshot);
            return Err(e);
        }
        if let Err(error) = persist_owned_output(&mut owned_snapshot, &target_path) {
            remove_owned_scratch_file(&owned_snapshot);
            return Err(error);
        }

        let file = match owned_snapshot.file().try_clone() {
            Ok(file) => file,
            Err(e) => {
                remove_owned_scratch_file(&owned_snapshot);
                return Err(VmmError::Snapshot(format!("open {path}: {e}")));
            }
        };
        if let Err(e) = file.sync_all() {
            remove_owned_scratch_file(&owned_snapshot);
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
                remove_owned_scratch_file(&owned_snapshot);
                return Err(VmmError::Snapshot(format!("UFFD suspend restore: {e}")));
            }
        };
        guest_mem.set_lazy_page_discard(lazy_restore.page_discard());

        if let Err(e) = vmm_memory_backend::madvise_dontneed(mem_ptr, mem_len) {
            drop(lazy_restore);
            remove_owned_scratch_file(&owned_snapshot);
            return Err(VmmError::Snapshot(format!("release guest RAM: {e}")));
        }
        drop_file_cache(&file, layout.mem_offset, layout.mem_len);
        vm.transient_files.set_suspend_snapshot(owned_snapshot);
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
            resume_running_vcpus(vm)?;
        }
    }
    result
}

pub(crate) fn private_runtime_dir() -> Result<PathBuf> {
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

fn cleanup_private_runtime_dir() {
    use std::io::ErrorKind;

    let dir = std::env::temp_dir()
        .join(".vmm-runtime")
        .join(format!("vmm-{}", std::process::id()));
    let metadata = match std::fs::symlink_metadata(&dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return,
        Err(error) => {
            log::warn!("stat private runtime dir {}: {error}", dir.display());
            return;
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        log::warn!(
            "refusing to remove replaced private runtime dir: {}",
            dir.display()
        );
        return;
    }
    if let Err(error) = std::fs::remove_dir(&dir) {
        if !matches!(
            error.kind(),
            ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
        ) {
            log::warn!("remove private runtime dir {}: {error}", dir.display());
        }
    }
}

#[cfg(all(
    feature = "test-failpoints",
    target_arch = "x86_64",
    target_os = "linux",
    feature = "boot"
))]
fn inject_live_snapshot_controller_failure(phase: &str) -> Result<()> {
    if std::env::var_os("TARIT_TEST_LIVE_SNAPSHOT_FAIL_PHASE").as_deref()
        == Some(std::ffi::OsStr::new(phase))
    {
        return Err(VmmError::Snapshot(format!(
            "injected live snapshot failure at {phase}"
        )));
    }
    Ok(())
}

#[cfg(all(
    not(feature = "test-failpoints"),
    target_arch = "x86_64",
    target_os = "linux",
    feature = "boot"
))]
fn inject_live_snapshot_controller_failure(_phase: &str) -> Result<()> {
    Ok(())
}

/// Btrfs CoW turns the final residual's 4 KiB positioned updates into extent
/// splitting and can stretch guest downtime from milliseconds to minutes.
/// Mark the still-empty private stage NOCOW; Btrfs continues to permit the
/// aligned FICLONERANGE used when the finished image is published.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn prepare_live_snapshot_stage(file: &std::fs::File) -> Result<()> {
    use std::os::fd::AsRawFd;

    const BTRFS_SUPER_MAGIC: libc::c_long = 0x9123_683E_u32 as libc::c_long;
    const FS_IOC_GETFLAGS: libc::Ioctl = 0x8008_6601_u32 as libc::Ioctl;
    const FS_IOC_SETFLAGS: libc::Ioctl = 0x4008_6602;
    const FS_NOCOW_FL: libc::c_long = 0x0080_0000;

    // SAFETY: statfs only writes the provided initialized structure and the
    // descriptor remains live for the duration of the call.
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut fs) } != 0 {
        return Err(VmmError::Snapshot(format!(
            "inspect live snapshot staging filesystem: {}",
            std::io::Error::last_os_error()
        )));
    }
    if fs.f_type as u64 != BTRFS_SUPER_MAGIC as u64 {
        return Ok(());
    }

    let mut flags: libc::c_long = 0;
    // SAFETY: these filesystem ioctls read/write one machine-word flag value;
    // the file is private, empty, and remains open throughout both calls.
    if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) } != 0 {
        return Err(VmmError::Snapshot(format!(
            "read Btrfs live snapshot staging flags: {}",
            std::io::Error::last_os_error()
        )));
    }
    flags |= FS_NOCOW_FL;
    if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_SETFLAGS, &flags) } != 0 {
        return Err(VmmError::Snapshot(format!(
            "enable NOCOW for Btrfs live snapshot staging: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn unique_scratch_snapshot_path(prefix: &str) -> Result<PathBuf> {
    unique_runtime_file_path(prefix, "snap")
}

pub(crate) fn unique_runtime_file_path(prefix: &str, suffix: &str) -> Result<PathBuf> {
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

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn unique_runtime_socket_path() -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    // sockaddr_un::sun_path is 108 bytes on Linux. Keep the per-process name
    // compact: timestamp-heavy scratch names can cross that limit when TMPDIR
    // is nested under a test, jail, or container runtime directory.
    const SUN_PATH_BYTES: usize = 108;
    static SOCKET_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SOCKET_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = private_runtime_dir()?.join(format!("vs-{seq:x}.sock"));
    if path.as_os_str().as_bytes().len() >= SUN_PATH_BYTES {
        return Err(VmmError::Device(format!(
            "runtime Unix socket path is too long ({} bytes, maximum 107): {}",
            path.as_os_str().as_bytes().len(),
            path.display()
        )));
    }
    Ok(path)
}

fn staged_output_path(target: &Path) -> Result<PathBuf> {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| VmmError::Snapshot(format!("invalid output path: {}", target.display())))?;
    let suffix = format!(
        "{file_name}.stage-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let parent = target.parent().ok_or_else(|| {
        VmmError::Snapshot(format!(
            "output path has no parent directory: {}",
            target.display()
        ))
    })?;
    Ok(parent.join(format!(".{suffix}")))
}

fn staged_owned_output(target: &Path) -> Result<OwnedScratchFile> {
    let stage = staged_output_path(target)?;
    OwnedScratchFile::create_new(stage).map_err(|error| {
        VmmError::Snapshot(format!(
            "create staged output for {}: {error}",
            target.display()
        ))
    })
}

fn persist_owned_output(owned: &mut OwnedScratchFile, target: &Path) -> Result<()> {
    owned.file().sync_all().map_err(|error| {
        VmmError::Snapshot(format!(
            "sync staged output for {}: {error}",
            target.display()
        ))
    })?;
    owned.rename_to(target).map_err(|error| {
        VmmError::Snapshot(format!(
            "publish staged output {}: {error}",
            target.display()
        ))
    })
}

/// Capture the writable disk upper inside a live snapshot's final stop.
///
/// This path deliberately requires FICLONE. Falling back to an extent or dense
/// copy while vCPUs are paused would make blackout proportional to disk dirtied
/// bytes and defeat the live-fork latency contract.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn capture_live_overlay(source: &Path) -> Result<OwnedScratchFile> {
    use std::os::fd::AsRawFd;

    const FICLONE: libc::Ioctl = 0x4004_9409;
    let source_owned = OwnedScratchFile::adopt_private(source).map_err(|error| {
        VmmError::Snapshot(format!(
            "adopt live overlay {} for atomic snapshot: {error}",
            source.display()
        ))
    })?;
    source_owned.file().sync_all().map_err(|error| {
        VmmError::Snapshot(format!(
            "sync live overlay {} before atomic snapshot: {error}",
            source.display()
        ))
    })?;
    // Reflinks cannot cross filesystem boundaries. Place the VMM-owned scratch
    // beside the live upper so this remains O(1) even when TMPDIR is on the
    // host root filesystem and VM storage is on a dedicated CoW volume.
    let target = crate::gc::owned_overlay_path(
        source.parent().ok_or_else(|| {
            VmmError::Snapshot(format!(
                "live overlay has no parent directory: {}",
                source.display()
            ))
        })?,
        0,
    );
    let mut captured = staged_owned_output(&target)?;
    let cloned = unsafe {
        libc::ioctl(
            captured.file().as_raw_fd(),
            FICLONE,
            source_owned.file().as_raw_fd(),
        )
    };
    if cloned != 0 {
        let error = std::io::Error::last_os_error();
        remove_owned_scratch_file(&captured);
        return Err(VmmError::Snapshot(format!(
            "atomic live disk snapshot requires reflink/FICLONE for {}: {error}",
            source.display()
        )));
    }
    if let Err(error) = persist_owned_output(&mut captured, &target) {
        remove_owned_scratch_file(&captured);
        return Err(error);
    }
    Ok(captured)
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
        log::warn!("snapshot: cannot fadvise file cache; offset too large");
        return;
    };
    let Ok(len) = libc::off_t::try_from(len) else {
        log::warn!("snapshot: cannot fadvise file cache; length too large");
        return;
    };
    // SAFETY: `file.as_raw_fd()` is a valid open fd, and offset/len were checked
    // to fit `off_t`; posix_fadvise does not dereference Rust pointers.
    let rc =
        unsafe { libc::posix_fadvise(file.as_raw_fd(), offset, len, libc::POSIX_FADV_DONTNEED) };
    if rc != 0 {
        log::warn!(
            "snapshot: POSIX_FADV_DONTNEED failed: {}",
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
fn apply_restore_network_override(
    config: &mut VmConfig,
    net_override: Option<Vec<crate::config::NetConfig>>,
) -> Result<()> {
    match net_override {
        None if !config.net.is_empty() => Err(VmmError::InvalidConfig(
            "networked snapshot restore requires an explicit net replacement".into(),
        )),
        None => Ok(()),
        Some(replacement) if replacement.len() != config.net.len() => {
            Err(VmmError::InvalidConfig(format!(
                "restore net replacement count {} does not match snapshot device count {}",
                replacement.len(),
                config.net.len()
            )))
        }
        Some(replacement) => {
            config.net = replacement;
            // Validate before any tap is opened. VmConfig validation also
            // rejects duplicate taps, MACs, IPs, and host port bindings.
            config.validate()?;
            Ok(())
        }
    }
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn apply_restore_volume_override(
    config: &mut VmConfig,
    volume_override: Option<Vec<crate::config::VolumeConfig>>,
) -> Result<()> {
    let saved_indices = config
        .volumes
        .iter()
        .enumerate()
        .filter_map(|(index, volume)| volume.inherited_fd.map(|_| index))
        .collect::<Vec<_>>();
    let Some(replacement) = volume_override else {
        return if saved_indices.is_empty() {
            Ok(())
        } else {
            Err(VmmError::InvalidConfig(
                "snapshot restore requires replacement descriptors for inherited volumes".into(),
            ))
        };
    };
    if replacement.len() != saved_indices.len() {
        return Err(VmmError::InvalidConfig(format!(
            "restore volume replacement count {} does not match inherited device count {}",
            replacement.len(),
            saved_indices.len()
        )));
    }
    for (index, replacement) in saved_indices.into_iter().zip(replacement) {
        let saved = &config.volumes[index];
        if replacement.inherited_fd.is_none()
            || replacement.overlay.is_some()
            || replacement.path != saved.path
            || replacement.read_only != saved.read_only
        {
            return Err(VmmError::InvalidConfig(format!(
                "restore volume replacement for device {index} changes immutable identity or lacks an inherited descriptor"
            )));
        }
        config.volumes[index] = replacement;
    }
    config.validate()?;
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
fn prepare_restore_overlay(config: &VmConfig, target: &str) -> Result<OwnedOverlayGuard> {
    match OwnedScratchFile::adopt_private(target) {
        Ok(adopted) => {
            // A preseeded target is authoritative. Never reopen the overlay
            // serialized in the RAM image: the source VM may already be gone.
            // But adopting the golden overlay itself would share its writable
            // state with the clone and delete it on stop, so refuse aliases.
            reject_golden_overlay_target(config, target, &adopted)?;
            return Ok(OwnedOverlayGuard::from_created(vec![adopted]));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(VmmError::Snapshot(format!(
                "adopt private restore overlay {target}: {error}"
            )));
        }
    }

    let target_path = PathBuf::from(target);
    let owned = match restore_overlay_seed(config, Some(target))? {
        Some((source, target)) => seed_restore_overlay(&source, &target)?,
        None => {
            let mut owned = staged_owned_output(&target_path)?;
            persist_owned_output(&mut owned, &target_path)?;
            owned
        }
    };
    Ok(OwnedOverlayGuard::from_created(vec![owned]))
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn reject_golden_overlay_target(
    config: &VmConfig,
    target: &str,
    adopted: &OwnedScratchFile,
) -> Result<()> {
    let Some(source) = restore_overlay_volume_index(&config.volumes)
        .and_then(|index| config.volumes[index].overlay.as_deref())
    else {
        return Ok(());
    };
    // Path equality catches the direct case; inode identity catches aliased
    // spellings of the same file. A missing source cannot alias anything.
    let aliases_golden = source == target
        || match OwnedScratchFile::identity_for(Path::new(source)) {
            Ok(identity) => adopted.identity().is_some_and(|adopted| {
                adopted.device == identity.device && adopted.inode == identity.inode
            }),
            Err(_) => false,
        };
    if aliases_golden {
        return Err(VmmError::InvalidConfig(format!(
            "restore overlay must differ from golden overlay: {target}"
        )));
    }
    Ok(())
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn seed_restore_overlay(source: &Path, target: &Path) -> Result<OwnedScratchFile> {
    if source == target {
        return Err(VmmError::InvalidConfig(format!(
            "restore overlay must differ from golden overlay: {}",
            target.display()
        )));
    }

    let mut owned_target = staged_owned_output(target)?;
    let copy_result = (|| -> std::io::Result<()> {
        let source_owned = OwnedScratchFile::adopt_private(source)?;
        let mut source_file = source_owned.file().try_clone()?;
        let mut target_file = owned_target.file().try_clone()?;
        let result = copy_restore_overlay(&mut source_file, &mut target_file);
        drop(target_file);
        drop(source_file);
        drop(source_owned);
        result
    })();

    if let Err(e) = copy_result {
        remove_owned_scratch_file(&owned_target);
        return Err(VmmError::Snapshot(format!(
            "seed restore overlay {} -> {}: {e}",
            source.display(),
            target.display()
        )));
    }
    if let Err(error) = persist_owned_output(&mut owned_target, target) {
        remove_owned_scratch_file(&owned_target);
        return Err(error);
    }
    Ok(owned_target)
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn copy_restore_overlay(
    source: &mut std::fs::File,
    target: &mut std::fs::File,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        const FICLONE: libc::Ioctl = 0x4004_9409;
        let cloned = unsafe { libc::ioctl(target.as_raw_fd(), FICLONE, source.as_raw_fd()) };
        if cloned == 0 {
            return target.sync_all();
        }
        let error = std::io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EOPNOTSUPP) | Some(libc::ENOTTY) | Some(libc::EXDEV) | Some(libc::EINVAL)
        ) {
            return Err(error);
        }
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
fn copy_dense_restore_overlay(
    source: &mut std::fs::File,
    target: &mut std::fs::File,
) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};

    source.seek(SeekFrom::Start(0))?;
    target.seek(SeekFrom::Start(0))?;
    std::io::copy(source, target)?;
    target.sync_all()
}

#[cfg(any(
    test,
    all(target_os = "linux", target_arch = "x86_64", feature = "boot")
))]
fn validate_sparse_extent(offset: u64, data: u64, hole: u64, length: u64) -> std::io::Result<()> {
    if data < offset || hole <= data || hole > length {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid overlay data extent",
        ));
    }
    Ok(())
}

#[cfg(all(
    target_os = "linux",
    any(test, all(target_arch = "x86_64", feature = "boot"))
))]
fn copy_sparse_restore_overlay(
    source: &mut std::fs::File,
    target: &mut std::fs::File,
) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;

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
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!(
                        "reflink and sparse extent discovery are unavailable; refusing dense overlay copy: {error}"
                    ),
                ));
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
        validate_sparse_extent(offset, data, hole, length)?;

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
        SnapshotCreateMode::CreateNew,
    )
}

fn write_scratch_snapshot_file(
    owned_file: &OwnedScratchFile,
    state_blob: &[u8],
    mem_dump: &[u8],
    diff: bool,
) -> Result<()> {
    write_snapshot_to_file(owned_file.file(), state_blob, mem_dump, diff)
}

/// Assemble a full snapshot from a file-backed live pre-copy without ever
/// allocating a second guest-RAM-sized buffer. The CRC field is patched before
/// the private staged artifact is synced and atomically published.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn write_scratch_snapshot_file_from_memory_file(
    owned_file: &OwnedScratchFile,
    state_blob: &[u8],
    memory_file: &std::fs::File,
    mem_len: u64,
    diff: bool,
) -> Result<tarit_proto::IntegrityManifest> {
    use sha2::{Digest as _, Sha256};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;

    const MAGIC: &[u8; 4] = b"VMSN";
    const MEM_CRC_OFFSET: u64 = 28;
    const COPY_BUFFER_BYTES: usize = 4 * 1024 * 1024;
    const WRITEBACK_INTERVAL: u64 = 32 * 1024 * 1024;
    const REFLINK_ALIGNMENT: u64 = 4096;
    const INTEGRITY_CHUNK_BYTES: usize = tarit_proto::INTEGRITY_CHUNK_SIZE as usize;
    const _: () = assert!(COPY_BUFFER_BYTES.is_multiple_of(INTEGRITY_CHUNK_BYTES));

    #[repr(C)]
    struct FileCloneRange {
        src_fd: i64,
        src_offset: u64,
        src_length: u64,
        dest_offset: u64,
    }

    const FICLONERANGE: libc::Ioctl = 0x4020_940D;

    let source_len = memory_file
        .metadata()
        .map_err(|error| VmmError::Snapshot(format!("stat staged RAM: {error}")))?
        .len();
    if source_len != mem_len {
        return Err(VmmError::Snapshot(format!(
            "staged RAM length mismatch: got {source_len}, expected {mem_len}"
        )));
    }

    let state_len = u64::try_from(state_blob.len())
        .map_err(|_| VmmError::Snapshot("state blob too large".into()))?;
    let unaligned_mem_offset = FULL_SNAPSHOT_HEADER_LEN
        .checked_add(state_len)
        .ok_or_else(|| VmmError::Snapshot("snapshot memory offset overflow".into()))?;
    let mem_offset = unaligned_mem_offset
        .checked_add(REFLINK_ALIGNMENT - 1)
        .map(|value| value / REFLINK_ALIGNMENT * REFLINK_ALIGNMENT)
        .ok_or_else(|| VmmError::Snapshot("snapshot memory offset overflow".into()))?;
    let padded_state_len = mem_offset - FULL_SNAPSHOT_HEADER_LEN;
    let padded_state_capacity = usize::try_from(padded_state_len)
        .map_err(|_| VmmError::Snapshot("padded state blob too large".into()))?;
    let mut padded_state = Vec::with_capacity(padded_state_capacity);
    padded_state.extend_from_slice(state_blob);
    padded_state.resize(padded_state_capacity, 0);
    let final_len = mem_offset
        .checked_add(mem_len)
        .ok_or_else(|| VmmError::Snapshot("snapshot length overflow".into()))?;
    let state_crc = crc32fast::hash(&padded_state);
    let flags: u16 = if diff { 1 } else { 0 };

    let mut output = owned_file
        .file()
        .try_clone()
        .map_err(|error| VmmError::Snapshot(format!("clone snapshot output: {error}")))?;
    output
        .set_len(0)
        .map_err(|error| VmmError::Snapshot(format!("truncate snapshot output: {error}")))?;
    output
        .seek(SeekFrom::Start(0))
        .map_err(|error| VmmError::Snapshot(format!("seek snapshot output: {error}")))?;
    output
        .write_all(MAGIC)
        .and_then(|()| output.write_all(&SNAPSHOT_VERSION.to_le_bytes()))
        .and_then(|()| output.write_all(&flags.to_le_bytes()))
        .and_then(|()| output.write_all(&padded_state_len.to_le_bytes()))
        .and_then(|()| output.write_all(&state_crc.to_le_bytes()))
        .and_then(|()| output.write_all(&mem_len.to_le_bytes()))
        .and_then(|()| output.write_all(&0u32.to_le_bytes()))
        .and_then(|()| output.write_all(&padded_state))
        .map_err(|error| VmmError::Snapshot(format!("write snapshot metadata: {error}")))?;

    output
        .set_len(final_len)
        .map_err(|error| VmmError::Snapshot(format!("size streamed snapshot: {error}")))?;
    let clone_range = FileCloneRange {
        src_fd: i64::from(memory_file.as_raw_fd()),
        src_offset: 0,
        src_length: mem_len,
        dest_offset: mem_offset,
    };
    // On a CoW filesystem publication should share the staged RAM extents,
    // not allocate and write a second guest-RAM-sized file just to prepend
    // snapshot metadata. The private stage remains owned until publication.
    let reflinked = unsafe {
        libc::ioctl(
            output.as_raw_fd(),
            FICLONERANGE,
            &clone_range as *const FileCloneRange,
        ) == 0
    };
    if !reflinked {
        let error = std::io::Error::last_os_error();
        log::warn!(
            "live_snapshot: RAM range reflink unavailable ({error}); using streamed publication"
        );
        output
            .seek(SeekFrom::Start(mem_offset))
            .map_err(|error| VmmError::Snapshot(format!("seek snapshot RAM: {error}")))?;
    }

    let mut source = memory_file
        .try_clone()
        .map_err(|error| VmmError::Snapshot(format!("clone staged RAM: {error}")))?;
    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| VmmError::Snapshot(format!("seek staged RAM: {error}")))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut memory_chunk_hashes = Vec::new();
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    let mut copied = 0u64;
    let mut synced = 0u64;
    while copied < mem_len {
        let chunk = (mem_len - copied).min(buffer.len() as u64) as usize;
        source
            .read_exact(&mut buffer[..chunk])
            .map_err(|error| VmmError::Snapshot(format!("read staged RAM: {error}")))?;
        hasher.update(&buffer[..chunk]);
        memory_chunk_hashes.extend(
            buffer[..chunk]
                .chunks(INTEGRITY_CHUNK_BYTES)
                .map(|bytes| -> [u8; 32] { Sha256::digest(bytes).into() }),
        );
        if !reflinked {
            output
                .write_all(&buffer[..chunk])
                .map_err(|error| VmmError::Snapshot(format!("stream snapshot RAM: {error}")))?;
        }
        let chunk_u64 = chunk as u64;
        drop_file_cache(memory_file, copied, chunk_u64);
        copied += chunk_u64;

        if !reflinked && copied - synced >= WRITEBACK_INTERVAL {
            output
                .sync_data()
                .map_err(|error| VmmError::Snapshot(format!("write back snapshot RAM: {error}")))?;
            drop_file_cache(&output, mem_offset + synced, copied - synced);
            synced = copied;
        }
    }

    let mem_crc = hasher.finalize();
    output
        .seek(SeekFrom::Start(MEM_CRC_OFFSET))
        .and_then(|_| output.write_all(&mem_crc.to_le_bytes()))
        .map_err(|error| VmmError::Snapshot(format!("patch snapshot RAM CRC: {error}")))?;
    output
        .sync_data()
        .map_err(|error| VmmError::Snapshot(format!("sync streamed snapshot: {error}")))?;
    if !reflinked && copied > synced {
        drop_file_cache(&output, mem_offset + synced, copied - synced);
    }
    let metadata_capacity = (FULL_SNAPSHOT_HEADER_LEN as usize)
        .checked_add(padded_state.len())
        .ok_or_else(|| VmmError::Snapshot("snapshot metadata length overflow".into()))?;
    let mut metadata = Vec::with_capacity(metadata_capacity);
    metadata.extend_from_slice(MAGIC);
    metadata.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    metadata.extend_from_slice(&flags.to_le_bytes());
    metadata.extend_from_slice(&padded_state_len.to_le_bytes());
    metadata.extend_from_slice(&state_crc.to_le_bytes());
    metadata.extend_from_slice(&mem_len.to_le_bytes());
    metadata.extend_from_slice(&mem_crc.to_le_bytes());
    metadata.extend_from_slice(&padded_state);
    let metadata_chunk_hashes = metadata
        .chunks(INTEGRITY_CHUNK_BYTES)
        .map(|bytes| -> [u8; 32] { Sha256::digest(bytes).into() })
        .collect();
    Ok(tarit_proto::IntegrityManifest {
        chunk_size: tarit_proto::INTEGRITY_CHUNK_SIZE,
        artifacts: vec![
            tarit_proto::ArtifactIntegrity {
                kind: tarit_proto::ArtifactKind::SnapshotMetadata,
                len: mem_offset,
                chunk_hashes: metadata_chunk_hashes,
            },
            tarit_proto::ArtifactIntegrity {
                kind: tarit_proto::ArtifactKind::Ram,
                len: mem_len,
                chunk_hashes: memory_chunk_hashes,
            },
        ],
    })
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum SnapshotCreateMode {
    CreateNew,
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn open_snapshot_output(path: &str, mode: SnapshotCreateMode) -> Result<std::fs::File> {
    match mode {
        SnapshotCreateMode::CreateNew => {
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            options
                .open(path)
                .map_err(|e| VmmError::Snapshot(format!("create {path}: {e}")))
        }
    }
}

#[cfg(any(
    test,
    all(target_arch = "x86_64", target_os = "linux", feature = "boot")
))]
fn write_snapshot_file_with_mode(
    path: &str,
    state_blob: &[u8],
    mem_dump: &[u8],
    diff: bool,
    mode: SnapshotCreateMode,
) -> Result<()> {
    let file = open_snapshot_output(path, mode)?;
    write_snapshot_to_file(&file, state_blob, mem_dump, diff)
}

fn write_snapshot_to_file(
    mut file: &std::fs::File,
    state_blob: &[u8],
    mem_dump: &[u8],
    diff: bool,
) -> Result<()> {
    use std::io::Write;
    const MAGIC: &[u8; 4] = b"VMSN";

    let state_crc = crc32fast::hash(state_blob);
    let mem_crc = crc32fast::hash(mem_dump);
    let flags: u16 = if diff { 1 } else { 0 };
    let state_len = u64::try_from(state_blob.len())
        .map_err(|_| VmmError::Snapshot("state blob too large".into()))?;
    let mem_len = u64::try_from(mem_dump.len())
        .map_err(|_| VmmError::Snapshot("memory image too large".into()))?;

    file.write_all(MAGIC)
        .map_err(|e| VmmError::Snapshot(e.to_string()))?;
    file.write_all(&SNAPSHOT_VERSION.to_le_bytes())
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
        SnapshotCreateMode::CreateNew,
    )
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn write_scratch_diff_snapshot_file(
    owned_file: &OwnedScratchFile,
    parent: &str,
    state_blob: &[u8],
    mem: &[u8],
    dirty: &vmm_memory_backend::dirty::DirtyBitmap,
) -> Result<usize> {
    write_diff_snapshot_to_file(owned_file.file(), parent, state_blob, mem, dirty)
}

#[cfg(all(test, target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn write_diff_snapshot_file_with_mode(
    path: &str,
    parent: &str,
    state_blob: &[u8],
    mem: &[u8],
    dirty: &vmm_memory_backend::dirty::DirtyBitmap,
    mode: SnapshotCreateMode,
) -> Result<usize> {
    let file = open_snapshot_output(path, mode)?;
    write_diff_snapshot_to_file(&file, parent, state_blob, mem, dirty)
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn write_diff_snapshot_to_file(
    mut file: &std::fs::File,
    parent: &str,
    state_blob: &[u8],
    mem: &[u8],
    dirty: &vmm_memory_backend::dirty::DirtyBitmap,
) -> Result<usize> {
    use std::io::Write;
    use vmm_snapshot::diff::build_diff;
    const MAGIC: &[u8; 4] = b"VMSD";

    let diff = build_diff(mem, dirty, Vec::new());
    let mut wr = |b: &[u8]| -> Result<()> {
        file.write_all(b)
            .map_err(|e| VmmError::Snapshot(e.to_string()))
    };
    wr(MAGIC)?;
    wr(&SNAPSHOT_VERSION.to_le_bytes())?;
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
const LEGACY_SNAPSHOT_VERSION: u16 = 1;
const SNAPSHOT_VERSION: u16 = 2;
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

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn open_snapshot_input(path: &Path) -> Result<std::fs::File> {
    let display = path.display();
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|e| VmmError::Snapshot(format!("open {display}: {e}")))?;
    if !file
        .metadata()
        .map_err(|e| VmmError::Snapshot(format!("stat {display}: {e}")))?
        .file_type()
        .is_file()
    {
        return Err(VmmError::Snapshot(format!(
            "snapshot is not a regular file: {display}"
        )));
    }
    Ok(file)
}

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
    version: u16,
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
    if !matches!(version, LEGACY_SNAPSHOT_VERSION | SNAPSHOT_VERSION) {
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
    if file_len > expected_len {
        return Err(VmmError::Snapshot(format!(
            "full snapshot has trailing data: expected {expected_len} bytes, got {file_len}"
        )));
    }
    Ok(FullSnapshotHeader {
        version,
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
    if !(crate::config::MIB..=crate::config::MAX_MEMORY_BYTES).contains(&mem_len) {
        return Err(VmmError::Snapshot(format!(
            "memory image size out of range in {path}: {mem_len} bytes"
        )));
    }
    if !mem_len.is_multiple_of(crate::config::MIB) {
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
fn load_authenticated_memory_manifest(
    anchor: &tarit_proto::MemoryIntegrity,
    header: &[u8; FULL_SNAPSHOT_REST_HEADER_LEN],
    state_blob: &[u8],
    memory_len: u64,
) -> Result<vmm_memory_backend::ChunkIntegrity> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let expected = anchor
        .manifest_sha256
        .strip_prefix("sha256:")
        .ok_or_else(|| VmmError::Snapshot("invalid integrity manifest digest scheme".into()))?;
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(VmmError::Snapshot(
            "invalid integrity manifest SHA-256".into(),
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&anchor.manifest_path)
        .map_err(|error| {
            VmmError::Snapshot(format!(
                "open integrity manifest {}: {error}",
                anchor.manifest_path
            ))
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| VmmError::Snapshot(format!("stat integrity manifest: {error}")))?;
    if !metadata.is_file() || metadata.len() > 128 * 1024 * 1024 {
        return Err(VmmError::Snapshot(
            "unsafe or oversized integrity manifest".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| VmmError::Snapshot(format!("read integrity manifest: {error}")))?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if !constant_time_ascii_eq(actual.as_bytes(), expected.to_ascii_lowercase().as_bytes()) {
        return Err(VmmError::Snapshot(
            "integrity manifest authentication failed".into(),
        ));
    }
    let manifest = tarit_proto::IntegrityManifest::decode(&bytes)
        .map_err(|error| VmmError::Snapshot(format!("parse integrity manifest: {error}")))?;
    let memory = manifest
        .artifact(tarit_proto::ArtifactKind::Ram)
        .ok_or_else(|| VmmError::Snapshot("integrity manifest has no RAM artifact".into()))?;
    if memory.len != memory_len {
        return Err(VmmError::Snapshot(
            "integrity manifest RAM length mismatch".into(),
        ));
    }
    let metadata_integrity = manifest
        .artifact(tarit_proto::ArtifactKind::SnapshotMetadata)
        .ok_or_else(|| VmmError::Snapshot("integrity manifest has no snapshot metadata".into()))?;
    let mut snapshot_metadata = Vec::with_capacity(4 + header.len() + state_blob.len());
    snapshot_metadata.extend_from_slice(b"VMSN");
    snapshot_metadata.extend_from_slice(header);
    snapshot_metadata.extend_from_slice(state_blob);
    verify_integrity_chunks(
        &snapshot_metadata,
        manifest.chunk_size as usize,
        metadata_integrity,
        "snapshot metadata",
    )?;
    Ok(vmm_memory_backend::ChunkIntegrity {
        chunk_size: manifest.chunk_size as usize,
        chunk_hashes: memory.chunk_hashes.clone(),
    })
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn verify_integrity_chunks(
    bytes: &[u8],
    chunk_size: usize,
    expected: &tarit_proto::ArtifactIntegrity,
    what: &str,
) -> Result<()> {
    use sha2::{Digest, Sha256};

    if expected.len != bytes.len() as u64
        || expected.chunk_hashes.len() != bytes.len().div_ceil(chunk_size)
    {
        return Err(VmmError::Snapshot(format!(
            "integrity manifest {what} shape mismatch"
        )));
    }
    for (index, chunk) in bytes.chunks(chunk_size).enumerate() {
        let actual: [u8; 32] = Sha256::digest(chunk).into();
        if actual != expected.chunk_hashes[index] {
            return Err(VmmError::Snapshot(format!(
                "integrity verification failed for {what} chunk {index}"
            )));
        }
    }
    Ok(())
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn constant_time_ascii_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
struct RestoredSnapshot {
    mem: vmm_memory_backend::GuestMemory,
    state_blob: Vec<u8>,
    snapshot_version: u16,
    lazy_restore: Option<vmm_memory_backend::LazyRestore>,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn eager_restore_snapshot(path: &str) -> Result<RestoredSnapshot> {
    let (mem, state_blob, snapshot_version) = load_snapshot_chain(path)?;
    Ok(RestoredSnapshot {
        mem,
        state_blob,
        snapshot_version,
        lazy_restore: None,
    })
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn restore_snapshot_with_policy(
    path: &str,
    memory_policy: tarit_proto::RestoreMemoryPolicy,
    memory_integrity: Option<&tarit_proto::MemoryIntegrity>,
) -> Result<RestoredSnapshot> {
    match memory_policy {
        tarit_proto::RestoreMemoryPolicy::Auto => {
            match try_lazy_restore_full_snapshot(path, memory_integrity) {
                Ok(Some(restored)) => Ok(restored),
                Ok(None) if memory_integrity.is_none() => {
                    log::info!(
                        "restore: eager policy required for this snapshot; replaying eagerly"
                    );
                    eager_restore_snapshot(path)
                }
                Ok(None) => Err(VmmError::Snapshot(
                    "authenticated snapshot is not eligible for lazy restore".into(),
                )),
                Err(error) if memory_integrity.is_none() => {
                    log::warn!(
                        "restore: lazy restore unavailable ({error}); falling back to eager"
                    );
                    eager_restore_snapshot(path)
                }
                Err(error) => Err(error),
            }
        }
        tarit_proto::RestoreMemoryPolicy::Eager if memory_integrity.is_some() => {
            Err(VmmError::Snapshot(
                "authenticated snapshots require chunk-verified lazy restore".into(),
            ))
        }
        tarit_proto::RestoreMemoryPolicy::Eager => eager_restore_snapshot(path),
        tarit_proto::RestoreMemoryPolicy::Lazy => {
            match try_lazy_restore_full_snapshot(path, memory_integrity) {
                Ok(Some(restored)) => Ok(restored),
                Ok(None) => Err(VmmError::Snapshot(
                    "lazy restore requires a full non-diff snapshot with UFFD backing".into(),
                )),
                Err(error) => Err(VmmError::Snapshot(format!(
                    "lazy restore requested but unavailable: {error}"
                ))),
            }
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn try_lazy_restore_full_snapshot(
    path: &str,
    memory_integrity: Option<&tarit_proto::MemoryIntegrity>,
) -> Result<Option<RestoredSnapshot>> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = open_snapshot_input(Path::new(path))?;
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
    let chunk_integrity = if let Some(anchor) = memory_integrity {
        Some(load_authenticated_memory_manifest(
            anchor,
            &hdr,
            &state_blob,
            layout.mem_len,
        )?)
    } else {
        let actual_mem_crc =
            crc32_file_range(&mut file, path, layout.mem_offset, layout.mem_len, "mem")?;
        if actual_mem_crc != header.mem_crc {
            return Err(VmmError::Snapshot(format!(
                "memory CRC mismatch in {path}: got {actual_mem_crc:#010x}, expected {:#010x}",
                header.mem_crc
            )));
        }
        None
    };

    let mem = vmm_memory_backend::GuestMemory::new(layout.mem_len)
        .map_err(|e| VmmError::Memory(e.to_string()))?;
    let lazy_restore = vmm_memory_backend::start_lazy_restore_with_integrity(
        mem.as_ptr() as *mut u8,
        mem_len,
        &file,
        layout.mem_offset,
        layout.mem_len,
        Some(mem.host_dirty_tracker()),
        chunk_integrity,
    )
    .map_err(|e| VmmError::Snapshot(format!("UFFD lazy restore: {e}")))?;
    mem.set_lazy_page_discard(lazy_restore.page_discard());

    log::info!(
        "restore: UFFD lazy full snapshot armed (mem_offset={}, mem_len={})",
        layout.mem_offset,
        layout.mem_len
    );
    Ok(Some(RestoredSnapshot {
        mem,
        state_blob,
        snapshot_version: header.version,
        lazy_restore: Some(lazy_restore),
    }))
}

/// One snapshot file's contents: a full base image, or a diff (parent + pages).
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
enum SnapshotContent {
    Full {
        mem: vmm_memory_backend::GuestMemory,
        state: Vec<u8>,
        version: u16,
    },
    Diff {
        parent: PathBuf,
        state: Vec<u8>,
        version: u16,
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
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| VmmError::Snapshot(format!("stat snapshot {path}: {e}")))?;
    if !metadata.file_type().is_file() {
        return Err(VmmError::Snapshot(format!(
            "snapshot tip must be a regular non-symlink file: {path}"
        )));
    }
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
    let metadata = std::fs::symlink_metadata(&candidate).map_err(|e| {
        VmmError::Snapshot(format!("stat diff parent {}: {e}", candidate.display()))
    })?;
    if !metadata.file_type().is_file() {
        return Err(VmmError::Snapshot(format!(
            "diff parent must be a regular non-symlink file: {}",
            candidate.display()
        )));
    }
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
    let mut file = open_snapshot_input(path)?;
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
        Ok(SnapshotContent::Full {
            mem,
            state,
            version: header.version,
        })
    } else if &magic == b"VMSD" {
        let mut u16b = [0u8; 2];
        rd(&mut file, &mut u16b, "read version")?;
        let version = u16::from_le_bytes(u16b);
        if !matches!(version, LEGACY_SNAPSHOT_VERSION | SNAPSHOT_VERSION) {
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
        let consumed = file
            .stream_position()
            .map_err(|e| VmmError::Snapshot(format!("tell {path_display}: {e}")))?;
        if consumed != file_len {
            return Err(VmmError::Snapshot(format!(
                "diff snapshot has trailing data: parsed {consumed} of {file_len} bytes"
            )));
        }
        Ok(SnapshotContent::Diff {
            parent,
            state,
            version,
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
fn load_snapshot_chain(path: &str) -> Result<(vmm_memory_backend::GuestMemory, Vec<u8>, u16)> {
    // Follow the chain tip→base, collecting each diff's (state, pages).
    type DiffPages = Vec<(u64, Vec<u8>)>;
    type DiffChain = Vec<(u16, Vec<u8>, DiffPages)>;

    let mut diffs: DiffChain = Vec::new();
    let mut chain_page_bytes = 0u64;
    let (mut cur, snapshot_root) = canonical_snapshot_tip(path)?;
    let mut seen = std::collections::HashSet::new();
    let (mem, base_state, base_version) = loop {
        if seen.len() >= MAX_DIFF_CHAIN_DEPTH {
            return Err(VmmError::Snapshot("snapshot chain too deep".into()));
        }
        if !seen.insert(cur.clone()) {
            return Err(VmmError::Snapshot("snapshot chain cycle".into()));
        }
        match read_snapshot(&cur, &snapshot_root)? {
            SnapshotContent::Full {
                mem,
                state,
                version,
            } => break (mem, state, version),
            SnapshotContent::Diff {
                parent,
                state,
                version,
                pages,
            } => {
                for (_, bytes) in &pages {
                    chain_page_bytes = validate_diff_payload_budget(chain_page_bytes, bytes.len())?;
                }
                diffs.push((version, state, pages));
                cur = parent;
            }
        }
    };
    // The tip is the first diff collected (or the base if no diffs).
    let (tip_version, tip_state) = diffs
        .first()
        .map(|(version, state, _)| (*version, state.clone()))
        .unwrap_or((base_version, base_state));
    // Apply diffs base→tip = reverse of the tip→base collection order.
    let mem_bytes = usize::try_from(mem.size_bytes)
        .map_err(|_| VmmError::Memory("restored memory too large".into()))?;
    let mem_slice: &mut [u8] = {
        // SAFETY: `mem` is owned here and sized `mem_bytes`; we only write in-range.
        unsafe { std::slice::from_raw_parts_mut(mem.as_ptr() as *mut u8, mem_bytes) }
    };
    for (_, _, pages) in diffs.iter().rev() {
        for (gpa, bytes) in pages {
            validate_diff_page_range(*gpa, bytes.len(), mem_bytes)?;
            let start = usize::try_from(*gpa)
                .map_err(|_| VmmError::Snapshot("diff page GPA too large".into()))?;
            let end = start + bytes.len();
            mem_slice[start..end].copy_from_slice(bytes);
        }
    }
    Ok((mem, tip_state, tip_version))
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
    /// Encoded in a framed trailer so historical postcard field order remains
    /// unchanged.
    #[serde(skip)]
    pub serial_runtime: Option<vmm_devices::serial::SerialRuntimeState>,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn decode_snapshot_component<T>(encoded: Option<&[u8]>, name: &str) -> Result<Option<T>>
where
    T: serde::de::DeserializeOwned,
{
    encoded
        .map(|bytes| {
            postcard::from_bytes(bytes).map(Some).map_err(|error| {
                VmmError::Snapshot(format!("snapshot {name} state is malformed: {error}"))
            })
        })
        .unwrap_or(Ok(None))
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn validate_restored_runtime_shape(
    saved: &StateBlob,
    has_balloon: bool,
    has_vcpu: bool,
    has_vm: bool,
    ap_states: usize,
    vcpus: u8,
) -> Result<()> {
    if !has_vcpu {
        if has_vm
            || ap_states != 0
            || !saved.virtio_blk.is_empty()
            || !saved.virtio_net.is_empty()
            || saved.vsock.is_some()
            || has_balloon
        {
            return Err(VmmError::Snapshot(
                "memory-only snapshot contains partial live runtime state".into(),
            ));
        }
        return Ok(());
    }
    if !has_vm {
        return Err(VmmError::Snapshot(
            "live snapshot is missing in-kernel VM state".into(),
        ));
    }
    let expected_aps = usize::from(vcpus.saturating_sub(1));
    if ap_states != expected_aps {
        return Err(VmmError::Snapshot(format!(
            "live snapshot AP state count mismatch: expected {expected_aps}, got {ap_states}"
        )));
    }
    if saved.virtio_blk.len() != saved.volumes.len() {
        return Err(VmmError::Snapshot(format!(
            "live snapshot block state count mismatch: expected {}, got {}",
            saved.volumes.len(),
            saved.virtio_blk.len()
        )));
    }
    if saved.virtio_net.len() != saved.net.len() {
        return Err(VmmError::Snapshot(format!(
            "live snapshot network state count mismatch: expected {}, got {}",
            saved.net.len(),
            saved.virtio_net.len()
        )));
    }
    if saved.vsock.is_none() {
        return Err(VmmError::Snapshot(
            "live snapshot is missing virtio-vsock state".into(),
        ));
    }
    if !has_balloon {
        return Err(VmmError::Snapshot(
            "live snapshot is missing virtio-balloon state".into(),
        ));
    }
    Ok(())
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const BALLOON_STATE_TRAILER_MAGIC: &[u8; 8] = b"TRTBLN01";

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const COMPATIBILITY_TRAILER_MAGIC: &[u8; 8] = b"TRTCMP01";
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const SERIAL_STATE_TRAILER_MAGIC: &[u8; 8] = b"TRTSER01";
#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    feature = "boot",
    not(feature = "test-incompatible-snapshot-abi")
))]
const SNAPSHOT_STATE_ABI: u16 = 1;
#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    feature = "boot",
    feature = "test-incompatible-snapshot-abi"
))]
const SNAPSHOT_STATE_ABI: u16 = 2;
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
const DEVICE_MODEL_ABI: u16 = 2;

/// Compatibility boundary for state that is meaningful only to a matching
/// VMM implementation. The outer file version protects framing; these fields
/// protect the KVM/device state carried inside that framing.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SnapshotCompatibility {
    state_abi: u16,
    device_model_abi: u16,
    architecture: String,
    cpu_template: String,
    cpu_template_digest: String,
    writer_version: String,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
impl SnapshotCompatibility {
    fn current() -> std::result::Result<Self, postcard::Error> {
        use sha2::{Digest as _, Sha256};

        let template = crate::cpu_template::CpuTemplate::bare();
        let encoded = postcard::to_allocvec(&template)?;
        Ok(Self {
            state_abi: SNAPSHOT_STATE_ABI,
            device_model_abi: DEVICE_MODEL_ABI,
            architecture: std::env::consts::ARCH.into(),
            cpu_template: template.name,
            cpu_template_digest: format!("sha256:{:x}", Sha256::digest(encoded)),
            writer_version: env!("CARGO_PKG_VERSION").into(),
        })
    }

    fn validate(&self) -> Result<()> {
        let current = Self::current().map_err(|error| {
            VmmError::Snapshot(format!("compute snapshot compatibility: {error}"))
        })?;
        if self.state_abi != current.state_abi {
            return Err(VmmError::Snapshot(format!(
                "incompatible snapshot state ABI: snapshot={}, VMM={}",
                self.state_abi, current.state_abi
            )));
        }
        if self.device_model_abi != current.device_model_abi {
            return Err(VmmError::Snapshot(format!(
                "incompatible snapshot device-model ABI: snapshot={}, VMM={}",
                self.device_model_abi, current.device_model_abi
            )));
        }
        if self.architecture != current.architecture {
            return Err(VmmError::Snapshot(format!(
                "incompatible snapshot architecture: snapshot={}, VMM={}",
                self.architecture, current.architecture
            )));
        }
        if self.cpu_template != current.cpu_template
            || self.cpu_template_digest != current.cpu_template_digest
        {
            return Err(VmmError::Snapshot(format!(
                "incompatible snapshot CPU template: snapshot={} ({}), VMM={} ({})",
                self.cpu_template,
                self.cpu_template_digest,
                current.cpu_template,
                current.cpu_template_digest
            )));
        }
        Ok(())
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn validate_snapshot_compatibility(
    snapshot_version: u16,
    compatibility: Option<&SnapshotCompatibility>,
) -> Result<()> {
    if let Some(compatibility) = compatibility {
        return compatibility.validate();
    }
    if snapshot_version >= SNAPSHOT_VERSION {
        return Err(VmmError::Snapshot(
            "snapshot compatibility manifest is missing".into(),
        ));
    }
    log::warn!("restore: accepting a legacy snapshot without an internal compatibility manifest");
    Ok(())
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn decode_trailer<'a, T: serde::de::DeserializeOwned>(
    trailing: &'a [u8],
    magic: &[u8; 8],
) -> Option<(T, &'a [u8])> {
    if trailing.len() < magic.len() + 4 || &trailing[..magic.len()] != magic {
        return None;
    }
    let length_offset = magic.len();
    let payload_len =
        u32::from_le_bytes(trailing[length_offset..length_offset + 4].try_into().ok()?) as usize;
    let payload_start = length_offset + 4;
    let payload_end = payload_start.checked_add(payload_len)?;
    let payload = trailing.get(payload_start..payload_end)?;
    Some((
        postcard::from_bytes(payload).ok()?,
        trailing.get(payload_end..)?,
    ))
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn decode_state_blob(
    bytes: &[u8],
) -> Option<(
    StateBlob,
    Option<vmm_devices::virtio::balloon::VirtioBalloonMmioState>,
    Option<SnapshotCompatibility>,
)> {
    let (mut blob, mut trailing) = postcard::take_from_bytes::<StateBlob>(bytes).ok()?;
    // Full live snapshots may pad the state area with zeroes so the following
    // RAM extent is block-aligned and can be range-reflinked from the pre-copy
    // stage. Zero padding is semantically empty and covered by the state CRC.
    if trailing.iter().all(|byte| *byte == 0) {
        return Some((blob, None, None));
    }
    let mut balloon = None;
    if trailing.starts_with(BALLOON_STATE_TRAILER_MAGIC) {
        let (decoded, remaining) = decode_trailer(trailing, BALLOON_STATE_TRAILER_MAGIC)?;
        balloon = Some(decoded);
        trailing = remaining;
    }
    if trailing.starts_with(SERIAL_STATE_TRAILER_MAGIC) {
        let (decoded, remaining) = decode_trailer(trailing, SERIAL_STATE_TRAILER_MAGIC)?;
        blob.serial_runtime = Some(decoded);
        trailing = remaining;
    }
    let mut compatibility = None;
    if trailing.starts_with(COMPATIBILITY_TRAILER_MAGIC) {
        let (decoded, remaining) = decode_trailer(trailing, COMPATIBILITY_TRAILER_MAGIC)?;
        compatibility = Some(decoded);
        trailing = remaining;
    }
    trailing
        .iter()
        .all(|byte| *byte == 0)
        .then_some((blob, balloon, compatibility))
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn encode_state_blob(
    blob: &StateBlob,
    balloon: Option<&vmm_devices::virtio::balloon::VirtioBalloonMmioState>,
) -> std::result::Result<Vec<u8>, postcard::Error> {
    let mut bytes = postcard::to_allocvec(blob)?;
    if let Some(balloon) = balloon {
        let payload = postcard::to_allocvec(balloon)?;
        let payload_len =
            u32::try_from(payload.len()).map_err(|_| postcard::Error::SerializeBufferFull)?;
        bytes.extend_from_slice(BALLOON_STATE_TRAILER_MAGIC);
        bytes.extend_from_slice(&payload_len.to_le_bytes());
        bytes.extend_from_slice(&payload);
    }
    if let Some(serial_runtime) = blob.serial_runtime.as_ref() {
        let payload = postcard::to_allocvec(serial_runtime)?;
        let payload_len =
            u32::try_from(payload.len()).map_err(|_| postcard::Error::SerializeBufferFull)?;
        bytes.extend_from_slice(SERIAL_STATE_TRAILER_MAGIC);
        bytes.extend_from_slice(&payload_len.to_le_bytes());
        bytes.extend_from_slice(&payload);
    }
    let compatibility = postcard::to_allocvec(&SnapshotCompatibility::current()?)?;
    let compatibility_len =
        u32::try_from(compatibility.len()).map_err(|_| postcard::Error::SerializeBufferFull)?;
    bytes.extend_from_slice(COMPATIBILITY_TRAILER_MAGIC);
    bytes.extend_from_slice(&compatibility_len.to_le_bytes());
    bytes.extend_from_slice(&compatibility);
    Ok(bytes)
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
) -> Result<Vec<Vec<u8>>> {
    devs.iter()
        .map(|dev| {
            let state = vmm_devices::persist::Persist::try_save(&**dev).map_err(|error| {
                VmmError::Snapshot(format!("capture virtio-blk state: {error}"))
            })?;
            postcard::to_allocvec(&state)
                .map_err(|error| VmmError::Snapshot(format!("serialize virtio-blk state: {error}")))
        })
        .collect()
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn capture_virtio_net_states(
    devs: &[Arc<vmm_devices::virtio::net_transport::VirtioNetMmio>],
) -> Result<Vec<Vec<u8>>> {
    devs.iter()
        .map(|dev| {
            let state = vmm_devices::persist::Persist::try_save(&**dev).map_err(|error| {
                VmmError::Snapshot(format!("capture virtio-net state: {error}"))
            })?;
            postcard::to_allocvec(&state)
                .map_err(|error| VmmError::Snapshot(format!("serialize virtio-net state: {error}")))
        })
        .collect()
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn restore_virtio_blk_states(
    devs: &mut [Arc<vmm_devices::virtio::blk_transport::VirtioBlkMmio>],
    states: &[Vec<u8>],
) -> Result<()> {
    if states.len() != devs.len() {
        return Err(VmmError::Snapshot(format!(
            "snapshot virtio-blk state count mismatch: expected {}, got {}",
            devs.len(),
            states.len(),
        )));
    }
    let decoded = states
        .iter()
        .enumerate()
        .map(|(index, bytes)| {
            postcard::from_bytes::<vmm_devices::virtio::blk_transport::VirtioBlkMmioState>(bytes)
                .map_err(|error| {
                    VmmError::Snapshot(format!(
                        "snapshot virtio-blk state {index} is malformed: {error}"
                    ))
                })
        })
        .collect::<Result<Vec<_>>>()?;
    for (dev, state) in devs.iter_mut().zip(decoded) {
        vmm_devices::persist::Persist::restore(dev, state);
    }
    Ok(())
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn restore_virtio_net_states(
    devs: &mut [Arc<vmm_devices::virtio::net_transport::VirtioNetMmio>],
    states: &[Vec<u8>],
) -> Result<()> {
    if states.len() != devs.len() {
        return Err(VmmError::Snapshot(format!(
            "snapshot virtio-net state count mismatch: expected {}, got {}",
            devs.len(),
            states.len(),
        )));
    }
    let decoded = states
        .iter()
        .enumerate()
        .map(|(index, bytes)| {
            postcard::from_bytes::<vmm_devices::virtio::net_transport::VirtioNetMmioState>(bytes)
                .map_err(|error| {
                    VmmError::Snapshot(format!(
                        "snapshot virtio-net state {index} is malformed: {error}"
                    ))
                })
        })
        .collect::<Result<Vec<_>>>()?;
    for (dev, state) in devs.iter_mut().zip(decoded) {
        vmm_devices::persist::Persist::restore(dev, state);
    }
    Ok(())
}

/// Reconstruct a *running* VM from restored guest memory + a captured vCPU
/// state. Mirrors `create_live`'s KVM/device plumbing, but re-applies the saved
/// vCPU state instead of setting up a fresh boot entry, and does NOT rewrite the
/// kernel/GDT/ACPI tables (they are already present in the restored memory).
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
struct RestoredRuntimeState<'a> {
    vcpu: &'a crate::vcpu_setup::VcpuFullState,
    aps: &'a [crate::vcpu_setup::VcpuFullState],
    vm: Option<&'a crate::kvm::VmFullState>,
    serial: &'a vmm_devices::serial::SerialState,
    serial_runtime: Option<&'a vmm_devices::serial::SerialRuntimeState>,
    virtio_blk: &'a [Vec<u8>],
    virtio_net: &'a [Vec<u8>],
    vsock: Option<&'a vmm_devices::virtio::vsock::VirtioVsockMmioState>,
    balloon: Option<&'a vmm_devices::virtio::balloon::VirtioBalloonMmioState>,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn build_running_vm(
    mem: vmm_memory_backend::GuestMemory,
    config: &VmConfig,
    restored: RestoredRuntimeState<'_>,
    entry: u64,
) -> Result<RunningVm> {
    use crate::vcpu_thread::VcpuThread;

    crate::vmgenid::require_snapshot_support(&mem)?;

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
        blk_io_evts,
        blk_mmio_bases,
        nets,
        rng_irq,
        vsock,
        balloon,
    } = build_devices(config, &mem)?;
    restore_virtio_blk_states(&mut blks, restored.virtio_blk)?;
    let mut net_devices: Vec<_> = nets.iter().map(|n| n.dev.clone()).collect();
    restore_virtio_net_states(&mut net_devices, restored.virtio_net)?;
    let mut balloon = balloon;
    if let (Some(wired), Some(state)) = (balloon.as_mut(), restored.balloon) {
        vmm_devices::persist::Persist::restore(&mut wired.device, state.clone());
    }
    let mut irq_evts: Vec<vmm_sys_util::eventfd::EventFd> = blk_irq_evts;

    let template = crate::cpu_template::CpuTemplate::bare();
    let kvm_vm = crate::kvm::KvmVm::new_with_options(mem, devices, template, full_boot)?;

    // Replace the snapshot's generation value before any restored vCPU can
    // execute. Old snapshots without the ACPI device were rejected above.
    let vmgenid = crate::vmgenid::VmGenId::new(&kvm_vm.mem)?;

    // Re-apply the guest's in-kernel IRQCHIP/PIT/clock over the freshly-created
    // ones, so the restored guest keeps its interrupt routing (a fresh IOAPIC
    // would be masked/default and post-restore device I/O would stall waiting
    // for interrupts). Must happen after the irqchip/PIT exist (new_with_options
    // created them) and before the vCPU runs.
    if let Some(vm_state) = restored.vm {
        kvm_vm.restore_vm_state(vm_state)?;
    }

    kvm_vm.register_irqfd(vmgenid.eventfd(), crate::vmgenid::VMGENID_GSI)?;

    for (i, evt) in irq_evts.iter().enumerate() {
        let irq = 5 + i as u32;
        kvm_vm.register_irqfd(evt, irq)?;
    }
    let mut blk_io_loops = Vec::with_capacity(blks.len());
    for (i, ((device, io_evt), mmio_base)) in
        blks.iter().zip(blk_io_evts).zip(blk_mmio_bases).enumerate()
    {
        kvm_vm.register_ioeventfd_datamatch(mmio_base + 0x50, &io_evt, 0)?;
        use std::os::fd::AsRawFd;
        let io_loop = vmm_devices::virtio::blk_io_loop::spawn_blk_io_loop(
            Arc::clone(device),
            io_evt.as_raw_fd(),
        )
        .map_err(|error| {
            VmmError::Device(format!(
                "spawn restored block I/O worker for volume {i}: {error}"
            ))
        })?;
        blk_io_loops.push(io_loop);
        irq_evts.push(io_evt);
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
    serial_dev
        .try_restore_snapshot(restored.serial.clone(), restored.serial_runtime)
        .map_err(|error| VmmError::Snapshot(format!("restore UART state: {error}")))?;
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
    let balloon_device = balloon.as_ref().map(|wired| wired.device.clone());
    let balloon_irq_resample = match balloon {
        Some(wired) => {
            kvm_vm.register_irqfd_with_resample(&wired.irq_evt, &wired.resample_evt, wired.irq)?;
            let loop_handle = BalloonIrqResample::spawn(
                wired.device,
                wired
                    .irq_evt
                    .try_clone()
                    .map_err(|error| VmmError::Kvm(format!("balloon irq clone: {error}")))?,
                wired.resample_evt,
            )?;
            irq_evts.push(wired.irq_evt);
            Some(loop_handle)
        }
        None => None,
    };

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
            let reset = restored.vsock.map(|state| {
                device.restore_transport_state(state);
                (device.clone(), state.connections.clone())
            });
            let pump =
                vmm_devices::virtio::vsock_io_loop::spawn_vsock_pump(device.clone(), tx_kick_fd)
                    .ok();
            let pump_wake = pump.as_ref().and_then(|p| p.wake_evt().ok());
            let pty_wake = pump.as_ref().and_then(|p| p.wake_evt().ok());
            irq_evts.push(wv.io_evt);
            let exec = Some(
                crate::vsock_exec::VsockExecChannel::bind_with_pump_wake(
                    &wv.control_socket,
                    pump_wake,
                )
                .map_err(|error| {
                    VmmError::Device(format!(
                        "bind restored vsock exec socket {}: {error}",
                        wv.control_socket.display()
                    ))
                })?,
            );
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
    crate::vcpu_setup::restore_vcpu_full_state(&vcpu, restored.vcpu)?;
    kvm_vm.apply_cpu_template_msrs(&vcpu)?;
    if let Err(error) = kvm_vm.notify_restored_clock(&vcpu) {
        // KVM documents this as meaningful only for guests using kvm-clock;
        // other clock sources may reject it. The notification is a watchdog
        // safety hint, not a reason to discard an otherwise valid restore.
        log::warn!("restored BSP clock notification unavailable: {error}");
    }
    let vcpu_thread = VcpuThread::spawn(vcpu, kvm_vm.mmio_bus.clone(), serial.clone());

    // SMP restore (phase B): recreate each AP (id 1..N) and re-apply its captured
    // state (which includes its RUNNABLE MP_STATE + LAPIC), so the restored VM
    // comes back with all vCPUs online. Per-AP CPUID (with its APIC id) is set
    // before the saved state, matching create_live.
    let mut ap_threads = Vec::with_capacity(restored.aps.len());
    for (i, ap_state) in restored.aps.iter().enumerate() {
        let id = (i + 1) as u8;
        let ap = kvm_vm.create_vcpu(id)?;
        kvm_vm.apply_boot_cpuid(&ap, id)?;
        crate::vcpu_setup::restore_vcpu_full_state(&ap, ap_state)?;
        kvm_vm.apply_cpu_template_msrs(&ap)?;
        if let Err(error) = kvm_vm.notify_restored_clock(&ap) {
            log::warn!("restored AP {id} clock notification unavailable: {error}");
        }
        ap_threads.push(VcpuThread::spawn(
            ap,
            kvm_vm.mmio_bus.clone(),
            serial.clone(),
        ));
    }

    // Notify only after the restored vCPUs are live. Injecting a GED edge into
    // a paused LAPIC can be lost, while injecting during early cold boot can
    // reach a kernel whose interrupt handlers are not ready. The synchronous
    // guest repair barrier still prevents workload admission until entropy is
    // repaired, so there is no race with customer code here.
    vmgenid.notify_after_restore()?;
    irq_evts.push(vmgenid.into_eventfd());

    // The guest vCPU(s) are live again. Inject an RST for each connection that
    // was open at snapshot time so the guest's vsock layer tears the stale
    // stream down and the agent re-dials the host exec channel (the host side
    // already re-accepts the new connection). Done here, post-resume, so the RX
    // completion interrupt lands on a running vCPU instead of a paused LAPIC.
    if let Some((dev, conns)) = vsock_reset {
        let resets = dev
            .reset_restored_connections(&conns)
            .map_err(|error| VmmError::Device(format!("reset restored vsock streams: {error}")))?;
        if resets > 0 {
            log::info!("vsock restore: injected RST for {resets} restored stream(s)");
        }
    }

    Ok(RunningVm {
        kvm_vm,
        vcpu_thread,
        ap_threads,
        loaded_entry: entry,
        blk_io_loops,
        net_io_loops,
        blk_devices: blks,
        net_devices,
        balloon_device,
        balloon_irq_resample,
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
    acpi_devices: Vec<(u64, u64, u32, bool)>,
    blks: Vec<Arc<vmm_devices::virtio::blk_transport::VirtioBlkMmio>>,
    blk_irq_evts: Vec<vmm_sys_util::eventfd::EventFd>,
    blk_io_evts: Vec<vmm_sys_util::eventfd::EventFd>,
    blk_mmio_bases: Vec<u64>,
    nets: Vec<WiredNet>,
    /// virtio-rng completion irqfd (gsi, EventFd), registered by the caller.
    rng_irq: Option<(u32, vmm_sys_util::eventfd::EventFd)>,
    /// virtio-vsock exec device (pump + control-socket accept wired by caller).
    vsock: Option<WiredVsock>,
    balloon: Option<WiredBalloon>,
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
struct WiredBalloon {
    device: Arc<vmm_devices::virtio::balloon::VirtioBalloonMmio>,
    irq_evt: vmm_sys_util::eventfd::EventFd,
    resample_evt: vmm_sys_util::eventfd::EventFd,
    irq: u32,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
pub struct BalloonIrqResample {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
impl BalloonIrqResample {
    fn spawn(
        device: Arc<vmm_devices::virtio::balloon::VirtioBalloonMmio>,
        irq_evt: vmm_sys_util::eventfd::EventFd,
        resample_evt: vmm_sys_util::eventfd::EventFd,
    ) -> Result<Self> {
        use std::os::fd::AsRawFd;
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("balloon-irq-resample".into())
            .spawn(move || {
                while !stop_thread.load(Ordering::Acquire) {
                    let mut pollfd = libc::pollfd {
                        fd: resample_evt.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: pollfd points to one initialized entry and the
                    // thread owns the eventfd for its entire lifetime.
                    let ready = unsafe { libc::poll(&mut pollfd, 1, 100) };
                    if ready <= 0 {
                        continue;
                    }
                    let _ = resample_evt.read();
                    if device.has_pending_interrupt() {
                        let _ = irq_evt.write(1);
                    }
                }
            })
            .map_err(|error| VmmError::Kvm(format!("spawn balloon IRQ resample: {error}")))?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
impl Drop for BalloonIrqResample {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
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
    let mut acpi_devices: Vec<(u64, u64, u32, bool)> = Vec::new();
    let mut blks: Vec<Arc<VirtioBlkMmio>> = Vec::new();
    let mut blk_irq_evts: Vec<vmm_sys_util::eventfd::EventFd> = Vec::new();
    let mut blk_io_evts: Vec<vmm_sys_util::eventfd::EventFd> = Vec::new();
    let mut blk_mmio_bases = Vec::new();
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
        let io_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| VmmError::Kvm(format!("EventFd: {e}")))?;
        transport.set_irq_evt(
            irq_evt
                .try_clone()
                .map_err(|e| VmmError::Kvm(format!("EventFd clone: {e}")))?,
        );
        devices.push((
            MmioRange::new(mmio_base, 0x1000),
            Box::new(transport.clone()),
        ));
        acpi_devices.push((mmio_base, 0x1000, irq, true));
        blks.push(transport);
        blk_irq_evts.push(irq_evt);
        blk_io_evts.push(io_evt);
        blk_mmio_bases.push(mmio_base);
        log::info!("volume {i}: {} at mmio 0x{mmio_base:x} irq {irq}", vol.path);
    }

    for (j, net) in config.net.iter().enumerate() {
        let slot = config.volumes.len() + j;
        let irq = 5 + slot as u32;
        let mmio_base = MMIO_START + (slot as u64) * 0x1000;
        let mac = parse_guest_mac(net.guest_mac.as_deref(), j);
        let tap = inherited_tap_fd(&net.tap)?
            .map(|fd| vmm_net::tap::Tap::from_inherited_fd(fd, &net.tap))
            .unwrap_or_else(|| vmm_net::tap::Tap::create(&net.tap))
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
        acpi_devices.push((mmio_base, 0x1000, irq, true));
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
    // restored/cloned guests to reseed their CRNG). It is bounded and serviced
    // synchronously on the MMIO bus, so it needs only a completion irqfd.
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
    acpi_devices.push((rng_mmio, 0x1000, rng_irq_num, true));
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
    let control_socket = unique_runtime_socket_path()?;
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
    acpi_devices.push((vsock_mmio, 0x1000, vsock_irq, true));
    log::info!(
        "virtio-vsock at mmio 0x{vsock_mmio:x} irq {vsock_irq} guest_cid {VSOCK_GUEST_CID} → {}",
        control_socket.display()
    );

    // Keep the established block/net/rng/vsock slots stable for snapshot
    // compatibility. Balloon is appended after vsock and starts at target 0;
    // the control API can change the target after boot.
    use vmm_devices::virtio::balloon::VirtioBalloonMmio;
    let balloon_slot = vsock_slot + 1;
    let balloon_irq = 5 + balloon_slot as u32;
    let balloon_mmio = MMIO_START + (balloon_slot as u64) * 0x1000;
    let balloon_device = Arc::new(
        VirtioBalloonMmio::new(balloon_irq, mem.clone(), 0)
            .map_err(|error| VmmError::Device(format!("virtio-balloon: {error}")))?,
    );
    let balloon_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
        .map_err(|error| VmmError::Kvm(format!("balloon EventFd: {error}")))?;
    let balloon_resample_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
        .map_err(|error| VmmError::Kvm(format!("balloon resample EventFd: {error}")))?;
    balloon_device.set_irq_evt(
        balloon_evt
            .try_clone()
            .map_err(|error| VmmError::Kvm(format!("balloon EventFd clone: {error}")))?,
    );
    devices.push((
        MmioRange::new(balloon_mmio, 0x1000),
        Box::new(balloon_device.clone()),
    ));
    // Virtio-mmio interrupts are active-high level interrupts. Balloon needs
    // this particularly because config and used-ring causes can overlap.
    acpi_devices.push((balloon_mmio, 0x1000, balloon_irq, false));
    log::info!("virtio-balloon at mmio 0x{balloon_mmio:x} irq {balloon_irq}");

    Ok(WiredDevices {
        devices,
        acpi_devices,
        blks,
        blk_irq_evts,
        blk_io_evts,
        blk_mmio_bases,
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
        balloon: Some(WiredBalloon {
            device: balloon_device,
            irq_evt: balloon_evt,
            resample_evt: balloon_resample_evt,
            irq: balloon_irq,
        }),
    })
}

/// Parse a `xx:xx:xx:xx:xx:xx` MAC, falling back to a deterministic locally
/// administered address (02:00:00:00:00:NN) if absent or malformed.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn inherited_tap_fd(tap_name: &str) -> Result<Option<i32>> {
    let Some(spec) = std::env::var_os("VMM_TAP_FDS") else {
        return Ok(None);
    };
    let spec = spec
        .into_string()
        .map_err(|_| VmmError::Device("VMM_TAP_FDS is not valid UTF-8".into()))?;
    parse_inherited_tap_fd(tap_name, &spec).map(Some)
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn parse_inherited_tap_fd(tap_name: &str, spec: &str) -> Result<i32> {
    let mut matched = None;
    for entry in spec.split(',').filter(|entry| !entry.is_empty()) {
        let (name, raw_fd) = entry
            .split_once('=')
            .ok_or_else(|| VmmError::Device(format!("invalid VMM_TAP_FDS entry {entry:?}")))?;
        let fd = raw_fd
            .parse::<i32>()
            .map_err(|_| VmmError::Device(format!("invalid inherited TAP fd in {entry:?}")))?;
        if name == tap_name && matched.replace(fd).is_some() {
            return Err(VmmError::Device(format!(
                "duplicate inherited TAP mapping for {tap_name}"
            )));
        }
    }
    matched.ok_or_else(|| {
        VmmError::Device(format!(
            "VMM_TAP_FDS is set but has no inherited descriptor for {tap_name}"
        ))
    })
}

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
        serial_runtime: None,
    };

    encode_state_blob(&blob, None).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    #[test]
    fn clone_repair_v3_command_has_fixed_width_realtime_suffix() {
        let nonce = "ab".repeat(48);
        let command = build_clone_repair_v3_command(
            &nonce,
            std::time::Duration::new(0x0102_0304_0506_0708, 0x0102_0304),
        );
        assert_eq!(
            command,
            format!("__TARIT_CLONE_REPAIR_V3__{nonce}010203040506070801020304")
        );
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    #[test]
    fn inherited_tap_mapping_requires_the_requested_tap() {
        let error = parse_inherited_tap_fd("tap-wanted", "tap-other=17").unwrap_err();
        assert!(error.to_string().contains("tap-wanted"));
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    #[test]
    fn inherited_tap_mapping_returns_the_exact_descriptor() {
        assert_eq!(
            parse_inherited_tap_fd("tap-wanted", "tap-other=17,tap-wanted=23").unwrap(),
            23
        );
    }
    use crate::config::{
        KernelConfig, MemoryConfig, NetConfig, VcpuConfig, VmConfig, VolumeConfig,
    };
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    #[cfg(unix)]
    fn write_private(path: &Path, bytes: &[u8]) {
        let mut options = std::fs::OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let mut file = options.open(path).expect("create private test file");
        std::io::Write::write_all(&mut file, bytes).expect("write private test file");
        file.sync_all().expect("sync private test file");
    }

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

    fn net(tap: &str, mac: &str, ip: &str) -> NetConfig {
        NetConfig {
            tap: tap.into(),
            guest_mac: Some(mac.into()),
            guest_ip: Some(ip.into()),
            port_forwards: Vec::new(),
        }
    }

    #[test]
    fn restore_networks_require_an_explicit_valid_replacement() {
        let mut config = cfg();
        config.net = vec![net("tap-old", "02:00:00:00:00:01", "10.0.0.2")];
        let error = apply_restore_network_override(&mut config, None).unwrap_err();
        assert!(error.to_string().contains("explicit net replacement"));

        let error = apply_restore_network_override(&mut config, Some(Vec::new())).unwrap_err();
        assert!(error.to_string().contains("does not match"));

        apply_restore_network_override(
            &mut config,
            Some(vec![net("tap-new", "02:00:00:00:00:02", "10.0.0.3")]),
        )
        .unwrap();
        assert_eq!(config.net[0].tap, "tap-new");
        assert_eq!(config.net[0].guest_ip.as_deref(), Some("10.0.0.3"));
    }

    #[test]
    fn restore_inherited_volumes_require_exact_fresh_descriptors() {
        let mut config = cfg();
        config.volumes = vec![
            VolumeConfig {
                path: "/base/rootfs.ext4".into(),
                read_only: false,
                overlay: Some("/golden/rootfs.cow".into()),
                inherited_fd: None,
            },
            VolumeConfig {
                path: "volume:data".into(),
                read_only: false,
                overlay: None,
                inherited_fd: Some(41),
            },
        ];

        assert!(apply_restore_volume_override(&mut config.clone(), None).is_err());
        assert!(apply_restore_volume_override(&mut config.clone(), Some(Vec::new())).is_err());
        assert!(apply_restore_volume_override(
            &mut config.clone(),
            Some(vec![VolumeConfig {
                path: "volume:other".into(),
                read_only: false,
                overlay: None,
                inherited_fd: Some(52),
            }])
        )
        .is_err());

        apply_restore_volume_override(
            &mut config,
            Some(vec![VolumeConfig {
                path: "volume:data".into(),
                read_only: false,
                overlay: None,
                inherited_fd: Some(52),
            }]),
        )
        .unwrap();
        assert_eq!(config.volumes[0].inherited_fd, None);
        assert_eq!(config.volumes[1].inherited_fd, Some(52));
    }

    #[test]
    fn restore_network_replacement_rejects_duplicate_host_bindings() {
        let mut config = cfg();
        config.net = vec![
            net("tap-old-a", "02:00:00:00:00:01", "10.0.0.2"),
            net("tap-old-b", "02:00:00:00:00:02", "10.0.0.3"),
        ];
        let duplicate = net("tap-new", "02:00:00:00:00:03", "10.0.0.4");
        assert!(apply_restore_network_override(
            &mut config,
            Some(vec![duplicate.clone(), duplicate])
        )
        .is_err());
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

    #[cfg(not(feature = "boot"))]
    #[test]
    fn unacknowledged_snapshot_is_removed_when_vm_stops() {
        let c = VmmController::new();
        c.create(cfg()).unwrap();
        let snapshot = c.snapshot(false).expect("create snapshot");
        let path = PathBuf::from(&snapshot);

        c.stop().expect("stop VM");

        assert!(
            !path.exists(),
            "the VM must remove a snapshot until its ownership is explicitly released"
        );
    }

    #[cfg(not(feature = "boot"))]
    #[test]
    fn exact_release_disarms_only_the_owned_snapshot() {
        let c = VmmController::new();
        c.create(cfg()).unwrap();
        let snapshot = c.snapshot(false).expect("create snapshot");
        let path = PathBuf::from(&snapshot);
        let identity = OwnedScratchFile::identity_for(&path).expect("snapshot identity");

        c.release_scratch(&snapshot, identity)
            .expect("release the exact owned snapshot");
        c.stop().expect("stop VM");

        assert!(
            path.exists(),
            "a released snapshot must outlive its source VM"
        );
        std::fs::remove_file(path).expect("clean up released snapshot");
    }

    #[test]
    fn restore_overlay_replaces_saved_golden_overlay() {
        let mut config = cfg();
        config.volumes = vec![VolumeConfig {
            path: "/base/rootfs.ext4".into(),
            read_only: true,
            overlay: Some("/golden/rootfs.overlay".into()),
            inherited_fd: None,
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
    fn sparse_restore_overlay_rejects_an_empty_data_extent() {
        let error = validate_sparse_extent(0, 4096, 4096, 8192)
            .expect_err("SEEK_HOLE must advance beyond SEEK_DATA");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn sparse_restore_overlay_rejects_a_backward_data_extent() {
        let error = validate_sparse_extent(4096, 0, 4096, 8192)
            .expect_err("SEEK_DATA must not move backwards from the current offset");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn fresh_boot_exec_is_only_allowed_for_created_vms() {
        assert!(VmmController::fresh_boot_exec_allowed(VmState::Created));
        assert!(!VmmController::fresh_boot_exec_allowed(VmState::Running));
        assert!(!VmmController::fresh_boot_exec_allowed(VmState::Paused));
        assert!(!VmmController::fresh_boot_exec_allowed(VmState::Suspended));
        assert!(!VmmController::fresh_boot_exec_allowed(VmState::Stopped));
    }

    #[test]
    fn fresh_boot_timeout_rounds_up_to_a_full_second() {
        assert_eq!(VmmController::fresh_boot_timeout_secs(0), 10);
        assert_eq!(VmmController::fresh_boot_timeout_secs(1), 1);
        assert_eq!(VmmController::fresh_boot_timeout_secs(1_000), 1);
        assert_eq!(VmmController::fresh_boot_timeout_secs(1_001), 2);
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

        write_private(&golden, golden_bytes);
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
    fn failed_restore_overlay_seed_preserves_a_preexisting_target() {
        let dir = private_runtime_dir().expect("private test runtime");
        let unique = format!(
            "restore-overlay-existing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        );
        let golden = dir.join(format!("{unique}-golden.cow"));
        let clone = dir.join(format!("{unique}-clone.cow"));
        let existing_bytes = b"another clone owns this overlay";
        write_private(&golden, b"golden state");
        write_private(&clone, existing_bytes);

        seed_restore_overlay(&golden, &clone).expect_err("existing target must reject seeding");

        assert_eq!(
            std::fs::read(&clone).expect("existing clone overlay must remain"),
            existing_bytes
        );
        let _ = std::fs::remove_file(golden);
        let _ = std::fs::remove_file(clone);
    }

    #[test]
    fn preseeded_restore_overlay_never_reopens_saved_source() {
        let dir = private_runtime_dir().expect("private test runtime");
        let unique = format!(
            "restore-overlay-preseeded-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        );
        let missing_source = dir.join(format!("{unique}-deleted-source.cow"));
        let target = dir.join(format!("{unique}-target.cow"));
        let checkpoint = b"snapshot-owned checkpoint";
        write_private(&target, checkpoint);

        let mut config = cfg();
        config.volumes = vec![VolumeConfig {
            path: "/base/rootfs.ext4".into(),
            read_only: true,
            overlay: Some(missing_source.display().to_string()),
            inherited_fd: None,
        }];
        let guard = prepare_restore_overlay(&config, target.to_str().unwrap())
            .expect("adopt the orchestrator-preseeded target");

        assert_eq!(
            std::fs::read(&target).expect("read adopted target"),
            checkpoint,
            "VMM must not overwrite the preseeded target from saved metadata"
        );
        drop(guard);
        assert!(
            !target.exists(),
            "failed restore cleanup must remove the exact adopted target"
        );
    }

    #[test]
    fn restore_overlay_refuses_to_adopt_the_golden_overlay() {
        let dir = private_runtime_dir().expect("private test runtime");
        let unique = format!(
            "restore-overlay-golden-adopt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        );
        let golden = dir.join(format!("{unique}-golden.cow"));
        let golden_bytes = b"reusable golden upper state";
        write_private(&golden, golden_bytes);

        let mut config = cfg();
        config.volumes = vec![VolumeConfig {
            path: "/base/rootfs.ext4".into(),
            read_only: true,
            overlay: Some(golden.display().to_string()),
            inherited_fd: None,
        }];

        let err = match prepare_restore_overlay(&config, golden.to_str().unwrap()) {
            Ok(_) => panic!("golden overlay must never be adopted as a restore target"),
            Err(err) => err,
        };
        assert!(matches!(err, VmmError::InvalidConfig(_)));
        assert_eq!(
            std::fs::read(&golden).expect("golden overlay must survive"),
            golden_bytes
        );
        let _ = std::fs::remove_file(golden);
    }

    #[test]
    fn restore_overlay_wraps_direct_rw_volume() {
        let mut config = cfg();
        config.volumes = vec![VolumeConfig {
            path: "/base/rootfs.ext4".into(),
            read_only: false,
            overlay: None,
            inherited_fd: None,
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
    fn concurrent_lifecycle_operations_are_rejected_deterministically() {
        let controller = VmmController::new();
        let _guard = controller
            .begin_lifecycle(LifecycleOp::Restore)
            .expect("first lifecycle operation must start");

        let error = controller
            .begin_lifecycle(LifecycleOp::Snapshot)
            .expect_err("concurrent lifecycle operation must be rejected");

        assert!(error.to_string().contains("restore"));
    }

    #[test]
    fn lifecycle_guard_releases_after_drop() {
        let controller = VmmController::new();
        {
            let _guard = controller
                .begin_lifecycle(LifecycleOp::Suspend)
                .expect("suspend guard must start");
        }
        controller
            .begin_lifecycle(LifecycleOp::Resume)
            .expect("a later lifecycle operation must be allowed after drop");
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

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    #[test]
    fn streamed_live_memory_snapshot_preserves_layout_and_crc() {
        use std::io::Write;

        let dir = private_runtime_dir().unwrap();
        let nonce = format!(
            "streamed-live-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let source_path = dir.join(format!("{nonce}-memory.raw"));
        let output_path = dir.join(format!("{nonce}-snapshot.snap"));
        let source = OwnedScratchFile::create_new(&source_path).unwrap();
        let output = OwnedScratchFile::create_new(&output_path).unwrap();
        let memory: Vec<u8> = (0..(2 * 1024 * 1024 + 37))
            .map(|index| (index as u8).wrapping_mul(31))
            .collect();
        let state = b"coherent live state";
        source
            .file()
            .try_clone()
            .unwrap()
            .write_all(&memory)
            .unwrap();

        let manifest = write_scratch_snapshot_file_from_memory_file(
            &output,
            state,
            source.file(),
            memory.len() as u64,
            false,
        )
        .unwrap();

        let bytes = std::fs::read(&output_path).unwrap();
        assert_eq!(&bytes[..4], b"VMSN");
        let padded_state_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
        let mem_offset = 32 + padded_state_len;
        assert_eq!(
            mem_offset % 4096,
            0,
            "live RAM extent must be range-reflink aligned"
        );
        assert_eq!(
            u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            crc32fast::hash(&bytes[32..mem_offset])
        );
        assert_eq!(
            u64::from_le_bytes(bytes[20..28].try_into().unwrap()),
            memory.len() as u64
        );
        assert_eq!(
            u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            crc32fast::hash(&memory)
        );
        assert_eq!(&bytes[32..32 + state.len()], state);
        assert!(bytes[32 + state.len()..mem_offset]
            .iter()
            .all(|byte| *byte == 0));
        assert_eq!(&bytes[mem_offset..], memory);
        let metadata = manifest
            .artifact(tarit_proto::ArtifactKind::SnapshotMetadata)
            .unwrap();
        let ram = manifest.artifact(tarit_proto::ArtifactKind::Ram).unwrap();
        assert_eq!(metadata.len, mem_offset as u64);
        assert_eq!(ram.len, memory.len() as u64);
        for (chunk, expected) in memory
            .chunks(tarit_proto::INTEGRITY_CHUNK_SIZE as usize)
            .zip(&ram.chunk_hashes)
        {
            use sha2::{Digest as _, Sha256};
            assert_eq!(*expected, <[u8; 32]>::from(Sha256::digest(chunk)));
        }

        remove_owned_scratch_file(&source);
        remove_owned_scratch_file(&output);
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

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    #[test]
    fn state_blob_without_balloon_field_remains_decodable() {
        #[derive(serde::Serialize)]
        struct LegacyStateBlob {
            entry: u64,
            mem_size: u64,
            vcpu: VcpuStateSave,
            kernel_path: String,
            cmdline: String,
            vcpus: u64,
            volumes: Vec<crate::config::VolumeConfig>,
            net: Vec<crate::config::NetConfig>,
            vcpu_full: Option<Vec<u8>>,
            vcpu_full_aps: Vec<Vec<u8>>,
            vm_full: Option<Vec<u8>>,
            serial: vmm_devices::serial::SerialState,
            virtio_blk: Vec<Vec<u8>>,
            virtio_net: Vec<Vec<u8>>,
            vsock: Option<vmm_devices::virtio::vsock::VirtioVsockMmioState>,
        }

        let bytes = postcard::to_allocvec(&LegacyStateBlob {
            entry: 0x100000,
            mem_size: 128 * crate::config::MIB,
            vcpu: VcpuStateSave::default(),
            kernel_path: "kernel".into(),
            cmdline: "console=ttyS0".into(),
            vcpus: 1,
            volumes: Vec::new(),
            net: Vec::new(),
            vcpu_full: None,
            vcpu_full_aps: Vec::new(),
            vm_full: None,
            serial: Default::default(),
            virtio_blk: Vec::new(),
            virtio_net: Vec::new(),
            vsock: None,
        })
        .unwrap();
        let (mut decoded, balloon, compatibility) = decode_state_blob(&bytes).unwrap();
        assert_eq!(decoded.kernel_path, "kernel");
        assert!(balloon.is_none());
        assert!(compatibility.is_none());
        let mut zero_padded = bytes.clone();
        zero_padded.resize(4096, 0);
        let (decoded_padded, padded_balloon, padded_compatibility) =
            decode_state_blob(&zero_padded).unwrap();
        assert_eq!(decoded_padded.kernel_path, "kernel");
        assert!(padded_balloon.is_none());
        assert!(padded_compatibility.is_none());

        let balloon_state = vmm_devices::virtio::balloon::VirtioBalloonMmioState::default();
        let serial_runtime = vmm_devices::serial::SerialRuntimeState {
            interrupt_identification: 4,
            line_status: 0x61,
            modem_status: 0xb0,
            in_buffer: b"pending".to_vec(),
        };
        decoded.serial_runtime = Some(serial_runtime.clone());
        let mut encoded = encode_state_blob(&decoded, Some(&balloon_state)).unwrap();
        encoded.resize(4096, 0);
        let (decoded_with_trailer, decoded_balloon, decoded_compatibility) =
            decode_state_blob(&encoded).unwrap();
        assert_eq!(decoded_with_trailer.kernel_path, "kernel");
        assert_eq!(decoded_with_trailer.serial_runtime, Some(serial_runtime));
        assert_eq!(decoded_balloon, Some(balloon_state));
        decoded_compatibility.unwrap().validate().unwrap();
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    #[test]
    fn snapshot_compatibility_rejects_build_and_template_boundaries() {
        let current = SnapshotCompatibility::current().unwrap();
        current.validate().unwrap();

        let mut incompatible_state = current.clone();
        incompatible_state.state_abi += 1;
        assert!(incompatible_state
            .validate()
            .unwrap_err()
            .to_string()
            .contains("state ABI"));

        let mut incompatible_devices = current.clone();
        incompatible_devices.device_model_abi += 1;
        assert!(incompatible_devices
            .validate()
            .unwrap_err()
            .to_string()
            .contains("device-model ABI"));

        let mut incompatible_template = current;
        incompatible_template.cpu_template = "different-template".into();
        assert!(incompatible_template
            .validate()
            .unwrap_err()
            .to_string()
            .contains("CPU template"));

        validate_snapshot_compatibility(LEGACY_SNAPSHOT_VERSION, None).unwrap();
        assert!(validate_snapshot_compatibility(SNAPSHOT_VERSION, None)
            .unwrap_err()
            .to_string()
            .contains("manifest is missing"));
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    #[test]
    fn live_restore_shape_rejects_partial_runtime_state() {
        let mut saved = StateBlob::default();

        let partial_memory_only = validate_restored_runtime_shape(&saved, false, false, true, 0, 1)
            .unwrap_err()
            .to_string();
        assert!(partial_memory_only.contains("partial live runtime state"));

        let partial_balloon = validate_restored_runtime_shape(&saved, true, false, false, 0, 1)
            .unwrap_err()
            .to_string();
        assert!(partial_balloon.contains("partial live runtime state"));

        let missing_vm = validate_restored_runtime_shape(&saved, true, true, false, 0, 1)
            .unwrap_err()
            .to_string();
        assert!(missing_vm.contains("missing in-kernel VM state"));

        let missing_ap = validate_restored_runtime_shape(&saved, true, true, true, 0, 2)
            .unwrap_err()
            .to_string();
        assert!(missing_ap.contains("AP state count mismatch"));

        let missing_vsock = validate_restored_runtime_shape(&saved, true, true, true, 0, 1)
            .unwrap_err()
            .to_string();
        assert!(missing_vsock.contains("missing virtio-vsock state"));

        saved.vsock = Some(Default::default());
        validate_restored_runtime_shape(&saved, true, true, true, 0, 1).unwrap();
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    #[test]
    fn malformed_runtime_components_are_rejected() {
        let error = decode_snapshot_component::<crate::vcpu_setup::VcpuFullState>(
            Some(&[0xff]),
            "BSP vCPU",
        )
        .err()
        .expect("malformed BSP state must fail")
        .to_string();
        assert!(error.contains("snapshot BSP vCPU state is malformed"));
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
    #[test]
    fn malformed_virtio_state_is_rejected_instead_of_reset() {
        use vmm_devices::virtio::blk_backend::BlkBackend;
        use vmm_devices::virtio::blk_transport::VirtioBlkMmio;
        use vmm_devices::virtio::net_transport::VirtioNetMmio;

        let backing = tempfile::tempfile().expect("create block backing");
        backing.set_len(4096).expect("size block backing");
        let backend = BlkBackend::from_file(backing, false, "restore-state-test")
            .expect("open block backend");
        let mut block = vec![Arc::new(VirtioBlkMmio::new(5, backend))];
        let block_error = restore_virtio_blk_states(&mut block, &[vec![0xff]])
            .unwrap_err()
            .to_string();
        assert!(block_error.contains("virtio-blk state 0 is malformed"));
        assert!(restore_virtio_blk_states(&mut block, &[])
            .unwrap_err()
            .to_string()
            .contains("count mismatch"));

        let mut net = vec![Arc::new(VirtioNetMmio::new(
            6,
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
        ))];
        let net_error = restore_virtio_net_states(&mut net, &[vec![0xff]])
            .unwrap_err()
            .to_string();
        assert!(net_error.contains("virtio-net state 0 is malformed"));
        assert!(restore_virtio_net_states(&mut net, &[])
            .unwrap_err()
            .to_string()
            .contains("count mismatch"));
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
        let (gm, state, version) = load_snapshot_chain(d2_s).unwrap();
        assert_eq!(state, b"state2");
        assert_eq!(version, SNAPSHOT_VERSION);
        let gm_len = usize::try_from(gm.size_bytes).expect("test memory size fits usize");
        // SAFETY: `gm` owns `gm_len` bytes for the duration of this assertion.
        let recon: &[u8] = unsafe { std::slice::from_raw_parts(gm.as_ptr(), gm_len) };
        assert_eq!(&recon[0..4096], &[0xAA; 4096][..], "page 0 from base");
        assert_eq!(&recon[4096..8192], &[0xBB; 4096][..], "page 1 from diff1");
        assert_eq!(&recon[8192..12288], &[0xCC; 4096][..], "page 2 from diff2");

        // Restoring an intermediate checkpoint (diff 1) reproduces only up to it.
        let (gm1, state1, version1) = load_snapshot_chain(d1_s).unwrap();
        assert_eq!(state1, b"state1");
        assert_eq!(version1, SNAPSHOT_VERSION);
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
    fn restore_rejects_snapshot_symlinks_and_trailing_data() {
        use std::io::Write;

        let dir = private_runtime_dir().unwrap();
        let base = unique_runtime_file_path("snapshot-input-test", "snap").unwrap();
        let link = dir.join(format!("snapshot-input-link-{}.snap", std::process::id()));
        let memory = vec![0u8; crate::config::MIB as usize];
        write_snapshot_file(base.to_str().unwrap(), b"state", &memory, false).unwrap();
        std::os::unix::fs::symlink(&base, &link).unwrap();

        let symlink_error = load_snapshot_chain(link.to_str().unwrap())
            .err()
            .expect("snapshot symlink must be rejected");
        assert!(symlink_error.to_string().contains("non-symlink"));

        std::fs::OpenOptions::new()
            .append(true)
            .open(&base)
            .unwrap()
            .write_all(b"trailing")
            .unwrap();
        let trailing_error = load_snapshot_chain(base.to_str().unwrap())
            .err()
            .expect("snapshot trailing data must be rejected");
        assert!(trailing_error.to_string().contains("trailing data"));

        let _ = std::fs::remove_file(link);
        let _ = std::fs::remove_file(base);
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
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
#[test]
fn vsock_runtime_path_fits_sockaddr_un() {
    use std::os::unix::ffi::OsStrExt;

    let path = unique_runtime_socket_path().unwrap();
    assert!(
        path.as_os_str().as_bytes().len() < 108,
        "{}",
        path.display()
    );
}
