use crate::config::{Config, WarmClass};
use crate::net::{NetAlloc, NetProvisioner};
use crate::scheduler::{ReservationError, ResourceShape, Scheduler};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};
use std::time::{Duration, Instant};
use tarit_types::{OrchError, VmRecord, VmStatus};
use tarit_vmm_client::{
    KernelConfig, MemoryConfig, NetConfig, ScratchIdentity, VcpuConfig, VmConfig, VmmClient,
    VolumeConfig,
};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

pub const DEFAULT_CMDLINE: &str = "earlycon=uart8250,io,0x3f8,115200n8 console=ttyS0 reboot=k panic=1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw virtio_mmio.device=4K@0xd0000000:5";
const GUEST_READY_TIMEOUT: Duration = Duration::from_secs(20);
const RESUME_READY_TIMEOUT: Duration = Duration::from_secs(5);
const GUEST_READY_EXEC_TIMEOUT: Duration = Duration::from_secs(1);
const GUEST_READY_POLL_INTERVAL: Duration = Duration::from_millis(20);
const SOCKET_WAIT_INITIAL: Duration = Duration::from_millis(1);
const SOCKET_WAIT_MAX: Duration = Duration::from_millis(4);
const TEARDOWN_STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// Upper bound for VMM ops that copy guest RAM (suspend, snapshot) or fault it
/// back in (a suspend right after resume). These are silent on the socket for
/// the whole copy, so they get a generous request deadline instead of the
/// 5-second per-read stream timeout of a plain client, which a multi-GiB
/// guest cannot meet.
const LIFECYCLE_OP_TIMEOUT: Duration = Duration::from_secs(600);
const NORMAL_CGROUP_CPU_WEIGHT: u64 = 100;

fn graceful_stop_vmm(socket_path: &Path) {
    if socket_path.as_os_str().is_empty() || !socket_path.exists() {
        return;
    }

    let _ = VmmClient::new(socket_path)
        .with_request_timeout(TEARDOWN_STOP_TIMEOUT)
        .stop();
}

/// Confirm that `pid` is a live VMM process that owns `socket_path`, guarding
/// re-adoption against PID reuse. taritd launches every VMM with
/// `serve --socket <socket_path>`, so the socket path must appear verbatim in
/// the process command line; a recycled PID running something else will not
/// match and is refused rather than adopted.
fn verify_live_vmm(pid: u32, socket_path: &Path) -> Result<(), String> {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return Err(format!("VMM PID {pid} is not alive"));
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map_err(|error| format!("read /proc/{pid}/cmdline: {error}"))?;
    let owns_socket = cmdline
        .split(|byte| *byte == 0)
        .any(|arg| arg == socket_path.as_os_str().as_bytes());
    if !owns_socket {
        return Err(format!(
            "PID {pid} does not own control socket {}; refusing to adopt a reused PID",
            socket_path.display()
        ));
    }
    Ok(())
}

/// Pin the exact process instance behind `pid` with a pidfd. Once taritd holds
/// this descriptor the kernel keeps the PID from being recycled, so a later
/// SIGKILL through the pidfd can never land on an unrelated process that reused
/// the number. Re-adoption runs only on Linux hosts; the non-Linux stub exists
/// so the crate still builds on developer machines.
#[cfg(target_os = "linux")]
fn pidfd_open(pid: u32) -> std::io::Result<OwnedFd> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as std::os::fd::RawFd) })
}

#[cfg(not(target_os = "linux"))]
fn pidfd_open(_pid: u32) -> std::io::Result<OwnedFd> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "pidfd requires Linux",
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadinessCheck {
    Boot,
    Resume,
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
    pub(crate) fn resource_shape(&self) -> ResourceShape {
        ResourceShape::new(self.vcpus, self.memory_mib)
    }

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

#[derive(Debug, Clone)]
struct RunningVm {
    pid: u32,
    socket_path: PathBuf,
    process: ManagedProcess,
    net: Option<NetAlloc>,
    /// Serializes operations whose VMM side effect and control-plane status
    /// must be observed as one transition. The gate is owned by the runtime
    /// entry, so deleting a VM also removes the only registry reference.
    operation_gate: Arc<AsyncMutex<()>>,
}

impl RunningVm {
    fn new(pid: u32, socket_path: PathBuf, process: ManagedProcess, net: Option<NetAlloc>) -> Self {
        Self {
            pid,
            socket_path,
            process,
            net,
            operation_gate: Arc::new(AsyncMutex::new(())),
        }
    }
}

#[derive(Default)]
struct NetworkLeaseState {
    active: usize,
    pending_teardown: Option<NetAlloc>,
    teardown_in_progress: bool,
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
    handle: ProcessHandle,
}

/// How the supervisor can terminate a VMM. A freshly spawned VMM is a child of
/// this process and is reaped through its `Child` handle. A VMM re-adopted after
/// a taritd restart was reparented to init, so taritd can only signal it by PID.
#[derive(Debug, Clone)]
enum ProcessHandle {
    Owned(Arc<Mutex<Child>>),
    Adopted(Arc<OwnedFd>),
}

#[derive(Debug)]
enum ReadoptFailure {
    Unadoptable(String),
    Fatal(String),
}

impl std::fmt::Display for ReadoptFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unadoptable(reason) | Self::Fatal(reason) => formatter.write_str(reason),
        }
    }
}

/// A golden artifact claimed by the supervisor after the builder VMM releases
/// its exact scratch token. The open descriptor protects it from VMM GC while
/// it remains reusable.
#[derive(Debug)]
struct OwnedArtifact {
    path: PathBuf,
    identity: ScratchIdentity,
    _file: File,
}

impl OwnedArtifact {
    fn capture(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(libc::O_NOFOLLOW);
        let file = options.open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        Ok(Self {
            identity: scratch_identity_from_metadata(&metadata),
            path,
            _file: file,
        })
    }

    fn create_private(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = options.open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        Ok(Self {
            identity: scratch_identity_from_metadata(&metadata),
            path,
            _file: file,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn identity(&self) -> ScratchIdentity {
        self.identity.clone()
    }

    fn matches(&self, path: &Path, identity: &ScratchIdentity) -> bool {
        self.path == path && &self.identity == identity
    }

    fn remove(&self) -> std::io::Result<bool> {
        let metadata = match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if scratch_identity_from_metadata(&metadata) != self.identity {
            return Ok(false);
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

/// A RAM snapshot and its snapshot-owned disk upper. Open descriptors pin the
/// exact files until the ownership row is durable; dropping an uncommitted
/// bundle removes only those exact inodes.
pub(crate) struct SnapshotBundle {
    snapshot_path: String,
    overlay_path: Option<String>,
    artifacts: Vec<OwnedArtifact>,
}

enum RestoreOverlay {
    None,
    Fresh,
    Seeded(PathBuf),
}

impl SnapshotBundle {
    pub(crate) fn snapshot_path(&self) -> &str {
        &self.snapshot_path
    }

    pub(crate) fn overlay_path(&self) -> Option<&str> {
        self.overlay_path.as_deref()
    }

    pub(crate) fn persist(mut self) {
        self.artifacts.clear();
    }

    fn cleanup(&mut self) {
        for artifact in self.artifacts.drain(..) {
            if let Err(error) = artifact.remove() {
                tracing::warn!(
                    path = %artifact.path().display(),
                    "remove uncommitted snapshot artifact failed: {error}"
                );
            }
        }
    }
}

impl Drop for SnapshotBundle {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn scratch_identity_from_metadata(metadata: &std::fs::Metadata) -> ScratchIdentity {
    let (created_secs, created_nanos) = metadata
        .created()
        .ok()
        .and_then(|created| {
            created
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .ok()
                .and_then(|duration| {
                    i64::try_from(duration.as_secs())
                        .ok()
                        .map(|seconds| (Some(seconds), Some(duration.subsec_nanos())))
                })
        })
        .unwrap_or((None, None));
    ScratchIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        created_secs,
        created_nanos,
    }
}

impl ManagedProcess {
    fn new(child: Child) -> Self {
        let pid = child.id();
        Self {
            pid,
            handle: ProcessHandle::Owned(Arc::new(Mutex::new(child))),
        }
    }

    /// Track a VMM that survived a taritd restart. taritd is no longer its
    /// parent, so it is identified and signalled through a pidfd that pins the
    /// exact process instance rather than through a `Child` handle.
    fn adopted(pid: u32, pidfd: OwnedFd) -> Self {
        Self {
            pid,
            handle: ProcessHandle::Adopted(Arc::new(pidfd)),
        }
    }

    #[cfg(test)]
    fn owned_child(&self) -> &Arc<Mutex<Child>> {
        match &self.handle {
            ProcessHandle::Owned(child) => child,
            ProcessHandle::Adopted(_) => panic!("adopted process has no owned child handle"),
        }
    }

    fn kill_wait(&self) -> Result<(), OrchError> {
        match &self.handle {
            ProcessHandle::Owned(child) => Self::kill_wait_owned(child),
            ProcessHandle::Adopted(pidfd) => self.kill_wait_adopted(pidfd),
        }
    }

    fn try_exit(&self) -> Result<Option<String>, OrchError> {
        match &self.handle {
            ProcessHandle::Owned(child) => child
                .lock()
                .map_err(|_| OrchError::Internal("VMM child lock poisoned".into()))?
                .try_wait()
                .map(|status| status.map(|status| status.to_string()))
                .map_err(|error| OrchError::Internal(format!("check VMM exit: {error}"))),
            ProcessHandle::Adopted(pidfd) => {
                use std::os::fd::AsRawFd;
                let mut poll_fd = libc::pollfd {
                    fd: pidfd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let rc = unsafe { libc::poll(&mut poll_fd, 1, 0) };
                if rc < 0 {
                    Err(OrchError::Internal(format!(
                        "poll adopted VMM {}: {}",
                        self.pid,
                        std::io::Error::last_os_error()
                    )))
                } else if rc == 0 {
                    Ok(None)
                } else {
                    Ok(Some("adopted VMM exited".into()))
                }
            }
        }
    }

    fn kill_wait_owned(child: &Arc<Mutex<Child>>) -> Result<(), OrchError> {
        let mut child = child
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

    /// Terminate a re-adopted VMM through its pidfd. Signalling the pidfd targets
    /// the exact pinned process, so SIGKILL can never hit a PID that was recycled
    /// after adoption. taritd is not the parent, so it polls the pidfd for exit
    /// notification instead of reaping. A process that already exited counts as
    /// terminated.
    #[cfg(target_os = "linux")]
    fn kill_wait_adopted(&self, pidfd: &OwnedFd) -> Result<(), OrchError> {
        use std::os::fd::AsRawFd;
        let fd = pidfd.as_raw_fd();
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd,
                libc::SIGKILL,
                std::ptr::null_mut::<libc::siginfo_t>(),
                0,
            )
        };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(OrchError::Internal(format!(
                "kill adopted VMM {}: {error}",
                self.pid
            )));
        }
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(OrchError::Internal(format!(
                    "adopted VMM {} did not exit after SIGKILL",
                    self.pid
                )));
            }
            let timeout_ms = remaining.as_millis().min(1000) as libc::c_int;
            let rc = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
            if rc < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(OrchError::Internal(format!(
                    "await adopted VMM {} exit: {error}",
                    self.pid
                )));
            }
            if rc > 0 && (poll_fd.revents & libc::POLLIN) != 0 {
                return Ok(());
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn kill_wait_adopted(&self, _pidfd: &OwnedFd) -> Result<(), OrchError> {
        let pid = self.pid as libc::pid_t;
        if unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return Ok(());
        }
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(OrchError::Internal(format!(
                "kill adopted VMM {pid}: {error}"
            )));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if unsafe { libc::kill(pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(OrchError::Internal(format!(
                    "adopted VMM {pid} did not exit after SIGKILL"
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[derive(Debug)]
struct BootControl {
    purpose: SpawnPurpose,
    cancelled: AtomicBool,
    cancellation: (Mutex<bool>, Condvar),
    completion: (Mutex<Option<Result<(), String>>>, Condvar),
}

/// Tracks a lifecycle worker independently of the API/refill future that waits
/// for it. A worker remains enumerable until it has either completed its
/// publication or compensation path; request cancellation only marks it.
#[derive(Debug)]
pub(crate) struct OwnedTaskControl {
    cancelled: AtomicBool,
    terminal_converged: AtomicBool,
    cancellation: (Mutex<bool>, Condvar),
    completion: (Mutex<Option<Result<(), String>>>, Condvar),
}

impl OwnedTaskControl {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            terminal_converged: AtomicBool::new(false),
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

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn mark_terminal_converged(&self) {
        self.terminal_converged.store(true, Ordering::SeqCst);
    }

    fn terminal_converged(&self) -> bool {
        self.terminal_converged.load(Ordering::SeqCst)
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
            .map_err(|_| OrchError::Internal("owned task completion lock poisoned".into()))?;
        while completed.is_none() {
            completed =
                self.completion.1.wait(completed).map_err(|_| {
                    OrchError::Internal("owned task completion lock poisoned".into())
                })?;
        }
        match completed.as_ref().expect("completion checked") {
            Ok(()) => Ok(()),
            Err(error) => Err(OrchError::Internal(error.clone())),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
struct SpawnAttachmentPause {
    state: Arc<(Mutex<(bool, bool)>, Condvar)>,
}

#[cfg(test)]
impl SpawnAttachmentPause {
    fn entered(&self) -> bool {
        self.state.0.lock().map(|state| state.0).unwrap_or(true)
    }

    fn wait_until_entered(&self) {
        let mut state = self.state.0.lock().unwrap();
        while !state.0 {
            state = self.state.1.wait(state).unwrap();
        }
    }

    fn release(&self) {
        if let Ok(mut state) = self.state.0.lock() {
            state.1 = true;
            self.state.1.notify_all();
        }
    }

    fn wait_after_spawn(&self) {
        let mut state = self.state.0.lock().unwrap();
        state.0 = true;
        self.state.1.notify_all();
        while !state.1 {
            state = self.state.1.wait(state).unwrap();
        }
    }
}

impl BootControl {
    fn new(purpose: SpawnPurpose) -> Self {
        Self {
            purpose,
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
    shape: ResourceShape,
}

pub(crate) struct BootedVm {
    id: Uuid,
    vm: RunningVm,
    control: Arc<BootControl>,
}

/// A lifecycle publisher may retain a fully booted VM when an external
/// publication step has committed but the next one failed. The supervisor then
/// transfers the VM into its running map instead of tearing down resources that
/// the durable lifecycle state still owns.
pub(crate) struct PublicationFailure(pub(crate) OrchError);

/// The result of handing a warm VM to a user lifecycle. In particular, callers
/// must not treat a retained publication failure like a pre-runtime claim
/// failure: the former still owns a live VMM and its reservation.
pub(crate) enum WarmClaimOutcome<T> {
    NoMatch,
    Published(T),
    PreRuntimeFailure(OrchError),
    RetainedPublicationFailure(OrchError),
}

#[derive(Default)]
pub(crate) struct VmAdmissionGate {
    closed: AtomicBool,
    operation: Mutex<()>,
}

impl VmAdmissionGate {
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _operation = self
            .operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }

    pub(crate) fn enter(&self) -> Result<std::sync::MutexGuard<'_, ()>, OrchError> {
        let operation = self
            .operation
            .lock()
            .map_err(|_| OrchError::Internal("supervisor admission lock poisoned".into()))?;
        if self.is_closed() {
            return Err(shutdown_error());
        }
        Ok(operation)
    }

    #[cfg(test)]
    fn admit<T>(&self, operation: impl FnOnce() -> T) -> Result<T, OrchError> {
        let _operation = self.enter()?;
        Ok(operation())
    }
}

fn shutdown_error() -> OrchError {
    OrchError::Overloaded {
        message: "taritd is shutting down".into(),
        retry_after_secs: 1,
    }
}

pub struct VmmSupervisor {
    config: Config,
    running: Mutex<HashMap<Uuid, RunningVm>>,
    network_leases: Mutex<HashMap<Uuid, NetworkLeaseState>>,
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
    /// Async lifecycle/refill workers are owned here rather than by the API or
    /// replenisher future that awaits their result. Shutdown can therefore mark,
    /// enumerate, and wait every worker before tearing resources down.
    owned_tasks: Mutex<HashMap<Uuid, Arc<OwnedTaskControl>>>,
    #[cfg(test)]
    spawn_attachment_pause: Mutex<Option<SpawnAttachmentPause>>,
    scheduler: Arc<Scheduler>,
    golden_artifacts: Mutex<Vec<OwnedArtifact>>,
    unexpected_exits: Mutex<VecDeque<UnexpectedVmmExit>>,
    net: Option<NetProvisioner>,
    shutting_down: AtomicBool,
    admission: Arc<VmAdmissionGate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnexpectedVmmExit {
    pub(crate) id: Uuid,
    pub(crate) pid: u32,
    pub(crate) status: String,
    pub(crate) cleanup_error: Option<String>,
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
        std::fs::create_dir_all(&config.socket_dir)
            .map_err(|error| OrchError::Internal(format!("create runtime directory: {error}")))?;
        let overlay_dir = config.socket_dir.join("overlays");
        std::fs::create_dir_all(&overlay_dir)
            .map_err(|error| OrchError::Internal(format!("create overlay directory: {error}")))?;
        std::fs::set_permissions(&overlay_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| OrchError::Internal(format!("protect overlay directory: {error}")))?;
        let snapshot_dir = config.socket_dir.join("snapshots");
        std::fs::create_dir_all(&snapshot_dir).map_err(|error| {
            OrchError::Internal(format!("create snapshot artifact directory: {error}"))
        })?;
        std::fs::set_permissions(&snapshot_dir, std::fs::Permissions::from_mode(0o700)).map_err(
            |error| OrchError::Internal(format!("protect snapshot artifact directory: {error}")),
        )?;
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
            network_leases: Mutex::new(HashMap::new()),
            booting: Mutex::new(HashMap::new()),
            boot_gate: AsyncMutex::new(()),
            warm: Mutex::new(VecDeque::new()),
            owned_tasks: Mutex::new(HashMap::new()),
            #[cfg(test)]
            spawn_attachment_pause: Mutex::new(None),
            scheduler,
            golden_artifacts: Mutex::new(Vec::new()),
            unexpected_exits: Mutex::new(VecDeque::new()),
            net,
            shutting_down: AtomicBool::new(false),
            admission: Arc::new(VmAdmissionGate::default()),
        })
    }

    fn socket_path_for(&self, id: Uuid) -> PathBuf {
        self.config.socket_dir.join(format!("{id}.sock"))
    }

    fn overlay_path_for(&self, id: Uuid) -> String {
        self.config
            .socket_dir
            .join("overlays")
            .join(format!("{id}.cow"))
            .display()
            .to_string()
    }

    fn overlay_path_for_config(&self, id: Uuid, cfg: &VmSpawnConfig) -> Option<String> {
        cfg.rootfs_path.is_some().then(|| self.overlay_path_for(id))
    }

    fn snapshot_overlay_path(&self) -> PathBuf {
        self.config
            .socket_dir
            .join("snapshots")
            .join(format!("{}.cow", Uuid::new_v4()))
    }

    fn snapshot_ram_path(&self) -> PathBuf {
        self.config
            .socket_dir
            .join("snapshots")
            .join(format!("bundle-{}.ram", Uuid::new_v4()))
    }

    /// Poll every locally owned process without allocating a thread per VM.
    /// The caller runs this on the bounded blocking pool from the existing
    /// lifecycle reconciliation cadence.
    pub(crate) fn scan_for_exited_processes(&self) {
        let processes = self
            .running
            .lock()
            .map(|running| {
                running
                    .iter()
                    .map(|(id, vm)| (*id, vm.process.clone(), Arc::clone(&vm.operation_gate)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let warm_processes = self
            .warm
            .lock()
            .map(|warm| {
                warm.iter()
                    .map(|vm| {
                        (
                            vm.id,
                            vm.vm.process.clone(),
                            Arc::clone(&vm.vm.operation_gate),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (id, process, operation_gate) in processes.into_iter().chain(warm_processes) {
            match process.try_exit() {
                Ok(Some(status)) => {
                    self.reconcile_process_exit(id, process.pid, &status, operation_gate);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(vm = %id, pid = process.pid, %error, "VMM exit scan failed");
                }
            }
        }
    }

    fn reconcile_process_exit(
        &self,
        id: Uuid,
        pid: u32,
        status: &str,
        operation_gate: Arc<AsyncMutex<()>>,
    ) {
        enum Location {
            Running(RunningVm),
            Warm(RunningVm),
        }

        // A completed live operation persists its state before releasing this
        // gate. Once the dead runtime is removed below, no new operation can
        // obtain the gate, preventing a late pause/resume result from replacing
        // the terminal error state.
        let _operation = operation_gate.blocking_lock();
        let gate = self.boot_gate.blocking_lock();
        let location = self
            .running
            .lock()
            .ok()
            .and_then(|mut running| {
                running
                    .get(&id)
                    .is_some_and(|vm| vm.pid == pid)
                    .then(|| running.remove(&id).map(Location::Running))
                    .flatten()
            })
            .or_else(|| {
                self.warm.lock().ok().and_then(|mut warm| {
                    warm.iter()
                        .position(|vm| vm.id == id && vm.vm.pid == pid)
                        .and_then(|index| warm.remove(index))
                        .map(|vm| Location::Warm(vm.vm))
                })
            });
        drop(gate);

        let Some(location) = location else {
            // Expected teardown removed ownership before signalling the child.
            return;
        };
        let (vm, user_vm) = match location {
            Location::Running(vm) => (vm, true),
            Location::Warm(vm) => (vm, false),
        };
        let cleanup_error = self.teardown_vm(id, &vm).err().map(|error| {
            tracing::error!(vm = %id, pid, %error, "unexpected VMM exit cleanup incomplete");
            error.to_string()
        });
        self.release_reservation_after_cleanup(id);
        if user_vm {
            tracing::error!(vm = %id, pid, %status, "VMM exited unexpectedly");
            if let Ok(mut exits) = self.unexpected_exits.lock() {
                exits.push_back(UnexpectedVmmExit {
                    id,
                    pid,
                    status: status.to_string(),
                    cleanup_error,
                });
            }
        } else {
            tracing::warn!(vm = %id, pid, %status, "warm VMM exited; capacity will be replenished");
        }
    }

    /// Drain local runtime failures for the durable lifecycle reconciler. The
    /// process watcher already removed runtime resources and released capacity;
    /// callers must persist an Error/Stopped observed state for these VM ids.
    pub(crate) fn take_unexpected_exits(&self) -> Vec<UnexpectedVmmExit> {
        self.unexpected_exits
            .lock()
            .map(|mut exits| exits.drain(..).collect())
            .unwrap_or_default()
    }

    /// Build `vmm serve` cgroup arguments from the exact scheduler reservation.
    /// Cold boot and restore receive identical CPU, memory and PID enforcement.
    fn cgroup_args(&self, id: Uuid, shape: ResourceShape) -> Result<Vec<String>, OrchError> {
        let Some(path) = self.exact_vm_cgroup_path(id) else {
            return Ok(Vec::new());
        };
        let cpu_millis = shape
            .vcpus
            .checked_mul(1_000)
            .ok_or_else(|| OrchError::BadRequest("vCPU cgroup limit overflow".into()))?;
        let max_mib = shape
            .memory_mib
            .checked_add(shape.memory_mib / 2)
            .and_then(|value| value.checked_add(256))
            .ok_or_else(|| OrchError::BadRequest("memory cgroup limit overflow".into()))?;
        Ok(vec![
            "--cgroup".to_string(),
            path.display().to_string(),
            "--cgroup-pids-max".to_string(),
            self.config.vm_cgroup_pids_max.to_string(),
            "--cgroup-cpu-max".to_string(),
            format!("{cpu_millis}m"),
            "--cgroup-memory-max".to_string(),
            format!("{max_mib}M"),
        ])
    }

    /// The VMM creates this child and applies the VM's exact CPU, memory and PID
    /// limits to it. Warm-pool prioritization may change `cpu.weight` inside
    /// this child, but must never move the process to a different cgroup.
    fn exact_vm_cgroup_path(&self, id: Uuid) -> Option<PathBuf> {
        self.config
            .vm_cgroup_parent
            .as_ref()
            .map(|parent| Path::new(parent).join(format!("tarit-{id}")))
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire) || self.admission.is_closed()
    }

    pub fn begin_shutdown(&self) {
        self.admission.close();
        self.shutting_down.store(true, Ordering::Release);
    }

    pub(crate) fn admission_gate(&self) -> Arc<VmAdmissionGate> {
        Arc::clone(&self.admission)
    }

    #[cfg(test)]
    pub(crate) fn admission_is_closed(&self) -> bool {
        self.admission.is_closed()
    }

    fn shutdown_error(&self) -> OrchError {
        shutdown_error()
    }

    fn configure_refill_cgroup(&self, id: Uuid, pid: u32) -> Result<(), OrchError> {
        let cgroup = &self.config.warm_pool.refill_cgroup;
        if let Some(path) = self.exact_vm_cgroup_path(id) {
            return write_cgroup_cpu_weight(&path, cgroup.cpu_weight).map_err(|error| {
                OrchError::Internal(format!(
                    "set refill CPU weight for exact VM cgroup {} (VM {id}, PID {pid}): {error}",
                    path.display()
                ))
            });
        }
        let Some(path) = cgroup.path.as_ref() else {
            return Ok(());
        };
        if let Err(e) = move_pid_to_configured_refill_cgroup(pid, path, cgroup.cpu_weight) {
            tracing::warn!(
                pid,
                path = %path.display(),
                cpu_weight = cgroup.cpu_weight,
                "refill cgroup placement skipped: {e}"
            );
        }
        Ok(())
    }

    fn configure_leased_cgroup(&self, id: Uuid, pid: u32) {
        if let Some(path) = self.exact_vm_cgroup_path(id) {
            if let Err(error) = write_cgroup_cpu_weight(&path, NORMAL_CGROUP_CPU_WEIGHT) {
                tracing::warn!(
                    vm = %id,
                    pid,
                    path = %path.display(),
                    cpu_weight = NORMAL_CGROUP_CPU_WEIGHT,
                    "failed to restore leased VM CPU weight in the exact VM cgroup: {error}"
                );
            }
            return;
        }
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

    pub(crate) async fn begin_boot_with_registration<F, Fut>(
        &self,
        id: Uuid,
        purpose: SpawnPurpose,
        shape: ResourceShape,
        on_registered: F,
    ) -> Result<BootTicket, OrchError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(), OrchError>>,
    {
        let _gate = self.boot_gate.lock().await;
        if self.is_shutting_down() {
            return Err(self.shutdown_error());
        }

        let control = Arc::new(BootControl::new(purpose));
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
        registered?;
        // Reserve capacity before the durable registration so capacity
        // rejections leave no durable trace: the admission loop retries the
        // same id, and a leftover Error tombstone from a rejected attempt
        // would make the re-registration fail with an incarnation conflict.
        if let Err(error) = self.scheduler.try_reserve(id, shape) {
            self.complete_booting(id, &control, Ok(()));
            return Err(match error {
                ReservationError::AlreadyReserved => {
                    OrchError::Conflict(format!("VM {id} already has a boot reservation"))
                }
                ReservationError::AccountingOverflow => {
                    OrchError::Internal("scheduler resource accounting failed".into())
                }
                ReservationError::VmLimit
                | ReservationError::VcpuLimit
                | ReservationError::MemoryLimit => OrchError::Overloaded {
                    message: format!("host resource capacity exhausted: {error:?}"),
                    retry_after_secs: 1,
                },
            });
        }
        if let Err(error) = on_registered().await {
            self.scheduler.release(id);
            self.complete_booting(id, &control, Ok(()));
            return Err(error);
        }
        Ok(BootTicket {
            id,
            control,
            purpose,
            shape,
        })
    }

    /// Register an operation before spawning its async worker. The API/refill
    /// caller only waits on a result channel; dropping that waiter never owns or
    /// cancels the registered operation.
    pub(crate) fn begin_owned_task(
        &self,
        id: Uuid,
        _purpose: SpawnPurpose,
    ) -> Result<Arc<OwnedTaskControl>, OrchError> {
        if self.is_shutting_down() {
            return Err(self.shutdown_error());
        }
        let mut tasks = self
            .owned_tasks
            .lock()
            .map_err(|_| OrchError::Internal("owned task lock poisoned".into()))?;
        if self.is_shutting_down() {
            return Err(self.shutdown_error());
        }
        if tasks.contains_key(&id) {
            return Err(OrchError::Conflict(format!(
                "VM {id} already has a supervisor-owned lifecycle task"
            )));
        }
        let control = Arc::new(OwnedTaskControl::new());
        tasks.insert(id, Arc::clone(&control));
        Ok(control)
    }

    pub(crate) fn finish_owned_task(
        &self,
        control: &Arc<OwnedTaskControl>,
        result: Result<(), OrchError>,
    ) {
        control.complete(result);
        if let Ok(mut tasks) = self.owned_tasks.lock() {
            tasks.retain(|_, current| !Arc::ptr_eq(current, control));
        }
    }

    fn bind_owned_task(&self, id: Uuid, control: &OwnedTaskControl) -> Result<(), OrchError> {
        let mut tasks = self
            .owned_tasks
            .lock()
            .map_err(|_| OrchError::Internal("owned task lock poisoned".into()))?;
        let existing_key = tasks.iter().find_map(|(existing_id, current)| {
            std::ptr::eq(Arc::as_ptr(current), control).then_some(*existing_id)
        });
        let Some(existing_key) = existing_key else {
            // Unit-level supervisor tests may exercise warm transfer without an
            // API-owned task. Production callers always register first.
            return Ok(());
        };
        if existing_key == id {
            return Ok(());
        }
        if tasks.contains_key(&id) {
            return Err(OrchError::Conflict(format!(
                "VM {id} already has a supervisor-owned lifecycle task"
            )));
        }
        let control = tasks
            .remove(&existing_key)
            .expect("existing owned task key was checked");
        tasks.insert(id, control);
        Ok(())
    }

    pub(crate) async fn run_owned_task<T, F, Fut>(
        self: &Arc<Self>,
        id: Uuid,
        purpose: SpawnPurpose,
        operation: F,
    ) -> Result<T, OrchError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<OwnedTaskControl>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, OrchError>> + Send + 'static,
    {
        let control = self.begin_owned_task(id, purpose)?;
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let worker_control = Arc::clone(&control);
        let worker = tokio::spawn(async move { operation(worker_control).await });
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            let result = match worker.await {
                Ok(result) => result,
                Err(error) => Err(supervisor.cleanup_registered_boot_failure(
                    id,
                    OrchError::Internal(format!(
                        "supervisor-owned lifecycle worker failed: {error}"
                    )),
                )),
            };
            let completion = match &result {
                Ok(_) => Ok(()),
                Err(_) if control.is_cancelled() && control.terminal_converged() => Ok(()),
                Err(error) => Err(OrchError::Internal(error.to_string())),
            };
            supervisor.finish_owned_task(&control, completion);
            let _ = result_tx.send(result);
        });
        result_rx.await.map_err(|_| {
            OrchError::Internal("supervisor-owned lifecycle worker ended before reporting".into())
        })?
    }

    #[cfg(test)]
    pub(crate) fn has_owned_task(&self, id: Uuid) -> bool {
        self.owned_tasks
            .lock()
            .map(|tasks| tasks.contains_key(&id))
            .unwrap_or(true)
    }

    fn request_boot_cancellation(&self, id: Uuid) {
        if let Ok(booting) = self.booting.lock() {
            if let Some(booting_vm) = booting.get(&id) {
                booting_vm.control.request_cancellation();
            }
        }
    }

    pub(crate) fn cancel_and_wait_owned_task(&self, id: Uuid) -> Result<bool, OrchError> {
        let control = self
            .owned_tasks
            .lock()
            .map_err(|_| OrchError::Internal("owned task lock poisoned".into()))?
            .get(&id)
            .cloned();
        let Some(control) = control else {
            return Ok(false);
        };
        control.request_cancellation();
        self.request_boot_cancellation(id);
        control.wait_for_completion()?;
        Ok(control.terminal_converged())
    }

    fn signal_owned_tasks(&self) -> Result<Vec<(Uuid, Arc<OwnedTaskControl>)>, OrchError> {
        let tasks = self
            .owned_tasks
            .lock()
            .map_err(|_| OrchError::Internal("owned task lock poisoned".into()))?
            .iter()
            .map(|(id, control)| (*id, Arc::clone(control)))
            .collect::<Vec<_>>();
        for (id, control) in &tasks {
            control.request_cancellation();
            self.request_boot_cancellation(*id);
        }
        Ok(tasks)
    }

    fn wait_for_owned_tasks(
        &self,
        tasks: Vec<(Uuid, Arc<OwnedTaskControl>)>,
    ) -> Vec<Result<(), OrchError>> {
        tasks
            .into_iter()
            .map(|(_, control)| control.wait_for_completion())
            .collect()
    }

    pub(crate) fn cancel_and_wait_all_owned_tasks(&self) -> Result<(), OrchError> {
        let outcomes = self.wait_for_owned_tasks(self.signal_owned_tasks()?);
        let failures = outcomes
            .into_iter()
            .filter_map(Result::err)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(OrchError::Internal(failures.join("; ")))
        }
    }

    #[cfg(test)]
    fn pause_after_spawn_before_registry_attachment_for_test(&self) -> SpawnAttachmentPause {
        let pause = SpawnAttachmentPause::default();
        *self.spawn_attachment_pause.lock().unwrap() = Some(pause.clone());
        pause
    }

    #[cfg(test)]
    fn wait_after_spawn_before_registry_attachment(&self) {
        let pause = self
            .spawn_attachment_pause
            .lock()
            .ok()
            .and_then(|pause| pause.clone());
        if let Some(pause) = pause {
            pause.wait_after_spawn();
        }
    }

    #[cfg(test)]
    fn track_booting(
        &self,
        id: Uuid,
        socket_path: PathBuf,
        process: ManagedProcess,
        purpose: SpawnPurpose,
    ) -> Result<Arc<BootControl>, OrchError> {
        let control = Arc::new(BootControl::new(purpose));
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
        self.scheduler.release(id);
    }

    pub(crate) fn release_reservation_after_terminal(&self, id: Uuid) -> Result<(), OrchError> {
        self.scheduler.release(id);
        Ok(())
    }

    /// Complete a registered live boot that failed before any VMM work began.
    /// The lifecycle owner performs the durable Error/Stopped transition and
    /// releases its reservation afterwards, so this only removes the boot entry.
    pub(crate) async fn abort_unstarted_boot(&self, ticket: &BootTicket) {
        let _gate = self.boot_gate.lock().await;
        let is_current = self
            .booting
            .lock()
            .ok()
            .and_then(|booting| booting.get(&ticket.id).cloned())
            .is_some_and(|booting_vm| Arc::ptr_eq(&booting_vm.control, &ticket.control));
        if is_current {
            self.complete_booting(ticket.id, &ticket.control, Ok(()));
            if ticket.purpose == SpawnPurpose::Refill {
                self.release_reservation_after_cleanup(ticket.id);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn reserve_existing_for_test(&self, id: Uuid) {
        self.scheduler
            .try_reserve(id, ResourceShape::new(1, 1))
            .unwrap();
    }

    #[cfg(test)]
    pub(crate) fn seed_warm_for_test(
        &self,
        id: Uuid,
        spec: VmSpawnConfig,
    ) -> Result<(), OrchError> {
        self.scheduler
            .try_reserve(id, spec.resource_shape())
            .unwrap();
        let process = ManagedProcess::new(
            Command::new("true")
                .spawn()
                .map_err(|error| OrchError::Internal(format!("spawn warm test VMM: {error}")))?,
        );
        self.warm
            .lock()
            .map_err(|_| OrchError::Internal("warm lock poisoned".into()))?
            .push_back(WarmVm {
                id,
                vm: RunningVm::new(
                    process.pid,
                    PathBuf::from(format!("warm-test-{id}.sock")),
                    process,
                    None,
                ),
                spec,
            });
        Ok(())
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
            if control.purpose == SpawnPurpose::Refill {
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

    pub(crate) fn cleanup_boot_join_failure(
        &self,
        id: Uuid,
        context: &str,
        join_error: tokio::task::JoinError,
    ) -> OrchError {
        self.cleanup_registered_boot_failure(
            id,
            OrchError::Internal(format!("{context}: {join_error}")),
        )
    }

    fn cleanup_registered_boot_failure(&self, id: Uuid, cause: OrchError) -> OrchError {
        let booting = self
            .booting
            .lock()
            .ok()
            .and_then(|booting| booting.get(&id).cloned());
        let Some(booting) = booting else {
            return cause;
        };
        booting.control.request_cancellation();
        match self.retry_booting_cleanup(id, &booting) {
            Ok(()) => {
                self.complete_booting(id, &booting.control, Ok(()));
                if booting.purpose == SpawnPurpose::Refill {
                    self.release_reservation_after_cleanup(id);
                }
                cause
            }
            Err(cleanup_error) => {
                self.complete_booting(
                    id,
                    &booting.control,
                    Err(OrchError::Internal(format!(
                        "{cause}; cleanup retained resources: {cleanup_error}"
                    ))),
                );
                OrchError::Internal(format!(
                    "{cause}; cleanup retained booting VM {id} for retry: {cleanup_error}"
                ))
            }
        }
    }

    /// The supervisor-owned lifecycle worker observed cancellation after the
    /// synchronous boot completed but before publication transferred ownership.
    /// Clean the attached VMM/network before allowing terminal compensation.
    pub(crate) fn discard_booted_vm(&self, booted: BootedVm) -> OrchError {
        self.cleanup_boot_failure(
            booted.id,
            &booted.control,
            &booted.vm,
            self.shutdown_error(),
        )
    }

    pub(crate) async fn publish_running_with<T, F, Fut>(
        self: &Arc<Self>,
        booted: BootedVm,
        publish_lifecycle: F,
    ) -> Result<T, OrchError>
    where
        T: Send,
        F: FnOnce(u32, PathBuf) -> Fut + Send,
        Fut: std::future::Future<Output = Result<T, PublicationFailure>> + Send,
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

        let (published, retained_error) = match publish_lifecycle(pid, socket_path.clone()).await {
            Ok(published) => (Some(published), None),
            Err(PublicationFailure(error)) => (None, Some(error)),
        };

        let mut running = match self.running.lock() {
            Ok(running) => running,
            Err(_) => {
                drop(gate);
                return Err(OrchError::Internal(
                    "supervisor lock poisoned after lifecycle publication; boot retained for retry"
                        .into(),
                ));
            }
        };
        let mut booting = match self.booting.lock() {
            Ok(booting) => booting,
            Err(_) => {
                drop(running);
                drop(gate);
                return Err(OrchError::Internal(
                    "supervisor booting lock poisoned after lifecycle publication; boot retained for retry"
                        .into(),
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
        drop(booting);
        drop(running);
        drop(gate);
        match retained_error {
            Some(error) => Err(error),
            None => Ok(published.expect("successful publication has a result")),
        }
    }

    async fn publish_warm(
        self: &Arc<Self>,
        booted: BootedVm,
        spec: VmSpawnConfig,
    ) -> Result<(), OrchError> {
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
        drop(booting);
        drop(warm);
        drop(gate);
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
                if control.purpose == SpawnPurpose::Refill {
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
        let mut delay = SOCKET_WAIT_INITIAL;
        while start.elapsed() < Duration::from_secs(30) {
            if control.is_cancelled() || self.is_shutting_down() {
                return Err(self.shutdown_error());
            }
            if socket_path.exists() {
                return Ok(());
            }
            std::thread::sleep(delay);
            delay = next_socket_wait_delay(delay);
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
                    &RunningVm::new(
                        process.pid,
                        booting_vm.socket_path.clone(),
                        process.clone(),
                        None,
                    ),
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

    fn spawn_server_for_boot(&self, ticket: &BootTicket) -> Result<RunningVm, OrchError> {
        let id = ticket.id;
        let socket_path = self.socket_path_for(id);
        let _ = std::fs::remove_file(&socket_path);
        let cgroup_args = self.cgroup_args(id, ticket.shape)?;
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
            // Preserve VMM diagnostics without per-VM log-pump threads. The
            // service manager owns bounded collection/rotation for taritd's
            // inherited stdout and stderr.
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                drop(boot_gate);
                self.complete_booting(id, &ticket.control, Ok(()));
                if ticket.control.purpose == SpawnPurpose::Refill {
                    self.release_reservation_after_cleanup(id);
                }
                return Err(OrchError::Internal(format!("spawn vmm: {error}")));
            }
        };

        let process = ManagedProcess::new(child);
        let pid = process.pid;
        // Attach cleanup ownership before observing cancellation. A cancelled
        // boot must remain retryable if its first teardown attempt fails.
        #[cfg(test)]
        self.wait_after_spawn_before_registry_attachment();
        let attached = self
            .booting
            .lock()
            .map_err(|_| OrchError::Internal("supervisor booting lock poisoned".into()))
            .and_then(|mut booting| {
                let booting_vm = booting.get_mut(&id).ok_or_else(|| {
                    OrchError::Internal(format!("boot registration disappeared for VM {id}"))
                })?;
                if !Arc::ptr_eq(&booting_vm.control, &ticket.control) {
                    return Err(OrchError::Conflict(format!(
                        "boot registration changed for VM {id}"
                    )));
                }
                booting_vm.socket_path = socket_path.clone();
                booting_vm.process = Some(process.clone());
                if ticket.control.is_cancelled() || self.is_shutting_down() {
                    Err(self.shutdown_error())
                } else {
                    Ok(())
                }
            });
        drop(boot_gate);
        let vm = RunningVm::new(pid, socket_path, process, None);
        if let Err(error) = attached {
            return Err(self.cleanup_boot_failure(id, &ticket.control, &vm, error));
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
        let base_vm = self.spawn_server_for_boot(&ticket)?;
        if let Err(error) =
            self.wait_for_socket_or_cancellation(&base_vm.socket_path, &ticket.control)
        {
            return Err(self.cleanup_boot_failure(id, &ticket.control, &base_vm, error));
        }
        // `vmm serve` creates and joins its exact per-VM cgroup before binding
        // the control socket. Apply refill priority only after that readiness
        // boundary so an ENOENT cannot silently leave refills at normal weight.
        if ticket.purpose == SpawnPurpose::Refill {
            if let Err(error) = self.configure_refill_cgroup(id, base_vm.pid) {
                return Err(self.cleanup_boot_failure(id, &ticket.control, &base_vm, error));
            }
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

        let overlay = self.overlay_path_for_config(id, vm_config);
        let vmm_config = build_vmm_config(vm_config, vm.net.as_ref(), overlay);
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
        let booted = self.boot_vm(ticket, &vm_config)?;
        self.require_booted_guest_ready(booted, "cold boot")
    }

    fn require_booted_guest_ready(
        &self,
        booted: BootedVm,
        operation: &str,
    ) -> Result<BootedVm, OrchError> {
        if let Err(error) = self.await_ready(&booted.vm.socket_path, &booted.control) {
            return Err(self.cleanup_boot_failure(
                booted.id,
                &booted.control,
                &booted.vm,
                OrchError::Vmm(format!("{operation} readiness: {error}")),
            ));
        }
        Ok(booted)
    }

    /// Restore a VM from a node-local snapshot file: spawn a fresh `vmm serve`,
    /// send Restore, and register the resumed VM. Host network bindings are
    /// always replaced with a fresh allocation; saved tap/IP bindings are never
    /// reused across VM incarnations.
    fn spawn_and_restore(
        &self,
        ticket: BootTicket,
        snapshot_path: &str,
        overlay: RestoreOverlay,
        shape: ResourceShape,
    ) -> Result<BootedVm, OrchError> {
        let id = ticket.id;
        debug_assert_eq!(ticket.shape, shape);
        let base_vm = self.spawn_server_for_boot(&ticket)?;
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

        // The Restore RPC receives only the new UUID-scoped destination. For a
        // durable snapshot this destination is seeded from SnapshotRecord's
        // owned disk artifact; the source overlay path serialized in RAM state
        // is never reopened.
        let overlay = match overlay {
            RestoreOverlay::None => None,
            RestoreOverlay::Fresh => Some(self.overlay_path_for(id)),
            RestoreOverlay::Seeded(source) => {
                let destination = PathBuf::from(self.overlay_path_for(id));
                if let Err(error) = copy_private_artifact(&source, &destination) {
                    return Err(self.cleanup_boot_failure(
                        id,
                        &ticket.control,
                        &base_vm,
                        OrchError::Internal(format!("seed private restore disk upper: {error}")),
                    ));
                }
                Some(destination.display().to_string())
            }
        };

        let net_alloc = match &self.net {
            Some(provisioner) => match provisioner.provision(id) {
                Ok(allocation) => Some(allocation),
                Err(error) => {
                    return Err(self.cleanup_boot_failure(id, &ticket.control, &base_vm, error));
                }
            },
            None => None,
        };
        let vm = RunningVm {
            net: net_alloc,
            ..base_vm
        };
        let net_override = vm
            .net
            .as_ref()
            .map(net_config_for_allocation)
            .into_iter()
            .collect::<Vec<_>>();

        let client = VmmClient::new(&vm.socket_path);
        if let Err(e) =
            client.restore_with_network_override(snapshot_path, overlay.clone(), Some(net_override))
        {
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
        snapshot_overlay_path: Option<String>,
        shape: ResourceShape,
    ) -> Result<BootedVm, OrchError> {
        let overlay = snapshot_overlay_path
            .map(PathBuf::from)
            .map(RestoreOverlay::Seeded)
            .unwrap_or(RestoreOverlay::None);
        let booted = self.spawn_and_restore(ticket, &snapshot_path, overlay, shape)?;
        let booted = self.require_booted_guest_ready(booted, "restore")?;
        self.reconfigure_restored_guest_network(booted)
    }

    fn reconfigure_restored_guest_network(&self, booted: BootedVm) -> Result<BootedVm, OrchError> {
        let Some(net) = booted.vm.net.as_ref() else {
            return Ok(booted);
        };
        let command = restored_network_rebind_command(net);
        let client = VmmClient::new(&booted.vm.socket_path);
        match client.exec(&command, 5_000) {
            Ok((0, _, _, _)) => Ok(booted),
            Ok((status, _, stderr, _)) => Err(self.cleanup_boot_failure(
                booted.id,
                &booted.control,
                &booted.vm,
                OrchError::Vmm(format!(
                    "restore network reconfiguration exited {status}: {stderr}"
                )),
            )),
            Err(error) => Err(self.cleanup_boot_failure(
                booted.id,
                &booted.control,
                &booted.vm,
                OrchError::Vmm(format!("restore network reconfiguration: {error}")),
            )),
        }
    }

    /// Boot one warm-pool VM of `class` and park it in the warm queue. The boot
    /// happens without the warm lock held; only the final enqueue takes it.
    /// Block until the guest agent can actually run a command, so we never park a
    /// still-booting VM. A freshly-parked, not-yet-ready VM handed out during a
    /// burst blocks the caller for seconds on its first agent dial (the burst
    /// p95 tail). Bounded; parks anyway on timeout so a wedged guest can't stall
    /// replenishment forever.
    fn await_ready(&self, socket: &Path, control: &BootControl) -> Result<(), OrchError> {
        wait_for_guest_ready(
            readiness_timeout(ReadinessCheck::Boot),
            || {
                if boot_can_publish(control, self.is_shutting_down()) {
                    Ok(())
                } else {
                    Err(self.shutdown_error())
                }
            },
            |remaining| {
                let request_timeout = readiness_request_timeout(remaining);
                let exec_timeout_ms = readiness_exec_timeout_ms(request_timeout);
                let client = VmmClient::new(socket)
                    .with_connect_timeout(request_timeout)
                    .with_request_timeout(request_timeout);
                match client.exec("true", exec_timeout_ms) {
                    Ok((0, _, _, _)) => Ok(true),
                    Ok((code, _, _, _)) => {
                        Err(format!("readiness command exited with status {code}"))
                    }
                    Err(error) => Err(error.to_string()),
                }
            },
        )
        .map_err(|error| match error {
            ReadinessWaitError::Cancelled(error) => error,
            ReadinessWaitError::TimedOut(last) => OrchError::Vmm(format!(
                "guest agent never became ready at {}: {last}",
                socket.display()
            )),
        })
    }

    pub(crate) async fn spawn_warm(self: Arc<Self>, class: WarmClass) -> Result<(), OrchError> {
        let id = Uuid::new_v4();
        let worker = Arc::clone(&self);
        self.run_owned_task(id, SpawnPurpose::Refill, move |task| async move {
            worker.spawn_warm_owned(id, class, &task).await
        })
        .await
    }

    async fn spawn_warm_owned(
        self: Arc<Self>,
        id: Uuid,
        class: WarmClass,
        task: &OwnedTaskControl,
    ) -> Result<(), OrchError> {
        let spec = VmSpawnConfig::from_warm_class(&self.config, &class);
        let ticket = self
            .begin_boot_with_registration(
                id,
                SpawnPurpose::Refill,
                spec.resource_shape(),
                || async { Ok(()) },
            )
            .await?;
        if task.is_cancelled() {
            self.abort_unstarted_boot(&ticket).await;
            task.mark_terminal_converged();
            return Err(self.shutdown_error());
        }
        let worker = Arc::clone(&self);
        let worker_spec = spec.clone();
        let booted =
            tokio::task::spawn_blocking(move || worker.boot_vm(ticket, &worker_spec)).await;
        let booted = match booted {
            Ok(Ok(booted)) => booted,
            Ok(Err(error)) => {
                if task.is_cancelled() && !self.has_retained_boot(id) {
                    task.mark_terminal_converged();
                }
                return Err(error);
            }
            Err(error) => {
                return Err(self.cleanup_boot_join_failure(id, "warm boot task", error));
            }
        };
        if task.is_cancelled() {
            let error = self.discard_booted_vm(booted);
            if !self.has_retained_boot(id) {
                task.mark_terminal_converged();
            }
            return Err(error);
        }
        let socket_path = booted.vm.socket_path.clone();
        let boot_control = Arc::clone(&booted.control);
        let worker = Arc::clone(&self);
        let ready = match tokio::task::spawn_blocking(move || {
            worker.await_ready(&socket_path, &boot_control)
        })
        .await
        {
            Ok(ready) => ready,
            Err(error) => {
                return Err(self.cleanup_boot_failure(
                    id,
                    &booted.control,
                    &booted.vm,
                    OrchError::Internal(format!("warm readiness task: {error}")),
                ));
            }
        };
        if let Err(error) = ready {
            let error = self.cleanup_boot_failure(id, &booted.control, &booted.vm, error);
            if task.is_cancelled() && !self.has_retained_boot(id) {
                task.mark_terminal_converged();
            }
            return Err(error);
        }
        let result = self.publish_warm(booted, spec).await;
        if task.is_cancelled() && !self.has_retained_boot(id) {
            task.mark_terminal_converged();
        }
        result
    }

    /// Cold-boot one VM for `class`, wait until it is ready, take a full golden
    /// snapshot, then tear down the builder VM. Runtime warm capacity is filled
    /// by restoring clones from the returned snapshot.
    pub(crate) async fn create_golden(
        self: Arc<Self>,
        class: WarmClass,
    ) -> Result<String, OrchError> {
        let id = Uuid::new_v4();
        let worker = Arc::clone(&self);
        self.run_owned_task(id, SpawnPurpose::Refill, move |task| async move {
            worker.create_golden_owned(id, class, &task).await
        })
        .await
    }

    async fn create_golden_owned(
        self: Arc<Self>,
        id: Uuid,
        class: WarmClass,
        task: &OwnedTaskControl,
    ) -> Result<String, OrchError> {
        let spec = VmSpawnConfig::from_warm_class(&self.config, &class);
        let ticket = self
            .begin_boot_with_registration(
                id,
                SpawnPurpose::Refill,
                spec.resource_shape(),
                || async { Ok(()) },
            )
            .await?;
        if task.is_cancelled() {
            self.abort_unstarted_boot(&ticket).await;
            task.mark_terminal_converged();
            return Err(self.shutdown_error());
        }
        let worker = Arc::clone(&self);
        let worker_spec = spec.clone();
        let booted =
            tokio::task::spawn_blocking(move || worker.boot_vm(ticket, &worker_spec)).await;
        let booted = match booted {
            Ok(Ok(booted)) => booted,
            Ok(Err(error)) => {
                if task.is_cancelled() && !self.has_retained_boot(id) {
                    task.mark_terminal_converged();
                }
                return Err(error);
            }
            Err(error) => {
                return Err(self.cleanup_boot_join_failure(id, "golden boot task", error));
            }
        };
        if task.is_cancelled() {
            let error = self.discard_booted_vm(booted);
            if !self.has_retained_boot(id) {
                task.mark_terminal_converged();
            }
            return Err(error);
        }
        let socket_path = booted.vm.socket_path.clone();
        let boot_control = Arc::clone(&booted.control);
        let worker = Arc::clone(&self);
        let ready = match tokio::task::spawn_blocking(move || {
            worker.await_ready(&socket_path, &boot_control)
        })
        .await
        {
            Ok(ready) => ready,
            Err(error) => {
                return Err(self.cleanup_boot_failure(
                    id,
                    &booted.control,
                    &booted.vm,
                    OrchError::Internal(format!("golden readiness task: {error}")),
                ));
            }
        };
        if let Err(error) = ready {
            let error = self.cleanup_boot_failure(id, &booted.control, &booted.vm, error);
            if task.is_cancelled() && !self.has_retained_boot(id) {
                task.mark_terminal_converged();
            }
            return Err(error);
        }
        if !boot_can_publish(&booted.control, self.is_shutting_down()) {
            let error =
                self.cleanup_boot_failure(id, &booted.control, &booted.vm, self.shutdown_error());
            if task.is_cancelled() && !self.has_retained_boot(id) {
                task.mark_terminal_converged();
            }
            return Err(error);
        }
        let socket_path = booted.vm.socket_path.clone();
        let snapshot_path = match tokio::task::spawn_blocking(move || {
            VmmClient::new(&socket_path)
                .with_request_timeout(LIFECYCLE_OP_TIMEOUT)
                .snapshot_unreleased(false)
                .map_err(|error| OrchError::Vmm(format!("snapshot golden: {error}")))
        })
        .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(self.cleanup_boot_failure(
                    id,
                    &booted.control,
                    &booted.vm,
                    OrchError::Internal(format!("golden snapshot task: {error}")),
                ));
            }
        }
        .map_err(|error| self.cleanup_boot_failure(id, &booted.control, &booted.vm, error))?;
        let mut artifacts = self.capture_golden_artifacts(
            &snapshot_path,
            self.overlay_path_for_config(id, &spec).as_deref(),
        )?;
        if task.is_cancelled() {
            cleanup_golden_artifacts(artifacts);
            return Err(self.discard_booted_vm(booted));
        }
        let client = VmmClient::new(&booted.vm.socket_path);
        for artifact in &artifacts {
            let path = artifact.path().display().to_string();
            let identity = artifact.identity();
            if let Err(error) = client.release_scratch(&path, identity) {
                cleanup_golden_artifacts(artifacts);
                return Err(self.cleanup_boot_failure(
                    id,
                    &booted.control,
                    &booted.vm,
                    OrchError::Vmm(format!("release golden scratch {path}: {error}")),
                ));
            }
        }
        let artifact_keys = artifacts
            .iter()
            .map(|artifact| (artifact.path.clone(), artifact.identity()))
            .collect::<Vec<_>>();
        self.remember_golden_artifacts(&mut artifacts)?;
        if task.is_cancelled() {
            cleanup_golden_artifacts(self.take_golden_artifacts(&artifact_keys));
            return Err(self.discard_booted_vm(booted));
        }
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
        let worker = Arc::clone(&self);
        self.run_owned_task(id, SpawnPurpose::Refill, move |task| async move {
            worker
                .spawn_warm_restore_owned(id, class, snapshot_path, &task)
                .await
        })
        .await
    }

    async fn spawn_warm_restore_owned(
        self: Arc<Self>,
        id: Uuid,
        class: WarmClass,
        snapshot_path: String,
        task: &OwnedTaskControl,
    ) -> Result<(), OrchError> {
        let spec = VmSpawnConfig::from_warm_class(&self.config, &class);
        let overlay = self.overlay_path_for_config(id, &spec);
        let ticket = self
            .begin_boot_with_registration(
                id,
                SpawnPurpose::Refill,
                spec.resource_shape(),
                || async { Ok(()) },
            )
            .await?;
        if task.is_cancelled() {
            self.abort_unstarted_boot(&ticket).await;
            task.mark_terminal_converged();
            return Err(self.shutdown_error());
        }
        let worker = Arc::clone(&self);
        let shape = spec.resource_shape();
        let booted = tokio::task::spawn_blocking(move || {
            let overlay = if overlay.is_some() {
                RestoreOverlay::Fresh
            } else {
                RestoreOverlay::None
            };
            worker.spawn_and_restore(ticket, &snapshot_path, overlay, shape)
        })
        .await;
        let booted = match booted {
            Ok(Ok(booted)) => booted,
            Ok(Err(error)) => {
                if task.is_cancelled() && !self.has_retained_boot(id) {
                    task.mark_terminal_converged();
                }
                return Err(error);
            }
            Err(error) => {
                return Err(self.cleanup_boot_join_failure(id, "warm restore task", error));
            }
        };
        if task.is_cancelled() {
            let error = self.discard_booted_vm(booted);
            if !self.has_retained_boot(id) {
                task.mark_terminal_converged();
            }
            return Err(error);
        }
        let socket_path = booted.vm.socket_path.clone();
        let boot_control = Arc::clone(&booted.control);
        let worker = Arc::clone(&self);
        let ready = match tokio::task::spawn_blocking(move || {
            worker.await_ready(&socket_path, &boot_control)
        })
        .await
        {
            Ok(ready) => ready,
            Err(error) => {
                return Err(self.cleanup_boot_failure(
                    id,
                    &booted.control,
                    &booted.vm,
                    OrchError::Internal(format!("warm restore readiness task: {error}")),
                ));
            }
        };
        if let Err(error) = ready {
            let error = self.cleanup_boot_failure(id, &booted.control, &booted.vm, error);
            if task.is_cancelled() && !self.has_retained_boot(id) {
                task.mark_terminal_converged();
            }
            return Err(error);
        }
        let booted = match self.reconfigure_restored_guest_network(booted) {
            Ok(booted) => booted,
            Err(error) => {
                if task.is_cancelled() && !self.has_retained_boot(id) {
                    task.mark_terminal_converged();
                }
                return Err(error);
            }
        };
        let result = self.publish_warm(booted, spec).await;
        if task.is_cancelled() && !self.has_retained_boot(id) {
            task.mark_terminal_converged();
        }
        result
    }

    /// Claim and publish a matching warm VM under the same lifecycle gate as a
    /// cold boot. A shutdown/delete either waits for this publication then tears
    /// it down, or wins before it starts; no write-behind warm visibility exists.
    pub(crate) async fn take_warm_with_publication<T, R, RFut, F, Fut>(
        &self,
        want: &VmSpawnConfig,
        task: &OwnedTaskControl,
        register_lifecycle: R,
        publish_lifecycle: F,
    ) -> Result<WarmClaimOutcome<T>, OrchError>
    where
        T: Send,
        R: FnOnce(Uuid) -> RFut + Send,
        RFut: std::future::Future<Output = Result<(), OrchError>> + Send,
        F: FnOnce(Uuid, u32, PathBuf) -> Fut + Send,
        Fut: std::future::Future<Output = Result<T, PublicationFailure>> + Send,
    {
        let _gate = self.boot_gate.lock().await;
        if self.is_shutting_down() || task.is_cancelled() {
            return Ok(WarmClaimOutcome::NoMatch);
        }
        let candidate_id = {
            let warm = self
                .warm
                .lock()
                .map_err(|_| OrchError::Internal("warm lock poisoned".into()))?;
            let Some(warm_vm) = warm.iter().find(|warm_vm| &warm_vm.spec == want) else {
                return Ok(WarmClaimOutcome::NoMatch);
            };
            warm_vm.id
        };
        self.bind_owned_task(candidate_id, task)?;
        if let Err(error) = register_lifecycle(candidate_id).await {
            return Ok(WarmClaimOutcome::PreRuntimeFailure(error));
        }
        if task.is_cancelled() {
            return Ok(WarmClaimOutcome::PreRuntimeFailure(self.shutdown_error()));
        }
        let taken = {
            let mut warm = self
                .warm
                .lock()
                .map_err(|_| OrchError::Internal("warm lock poisoned".into()))?;
            let Some(pos) = warm.iter().position(|warm_vm| warm_vm.id == candidate_id) else {
                return Err(OrchError::Internal(format!(
                    "registered warm VM {candidate_id} disappeared before transfer"
                )));
            };
            warm.remove(pos).expect("warm position was selected")
        };
        let pid = taken.vm.pid;
        let socket = taken.vm.socket_path.clone();
        self.configure_leased_cgroup(candidate_id, pid);
        let WarmVm { id, vm, .. } = taken;
        self.running
            .lock()
            .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))
            .map(|mut running| {
                running.insert(id, vm);
            })?;
        let published = match publish_lifecycle(id, pid, socket).await {
            Ok(published) => published,
            Err(PublicationFailure(error)) => {
                return Ok(WarmClaimOutcome::RetainedPublicationFailure(error));
            }
        };
        if task.is_cancelled() {
            return Ok(WarmClaimOutcome::RetainedPublicationFailure(
                self.shutdown_error(),
            ));
        }
        Ok(WarmClaimOutcome::Published(published))
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

    fn capture_golden_artifacts(
        &self,
        snapshot_path: &str,
        overlay_path: Option<&str>,
    ) -> Result<Vec<OwnedArtifact>, OrchError> {
        let mut artifacts = vec![OwnedArtifact::capture(snapshot_path)
            .map_err(|error| OrchError::Internal(format!("capture golden snapshot: {error}")))?];
        if let Some(overlay_path) = overlay_path {
            artifacts.push(OwnedArtifact::capture(overlay_path).map_err(|error| {
                OrchError::Internal(format!("capture golden overlay: {error}"))
            })?);
        }
        Ok(artifacts)
    }

    fn remember_golden_artifacts(
        &self,
        artifacts: &mut Vec<OwnedArtifact>,
    ) -> Result<(), OrchError> {
        let mut registered = self
            .golden_artifacts
            .lock()
            .map_err(|_| OrchError::Internal("golden artifact lock poisoned".into()))?;
        registered.append(artifacts);
        Ok(())
    }

    fn take_golden_artifacts(&self, keys: &[(PathBuf, ScratchIdentity)]) -> Vec<OwnedArtifact> {
        let mut registered = self.golden_artifacts.lock().unwrap_or_else(|poisoned| {
            tracing::error!("golden artifact lock poisoned during cancellation cleanup");
            poisoned.into_inner()
        });
        take_matching_artifacts(&mut registered, keys)
    }

    fn owns_golden_artifact(&self, path: &Path) -> Result<bool, OrchError> {
        Ok(self
            .golden_artifacts
            .lock()
            .map_err(|_| OrchError::Internal("golden artifact lock poisoned".into()))?
            .iter()
            .any(|artifact| artifact.path() == path))
    }

    /// Re-adopt VMs that were left running when this taritd instance restarted
    /// (reap disabled). `NetProvisioner` recovery already reconciled their
    /// network policy; this restores the control-plane handle so exec, pause,
    /// snapshot, and delete work again. VMs whose VMM process is gone or does
    /// not match the persisted socket (PID reuse), whose control socket is
    /// missing, or whose network allocation cannot be recovered are torn down
    /// and their ids returned so the caller can mark them terminal. The API must
    /// never report an uncontrollable VM as running.
    pub async fn readopt_running_vms(
        self: &Arc<Self>,
        records: &mut [VmRecord],
    ) -> Result<Vec<Uuid>, String> {
        let mut failed = Vec::new();
        for record in records {
            match self.readopt_one(record).await {
                Ok(true) => {
                    tracing::info!(vm = %record.id, pid = record.pid.unwrap_or(0),
                        "re-adopted running VM after restart");
                }
                Ok(false) => {}
                Err(ReadoptFailure::Unadoptable(reason)) => {
                    tracing::warn!(vm = %record.id, reason = %reason,
                        "cannot re-adopt VM after restart; tearing down its network and marking it failed");
                    if let Some(net) = &self.net {
                        if let Err(error) = net.teardown_vm_id(record.id) {
                            return Err(format!(
                                "tear down network for unadoptable VM {}: {error}",
                                record.id
                            ));
                        }
                    }
                    failed.push(record.id);
                }
                Err(ReadoptFailure::Fatal(reason)) => return Err(reason),
            }
        }
        Ok(failed)
    }

    /// Attempt to re-adopt a single persisted VM. Returns `Ok(true)` on success,
    /// `Ok(false)` when the record is not a locally running VM (nothing to do),
    /// and `Err` when the VM existed here but can no longer be controlled. When a
    /// VMM is positively identified as ours but cannot be managed, it is
    /// terminated through its pinned pidfd so no unmanaged VMM is left running.
    async fn readopt_one(self: &Arc<Self>, record: &mut VmRecord) -> Result<bool, ReadoptFailure> {
        if record.host_id != self.config.host_id
            || !matches!(
                record.status,
                VmStatus::Running | VmStatus::Paused | VmStatus::Suspended
            )
        {
            return Ok(false);
        }
        let pid = record
            .pid
            .ok_or_else(|| ReadoptFailure::Unadoptable("persisted VM has no PID".into()))?;
        let socket_path = PathBuf::from(record.socket_path.as_deref().ok_or_else(|| {
            ReadoptFailure::Unadoptable("persisted VM has no control socket path".into())
        })?);
        // Pin the process before any /proc inspection so the PID cannot be
        // recycled between verification and adoption. If the process is already
        // gone there is nothing to adopt.
        let pidfd = pidfd_open(pid).map_err(|error| {
            ReadoptFailure::Unadoptable(format!("pin VMM {pid} for adoption: {error}"))
        })?;
        // Confirm identity while pinned. A failure here means the process is not
        // our VMM (or is already gone), so it must not be signalled.
        verify_live_vmm(pid, &socket_path).map_err(ReadoptFailure::Unadoptable)?;
        let process = ManagedProcess::adopted(pid, pidfd);
        // Identity is confirmed. Any failure below leaves a live, taritd-owned
        // VMM that the control plane cannot manage, so terminate it through the
        // pinned pidfd before marking the VM terminal.
        let recovered: Result<Option<NetAlloc>, String> = 'recover: {
            if !socket_path.exists() {
                break 'recover Err(format!(
                    "control socket {} is absent",
                    socket_path.display()
                ));
            }
            match &self.net {
                None => Ok(None),
                Some(provisioner) => match provisioner.allocation_for_vm(record.id) {
                    Err(error) => Err(error.to_string()),
                    Ok(None) => {
                        Err("network is enabled but the VM has no recovered allocation".to_string())
                    }
                    Ok(Some(alloc)) => Ok(Some(alloc)),
                },
            }
        };
        let net = match recovered {
            Ok(net) => net,
            Err(reason) => {
                let vm = RunningVm::new(pid, socket_path, process, None);
                if let Err(error) = self.teardown_vm(record.id, &vm) {
                    return Err(ReadoptFailure::Fatal(format!(
                        "{reason}; clean up identified VMM {} after adoption failed: {error}",
                        record.id
                    )));
                }
                return Err(ReadoptFailure::Unadoptable(reason));
            }
        };
        let vm = RunningVm::new(pid, socket_path, process, net);
        {
            let gate = self.boot_gate.lock().await;
            if let Err(error) = self.scheduler.reserve_existing(
                record.id,
                ResourceShape::new(record.vcpus, record.memory_mib),
            ) {
                drop(gate);
                return match self.teardown_vm(record.id, &vm) {
                    Ok(()) => Err(ReadoptFailure::Unadoptable(format!(
                        "account re-adopted VM resources: {error:?}"
                    ))),
                    Err(cleanup_error) => Err(ReadoptFailure::Fatal(format!(
                        "account re-adopted VM resources: {error:?}; clean up identified VMM {}: {cleanup_error}",
                        record.id
                    ))),
                };
            }
            match self.running.lock() {
                Ok(mut running) => {
                    running.insert(record.id, vm);
                }
                Err(_) => {
                    drop(gate);
                    let cleanup = self.teardown_vm(record.id, &vm);
                    if cleanup.is_ok() {
                        self.scheduler.release(record.id);
                    }
                    return Err(ReadoptFailure::Fatal(match cleanup {
                        Ok(()) => "supervisor running lock poisoned during re-adoption".into(),
                        Err(error) => format!(
                            "supervisor running lock poisoned during re-adoption; clean up identified VMM {}: {error}",
                            record.id
                        ),
                    }));
                }
            }
        }
        self.reconcile_readopted_status(record).await?;
        Ok(true)
    }

    /// Never trust persisted lifecycle state across a coordinator crash. Pin
    /// the re-adopted runtime behind its operation gate, observe the VMM, and
    /// fence that observation at N+2 before serving traffic. N+1 may already
    /// have reached the fleet before the previous coordinator crashed while
    /// SQLite still retained N.
    async fn reconcile_readopted_status(
        self: &Arc<Self>,
        record: &mut VmRecord,
    ) -> Result<(), ReadoptFailure> {
        let gate = match self.operation_gate(record.id) {
            Ok(gate) => gate,
            Err(error) => {
                let reason = format!("gate re-adopted VM: {error}");
                self.quarantine_readopted_runtime(record.id).await?;
                return Err(ReadoptFailure::Unadoptable(reason));
            }
        };
        let operation = gate.lock_owned().await;
        let observed = self.status_vm(record.id);
        drop(operation);
        let observed = match observed.and_then(|status| match status.state {
            tarit_vmm_client::VmState::Running => Ok(VmStatus::Running),
            tarit_vmm_client::VmState::Paused => Ok(VmStatus::Paused),
            tarit_vmm_client::VmState::Suspended => Ok(VmStatus::Suspended),
            state => Err(OrchError::Vmm(format!(
                "re-adopted VMM reported non-live state {state:?}"
            ))),
        }) {
            Ok(status) => status,
            Err(error) => {
                self.quarantine_readopted_runtime(record.id).await?;
                return Err(ReadoptFailure::Unadoptable(format!(
                    "observe re-adopted VMM state: {error}"
                )));
            }
        };
        let revision = match record.revision.checked_add(2) {
            Some(revision) => revision,
            None => {
                self.quarantine_readopted_runtime(record.id).await?;
                return Err(ReadoptFailure::Unadoptable(
                    "persisted VM revision exhausted during re-adoption".into(),
                ));
            }
        };
        record.status = observed;
        record.revision = revision;
        record.updated_at = chrono::Utc::now();
        Ok(())
    }

    async fn quarantine_readopted_runtime(
        self: &Arc<Self>,
        id: Uuid,
    ) -> Result<(), ReadoptFailure> {
        let supervisor = Arc::clone(self);
        let stopped = tokio::task::spawn_blocking(move || {
            supervisor.stop_vm(id)?;
            supervisor.scheduler.release(id);
            Ok::<(), OrchError>(())
        })
        .await;
        stopped
            .map_err(|error| OrchError::Internal(format!("quarantine task: {error}")))
            .and_then(|result| result)
            .map_err(|error| {
                ReadoptFailure::Fatal(format!("failed to quarantine re-adopted VMM {id}: {error}"))
            })
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

    /// Client for lifecycle transitions and snapshots, whose RAM-sized work
    /// keeps the socket silent far past the plain client's 5s read timeout.
    fn lifecycle_client_for(&self, id: Uuid) -> Result<VmmClient, OrchError> {
        Ok(self
            .client_for(id)?
            .with_request_timeout(LIFECYCLE_OP_TIMEOUT))
    }

    /// Return the operation gate owned by this exact live runtime. Gates are
    /// not kept in a separate UUID map: removing the `RunningVm` drops the
    /// supervisor's reference, so churn cannot grow an unbounded registry.
    pub(crate) fn operation_gate(&self, id: Uuid) -> Result<Arc<AsyncMutex<()>>, OrchError> {
        self.running
            .lock()
            .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?
            .get(&id)
            .map(|vm| Arc::clone(&vm.operation_gate))
            .ok_or_else(|| OrchError::NotFound(format!("vm {id} not running")))
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
            let warm = {
                let _gate = self.boot_gate.blocking_lock();
                let mut warm = self
                    .warm
                    .lock()
                    .map_err(|_| OrchError::Internal("warm lock poisoned".into()))?;
                warm.iter()
                    .position(|warm_vm| warm_vm.id == id)
                    .and_then(|index| warm.remove(index))
            };
            if let Some(warm) = warm {
                let client = VmmClient::new(&warm.vm.socket_path);
                let _ = client.stop();
                return self.teardown_vm(id, &warm.vm);
            }
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
        let client = self.lifecycle_client_for(id)?;
        client.pause().map_err(|e| OrchError::Vmm(e.to_string()))
    }

    pub fn suspend_vm(&self, id: Uuid) -> Result<(), OrchError> {
        let client = self.lifecycle_client_for(id)?;
        client
            .suspend()
            .map_err(|error| OrchError::Vmm(error.to_string()))
    }

    pub fn resume_vm(&self, id: Uuid) -> Result<(), OrchError> {
        let client = self.lifecycle_client_for(id)?;
        let _admission = self.admission.enter()?;
        let state_before = client
            .status()
            .map_err(|error| OrchError::Vmm(format!("status before resume: {error}")))?
            .state;
        client
            .resume()
            .map_err(|error| OrchError::Vmm(error.to_string()))?;
        let socket = self
            .running
            .lock()
            .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))?
            .get(&id)
            .map(|vm| vm.socket_path.clone())
            .ok_or_else(|| OrchError::NotFound(format!("vm {id} not running")))?;
        if let Err(error) = self.await_resumed_ready(&socket) {
            let rollback = match state_before {
                tarit_vmm_client::VmState::Suspended => client.suspend(),
                tarit_vmm_client::VmState::Paused => client.pause(),
                _ => Ok(()),
            };
            return Err(match rollback {
                Ok(()) => error,
                Err(rollback) => OrchError::Vmm(format!(
                    "{error}; failed to restore pre-resume state: {rollback}"
                )),
            });
        }
        Ok(())
    }

    fn await_resumed_ready(&self, socket: &Path) -> Result<(), OrchError> {
        wait_for_guest_ready(
            readiness_timeout(ReadinessCheck::Resume),
            || {
                if self.is_shutting_down() {
                    Err(self.shutdown_error())
                } else {
                    Ok(())
                }
            },
            |remaining| {
                let request_timeout = readiness_request_timeout(remaining);
                let exec_timeout_ms = readiness_exec_timeout_ms(request_timeout);
                let client = VmmClient::new(socket)
                    .with_connect_timeout(request_timeout)
                    .with_request_timeout(request_timeout);
                match client.exec("true", exec_timeout_ms) {
                    Ok((0, _, _, _)) => Ok(true),
                    Ok((code, _, _, _)) => {
                        Err(format!("resume readiness exited with status {code}"))
                    }
                    Err(error) => Err(error.to_string()),
                }
            },
        )
        .map_err(|error| match error {
            ReadinessWaitError::Cancelled(error) => error,
            ReadinessWaitError::TimedOut(last) => {
                OrchError::Vmm(format!("guest did not become ready after resume: {last}"))
            }
        })
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
            RunningVm::new(process.pid, PathBuf::new(), process, Some(allocation)),
        );
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn install_test_control_runtime(&self, id: Uuid, socket_path: PathBuf) {
        let process = ManagedProcess::new(
            Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn test VMM process"),
        );
        self.running
            .lock()
            .unwrap()
            .insert(id, RunningVm::new(process.pid, socket_path, process, None));
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
        // An exec request is not replay-safe: a lost response may mean the
        // guest accepted and ran the command. Send it exactly once and surface
        // an ambiguous transport failure to the caller. Readiness retries use
        // the harmless `true` probe before a VM enters the warm pool.
        client
            .exec(command, timeout_ms)
            .map_err(|error| OrchError::Vmm(format!("exec: {error}")))
    }

    pub(crate) fn snapshot_bundle_vm(
        &self,
        id: Uuid,
        diff: bool,
        resume_after: bool,
        has_overlay: bool,
    ) -> Result<SnapshotBundle, OrchError> {
        let client = self.lifecycle_client_for(id)?;
        if resume_after {
            // Pause is synchronous: it returns only after every vCPU has left
            // KVM_RUN and completed its current MMIO handler. The pause window
            // ends as soon as RAM is immutable and the disk upper is captured;
            // moving immutable RAM into taritd's durable namespace happens
            // only after resume.
            client
                .pause()
                .map_err(|error| OrchError::Vmm(format!("pause for snapshot: {error}")))?;
        }
        let pause_started = resume_after.then(Instant::now);

        let scratch_snapshot_path = match client.snapshot_unreleased(diff) {
            Ok(path) => path,
            Err(error) => {
                return Err(compensate_snapshot_pause(
                    &client,
                    resume_after,
                    OrchError::Vmm(format!("snapshot RAM: {error}")),
                ));
            }
        };
        // Keep the VMM's cleanup token armed until every member of the bundle
        // has been captured. A failed disk copy therefore cannot publish a
        // RAM-only snapshot.
        let scratch_ram_artifact = match OwnedArtifact::capture(&scratch_snapshot_path) {
            Ok(artifact) => artifact,
            Err(error) => {
                return Err(compensate_snapshot_pause(
                    &client,
                    resume_after,
                    OrchError::Internal(format!("capture RAM snapshot ownership: {error}")),
                ));
            }
        };
        let overlay_artifact = if has_overlay {
            let source = PathBuf::from(self.overlay_path_for(id));
            let destination = self.snapshot_overlay_path();
            match copy_private_artifact(&source, &destination) {
                Ok(artifact) => Some(artifact),
                Err(error) => {
                    return Err(compensate_snapshot_pause(
                        &client,
                        resume_after,
                        OrchError::Internal(format!("capture snapshot disk upper: {error}")),
                    ));
                }
            }
        } else {
            None
        };

        if resume_after {
            if let Err(error) = client.resume() {
                if let Some(artifact) = overlay_artifact {
                    let _ = artifact.remove();
                }
                return Err(OrchError::Vmm(format!(
                    "resume after disk-consistent snapshot: {error}; disk artifact was discarded and RAM scratch remains VMM-owned"
                )));
            }
            tracing::info!(
                vm = %id,
                paused_ms = pause_started
                    .map(|started| started.elapsed().as_millis())
                    .unwrap_or_default(),
                "snapshot consistency pause completed before durable RAM transfer"
            );
        }

        let ram_destination = self.snapshot_ram_path();
        let ram_artifact =
            match copy_private_artifact(Path::new(&scratch_snapshot_path), &ram_destination) {
                Ok(artifact) => artifact,
                Err(error) => {
                    if let Some(artifact) = overlay_artifact {
                        let _ = artifact.remove();
                    }
                    return Err(OrchError::Internal(format!(
                        "claim durable RAM snapshot after consistency pause: {error}"
                    )));
                }
            };
        if let Err(error) =
            client.release_scratch(&scratch_snapshot_path, scratch_ram_artifact.identity())
        {
            let _ = ram_artifact.remove();
            if let Some(artifact) = overlay_artifact {
                let _ = artifact.remove();
            }
            return Err(OrchError::Vmm(format!("claim RAM snapshot: {error}")));
        }
        match scratch_ram_artifact.remove() {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                path = %scratch_snapshot_path,
                "released VMM RAM scratch path no longer names the captured inode"
            ),
            Err(error) => tracing::warn!(
                path = %scratch_snapshot_path,
                "released VMM RAM scratch cleanup deferred to VMM GC: {error}"
            ),
        }

        let overlay_path = overlay_artifact
            .as_ref()
            .map(|artifact| artifact.path().display().to_string());
        let mut artifacts = vec![ram_artifact];
        artifacts.extend(overlay_artifact);
        let bundle = SnapshotBundle {
            snapshot_path: ram_destination.display().to_string(),
            overlay_path,
            artifacts,
        };
        Ok(bundle)
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

    pub(crate) fn has_retained_boot(&self, id: Uuid) -> bool {
        self.booting
            .lock()
            .map(|booting| booting.contains_key(&id))
            .unwrap_or(true)
    }

    /// Notify synchronous shutdown paths that an async request abandoned its
    /// lifecycle. This deliberately does not tear anything down: a DELETE or
    /// stop-all owns the later, durable terminal transition.
    pub(crate) fn abandon_lifecycle(&self, id: Uuid) {
        if let Ok(booting) = self.booting.lock() {
            if let Some(booting_vm) = booting.get(&id) {
                booting_vm.control.request_cancellation();
                booting_vm.control.complete(Err(OrchError::Internal(
                    "request abandoned lifecycle publication".into(),
                )));
            }
        }

        // A warm claim can be abandoned while its Creating record is awaiting
        // durable publication. Move that exact VM into the normal live registry
        // so DELETE/stop-all sees and tears it down rather than losing it in warm.
        let warm = self.warm.lock().ok().and_then(|mut warm| {
            warm.iter()
                .position(|warm_vm| warm_vm.id == id)
                .and_then(|index| warm.remove(index))
        });
        if let Some(warm) = warm {
            match self.running.lock() {
                Ok(mut running) if !running.contains_key(&id) => {
                    running.insert(id, warm.vm);
                }
                Ok(_) | Err(_) => {
                    if let Ok(mut warm_queue) = self.warm.lock() {
                        warm_queue.push_back(warm);
                    } else {
                        tracing::error!(
                            %id,
                            "abandoned warm lifecycle could not retain its warm registry entry"
                        );
                    }
                }
            }
        }
    }

    pub(crate) fn stop_all(&self) -> Result<ShutdownSummary, Box<ShutdownFailure>> {
        let (booting, owned_tasks) = {
            let _gate = self.boot_gate.blocking_lock();
            // This is the linearization point with user lifecycle publication:
            // after it, no boot can enter its durable Running publication.
            self.shutting_down.store(true, Ordering::SeqCst);
            let booting = self
                .signal_booting_tasks()
                .map_err(|error| Box::new(ShutdownFailure::from(error)))?;
            let owned_tasks = self
                .signal_owned_tasks()
                .map_err(|error| Box::new(ShutdownFailure::from(error)))?;
            (booting, owned_tasks)
        };
        // A caller may have been dropped, but its worker remains in
        // `owned_tasks`. Wait until it has finished publication or compensation
        // before draining `running`/`booting` below.
        let owned_outcomes = self.wait_for_owned_tasks(owned_tasks);
        let booting = self.complete_cancelled_booting_tasks(booting);
        let (running, warm, golden_artifacts) = {
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
            let mut golden_artifacts = self.golden_artifacts.lock().map_err(|_| {
                Box::new(ShutdownFailure::from(OrchError::Internal(
                    "golden artifact lock poisoned".into(),
                )))
            })?;
            (
                running.drain().collect::<Vec<_>>(),
                warm.drain(..).collect::<Vec<_>>(),
                golden_artifacts.drain(..).collect::<Vec<_>>(),
            )
        };
        let mut transitions = ShutdownTransitions::default();
        for outcome in owned_outcomes {
            if let Err(error) = outcome {
                transitions.record_internal_failure(OrchError::Internal(format!(
                    "supervisor-owned lifecycle worker retained work for retry: {error}"
                )));
            }
        }
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
        cleanup_golden_artifacts(golden_artifacts);

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
        graceful_stop_vmm(&vm.socket_path);
        let process_exited = match vm.process.kill_wait() {
            Ok(()) => true,
            Err(error) => {
                failures.push(error.to_string());
                false
            }
        };
        if let Err(error) = remove_file_if_present(&vm.socket_path) {
            failures.push(format!("remove VMM socket: {error}"));
        }
        // The golden registry owns a golden source VM's overlay: warm restores
        // seed every clone from it, so tearing down that VM must not delete it.
        match self.owns_golden_artifact(Path::new(&self.overlay_path_for(id))) {
            Ok(true) => {}
            Ok(false) => {
                if let Err(error) = remove_file_if_present(Path::new(&self.overlay_path_for(id))) {
                    failures.push(format!("remove VMM overlay: {error}"));
                }
            }
            Err(error) => failures.push(error.to_string()),
        }
        // The exact child can only be empty after the process is confirmed
        // dead. Removing only this UUID-derived child preserves the
        // operator-owned parent cgroup.
        if process_exited {
            if let Some(path) = self.exact_vm_cgroup_path(id) {
                if let Err(error) = remove_dir_if_present(&path) {
                    failures.push(format!(
                        "remove exact VM cgroup {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        if let (Some(p), Some(a)) = (&self.net, &vm.net) {
            match self.defer_network_teardown(id, a.clone()) {
                Ok(Some(allocation)) => {
                    if let Err(error) = p.teardown(&allocation) {
                        failures.push(format!("teardown network allocation: {error}"));
                    }
                }
                Ok(None) => {}
                Err(error) => failures.push(format!("defer network teardown: {error}")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(OrchError::Internal(failures.join("; ")))
        }
    }

    fn defer_network_teardown(
        &self,
        id: Uuid,
        allocation: NetAlloc,
    ) -> Result<Option<NetAlloc>, OrchError> {
        let mut leases = self
            .network_leases
            .lock()
            .map_err(|_| OrchError::Internal("supervisor network lease lock poisoned".into()))?;
        let Some(lease) = leases.get_mut(&id) else {
            return Ok(Some(allocation));
        };
        let teardown = lease.defer_teardown(allocation);
        if lease.active == 0 && !lease.teardown_in_progress() {
            leases.remove(&id);
        }
        Ok(teardown)
    }

    fn release_network_lease(&self, id: Uuid) {
        let teardown = {
            let Ok(mut leases) = self.network_leases.lock() else {
                tracing::error!(%id, "network lease lock poisoned while releasing lease");
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
                if let Err(error) = provisioner.teardown(&allocation) {
                    tracing::error!(%id, %error, "failed deferred network teardown");
                    return;
                }
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

/// Copy an orchestrator-owned artifact without following either leaf path.
/// The destination is created exclusively at mode 0600, then both it and its
/// parent directory are synced before the path can be persisted.
fn copy_private_artifact(
    source_path: &Path,
    destination_path: &Path,
) -> std::io::Result<OwnedArtifact> {
    let mut source_options = OpenOptions::new();
    source_options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let source = source_options.open(source_path)?;
    let before = source.metadata()?;
    if !before.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", source_path.display()),
        ));
    }
    // Snapshot material is private to the taritd identity. Refuse hard-linked
    // or broadly accessible sources because either permits out-of-band
    // mutation while a bundle is captured.
    let effective_uid = unsafe { libc::geteuid() };
    if before.uid() != effective_uid || before.nlink() != 1 || before.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} must be owned by uid {effective_uid}, mode 0600, with one link",
                source_path.display()
            ),
        ));
    }
    // All block requests are handled synchronously on a vCPU's MMIO exit.
    // Once pause() acknowledges every vCPU, no request remains in flight; this
    // fsync pushes the completed overlay writes before we copy its upper.
    source.sync_all()?;

    let destination = OwnedArtifact::create_private(destination_path)?;
    let result = (|| {
        copy_artifact_data(&source, &destination._file, before.len())?;
        destination._file.sync_all()?;

        let after = source.metadata()?;
        let path_metadata = std::fs::symlink_metadata(source_path)?;
        let unchanged = before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && after.dev() == path_metadata.dev()
            && after.ino() == path_metadata.ino();
        if !unchanged {
            return Err(std::io::Error::other(format!(
                "{} changed while it was copied",
                source_path.display()
            )));
        }

        let parent = destination_path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} has no parent directory", destination_path.display()),
            )
        })?;
        let mut parent_options = OpenOptions::new();
        parent_options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        parent_options.open(parent)?.sync_all()
    })();
    if let Err(error) = result {
        let _ = destination.remove();
        return Err(error);
    }
    Ok(destination)
}

#[cfg(target_os = "linux")]
fn copy_artifact_data(source: &File, destination: &File, source_len: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    // FICLONE is the fastest safe path on XFS/btrfs and compatible filesystems:
    // the new inode gets private CoW mappings without copying the sparse upper.
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let cloned = unsafe { libc::ioctl(destination.as_raw_fd(), FICLONE, source.as_raw_fd()) };
    if cloned == 0 {
        return Ok(());
    }
    let clone_error = std::io::Error::last_os_error();
    match clone_error.raw_os_error() {
        Some(libc::EOPNOTSUPP) | Some(libc::ENOTTY) | Some(libc::EXDEV) | Some(libc::EINVAL) => {}
        _ => return Err(clone_error),
    }

    // Reflink is optional. The fallback copies only allocated extents, keeping
    // a large CoW file sparse instead of reading and allocating its virtual
    // data region. SEEK_DATA/SEEK_HOLE failure is fatal: silently using a dense
    // copy would make snapshot latency scale with virtual disk size.
    const COPY_CHUNK: usize = 1024 * 1024;
    let mut cursor = 0u64;
    let mut buffer = vec![0u8; COPY_CHUNK];
    while cursor < source_len {
        let data = unsafe {
            libc::lseek(
                source.as_raw_fd(),
                i64::try_from(cursor).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "artifact is too large")
                })?,
                libc::SEEK_DATA,
            )
        };
        if data < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            return Err(std::io::Error::new(
                error.kind(),
                format!("SEEK_DATA is required for sparse artifact capture: {error}"),
            ));
        }
        let data = u64::try_from(data).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "negative data extent")
        })?;
        let hole = unsafe { libc::lseek(source.as_raw_fd(), data as i64, libc::SEEK_HOLE) };
        if hole < 0 {
            let error = std::io::Error::last_os_error();
            return Err(std::io::Error::new(
                error.kind(),
                format!("SEEK_HOLE is required for sparse artifact capture: {error}"),
            ));
        }
        let extent_end = u64::try_from(hole)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "negative hole"))?
            .min(source_len);
        if extent_end <= data {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "filesystem returned an invalid sparse extent",
            ));
        }
        let mut offset = data;
        while offset < extent_end {
            let amount = usize::try_from((extent_end - offset).min(COPY_CHUNK as u64))
                .expect("bounded copy length fits usize");
            source.read_exact_at(&mut buffer[..amount], offset)?;
            destination.write_all_at(&buffer[..amount], offset)?;
            offset += amount as u64;
        }
        cursor = extent_end;
    }
    destination.set_len(source_len)
}

#[cfg(not(target_os = "linux"))]
fn copy_artifact_data(source: &File, destination: &File, source_len: u64) -> std::io::Result<()> {
    const COPY_CHUNK: usize = 1024 * 1024;
    let mut buffer = vec![0u8; COPY_CHUNK];
    let mut offset = 0u64;
    while offset < source_len {
        let amount = usize::try_from((source_len - offset).min(COPY_CHUNK as u64))
            .expect("bounded copy length fits usize");
        source.read_exact_at(&mut buffer[..amount], offset)?;
        destination.write_all_at(&buffer[..amount], offset)?;
        offset += amount as u64;
    }
    destination.set_len(source_len)
}

fn compensate_snapshot_pause(
    client: &VmmClient,
    resume_after: bool,
    primary: OrchError,
) -> OrchError {
    if !resume_after {
        return primary;
    }
    match client.resume() {
        Ok(()) => primary,
        Err(resume) => OrchError::Vmm(format!(
            "{primary}; failed to resume VM after snapshot failure: {resume}"
        )),
    }
}

fn remove_dir_if_present(path: &Path) -> Result<(), std::io::Error> {
    match std::fs::remove_dir(path) {
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

fn cleanup_golden_artifacts(artifacts: impl IntoIterator<Item = OwnedArtifact>) {
    for artifact in artifacts {
        match artifact.remove() {
            Ok(true) => {
                tracing::info!(path = %artifact.path().display(), "removed golden artifact")
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(
                path = %artifact.path().display(),
                "remove golden artifact failed: {error}"
            ),
        }
    }
}

fn take_matching_artifacts(
    artifacts: &mut Vec<OwnedArtifact>,
    keys: &[(PathBuf, ScratchIdentity)],
) -> Vec<OwnedArtifact> {
    let mut removed = Vec::new();
    let mut retained = Vec::with_capacity(artifacts.len());
    for artifact in artifacts.drain(..) {
        if keys
            .iter()
            .any(|(path, identity)| artifact.matches(path, identity))
        {
            removed.push(artifact);
        } else {
            retained.push(artifact);
        }
    }
    *artifacts = retained;
    removed
}

fn readiness_timeout(check: ReadinessCheck) -> Duration {
    match check {
        ReadinessCheck::Boot => GUEST_READY_TIMEOUT,
        ReadinessCheck::Resume => RESUME_READY_TIMEOUT,
    }
}

fn next_socket_wait_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(SOCKET_WAIT_MAX)
}

fn readiness_exec_timeout_ms(remaining: Duration) -> u64 {
    let timeout = remaining.min(GUEST_READY_EXEC_TIMEOUT);
    u64::try_from(timeout.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}

fn readiness_request_timeout(remaining: Duration) -> Duration {
    remaining.min(GUEST_READY_EXEC_TIMEOUT)
}

fn readiness_poll_sleep(remaining: Duration) -> Duration {
    remaining.min(GUEST_READY_POLL_INTERVAL)
}

#[derive(Debug)]
enum ReadinessWaitError {
    Cancelled(OrchError),
    TimedOut(String),
}

fn wait_for_guest_ready<C, F>(
    timeout: Duration,
    mut ensure_active: C,
    mut probe: F,
) -> Result<(), ReadinessWaitError>
where
    C: FnMut() -> Result<(), OrchError>,
    F: FnMut(Duration) -> Result<bool, String>,
{
    let deadline = Instant::now() + timeout;
    let mut last = "guest agent returned no successful readiness response".to_string();

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        ensure_active().map_err(ReadinessWaitError::Cancelled)?;
        match probe(remaining) {
            Ok(true) => return Ok(()),
            Ok(false) => {
                last = "guest agent readiness command did not succeed".to_string();
            }
            Err(error) => last = error,
        }
        ensure_active().map_err(ReadinessWaitError::Cancelled)?;
        let sleep = readiness_poll_sleep(deadline.saturating_duration_since(Instant::now()));
        if !sleep.is_zero() {
            std::thread::sleep(sleep);
        }
    }

    Err(ReadinessWaitError::TimedOut(format!(
        "guest agent never became ready: {last}"
    )))
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

#[cfg(target_os = "linux")]
fn write_cgroup_cpu_weight(cgroup_dir: &Path, cpu_weight: u64) -> std::io::Result<()> {
    write_single_file(&cgroup_dir.join("cpu.weight"), &cpu_weight.to_string())
}

#[cfg(not(target_os = "linux"))]
fn write_cgroup_cpu_weight(_cgroup_dir: &Path, _cpu_weight: u64) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "cgroup v2 is only available on Linux",
    ))
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

fn build_vmm_config(
    cfg: &VmSpawnConfig,
    net: Option<&NetAlloc>,
    overlay: Option<String>,
) -> VmConfig {
    let mut volumes = Vec::new();
    // Every rootfs is an immutable base with a per-VM sparse CoW overlay. Never
    // attach a shared base read-write: one unsafe default or request must not let
    // two guests corrupt or observe each other's filesystem writes.
    if let Some(rootfs) = &cfg.rootfs_path {
        volumes.push(VolumeConfig {
            path: rootfs.display().to_string(),
            read_only: false,
            overlay: overlay.clone(),
        });
    }

    // Host isolation always uses a CoW overlay. This independent flag controls
    // whether the guest itself mounts the root filesystem read-only.
    let base_cmdline = if cfg.read_only {
        cfg.cmdline.replace("root=/dev/vda rw", "root=/dev/vda ro")
    } else {
        cfg.cmdline.clone()
    };

    // With per-VM networking, attach a virtio-net device on the provisioned tap
    // and append the kernel `ip=` fragment so the guest configures eth0 at boot.
    let (nets, cmdline) = match net {
        Some(a) => (
            vec![net_config_for_allocation(a)],
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

fn net_config_for_allocation(allocation: &NetAlloc) -> NetConfig {
    NetConfig {
        tap: allocation.tap.clone(),
        guest_mac: None,
        guest_ip: Some(allocation.guest_ip.clone()),
        port_forwards: vec![],
    }
}

fn restored_network_rebind_command(allocation: &NetAlloc) -> String {
    format!(
        "ip addr flush dev eth0 scope global && ip addr add {guest}/{prefix} dev eth0 && ip link set eth0 up && ip route replace default via {gateway} && ip -4 -o addr show dev eth0 scope global | grep -F -q ' {guest}/{prefix} ' && ip route show default | grep -F -q 'default via {gateway} '",
        guest = allocation.guest_ip,
        prefix = allocation.prefix,
        gateway = allocation.host_ip,
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiKeyRegistry, ApiRole, AutoscaleConfig, Config, WarmPoolConfig};
    use std::io::{Read, Write};
    use std::sync::mpsc;
    use std::thread;

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_live_vmm_accepts_process_owning_the_socket() {
        let socket = std::env::temp_dir().join(format!("tarit-adopt-{}.sock", Uuid::new_v4()));
        // A shell that stays alive and carries the socket path in its argv, the
        // way taritd launches `vmm serve --socket <path>`. `read` is a builtin,
        // so the shell does not exec-optimize into another program (which would
        // drop the socket from argv), and it blocks on the piped stdin we keep
        // open, so the process stays alive until we kill it.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("read _line")
            .arg("tarit-vmm")
            .arg(&socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn stand-in VMM");
        // /proc/<pid>/cmdline can read empty for a brief window right after exec
        // under parallel-test load, so retry until the argv is published.
        let mut result = Err(String::from("verify not attempted"));
        for _ in 0..200 {
            result = verify_live_vmm(child.id(), &socket);
            if result.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            result.is_ok(),
            "owner process must be adoptable: {result:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_live_vmm_rejects_pid_that_does_not_own_the_socket() {
        let socket = std::env::temp_dir().join(format!("tarit-adopt-{}.sock", Uuid::new_v4()));
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn unrelated process");
        let result = verify_live_vmm(child.id(), &socket);
        let _ = child.kill();
        let _ = child.wait();
        let error = result.expect_err("a reused PID must not be adopted");
        assert!(error.contains("does not own"), "unexpected error: {error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_live_vmm_rejects_dead_pid() {
        let mut child = Command::new("true")
            .spawn()
            .expect("spawn short-lived process");
        let pid = child.id();
        child.wait().expect("reap short-lived process");
        let socket = std::env::temp_dir().join("tarit-adopt-dead.sock");
        let error = verify_live_vmm(pid, &socket).expect_err("dead PID must not be adopted");
        assert!(error.contains("not alive"), "unexpected error: {error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kill_wait_adopted_treats_absent_pid_as_terminated() {
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn short-lived process");
        let pid = child.id();
        let pidfd = pidfd_open(pid).expect("pin child with pidfd");
        child.kill().expect("kill child");
        child.wait().expect("reap short-lived process");
        // The process is gone, so signalling the pinned pidfd reports ESRCH and
        // terminating the adopted handle is a no-op.
        ManagedProcess::adopted(pid, pidfd)
            .kill_wait()
            .expect("terminating an already-exited adopted VMM must succeed");
    }

    fn supervisor_config(root: &Path) -> Config {
        Config {
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
            allow_insecure_peer_http: true,
            enable_net: false,
            rootfs_read_only: false,
            metrics_expose_tenant_labels: false,
            api_max_in_flight: 128,
            api_requests_per_second: 10_000,
            api_request_timeout_ms: 5_000,
            api_max_body_bytes: 1024 * 1024,
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
        }
    }

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
            allow_insecure_peer_http: true,
            enable_net: false,
            rootfs_read_only: false,
            metrics_expose_tenant_labels: false,
            api_max_in_flight: 128,
            api_requests_per_second: 10_000,
            api_request_timeout_ms: 5_000,
            api_max_body_bytes: 1024 * 1024,
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
            share_listen: None,
            share_domain: None,
            share_token_key: None,
            share_token_ttl_secs: 300,
            share_connect_timeout_ms: 1_000,
            share_idle_timeout_secs: 1,
        };
        Arc::new(VmmSupervisor::new(config))
    }

    fn restart_record(id: Uuid, socket_path: &Path) -> VmRecord {
        let now = chrono::Utc::now();
        VmRecord {
            id,
            host_id: "test-host".into(),
            owner_key: Some("tenant-a".into()),
            api_key_id: Some("test-key".into()),
            status: VmStatus::Running,
            revision: 7,
            startup_path: None,
            memory_mib: 256,
            vcpus: 1,
            kernel_path: "kernel".into(),
            rootfs_path: Some("rootfs".into()),
            cmdline: DEFAULT_CMDLINE.into(),
            socket_path: Some(socket_path.display().to_string()),
            pid: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn restart_reconciliation_fences_observed_paused_state() {
        let root = PathBuf::from(format!("target/taritd-restart-paused-{}", Uuid::new_v4()));
        let socket_path = root.join("vmm.sock");
        std::fs::create_dir_all(&root).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut length = [0_u8; 4];
            stream.read_exact(&mut length).unwrap();
            let mut body = vec![0; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut body).unwrap();
            let request: tarit_vmm_client::ApiRequest = serde_json::from_slice(&body).unwrap();
            assert!(matches!(request, tarit_vmm_client::ApiRequest::Status));
            let response = tarit_vmm_client::ApiResponse::Status(tarit_vmm_client::VmStatus {
                state: tarit_vmm_client::VmState::Paused,
                uptime_ms: 1,
                vcpus: 1,
                mem_mib: 256,
                volumes: 0,
                nets: 0,
                kernel: "kernel".into(),
                vcpu_alive: true,
            });
            let encoded = serde_json::to_vec(&response).unwrap();
            stream
                .write_all(&(encoded.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&encoded).unwrap();
            stream.flush().unwrap();
        });
        let supervisor = Arc::new(VmmSupervisor::new(supervisor_config(&root)));
        let id = Uuid::new_v4();
        let process = ManagedProcess::new(Command::new("sleep").arg("30").spawn().unwrap());
        supervisor
            .scheduler
            .reserve_existing(id, ResourceShape::new(1, 256))
            .unwrap();
        supervisor.running.lock().unwrap().insert(
            id,
            RunningVm::new(process.pid, socket_path.clone(), process, None),
        );
        let mut record = restart_record(id, &socket_path);
        let previous_updated_at = record.updated_at;

        test_runtime()
            .block_on(supervisor.reconcile_readopted_status(&mut record))
            .unwrap();

        server.join().unwrap();
        assert_eq!(record.status, VmStatus::Paused);
        assert_eq!(record.revision, 9);
        assert!(record.updated_at >= previous_updated_at);
        supervisor.stop_vm(id).unwrap();
        assert!(supervisor.scheduler.release(id));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restart_reconciliation_quarantines_unobservable_runtime() {
        let root = PathBuf::from(format!(
            "target/taritd-restart-unobservable-{}",
            Uuid::new_v4()
        ));
        let socket_path = root.join("missing-vmm.sock");
        let supervisor = Arc::new(VmmSupervisor::new(supervisor_config(&root)));
        let id = Uuid::new_v4();
        let process = ManagedProcess::new(Command::new("sleep").arg("30").spawn().unwrap());
        supervisor
            .scheduler
            .reserve_existing(id, ResourceShape::new(1, 256))
            .unwrap();
        supervisor.running.lock().unwrap().insert(
            id,
            RunningVm::new(process.pid, socket_path.clone(), process, None),
        );
        let mut record = restart_record(id, &socket_path);

        let error = test_runtime()
            .block_on(supervisor.reconcile_readopted_status(&mut record))
            .unwrap_err();

        assert!(
            error.to_string().contains("observe re-adopted VMM state"),
            "{error}"
        );
        assert_eq!(record.status, VmStatus::Running);
        assert_eq!(record.revision, 7);
        assert!(supervisor.operation_gate(id).is_err());
        assert!(!supervisor.scheduler.is_reserved(id));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restart_reconciliation_propagates_quarantine_cleanup_failure() {
        let root = PathBuf::from(format!(
            "target/taritd-restart-quarantine-failure-{}",
            Uuid::new_v4()
        ));
        let socket_path = root.join("missing-vmm.sock");
        let supervisor = Arc::new(VmmSupervisor::new(supervisor_config(&root)));
        let id = Uuid::new_v4();
        let process = ManagedProcess::new(Command::new("sleep").arg("30").spawn().unwrap());
        supervisor
            .scheduler
            .reserve_existing(id, ResourceShape::new(1, 256))
            .unwrap();
        supervisor.running.lock().unwrap().insert(
            id,
            RunningVm::new(process.pid, socket_path.clone(), process, None),
        );
        let overlay_path = PathBuf::from(supervisor.overlay_path_for(id));
        std::fs::create_dir_all(&overlay_path).unwrap();
        let mut record = restart_record(id, &socket_path);

        let error = test_runtime()
            .block_on(supervisor.reconcile_readopted_status(&mut record))
            .unwrap_err();

        assert!(
            matches!(error, ReadoptFailure::Fatal(_)),
            "cleanup failure must abort startup: {error}"
        );
        assert!(supervisor.operation_gate(id).is_ok());
        assert!(supervisor.scheduler.is_reserved(id));
        std::fs::remove_dir(&overlay_path).unwrap();
        supervisor.stop_vm(id).unwrap();
        assert!(supervisor.scheduler.release(id));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn network_disabled_startup_rejects_contained_taps_or_live_recovery() {
        assert!(validate_network_startup_mode(false, &[], 0).is_ok());
        assert!(validate_network_startup_mode(false, &["insta7".into()], 0).is_err());
        assert!(validate_network_startup_mode(false, &[], 1).is_err());
        assert!(validate_network_startup_mode(true, &["insta7".into()], 1).is_ok());
    }

    #[test]
    fn shutdown_prevents_warm_refill_before_vmm_spawn() {
        let root = PathBuf::from(format!(
            "target/taritd-supervisor-shutdown-{}",
            Uuid::new_v4()
        ));
        let config = supervisor_config(&root);
        let class = config.warm_pool.classes[0].clone();
        let supervisor = Arc::new(VmmSupervisor::new(config.clone()));
        supervisor.begin_shutdown();

        let error = test_runtime()
            .block_on(Arc::clone(&supervisor).spawn_warm(class))
            .unwrap_err();

        assert!(matches!(error, OrchError::Overloaded { .. }));
        assert!(
            std::fs::read_dir(&config.socket_dir)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| entry.path().extension().is_none_or(|ext| ext != "sock")),
            "shutdown must reject refill before it creates a VMM socket"
        );
        drop(supervisor);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn teardown_vm_stops_vmm_before_killing_process() {
        let root = PathBuf::from(format!(
            "target/taritd-supervisor-teardown-{}",
            Uuid::new_v4()
        ));
        let socket_path = PathBuf::from(format!(
            "target/taritd-teardown-{}-{}.sock",
            std::process::id(),
            Uuid::new_v4()
        ));
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind test VMM socket");
        listener
            .set_nonblocking(true)
            .expect("make test VMM socket nonblocking");
        let process = ManagedProcess::new(
            Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn test VMM process"),
        );
        let process_for_liveness_check = process.clone();
        let process_for_assertion = process.clone();
        let (request_tx, request_rx) = std::sync::mpsc::sync_channel::<
            Result<(tarit_vmm_client::ApiRequest, bool), String>,
        >(1);
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut length = [0; 4];
                        stream.read_exact(&mut length).expect("read request length");
                        let mut body = vec![0; u32::from_be_bytes(length) as usize];
                        stream.read_exact(&mut body).expect("read request body");
                        let request = serde_json::from_slice(&body).expect("decode request");
                        let child_alive = process_for_liveness_check
                            .owned_child()
                            .lock()
                            .expect("lock child")
                            .try_wait()
                            .expect("inspect child")
                            .is_none();
                        let response =
                            serde_json::to_vec(&tarit_vmm_client::ApiResponse::Ok).unwrap();
                        stream
                            .write_all(&(response.len() as u32).to_be_bytes())
                            .expect("write response length");
                        stream.write_all(&response).expect("write response body");
                        stream.flush().expect("flush response");
                        request_tx
                            .send(Ok((request, child_alive)))
                            .expect("record request");
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            request_tx
                                .send(Err("timed out waiting for VMM request".into()))
                                .expect("record timeout");
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => {
                        request_tx
                            .send(Err(error.to_string()))
                            .expect("record accept error");
                        return;
                    }
                }
            }
        });
        let vm = RunningVm::new(process.pid, socket_path.clone(), process, None);
        let supervisor = VmmSupervisor::new(supervisor_config(&root));

        supervisor.teardown_vm(Uuid::new_v4(), &vm).unwrap();

        let (request, child_alive) = request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("teardown must contact the VMM")
            .expect("test VMM server must receive a request");
        assert!(
            matches!(request, tarit_vmm_client::ApiRequest::Stop),
            "teardown must send Stop before killing the VMM, got {request:?}"
        );
        assert!(
            child_alive,
            "the VMM process must still be alive when it receives Stop"
        );
        server.join().expect("join test VMM server");
        assert!(
            process_for_assertion
                .owned_child()
                .lock()
                .expect("lock child")
                .try_wait()
                .expect("inspect child")
                .is_some(),
            "teardown must reap the VMM process"
        );

        let _ = std::fs::remove_file(socket_path);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn admission_gate_rejects_refill_after_shutdown_between_planning_and_create() {
        use std::sync::{Arc, Barrier};

        let gate = Arc::new(VmAdmissionGate::default());
        let planned = Arc::new(Barrier::new(2));
        let release_create = Arc::new(Barrier::new(2));
        let created = Arc::new(AtomicBool::new(false));
        let worker_gate = Arc::clone(&gate);
        let worker_planned = Arc::clone(&planned);
        let worker_release = Arc::clone(&release_create);
        let worker_created = Arc::clone(&created);

        let refill = std::thread::spawn(move || {
            worker_planned.wait();
            worker_release.wait();
            worker_gate
                .admit(|| worker_created.store(true, Ordering::Release))
                .unwrap_err()
        });

        planned.wait();
        gate.close();
        release_create.wait();

        assert!(matches!(
            refill.join().unwrap(),
            OrchError::Overloaded { .. }
        ));
        assert!(
            !created.load(Ordering::Acquire),
            "a refill planned before shutdown must not create a VMM after admission closes"
        );
    }

    #[test]
    fn every_rootfs_uses_a_private_vm_overlay() {
        let supervisor = test_supervisor();
        let id = Uuid::parse_str("018f9f4d-07f5-7cc6-a1fd-111111111111").unwrap();
        let cfg = spawn_config(true, Some(PathBuf::from("/rootfs.ext4")));
        let expected = supervisor.overlay_path_for(id);

        assert_eq!(
            supervisor.overlay_path_for_config(id, &cfg),
            Some(expected.clone())
        );

        let vmm_config = build_vmm_config(&cfg, None, Some(expected.clone()));
        assert_eq!(vmm_config.volumes.len(), 1);
        assert_eq!(vmm_config.volumes[0].overlay, Some(expected));
        assert!(!vmm_config.volumes[0].read_only);
        assert!(vmm_config.kernel.cmdline.contains("root=/dev/vda ro"));

        let configured_rw = spawn_config(false, Some(PathBuf::from("/rootfs.ext4")));
        assert_eq!(
            supervisor.overlay_path_for_config(id, &configured_rw),
            Some(supervisor.overlay_path_for(id))
        );
        let rw_config = build_vmm_config(
            &configured_rw,
            None,
            supervisor.overlay_path_for_config(id, &configured_rw),
        );
        assert!(rw_config.kernel.cmdline.contains("root=/dev/vda rw"));
    }

    #[test]
    fn cgroup_limits_follow_exact_reserved_shape() {
        let root = PathBuf::from(format!("target/cgroup-args-{}", Uuid::new_v4()));
        let mut config = supervisor_config(&root);
        config.vm_cgroup_parent = Some("/sys/fs/cgroup/tarit".into());
        let supervisor = VmmSupervisor::new(config);
        let id = Uuid::new_v4();
        for (vcpus, memory_mib, cpu, memory) in [
            (1, 256, "1000m", "640M"),
            (2, 512, "2000m", "1024M"),
            (8, 4096, "8000m", "6400M"),
        ] {
            let args = supervisor
                .cgroup_args(id, ResourceShape::new(vcpus, memory_mib))
                .unwrap();
            let path_index = args.iter().position(|arg| arg == "--cgroup").unwrap();
            let cpu_index = args
                .iter()
                .position(|arg| arg == "--cgroup-cpu-max")
                .unwrap();
            let memory_index = args
                .iter()
                .position(|arg| arg == "--cgroup-memory-max")
                .unwrap();
            assert_eq!(
                args[path_index + 1],
                format!("/sys/fs/cgroup/tarit/tarit-{id}")
            );
            assert_eq!(args[cpu_index + 1], cpu);
            assert_eq!(args[memory_index + 1], memory);
        }
    }

    #[test]
    fn exact_cgroup_cleanup_removes_only_the_vm_child() {
        let parent = PathBuf::from(format!("target/cgroup-cleanup-{}", Uuid::new_v4()));
        let mut config = supervisor_config(&parent);
        config.vm_cgroup_parent = Some(parent.display().to_string());
        let supervisor = VmmSupervisor::new(config);
        let id = Uuid::new_v4();
        let child = supervisor.exact_vm_cgroup_path(id).unwrap();
        std::fs::create_dir_all(&child).unwrap();

        remove_dir_if_present(&child).unwrap();

        assert!(!child.exists());
        assert!(parent.exists(), "operator-owned cgroup parent was removed");
        remove_dir_if_present(&child).unwrap();
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn warm_priority_keeps_the_pid_in_its_exact_limited_cgroup() {
        let parent = PathBuf::from(format!("target/cgroup-priority-{}", Uuid::new_v4()));
        let shared_refill = parent.join("shared-refill");
        let mut config = supervisor_config(&parent);
        config.vm_cgroup_parent = Some(parent.display().to_string());
        config.warm_pool.refill_cgroup.path = Some(shared_refill.clone());
        config.warm_pool.refill_cgroup.cpu_weight = 200;
        let supervisor = VmmSupervisor::new(config);
        let id = Uuid::new_v4();
        let child = supervisor.exact_vm_cgroup_path(id).unwrap();
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("cpu.weight"), "100").unwrap();

        supervisor.configure_refill_cgroup(id, 4242).unwrap();
        assert_eq!(
            std::fs::read_to_string(child.join("cpu.weight")).unwrap(),
            "200"
        );
        assert!(
            !shared_refill.exists(),
            "refill moved the VM out of its exact limited cgroup"
        );
        assert!(
            !child.join("cgroup.procs").exists(),
            "priority changes must not rewrite cgroup membership"
        );

        supervisor.configure_leased_cgroup(id, 4242);
        assert_eq!(
            std::fs::read_to_string(child.join("cpu.weight")).unwrap(),
            "100"
        );
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn overlay_path_for_config_is_absent_without_rootfs() {
        let supervisor = test_supervisor();
        let id = Uuid::parse_str("018f9f4d-07f5-7cc6-a1fd-222222222222").unwrap();

        assert_eq!(
            supervisor.overlay_path_for_config(id, &spawn_config(true, None)),
            None
        );
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
    fn cancellation_between_spawn_and_registry_attachment_waits_for_cleanup() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let ticket = test_runtime()
            .block_on(supervisor.begin_boot_with_registration(
                id,
                SpawnPurpose::Live,
                ResourceShape::new(1, 1),
                || async { Ok(()) },
            ))
            .expect("boot registration must precede process spawn");
        let control = Arc::clone(&ticket.control);
        let pause = supervisor.pause_after_spawn_before_registry_attachment_for_test();
        let (done_tx, done_rx) = mpsc::channel();
        let worker_supervisor = Arc::clone(&supervisor);
        let worker = thread::spawn(move || {
            done_tx
                .send(worker_supervisor.spawn_server_for_boot(&ticket))
                .unwrap();
        });

        pause.wait_until_entered();
        control.request_cancellation();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "cancellation must not complete before the spawned process is attached"
        );

        pause.release();
        assert!(
            done_rx.recv().unwrap().is_err(),
            "the attached cancelled process must be cleaned before completion"
        );
        assert!(control.wait_for_completion().is_ok());
        assert!(!supervisor.has_retained_boot(id));
        worker.join().unwrap();
    }

    #[test]
    fn stop_all_enumerates_abandoned_cold_golden_and_restore_refill_workers() {
        for refill_kind in ["cold golden", "snapshot restore"] {
            let supervisor = test_supervisor();
            let id = Uuid::new_v4();
            let control = supervisor
                .begin_owned_task(id, SpawnPurpose::Refill)
                .expect("refill work must be supervisor-owned before its caller awaits it");
            let worker_control = Arc::clone(&control);
            let worker_supervisor = Arc::clone(&supervisor);
            let (cleanup_started_tx, cleanup_started_rx) = mpsc::channel();
            let (allow_cleanup_tx, allow_cleanup_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                worker_control.wait_for_cancellation();
                cleanup_started_tx.send(()).unwrap();
                allow_cleanup_rx.recv().unwrap();
                worker_supervisor.finish_owned_task(&worker_control, Ok(()));
            });

            let stop_supervisor = Arc::clone(&supervisor);
            let (done_tx, done_rx) = mpsc::channel();
            let stopper = thread::spawn(move || done_tx.send(stop_supervisor.stop_all()).unwrap());
            cleanup_started_rx.recv().unwrap();
            assert!(
                done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "stop-all must await the {refill_kind} worker after its caller is gone"
            );

            allow_cleanup_tx.send(()).unwrap();
            stopper.join().unwrap();
            done_rx
                .recv()
                .unwrap()
                .expect("completed refill cleanup must not block stop-all");
            worker.join().unwrap();
        }
    }

    #[test]
    fn owned_task_panic_completes_waiters_and_releases_registry_entry() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let result: Result<(), OrchError> = test_runtime().block_on({
            let supervisor = Arc::clone(&supervisor);
            async move {
                supervisor
                    .run_owned_task(id, SpawnPurpose::Refill, |_| async move {
                        panic!("injected owned task panic");
                    })
                    .await
            }
        });

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("supervisor-owned lifecycle worker failed"));
        assert!(!supervisor.has_owned_task(id));
    }

    #[test]
    fn aborting_unstarted_refill_releases_its_reservation() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let ticket = test_runtime()
            .block_on(supervisor.begin_boot_with_registration(
                id,
                SpawnPurpose::Refill,
                ResourceShape::new(1, 1),
                || async { Ok(()) },
            ))
            .unwrap();
        assert!(supervisor.scheduler.is_reserved(id));

        test_runtime().block_on(supervisor.abort_unstarted_boot(&ticket));

        assert!(!supervisor.scheduler.is_reserved(id));
        assert!(!supervisor.has_retained_boot(id));
    }

    #[test]
    fn capacity_rejection_never_runs_durable_registration() {
        let supervisor = test_supervisor();
        for _ in 0..supervisor.config.max_vms {
            supervisor.reserve_existing_for_test(Uuid::new_v4());
        }
        let id = Uuid::new_v4();
        let registered = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&registered);
        let result = test_runtime().block_on(supervisor.begin_boot_with_registration(
            id,
            SpawnPurpose::Live,
            ResourceShape::new(1, 1),
            move || async move {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            },
        ));
        assert!(matches!(result, Err(OrchError::Overloaded { .. })));
        assert!(
            !registered.load(Ordering::SeqCst),
            "capacity rejection must leave no durable trace so admission retries the same id"
        );
        assert!(!supervisor.has_retained_boot(id));
    }

    #[test]
    fn registration_failure_releases_capacity_reservation() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let result = test_runtime().block_on(supervisor.begin_boot_with_registration(
            id,
            SpawnPurpose::Live,
            ResourceShape::new(1, 1),
            || async { Err(OrchError::Internal("registration failed".into())) },
        ));
        assert!(matches!(result, Err(OrchError::Internal(_))));
        assert!(!supervisor.scheduler.is_reserved(id));
        assert!(!supervisor.has_retained_boot(id));
    }

    #[test]
    fn teardown_preserves_a_remembered_golden_overlay() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let overlay = PathBuf::from(supervisor.overlay_path_for(id));
        std::fs::create_dir_all(overlay.parent().unwrap()).unwrap();
        std::fs::write(&overlay, b"golden upper").unwrap();
        let mut artifacts = vec![OwnedArtifact::capture(&overlay).unwrap()];
        supervisor
            .remember_golden_artifacts(&mut artifacts)
            .unwrap();

        let process = ManagedProcess::new(Command::new("true").spawn().unwrap());
        let vm = RunningVm::new(process.pid, PathBuf::new(), process, None);
        supervisor.teardown_vm(id, &vm).unwrap();
        assert!(
            overlay.exists(),
            "warm restores seed from the golden overlay; tearing down its source VM must not delete it"
        );
        std::fs::remove_file(&overlay).unwrap();

        let other = Uuid::new_v4();
        let scratch = PathBuf::from(supervisor.overlay_path_for(other));
        std::fs::write(&scratch, b"private upper").unwrap();
        let process = ManagedProcess::new(Command::new("true").spawn().unwrap());
        let vm = RunningVm::new(process.pid, PathBuf::new(), process, None);
        supervisor.teardown_vm(other, &vm).unwrap();
        assert!(
            !scratch.exists(),
            "a non-golden overlay is removed on teardown"
        );
    }

    #[test]
    fn aborting_cold_golden_refill_caller_leaves_a_supervised_cleanup_worker() {
        let supervisor = test_supervisor();
        let class = supervisor.config.warm_pool.classes[0].clone();
        let pause = supervisor.pause_after_spawn_before_registry_attachment_for_test();
        let caller_supervisor = Arc::clone(&supervisor);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let caller = runtime.spawn(async move { caller_supervisor.create_golden(class).await });
        let wait_pause = pause.clone();
        runtime.block_on(async move {
            while !wait_pause.entered() {
                tokio::task::yield_now().await;
            }
        });

        pause.wait_until_entered();
        caller.abort();
        assert!(matches!(
            runtime.block_on(caller),
            Err(error) if error.is_cancelled()
        ));

        let stop_supervisor = Arc::clone(&supervisor);
        let (done_tx, done_rx) = mpsc::channel();
        let stopper = thread::spawn(move || done_tx.send(stop_supervisor.stop_all()).unwrap());
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "stop-all must enumerate the golden worker after its caller is aborted"
        );
        pause.release();
        done_rx
            .recv()
            .unwrap()
            .expect("the cancelled golden worker must finish cleanup");
        stopper.join().unwrap();
    }

    #[test]
    fn aborting_snapshot_restore_refill_caller_leaves_a_supervised_cleanup_worker() {
        let supervisor = test_supervisor();
        let class = supervisor.config.warm_pool.classes[0].clone();
        let pause = supervisor.pause_after_spawn_before_registry_attachment_for_test();
        let caller_supervisor = Arc::clone(&supervisor);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let caller = runtime.spawn(async move {
            caller_supervisor
                .spawn_warm_restore(class, "golden.snap".into())
                .await
        });
        let wait_pause = pause.clone();
        runtime.block_on(async move {
            while !wait_pause.entered() {
                tokio::task::yield_now().await;
            }
        });

        pause.wait_until_entered();
        caller.abort();
        assert!(matches!(
            runtime.block_on(caller),
            Err(error) if error.is_cancelled()
        ));

        let stop_supervisor = Arc::clone(&supervisor);
        let (done_tx, done_rx) = mpsc::channel();
        let stopper = thread::spawn(move || done_tx.send(stop_supervisor.stop_all()).unwrap());
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "stop-all must enumerate the restore worker after its caller is aborted"
        );
        pause.release();
        done_rx
            .recv()
            .unwrap()
            .expect("the cancelled restore worker must finish cleanup");
        stopper.join().unwrap();
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
    fn delete_waits_for_creating_claim_registered_under_lifecycle_gate() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let (claim_started_tx, claim_started_rx) = mpsc::channel();
        let (allow_claim_tx, allow_claim_rx) = mpsc::channel();
        let create_supervisor = Arc::clone(&supervisor);
        let creator = thread::spawn(move || {
            let ticket = test_runtime()
                .block_on(create_supervisor.begin_boot_with_registration(
                    id,
                    SpawnPurpose::Live,
                    ResourceShape::new(1, 1),
                    move || async move {
                        claim_started_tx.send(()).unwrap();
                        allow_claim_rx.recv().unwrap();
                        Ok(())
                    },
                ))
                .expect("the Creating registration must establish a boot entry");
            ticket.control.wait_for_cancellation();
            create_supervisor.complete_booting(id, &ticket.control, Ok(()));
        });

        claim_started_rx.recv().unwrap();
        let (delete_done_tx, delete_done_rx) = mpsc::channel();
        let delete_supervisor = Arc::clone(&supervisor);
        let deleter = thread::spawn(move || {
            delete_done_tx.send(delete_supervisor.stop_vm(id)).unwrap();
        });
        assert!(
            delete_done_rx.try_recv().is_err(),
            "DELETE must not overtake the Creating ownership claim"
        );

        allow_claim_tx.send(()).unwrap();
        delete_done_rx
            .recv()
            .unwrap()
            .expect("DELETE must cancel and wait for the registered boot");
        creator.join().unwrap();
        deleter.join().unwrap();
        assert!(!supervisor.is_running(id));
    }

    #[test]
    fn warm_handoff_and_stop_all_share_the_publication_gate() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let spec = spawn_config(false, Some(PathBuf::from("/rootfs.ext4")));
        let ticket = test_runtime()
            .block_on(supervisor.begin_boot_with_registration(
                id,
                SpawnPurpose::Refill,
                spec.resource_shape(),
                || async { Ok(()) },
            ))
            .unwrap();
        let process = ManagedProcess::new(Command::new("true").spawn().unwrap());
        supervisor.complete_booting(id, &ticket.control, Ok(()));
        supervisor.warm.lock().unwrap().push_back(WarmVm {
            id,
            vm: RunningVm::new(
                process.pid,
                PathBuf::from("warm-handoff.sock"),
                process,
                None,
            ),
            spec: spec.clone(),
        });

        let (publication_started_tx, publication_started_rx) = mpsc::channel();
        let (allow_publication_tx, allow_publication_rx) = mpsc::channel();
        let handoff_supervisor = Arc::clone(&supervisor);
        let handoff_task = Arc::new(OwnedTaskControl::new());
        let handoff = thread::spawn(move || {
            match test_runtime()
                .block_on(handoff_supervisor.take_warm_with_publication(
                    &spec,
                    &handoff_task,
                    |_| async { Ok(()) },
                    move |vm_id, _, _| async move {
                        publication_started_tx.send(()).unwrap();
                        allow_publication_rx.recv().unwrap();
                        Ok(vm_id)
                    },
                ))
                .unwrap()
            {
                WarmClaimOutcome::Published(id) => id,
                _ => panic!("warm handoff must publish"),
            }
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
    fn warm_registration_failure_never_dequeues_the_unregistered_vm() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let spec = spawn_config(false, Some(PathBuf::from("/rootfs.ext4")));
        let ticket = test_runtime()
            .block_on(supervisor.begin_boot_with_registration(
                id,
                SpawnPurpose::Refill,
                spec.resource_shape(),
                || async { Ok(()) },
            ))
            .unwrap();
        let process = ManagedProcess::new(Command::new("true").spawn().unwrap());
        supervisor.complete_booting(id, &ticket.control, Ok(()));
        supervisor.warm.lock().unwrap().push_back(WarmVm {
            id,
            vm: RunningVm::new(
                process.pid,
                PathBuf::from("warm-registration.sock"),
                process,
                None,
            ),
            spec: spec.clone(),
        });

        let (registration_started_tx, registration_started_rx) = mpsc::channel();
        let (finish_registration_tx, finish_registration_rx) = mpsc::channel();
        let handoff_supervisor = Arc::clone(&supervisor);
        let handoff_task = Arc::new(OwnedTaskControl::new());
        let handoff = thread::spawn(move || {
            test_runtime().block_on(handoff_supervisor.take_warm_with_publication(
                &spec,
                &handoff_task,
                move |_| async move {
                    registration_started_tx.send(()).unwrap();
                    finish_registration_rx.recv().unwrap();
                    Err(OrchError::Internal(
                        "injected Creating registration failure".into(),
                    ))
                },
                |_, _, _| async {
                    Err::<Uuid, PublicationFailure>(PublicationFailure(OrchError::Internal(
                        "unexpected warm publication".into(),
                    )))
                },
            ))
        });

        registration_started_rx.recv().unwrap();
        assert_eq!(
            supervisor.warm_count(1, 256),
            1,
            "a selected warm VM must remain in the warm registry until Creating is registered"
        );
        finish_registration_tx.send(()).unwrap();
        assert!(matches!(
            handoff.join().unwrap().unwrap(),
            WarmClaimOutcome::PreRuntimeFailure(_)
        ));
        assert_eq!(supervisor.warm_count(1, 256), 1);
        assert!(!supervisor.is_running(id));
    }

    #[test]
    fn failed_boot_cleanup_retains_its_scheduler_reservation() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let ticket = test_runtime()
            .block_on(supervisor.begin_boot_with_registration(
                id,
                SpawnPurpose::Refill,
                ResourceShape::new(1, 1),
                || async { Ok(()) },
            ))
            .unwrap();
        let retained_socket = PathBuf::from(format!("target/taritd-supervisor-test/retained-{id}"));
        std::fs::create_dir_all(&retained_socket).unwrap();
        let process = ManagedProcess::new(Command::new("true").spawn().unwrap());
        let vm = RunningVm::new(process.pid, retained_socket.clone(), process, None);

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
        assert!(supervisor.scheduler.is_reserved(id));
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
            let vm = RunningVm::new(
                process.pid,
                PathBuf::from("booting-publication.sock"),
                process,
                None,
            );
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
                    vm: RunningVm::new(
                        process.pid,
                        PathBuf::from("single-stop-publication.sock"),
                        process,
                        None,
                    ),
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
    fn restored_network_rebind_flushes_and_verifies_fresh_allocation() {
        let allocation = NetAlloc {
            idx: 3,
            vm_id: Uuid::new_v4(),
            tap: "tarit3".into(),
            host_ip: "172.16.0.13".into(),
            guest_ip: "172.16.0.14".into(),
            prefix: 30,
        };
        let command = restored_network_rebind_command(&allocation);
        assert!(command.starts_with("ip addr flush dev eth0 scope global"));
        assert!(command.contains("ip addr add 172.16.0.14/30 dev eth0"));
        assert!(command.contains("ip route replace default via 172.16.0.13"));
        assert!(command.contains("grep -F -q ' 172.16.0.14/30 '"));
        assert!(command.contains("grep -F -q 'default via 172.16.0.13 '"));
    }

    #[test]
    fn shared_exit_scan_releases_resources_and_emits_reconcile_event() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        supervisor.reserve_existing_for_test(id);
        let process = ManagedProcess::new(
            Command::new("sh")
                .arg("-c")
                .arg("exit 7")
                .spawn()
                .expect("spawn exiting VMM stand-in"),
        );
        let pid = process.pid;
        supervisor.running.lock().unwrap().insert(
            id,
            RunningVm::new(pid, PathBuf::new(), process.clone(), None),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let exit = loop {
            supervisor.scan_for_exited_processes();
            if let Some(exit) = supervisor.take_unexpected_exits().into_iter().next() {
                break exit;
            }
            assert!(Instant::now() < deadline, "exit scan did not reconcile");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(exit.id, id);
        assert_eq!(exit.pid, pid);
        assert!(exit.status.contains('7'));
        assert!(!supervisor.is_running(id));
        assert_eq!(supervisor.scheduler.usage(), Default::default());
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
        let error = wait_for_guest_ready(Duration::ZERO, || Ok(()), |_| Ok(false))
            .expect_err("an unresponsive guest must not pass the readiness gate");

        assert!(matches!(
            error,
            ReadinessWaitError::TimedOut(message) if message.contains("guest agent never became ready")
        ));
    }

    #[test]
    fn guest_readiness_gate_accepts_a_successful_probe() {
        let mut attempts = 0;

        wait_for_guest_ready(
            Duration::from_secs(1),
            || Ok(()),
            |_| {
                attempts += 1;
                Ok(true)
            },
        )
        .expect("a successful guest-agent probe must pass the readiness gate");

        assert_eq!(attempts, 1);
    }

    #[test]
    fn guest_readiness_gate_stops_when_refill_is_cancelled_between_probes() {
        let cancelled = AtomicBool::new(false);
        let mut attempts = 0;

        let error = wait_for_guest_ready(
            Duration::from_secs(1),
            || {
                if cancelled.load(Ordering::Acquire) {
                    return Err(shutdown_error());
                }
                Ok(())
            },
            |_| {
                attempts += 1;
                cancelled.store(true, Ordering::Release);
                Ok(false)
            },
        )
        .expect_err("a cancelled refill must stop waiting for guest readiness");

        assert_eq!(
            attempts, 1,
            "cancellation must prevent another readiness probe"
        );
        assert!(matches!(
            error,
            ReadinessWaitError::Cancelled(OrchError::Overloaded { .. })
        ));
    }

    #[test]
    fn boot_readiness_uses_the_full_guest_ready_window() {
        assert_eq!(
            readiness_timeout(ReadinessCheck::Boot),
            GUEST_READY_TIMEOUT,
            "newly booted, refilled, and golden-builder VMs need the full readiness window"
        );
    }

    #[test]
    fn resume_readiness_is_bounded() {
        assert_eq!(
            readiness_timeout(ReadinessCheck::Resume),
            RESUME_READY_TIMEOUT,
            "resume must prove the guest agent is usable without inheriting the full boot window"
        );
        assert!(RESUME_READY_TIMEOUT < GUEST_READY_TIMEOUT);
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
    fn readiness_request_timeout_is_capped_by_the_per_probe_limit() {
        assert_eq!(
            readiness_request_timeout(Duration::from_secs(20)),
            GUEST_READY_EXEC_TIMEOUT
        );
        assert_eq!(
            readiness_request_timeout(Duration::from_millis(200)),
            Duration::from_millis(200)
        );
    }

    #[test]
    fn readiness_poll_sleep_never_exceeds_the_remaining_deadline() {
        assert_eq!(
            readiness_poll_sleep(Duration::from_millis(200)),
            GUEST_READY_POLL_INTERVAL
        );
        assert_eq!(
            readiness_poll_sleep(Duration::from_millis(5)),
            Duration::from_millis(5)
        );
        assert_eq!(readiness_poll_sleep(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn socket_wait_backoff_keeps_startup_quantization_below_five_milliseconds() {
        let mut delay = SOCKET_WAIT_INITIAL;
        let mut observed = Vec::new();
        for _ in 0..6 {
            observed.push(delay);
            delay = next_socket_wait_delay(delay);
        }
        assert_eq!(
            observed,
            vec![
                Duration::from_millis(1),
                Duration::from_millis(2),
                Duration::from_millis(4),
                Duration::from_millis(4),
                Duration::from_millis(4),
                Duration::from_millis(4),
            ]
        );
    }

    #[test]
    fn snapshot_disk_artifact_survives_source_deletion_and_seeds_private_restores() {
        let dir = PathBuf::from(format!("target/snapshot-disk-bundle-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create test directory");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("protect test directory");
        let source = dir.join("live.cow");
        let bundle = dir.join("snapshot.cow");
        let clone_a = dir.join("clone-a.cow");
        let clone_b = dir.join("clone-b.cow");
        let checkpoint = b"checkpoint-disk-state";

        let mut source_options = OpenOptions::new();
        source_options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let mut source_file = source_options.open(&source).expect("create live upper");
        source_file
            .write_all(checkpoint)
            .expect("write checkpoint state");
        source_file.sync_all().expect("sync live upper");
        drop(source_file);

        let bundle_artifact =
            copy_private_artifact(&source, &bundle).expect("capture snapshot disk artifact");
        std::fs::remove_file(&source).expect("delete source VM upper");
        let clone_a_artifact =
            copy_private_artifact(&bundle, &clone_a).expect("seed first restore");
        let clone_b_artifact =
            copy_private_artifact(&bundle, &clone_b).expect("seed second restore");

        assert_ne!(clone_a, clone_b, "restores must use unique writable uppers");
        assert_eq!(std::fs::read(&clone_a).unwrap(), checkpoint);
        assert_eq!(std::fs::read(&clone_b).unwrap(), checkpoint);
        std::fs::write(&clone_a, b"first-restore-private").expect("mutate first restore");
        assert_eq!(
            std::fs::read(&clone_b).unwrap(),
            checkpoint,
            "restores must not share writable disk state"
        );
        assert_eq!(
            std::fs::read(&bundle).unwrap(),
            checkpoint,
            "restore writes must not mutate the snapshot artifact"
        );

        clone_a_artifact.remove().unwrap();
        clone_b_artifact.remove().unwrap();
        bundle_artifact.remove().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn durable_ram_bundle_survives_removal_of_vmm_scratch_name() {
        let dir = PathBuf::from(format!("target/snapshot-ram-bundle-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create test directory");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("protect test directory");
        let scratch = dir.join("vmm-snap-123-456.snap");
        let durable = dir.join(format!("bundle-{}.ram", Uuid::new_v4()));
        let bytes = b"RAM snapshot contents";
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let mut scratch_file = options.open(&scratch).unwrap();
        scratch_file.write_all(bytes).unwrap();
        scratch_file.sync_all().unwrap();
        drop(scratch_file);

        let scratch_artifact = OwnedArtifact::capture(&scratch).unwrap();
        let durable_artifact = copy_private_artifact(&scratch, &durable).unwrap();
        assert!(scratch_artifact.remove().unwrap());
        assert!(!scratch.exists(), "released VMM scratch must be removed");
        assert_eq!(
            std::fs::read(&durable).unwrap(),
            bytes,
            "durable RAM bundle must not depend on the VMM scratch name"
        );

        durable_artifact.remove().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn snapshot_disk_capture_preserves_sparse_virtual_regions() {
        let dir = PathBuf::from(format!("target/snapshot-disk-sparse-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create test directory");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("protect test directory");
        let source = dir.join("live.cow");
        let snapshot = dir.join("snapshot.cow");
        let mut source_options = OpenOptions::new();
        source_options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let source_file = source_options.open(&source).expect("create sparse source");
        let virtual_len = 1024 * 1024 * 1024u64;
        source_file.set_len(virtual_len).unwrap();
        source_file.write_all_at(b"header", 0).unwrap();
        source_file.write_all_at(b"tail", virtual_len - 4).unwrap();
        source_file.sync_all().unwrap();
        drop(source_file);

        let artifact =
            copy_private_artifact(&source, &snapshot).expect("capture sparse snapshot upper");
        let metadata = std::fs::metadata(&snapshot).unwrap();
        assert_eq!(metadata.len(), virtual_len);
        assert!(
            metadata.blocks() * 512 < virtual_len / 8,
            "snapshot capture unexpectedly allocated the virtual disk: {} blocks",
            metadata.blocks()
        );
        let snapshot_file = File::open(&snapshot).unwrap();
        let mut header = [0u8; 6];
        snapshot_file.read_exact_at(&mut header, 0).unwrap();
        assert_eq!(&header, b"header");
        let mut tail = [0u8; 4];
        snapshot_file
            .read_exact_at(&mut tail, virtual_len - 4)
            .unwrap();
        assert_eq!(&tail, b"tail");

        artifact.remove().unwrap();
        std::fs::remove_file(source).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn golden_artifact_cleanup_removes_snapshot_and_overlay() {
        let dir = PathBuf::from(format!("target/golden-artifact-cleanup-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create test directory");
        let snapshot = dir.join("golden.snap");
        let overlay = dir.join("golden.overlay");
        std::fs::write(&snapshot, b"snapshot").expect("write snapshot");
        std::fs::write(&overlay, b"overlay").expect("write overlay");

        let artifacts = [
            OwnedArtifact::capture(&snapshot).expect("capture snapshot"),
            OwnedArtifact::capture(&overlay).expect("capture overlay"),
        ];
        cleanup_golden_artifacts(artifacts);

        assert!(!snapshot.exists(), "golden snapshot must be removed");
        assert!(!overlay.exists(), "golden overlay must be removed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn golden_artifact_cleanup_preserves_replacements() {
        let dir = PathBuf::from(format!(
            "target/golden-artifact-replacement-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create test directory");
        let snapshot = dir.join("vmm-snap-123-456.snap");
        std::fs::write(&snapshot, b"owned snapshot").expect("write owned snapshot");
        let artifact = OwnedArtifact::capture(&snapshot).expect("capture owned artifact");
        std::fs::remove_file(&snapshot).expect("replace owned artifact");
        std::fs::write(&snapshot, b"replacement").expect("write replacement");

        cleanup_golden_artifacts([artifact]);

        assert_eq!(
            std::fs::read(&snapshot).expect("replacement survives cleanup"),
            b"replacement"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn golden_cancellation_removes_registry_entry_and_preserves_replacement() {
        let dir = PathBuf::from(format!(
            "target/golden-artifact-cancellation-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create test directory");
        let snapshot = dir.join("vmm-snap-123-456.snap");
        std::fs::write(&snapshot, b"owned snapshot").expect("write owned snapshot");
        let artifact = OwnedArtifact::capture(&snapshot).expect("capture golden artifact");
        let key = (artifact.path.clone(), artifact.identity());
        let mut registry = vec![artifact];

        let cancelled = take_matching_artifacts(&mut registry, &[key]);
        assert!(
            registry.is_empty(),
            "cancellation must remove the registry entry"
        );
        std::fs::remove_file(&snapshot).expect("replace the cancelled artifact");
        std::fs::write(&snapshot, b"replacement").expect("write replacement");
        cleanup_golden_artifacts(cancelled);

        assert_eq!(std::fs::read(&snapshot).unwrap(), b"replacement");
        let _ = std::fs::remove_dir_all(dir);
    }
}
