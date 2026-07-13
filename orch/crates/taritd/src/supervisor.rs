use crate::config::{Config, WarmClass};
use crate::net::{NetAlloc, NetProvisioner};
use std::collections::{HashMap, VecDeque};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};
use std::time::{Duration, Instant};
use tarit_types::OrchError;
use tarit_vmm_client::{
    KernelConfig, MemoryConfig, NetConfig, VcpuConfig, VmConfig, VmmClient, VolumeConfig,
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

    fn kill_wait(&self) -> Result<(), OrchError> {
        let mut child = self
            .child
            .lock()
            .map_err(|_| OrchError::Internal("VMM child lock poisoned".into()))?;
        if child
            .try_wait()
            .map_err(|error| OrchError::Internal(format!("check VMM exit: {error}")))?
            .is_some()
        {
            return Ok(());
        }
        if let Err(error) = child.kill() {
            if child
                .try_wait()
                .map_err(|check| OrchError::Internal(format!("check VMM exit: {check}")))?
                .is_none()
            {
                return Err(OrchError::Internal(format!("kill VMM: {error}")));
            }
            return Ok(());
        }
        child
            .wait()
            .map(|_| ())
            .map_err(|error| OrchError::Internal(format!("wait for VMM exit: {error}")))
    }
}

#[derive(Debug)]
struct BootControl {
    cancelled: AtomicBool,
    cancellation: (Mutex<bool>, Condvar),
    completion: (Mutex<Option<Result<(), String>>>, Condvar),
}

impl BootControl {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            cancellation: (Mutex::new(false), Condvar::new()),
            completion: (Mutex::new(None), Condvar::new()),
        }
    }

    fn request_cancellation(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut cancelled) = self.cancellation.0.lock() {
            *cancelled = true;
            self.cancellation.1.notify_all();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn wait_for_cancellation(&self) {
        let mut cancelled = self.cancellation.0.lock().unwrap();
        while !*cancelled {
            cancelled = self.cancellation.1.wait(cancelled).unwrap();
        }
    }

    fn complete(&self, result: Result<(), OrchError>) {
        let completion = result.map_err(|error| error.to_string());
        if let Ok(mut completed) = self.completion.0.lock() {
            if completed.is_none() {
                *completed = Some(completion);
                self.completion.1.notify_all();
            }
        }
    }

    fn wait_for_completion(&self) -> Result<(), OrchError> {
        let mut completed = self
            .completion
            .0
            .lock()
            .map_err(|_| OrchError::Internal("boot completion lock poisoned".into()))?;
        while completed.is_none() {
            completed = self
                .completion
                .1
                .wait(completed)
                .map_err(|_| OrchError::Internal("boot completion lock poisoned".into()))?;
        }
        match completed.as_ref().expect("completion checked") {
            Ok(()) => Ok(()),
            Err(error) => Err(OrchError::Internal(error.clone())),
        }
    }
}

#[derive(Debug, Clone)]
struct BootingVm {
    socket_path: PathBuf,
    process: ManagedProcess,
    control: Arc<BootControl>,
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
    /// Serializes VMM spawn registration with shutdown's boot cancellation sweep.
    boot_gate: Mutex<()>,
    /// Pre-booted, unassigned VMs kept ready by the warm-pool replenisher.
    warm: Mutex<VecDeque<WarmVm>>,
    net: Option<NetProvisioner>,
    shutting_down: AtomicBool,
}

#[derive(Debug, Default, Clone)]
pub struct ShutdownSummary {
    pub running_ids: Vec<Uuid>,
    pub booting_ids: Vec<Uuid>,
    pub running: usize,
    pub warm: usize,
    pub booting: usize,
}

impl ShutdownSummary {
    pub fn total(&self) -> usize {
        self.running + self.warm + self.booting
    }
}

#[derive(Debug)]
pub(crate) struct ShutdownFailure {
    pub(crate) summary: ShutdownSummary,
    pub(crate) error: OrchError,
}

impl From<OrchError> for ShutdownFailure {
    fn from(error: OrchError) -> Self {
        Self {
            summary: ShutdownSummary::default(),
            error,
        }
    }
}

#[derive(Default)]
struct ShutdownTransitions {
    summary: ShutdownSummary,
    failures: Vec<String>,
}

impl ShutdownTransitions {
    fn running(&mut self, id: Uuid, result: Result<(), OrchError>) -> bool {
        match result {
            Ok(()) => {
                self.summary.running_ids.push(id);
                self.summary.running += 1;
                true
            }
            Err(error) => {
                self.failures.push(format!(
                    "VM {id} teardown retained allocation for retry: {error}"
                ));
                false
            }
        }
    }

    fn warm(&mut self, id: Uuid, result: Result<(), OrchError>) -> bool {
        match result {
            Ok(()) => {
                self.summary.warm += 1;
                true
            }
            Err(error) => {
                self.failures.push(format!(
                    "warm VM {id} teardown retained allocation for retry: {error}"
                ));
                false
            }
        }
    }

    fn booting(&mut self, id: Uuid, result: Result<(), OrchError>) {
        match result {
            Ok(()) => {
                self.summary.booting_ids.push(id);
                self.summary.booting += 1;
            }
            Err(error) => self.failures.push(format!(
                "booting VM {id} cleanup retained allocation for retry: {error}"
            )),
        }
    }

    fn record_internal_failure(&mut self, error: OrchError) {
        self.failures.push(error.to_string());
    }

    fn finish(self) -> Result<ShutdownSummary, ShutdownFailure> {
        if self.failures.is_empty() {
            Ok(self.summary)
        } else {
            Err(ShutdownFailure {
                summary: self.summary,
                error: OrchError::Internal(self.failures.join("; ")),
            })
        }
    }
}

impl VmmSupervisor {
    #[cfg(test)]
    pub fn new(config: Config) -> Self {
        Self::new_with_live_vms(config, std::iter::empty(), &[])
            .expect("test supervisor networking setup must succeed")
    }

    pub fn new_with_live_vms(
        config: Config,
        live_vm_ids: impl IntoIterator<Item = Uuid>,
        preflight_taps: &[String],
    ) -> Result<Self, OrchError> {
        std::fs::create_dir_all(&config.socket_dir).ok();
        let live_vm_ids = live_vm_ids.into_iter().collect::<Vec<_>>();
        validate_network_startup_mode(config.enable_net, preflight_taps, live_vm_ids.len())?;
        let net = if config.enable_net {
            let provisioner = NetProvisioner::new(config.net_state_path.clone(), live_vm_ids)?;
            tracing::info!(uplink = provisioner.uplink(), "per-VM networking enabled");
            Some(provisioner)
        } else {
            None
        };
        Ok(Self {
            config,
            running: Mutex::new(HashMap::new()),
            booting: Mutex::new(HashMap::new()),
            boot_gate: Mutex::new(()),
            warm: Mutex::new(VecDeque::new()),
            net,
            shutting_down: AtomicBool::new(false),
        })
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

    pub(crate) fn is_shutting_down(&self) -> bool {
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
    ) -> Result<Arc<BootControl>, OrchError> {
        let control = Arc::new(BootControl::new());
        let mut booting = self
            .booting
            .lock()
            .map_err(|_| OrchError::Internal("supervisor booting lock poisoned".into()))?;
        booting.insert(
            id,
            BootingVm {
                socket_path,
                process,
                control: Arc::clone(&control),
            },
        );
        Ok(control)
    }

    fn complete_booting(
        &self,
        id: Uuid,
        control: &Arc<BootControl>,
        result: Result<(), OrchError>,
    ) {
        if result.is_ok() {
            if let Ok(mut booting) = self.booting.lock() {
                if booting
                    .get(&id)
                    .is_some_and(|booting_vm| Arc::ptr_eq(&booting_vm.control, control))
                {
                    booting.remove(&id);
                }
            }
        }
        control.complete(result);
    }

    fn cleanup_boot_failure(
        &self,
        id: Uuid,
        control: &Arc<BootControl>,
        vm: &RunningVm,
        cause: OrchError,
    ) -> OrchError {
        let mut cleanup_failures = self
            .teardown_vm(id, vm)
            .err()
            .map(|error| vec![error.to_string()])
            .unwrap_or_default();
        if vm.net.is_none() {
            if let Some(net) = &self.net {
                if let Err(error) = net.teardown_vm_id(id) {
                    cleanup_failures.push(format!(
                        "teardown partially provisioned network allocation: {error}"
                    ));
                }
            }
        }
        if cleanup_failures.is_empty() {
            self.complete_booting(id, control, Ok(()));
            cause
        } else {
            let cleanup = cleanup_failures.join("; ");
            let error = OrchError::Internal(format!(
                "{cause}; shutdown cleanup retained booting VM {id} for retry: {cleanup}"
            ));
            self.complete_booting(
                id,
                control,
                Err(OrchError::Internal(format!(
                    "boot cleanup retained resources for retry: {cleanup}"
                ))),
            );
            error
        }
    }

    fn publish_running(
        &self,
        id: Uuid,
        vm: RunningVm,
        control: Arc<BootControl>,
    ) -> Result<(u32, PathBuf), OrchError> {
        let pid = vm.pid;
        let socket_path = vm.socket_path.clone();
        let mut running = match self.running.lock() {
            Ok(running) => running,
            Err(_) => {
                return Err(self.cleanup_boot_failure(
                    id,
                    &control,
                    &vm,
                    OrchError::Internal("supervisor lock poisoned".into()),
                ))
            }
        };
        let mut booting = match self.booting.lock() {
            Ok(booting) => booting,
            Err(_) => {
                drop(running);
                return Err(self.cleanup_boot_failure(
                    id,
                    &control,
                    &vm,
                    OrchError::Internal("supervisor booting lock poisoned".into()),
                ));
            }
        };
        if !boot_can_publish(&control, self.is_shutting_down())
            || !booting
                .get(&id)
                .is_some_and(|booting_vm| Arc::ptr_eq(&booting_vm.control, &control))
        {
            drop(booting);
            drop(running);
            return Err(self.cleanup_boot_failure(id, &control, &vm, self.shutdown_error()));
        }
        booting.remove(&id);
        running.insert(id, vm);
        control.complete(Ok(()));
        Ok((pid, socket_path))
    }

    fn publish_warm(
        &self,
        id: Uuid,
        vm: RunningVm,
        spec: VmSpawnConfig,
        control: Arc<BootControl>,
    ) -> Result<(), OrchError> {
        let mut warm = match self.warm.lock() {
            Ok(warm) => warm,
            Err(_) => {
                return Err(self.cleanup_boot_failure(
                    id,
                    &control,
                    &vm,
                    OrchError::Internal("warm lock poisoned".into()),
                ))
            }
        };
        let mut booting = match self.booting.lock() {
            Ok(booting) => booting,
            Err(_) => {
                drop(warm);
                return Err(self.cleanup_boot_failure(
                    id,
                    &control,
                    &vm,
                    OrchError::Internal("supervisor booting lock poisoned".into()),
                ));
            }
        };
        if !boot_can_publish(&control, self.is_shutting_down())
            || !booting
                .get(&id)
                .is_some_and(|booting_vm| Arc::ptr_eq(&booting_vm.control, &control))
        {
            drop(booting);
            drop(warm);
            return Err(self.cleanup_boot_failure(id, &control, &vm, self.shutdown_error()));
        }
        booting.remove(&id);
        warm.push_back(WarmVm { id, vm, spec });
        control.complete(Ok(()));
        Ok(())
    }

    fn finish_booted_vm(
        &self,
        id: Uuid,
        control: Arc<BootControl>,
        vm: &RunningVm,
    ) -> Result<(), OrchError> {
        match self.teardown_vm(id, vm) {
            Ok(()) => {
                self.complete_booting(id, &control, Ok(()));
                Ok(())
            }
            Err(error) => {
                self.complete_booting(
                    id,
                    &control,
                    Err(OrchError::Internal(format!(
                        "boot cleanup retained resources for retry: {error}"
                    ))),
                );
                Err(error)
            }
        }
    }

    fn wait_for_socket_or_cancellation(
        &self,
        socket_path: &Path,
        control: &BootControl,
    ) -> Result<(), OrchError> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            if control.is_cancelled() || self.is_shutting_down() {
                return Err(self.shutdown_error());
            }
            if socket_path.exists() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(OrchError::Vmm(format!(
            "wait for socket: timed out waiting for {}",
            socket_path.display()
        )))
    }

    fn signal_booting_tasks(&self) -> Result<Vec<(Uuid, BootingVm)>, OrchError> {
        let booting = self
            .booting
            .lock()
            .map_err(|_| OrchError::Internal("supervisor booting lock poisoned".into()))?
            .iter()
            .map(|(id, booting_vm)| (*id, booting_vm.clone()))
            .collect::<Vec<_>>();
        for (_, booting_vm) in &booting {
            booting_vm.control.request_cancellation();
        }
        Ok(booting)
    }

    fn complete_cancelled_booting_tasks(
        &self,
        booting: Vec<(Uuid, BootingVm)>,
    ) -> Vec<(Uuid, Result<(), OrchError>)> {
        let outcomes = wait_for_booting_tasks(
            booting
                .iter()
                .map(|(_, booting_vm)| Arc::clone(&booting_vm.control)),
        );
        booting
            .into_iter()
            .zip(outcomes)
            .map(|((id, booting_vm), outcome)| {
                let outcome = match outcome {
                    Ok(()) => Ok(()),
                    Err(completion_error) => {
                        self.retry_booting_cleanup(id, &booting_vm)
                            .map_err(|retry_error| {
                                OrchError::Internal(format!(
                                "{completion_error}; retrying boot cleanup failed: {retry_error}"
                            ))
                            })
                    }
                };
                if outcome.is_ok() {
                    self.complete_booting(id, &booting_vm.control, Ok(()));
                }
                (id, outcome)
            })
            .collect()
    }

    fn retry_booting_cleanup(&self, id: Uuid, booting_vm: &BootingVm) -> Result<(), OrchError> {
        let vm = RunningVm {
            pid: booting_vm.process.pid,
            socket_path: booting_vm.socket_path.clone(),
            process: booting_vm.process.clone(),
            net: None,
        };
        let mut failures = self
            .teardown_vm(id, &vm)
            .err()
            .map(|error| vec![error.to_string()])
            .unwrap_or_default();
        if let Some(net) = &self.net {
            if let Err(error) = net.teardown_vm_id(id) {
                failures.push(format!("teardown retained network allocation: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(OrchError::Internal(failures.join("; ")))
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
    ) -> Result<(RunningVm, Arc<BootControl>), OrchError> {
        self.ensure_accepting_work()?;
        let socket_path = self.socket_path_for(id);
        let _ = std::fs::remove_file(&socket_path);

        let cgroup_args = self.cgroup_args(id, Some(vm_config.memory_mib));
        let boot_gate = self
            .boot_gate
            .lock()
            .map_err(|_| OrchError::Internal("supervisor boot gate lock poisoned".into()))?;
        if self.is_shutting_down() {
            return Err(self.shutdown_error());
        }
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
        let base_vm = RunningVm {
            pid,
            socket_path: socket_path.clone(),
            process: process.clone(),
            net: None,
        };
        let control = match self.track_booting(id, socket_path.clone(), process.clone()) {
            Ok(control) => control,
            Err(error) => {
                drop(boot_gate);
                return Err(match self.teardown_vm(id, &base_vm) {
                    Ok(()) => error,
                    Err(cleanup) => OrchError::Internal(format!(
                        "{error}; boot registration cleanup retained resources for retry: {cleanup}"
                    )),
                });
            }
        };
        drop(boot_gate);

        if let Err(error) = self.wait_for_socket_or_cancellation(&socket_path, &control) {
            return Err(self.cleanup_boot_failure(id, &control, &base_vm, error));
        }
        if !boot_can_publish(&control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(id, &control, &base_vm, self.shutdown_error()));
        }

        // Provision per-VM host networking (tap + /30 + NAT) if enabled. The
        // guest auto-configures eth0 from the kernel `ip=` cmdline we append.
        let net_alloc = match &self.net {
            Some(p) => match p.provision(id) {
                Ok(a) => Some(a),
                Err(error) => {
                    let cause = match error {
                        error @ OrchError::Overloaded { .. } => error,
                        error => OrchError::Internal(format!("net provision: {error}")),
                    };
                    return Err(self.cleanup_boot_failure(id, &control, &base_vm, cause));
                }
            },
            None => None,
        };
        let vm = RunningVm {
            pid,
            socket_path,
            process,
            net: net_alloc,
        };
        if !boot_can_publish(&control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(id, &control, &vm, self.shutdown_error()));
        }

        let vmm_config = build_vmm_config(id, vm_config, vm.net.as_ref());
        let client = VmmClient::new(&vm.socket_path);
        if let Err(e) = client.create(vmm_config) {
            return Err(self.cleanup_boot_failure(
                id,
                &control,
                &vm,
                OrchError::Vmm(format!("create vm: {e}")),
            ));
        }
        if !boot_can_publish(&control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(id, &control, &vm, self.shutdown_error()));
        }

        Ok((vm, control))
    }

    pub fn spawn_vm(
        &self,
        id: Uuid,
        vm_config: VmSpawnConfig,
    ) -> Result<(u32, PathBuf), OrchError> {
        let (vm, control) = self.boot_vm(id, &vm_config, SpawnPurpose::Live)?;
        self.publish_running(id, vm, control)
    }

    /// Restore a VM from a node-local snapshot file: spawn a fresh `vmm serve`,
    /// send Restore, and register the resumed VM. The snapshot carries the
    /// guest's device/net config, so we do not re-provision host networking
    /// here (restore is used for the fast warm/resume path).
    pub fn restore_vm(&self, id: Uuid, snapshot_path: &str) -> Result<(u32, PathBuf), OrchError> {
        let (vm, control) = self.spawn_and_restore(id, snapshot_path, None, SpawnPurpose::Live)?;
        self.publish_running(id, vm, control)
    }

    fn spawn_and_restore(
        &self,
        id: Uuid,
        snapshot_path: &str,
        overlay: Option<String>,
        purpose: SpawnPurpose,
    ) -> Result<(RunningVm, Arc<BootControl>), OrchError> {
        self.ensure_accepting_work()?;
        let socket_path = self.socket_path_for(id);
        let _ = std::fs::remove_file(&socket_path);

        let cgroup_args = self.cgroup_args(id, None);
        let boot_gate = self
            .boot_gate
            .lock()
            .map_err(|_| OrchError::Internal("supervisor boot gate lock poisoned".into()))?;
        if self.is_shutting_down() {
            return Err(self.shutdown_error());
        }
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
        let vm = RunningVm {
            pid,
            socket_path: socket_path.clone(),
            process,
            net: None,
        };
        let control = match self.track_booting(id, socket_path.clone(), vm.process.clone()) {
            Ok(control) => control,
            Err(error) => {
                drop(boot_gate);
                return Err(match self.teardown_vm(id, &vm) {
                    Ok(()) => error,
                    Err(cleanup) => OrchError::Internal(format!(
                        "{error}; boot registration cleanup retained resources for retry: {cleanup}"
                    )),
                });
            }
        };
        drop(boot_gate);

        if let Err(error) = self.wait_for_socket_or_cancellation(&socket_path, &control) {
            return Err(self.cleanup_boot_failure(id, &control, &vm, error));
        }
        if !boot_can_publish(&control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(id, &control, &vm, self.shutdown_error()));
        }

        let client = VmmClient::new(&socket_path);
        if let Err(e) = client.restore(snapshot_path, overlay.clone()) {
            return Err(self.cleanup_boot_failure(
                id,
                &control,
                &vm,
                OrchError::Vmm(format!("restore vm: {e}")),
            ));
        }
        if !boot_can_publish(&control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(id, &control, &vm, self.shutdown_error()));
        }
        Ok((vm, control))
    }

    /// Boot one warm-pool VM of `class` and park it in the warm queue. The boot
    /// happens without the warm lock held; only the final enqueue takes it.
    /// Block until the guest agent can actually run a command, so we never park a
    /// still-booting VM. A freshly-parked, not-yet-ready VM handed out during a
    /// burst blocks the caller for seconds on its first agent dial (the burst
    /// p95 tail). Bounded; parks anyway on timeout so a wedged guest can't stall
    /// replenishment forever.
    fn await_ready(&self, socket: &Path) {
        let client = VmmClient::new(socket);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if client
                .exec("true", 1000)
                .map(|(code, _, _, _)| code == 0)
                .unwrap_or(false)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn spawn_warm(&self, class: &WarmClass) -> Result<(), OrchError> {
        let id = Uuid::new_v4();
        let spec = VmSpawnConfig::from_warm_class(&self.config, class);
        let (vm, control) = self.boot_vm(id, &spec, SpawnPurpose::Refill)?;
        self.await_ready(&vm.socket_path);
        self.publish_warm(id, vm, spec, control)
    }

    /// Cold-boot one VM for `class`, wait until it is ready, take a full golden
    /// snapshot, then tear down the builder VM. Runtime warm capacity is filled
    /// by restoring clones from the returned snapshot.
    pub fn create_golden(&self, class: &WarmClass) -> Result<String, OrchError> {
        let id = Uuid::new_v4();
        let spec = VmSpawnConfig::from_warm_class(&self.config, class);
        let (vm, control) = self.boot_vm(id, &spec, SpawnPurpose::Refill)?;
        self.await_ready(&vm.socket_path);
        if !boot_can_publish(&control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(id, &control, &vm, self.shutdown_error()));
        }
        let client = VmmClient::new(&vm.socket_path);
        let snapshot_path = match client.snapshot(false) {
            Ok(path) => path,
            Err(e) => {
                return Err(self.cleanup_boot_failure(
                    id,
                    &control,
                    &vm,
                    OrchError::Vmm(format!("snapshot golden: {e}")),
                ));
            }
        };

        self.finish_booted_vm(id, control, &vm)?;
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
        let (vm, control) =
            self.spawn_and_restore(id, snapshot_path, overlay, SpawnPurpose::Refill)?;
        self.await_ready(&vm.socket_path);
        self.publish_warm(id, vm, spec, control)
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
            if let Err(error) = self.teardown_vm(taken.id, &taken.vm) {
                if let Ok(mut warm) = self.warm.lock() {
                    warm.push_back(taken);
                }
                tracing::error!(%error, "warm VM teardown failed during shutdown; retained for retry");
            }
            return None;
        }
        let mut running = self.running.lock().ok()?;
        if self.is_shutting_down() {
            drop(running);
            if let Err(error) = self.teardown_vm(taken.id, &taken.vm) {
                if let Ok(mut warm) = self.warm.lock() {
                    warm.push_back(taken);
                }
                tracing::error!(%error, "warm VM teardown failed during shutdown; retained for retry");
            }
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
                net.teardown_vm_id(id)?;
            }
            return Ok(());
        };

        let client = VmmClient::new(&running.socket_path);
        let _ = client.stop();
        if let Err(error) = self.teardown_vm(id, &running) {
            self.running
                .lock()
                .map_err(|_| {
                    OrchError::Internal(format!(
                        "VM {id} teardown failed ({error}) and supervisor could not retain it for retry"
                    ))
                })?
                .insert(id, running);
            return Err(error);
        }
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

    pub(crate) fn stop_all(&self) -> Result<ShutdownSummary, ShutdownFailure> {
        self.shutting_down.store(true, Ordering::SeqCst);
        let booting = {
            let _gate = self
                .boot_gate
                .lock()
                .map_err(|_| OrchError::Internal("supervisor boot gate lock poisoned".into()))?;
            self.signal_booting_tasks().map_err(ShutdownFailure::from)?
        };
        let booting = self.complete_cancelled_booting_tasks(booting);
        let (running, warm) = {
            let mut running = self
                .running
                .lock()
                .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?;
            let mut warm = self
                .warm
                .lock()
                .map_err(|_| OrchError::Internal("warm lock poisoned".into()))?;
            (
                running.drain().collect::<Vec<_>>(),
                warm.drain(..).collect::<Vec<_>>(),
            )
        };
        let mut transitions = ShutdownTransitions::default();
        let mut retained_running = Vec::new();
        for (id, vm) in running {
            let client = VmmClient::new(&vm.socket_path);
            let _ = client.stop();
            if !transitions.running(id, self.teardown_vm(id, &vm)) {
                retained_running.push((id, vm));
            }
        }
        let mut retained_warm = Vec::new();
        for warm_vm in warm {
            let client = VmmClient::new(&warm_vm.vm.socket_path);
            let _ = client.stop();
            if !transitions.warm(warm_vm.id, self.teardown_vm(warm_vm.id, &warm_vm.vm)) {
                retained_warm.push(warm_vm);
            }
        }
        for (id, result) in booting {
            transitions.booting(id, result);
        }

        if !retained_running.is_empty() {
            match self.running.lock() {
                Ok(mut running) => running.extend(retained_running),
                Err(_) => transitions.record_internal_failure(OrchError::Internal(
                    "supervisor lock poisoned while retaining failed teardown".into(),
                )),
            }
        }
        if !retained_warm.is_empty() {
            match self.warm.lock() {
                Ok(mut warm) => warm.extend(retained_warm),
                Err(_) => transitions.record_internal_failure(OrchError::Internal(
                    "warm lock poisoned while retaining failed teardown".into(),
                )),
            }
        }
        transitions.finish()
    }

    fn teardown_vm(&self, id: Uuid, vm: &RunningVm) -> Result<(), OrchError> {
        let mut failures = Vec::new();
        if let Err(error) = vm.process.kill_wait() {
            failures.push(error.to_string());
        }
        if let Err(error) = remove_file_if_present(&vm.socket_path) {
            failures.push(format!("remove VMM socket: {error}"));
        }
        if let Err(error) = remove_file_if_present(Path::new(&overlay_path_for(id))) {
            failures.push(format!("remove VMM overlay: {error}"));
        }
        if let (Some(p), Some(a)) = (&self.net, &vm.net) {
            if let Err(error) = p.teardown(a) {
                failures.push(format!("teardown network allocation: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(OrchError::Internal(failures.join("; ")))
        }
    }
}

fn wait_for_booting_tasks(
    controls: impl IntoIterator<Item = Arc<BootControl>>,
) -> Vec<Result<(), OrchError>> {
    controls
        .into_iter()
        .map(|control| control.wait_for_completion())
        .collect()
}

fn boot_can_publish(control: &BootControl, shutting_down: bool) -> bool {
    !shutting_down && !control.is_cancelled()
}

fn remove_file_if_present(path: &Path) -> Result<(), std::io::Error> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
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

fn validate_network_startup_mode(
    enable_net: bool,
    preflight_taps: &[String],
    recovered_live_vm_count: usize,
) -> Result<(), OrchError> {
    if !enable_net && (!preflight_taps.is_empty() || recovered_live_vm_count > 0) {
        return Err(OrchError::Internal(
            "network-disabled startup refused: contained Tarit TAPs or recovered live VMs require TARIT_ENABLE_NET=1"
                .into(),
        ));
    }
    Ok(())
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
    use crate::config::{ApiKeyRegistry, ApiRole, AutoscaleConfig, WarmPoolConfig};
    use std::sync::mpsc;
    use std::thread;

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

    fn test_supervisor() -> Arc<VmmSupervisor> {
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            api_keys: ApiKeyRegistry::from_plaintext_entries(vec![(
                "test-key".into(),
                "test".into(),
                ApiRole::Admin,
                0,
            )])
            .unwrap(),
            host_id: "test-host".into(),
            vmm_bin: PathBuf::from("true"),
            kernel: PathBuf::from("kernel"),
            rootfs: PathBuf::from("rootfs"),
            socket_dir: PathBuf::from("target/taritd-supervisor-test/sockets"),
            db_path: PathBuf::from("target/taritd-supervisor-test/fleet.db"),
            net_state_path: PathBuf::from("target/taritd-supervisor-test/net-state.json"),
            images_dir: PathBuf::from("target/taritd-supervisor-test/images"),
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
            ssh_gateway_host_key_path: PathBuf::from("target/taritd-supervisor-test/ssh_host"),
        };
        Arc::new(VmmSupervisor::new(config))
    }

    #[test]
    fn network_disabled_startup_rejects_contained_taps_or_live_recovery() {
        assert!(validate_network_startup_mode(false, &[], 0).is_ok());
        assert!(validate_network_startup_mode(false, &["insta7".into()], 0).is_err());
        assert!(validate_network_startup_mode(false, &[], 1).is_err());
        assert!(validate_network_startup_mode(true, &["insta7".into()], 1).is_ok());
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
    fn stop_all_commits_successful_transitions_before_returning_mixed_failure() {
        let stopped_id = Uuid::new_v4();
        let retained_id = Uuid::new_v4();
        let mut transitions = ShutdownTransitions::default();

        assert!(transitions.running(stopped_id, Ok(())));
        assert!(!transitions.running(
            retained_id,
            Err(OrchError::Internal(
                "simulated retained network allocation".into()
            ))
        ));
        assert!(transitions.warm(Uuid::new_v4(), Ok(())));
        transitions.booting(Uuid::new_v4(), Ok(()));

        let failure = transitions
            .finish()
            .expect_err("a retained VM must make stop_all fail after successes commit");
        assert_eq!(failure.summary.running_ids, vec![stopped_id]);
        assert_eq!(failure.summary.running, 1);
        assert_eq!(failure.summary.warm, 1);
        assert_eq!(failure.summary.booting, 1);
        assert!(failure.error.to_string().contains(&retained_id.to_string()));
    }

    #[test]
    fn stop_all_waits_for_cancelled_provisioning_cleanup_before_transitioning_booting_vm() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let process = ManagedProcess::new(Command::new("true").spawn().unwrap());
        let control = supervisor
            .track_booting(id, PathBuf::from("booting.sock"), process.clone())
            .unwrap();
        let task_control = Arc::clone(&control);
        let (cleanup_started_tx, cleanup_started_rx) = mpsc::channel();
        let (allow_cleanup_tx, allow_cleanup_rx) = mpsc::channel();
        let task = thread::spawn(move || {
            task_control.wait_for_cancellation();
            cleanup_started_tx.send(()).unwrap();
            allow_cleanup_rx.recv().unwrap();
            process.kill_wait().unwrap();
            task_control.complete(Ok(()));
        });

        let (stop_done_tx, stop_done_rx) = mpsc::channel();
        let stop_supervisor = Arc::clone(&supervisor);
        let stopper = thread::spawn(move || {
            stop_done_tx.send(stop_supervisor.stop_all()).unwrap();
        });

        cleanup_started_rx.recv().unwrap();
        assert!(stop_done_rx.try_recv().is_err());

        allow_cleanup_tx.send(()).unwrap();
        let summary = stop_done_rx.recv().unwrap().unwrap();
        assert_eq!(summary.booting_ids, vec![id]);
        assert_eq!(summary.booting, 1);
        stopper.join().unwrap();
        task.join().unwrap();
    }

    #[test]
    fn cancelled_boot_cannot_publish_running_state() {
        let control = BootControl::new();
        control.request_cancellation();

        assert!(!boot_can_publish(&control, false));
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
