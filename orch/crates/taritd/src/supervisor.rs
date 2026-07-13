use crate::config::{Config, WarmClass};
use crate::net::{NetAlloc, NetProvisioner};
use crate::scheduler::Scheduler;
use std::collections::{HashMap, HashSet, VecDeque};
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
use tokio::sync::Mutex as AsyncMutex;
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
    process: Option<ManagedProcess>,
    control: Arc<BootControl>,
    purpose: SpawnPurpose,
}

/// A pre-booted VM held in the warm pool, ready to be assigned instantly.
#[derive(Debug)]
struct WarmVm {
    id: Uuid,
    vm: RunningVm,
    spec: VmSpawnConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnPurpose {
    Live,
    Refill,
}

/// A registered boot that owns a scheduler reservation until either its cleanup
/// succeeds or its terminal lifecycle transition releases it.
pub(crate) struct BootTicket {
    id: Uuid,
    control: Arc<BootControl>,
    purpose: SpawnPurpose,
}

pub(crate) struct BootedVm {
    id: Uuid,
    vm: RunningVm,
    control: Arc<BootControl>,
}

pub struct VmmSupervisor {
    config: Config,
    running: Mutex<HashMap<Uuid, RunningVm>>,
    booting: Mutex<HashMap<Uuid, BootingVm>>,
    /// Serializes VMM spawn registration with shutdown's boot cancellation sweep.
    ///
    /// Lifecycle publication orders locks as `boot_gate` -> `running`/`warm` ->
    /// `booting`. It is async because Running publication intentionally holds it
    /// through fleet and durable-store acknowledgement, never while holding a
    /// synchronous supervisor lock.
    boot_gate: AsyncMutex<()>,
    /// Pre-booted, unassigned VMs kept ready by the warm-pool replenisher.
    warm: Mutex<VecDeque<WarmVm>>,
    /// Every entry is an acquired scheduler reservation. Entries transfer from
    /// booting to warm/running and are removed only after confirmed cleanup.
    reservations: Mutex<HashSet<Uuid>>,
    scheduler: Arc<Scheduler>,
    net: Option<NetProvisioner>,
    shutting_down: AtomicBool,
}

#[derive(Debug, Default, Clone)]
pub struct ShutdownSummary {
    pub running_ids: Vec<Uuid>,
    pub booting_ids: Vec<Uuid>,
    pub warm_ids: Vec<Uuid>,
    pub internal_booting_ids: Vec<Uuid>,
    pub running: usize,
    pub warm: usize,
    pub booting: usize,
    /// Successfully cleaned internal refill/golden boots. They have no user VM
    /// record, but their scheduler reservations must still be released.
    pub internal_booting: usize,
}

impl ShutdownSummary {
    pub fn total(&self) -> usize {
        self.running + self.warm + self.booting + self.internal_booting
    }
}

#[derive(Debug)]
pub(crate) struct ShutdownFailure {
    pub(crate) summary: ShutdownSummary,
    pub(crate) error: Box<OrchError>,
}

impl From<OrchError> for ShutdownFailure {
    fn from(error: OrchError) -> Self {
        Self {
            summary: ShutdownSummary::default(),
            error: Box::new(error),
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
                self.summary.warm_ids.push(id);
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

    fn booting(&mut self, id: Uuid, purpose: SpawnPurpose, result: Result<(), OrchError>) {
        match result {
            Ok(()) => {
                if purpose == SpawnPurpose::Live {
                    self.summary.booting_ids.push(id);
                    self.summary.booting += 1;
                } else {
                    self.summary.internal_booting_ids.push(id);
                    self.summary.internal_booting += 1;
                }
            }
            Err(error) => self.failures.push(format!(
                "booting VM {id} cleanup retained allocation for retry: {error}"
            )),
        }
    }

    fn record_internal_failure(&mut self, error: OrchError) {
        self.failures.push(error.to_string());
    }

    fn finish(self) -> Result<ShutdownSummary, Box<ShutdownFailure>> {
        if self.failures.is_empty() {
            Ok(self.summary)
        } else {
            Err(Box::new(ShutdownFailure {
                summary: self.summary,
                error: Box::new(OrchError::Internal(self.failures.join("; "))),
            }))
        }
    }
}

impl VmmSupervisor {
    #[cfg(test)]
    pub fn new(config: Config) -> Self {
        let scheduler = Arc::new(Scheduler::new(config.clone()));
        Self::new_with_live_vms(config, std::iter::empty(), &[], scheduler)
            .expect("test supervisor networking setup must succeed")
    }

    pub fn new_with_live_vms(
        config: Config,
        live_vm_ids: impl IntoIterator<Item = Uuid>,
        preflight_taps: &[String],
        scheduler: Arc<Scheduler>,
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
            boot_gate: AsyncMutex::new(()),
            warm: Mutex::new(VecDeque::new()),
            reservations: Mutex::new(HashSet::new()),
            scheduler,
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

    pub(crate) async fn begin_boot(
        &self,
        id: Uuid,
        purpose: SpawnPurpose,
        on_registered: impl FnOnce() -> Result<(), OrchError>,
    ) -> Result<BootTicket, OrchError> {
        let _gate = self.boot_gate.lock().await;
        if self.is_shutting_down() {
            return Err(self.shutdown_error());
        }
        if !self.scheduler.try_reserve() {
            return Err(OrchError::Overloaded {
                message: "host at capacity".into(),
                retry_after_secs: 1,
            });
        }
        let inserted_reservation = match self.reservations.lock() {
            Ok(mut reservations) => reservations.insert(id),
            Err(_) => {
                self.scheduler.release();
                return Err(OrchError::Internal(
                    "supervisor reservation lock poisoned".into(),
                ));
            }
        };
        if !inserted_reservation {
            self.scheduler.release();
            return Err(OrchError::Conflict(format!(
                "VM {id} already has a boot reservation"
            )));
        }

        let control = Arc::new(BootControl::new());
        let socket_path = self.socket_path_for(id);
        let registered = self
            .booting
            .lock()
            .map_err(|_| OrchError::Internal("supervisor booting lock poisoned".into()))
            .map(|mut booting| {
                booting.insert(
                    id,
                    BootingVm {
                        socket_path,
                        process: None,
                        control: Arc::clone(&control),
                        purpose,
                    },
                );
            });
        if let Err(error) = registered {
            self.release_reservation_after_cleanup(id);
            return Err(error);
        }
        if let Err(error) = on_registered() {
            self.complete_booting(id, &control, Ok(()));
            self.release_reservation_after_cleanup(id);
            return Err(error);
        }
        Ok(BootTicket {
            id,
            control,
            purpose,
        })
    }

    #[cfg(test)]
    fn track_booting(
        &self,
        id: Uuid,
        socket_path: PathBuf,
        process: ManagedProcess,
        purpose: SpawnPurpose,
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
                process: Some(process),
                control: Arc::clone(&control),
                purpose,
            },
        );
        Ok(control)
    }

    fn release_reservation_after_cleanup(&self, id: Uuid) {
        let released = self
            .reservations
            .lock()
            .map(|mut reservations| reservations.remove(&id))
            .unwrap_or(false);
        if released {
            self.scheduler.release();
        }
    }

    pub(crate) fn release_reservation_after_terminal(&self, id: Uuid) -> Result<(), OrchError> {
        let released = self
            .reservations
            .lock()
            .map_err(|_| OrchError::Internal("supervisor reservation lock poisoned".into()))?
            .remove(&id);
        if released {
            self.scheduler.release();
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn reserve_existing_for_test(&self, id: Uuid) {
        assert!(self.scheduler.try_reserve());
        assert!(self.reservations.lock().unwrap().insert(id));
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
            if !control.is_cancelled() {
                self.release_reservation_after_cleanup(id);
            }
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

    pub(crate) async fn publish_running_with<T, F, Fut>(
        &self,
        booted: BootedVm,
        publish_lifecycle: F,
    ) -> Result<T, OrchError>
    where
        T: Send,
        F: FnOnce(u32, PathBuf) -> Fut + Send,
        Fut: std::future::Future<Output = Result<T, OrchError>> + Send,
    {
        let BootedVm { id, vm, control } = booted;
        let pid = vm.pid;
        let socket_path = vm.socket_path.clone();
        let gate = self.boot_gate.lock().await;
        let boot_is_current = match self.booting.lock() {
            Ok(booting) => {
                boot_can_publish(&control, self.is_shutting_down())
                    && booting
                        .get(&id)
                        .is_some_and(|booting_vm| Arc::ptr_eq(&booting_vm.control, &control))
            }
            Err(_) => {
                drop(gate);
                return Err(self.cleanup_boot_failure(
                    id,
                    &control,
                    &vm,
                    OrchError::Internal("supervisor booting lock poisoned".into()),
                ));
            }
        };
        if !boot_is_current {
            drop(gate);
            return Err(self.cleanup_boot_failure(id, &control, &vm, self.shutdown_error()));
        }

        let published = match publish_lifecycle(pid, socket_path.clone()).await {
            Ok(published) => published,
            Err(error) => {
                drop(gate);
                return Err(self.cleanup_boot_failure(id, &control, &vm, error));
            }
        };

        let mut running = match self.running.lock() {
            Ok(running) => running,
            Err(_) => {
                drop(gate);
                return Err(self.cleanup_boot_failure(
                    id,
                    &control,
                    &vm,
                    OrchError::Internal("supervisor lock poisoned".into()),
                ));
            }
        };
        let mut booting = match self.booting.lock() {
            Ok(booting) => booting,
            Err(_) => {
                drop(running);
                drop(gate);
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
            drop(gate);
            return Err(self.cleanup_boot_failure(id, &control, &vm, self.shutdown_error()));
        }
        booting.remove(&id);
        running.insert(id, vm);
        control.complete(Ok(()));
        Ok(published)
    }

    async fn publish_warm(&self, booted: BootedVm, spec: VmSpawnConfig) -> Result<(), OrchError> {
        let BootedVm { id, vm, control } = booted;
        let gate = self.boot_gate.lock().await;
        let mut warm = match self.warm.lock() {
            Ok(warm) => warm,
            Err(_) => {
                drop(gate);
                return Err(self.cleanup_boot_failure(
                    id,
                    &control,
                    &vm,
                    OrchError::Internal("warm lock poisoned".into()),
                ));
            }
        };
        let mut booting = match self.booting.lock() {
            Ok(booting) => booting,
            Err(_) => {
                drop(warm);
                drop(gate);
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
            drop(gate);
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
                if !control.is_cancelled() {
                    self.release_reservation_after_cleanup(id);
                }
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
    ) -> Vec<(Uuid, SpawnPurpose, Result<(), OrchError>)> {
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
                (id, booting_vm.purpose, outcome)
            })
            .collect()
    }

    fn retry_booting_cleanup(&self, id: Uuid, booting_vm: &BootingVm) -> Result<(), OrchError> {
        let mut failures = booting_vm
            .process
            .as_ref()
            .and_then(|process| {
                self.teardown_vm(
                    id,
                    &RunningVm {
                        pid: process.pid,
                        socket_path: booting_vm.socket_path.clone(),
                        process: process.clone(),
                        net: None,
                    },
                )
                .err()
            })
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

    fn spawn_server_for_boot(
        &self,
        ticket: &BootTicket,
        memory_mib: Option<u64>,
    ) -> Result<RunningVm, OrchError> {
        let id = ticket.id;
        let socket_path = self.socket_path_for(id);
        let _ = std::fs::remove_file(&socket_path);
        let cgroup_args = self.cgroup_args(id, memory_mib);
        let boot_gate = self.boot_gate.blocking_lock();
        let can_start = !self.is_shutting_down()
            && !ticket.control.is_cancelled()
            && self
                .booting
                .lock()
                .map_err(|_| OrchError::Internal("supervisor booting lock poisoned".into()))?
                .get(&id)
                .is_some_and(|booting_vm| Arc::ptr_eq(&booting_vm.control, &ticket.control));
        if !can_start {
            drop(boot_gate);
            self.complete_booting(id, &ticket.control, Ok(()));
            return Err(self.shutdown_error());
        }
        let child = match Command::new(&self.config.vmm_bin)
            .arg("serve")
            .arg("--socket")
            .arg(&socket_path)
            .args(&cgroup_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                drop(boot_gate);
                self.complete_booting(id, &ticket.control, Ok(()));
                if !ticket.control.is_cancelled() {
                    self.release_reservation_after_cleanup(id);
                }
                return Err(OrchError::Internal(format!("spawn vmm: {error}")));
            }
        };

        let process = ManagedProcess::new(child);
        let pid = process.pid;
        let attached = self
            .booting
            .lock()
            .map_err(|_| OrchError::Internal("supervisor booting lock poisoned".into()))
            .and_then(|mut booting| {
                let booting_vm = booting.get_mut(&id).ok_or_else(|| {
                    OrchError::Internal(format!("boot registration disappeared for VM {id}"))
                })?;
                if !Arc::ptr_eq(&booting_vm.control, &ticket.control)
                    || ticket.control.is_cancelled()
                    || self.is_shutting_down()
                {
                    return Err(self.shutdown_error());
                }
                booting_vm.socket_path = socket_path.clone();
                booting_vm.process = Some(process.clone());
                Ok(())
            });
        drop(boot_gate);
        let vm = RunningVm {
            pid,
            socket_path,
            process,
            net: None,
        };
        if let Err(error) = attached {
            return Err(self.cleanup_boot_failure(id, &ticket.control, &vm, error));
        }
        if ticket.purpose == SpawnPurpose::Refill {
            self.move_pid_to_refill_cgroup(pid);
        }
        Ok(vm)
    }

    /// Boot a VM (spawn `vmm serve`, wait for its socket, provision networking,
    /// send Create) without holding the running/warm locks.
    fn boot_vm(
        &self,
        ticket: BootTicket,
        vm_config: &VmSpawnConfig,
    ) -> Result<BootedVm, OrchError> {
        let id = ticket.id;
        let base_vm = self.spawn_server_for_boot(&ticket, Some(vm_config.memory_mib))?;
        if let Err(error) =
            self.wait_for_socket_or_cancellation(&base_vm.socket_path, &ticket.control)
        {
            return Err(self.cleanup_boot_failure(id, &ticket.control, &base_vm, error));
        }
        if !boot_can_publish(&ticket.control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(
                id,
                &ticket.control,
                &base_vm,
                self.shutdown_error(),
            ));
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
                    return Err(self.cleanup_boot_failure(id, &ticket.control, &base_vm, cause));
                }
            },
            None => None,
        };
        let vm = RunningVm {
            net: net_alloc,
            ..base_vm
        };
        if !boot_can_publish(&ticket.control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(id, &ticket.control, &vm, self.shutdown_error()));
        }

        let vmm_config = build_vmm_config(id, vm_config, vm.net.as_ref());
        let client = VmmClient::new(&vm.socket_path);
        if let Err(e) = client.create(vmm_config) {
            return Err(self.cleanup_boot_failure(
                id,
                &ticket.control,
                &vm,
                OrchError::Vmm(format!("create vm: {e}")),
            ));
        }
        if !boot_can_publish(&ticket.control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(id, &ticket.control, &vm, self.shutdown_error()));
        }

        Ok(BootedVm {
            id,
            vm,
            control: ticket.control,
        })
    }

    pub(crate) fn spawn_vm(
        &self,
        ticket: BootTicket,
        vm_config: VmSpawnConfig,
    ) -> Result<BootedVm, OrchError> {
        self.boot_vm(ticket, &vm_config)
    }

    /// Restore a VM from a node-local snapshot file: spawn a fresh `vmm serve`,
    /// send Restore, and register the resumed VM. The snapshot carries the
    /// guest's device/net config, so we do not re-provision host networking
    /// here (restore is used for the fast warm/resume path).
    fn spawn_and_restore(
        &self,
        ticket: BootTicket,
        snapshot_path: &str,
        overlay: Option<String>,
    ) -> Result<BootedVm, OrchError> {
        let id = ticket.id;
        let vm = self.spawn_server_for_boot(&ticket, None)?;
        if let Err(error) = self.wait_for_socket_or_cancellation(&vm.socket_path, &ticket.control) {
            return Err(self.cleanup_boot_failure(id, &ticket.control, &vm, error));
        }
        if !boot_can_publish(&ticket.control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(id, &ticket.control, &vm, self.shutdown_error()));
        }

        let client = VmmClient::new(&vm.socket_path);
        if let Err(e) = client.restore(snapshot_path, overlay.clone()) {
            return Err(self.cleanup_boot_failure(
                id,
                &ticket.control,
                &vm,
                OrchError::Vmm(format!("restore vm: {e}")),
            ));
        }
        if !boot_can_publish(&ticket.control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(id, &ticket.control, &vm, self.shutdown_error()));
        }
        Ok(BootedVm {
            id,
            vm,
            control: ticket.control,
        })
    }

    pub(crate) fn restore_vm(
        &self,
        ticket: BootTicket,
        snapshot_path: String,
    ) -> Result<BootedVm, OrchError> {
        self.spawn_and_restore(ticket, &snapshot_path, None)
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

    pub(crate) async fn spawn_warm(self: Arc<Self>, class: WarmClass) -> Result<(), OrchError> {
        let id = Uuid::new_v4();
        let spec = VmSpawnConfig::from_warm_class(&self.config, &class);
        let ticket = self.begin_boot(id, SpawnPurpose::Refill, || Ok(())).await?;
        let worker = Arc::clone(&self);
        let worker_spec = spec.clone();
        let booted = tokio::task::spawn_blocking(move || worker.boot_vm(ticket, &worker_spec))
            .await
            .map_err(|error| OrchError::Internal(format!("warm boot task: {error}")))??;
        let socket_path = booted.vm.socket_path.clone();
        let worker = Arc::clone(&self);
        tokio::task::spawn_blocking(move || worker.await_ready(&socket_path))
            .await
            .map_err(|error| OrchError::Internal(format!("warm readiness task: {error}")))?;
        self.publish_warm(booted, spec).await
    }

    /// Cold-boot one VM for `class`, wait until it is ready, take a full golden
    /// snapshot, then tear down the builder VM. Runtime warm capacity is filled
    /// by restoring clones from the returned snapshot.
    pub(crate) async fn create_golden(
        self: Arc<Self>,
        class: WarmClass,
    ) -> Result<String, OrchError> {
        let id = Uuid::new_v4();
        let spec = VmSpawnConfig::from_warm_class(&self.config, &class);
        let ticket = self.begin_boot(id, SpawnPurpose::Refill, || Ok(())).await?;
        let worker = Arc::clone(&self);
        let worker_spec = spec.clone();
        let booted = tokio::task::spawn_blocking(move || worker.boot_vm(ticket, &worker_spec))
            .await
            .map_err(|error| OrchError::Internal(format!("golden boot task: {error}")))??;
        let socket_path = booted.vm.socket_path.clone();
        let worker = Arc::clone(&self);
        tokio::task::spawn_blocking(move || worker.await_ready(&socket_path))
            .await
            .map_err(|error| OrchError::Internal(format!("golden readiness task: {error}")))?;
        if !boot_can_publish(&booted.control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(
                id,
                &booted.control,
                &booted.vm,
                self.shutdown_error(),
            ));
        }
        let socket_path = booted.vm.socket_path.clone();
        let snapshot_path = tokio::task::spawn_blocking(move || {
            VmmClient::new(&socket_path)
                .snapshot(false)
                .map_err(|error| OrchError::Vmm(format!("snapshot golden: {error}")))
        })
        .await
        .map_err(|error| OrchError::Internal(format!("golden snapshot task: {error}")))?
        .map_err(|error| self.cleanup_boot_failure(id, &booted.control, &booted.vm, error))?;

        self.finish_booted_vm(id, booted.control, &booted.vm)?;
        Ok(snapshot_path)
    }

    /// Restore one warm-pool VM from an existing golden snapshot and park it.
    pub(crate) async fn spawn_warm_restore(
        self: Arc<Self>,
        class: WarmClass,
        snapshot_path: String,
    ) -> Result<(), OrchError> {
        let id = Uuid::new_v4();
        let spec = VmSpawnConfig::from_warm_class(&self.config, &class);
        let overlay = overlay_path_for_config(id, &spec);
        let ticket = self.begin_boot(id, SpawnPurpose::Refill, || Ok(())).await?;
        let worker = Arc::clone(&self);
        let booted = tokio::task::spawn_blocking(move || {
            worker.spawn_and_restore(ticket, &snapshot_path, overlay)
        })
        .await
        .map_err(|error| OrchError::Internal(format!("warm restore task: {error}")))??;
        let socket_path = booted.vm.socket_path.clone();
        let worker = Arc::clone(&self);
        tokio::task::spawn_blocking(move || worker.await_ready(&socket_path))
            .await
            .map_err(|error| {
                OrchError::Internal(format!("warm restore readiness task: {error}"))
            })?;
        self.publish_warm(booted, spec).await
    }

    /// Claim and publish a matching warm VM under the same lifecycle gate as a
    /// cold boot. A shutdown/delete either waits for this publication then tears
    /// it down, or wins before it starts; no write-behind warm visibility exists.
    pub(crate) async fn take_warm_with_publication<T, F, Fut>(
        &self,
        want: &VmSpawnConfig,
        publish_lifecycle: F,
    ) -> Result<Option<T>, OrchError>
    where
        T: Send,
        F: FnOnce(Uuid, u32, PathBuf) -> Fut + Send,
        Fut: std::future::Future<Output = Result<T, OrchError>> + Send,
    {
        let _gate = self.boot_gate.lock().await;
        if self.is_shutting_down() {
            return Ok(None);
        }
        let taken = {
            let mut warm = self
                .warm
                .lock()
                .map_err(|_| OrchError::Internal("warm lock poisoned".into()))?;
            let Some(pos) = warm.iter().position(|warm_vm| &warm_vm.spec == want) else {
                return Ok(None);
            };
            warm.remove(pos).expect("warm position was selected")
        };
        let pid = taken.vm.pid;
        let socket = taken.vm.socket_path.clone();
        self.move_pid_to_default_cgroup(pid);
        let published = match publish_lifecycle(taken.id, pid, socket).await {
            Ok(published) => published,
            Err(error) => {
                self.warm
                    .lock()
                    .map_err(|_| OrchError::Internal("warm lock poisoned".into()))?
                    .push_back(taken);
                return Err(error);
            }
        };
        if self.is_shutting_down() {
            self.warm
                .lock()
                .map_err(|_| OrchError::Internal("warm lock poisoned".into()))?
                .push_back(taken);
            return Err(self.shutdown_error());
        }
        self.running
            .lock()
            .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?
            .insert(taken.id, taken.vm);
        Ok(Some(published))
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
        let booting = {
            let _gate = self.boot_gate.blocking_lock();
            let booting_vm = self
                .booting
                .lock()
                .map_err(|_| OrchError::Internal("supervisor booting lock poisoned".into()))?
                .get(&id)
                .cloned();
            if let Some(booting_vm) = booting_vm {
                booting_vm.control.request_cancellation();
                Some(booting_vm)
            } else {
                None
            }
        };
        if let Some(booting_vm) = booting {
            return self.finish_cancelled_boot(id, booting_vm);
        }

        // Remove from the running map under the lifecycle gate, then do slow
        // teardown without any lock held.
        let running = {
            let _gate = self.boot_gate.blocking_lock();
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

    fn finish_cancelled_boot(&self, id: Uuid, booting_vm: BootingVm) -> Result<(), OrchError> {
        match booting_vm.control.wait_for_completion() {
            Ok(()) => Ok(()),
            Err(completion_error) => {
                self.retry_booting_cleanup(id, &booting_vm)
                    .map_err(|retry_error| {
                        OrchError::Internal(format!(
                            "{completion_error}; retrying boot cleanup failed: {retry_error}"
                        ))
                    })
            }
        }?;
        self.complete_booting(id, &booting_vm.control, Ok(()));
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

    pub(crate) fn stop_all(&self) -> Result<ShutdownSummary, Box<ShutdownFailure>> {
        let booting = {
            let _gate = self.boot_gate.blocking_lock();
            // This is the linearization point with user lifecycle publication:
            // after it, no boot can enter its durable Running publication.
            self.shutting_down.store(true, Ordering::SeqCst);
            self.signal_booting_tasks()
                .map_err(|error| Box::new(ShutdownFailure::from(error)))?
        };
        let booting = self.complete_cancelled_booting_tasks(booting);
        let (running, warm) = {
            let mut running = self.running.lock().map_err(|_| {
                Box::new(ShutdownFailure::from(OrchError::Internal(
                    "supervisor lock poisoned".into(),
                )))
            })?;
            let mut warm = self.warm.lock().map_err(|_| {
                Box::new(ShutdownFailure::from(OrchError::Internal(
                    "warm lock poisoned".into(),
                )))
            })?;
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
        for (id, purpose, result) in booting {
            transitions.booting(id, purpose, result);
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
        self.shutting_down.store(true, Ordering::SeqCst);
        let running = self.running.lock().map(|vms| vms.len()).unwrap_or_default();
        let warm = self.warm.lock().map(|vms| vms.len()).unwrap_or_default();
        let booting = self.booting.lock().map(|vms| vms.len()).unwrap_or_default();
        if running + warm + booting > 0 {
            tracing::error!(
                running,
                warm,
                booting,
                "supervisor dropped with retained VMs; no teardown retry is safe without durable lifecycle persistence"
            );
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
        transitions.booting(Uuid::new_v4(), SpawnPurpose::Live, Ok(()));

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
            .track_booting(
                id,
                PathBuf::from("booting.sock"),
                process.clone(),
                SpawnPurpose::Live,
            )
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
    fn stop_all_winning_after_create_vmm_setup_cannot_publish_lifecycle() {
        assert_stop_all_cancellation_blocks_live_publication();
    }

    #[test]
    fn stop_all_winning_after_restore_vmm_setup_cannot_publish_lifecycle() {
        assert_stop_all_cancellation_blocks_live_publication();
    }

    #[test]
    fn single_delete_winning_after_create_vmm_setup_cancels_publication() {
        assert_single_stop_cancellation_blocks_live_publication();
    }

    #[test]
    fn single_delete_winning_after_restore_vmm_setup_cancels_publication() {
        assert_single_stop_cancellation_blocks_live_publication();
    }

    #[test]
    fn warm_handoff_and_stop_all_share_the_publication_gate() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let spec = spawn_config(false, Some(PathBuf::from("/rootfs.ext4")));
        let ticket = test_runtime()
            .block_on(supervisor.begin_boot(id, SpawnPurpose::Refill, || Ok(())))
            .unwrap();
        let process = ManagedProcess::new(Command::new("true").spawn().unwrap());
        supervisor.complete_booting(id, &ticket.control, Ok(()));
        supervisor.warm.lock().unwrap().push_back(WarmVm {
            id,
            vm: RunningVm {
                pid: process.pid,
                socket_path: PathBuf::from("warm-handoff.sock"),
                process,
                net: None,
            },
            spec: spec.clone(),
        });

        let (publication_started_tx, publication_started_rx) = mpsc::channel();
        let (allow_publication_tx, allow_publication_rx) = mpsc::channel();
        let handoff_supervisor = Arc::clone(&supervisor);
        let handoff = thread::spawn(move || {
            test_runtime()
                .block_on(handoff_supervisor.take_warm_with_publication(
                    &spec,
                    move |vm_id, _, _| async move {
                        publication_started_tx.send(()).unwrap();
                        allow_publication_rx.recv().unwrap();
                        Ok(vm_id)
                    },
                ))
                .unwrap()
                .unwrap()
        });

        publication_started_rx.recv().unwrap();
        let (stop_done_tx, stop_done_rx) = mpsc::channel();
        let stop_supervisor = Arc::clone(&supervisor);
        let stopper = thread::spawn(move || {
            stop_done_tx.send(stop_supervisor.stop_all()).unwrap();
        });
        assert!(
            stop_done_rx.try_recv().is_err(),
            "stop-all must wait for the in-flight warm publication"
        );

        allow_publication_tx.send(()).unwrap();
        assert_eq!(handoff.join().unwrap(), id);
        let summary = stop_done_rx.recv().unwrap().unwrap();
        stopper.join().unwrap();

        assert_eq!(summary.running_ids, vec![id]);
        assert!(summary.warm_ids.is_empty());
        assert!(!supervisor.is_running(id));
    }

    #[test]
    fn failed_boot_cleanup_retains_its_scheduler_reservation() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let ticket = test_runtime()
            .block_on(supervisor.begin_boot(id, SpawnPurpose::Refill, || Ok(())))
            .unwrap();
        let retained_socket = PathBuf::from(format!("target/taritd-supervisor-test/retained-{id}"));
        std::fs::create_dir_all(&retained_socket).unwrap();
        let process = ManagedProcess::new(Command::new("true").spawn().unwrap());
        let vm = RunningVm {
            pid: process.pid,
            socket_path: retained_socket.clone(),
            process,
            net: None,
        };

        let error = supervisor.cleanup_boot_failure(
            id,
            &ticket.control,
            &vm,
            OrchError::Internal("injected boot failure".into()),
        );

        assert!(error
            .to_string()
            .contains("shutdown cleanup retained booting VM"));
        assert!(supervisor.booting.lock().unwrap().contains_key(&id));
        assert!(supervisor.reservations.lock().unwrap().contains(&id));
        assert_eq!(
            supervisor.scheduler.local_capacity(1, 1).sandbox_count,
            1,
            "a retained VMM/socket cleanup must retain the matching capacity reservation"
        );
        std::fs::remove_dir(&retained_socket).unwrap();
    }

    #[test]
    fn cancelled_internal_boot_is_not_returned_as_a_user_stopped_transition() {
        let mut transitions = ShutdownTransitions::default();
        let internal_id = Uuid::new_v4();

        transitions.booting(internal_id, SpawnPurpose::Refill, Ok(()));
        let summary = transitions.finish().unwrap();

        assert!(summary.booting_ids.is_empty());
        assert_eq!(summary.booting, 0);
        assert_eq!(summary.internal_booting, 1);
    }

    fn assert_stop_all_cancellation_blocks_live_publication() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let process = ManagedProcess::new(Command::new("true").spawn().unwrap());
        let control = supervisor
            .track_booting(
                id,
                PathBuf::from("booting-publication.sock"),
                process.clone(),
                SpawnPurpose::Live,
            )
            .unwrap();
        let published = Arc::new(AtomicBool::new(false));
        let worker_supervisor = Arc::clone(&supervisor);
        let worker_control = Arc::clone(&control);
        let worker_published = Arc::clone(&published);
        let (vmm_ready_tx, vmm_ready_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            vmm_ready_tx.send(()).unwrap();
            worker_control.wait_for_cancellation();
            let vm = RunningVm {
                pid: process.pid,
                socket_path: PathBuf::from("booting-publication.sock"),
                process,
                net: None,
            };
            let result = test_runtime().block_on(worker_supervisor.publish_running_with(
                BootedVm {
                    id,
                    vm,
                    control: worker_control,
                },
                move |_, _| async move {
                    worker_published.store(true, Ordering::SeqCst);
                    Ok(())
                },
            ));
            assert!(result.is_err());
        });

        vmm_ready_rx.recv().unwrap();
        let summary = supervisor.stop_all().unwrap();
        worker.join().unwrap();

        assert_eq!(summary.booting_ids, vec![id]);
        assert!(!published.load(Ordering::SeqCst));
        assert!(!supervisor.is_running(id));
    }

    fn assert_single_stop_cancellation_blocks_live_publication() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let process = ManagedProcess::new(Command::new("true").spawn().unwrap());
        let control = supervisor
            .track_booting(
                id,
                PathBuf::from("single-stop-publication.sock"),
                process.clone(),
                SpawnPurpose::Live,
            )
            .unwrap();
        let worker_supervisor = Arc::clone(&supervisor);
        let worker_control = Arc::clone(&control);
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            worker_control.wait_for_cancellation();
            let result = test_runtime().block_on(worker_supervisor.publish_running_with(
                BootedVm {
                    id,
                    vm: RunningVm {
                        pid: process.pid,
                        socket_path: PathBuf::from("single-stop-publication.sock"),
                        process,
                        net: None,
                    },
                    control: worker_control,
                },
                |_, _| async { Ok(()) },
            ));
            assert!(result.is_err());
        });

        ready_rx.recv().unwrap();
        supervisor
            .stop_vm(id)
            .expect("delete must cancel an in-flight boot");
        worker.join().unwrap();

        assert!(
            control.is_cancelled(),
            "delete must cancel the boot before it can publish Running"
        );
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
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
