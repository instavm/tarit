use crate::config::{Config, WarmClass};
use crate::net::{NetAlloc, NetProvisioner};
use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tarit_types::OrchError;
use tarit_vmm_client::{
    wait_for_socket, KernelConfig, MemoryConfig, NetConfig, VcpuConfig, VmConfig, VmmClient,
    VolumeConfig,
};
use uuid::Uuid;

pub const DEFAULT_CMDLINE: &str = "earlycon=uart8250,io,0x3f8,115200n8 console=ttyS0 reboot=k panic=1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw virtio_mmio.device=4K@0xd0000000:5";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VmSpawnConfig {
    pub memory_mib: u64,
    pub vcpus: u8,
    pub kernel_path: PathBuf,
    pub rootfs_path: Option<PathBuf>,
    pub cmdline: String,
    /// Mount the rootfs read-only (shared immutable base). Set from
    /// `Config::rootfs_read_only` so warm VMs and requests agree.
    pub read_only: bool,
}

impl VmSpawnConfig {
    pub fn from_defaults(config: &Config, req: &tarit_types::CreateVmRequest) -> Self {
        let rootfs_path = match &req.rootfs_path {
            Some(s) if s.is_empty() => None,
            Some(s) => Some(PathBuf::from(s)),
            None => Some(config.rootfs.clone()),
        };
        let cmdline = req.cmdline.clone().unwrap_or_else(|| {
            if rootfs_path.is_some() {
                DEFAULT_CMDLINE.to_string()
            } else {
                "console=ttyS0 panic=1".to_string()
            }
        });
        Self {
            memory_mib: req.memory_mib,
            vcpus: req.vcpus,
            kernel_path: req
                .kernel_path
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| config.kernel.clone()),
            rootfs_path,
            cmdline,
            read_only: config.rootfs_read_only,
        }
    }

    /// Build the spawn config for a warm-pool class (rootfs falls back to the
    /// host default). Must resolve to the same fields `from_defaults` would for
    /// an equivalent request, so a warm VM can be matched to a create request.
    pub fn from_warm_class(config: &Config, class: &WarmClass) -> Self {
        let rootfs_path = Some(
            class
                .rootfs
                .clone()
                .unwrap_or_else(|| config.rootfs.clone()),
        );
        Self {
            memory_mib: class.memory_mib,
            vcpus: class.vcpus,
            kernel_path: config.kernel.clone(),
            rootfs_path,
            cmdline: DEFAULT_CMDLINE.to_string(),
            read_only: config.rootfs_read_only,
        }
    }
}

#[derive(Debug)]
struct RunningVm {
    pid: u32,
    socket_path: PathBuf,
    process: ManagedProcess,
    net: Option<NetAlloc>,
}

#[derive(Default)]
struct NetworkLeaseState {
    active: usize,
    pending_teardown: Option<NetAlloc>,
    teardown_in_progress: bool,
}

#[derive(Default)]
struct StopState {
    stopping: HashSet<Uuid>,
}

impl StopState {
    fn begin(&mut self, id: Uuid) -> bool {
        self.stopping.insert(id)
    }

    fn complete(&mut self, id: Uuid) {
        self.stopping.remove(&id);
    }
}

impl NetworkLeaseState {
    fn acquire(&mut self) {
        self.active += 1;
    }

    fn defer_teardown(&mut self, allocation: NetAlloc) -> Option<NetAlloc> {
        if self.active == 0 {
            Some(allocation)
        } else {
            self.pending_teardown = Some(allocation);
            None
        }
    }

    fn release(&mut self) -> Option<NetAlloc> {
        self.active = self.active.saturating_sub(1);
        if self.active != 0 {
            return None;
        }
        let teardown = self.pending_teardown.take();
        self.teardown_in_progress = teardown.is_some();
        teardown
    }

    fn teardown_in_progress(&self) -> bool {
        self.teardown_in_progress
    }

    fn complete_teardown(&mut self) {
        self.teardown_in_progress = false;
    }
}

pub(crate) struct NetworkLease {
    supervisor: Arc<VmmSupervisor>,
    id: Uuid,
    allocation: NetAlloc,
}

impl NetworkLease {
    pub(crate) fn allocation(&self) -> &NetAlloc {
        &self.allocation
    }
}

impl Drop for NetworkLease {
    fn drop(&mut self) {
        self.supervisor.release_network_lease(self.id);
    }
}

#[derive(Debug, Clone)]
struct ManagedProcess {
    pid: u32,
    child: Arc<Mutex<Child>>,
}

impl ManagedProcess {
    fn new(child: Child) -> Self {
        let pid = child.id();
        Self {
            pid,
            child: Arc::new(Mutex::new(child)),
        }
    }

    fn kill_wait(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Debug, Clone)]
struct BootingVm {
    socket_path: PathBuf,
    process: ManagedProcess,
}

/// A pre-booted VM held in the warm pool, ready to be assigned instantly.
#[derive(Debug)]
struct WarmVm {
    id: Uuid,
    vm: RunningVm,
    spec: VmSpawnConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpawnPurpose {
    Live,
    Refill,
}

pub struct VmmSupervisor {
    config: Config,
    running: Mutex<HashMap<Uuid, RunningVm>>,
    stopping: Mutex<StopState>,
    network_leases: Mutex<HashMap<Uuid, NetworkLeaseState>>,
    booting: Mutex<HashMap<Uuid, BootingVm>>,
    /// Pre-booted, unassigned VMs kept ready by the warm-pool replenisher.
    warm: Mutex<VecDeque<WarmVm>>,
    net: Option<NetProvisioner>,
    shutting_down: AtomicBool,
}

#[derive(Debug, Default, Clone)]
pub struct ShutdownSummary {
    pub running_ids: Vec<Uuid>,
    pub running: usize,
    pub warm: usize,
    pub booting: usize,
}

impl ShutdownSummary {
    pub fn total(&self) -> usize {
        self.running + self.warm + self.booting
    }
}

impl VmmSupervisor {
    #[cfg(test)]
    pub fn new(config: Config) -> Self {
        Self::new_with_live_vms(config, std::iter::empty())
    }

    pub fn new_with_live_vms(config: Config, live_vm_ids: impl IntoIterator<Item = Uuid>) -> Self {
        std::fs::create_dir_all(&config.socket_dir).ok();
        let net = if config.enable_net {
            match NetProvisioner::new(config.net_state_path.clone(), live_vm_ids) {
                Ok(p) => {
                    tracing::info!(uplink = p.uplink(), "per-VM networking enabled");
                    Some(p)
                }
                Err(e) => {
                    tracing::error!("net provisioning disabled (setup failed): {e}");
                    None
                }
            }
        } else {
            None
        };
        Self {
            config,
            running: Mutex::new(HashMap::new()),
            stopping: Mutex::new(StopState::default()),
            network_leases: Mutex::new(HashMap::new()),
            booting: Mutex::new(HashMap::new()),
            warm: Mutex::new(VecDeque::new()),
            net,
            shutting_down: AtomicBool::new(false),
        }
    }

    fn socket_path_for(&self, id: Uuid) -> PathBuf {
        self.config.socket_dir.join(format!("{id}.sock"))
    }

    /// Build `vmm serve` cgroup arguments for a VM when a parent cgroup is
    /// configured (R-004). The VMM creates the per-VM cgroup, applies the limits
    /// and places itself in it before serving. `memory_mib` is `Some` for a
    /// fresh boot (memory.max is sized with generous headroom over guest RAM)
    /// and `None` for restore (guest memory is carried by the snapshot, so only
    /// pids.max is capped). Returns an empty vec when no parent is configured.
    fn cgroup_args(&self, id: Uuid, memory_mib: Option<u64>) -> Vec<String> {
        let Some(parent) = self.config.vm_cgroup_parent.as_deref() else {
            return Vec::new();
        };
        let path = format!("{}/tarit-{id}", parent.trim_end_matches('/'));
        let mut args = vec![
            "--cgroup".to_string(),
            path,
            "--cgroup-pids-max".to_string(),
            self.config.vm_cgroup_pids_max.to_string(),
        ];
        if let Some(mem) = memory_mib {
            // 1.5x guest RAM + 256 MiB headroom: legitimate VMs never hit the
            // ceiling, a runaway allocation is still capped.
            let max_mib = mem + mem / 2 + 256;
            args.push("--cgroup-memory-max".to_string());
            args.push(format!("{max_mib}M"));
        }
        args
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    fn shutdown_error(&self) -> OrchError {
        OrchError::Overloaded {
            message: "taritd is shutting down".into(),
            retry_after_secs: 1,
        }
    }

    fn ensure_accepting_work(&self) -> Result<(), OrchError> {
        if self.is_shutting_down() {
            Err(self.shutdown_error())
        } else {
            Ok(())
        }
    }

    fn ensure_refill_active(&self, cancelled: &AtomicBool) -> Result<(), OrchError> {
        if cancelled.load(Ordering::Acquire) {
            Err(self.shutdown_error())
        } else {
            self.ensure_accepting_work()
        }
    }

    fn refill_cancelled(&self, cancelled: Option<&AtomicBool>) -> bool {
        self.is_shutting_down()
            || cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Acquire))
    }

    fn move_pid_to_refill_cgroup(&self, pid: u32) {
        let cgroup = &self.config.warm_pool.refill_cgroup;
        let Some(path) = cgroup.path.as_ref() else {
            return;
        };
        if let Err(e) = move_pid_to_configured_refill_cgroup(pid, path, cgroup.cpu_weight) {
            tracing::warn!(
                pid,
                path = %path.display(),
                cpu_weight = cgroup.cpu_weight,
                "refill cgroup placement skipped: {e}"
            );
        }
    }

    fn move_pid_to_default_cgroup(&self, pid: u32) {
        if self.config.warm_pool.refill_cgroup.path.is_none() {
            return;
        }
        match default_cgroup_path() {
            Some(path) => {
                if let Err(e) = write_pid_to_cgroup(&path, pid) {
                    tracing::warn!(
                        pid,
                        path = %path.display(),
                        "failed to move leased warm VM back to default cgroup: {e}"
                    );
                }
            }
            None => {
                tracing::warn!(
                    pid,
                    "failed to move leased warm VM back to default cgroup: cgroup v2 path unavailable"
                );
            }
        }
    }

    fn track_booting(
        &self,
        id: Uuid,
        socket_path: PathBuf,
        process: ManagedProcess,
    ) -> Result<(), OrchError> {
        let mut booting = self
            .booting
            .lock()
            .map_err(|_| OrchError::Internal("supervisor booting lock poisoned".into()))?;
        booting.insert(
            id,
            BootingVm {
                socket_path,
                process,
            },
        );
        Ok(())
    }

    fn untrack_booting(&self, id: Uuid) {
        if let Ok(mut booting) = self.booting.lock() {
            booting.remove(&id);
        }
    }

    /// Boot a VM (spawn `vmm serve`, wait for its socket, provision networking,
    /// send Create) WITHOUT holding the running/warm locks, so many VMs can be
    /// booted concurrently (warm-pool replenish + cold create in parallel).
    fn boot_vm(
        &self,
        id: Uuid,
        vm_config: &VmSpawnConfig,
        purpose: SpawnPurpose,
        refill_cancelled: Option<&AtomicBool>,
    ) -> Result<RunningVm, OrchError> {
        self.ensure_accepting_work()?;
        if let Some(cancelled) = refill_cancelled {
            self.ensure_refill_active(cancelled)?;
        }
        let socket_path = self.socket_path_for(id);
        let _ = std::fs::remove_file(&socket_path);

        let cgroup_args = self.cgroup_args(id, Some(vm_config.memory_mib));
        let child = Command::new(&self.config.vmm_bin)
            .arg("serve")
            .arg("--socket")
            .arg(&socket_path)
            .args(&cgroup_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| OrchError::Internal(format!("spawn vmm: {e}")))?;

        let process = ManagedProcess::new(child);
        let pid = process.pid;
        if purpose == SpawnPurpose::Refill {
            self.move_pid_to_refill_cgroup(pid);
        }
        self.track_booting(id, socket_path.clone(), process.clone())?;

        if let Err(e) = self.wait_for_socket(&socket_path, refill_cancelled) {
            self.untrack_booting(id);
            process.kill_wait();
            return Err(e);
        }

        if self.refill_cancelled(refill_cancelled) {
            self.untrack_booting(id);
            process.kill_wait();
            let _ = std::fs::remove_file(&socket_path);
            return Err(self.shutdown_error());
        }

        // Provision per-VM host networking (tap + /30 + NAT) if enabled. The
        // guest auto-configures eth0 from the kernel `ip=` cmdline we append.
        let net_alloc = match &self.net {
            Some(p) => match p.provision(id) {
                Ok(a) => Some(a),
                Err(e @ OrchError::Overloaded { .. }) => {
                    self.untrack_booting(id);
                    process.kill_wait();
                    let _ = std::fs::remove_file(&socket_path);
                    return Err(e);
                }
                Err(e) => {
                    self.untrack_booting(id);
                    process.kill_wait();
                    let _ = std::fs::remove_file(&socket_path);
                    return Err(OrchError::Internal(format!("net provision: {e}")));
                }
            },
            None => None,
        };
        if self.refill_cancelled(refill_cancelled) {
            self.untrack_booting(id);
            process.kill_wait();
            let _ = std::fs::remove_file(&socket_path);
            let _ = std::fs::remove_file(overlay_path_for(id));
            if let (Some(p), Some(a)) = (&self.net, &net_alloc) {
                p.teardown(a);
            }
            return Err(self.shutdown_error());
        }

        let vmm_config = build_vmm_config(id, vm_config, net_alloc.as_ref());
        let client = VmmClient::new(&socket_path);
        if let Err(e) = client.create(vmm_config) {
            self.untrack_booting(id);
            process.kill_wait();
            let _ = std::fs::remove_file(&socket_path);
            let _ = std::fs::remove_file(overlay_path_for(id));
            if let (Some(p), Some(a)) = (&self.net, &net_alloc) {
                p.teardown(a);
            }
            return Err(OrchError::Vmm(format!("create vm: {e}")));
        }
        self.untrack_booting(id);
        if self.refill_cancelled(refill_cancelled) {
            let vm = RunningVm {
                pid,
                socket_path,
                process,
                net: net_alloc,
            };
            self.teardown_vm(id, vm);
            return Err(self.shutdown_error());
        }

        Ok(RunningVm {
            pid,
            socket_path,
            process,
            net: net_alloc,
        })
    }

    fn wait_for_socket(
        &self,
        socket_path: &Path,
        refill_cancelled: Option<&AtomicBool>,
    ) -> Result<(), OrchError> {
        let Some(cancelled) = refill_cancelled else {
            return wait_for_socket(socket_path, Duration::from_secs(30))
                .map_err(|e| OrchError::Vmm(format!("wait for socket: {e}")));
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            self.ensure_refill_active(cancelled)?;
            if UnixStream::connect(socket_path).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(OrchError::Vmm("wait for socket: timed out".into()))
    }

    pub fn spawn_vm(
        &self,
        id: Uuid,
        vm_config: VmSpawnConfig,
    ) -> Result<(u32, PathBuf), OrchError> {
        let vm = self.boot_vm(id, &vm_config, SpawnPurpose::Live, None)?;
        let pid = vm.pid;
        let socket_path = vm.socket_path.clone();
        if self.is_shutting_down() {
            self.teardown_vm(id, vm);
            return Err(self.shutdown_error());
        }
        let mut guard = self
            .running
            .lock()
            .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?;
        if self.is_shutting_down() {
            drop(guard);
            self.teardown_vm(id, vm);
            return Err(self.shutdown_error());
        }
        guard.insert(id, vm);
        Ok((pid, socket_path))
    }

    /// Restore a VM from a node-local snapshot file: spawn a fresh `vmm serve`,
    /// send Restore, and register the resumed VM. The snapshot carries the
    /// guest's device/net config, so we do not re-provision host networking
    /// here (restore is used for the fast warm/resume path).
    pub fn restore_vm(&self, id: Uuid, snapshot_path: &str) -> Result<(u32, PathBuf), OrchError> {
        let vm = self.spawn_and_restore(id, snapshot_path, None, SpawnPurpose::Live, None)?;
        let pid = vm.pid;
        let socket_path = vm.socket_path.clone();
        if self.is_shutting_down() {
            self.teardown_vm(id, vm);
            return Err(self.shutdown_error());
        }
        let mut guard = self
            .running
            .lock()
            .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?;
        if self.is_shutting_down() {
            drop(guard);
            self.teardown_vm(id, vm);
            return Err(self.shutdown_error());
        }
        guard.insert(id, vm);
        Ok((pid, socket_path))
    }

    fn spawn_and_restore(
        &self,
        id: Uuid,
        snapshot_path: &str,
        overlay: Option<String>,
        purpose: SpawnPurpose,
        refill_cancelled: Option<&AtomicBool>,
    ) -> Result<RunningVm, OrchError> {
        self.ensure_accepting_work()?;
        if let Some(cancelled) = refill_cancelled {
            self.ensure_refill_active(cancelled)?;
        }
        let socket_path = self.socket_path_for(id);
        let _ = std::fs::remove_file(&socket_path);

        let cgroup_args = self.cgroup_args(id, None);
        let child = Command::new(&self.config.vmm_bin)
            .arg("serve")
            .arg("--socket")
            .arg(&socket_path)
            .args(&cgroup_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| OrchError::Internal(format!("spawn vmm: {e}")))?;
        let process = ManagedProcess::new(child);
        let pid = process.pid;
        if purpose == SpawnPurpose::Refill {
            self.move_pid_to_refill_cgroup(pid);
        }
        self.track_booting(id, socket_path.clone(), process.clone())?;

        if let Err(e) = self.wait_for_socket(&socket_path, refill_cancelled) {
            self.untrack_booting(id);
            process.kill_wait();
            return Err(e);
        }
        if self.refill_cancelled(refill_cancelled) {
            self.untrack_booting(id);
            process.kill_wait();
            let _ = std::fs::remove_file(&socket_path);
            if let Some(overlay) = overlay {
                let _ = std::fs::remove_file(overlay);
            }
            return Err(self.shutdown_error());
        }

        let client = VmmClient::new(&socket_path);
        if let Err(e) = client.restore(snapshot_path, overlay.clone()) {
            self.untrack_booting(id);
            process.kill_wait();
            let _ = std::fs::remove_file(&socket_path);
            if let Some(overlay) = overlay {
                let _ = std::fs::remove_file(overlay);
            }
            return Err(OrchError::Vmm(format!("restore vm: {e}")));
        }
        self.untrack_booting(id);
        if self.refill_cancelled(refill_cancelled) {
            let vm = RunningVm {
                pid,
                socket_path: socket_path.clone(),
                process,
                net: None,
            };
            self.teardown_vm(id, vm);
            return Err(self.shutdown_error());
        }

        let vm = RunningVm {
            pid,
            socket_path: socket_path.clone(),
            process,
            net: None,
        };
        Ok(vm)
    }

    /// Boot one warm-pool VM of `class` and park it in the warm queue. The boot
    /// happens without the warm lock held; only the final enqueue takes it.
    /// Block until the guest agent can actually run a command, so we never park a
    /// still-booting VM. A freshly-parked, not-yet-ready VM handed out during a
    /// burst blocks the caller for seconds on its first agent dial (the burst
    /// p95 tail). Bounded; parks anyway on timeout so a wedged guest can't stall
    /// replenishment forever.
    fn await_ready(&self, socket: &Path, cancelled: &AtomicBool) -> Result<(), OrchError> {
        let client = VmmClient::new(socket);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            self.ensure_refill_active(cancelled)?;
            if client
                .exec("true", 1000)
                .map(|(code, _, _, _)| code == 0)
                .unwrap_or(false)
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.ensure_refill_active(cancelled)
    }

    pub fn spawn_warm_cancellable(
        &self,
        class: &WarmClass,
        cancelled: &AtomicBool,
    ) -> Result<(), OrchError> {
        self.ensure_refill_active(cancelled)?;
        let id = Uuid::new_v4();
        let spec = VmSpawnConfig::from_warm_class(&self.config, class);
        let vm = self.boot_vm(id, &spec, SpawnPurpose::Refill, Some(cancelled))?;
        if let Err(error) = self.await_ready(&vm.socket_path, cancelled) {
            self.teardown_vm(id, vm);
            return Err(error);
        }
        if cancelled.load(Ordering::Acquire) || self.is_shutting_down() {
            self.teardown_vm(id, vm);
            return Err(self.shutdown_error());
        }
        let mut warm = match self.warm.lock() {
            Ok(warm) => warm,
            Err(_) => {
                self.teardown_vm(id, vm);
                return Err(OrchError::Internal("warm lock poisoned".into()));
            }
        };
        if cancelled.load(Ordering::Acquire) || self.is_shutting_down() {
            drop(warm);
            self.teardown_vm(id, vm);
            return Err(self.shutdown_error());
        }
        warm.push_back(WarmVm { id, vm, spec });
        Ok(())
    }

    /// Cold-boot one VM for `class`, wait until it is ready, take a full golden
    /// snapshot, then tear down the builder VM. Runtime warm capacity is filled
    /// by restoring clones from the returned snapshot.
    pub fn create_golden_cancellable(
        &self,
        class: &WarmClass,
        cancelled: &AtomicBool,
    ) -> Result<String, OrchError> {
        self.ensure_refill_active(cancelled)?;
        let id = Uuid::new_v4();
        let spec = VmSpawnConfig::from_warm_class(&self.config, class);
        let vm = self.boot_vm(id, &spec, SpawnPurpose::Refill, Some(cancelled))?;
        if let Err(error) = self.await_ready(&vm.socket_path, cancelled) {
            self.teardown_vm(id, vm);
            return Err(error);
        }
        if cancelled.load(Ordering::Acquire) || self.is_shutting_down() {
            self.teardown_vm(id, vm);
            return Err(self.shutdown_error());
        }
        let client = VmmClient::new(&vm.socket_path);
        self.ensure_refill_active(cancelled)?;
        let snapshot_path = match client.snapshot(false) {
            Ok(path) => path,
            Err(e) => {
                self.teardown_vm(id, vm);
                return Err(OrchError::Vmm(format!("snapshot golden: {e}")));
            }
        };

        self.teardown_vm(id, vm);
        self.ensure_refill_active(cancelled)?;
        Ok(snapshot_path)
    }

    /// Restore one warm-pool VM from an existing golden snapshot and park it.
    pub fn spawn_warm_restore_cancellable(
        &self,
        class: &WarmClass,
        snapshot_path: &str,
        cancelled: &AtomicBool,
    ) -> Result<(), OrchError> {
        self.ensure_refill_active(cancelled)?;
        let id = Uuid::new_v4();
        let spec = VmSpawnConfig::from_warm_class(&self.config, class);
        let overlay = overlay_path_for_config(id, &spec);
        let vm = self.spawn_and_restore(
            id,
            snapshot_path,
            overlay,
            SpawnPurpose::Refill,
            Some(cancelled),
        )?;
        if let Err(error) = self.await_ready(&vm.socket_path, cancelled) {
            self.teardown_vm(id, vm);
            return Err(error);
        }
        if cancelled.load(Ordering::Acquire) || self.is_shutting_down() {
            self.teardown_vm(id, vm);
            return Err(self.shutdown_error());
        }
        let mut warm = match self.warm.lock() {
            Ok(warm) => warm,
            Err(_) => {
                self.teardown_vm(id, vm);
                return Err(OrchError::Internal("warm lock poisoned".into()));
            }
        };
        if cancelled.load(Ordering::Acquire) || self.is_shutting_down() {
            drop(warm);
            self.teardown_vm(id, vm);
            return Err(self.shutdown_error());
        }
        warm.push_back(WarmVm { id, vm, spec });
        Ok(())
    }

    /// Try to hand out a warm VM whose spec matches `want`, moving it into the
    /// running set under its own id. Returns (id, pid, socket) or None if the
    /// pool has no match (caller then cold-starts).
    pub fn take_warm(&self, want: &VmSpawnConfig) -> Option<(Uuid, u32, PathBuf)> {
        if self.is_shutting_down() {
            return None;
        }
        let mut warm = self.warm.lock().ok()?;
        let pos = warm.iter().position(|w| &w.spec == want)?;
        let taken = warm.remove(pos)?;
        drop(warm);
        let pid = taken.vm.pid;
        let socket = taken.vm.socket_path.clone();
        self.move_pid_to_default_cgroup(pid);
        if self.is_shutting_down() {
            self.teardown_vm(taken.id, taken.vm);
            return None;
        }
        let mut running = self.running.lock().ok()?;
        if self.is_shutting_down() {
            drop(running);
            self.teardown_vm(taken.id, taken.vm);
            return None;
        }
        running.insert(taken.id, taken.vm);
        Some((taken.id, pid, socket))
    }

    /// Number of warm VMs currently parked for the given class shape.
    pub fn warm_count(&self, vcpus: u8, memory_mib: u64) -> usize {
        self.warm
            .lock()
            .map(|w| {
                w.iter()
                    .filter(|x| x.spec.vcpus == vcpus && x.spec.memory_mib == memory_mib)
                    .count()
            })
            .unwrap_or(0)
    }

    fn client_for(&self, id: Uuid) -> Result<VmmClient, OrchError> {
        let guard = self
            .running
            .lock()
            .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?;
        let running = guard
            .get(&id)
            .ok_or_else(|| OrchError::NotFound(format!("vm {id} not running")))?;
        let client = VmmClient::new(running.socket_path.clone());
        Ok(client)
    }

    pub fn stop_vm(&self, id: Uuid) -> Result<(), OrchError> {
        let should_stop = self
            .stopping
            .lock()
            .map_err(|_| OrchError::Internal("supervisor stop lock poisoned".into()))?
            .begin(id);
        if !should_stop {
            return Ok(());
        }
        let result = self.stop_vm_once(id);
        self.complete_stop(id);
        result
    }

    fn stop_vm_once(&self, id: Uuid) -> Result<(), OrchError> {
        // Remove from the running map under a brief lock, then do the slow
        // teardown (stop RPC + kill + wait) WITHOUT the lock held. Otherwise
        // every concurrent exec/create/delete serializes behind this VM's
        // child.wait(), which collapses burst throughput (the burst p95 tail).
        let running = {
            let mut guard = self
                .running
                .lock()
                .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?;
            guard.remove(&id)
        };
        let Some(running) = running else {
            if self.has_active_network_lease(id) {
                return Ok(());
            }
            if let Some(net) = &self.net {
                net.teardown_vm_id(id);
            }
            return Ok(());
        };

        let client = VmmClient::new(&running.socket_path);
        let _ = client.stop();
        self.teardown_vm(id, running);
        Ok(())
    }

    pub fn pause_vm(&self, id: Uuid) -> Result<(), OrchError> {
        let client = self.client_for(id)?;
        client.pause().map_err(|e| OrchError::Vmm(e.to_string()))
    }

    pub fn resume_vm(&self, id: Uuid) -> Result<(), OrchError> {
        let client = self.client_for(id)?;
        client.resume().map_err(|e| OrchError::Vmm(e.to_string()))
    }

    #[allow(dead_code)]
    pub fn network_allocation(&self, id: Uuid) -> Result<NetAlloc, OrchError> {
        self.running
            .lock()
            .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?
            .get(&id)
            .and_then(|vm| vm.net.clone())
            .ok_or_else(|| OrchError::Conflict(format!("vm {id} has no active network")))
    }

    pub(crate) fn acquire_network_lease(
        self: &Arc<Self>,
        id: Uuid,
    ) -> Result<NetworkLease, OrchError> {
        let mut leases = self
            .network_leases
            .lock()
            .map_err(|_| OrchError::Internal("supervisor network lease lock poisoned".into()))?;
        let allocation = self
            .running
            .lock()
            .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?
            .get(&id)
            .and_then(|vm| vm.net.clone())
            .ok_or_else(|| OrchError::Conflict(format!("vm {id} has no active network")))?;
        leases.entry(id).or_default().acquire();
        Ok(NetworkLease {
            supervisor: Arc::clone(self),
            id,
            allocation,
        })
    }

    #[cfg(test)]
    pub(crate) fn install_test_network_allocation(&self, id: Uuid, allocation: NetAlloc) {
        let process = ManagedProcess::new(Command::new("true").spawn().unwrap());
        self.running.lock().unwrap().insert(
            id,
            RunningVm {
                pid: process.pid,
                socket_path: PathBuf::new(),
                process,
                net: Some(allocation),
            },
        );
    }

    /// Live VMM status (state/uptime/vcpus/mem/config/vcpu_alive) for a running VM.
    pub fn status_vm(&self, id: Uuid) -> Result<tarit_vmm_client::VmStatus, OrchError> {
        let client = self.client_for(id)?;
        client.status().map_err(|e| OrchError::Vmm(e.to_string()))
    }

    pub fn exec_vm(
        &self,
        id: Uuid,
        command: &str,
        timeout_ms: u64,
    ) -> Result<(i32, String, String, u64), OrchError> {
        let client = self.client_for(id)?;
        // Right after boot the guest agent may not have accepted the vsock
        // connection yet (EAGAIN). Retry the LIVE exec briefly. Never fall back
        // to a fake path: a real failure MUST surface as an error, not a
        // hardcoded exit 0 with empty output (which silently corrupts results).
        let mut last = String::from("guest agent unavailable");
        for _ in 0..100 {
            match client.exec(command, timeout_ms) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    last = e.to_string();
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        Err(OrchError::Vmm(format!(
            "exec: guest agent never responded: {last}"
        )))
    }

    pub fn snapshot_vm(&self, id: Uuid, diff: bool) -> Result<String, OrchError> {
        let client = self.client_for(id)?;
        client
            .snapshot(diff)
            .map_err(|e| OrchError::Vmm(e.to_string()))
    }

    pub fn update_egress(
        &self,
        id: Uuid,
        allowlist: Vec<String>,
        allow_existing: bool,
    ) -> Result<usize, OrchError> {
        // R-005: enforce the allowlist on the orchestrator-owned host networking
        // path. Without provisioned networking there is no tap/guest IP to
        // filter, so we refuse rather than report a policy we did not apply.
        let Some(provisioner) = self.net.as_ref() else {
            return Err(OrchError::BadRequest(
                "egress enforcement requires orchestrator-provisioned networking (TARIT_ENABLE_NET=1)"
                    .into(),
            ));
        };
        let alloc = {
            let running = self
                .running
                .lock()
                .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?;
            running.get(&id).and_then(|vm| vm.net.clone())
        };
        let Some(alloc) = alloc else {
            return Err(OrchError::BadRequest(
                "VM has no orchestrator-provisioned network to enforce egress on".into(),
            ));
        };
        provisioner.apply_egress(&alloc, &allowlist, allow_existing)
    }

    pub fn attach_pty(
        &self,
        id: Uuid,
        cols: u16,
        rows: u16,
        shell: Option<String>,
    ) -> Result<UnixStream, OrchError> {
        let client = self.client_for(id)?;
        client
            .attach_pty(cols, rows, shell)
            .map_err(|e| OrchError::Vmm(e.to_string()))
    }

    pub fn is_running(&self, id: Uuid) -> bool {
        self.running
            .lock()
            .map(|g| g.contains_key(&id))
            .unwrap_or(false)
    }

    pub fn stop_all(&self) -> Result<ShutdownSummary, OrchError> {
        self.shutting_down.store(true, Ordering::SeqCst);
        let running = {
            let mut stopping = self
                .stopping
                .lock()
                .map_err(|_| OrchError::Internal("supervisor stop lock poisoned".into()))?;
            let mut guard = self
                .running
                .lock()
                .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?;
            let running = guard.drain().collect::<Vec<_>>();
            for (id, _) in &running {
                stopping.begin(*id);
            }
            running
        };
        let warm = {
            let mut guard = self
                .warm
                .lock()
                .map_err(|_| OrchError::Internal("warm lock poisoned".into()))?;
            guard.drain(..).collect::<Vec<_>>()
        };
        let booting = {
            let mut guard = self
                .booting
                .lock()
                .map_err(|_| OrchError::Internal("supervisor booting lock poisoned".into()))?;
            guard.drain().collect::<Vec<_>>()
        };

        let running_ids = running.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        let summary = ShutdownSummary {
            running_ids,
            running: running.len(),
            warm: warm.len(),
            booting: booting.len(),
        };

        for (id, vm) in running {
            let client = VmmClient::new(&vm.socket_path);
            let _ = client.stop();
            self.teardown_vm(id, vm);
            self.complete_stop(id);
        }
        for warm_vm in warm {
            let client = VmmClient::new(&warm_vm.vm.socket_path);
            let _ = client.stop();
            self.teardown_vm(warm_vm.id, warm_vm.vm);
        }
        for (id, vm) in booting {
            vm.process.kill_wait();
            let _ = std::fs::remove_file(&vm.socket_path);
            let _ = std::fs::remove_file(overlay_path_for(id));
        }

        Ok(summary)
    }

    fn teardown_vm(&self, id: Uuid, vm: RunningVm) {
        vm.process.kill_wait();
        let _ = std::fs::remove_file(&vm.socket_path);
        let _ = std::fs::remove_file(overlay_path_for(id));
        if let (Some(p), Some(a)) = (&self.net, &vm.net) {
            if let Some(allocation) = self.defer_network_teardown(id, a.clone()) {
                p.teardown(&allocation);
            }
        }
    }

    fn has_active_network_lease(&self, id: Uuid) -> bool {
        self.network_leases
            .lock()
            .map(|leases| {
                leases
                    .get(&id)
                    .is_some_and(|lease| lease.active > 0 || lease.teardown_in_progress())
            })
            .unwrap_or(true)
    }

    fn complete_stop(&self, id: Uuid) {
        if let Ok(mut stopping) = self.stopping.lock() {
            stopping.complete(id);
        }
    }

    fn defer_network_teardown(&self, id: Uuid, allocation: NetAlloc) -> Option<NetAlloc> {
        let Ok(mut leases) = self.network_leases.lock() else {
            return Some(allocation);
        };
        let Some(lease) = leases.get_mut(&id) else {
            return Some(allocation);
        };
        let teardown = lease.defer_teardown(allocation);
        if lease.active == 0 && !lease.teardown_in_progress() {
            leases.remove(&id);
        }
        teardown
    }

    fn release_network_lease(&self, id: Uuid) {
        let teardown = {
            let Ok(mut leases) = self.network_leases.lock() else {
                return;
            };
            let Some(lease) = leases.get_mut(&id) else {
                return;
            };
            let teardown = lease.release();
            if lease.active == 0 && !lease.teardown_in_progress() {
                leases.remove(&id);
            }
            teardown
        };
        if let Some(allocation) = teardown {
            if let Some(provisioner) = &self.net {
                provisioner.teardown(&allocation);
            }
            self.complete_network_teardown(id);
        }
    }

    fn complete_network_teardown(&self, id: Uuid) {
        let Ok(mut leases) = self.network_leases.lock() else {
            return;
        };
        let Some(lease) = leases.get_mut(&id) else {
            return;
        };
        lease.complete_teardown();
        if lease.active == 0 {
            leases.remove(&id);
        }
    }
}

impl Drop for VmmSupervisor {
    fn drop(&mut self) {
        if let Ok(summary) = self.stop_all() {
            if summary.total() > 0 {
                tracing::warn!(
                    running = summary.running,
                    warm = summary.warm,
                    booting = summary.booting,
                    "supervisor dropped with live VMs; reaped as safety net"
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn move_pid_to_configured_refill_cgroup(
    pid: u32,
    cgroup_dir: &Path,
    cpu_weight: u64,
) -> std::io::Result<()> {
    std::fs::create_dir_all(cgroup_dir)?;
    write_single_file(&cgroup_dir.join("cpu.weight"), &cpu_weight.to_string())?;
    write_pid_to_cgroup(cgroup_dir, pid)
}

#[cfg(not(target_os = "linux"))]
fn move_pid_to_configured_refill_cgroup(
    _pid: u32,
    _cgroup_dir: &Path,
    _cpu_weight: u64,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "cgroup v2 is only available on Linux",
    ))
}

#[cfg(target_os = "linux")]
fn write_pid_to_cgroup(cgroup_dir: &Path, pid: u32) -> std::io::Result<()> {
    write_single_file(&cgroup_dir.join("cgroup.procs"), &pid.to_string())
}

#[cfg(not(target_os = "linux"))]
fn write_pid_to_cgroup(_cgroup_dir: &Path, _pid: u32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "cgroup v2 is only available on Linux",
    ))
}

#[cfg(target_os = "linux")]
fn write_single_file(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    let bytes = contents.as_bytes();
    let written = file.write(bytes)?;
    if written == bytes.len() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            format!("short cgroup write to {}", path.display()),
        ))
    }
}

#[cfg(target_os = "linux")]
fn default_cgroup_path() -> Option<PathBuf> {
    parse_self_cgroup(&std::fs::read_to_string("/proc/self/cgroup").ok()?)
}

#[cfg(not(target_os = "linux"))]
fn default_cgroup_path() -> Option<PathBuf> {
    None
}

#[cfg(target_os = "linux")]
fn parse_self_cgroup(contents: &str) -> Option<PathBuf> {
    let relative = contents
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim();
    let root = PathBuf::from("/sys/fs/cgroup");
    if relative == "/" {
        Some(root)
    } else {
        Some(root.join(relative.trim_start_matches('/')))
    }
}

fn build_vmm_config(id: Uuid, cfg: &VmSpawnConfig, net: Option<&NetAlloc>) -> VmConfig {
    let mut volumes = Vec::new();
    // A shared read-only base gets a per-VM sparse CoW overlay so every assigned
    // VM is truly isolated (its writes go to its own overlay; the base stays
    // byte-for-byte unchanged) AND writable. The overlay is thin-provisioned: it
    // costs 0 bytes until written, up to the base's virtual size -- make the base
    // a large sparse image to hand each VM a big writable disk for free.
    let overlay = overlay_path_for_config(id, cfg);
    if let Some(rootfs) = &cfg.rootfs_path {
        volumes.push(VolumeConfig {
            path: rootfs.display().to_string(),
            read_only: cfg.read_only,
            overlay: overlay.clone(),
        });
    }

    // With an overlay the guest disk IS writable (writes land in the overlay), so
    // it must mount `rw`. Only a read-only base with NO overlay needs `ro` (else
    // ext4 replays its journal onto a read-only device and panics).
    let base_cmdline = if cfg.read_only && overlay.is_none() {
        cfg.cmdline.replace("root=/dev/vda rw", "root=/dev/vda ro")
    } else {
        cfg.cmdline.clone()
    };

    // With per-VM networking, attach a virtio-net device on the provisioned tap
    // and append the kernel `ip=` fragment so the guest configures eth0 at boot.
    let (nets, cmdline) = match net {
        Some(a) => (
            vec![NetConfig {
                tap: a.tap.clone(),
                guest_mac: None,
                guest_ip: Some(a.guest_ip.clone()),
                port_forwards: vec![],
            }],
            format!("{} {}", base_cmdline.trim(), a.ip_cmdline()),
        ),
        None => (vec![], base_cmdline),
    };

    VmConfig {
        kernel: KernelConfig {
            path: cfg.kernel_path.display().to_string(),
            cmdline,
            initramfs: None,
        },
        memory: MemoryConfig {
            size_mib: cfg.memory_mib,
        },
        vcpus: VcpuConfig { count: cfg.vcpus },
        volumes,
        net: nets,
    }
}

#[allow(dead_code)]
fn path_exists(p: &Path) -> bool {
    p.exists()
}

/// Per-VM sparse CoW overlay path (this VM's writes only; removed on stop_vm).
fn overlay_path_for(id: Uuid) -> String {
    format!("/tmp/vmm-ov-{id}.cow")
}

fn overlay_path_for_config(id: Uuid, cfg: &VmSpawnConfig) -> Option<String> {
    if cfg.read_only && cfg.rootfs_path.is_some() {
        Some(overlay_path_for(id))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiKeyRegistry, ApiRole, AutoscaleConfig, Config, WarmPoolConfig};

    fn spawn_config(read_only: bool, rootfs_path: Option<PathBuf>) -> VmSpawnConfig {
        VmSpawnConfig {
            memory_mib: 256,
            vcpus: 1,
            kernel_path: PathBuf::from("/kernel"),
            rootfs_path,
            cmdline: DEFAULT_CMDLINE.to_string(),
            read_only,
        }
    }

    #[test]
    fn shutdown_prevents_warm_refill_before_vmm_spawn() {
        let root = PathBuf::from(format!(
            "target/taritd-supervisor-shutdown-{}",
            Uuid::new_v4()
        ));
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            api_keys: ApiKeyRegistry::from_plaintext_entries(vec![(
                "test-key".into(),
                "tenant-a".into(),
                ApiRole::Admin,
                0,
            )])
            .unwrap(),
            host_id: "test-host".into(),
            vmm_bin: root.join("vmm-must-not-run"),
            kernel: root.join("kernel"),
            rootfs: root.join("rootfs"),
            socket_dir: root.join("sockets"),
            db_path: root.join("fleet.db"),
            net_state_path: root.join("net-state.json"),
            images_dir: root.join("images"),
            max_vms: 4,
            max_vcpus: 4,
            max_memory_mib: 1024,
            peer_secret: "peer-secret".into(),
            database_url: None,
            rpc_addr: "http://127.0.0.1:0".into(),
            enable_net: false,
            rootfs_read_only: false,
            metrics_expose_tenant_labels: false,
            vm_cgroup_parent: None,
            vm_cgroup_pids_max: 1024,
            warm_pool: WarmPoolConfig::default(),
            admission_timeout_ms: 1,
            reap_on_shutdown: true,
            region: "local".into(),
            zone: "local".into(),
            cloud: "onprem".into(),
            autoscale: AutoscaleConfig::default(),
            ssh_gateway_enabled: false,
            ssh_gateway_addr: "127.0.0.1:0".parse().unwrap(),
            ssh_gateway_host_key_path: root.join("ssh_host"),
            share_listen: None,
            share_domain: None,
            share_token_key: None,
            share_token_ttl_secs: 300,
            share_connect_timeout_ms: 1_000,
            share_idle_timeout_secs: 1,
        };
        let class = config.warm_pool.classes[0].clone();
        let supervisor = VmmSupervisor::new(config.clone());
        supervisor.begin_shutdown();

        let error = supervisor
            .spawn_warm_cancellable(&class, &AtomicBool::new(false))
            .unwrap_err();

        assert!(matches!(error, OrchError::Overloaded { .. }));
        assert!(
            std::fs::read_dir(&config.socket_dir)
                .unwrap()
                .next()
                .is_none(),
            "shutdown must reject refill before it creates a VMM socket"
        );
        drop(supervisor);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overlay_path_for_config_uses_vm_id_when_rootfs_is_read_only() {
        let id = Uuid::parse_str("018f9f4d-07f5-7cc6-a1fd-111111111111").unwrap();
        let cfg = spawn_config(true, Some(PathBuf::from("/rootfs.ext4")));
        let expected = format!("/tmp/vmm-ov-{id}.cow");

        assert_eq!(overlay_path_for_config(id, &cfg), Some(expected.clone()));

        let vmm_config = build_vmm_config(id, &cfg, None);
        assert_eq!(vmm_config.volumes.len(), 1);
        assert_eq!(vmm_config.volumes[0].overlay, Some(expected));
    }

    #[test]
    fn overlay_path_for_config_is_absent_without_read_only_rootfs() {
        let id = Uuid::parse_str("018f9f4d-07f5-7cc6-a1fd-222222222222").unwrap();

        assert_eq!(
            overlay_path_for_config(
                id,
                &spawn_config(false, Some(PathBuf::from("/rootfs.ext4")))
            ),
            None
        );
        assert_eq!(overlay_path_for_config(id, &spawn_config(true, None)), None);
    }

    #[test]
    fn network_lease_defers_teardown_until_final_release() {
        let alloc = NetAlloc {
            idx: 7,
            vm_id: Uuid::nil(),
            tap: "insta7".into(),
            host_ip: "172.16.0.29".into(),
            guest_ip: "172.16.0.30".into(),
            prefix: 30,
        };
        let mut state = NetworkLeaseState::default();
        state.acquire();

        assert_eq!(state.defer_teardown(alloc.clone()), None);
        assert_eq!(state.release(), Some(alloc));
        assert!(state.teardown_in_progress());
        state.complete_teardown();
        assert!(!state.teardown_in_progress());
    }

    #[test]
    fn stop_state_rejects_duplicate_teardown_until_completion() {
        let id = Uuid::nil();
        let mut state = StopState::default();

        assert!(state.begin(id));
        assert!(!state.begin(id));
        state.complete(id);
        assert!(state.begin(id));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_cgroup_v2_self_path() {
        assert_eq!(
            parse_self_cgroup("0::/user.slice/taritd.service\n"),
            Some(PathBuf::from("/sys/fs/cgroup/user.slice/taritd.service"))
        );
        assert_eq!(
            parse_self_cgroup("0::/\n"),
            Some(PathBuf::from("/sys/fs/cgroup"))
        );
    }
}
