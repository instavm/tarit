use crate::config::{Config, WarmClass};
use crate::net::{NetAlloc, NetProvisioner};
use std::collections::{HashMap, VecDeque};
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
const GUEST_READY_TIMEOUT: Duration = Duration::from_secs(20);
const WARM_HANDOFF_READY_TIMEOUT: Duration = Duration::from_millis(200);
const GUEST_READY_EXEC_TIMEOUT: Duration = Duration::from_secs(1);
const GUEST_READY_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadinessCheck {
    Boot,
    WarmHandoff,
}

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
    booting: Mutex<HashMap<Uuid, BootingVm>>,
    /// Pre-booted, unassigned VMs kept ready by the warm-pool replenisher.
    warm: Mutex<VecDeque<WarmVm>>,
    golden_artifacts: Mutex<Vec<PathBuf>>,
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
            booting: Mutex::new(HashMap::new()),
            warm: Mutex::new(VecDeque::new()),
            golden_artifacts: Mutex::new(Vec::new()),
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
    ) -> Result<RunningVm, OrchError> {
        self.ensure_accepting_work()?;
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

        if let Err(e) = wait_for_socket(&socket_path, Duration::from_secs(30)) {
            self.untrack_booting(id);
            process.kill_wait();
            return Err(OrchError::Vmm(format!("wait for socket: {e}")));
        }
        if self.is_shutting_down() {
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
        if self.is_shutting_down() {
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
        if self.is_shutting_down() {
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

    pub fn spawn_vm(
        &self,
        id: Uuid,
        vm_config: VmSpawnConfig,
    ) -> Result<(u32, PathBuf), OrchError> {
        let vm = self.boot_vm(id, &vm_config, SpawnPurpose::Live)?;
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
        let purpose = SpawnPurpose::Live;
        let vm = self.spawn_and_restore(id, snapshot_path, None, purpose)?;
        if let Err(e) = self.await_restore_ready(&vm.socket_path, purpose) {
            self.teardown_vm(id, vm);
            return Err(e);
        }
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
    ) -> Result<RunningVm, OrchError> {
        self.ensure_accepting_work()?;
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

        if let Err(e) = wait_for_socket(&socket_path, Duration::from_secs(30)) {
            self.untrack_booting(id);
            process.kill_wait();
            return Err(OrchError::Vmm(format!("wait for socket: {e}")));
        }
        if self.is_shutting_down() {
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
        if self.is_shutting_down() {
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
    /// Block until the guest agent can actually run a command.
    fn await_ready(&self, socket: &Path, timeout: Duration) -> Result<(), OrchError> {
        wait_for_guest_ready(timeout, |remaining| {
            let exec_timeout_ms = readiness_exec_timeout_ms(remaining);
            let client =
                VmmClient::new(socket).with_connect_timeout(Duration::from_millis(exec_timeout_ms));
            match client.exec("true", exec_timeout_ms) {
                Ok((0, _, _, _)) => Ok(true),
                Ok((code, _, _, _)) => Err(format!("readiness command exited with status {code}")),
                Err(error) => Err(error.to_string()),
            }
        })
        .map_err(|last| {
            OrchError::Vmm(format!(
                "guest agent never became ready at {}: {last}",
                socket.display()
            ))
        })
    }

    fn await_restore_ready(&self, socket: &Path, purpose: SpawnPurpose) -> Result<(), OrchError> {
        if restore_requires_guest_readiness(purpose) {
            self.await_ready(socket, readiness_timeout(ReadinessCheck::Boot))
        } else {
            Ok(())
        }
    }

    pub fn spawn_warm(&self, class: &WarmClass) -> Result<(), OrchError> {
        let id = Uuid::new_v4();
        let spec = VmSpawnConfig::from_warm_class(&self.config, class);
        let vm = self.boot_vm(id, &spec, SpawnPurpose::Refill)?;
        if let Err(e) = self.await_ready(&vm.socket_path, readiness_timeout(ReadinessCheck::Boot)) {
            self.teardown_vm(id, vm);
            return Err(e);
        }
        if self.is_shutting_down() {
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
        if self.is_shutting_down() {
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
    pub fn create_golden(&self, class: &WarmClass) -> Result<String, OrchError> {
        let id = Uuid::new_v4();
        let spec = VmSpawnConfig::from_warm_class(&self.config, class);
        let vm = self.boot_vm(id, &spec, SpawnPurpose::Refill)?;
        if let Err(e) = self.await_ready(&vm.socket_path, readiness_timeout(ReadinessCheck::Boot)) {
            self.teardown_vm(id, vm);
            return Err(e);
        }
        if self.is_shutting_down() {
            self.teardown_vm(id, vm);
            return Err(self.shutdown_error());
        }
        let client = VmmClient::new(&vm.socket_path);
        let snapshot_path = match client.snapshot(false) {
            Ok(path) => path,
            Err(e) => {
                self.teardown_vm(id, vm);
                return Err(OrchError::Vmm(format!("snapshot golden: {e}")));
            }
        };

        if self.is_shutting_down() {
            self.teardown_vm(id, vm);
            cleanup_golden_artifacts([PathBuf::from(&snapshot_path)]);
            return Err(self.shutdown_error());
        }
        if let Err(e) = self.remember_golden_artifacts(
            &snapshot_path,
            overlay_path_for_config(id, &spec).as_deref(),
        ) {
            self.teardown_vm(id, vm);
            cleanup_golden_artifacts([PathBuf::from(&snapshot_path)]);
            return Err(e);
        }
        if self.is_shutting_down() {
            self.teardown_vm(id, vm);
            cleanup_golden_artifacts([PathBuf::from(&snapshot_path)]);
            return Err(self.shutdown_error());
        }
        self.teardown_vm_preserving_overlay(id, vm);
        Ok(snapshot_path)
    }

    /// Restore one warm-pool VM from an existing golden snapshot and park it.
    pub fn spawn_warm_restore(
        &self,
        class: &WarmClass,
        snapshot_path: &str,
    ) -> Result<(), OrchError> {
        let id = Uuid::new_v4();
        let spec = VmSpawnConfig::from_warm_class(&self.config, class);
        let overlay = overlay_path_for_config(id, &spec);
        let purpose = SpawnPurpose::Refill;
        let vm = self.spawn_and_restore(id, snapshot_path, overlay, purpose)?;
        if let Err(e) = self.await_restore_ready(&vm.socket_path, purpose) {
            self.teardown_vm(id, vm);
            return Err(e);
        }
        if self.is_shutting_down() {
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
        if self.is_shutting_down() {
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
        loop {
            if self.is_shutting_down() {
                return None;
            }
            let taken = {
                let mut warm = self.warm.lock().ok()?;
                let pos = warm.iter().position(|w| &w.spec == want)?;
                warm.remove(pos)?
            };
            if let Err(error) = self.await_ready(
                &taken.vm.socket_path,
                readiness_timeout(ReadinessCheck::WarmHandoff),
            ) {
                tracing::warn!(id = %taken.id, error = %error, "discarding unready warm VM");
                self.teardown_vm(taken.id, taken.vm);
                continue;
            }

            let pid = taken.vm.pid;
            let socket = taken.vm.socket_path.clone();
            self.move_pid_to_default_cgroup(pid);
            if self.is_shutting_down() {
                self.teardown_vm(taken.id, taken.vm);
                return None;
            }
            let mut running = match self.running.lock() {
                Ok(running) => running,
                Err(_) => {
                    self.teardown_vm(taken.id, taken.vm);
                    return None;
                }
            };
            if self.is_shutting_down() {
                drop(running);
                self.teardown_vm(taken.id, taken.vm);
                return None;
            }
            running.insert(taken.id, taken.vm);
            return Some((taken.id, pid, socket));
        }
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

    fn remember_golden_artifacts(
        &self,
        snapshot_path: &str,
        overlay_path: Option<&str>,
    ) -> Result<(), OrchError> {
        self.ensure_accepting_work()?;
        let mut artifacts = self
            .golden_artifacts
            .lock()
            .map_err(|_| OrchError::Internal("golden artifact lock poisoned".into()))?;
        self.ensure_accepting_work()?;
        artifacts.push(PathBuf::from(snapshot_path));
        if let Some(overlay_path) = overlay_path {
            artifacts.push(PathBuf::from(overlay_path));
        }
        Ok(())
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
            let mut guard = self
                .running
                .lock()
                .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?;
            guard.drain().collect::<Vec<_>>()
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
        let golden_artifacts = {
            let mut guard = self
                .golden_artifacts
                .lock()
                .map_err(|_| OrchError::Internal("golden artifact lock poisoned".into()))?;
            guard.drain(..).collect::<Vec<_>>()
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
        cleanup_golden_artifacts(golden_artifacts);

        Ok(summary)
    }

    fn teardown_vm(&self, id: Uuid, vm: RunningVm) {
        self.teardown_vm_inner(id, vm, true);
    }

    fn teardown_vm_preserving_overlay(&self, id: Uuid, vm: RunningVm) {
        self.teardown_vm_inner(id, vm, false);
    }

    fn teardown_vm_inner(&self, id: Uuid, vm: RunningVm, remove_overlay: bool) {
        vm.process.kill_wait();
        let _ = std::fs::remove_file(&vm.socket_path);
        if remove_overlay {
            let _ = std::fs::remove_file(overlay_path_for(id));
        }
        if let (Some(p), Some(a)) = (&self.net, &vm.net) {
            p.teardown(a);
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

fn cleanup_golden_artifacts(paths: impl IntoIterator<Item = PathBuf>) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

fn restore_requires_guest_readiness(purpose: SpawnPurpose) -> bool {
    purpose == SpawnPurpose::Refill
}

fn readiness_timeout(check: ReadinessCheck) -> Duration {
    match check {
        ReadinessCheck::Boot => GUEST_READY_TIMEOUT,
        ReadinessCheck::WarmHandoff => WARM_HANDOFF_READY_TIMEOUT,
    }
}

fn readiness_exec_timeout_ms(remaining: Duration) -> u64 {
    let timeout = remaining.min(GUEST_READY_EXEC_TIMEOUT);
    u64::try_from(timeout.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}

fn wait_for_guest_ready<F>(timeout: Duration, mut probe: F) -> Result<(), String>
where
    F: FnMut(Duration) -> Result<bool, String>,
{
    let deadline = Instant::now() + timeout;
    let mut last = "guest agent returned no successful readiness response".to_string();

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match probe(remaining) {
            Ok(true) => return Ok(()),
            Ok(false) => {
                last = "guest agent readiness command did not succeed".to_string();
            }
            Err(error) => last = error,
        }
        std::thread::sleep(GUEST_READY_POLL_INTERVAL);
    }

    Err(format!("guest agent never became ready: {last}"))
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

    #[test]
    fn guest_readiness_gate_rejects_an_unresponsive_agent() {
        let error = wait_for_guest_ready(Duration::ZERO, |_| Ok(false))
            .expect_err("an unresponsive guest must not pass the readiness gate");

        assert!(error.contains("guest agent never became ready"));
    }

    #[test]
    fn guest_readiness_gate_accepts_a_successful_probe() {
        let mut attempts = 0;

        wait_for_guest_ready(Duration::from_secs(1), |_| {
            attempts += 1;
            Ok(true)
        })
        .expect("a successful guest-agent probe must pass the readiness gate");

        assert_eq!(attempts, 1);
    }

    #[test]
    fn boot_and_warm_handoff_use_their_respective_readiness_timeouts() {
        assert_eq!(
            readiness_timeout(ReadinessCheck::Boot),
            GUEST_READY_TIMEOUT,
            "newly booted, refilled, and golden-builder VMs need the full readiness window"
        );
        assert_eq!(
            readiness_timeout(ReadinessCheck::WarmHandoff),
            Duration::from_millis(200)
        );
    }

    #[test]
    fn warm_handoff_exec_timeout_is_short_and_nonzero() {
        assert_eq!(
            readiness_exec_timeout_ms(Duration::from_secs(20)),
            1_000,
            "long boot readiness retains its existing per-exec timeout"
        );
        assert_eq!(
            readiness_exec_timeout_ms(Duration::from_millis(200)),
            200,
            "a wedged parked VM must not use the long readiness probe timeout"
        );
        assert_eq!(readiness_exec_timeout_ms(Duration::ZERO), 1);
    }

    #[test]
    fn only_warm_refill_restores_require_guest_agent_readiness() {
        assert!(!restore_requires_guest_readiness(SpawnPurpose::Live));
        assert!(restore_requires_guest_readiness(SpawnPurpose::Refill));
    }

    #[test]
    fn golden_artifact_cleanup_removes_snapshot_and_overlay() {
        let dir = std::env::temp_dir().join(format!(
            "golden-artifact-cleanup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create test directory");
        let snapshot = dir.join("golden.snap");
        let overlay = dir.join("golden.overlay");
        std::fs::write(&snapshot, b"snapshot").expect("write snapshot");
        std::fs::write(&overlay, b"overlay").expect("write overlay");

        cleanup_golden_artifacts([snapshot.clone(), overlay.clone()]);

        assert!(!snapshot.exists(), "golden snapshot must be removed");
        assert!(!overlay.exists(), "golden overlay must be removed");
        let _ = std::fs::remove_dir_all(dir);
    }
}
