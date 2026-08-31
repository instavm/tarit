use crate::config::{Config, WarmClass};
use crate::disk::{
    ArtifactReferences, DiskPressure, DiskPressureSnapshot, DiskReservation, GcReport, PathGrowth,
};
use crate::net::{NetAlloc, NetProvisioner};
use crate::scheduler::{ReservationError, ResourceShape, Scheduler};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};
use std::time::{Duration, Instant};
use tarit_types::{OrchError, VmRecord, VmRuntimeLayout, VmStatus};
use tarit_vmm_client::{
    KernelConfig, MemoryConfig, NetConfig, ScratchIdentity, VcpuConfig, VmConfig, VmmClient,
    VolumeConfig,
};
use tarit_volume::{AccessMode, PreparedBlockAttachment};
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
const TEARDOWN_KILL_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for VMM ops that copy guest RAM (suspend, snapshot) or fault it
/// back in (a suspend right after resume). These are silent on the socket for
/// the whole copy, so they get a generous request deadline instead of the
/// 5-second per-read stream timeout of a plain client, which a multi-GiB
/// guest cannot meet.
const LIFECYCLE_OP_TIMEOUT: Duration = Duration::from_secs(600);
/// Margin on top of the guest-side exec timeout for VMM scheduling and
/// response marshalling. The plain client's 5s per-read socket timeout is
/// shorter than most exec budgets, so exec must use a deadline client sized
/// to the command's own timeout.
const EXEC_OP_MARGIN: Duration = Duration::from_secs(10);
/// Status is a fast RPC, but under host CPU contention the VMM can hold the
/// response past the plain client's 5s per-read timeout. Give it a modest
/// real deadline instead of surfacing EAGAIN as a 500.
const STATUS_OP_TIMEOUT: Duration = Duration::from_secs(30);
const RESTORE_NETWORK_EXEC_TIMEOUT: Duration = Duration::from_secs(5);
const NORMAL_CGROUP_CPU_WEIGHT: u64 = 100;
#[cfg(target_os = "linux")]
const TUNSETIFF: libc::c_ulong = 0x400454ca;
#[cfg(target_os = "linux")]
const IFF_TAP: u16 = 0x0002;
#[cfg(target_os = "linux")]
const IFF_NO_PI: u16 = 0x1000;

#[cfg(target_os = "linux")]
#[repr(C)]
struct TapIfreq {
    name: [u8; 16],
    flags: u16,
    _pad: [u8; 22],
}

#[cfg(target_os = "linux")]
fn open_inherited_tap(tap_name: &str) -> Result<OwnedFd, OrchError> {
    if tap_name.is_empty() || tap_name.len() > 15 || tap_name.as_bytes().contains(&0) {
        return Err(OrchError::Internal(format!(
            "invalid TAP name for inherited descriptor: {tap_name:?}"
        )));
    }
    let path = std::ffi::CString::new("/dev/net/tun").expect("static TUN path");
    // SAFETY: `path` is a valid NUL-terminated string and `open` does not
    // retain the pointer after returning.
    let raw_fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if raw_fd < 0 {
        return Err(OrchError::Internal(format!(
            "open /dev/net/tun for {tap_name}: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: ownership of the successfully opened descriptor is transferred
    // exactly once to OwnedFd.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let mut ifreq = TapIfreq {
        name: [0; 16],
        flags: IFF_TAP | IFF_NO_PI,
        _pad: [0; 22],
    };
    ifreq.name[..tap_name.len()].copy_from_slice(tap_name.as_bytes());
    // SAFETY: `fd` is an open TUN descriptor and `ifreq` is a valid writable
    // TUNSETIFF request that remains alive for the duration of the ioctl.
    if unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETIFF as _, &mut ifreq) } < 0 {
        return Err(OrchError::Internal(format!(
            "attach inherited TAP queue for {tap_name}: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(fd)
}

#[cfg(target_os = "linux")]
fn open_inherited_kvm() -> Result<File, OrchError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open("/dev/kvm")
        .map_err(|error| OrchError::Internal(format!("open inherited KVM device: {error}")))?;
    if !file
        .metadata()
        .map_err(|error| OrchError::Internal(format!("inspect inherited KVM device: {error}")))?
        .file_type()
        .is_char_device()
    {
        return Err(OrchError::Internal(
            "inherited KVM path is not a character device".into(),
        ));
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
fn clear_cloexec_for_child(fd: libc::c_int) -> std::io::Result<()> {
    let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if descriptor_flags < 0
        || unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor_flags & !libc::FD_CLOEXEC) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn graceful_stop_vmm(socket_path: &Path) {
    if socket_path.as_os_str().is_empty() || !socket_path.exists() {
        return;
    }

    let _ = VmmClient::new(socket_path)
        .with_request_timeout(TEARDOWN_STOP_TIMEOUT)
        .stop();
}

fn is_process_gone(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOENT) | Some(libc::ESRCH) | Some(libc::ENOTDIR)
    ) || error.kind() == std::io::ErrorKind::NotFound
}

#[cfg(any(target_os = "linux", test))]
#[cfg(test)]
fn tolerate_process_disappearance<T>(result: std::io::Result<T>) -> std::io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if is_process_gone(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn valid_process_id(pid: u32) -> bool {
    pid != 0 && pid <= libc::pid_t::MAX as u32
}

#[cfg(any(target_os = "linux", test))]
fn parse_process_id(raw: &str) -> Option<u32> {
    if raw.is_empty() || !raw.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    raw.parse::<u32>().ok().filter(|pid| valid_process_id(*pid))
}

#[cfg(any(target_os = "linux", test))]
fn proc_pid_entries(proc_root: &Path) -> std::io::Result<Vec<(u32, PathBuf)>> {
    let mut processes = Vec::new();
    for entry in std::fs::read_dir(proc_root)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(%error, "skip unreadable proc directory entry");
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(pid) = parse_process_id(name) else {
            continue;
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                tracing::debug!(pid, %error, "skip vanished or unreadable proc process entry");
                continue;
            }
        };
        if file_type.is_dir() {
            processes.push((pid, entry.path()));
        }
    }
    Ok(processes)
}

#[cfg(any(target_os = "linux", test))]
fn parse_cgroup_processes(contents: &str, source: &Path) -> std::io::Result<HashSet<u32>> {
    contents
        .lines()
        .map(|raw| {
            let value = raw.trim();
            parse_process_id(value).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "invalid positive process ID {raw:?} in {}",
                        source.display()
                    ),
                )
            })
        })
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn parse_process_parent(contents: &str, source: &Path) -> std::io::Result<u32> {
    let raw = contents
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .map(str::trim)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("missing PPid in {}", source.display()),
            )
        })?;
    raw.parse::<u32>().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid PPid {raw:?} in {}", source.display()),
        )
    })
}

#[cfg(target_os = "linux")]
fn process_parent_pid(pid: u32) -> std::io::Result<u32> {
    let path = PathBuf::from(format!("/proc/{pid}/status"));
    let contents = std::fs::read_to_string(&path)?;
    parse_process_parent(&contents, &path)
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug)]
struct ExpectedExecutable {
    path: PathBuf,
    device: u64,
    inode: u64,
}

#[cfg(any(target_os = "linux", test))]
impl ExpectedExecutable {
    fn resolve(configured: &Path) -> std::io::Result<Self> {
        let resolved = if configured.as_os_str().as_bytes().contains(&b'/') {
            configured.to_path_buf()
        } else {
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .map(|directory| directory.join(configured))
                .find(|candidate| {
                    std::fs::metadata(candidate)
                        .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
                })
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!(
                            "configured VMM executable {} was not found in PATH",
                            configured.display()
                        ),
                    )
                })?
        };
        let metadata = std::fs::metadata(&resolved)?;
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "configured VMM executable {} is not a regular file",
                    resolved.display()
                ),
            ));
        }
        Ok(Self {
            path: std::fs::canonicalize(&resolved).unwrap_or(resolved),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug)]
enum ProcessVerificationError {
    Gone {
        action: String,
        error: std::io::Error,
    },
    Rejected(String),
    Inspect {
        action: String,
        error: std::io::Error,
    },
}

#[cfg(any(target_os = "linux", test))]
impl ProcessVerificationError {
    fn from_io(action: impl Into<String>, error: std::io::Error) -> Self {
        let action = action.into();
        if is_process_gone(&error) {
            Self::Gone { action, error }
        } else {
            Self::Inspect { action, error }
        }
    }

    fn is_gone(&self) -> bool {
        matches!(self, Self::Gone { .. })
    }

    #[cfg(test)]
    fn is_permission_denied(&self) -> bool {
        matches!(
            self,
            Self::Inspect { error, .. }
                if error.kind() == std::io::ErrorKind::PermissionDenied
        )
    }
}

#[cfg(any(target_os = "linux", test))]
impl std::fmt::Display for ProcessVerificationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone { action, error } | Self::Inspect { action, error } => {
                write!(formatter, "{action}: {error}")
            }
            Self::Rejected(reason) => formatter.write_str(reason),
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn verified_process_cmdline(
    pid: u32,
    proc_dir: &Path,
    executable: &ExpectedExecutable,
    allowed_uids: &HashSet<u32>,
) -> Result<Vec<u8>, ProcessVerificationError> {
    if !valid_process_id(pid) {
        return Err(ProcessVerificationError::Rejected(format!(
            "invalid VMM PID {pid}"
        )));
    }
    let process_metadata = std::fs::metadata(proc_dir).map_err(|error| {
        ProcessVerificationError::from_io(format!("inspect VMM PID {pid}"), error)
    })?;
    let process_uid = process_metadata.uid();
    if !allowed_uids.contains(&process_uid) {
        return Err(ProcessVerificationError::Rejected(format!(
            "VMM PID {pid} is owned by unexpected uid {process_uid}"
        )));
    }
    let executable_path = proc_dir.join("exe");
    let process_executable = std::fs::metadata(&executable_path).map_err(|error| {
        ProcessVerificationError::from_io(format!("inspect executable for VMM PID {pid}"), error)
    })?;
    let same_file = process_executable.dev() == executable.device
        && process_executable.ino() == executable.inode;
    let same_path = if same_file {
        true
    } else {
        let link = std::fs::read_link(&executable_path).map_err(|error| {
            ProcessVerificationError::from_io(
                format!("read executable link for VMM PID {pid}"),
                error,
            )
        })?;
        let raw = link.as_os_str().as_bytes();
        let raw = raw.strip_suffix(b" (deleted)").unwrap_or(raw);
        Path::new(std::ffi::OsStr::from_bytes(raw)) == executable.path
    };
    if !same_path {
        return Err(ProcessVerificationError::Rejected(format!(
            "VMM PID {pid} is not running configured executable {}",
            executable.path.display()
        )));
    }
    std::fs::read(proc_dir.join("cmdline")).map_err(|error| {
        ProcessVerificationError::from_io(format!("read /proc/{pid}/cmdline"), error)
    })
}

/// Confirm that `pid` is a live VMM process that owns `socket_path`, guarding
/// re-adoption against PID reuse. taritd launches every VMM with
/// `serve --socket <socket_path>`, so the socket path must appear verbatim in
/// the process command line; a recycled PID running something else will not
/// match and is refused rather than adopted.
#[cfg(any(target_os = "linux", test))]
fn verify_live_vmm(
    pid: u32,
    socket_path: &Path,
    jail_root: Option<&Path>,
    vmm_bin: &Path,
    allowed_uids: &HashSet<u32>,
) -> Result<(), ProcessVerificationError> {
    let executable = ExpectedExecutable::resolve(vmm_bin).map_err(|error| {
        ProcessVerificationError::from_io(
            format!("resolve configured VMM executable {}", vmm_bin.display()),
            error,
        )
    })?;
    let cmdline = verified_process_cmdline(
        pid,
        Path::new(&format!("/proc/{pid}")),
        &executable,
        allowed_uids,
    )?;
    let args = cmdline.split(|byte| *byte == 0).collect::<Vec<_>>();
    let owns_socket = args
        .iter()
        .any(|arg| *arg == socket_path.as_os_str().as_bytes())
        || jail_root.is_some_and(|root| {
            args.iter().any(|arg| *arg == JAIL_SOCKET_PATH.as_bytes())
                && args.iter().any(|arg| *arg == root.as_os_str().as_bytes())
        });
    if !owns_socket {
        return Err(ProcessVerificationError::Rejected(format!(
            "PID {pid} does not own control socket {}; refusing to adopt a reused PID",
            socket_path.display()
        )));
    }
    Ok(())
}

#[cfg(all(not(target_os = "linux"), not(test)))]
fn verify_live_vmm(
    _pid: u32,
    _socket_path: &Path,
    _jail_root: Option<&Path>,
    _vmm_bin: &Path,
    _allowed_uids: &HashSet<u32>,
) -> Result<(), String> {
    Err("live VMM verification requires Linux pidfds and /proc".into())
}

#[cfg(any(target_os = "linux", test))]
fn verify_legacy_nonjailed_vmm(
    pid: u32,
    socket_path: &Path,
    vmm_bin: &Path,
) -> Result<(), ProcessVerificationError> {
    let allowed_uids = HashSet::from([unsafe { libc::geteuid() }]);
    verify_live_vmm(pid, socket_path, None, vmm_bin, &allowed_uids)?;
    let executable = ExpectedExecutable::resolve(vmm_bin).map_err(|error| {
        ProcessVerificationError::from_io(
            format!("resolve configured VMM executable {}", vmm_bin.display()),
            error,
        )
    })?;
    let cmdline = verified_process_cmdline(
        pid,
        Path::new(&format!("/proc/{pid}")),
        &executable,
        &allowed_uids,
    )?;
    let args = cmdline.split(|byte| *byte == 0).collect::<Vec<_>>();
    if args
        .iter()
        .any(|arg| *arg == b"--jail" || *arg == b"--jail-root")
    {
        return Err(ProcessVerificationError::Rejected(format!(
            "VMM PID {pid} has jail arguments and is not a legacy non-jailed runtime"
        )));
    }
    if !args.iter().any(|arg| *arg == b"serve") {
        return Err(ProcessVerificationError::Rejected(format!(
            "VMM PID {pid} is missing the serve subcommand"
        )));
    }
    let socket_args = args
        .windows(2)
        .filter_map(|pair| (pair[0] == b"--socket").then_some(pair[1]))
        .collect::<Vec<_>>();
    if socket_args.len() != 1 || socket_args[0] != socket_path.as_os_str().as_bytes() {
        return Err(ProcessVerificationError::Rejected(format!(
            "VMM PID {pid} does not have one exact --socket {} argument",
            socket_path.display()
        )));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn verify_owned_vmm(
    pid: u32,
    socket_path: &Path,
    jail_root: Option<&Path>,
    vmm_bin: &Path,
    jail_uid: Option<u32>,
) -> Result<(), ProcessVerificationError> {
    let Some(jail_root) = jail_root else {
        return verify_legacy_nonjailed_vmm(pid, socket_path, vmm_bin);
    };

    let mut allowed_uids = HashSet::from([unsafe { libc::geteuid() }]);
    allowed_uids.extend(jail_uid);
    verify_live_vmm(pid, socket_path, Some(jail_root), vmm_bin, &allowed_uids)?;
    let executable = ExpectedExecutable::resolve(vmm_bin).map_err(|error| {
        ProcessVerificationError::from_io(
            format!("resolve configured VMM executable {}", vmm_bin.display()),
            error,
        )
    })?;
    let cmdline = verified_process_cmdline(
        pid,
        Path::new(&format!("/proc/{pid}")),
        &executable,
        &allowed_uids,
    )?;
    let args = cmdline.split(|byte| *byte == 0).collect::<Vec<_>>();
    if !args.iter().any(|arg| *arg == b"serve") {
        return Err(ProcessVerificationError::Rejected(format!(
            "VMM PID {pid} is missing the serve subcommand"
        )));
    }
    let socket_args = args
        .windows(2)
        .filter_map(|pair| (pair[0] == b"--socket").then_some(pair[1]))
        .collect::<Vec<_>>();
    if socket_args.len() != 1 || socket_args[0] != JAIL_SOCKET_PATH.as_bytes() {
        return Err(ProcessVerificationError::Rejected(format!(
            "VMM PID {pid} does not have one exact --socket {JAIL_SOCKET_PATH} argument"
        )));
    }
    let jail_args = args
        .windows(2)
        .filter_map(|pair| (pair[0] == b"--jail" || pair[0] == b"--jail-root").then_some(pair[1]))
        .collect::<Vec<_>>();
    if jail_args.len() != 1 || jail_args[0] != jail_root.as_os_str().as_bytes() {
        return Err(ProcessVerificationError::Rejected(format!(
            "VMM PID {pid} does not have one exact jail argument for {}",
            jail_root.display()
        )));
    }
    Ok(())
}

#[cfg(all(not(target_os = "linux"), not(test)))]
fn verify_owned_vmm(
    pid: u32,
    socket_path: &Path,
    jail_root: Option<&Path>,
    vmm_bin: &Path,
    _jail_uid: Option<u32>,
) -> Result<(), String> {
    verify_live_vmm(pid, socket_path, jail_root, vmm_bin, &HashSet::new())
}

fn legacy_layout_drain_required(record: &VmRecord, reason: impl std::fmt::Display) -> OrchError {
    OrchError::Internal(format!(
        "legacy active VM {} has no runtime layout and cannot be inferred unambiguously ({reason}); drain required before upgrade",
        record.id
    ))
}

pub(crate) fn infer_legacy_nonjailed_runtime_layout(
    config: &Config,
    record: &VmRecord,
) -> Result<Option<VmRuntimeLayout>, OrchError> {
    if record.runtime_layout.is_some()
        || record.host_id != config.host_id
        || !matches!(
            record.status,
            VmStatus::Creating | VmStatus::Running | VmStatus::Paused | VmStatus::Suspended
        )
    {
        return Ok(None);
    }
    if config.vm_jail.is_some() {
        return Err(legacy_layout_drain_required(
            record,
            "the current host enables VM jails, but legacy rows only describe the pre-jail layout",
        ));
    }

    let socket_path = record
        .socket_path
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| {
            legacy_layout_drain_required(record, "the persisted control socket is missing")
        })?;
    let legacy_socket_name = format!("{}.sock", record.id);
    if socket_path.file_name().and_then(|name| name.to_str()) != Some(&legacy_socket_name) {
        return Err(legacy_layout_drain_required(
            record,
            format!(
                "persisted control socket {} is not the legacy UUID-scoped socket",
                socket_path.display()
            ),
        ));
    }
    let runtime_root = socket_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            legacy_layout_drain_required(record, "the persisted control socket has no runtime root")
        })?;
    let socket_metadata = std::fs::symlink_metadata(&socket_path).map_err(|error| {
        legacy_layout_drain_required(
            record,
            format!(
                "inspect persisted control socket {}: {error}",
                socket_path.display()
            ),
        )
    })?;
    if !socket_metadata.file_type().is_socket() {
        return Err(legacy_layout_drain_required(
            record,
            format!(
                "persisted control socket {} is not a Unix socket",
                socket_path.display()
            ),
        ));
    }
    infer_legacy_nonjailed_runtime_layout_platform(
        record,
        socket_path,
        runtime_root,
        socket_metadata,
        &config.vmm_bin,
    )
    .map(Some)
}

#[cfg(not(target_os = "linux"))]
fn infer_legacy_nonjailed_runtime_layout_platform(
    record: &VmRecord,
    _socket_path: PathBuf,
    _runtime_root: PathBuf,
    _socket_metadata: std::fs::Metadata,
    _vmm_bin: &Path,
) -> Result<VmRuntimeLayout, OrchError> {
    Err(legacy_layout_drain_required(
        record,
        "live process ownership verification requires Linux pidfds and /proc",
    ))
}

#[cfg(target_os = "linux")]
fn infer_legacy_nonjailed_runtime_layout_platform(
    record: &VmRecord,
    socket_path: PathBuf,
    runtime_root: PathBuf,
    socket_metadata: std::fs::Metadata,
    vmm_bin: &Path,
) -> Result<VmRuntimeLayout, OrchError> {
    let pid = record
        .pid
        .ok_or_else(|| legacy_layout_drain_required(record, "the persisted VMM PID is missing"))?;
    let _pidfd = pidfd_open(pid).map_err(|error| {
        legacy_layout_drain_required(record, format!("pin persisted VMM PID {pid}: {error}"))
    })?;
    let service_uid = unsafe { libc::geteuid() };
    let process_uid = std::fs::metadata(format!("/proc/{pid}"))
        .map_err(|error| {
            legacy_layout_drain_required(
                record,
                format!("inspect persisted VMM PID {pid}: {error}"),
            )
        })?
        .uid();
    if process_uid != service_uid || socket_metadata.uid() != service_uid {
        return Err(legacy_layout_drain_required(
            record,
            format!(
                "VMM PID {pid} and control socket {} are not owned by taritd uid {service_uid}",
                socket_path.display()
            ),
        ));
    }
    verify_legacy_nonjailed_vmm(pid, &socket_path, vmm_bin)
        .map_err(|reason| legacy_layout_drain_required(record, reason))?;

    if record.rootfs_path.as_deref().is_some_and(str::is_empty) {
        return Err(legacy_layout_drain_required(
            record,
            "the persisted rootfs path is empty",
        ));
    }
    let legacy_overlay = runtime_root
        .join("overlays")
        .join(format!("{}.cow", record.id));
    let overlay_path = if record.rootfs_path.is_some() {
        let metadata = std::fs::symlink_metadata(&legacy_overlay).map_err(|error| {
            legacy_layout_drain_required(
                record,
                format!(
                    "inspect legacy overlay {} derived from the persisted socket and rootfs: {error}",
                    legacy_overlay.display()
                ),
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(legacy_layout_drain_required(
                record,
                format!(
                    "legacy overlay {} is not a regular file",
                    legacy_overlay.display()
                ),
            ));
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(legacy_layout_drain_required(
                record,
                format!(
                    "legacy overlay {} is not owned by taritd",
                    legacy_overlay.display()
                ),
            ));
        }
        Some(legacy_overlay.display().to_string())
    } else {
        match std::fs::symlink_metadata(&legacy_overlay) {
            Ok(_) => {
                return Err(legacy_layout_drain_required(
                    record,
                    format!(
                        "legacy overlay {} exists although the persisted VM has no rootfs",
                        legacy_overlay.display()
                    ),
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(legacy_layout_drain_required(
                    record,
                    format!(
                        "inspect unexpected legacy overlay {}: {error}",
                        legacy_overlay.display()
                    ),
                ))
            }
        }
    };
    let mut artifact_paths = vec![socket_path.display().to_string()];
    if let Some(path) = &overlay_path {
        artifact_paths.push(path.clone());
    }
    Ok(VmRuntimeLayout {
        overlay_path,
        jail_path: None,
        artifact_paths,
    })
}

/// Pin the exact process instance behind `pid` with a pidfd. Once taritd holds
/// this descriptor the kernel keeps the PID from being recycled, so a later
/// SIGKILL through the pidfd can never land on an unrelated process that reused
/// the number. Re-adoption runs only on Linux hosts; the non-Linux stub exists
/// so the crate still builds on developer machines.
#[cfg(target_os = "linux")]
fn pidfd_open(pid: u32) -> std::io::Result<OwnedFd> {
    use std::os::fd::FromRawFd;
    if !valid_process_id(pid) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid positive process ID {pid}"),
        ));
    }
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
pub struct VmDataVolumeConfig {
    pub id: Uuid,
    pub provider: String,
    pub size_bytes: u64,
    pub read_only: bool,
    pub generation: u64,
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
    /// Desired host-enforced egress policy, installed before guest startup.
    pub egress_allowlist: Vec<String>,
    pub egress_allow_existing: bool,
    pub data_volumes: Vec<VmDataVolumeConfig>,
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
            egress_allowlist: Vec::new(),
            egress_allow_existing: false,
            data_volumes: Vec::new(),
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
            egress_allowlist: Vec::new(),
            egress_allow_existing: false,
            data_volumes: Vec::new(),
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

#[derive(Debug, Clone)]
pub(crate) struct ReadoptWarning {
    pub(crate) id: Uuid,
    pub(crate) reason: String,
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

const JAIL_MARKER_VERSION: u32 = 1;
const JAIL_BASE_MARKER_VERSION: u32 = 1;
const JAIL_SOCKET_PATH: &str = "/run/vmm.sock";
const JAIL_KERNEL_PATH: &str = "/assets/kernel";
const JAIL_ROOTFS_PATH: &str = "/assets/rootfs";
const JAIL_OVERLAY_PATH: &str = "/assets/rootfs.cow";
const JAIL_RESTORE_PATH: &str = "/assets/restore.ram";
const JAIL_RESTORE_INTEGRITY_PATH: &str = "/assets/restore.integrity";

fn jail_restore_overlay_path(id: Uuid) -> String {
    format!("/assets/restored-rootfs-{id}.cow")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JailMarker {
    version: u32,
    vm_id: Uuid,
    uid: u32,
    gid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JailBaseMarker {
    version: u32,
    uid: u32,
}

#[derive(Debug, Clone)]
struct JailLease {
    root: PathBuf,
    uid: u32,
    gid: u32,
}

#[derive(Default)]
struct JailLeaseState {
    by_vm: HashMap<Uuid, JailLease>,
    by_slot: HashMap<u32, Uuid>,
}

struct JailManager {
    config: crate::config::VmJailConfig,
    state: Mutex<JailLeaseState>,
}

impl JailManager {
    fn new(config: crate::config::VmJailConfig) -> Result<Self, OrchError> {
        ensure_private_jail_base(&config.base_dir)?;
        let mut state = JailLeaseState::default();
        for entry in std::fs::read_dir(&config.base_dir).map_err(|error| {
            OrchError::Internal(format!(
                "scan VM jail base {}: {error}",
                config.base_dir.display()
            ))
        })? {
            let entry = entry.map_err(|error| {
                OrchError::Internal(format!(
                    "scan VM jail base {}: {error}",
                    config.base_dir.display()
                ))
            })?;
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let file_type = entry.file_type().map_err(|error| {
                OrchError::Internal(format!(
                    "inspect VM jail entry {}: {error}",
                    entry.path().display()
                ))
            })?;
            if name
                .strip_prefix(".tarit-stage-")
                .and_then(|id| Uuid::parse_str(id).ok())
                .is_some()
            {
                if !file_type.is_dir() || file_type.is_symlink() {
                    return Err(OrchError::Internal(format!(
                        "VM jail staging entry {} must be a real directory, not a symlink",
                        entry.path().display()
                    )));
                }
                std::fs::remove_dir_all(entry.path()).map_err(|error| {
                    OrchError::Internal(format!(
                        "remove interrupted VM jail staging entry {}: {error}",
                        entry.path().display()
                    ))
                })?;
                continue;
            }
            let Some(vm_id) = name
                .strip_prefix("tarit-")
                .and_then(|id| Uuid::parse_str(id).ok())
            else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                return Err(OrchError::Internal(format!(
                    "VM jail entry {} must be a real directory, not a symlink",
                    entry.path().display()
                )));
            }
            let root = entry.path();
            let marker = read_jail_marker(&root)?;
            if marker.version != JAIL_MARKER_VERSION || marker.vm_id != vm_id {
                return Err(OrchError::Internal(format!(
                    "invalid VM jail marker in {}",
                    root.display()
                )));
            }
            let uid_slot = marker.uid.checked_sub(config.uid_base);
            let gid_slot = marker.gid.checked_sub(config.gid_base);
            let Some(slot) = uid_slot.filter(|slot| Some(*slot) == gid_slot) else {
                return Err(OrchError::Internal(format!(
                    "VM jail {} has an identity outside the configured paired UID/GID range",
                    root.display()
                )));
            };
            if slot >= config.id_count || state.by_slot.insert(slot, vm_id).is_some() {
                return Err(OrchError::Internal(format!(
                    "VM jail {} has a duplicate or out-of-range identity lease",
                    root.display()
                )));
            }
            state.by_vm.insert(
                vm_id,
                JailLease {
                    root,
                    uid: marker.uid,
                    gid: marker.gid,
                },
            );
        }
        Ok(Self {
            config,
            state: Mutex::new(state),
        })
    }

    fn root_for(&self, id: Uuid) -> PathBuf {
        self.config.base_dir.join(format!("tarit-{id}"))
    }

    fn lease(&self, id: Uuid) -> Result<JailLease, OrchError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| OrchError::Internal("VM jail lease lock poisoned".into()))?;
        if let Some(lease) = state.by_vm.get(&id) {
            return Ok(lease.clone());
        }
        let slot = (0..self.config.id_count)
            .find(|slot| !state.by_slot.contains_key(slot))
            .ok_or_else(|| OrchError::Overloaded {
                message: "VM jail UID/GID lease range exhausted".into(),
                retry_after_secs: 1,
            })?;
        let uid = self
            .config
            .uid_base
            .checked_add(slot)
            .ok_or_else(|| OrchError::Internal("VM jail UID lease overflow".into()))?;
        let gid = self
            .config
            .gid_base
            .checked_add(slot)
            .ok_or_else(|| OrchError::Internal("VM jail GID lease overflow".into()))?;
        let root = self.root_for(id);
        if root.exists() {
            return Err(OrchError::Internal(format!(
                "VM jail {} already exists without an active lease",
                root.display()
            )));
        }
        let staging = self
            .config
            .base_dir
            .join(format!(".tarit-stage-{}", Uuid::new_v4()));
        std::fs::create_dir(&staging).map_err(|error| {
            OrchError::Internal(format!(
                "create VM jail staging directory {}: {error}",
                staging.display()
            ))
        })?;
        if let Err(error) =
            std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
        {
            let _ = std::fs::remove_dir(&staging);
            return Err(OrchError::Internal(format!(
                "protect VM jail staging directory {}: {error}",
                staging.display()
            )));
        }
        let marker = JailMarker {
            version: JAIL_MARKER_VERSION,
            vm_id: id,
            uid,
            gid,
        };
        if let Err(error) = write_jail_marker(&staging, &marker) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&staging, &root) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(OrchError::Internal(format!(
                "publish VM jail {}: {error}",
                root.display()
            )));
        }
        if let Err(error) =
            File::open(&self.config.base_dir).and_then(|directory| directory.sync_all())
        {
            let _ = std::fs::remove_dir_all(&root);
            return Err(OrchError::Internal(format!(
                "sync VM jail base {} after publishing {}: {error}",
                self.config.base_dir.display(),
                root.display()
            )));
        }
        let lease = JailLease { root, uid, gid };
        state.by_slot.insert(slot, id);
        state.by_vm.insert(id, lease.clone());
        Ok(lease)
    }

    fn identity(&self, id: Uuid) -> Result<Option<(u32, u32)>, OrchError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| OrchError::Internal("VM jail lease lock poisoned".into()))?
            .by_vm
            .get(&id)
            .map(|lease| (lease.uid, lease.gid)))
    }

    fn ids(&self) -> Result<Vec<Uuid>, OrchError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| OrchError::Internal("VM jail lease lock poisoned".into()))?
            .by_vm
            .keys()
            .copied()
            .collect())
    }

    #[cfg(target_os = "linux")]
    fn uids(&self) -> Result<HashSet<u32>, OrchError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| OrchError::Internal("VM jail lease lock poisoned".into()))?
            .by_vm
            .values()
            .map(|lease| lease.uid)
            .collect())
    }

    fn release(&self, id: Uuid) -> Result<(), OrchError> {
        let lease = self
            .state
            .lock()
            .map_err(|_| OrchError::Internal("VM jail lease lock poisoned".into()))?
            .by_vm
            .get(&id)
            .cloned();
        let Some(lease) = lease else {
            return Ok(());
        };
        let marker = read_jail_marker(&lease.root)?;
        if marker.vm_id != id || marker.uid != lease.uid || marker.gid != lease.gid {
            return Err(OrchError::Internal(format!(
                "refuse to remove VM jail {} after marker identity changed",
                lease.root.display()
            )));
        }
        match std::fs::remove_dir_all(&lease.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(OrchError::Internal(format!(
                    "remove VM jail {}: {error}",
                    lease.root.display()
                )))
            }
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| OrchError::Internal("VM jail lease lock poisoned".into()))?;
        state.by_vm.remove(&id);
        state.by_slot.retain(|_, owner| *owner != id);
        Ok(())
    }

    fn reconcile_present(&self) -> Result<(), OrchError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| OrchError::Internal("VM jail lease lock poisoned".into()))?;
        let removed = state
            .by_vm
            .iter()
            .filter_map(|(id, lease)| (!lease.root.exists()).then_some(*id))
            .collect::<Vec<_>>();
        for id in removed {
            state.by_vm.remove(&id);
            state.by_slot.retain(|_, owner| *owner != id);
        }
        Ok(())
    }
}

fn ensure_private_jail_base(path: &Path) -> Result<(), OrchError> {
    validate_jail_base_path(path)?;
    let mut directory = File::open("/").map_err(|error| {
        OrchError::Internal(format!("open filesystem root for jail base: {error}"))
    })?;
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::RootDir => None,
            Component::Normal(name) => Some(name.to_owned()),
            _ => unreachable!("validated jail path contains only root and normal components"),
        })
        .collect::<Vec<_>>();
    for component in &components {
        let name = std::ffi::CString::new(component.as_bytes()).map_err(|_| {
            OrchError::Internal(format!("VM jail base contains NUL: {}", path.display()))
        })?;
        let mut fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(OrchError::Internal(format!(
                    "open VM jail base component {} without following symlinks: {error}",
                    component.to_string_lossy()
                )));
            }
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                return Err(OrchError::Internal(format!(
                    "create private VM jail base component {}: {}",
                    component.to_string_lossy(),
                    std::io::Error::last_os_error()
                )));
            }
            fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(OrchError::Internal(format!(
                    "open newly created VM jail base component {}: {}",
                    component.to_string_lossy(),
                    std::io::Error::last_os_error()
                )));
            }
        }
        // SAFETY: fd is a unique successful openat result and ownership moves
        // into File immediately.
        directory = unsafe { File::from_raw_fd(fd) };
    }

    let metadata = directory.metadata().map_err(|error| {
        OrchError::Internal(format!("inspect VM jail base {}: {error}", path.display()))
    })?;
    let expected_uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != expected_uid || metadata.mode() & 0o077 != 0 {
        return Err(OrchError::Internal(format!(
            "VM jail base {} must be a private directory owned by uid {expected_uid}; existing host paths are never chmodded or claimed",
            path.display()
        )));
    }
    if is_mount_root(path, &metadata)? {
        return Err(OrchError::Internal(format!(
            "VM jail base {} must not be a filesystem mount root",
            path.display()
        )));
    }
    claim_or_validate_jail_base(path, expected_uid)
}

fn validate_jail_base_path(path: &Path) -> Result<(), OrchError> {
    if !path.is_absolute() {
        return Err(OrchError::Internal(
            "VM jail base must be an absolute dedicated directory".into(),
        ));
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(OrchError::Internal(format!(
            "VM jail base {} contains a non-normal path component",
            path.display()
        )));
    }
    const PROTECTED_ROOTS: &[&str] = &[
        "/",
        "/Applications",
        "/boot",
        "/dev",
        "/etc",
        "/home",
        "/Library",
        "/media",
        "/mnt",
        "/opt",
        "/private",
        "/proc",
        "/root",
        "/run",
        "/srv",
        "/sys",
        "/System",
        "/tmp",
        "/Users",
        "/usr",
        "/var",
        "/Volumes",
    ];
    if PROTECTED_ROOTS
        .iter()
        .any(|protected| path == Path::new(protected))
    {
        return Err(OrchError::Internal(format!(
            "VM jail base {} is a protected broad host path; configure a dedicated child directory",
            path.display()
        )));
    }
    Ok(())
}

fn claim_or_validate_jail_base(path: &Path, expected_uid: u32) -> Result<(), OrchError> {
    use std::io::{Read as _, Write as _};

    let marker_path = path.join(".tarit-jail-base.json");
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    match options.open(&marker_path) {
        Ok(mut marker_file) => {
            let metadata = marker_file.metadata().map_err(|error| {
                OrchError::Internal(format!(
                    "inspect VM jail base marker {}: {error}",
                    marker_path.display()
                ))
            })?;
            if !metadata.is_file()
                || metadata.uid() != expected_uid
                || metadata.nlink() != 1
                || metadata.mode() & 0o077 != 0
            {
                return Err(OrchError::Internal(format!(
                    "unsafe VM jail base marker {}",
                    marker_path.display()
                )));
            }
            let mut body = Vec::new();
            marker_file.read_to_end(&mut body).map_err(|error| {
                OrchError::Internal(format!(
                    "read VM jail base marker {}: {error}",
                    marker_path.display()
                ))
            })?;
            let marker: JailBaseMarker = serde_json::from_slice(&body).map_err(|error| {
                OrchError::Internal(format!(
                    "parse VM jail base marker {}: {error}",
                    marker_path.display()
                ))
            })?;
            if marker.version != JAIL_BASE_MARKER_VERSION || marker.uid != expected_uid {
                return Err(OrchError::Internal(format!(
                    "VM jail base marker {} does not match this Tarit identity",
                    marker_path.display()
                )));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut entries = std::fs::read_dir(path).map_err(|error| {
                OrchError::Internal(format!(
                    "scan unclaimed VM jail base {}: {error}",
                    path.display()
                ))
            })?;
            if entries
                .next()
                .transpose()
                .map_err(|error| {
                    OrchError::Internal(format!(
                        "scan unclaimed VM jail base {}: {error}",
                        path.display()
                    ))
                })?
                .is_some()
            {
                return Err(OrchError::Internal(format!(
                    "refuse to claim non-empty existing VM jail base {} without a Tarit ownership marker",
                    path.display()
                )));
            }
            let marker = JailBaseMarker {
                version: JAIL_BASE_MARKER_VERSION,
                uid: expected_uid,
            };
            let mut marker_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&marker_path)
                .map_err(|error| {
                    OrchError::Internal(format!("claim VM jail base {}: {error}", path.display()))
                })?;
            marker_file
                .write_all(&serde_json::to_vec(&marker).map_err(|error| {
                    OrchError::Internal(format!("encode VM jail base marker: {error}"))
                })?)
                .and_then(|_| marker_file.sync_all())
                .map_err(|error| {
                    OrchError::Internal(format!(
                        "persist VM jail base marker {}: {error}",
                        marker_path.display()
                    ))
                })?;
            File::open(path)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| {
                    OrchError::Internal(format!("sync VM jail base {}: {error}", path.display()))
                })
        }
        Err(error) => Err(OrchError::Internal(format!(
            "open VM jail base marker {}: {error}",
            marker_path.display()
        ))),
    }
}

fn is_mount_root(path: &Path, metadata: &std::fs::Metadata) -> Result<bool, OrchError> {
    let parent = path.parent().ok_or_else(|| {
        OrchError::Internal(format!("VM jail base {} has no parent", path.display()))
    })?;
    let parent_metadata = std::fs::metadata(parent).map_err(|error| {
        OrchError::Internal(format!(
            "inspect VM jail base parent {}: {error}",
            parent.display()
        ))
    })?;
    if metadata.dev() != parent_metadata.dev() {
        return Ok(true);
    }
    #[cfg(target_os = "linux")]
    {
        let target = path.as_os_str().as_bytes();
        let mountinfo = std::fs::read("/proc/self/mountinfo").map_err(|error| {
            OrchError::Internal(format!(
                "read mount table for VM jail base validation: {error}"
            ))
        })?;
        for line in mountinfo.split(|byte| *byte == b'\n') {
            let mut fields = line.split(|byte| *byte == b' ');
            let mount_point = fields.nth(4).unwrap_or_default();
            if decode_mountinfo_field(mount_point) == target {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(any(target_os = "linux", test))]
fn decode_mountinfo_field(field: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        if field[index] == b'\\' && index + 3 < field.len() {
            let digits = &field[index + 1..index + 4];
            if digits.iter().all(|digit| matches!(digit, b'0'..=b'7')) {
                decoded.push((digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + (digits[2] - b'0'));
                index += 4;
                continue;
            }
        }
        decoded.push(field[index]);
        index += 1;
    }
    decoded
}

fn write_jail_marker(root: &Path, marker: &JailMarker) -> Result<(), OrchError> {
    use std::io::Write as _;

    let path = root.join(".tarit-jail.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| {
            OrchError::Internal(format!("create VM jail marker {}: {error}", path.display()))
        })?;
    let body = serde_json::to_vec(marker)
        .map_err(|error| OrchError::Internal(format!("encode VM jail marker: {error}")))?;
    file.write_all(&body)
        .and_then(|_| file.sync_all())
        .map_err(|error| {
            OrchError::Internal(format!(
                "persist VM jail marker {}: {error}",
                path.display()
            ))
        })?;
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            OrchError::Internal(format!(
                "sync VM jail directory {}: {error}",
                root.display()
            ))
        })
}

fn read_jail_marker(root: &Path) -> Result<JailMarker, OrchError> {
    use std::io::Read as _;

    let path = root.join(".tarit-jail.json");
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| {
            OrchError::Internal(format!("read VM jail marker {}: {error}", path.display()))
        })?;
    let metadata = file.metadata().map_err(|error| {
        OrchError::Internal(format!(
            "inspect VM jail marker {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
    {
        return Err(OrchError::Internal(format!(
            "unsafe VM jail marker {}",
            path.display()
        )));
    }
    let mut body = Vec::new();
    file.read_to_end(&mut body).map_err(|error| {
        OrchError::Internal(format!("read VM jail marker {}: {error}", path.display()))
    })?;
    serde_json::from_slice(&body).map_err(|error| {
        OrchError::Internal(format!("parse VM jail marker {}: {error}", path.display()))
    })
}

#[derive(Debug)]
struct PreparedRuntime {
    host_socket: PathBuf,
    socket_argument: PathBuf,
    vm_config: VmSpawnConfig,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    host_rootfs: Option<PathBuf>,
    host_overlay: Option<PathBuf>,
    guest_overlay: Option<String>,
    guest_snapshot: Option<String>,
    data_volumes: Vec<PreparedBlockAttachment>,
    jail: Option<JailLease>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CgroupLimitPlan {
    cpu_max: Option<String>,
    cpu_weight: Option<u64>,
    memory_max: Option<u64>,
    pids_max: Option<u64>,
    io_weight: Option<u64>,
    io_max: Option<String>,
    cpuset_cpus: Option<String>,
    cpuset_mems: Option<String>,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl CgroupLimitPlan {
    fn entries(&self) -> Vec<(&'static str, String)> {
        [
            self.cpuset_mems
                .as_ref()
                .map(|value| ("cpuset.mems", value.clone())),
            self.cpuset_cpus
                .as_ref()
                .map(|value| ("cpuset.cpus", value.clone())),
            self.cpu_max
                .as_ref()
                .map(|value| ("cpu.max", value.clone())),
            self.cpu_weight
                .map(|value| ("cpu.weight", value.to_string())),
            self.memory_max
                .map(|value| ("memory.max", value.to_string())),
            self.pids_max.map(|value| ("pids.max", value.to_string())),
            self.io_weight.map(|value| ("io.weight", value.to_string())),
            self.io_max.as_ref().map(|value| ("io.max", value.clone())),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
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

    fn publish(&mut self, destination: &Path) -> std::io::Result<()> {
        std::fs::rename(&self.path, destination)?;
        self.path = destination.to_path_buf();
        let parent = destination.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} has no parent directory", destination.display()),
            )
        })?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        options.open(parent)?.sync_all()
    }
}

/// A RAM snapshot and its snapshot-owned disk upper. Open descriptors pin the
/// exact files until the ownership row is durable; dropping an uncommitted
/// bundle removes only those exact inodes.
pub(crate) struct SnapshotBundle {
    snapshot_path: String,
    overlay_path: Option<String>,
    live_stats: Option<tarit_proto::LiveSnapshotStats>,
    artifacts: Vec<OwnedArtifact>,
    /// VMM-generated hashes for the exact live RAM artifact. This temporary
    /// sidecar is consumed when the durable manifest is created and never
    /// becomes a public artifact itself.
    precomputed_integrity: Option<OwnedArtifact>,
    in_progress_artifacts: Arc<Mutex<HashSet<PathBuf>>>,
    registered_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotIntegrity {
    pub(crate) content_digest: String,
    pub(crate) size_bytes: u64,
    pub(crate) chunk_size_bytes: u64,
    pub(crate) chunk_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedSnapshotIntegrity {
    pub(crate) manifest_path: String,
    pub(crate) manifest_sha256: String,
    overlay: Option<tarit_proto::ArtifactIntegrity>,
    chunk_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoldenBundle {
    snapshot_path: String,
    overlay_path: Option<String>,
}

impl GoldenBundle {
    pub(crate) fn snapshot_path(&self) -> &str {
        &self.snapshot_path
    }

    pub(crate) fn overlay_path(&self) -> Option<&str> {
        self.overlay_path.as_deref()
    }

    fn into_restore_parts(self) -> (String, RestoreOverlay) {
        let overlay = self
            .overlay_path
            .map(PathBuf::from)
            .map(RestoreOverlay::Seeded)
            .unwrap_or(RestoreOverlay::None);
        (self.snapshot_path, overlay)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RestoreOverlay {
    None,
    Seeded(PathBuf),
}

impl SnapshotBundle {
    pub(crate) fn snapshot_path(&self) -> &str {
        &self.snapshot_path
    }

    pub(crate) fn overlay_path(&self) -> Option<&str> {
        self.overlay_path.as_deref()
    }

    pub(crate) fn live_stats(&self) -> Option<&tarit_proto::LiveSnapshotStats> {
        self.live_stats.as_ref()
    }

    /// Hash the exact open inodes owned by this bundle. Path replacement cannot
    /// substitute bytes between capture and manifest creation.
    pub(crate) fn integrity(&mut self) -> Result<SnapshotIntegrity, OrchError> {
        let manifest =
            snapshot_integrity_manifest(&self.artifacts, self.precomputed_integrity.as_ref())?;
        let chunk_count = manifest
            .artifacts
            .iter()
            .try_fold(0u64, |total, artifact| {
                total
                    .checked_add(artifact.chunk_hashes.len() as u64)
                    .ok_or_else(|| OrchError::Internal("snapshot chunk count overflow".into()))
            })?;
        let chunk_size_bytes = u64::from(manifest.chunk_size);
        let encoded = manifest.encode().map_err(|error| {
            OrchError::Internal(format!("encode snapshot integrity manifest: {error}"))
        })?;
        let manifest_path = snapshot_manifest_path(&self.snapshot_path);
        {
            let mut registered = self
                .in_progress_artifacts
                .lock()
                .map_err(|_| OrchError::Internal("artifact ownership registry poisoned".into()))?;
            if !registered.insert(manifest_path.clone()) {
                return Err(OrchError::Internal(format!(
                    "snapshot manifest path is already registered: {}",
                    manifest_path.display()
                )));
            }
        }
        self.registered_paths.push(manifest_path.clone());
        let artifact = OwnedArtifact::create_private(&manifest_path).map_err(|error| {
            OrchError::Internal(format!("create snapshot integrity manifest: {error}"))
        })?;
        self.artifacts.push(artifact);
        use std::io::Write as _;
        let manifest_artifact = self
            .artifacts
            .last_mut()
            .expect("manifest artifact was just appended");
        manifest_artifact
            ._file
            .write_all(&encoded)
            .and_then(|_| manifest_artifact._file.sync_all())
            .map_err(|error| {
                OrchError::Internal(format!("persist snapshot integrity manifest: {error}"))
            })?;
        let size_bytes = self.artifacts[..self.artifacts.len() - 1].iter().try_fold(
            0u64,
            |total, artifact| {
                artifact
                    ._file
                    .metadata()
                    .map_err(|error| {
                        OrchError::Internal(format!("stat snapshot artifact: {error}"))
                    })?
                    .len()
                    .checked_add(total)
                    .ok_or_else(|| OrchError::Internal("snapshot bundle size overflow".into()))
            },
        )?;
        if let Some(precomputed) = self.precomputed_integrity.take() {
            let removed = precomputed.remove().map_err(|error| {
                OrchError::Internal(format!(
                    "remove consumed live integrity sidecar {}: {error}",
                    precomputed.path().display()
                ))
            })?;
            if !removed {
                return Err(OrchError::BadRequest(
                    "live integrity sidecar path changed before cleanup".into(),
                ));
            }
        }
        Ok(SnapshotIntegrity {
            content_digest: format!("sha256:{:x}", Sha256::digest(&encoded)),
            size_bytes,
            chunk_size_bytes,
            chunk_count,
        })
    }

    pub(crate) fn persist(mut self) {
        // Keep successfully published paths in the process registry. A GC pass
        // may have loaded durable snapshot rows just before this transaction
        // committed; retaining the publication fence prevents that stale pass
        // from deleting files now referenced by durable metadata. Restart
        // rebuilds the fence from the snapshot table.
        self.registered_paths.clear();
        self.artifacts.clear();
        if let Some(precomputed) = self.precomputed_integrity.take() {
            if let Err(error) = precomputed.remove() {
                tracing::warn!(
                    path = %precomputed.path().display(),
                    "remove unused live integrity sidecar failed: {error}"
                );
            }
        }
    }

    fn cleanup(&mut self) {
        if let Some(precomputed) = self.precomputed_integrity.take() {
            if let Err(error) = precomputed.remove() {
                tracing::warn!(
                    path = %precomputed.path().display(),
                    "remove uncommitted live integrity sidecar failed: {error}"
                );
            }
        }
        for artifact in self.artifacts.drain(..) {
            if let Err(error) = artifact.remove() {
                tracing::warn!(
                    path = %artifact.path().display(),
                    "remove uncommitted snapshot artifact failed: {error}"
                );
            }
        }
        self.unregister();
    }

    fn unregister(&mut self) {
        if let Ok(mut registered) = self.in_progress_artifacts.lock() {
            for path in self.registered_paths.drain(..) {
                registered.remove(&path);
            }
        }
    }
}

fn snapshot_manifest_path(snapshot_path: &str) -> PathBuf {
    PathBuf::from(format!("{snapshot_path}.integrity"))
}

fn snapshot_integrity_manifest(
    artifacts: &[OwnedArtifact],
    precomputed: Option<&OwnedArtifact>,
) -> Result<tarit_proto::IntegrityManifest, OrchError> {
    if artifacts.is_empty() || artifacts.len() > 2 {
        return Err(OrchError::Internal(
            "invalid snapshot artifact bundle".into(),
        ));
    }
    let ram_len = artifacts[0]
        ._file
        .metadata()
        .map_err(|error| OrchError::Internal(format!("stat RAM snapshot: {error}")))?
        .len();
    let mut header = [0u8; 32];
    read_exact_at(&artifacts[0]._file, &mut header, 0, "snapshot header")?;
    if &header[..4] != b"VMSN" {
        return Err(OrchError::BadRequest(
            "authenticated lazy restore requires a full VMSN snapshot".into(),
        ));
    }
    let flags = u16::from_le_bytes(header[6..8].try_into().expect("VMSN flags"));
    if flags != 0 {
        return Err(OrchError::BadRequest(
            "authenticated lazy restore does not support diff snapshots".into(),
        ));
    }
    let state_len = u64::from_le_bytes(header[8..16].try_into().expect("VMSN state length"));
    let memory_len = u64::from_le_bytes(header[20..28].try_into().expect("VMSN memory length"));
    let memory_offset = 32u64
        .checked_add(state_len)
        .ok_or_else(|| OrchError::Internal("snapshot memory offset overflow".into()))?;
    if memory_offset.checked_add(memory_len) != Some(ram_len) {
        return Err(OrchError::BadRequest(
            "snapshot layout does not match its file length".into(),
        ));
    }
    let chunk_size = u64::from(tarit_proto::INTEGRITY_CHUNK_SIZE);
    let mut manifest_artifacts = if let Some(precomputed) = precomputed {
        let metadata_chunks = memory_offset.div_ceil(chunk_size);
        let memory_chunks = memory_len.div_ceil(chunk_size);
        let expected_len = 12u64
            .checked_add(2 * 24)
            .and_then(|base| {
                metadata_chunks
                    .checked_add(memory_chunks)
                    .and_then(|chunks| chunks.checked_mul(32))
                    .and_then(|hashes| base.checked_add(hashes))
            })
            .ok_or_else(|| OrchError::Internal("live integrity size overflow".into()))?;
        let actual_len = precomputed
            ._file
            .metadata()
            .map_err(|error| OrchError::Internal(format!("stat live integrity sidecar: {error}")))?
            .len();
        if actual_len != expected_len {
            return Err(OrchError::BadRequest(format!(
                "live integrity sidecar length mismatch: got {actual_len}, expected {expected_len}"
            )));
        }
        let mut encoded = vec![
            0u8;
            usize::try_from(actual_len).map_err(|_| {
                OrchError::Internal("live integrity sidecar is too large".into())
            })?
        ];
        read_exact_at(
            &precomputed._file,
            &mut encoded,
            0,
            "live integrity sidecar",
        )?;
        let manifest = tarit_proto::IntegrityManifest::decode(&encoded).map_err(|error| {
            OrchError::BadRequest(format!("invalid live integrity sidecar: {error}"))
        })?;
        if manifest.chunk_size != tarit_proto::INTEGRITY_CHUNK_SIZE
            || manifest.artifacts.len() != 2
            || manifest
                .artifact(tarit_proto::ArtifactKind::Overlay)
                .is_some()
        {
            return Err(OrchError::BadRequest(
                "live integrity sidecar has an invalid artifact set".into(),
            ));
        }
        let metadata = manifest
            .artifact(tarit_proto::ArtifactKind::SnapshotMetadata)
            .ok_or_else(|| OrchError::BadRequest("live integrity metadata is missing".into()))?;
        let memory = manifest
            .artifact(tarit_proto::ArtifactKind::Ram)
            .ok_or_else(|| OrchError::BadRequest("live integrity RAM is missing".into()))?;
        if metadata.len != memory_offset || memory.len != memory_len {
            return Err(OrchError::BadRequest(
                "live integrity sidecar does not match the snapshot layout".into(),
            ));
        }
        let actual_metadata = hash_artifact_range(
            &artifacts[0]._file,
            tarit_proto::ArtifactKind::SnapshotMetadata,
            0,
            memory_offset,
            chunk_size,
        )?;
        if actual_metadata.chunk_hashes != metadata.chunk_hashes {
            return Err(OrchError::BadRequest(
                "live integrity sidecar does not match snapshot metadata".into(),
            ));
        }
        tracing::info!(
            ram_bytes = memory_len,
            ram_chunks = memory.chunk_hashes.len(),
            "adopted VMM live integrity sidecar without rereading RAM"
        );
        manifest.artifacts
    } else {
        vec![
            hash_artifact_range(
                &artifacts[0]._file,
                tarit_proto::ArtifactKind::SnapshotMetadata,
                0,
                memory_offset,
                chunk_size,
            )?,
            hash_artifact_range(
                &artifacts[0]._file,
                tarit_proto::ArtifactKind::Ram,
                memory_offset,
                memory_len,
                chunk_size,
            )?,
        ]
    };
    if let Some(overlay) = artifacts.get(1) {
        let len = overlay
            ._file
            .metadata()
            .map_err(|error| OrchError::Internal(format!("stat snapshot overlay: {error}")))?
            .len();
        manifest_artifacts.push(hash_artifact_range(
            &overlay._file,
            tarit_proto::ArtifactKind::Overlay,
            0,
            len,
            chunk_size,
        )?);
    }
    Ok(tarit_proto::IntegrityManifest {
        chunk_size: tarit_proto::INTEGRITY_CHUNK_SIZE,
        artifacts: manifest_artifacts,
    })
}

fn read_exact_at(
    file: &File,
    mut output: &mut [u8],
    mut offset: u64,
    what: &str,
) -> Result<(), OrchError> {
    while !output.is_empty() {
        let read = file
            .read_at(output, offset)
            .map_err(|error| OrchError::Internal(format!("read {what} for integrity: {error}")))?;
        if read == 0 {
            return Err(OrchError::BadRequest(format!("truncated {what}")));
        }
        offset = offset
            .checked_add(read as u64)
            .ok_or_else(|| OrchError::Internal("integrity read offset overflow".into()))?;
        output = &mut output[read..];
    }
    Ok(())
}

fn hash_artifact_range(
    file: &File,
    kind: tarit_proto::ArtifactKind,
    start: u64,
    len: u64,
    chunk_size: u64,
) -> Result<tarit_proto::ArtifactIntegrity, OrchError> {
    #[cfg(test)]
    if kind == tarit_proto::ArtifactKind::Ram {
        TEST_RAM_INTEGRITY_HASH_PASSES.with(|passes| passes.set(passes.get() + 1));
    }
    let mut chunk_hashes = Vec::new();
    let mut offset = 0u64;
    // Read in large sequential windows while retaining small verification
    // chunks. This keeps snapshot publication throughput close to the old
    // whole-file hash (one pread per MiB, not one syscall per 64 KiB chunk).
    let read_window = chunk_size.max(1024 * 1024);
    let mut buffer = vec![0u8; read_window as usize];
    while offset < len {
        let wanted = usize::try_from((len - offset).min(read_window))
            .map_err(|_| OrchError::Internal("integrity read length overflow".into()))?;
        read_exact_at(
            file,
            &mut buffer[..wanted],
            start
                .checked_add(offset)
                .ok_or_else(|| OrchError::Internal("integrity range overflow".into()))?,
            "snapshot artifact",
        )?;
        chunk_hashes.extend(
            buffer[..wanted]
                .chunks(chunk_size as usize)
                .map(|chunk| -> [u8; 32] { Sha256::digest(chunk).into() }),
        );
        offset = offset
            .checked_add(wanted as u64)
            .ok_or_else(|| OrchError::Internal("integrity offset overflow".into()))?;
    }
    Ok(tarit_proto::ArtifactIntegrity {
        kind,
        len,
        chunk_hashes,
    })
}

#[cfg(test)]
thread_local! {
    static TEST_RAM_INTEGRITY_HASH_PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Authenticate the durable chunk manifest, validate artifact shapes, and
/// eagerly verify only the disk upper. RAM remains lazy: the VMM verifies each
/// authenticated chunk from a stable private buffer before UFFDIO_COPY.
pub(crate) fn verify_snapshot_integrity(
    record: &tarit_store::SnapshotRecord,
) -> Result<VerifiedSnapshotIntegrity, OrchError> {
    let expected_digest = record.content_digest.as_deref().ok_or_else(|| {
        OrchError::BadRequest(
            "snapshot predates authenticated artifact metadata; create a new snapshot".into(),
        )
    })?;
    let expected_size = record.size_bytes.ok_or_else(|| {
        OrchError::BadRequest(
            "snapshot predates authenticated artifact sizing; create a new snapshot".into(),
        )
    })?;
    let artifacts = [
        OwnedArtifact::capture(Path::new(&record.path)).map_err(|error| {
            OrchError::BadRequest(format!(
                "snapshot RAM artifact is unsafe or unavailable: {error}"
            ))
        })?,
    ];
    let ram_size = artifacts[0]
        ._file
        .metadata()
        .map_err(|error| OrchError::BadRequest(format!("stat snapshot RAM: {error}")))?
        .len();
    let overlay = record
        .overlay_path
        .as_deref()
        .map(|path| {
            OwnedArtifact::capture(Path::new(path)).map_err(|error| {
                OrchError::BadRequest(format!(
                    "snapshot disk artifact is unsafe or unavailable: {error}"
                ))
            })
        })
        .transpose()?;
    let overlay_size = overlay
        .as_ref()
        .map(|artifact| artifact._file.metadata().map(|metadata| metadata.len()))
        .transpose()
        .map_err(|error| OrchError::BadRequest(format!("stat snapshot disk: {error}")))?
        .unwrap_or(0);
    if ram_size.checked_add(overlay_size) != Some(expected_size) {
        return Err(OrchError::BadRequest(
            "snapshot artifact integrity verification failed".into(),
        ));
    }
    let manifest_path = snapshot_manifest_path(&record.path);
    let manifest_artifact = OwnedArtifact::capture(&manifest_path).map_err(|error| {
        OrchError::BadRequest(format!(
            "snapshot integrity manifest is unsafe or unavailable: {error}"
        ))
    })?;
    let manifest_len = manifest_artifact
        ._file
        .metadata()
        .map_err(|error| OrchError::BadRequest(format!("stat snapshot manifest: {error}")))?
        .len();
    if manifest_len > 128 * 1024 * 1024 {
        return Err(OrchError::BadRequest(
            "snapshot integrity manifest is oversized".into(),
        ));
    }
    let mut encoded = vec![0u8; manifest_len as usize];
    read_exact_at(
        &manifest_artifact._file,
        &mut encoded,
        0,
        "integrity manifest",
    )?;
    let actual_digest = format!("sha256:{:x}", Sha256::digest(&encoded));
    if actual_digest != expected_digest {
        return Err(OrchError::BadRequest(
            "snapshot integrity manifest authentication failed".into(),
        ));
    }
    let manifest = tarit_proto::IntegrityManifest::decode(&encoded).map_err(|error| {
        OrchError::BadRequest(format!("invalid snapshot integrity manifest: {error}"))
    })?;
    let memory = manifest
        .artifact(tarit_proto::ArtifactKind::Ram)
        .ok_or_else(|| OrchError::BadRequest("snapshot manifest has no RAM entry".into()))?;
    let metadata = manifest
        .artifact(tarit_proto::ArtifactKind::SnapshotMetadata)
        .ok_or_else(|| OrchError::BadRequest("snapshot manifest has no metadata entry".into()))?;
    if metadata.len.checked_add(memory.len) != Some(ram_size) {
        return Err(OrchError::BadRequest(
            "snapshot manifest RAM size mismatch".into(),
        ));
    }
    match (
        overlay.as_ref(),
        manifest.artifact(tarit_proto::ArtifactKind::Overlay),
    ) {
        (None, None) => {}
        (Some(overlay), Some(integrity)) if integrity.len == overlay_size => {
            verify_file_artifact_integrity(
                &overlay._file,
                integrity,
                manifest.chunk_size as u64,
                "snapshot disk",
            )?;
        }
        _ => {
            return Err(OrchError::BadRequest(
                "snapshot manifest disk shape mismatch".into(),
            ))
        }
    }
    Ok(VerifiedSnapshotIntegrity {
        manifest_path: manifest_path.display().to_string(),
        manifest_sha256: actual_digest,
        overlay: manifest
            .artifact(tarit_proto::ArtifactKind::Overlay)
            .cloned(),
        chunk_size: manifest.chunk_size as u64,
    })
}

fn verify_file_artifact_integrity(
    file: &File,
    integrity: &tarit_proto::ArtifactIntegrity,
    chunk_size: u64,
    what: &str,
) -> Result<(), OrchError> {
    let actual = hash_artifact_range(file, integrity.kind, 0, integrity.len, chunk_size)?;
    if actual.chunk_hashes != integrity.chunk_hashes {
        return Err(OrchError::BadRequest(format!(
            "{what} integrity verification failed"
        )));
    }
    Ok(())
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
        let pid = child.id();
        let exited = poll_process_exit(TEARDOWN_KILL_TIMEOUT, || {
            child.try_wait().map(|status| status.is_some())
        })
        .map_err(|error| OrchError::Internal(format!("check VMM {pid} exit: {error}")))?;
        if exited {
            Ok(())
        } else {
            Err(OrchError::Internal(format!(
                "owned VMM {pid} did not exit after SIGKILL within {:?}",
                TEARDOWN_KILL_TIMEOUT
            )))
        }
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
        let deadline = Instant::now() + TEARDOWN_KILL_TIMEOUT;
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
        let deadline = Instant::now() + TEARDOWN_KILL_TIMEOUT;
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

fn poll_process_exit<F>(timeout: Duration, mut exited: F) -> std::io::Result<bool>
where
    F: FnMut() -> std::io::Result<bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if exited()? {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(20)));
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
        let completed = completed.as_ref().ok_or_else(|| {
            OrchError::Internal("owned task completion disappeared after wait".into())
        })?;
        match completed {
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
        let completed = completed
            .as_ref()
            .ok_or_else(|| OrchError::Internal("boot completion disappeared after wait".into()))?;
        match completed {
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
    /// Authoritative ownership for UUID-scoped runtime artifacts. An id enters
    /// before any jail/overlay can be created and leaves only after teardown
    /// succeeds, so registry-to-registry lifecycle handoffs cannot expose a GC
    /// deletion gap.
    artifact_owners: Mutex<HashSet<Uuid>>,
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
    #[cfg(test)]
    warm_handoff_pause: Mutex<Option<SpawnAttachmentPause>>,
    scheduler: Arc<Scheduler>,
    golden_artifacts: Mutex<Vec<OwnedArtifact>>,
    in_progress_artifacts: Arc<Mutex<HashSet<PathBuf>>>,
    unexpected_exits: Mutex<VecDeque<UnexpectedVmmExit>>,
    net: Option<NetProvisioner>,
    jails: Option<JailManager>,
    disk_pressure: Arc<DiskPressure>,
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
        let artifact_owners = live_vm_ids.iter().copied().collect();
        let jails = config.vm_jail.clone().map(JailManager::new).transpose()?;
        let mut pressure_roots = vec![config.socket_dir.clone()];
        if let Some(jail) = &config.vm_jail {
            pressure_roots.push(jail.base_dir.clone());
        } else {
            pressure_roots.push(std::env::temp_dir());
        }
        let disk_pressure = Arc::new(DiskPressure::new(
            config.disk_pressure.clone(),
            pressure_roots,
        )?);
        validate_network_startup_mode(config.enable_net, preflight_taps)?;
        let net = if config.enable_net {
            let mut tap_owners = HashMap::new();
            if let Some(jails) = &jails {
                for id in &live_vm_ids {
                    if let Some(identity) = jails.identity(*id)? {
                        tap_owners.insert(*id, identity);
                    }
                }
            }
            let provisioner = NetProvisioner::new_with_tap_owners(
                config.net_state_path.clone(),
                live_vm_ids.iter().copied(),
                config.vm_net_quota.clone(),
                tap_owners,
            )?;
            tracing::info!(uplink = provisioner.uplink(), "per-VM networking enabled");
            Some(provisioner)
        } else {
            None
        };
        Ok(Self {
            config,
            running: Mutex::new(HashMap::new()),
            artifact_owners: Mutex::new(artifact_owners),
            network_leases: Mutex::new(HashMap::new()),
            booting: Mutex::new(HashMap::new()),
            boot_gate: AsyncMutex::new(()),
            warm: Mutex::new(VecDeque::new()),
            owned_tasks: Mutex::new(HashMap::new()),
            #[cfg(test)]
            spawn_attachment_pause: Mutex::new(None),
            #[cfg(test)]
            warm_handoff_pause: Mutex::new(None),
            scheduler,
            golden_artifacts: Mutex::new(Vec::new()),
            in_progress_artifacts: Arc::new(Mutex::new(HashSet::new())),
            unexpected_exits: Mutex::new(VecDeque::new()),
            net,
            jails,
            disk_pressure,
            shutting_down: AtomicBool::new(false),
            admission: Arc::new(VmAdmissionGate::default()),
        })
    }

    fn socket_path_for(&self, id: Uuid) -> PathBuf {
        match &self.jails {
            Some(jails) => jails.root_for(id).join("run/vmm.sock"),
            None => self.config.socket_dir.join(format!("{id}.sock")),
        }
    }

    fn overlay_path_for(&self, id: Uuid) -> String {
        match &self.jails {
            Some(jails) => jails.root_for(id).join("assets/rootfs.cow"),
            None => self
                .config
                .socket_dir
                .join("overlays")
                .join(format!("{id}.cow")),
        }
        .display()
        .to_string()
    }

    fn restore_overlay_path_for(&self, id: Uuid) -> String {
        match &self.jails {
            Some(jails) => jails
                .root_for(id)
                .join(jail_restore_overlay_path(id).trim_start_matches('/')),
            None => self
                .config
                .socket_dir
                .join("overlays")
                .join(format!("{id}.cow")),
        }
        .display()
        .to_string()
    }

    fn restore_overlay_path_for_snapshot(&self, id: Uuid, snapshot_path: &Path) -> String {
        let digest = Sha256::digest(snapshot_path.as_os_str().as_bytes());
        let token = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        match &self.jails {
            Some(jails) => jails
                .root_for(id)
                .join(format!("assets/restored-rootfs-{id}-{token}.cow")),
            None => self
                .config
                .socket_dir
                .join("overlays")
                .join(format!("{id}-restore-{token}.cow")),
        }
        .display()
        .to_string()
    }

    fn overlay_path_for_config(&self, id: Uuid, cfg: &VmSpawnConfig) -> Option<String> {
        cfg.rootfs_path.is_some().then(|| self.overlay_path_for(id))
    }

    pub(crate) fn runtime_layout_for_config(
        &self,
        id: Uuid,
        cfg: &VmSpawnConfig,
    ) -> VmRuntimeLayout {
        self.runtime_layout_with_overlay(id, cfg, self.overlay_path_for(id))
    }

    pub(crate) fn runtime_layout_for_restore_config(
        &self,
        id: Uuid,
        cfg: &VmSpawnConfig,
    ) -> VmRuntimeLayout {
        self.runtime_layout_with_overlay(id, cfg, self.restore_overlay_path_for(id))
    }

    pub(crate) fn runtime_layout_for_snapshot_restore(
        &self,
        id: Uuid,
        cfg: &VmSpawnConfig,
        snapshot_path: &Path,
    ) -> VmRuntimeLayout {
        self.runtime_layout_with_overlay(
            id,
            cfg,
            self.restore_overlay_path_for_snapshot(id, snapshot_path),
        )
    }

    fn runtime_layout_with_overlay(
        &self,
        id: Uuid,
        cfg: &VmSpawnConfig,
        overlay: String,
    ) -> VmRuntimeLayout {
        let overlay_path = cfg.rootfs_path.is_some().then_some(overlay);
        let jail_path = self
            .jails
            .as_ref()
            .map(|jails| jails.root_for(id).display().to_string());
        let mut artifact_paths = vec![self.socket_path_for(id).display().to_string()];
        if let Some(path) = &overlay_path {
            artifact_paths.push(path.clone());
        }
        if let Some(path) = &jail_path {
            artifact_paths.push(path.clone());
        }
        VmRuntimeLayout {
            overlay_path,
            jail_path,
            artifact_paths,
        }
    }

    fn expected_runtime_layout(&self, record: &VmRecord) -> VmRuntimeLayout {
        let config = VmSpawnConfig {
            memory_mib: record.memory_mib,
            vcpus: record.vcpus,
            kernel_path: PathBuf::from(&record.kernel_path),
            rootfs_path: record.rootfs_path.as_ref().map(PathBuf::from),
            cmdline: record.cmdline.clone(),
            read_only: record.rootfs_read_only,
            egress_allowlist: Vec::new(),
            egress_allow_existing: false,
            data_volumes: Vec::new(),
        };
        if record.startup_path == Some(tarit_types::VmStartupPath::SnapshotRestore) {
            let persisted_overlay = record
                .runtime_layout
                .as_ref()
                .and_then(|layout| layout.overlay_path.as_deref())
                .filter(|path| self.is_valid_restore_overlay_path(record.id, Path::new(path)));
            match persisted_overlay {
                Some(path) => {
                    self.runtime_layout_with_overlay(record.id, &config, path.to_string())
                }
                None => self.runtime_layout_for_restore_config(record.id, &config),
            }
        } else {
            self.runtime_layout_for_config(record.id, &config)
        }
    }

    fn is_valid_restore_overlay_path(&self, id: Uuid, path: &Path) -> bool {
        if path == Path::new(&self.restore_overlay_path_for(id)) {
            return true;
        }
        let (expected_parent, prefix) = match &self.jails {
            Some(jails) => (
                jails.root_for(id).join("assets"),
                format!("restored-rootfs-{id}-"),
            ),
            None => (
                self.config.socket_dir.join("overlays"),
                format!("{id}-restore-"),
            ),
        };
        if path.parent() != Some(expected_parent.as_path()) {
            return false;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        let Some(token) = name
            .strip_prefix(&prefix)
            .and_then(|name| name.strip_suffix(".cow"))
        else {
            return false;
        };
        token.len() == 16
            && token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    fn register_artifact_owner(&self, id: Uuid) -> Result<(), OrchError> {
        let mut owners = self
            .artifact_owners
            .lock()
            .map_err(|_| OrchError::Internal("artifact ownership registry poisoned".into()))?;
        if !owners.insert(id) {
            return Err(OrchError::Conflict(format!(
                "VM {id} already owns runtime artifacts"
            )));
        }
        Ok(())
    }

    fn protect_artifact_owner(&self, id: Uuid) -> Result<(), OrchError> {
        self.artifact_owners
            .lock()
            .map_err(|_| OrchError::Internal("artifact ownership registry poisoned".into()))?
            .insert(id);
        Ok(())
    }

    fn release_artifact_owner(&self, id: Uuid) -> Result<(), OrchError> {
        self.artifact_owners
            .lock()
            .map_err(|_| OrchError::Internal("artifact ownership registry poisoned".into()))?
            .remove(&id);
        Ok(())
    }

    fn snapshot_overlay_path(&self) -> PathBuf {
        self.config
            .socket_dir
            .join("snapshots")
            .join(format!("{}.cow", Uuid::new_v4()))
    }

    fn snapshot_overlay_staging_path(&self) -> PathBuf {
        self.config
            .socket_dir
            .join("snapshots")
            .join(format!(".stage-{}.cow", Uuid::new_v4()))
    }

    fn snapshot_ram_path(&self) -> PathBuf {
        self.config
            .socket_dir
            .join("snapshots")
            .join(format!("bundle-{}.ram", Uuid::new_v4()))
    }

    fn snapshot_ram_staging_path(&self) -> PathBuf {
        self.config
            .socket_dir
            .join("snapshots")
            .join(format!(".stage-{}.ram", Uuid::new_v4()))
    }

    fn prepare_runtime(
        &self,
        id: Uuid,
        vm_config: &VmSpawnConfig,
        snapshot_path: Option<&Path>,
    ) -> Result<PreparedRuntime, OrchError> {
        let data_volumes = self.prepare_data_volumes(vm_config)?;
        let Some(jails) = &self.jails else {
            let host_overlay = vm_config.rootfs_path.as_ref().map(|_| {
                snapshot_path.map_or_else(
                    || PathBuf::from(self.overlay_path_for(id)),
                    |snapshot| PathBuf::from(self.restore_overlay_path_for_snapshot(id, snapshot)),
                )
            });
            return Ok(PreparedRuntime {
                host_socket: self.socket_path_for(id),
                socket_argument: self.socket_path_for(id),
                vm_config: vm_config.clone(),
                host_rootfs: vm_config.rootfs_path.clone(),
                guest_overlay: host_overlay.as_ref().map(|path| path.display().to_string()),
                host_overlay,
                guest_snapshot: snapshot_path.map(|path| path.display().to_string()),
                data_volumes,
                jail: None,
            });
        };
        let lease = jails.lease(id)?;
        if let Err(error) = prepare_jail_layout(&lease) {
            let _ = jails.release(id);
            return Err(error);
        }
        let kernel_host = lease.root.join(JAIL_KERNEL_PATH.trim_start_matches('/'));
        if let Err(error) =
            copy_jail_asset(&vm_config.kernel_path, &kernel_host, lease.uid, lease.gid)
        {
            let _ = jails.release(id);
            return Err(error);
        }
        let (rootfs_path, host_overlay, guest_overlay) =
            if let Some(rootfs) = &vm_config.rootfs_path {
                let rootfs_host = lease.root.join(JAIL_ROOTFS_PATH.trim_start_matches('/'));
                if let Err(error) = copy_jail_asset(rootfs, &rootfs_host, lease.uid, lease.gid) {
                    let _ = jails.release(id);
                    return Err(error);
                }
                let overlay_path = if let Some(snapshot_path) = snapshot_path {
                    // VMM-created scratch paths are returned to taritd for an
                    // inode-identity ownership handoff. Keep every jailed VMM
                    // path absolute so host_path_for_vmm maps it back beneath
                    // this VM's jail root; a relative `assets/...` path would
                    // incorrectly resolve against taritd's working directory.
                    jail_guest_path(
                        &lease.root,
                        Path::new(&self.restore_overlay_path_for_snapshot(id, snapshot_path)),
                    )?
                } else {
                    JAIL_OVERLAY_PATH.to_string()
                };
                (
                    Some(PathBuf::from(JAIL_ROOTFS_PATH)),
                    Some(lease.root.join(overlay_path.trim_start_matches('/'))),
                    Some(overlay_path),
                )
            } else {
                (None, None, None)
            };
        let guest_snapshot = if let Some(snapshot_path) = snapshot_path {
            let host = lease.root.join(JAIL_RESTORE_PATH.trim_start_matches('/'));
            if let Err(error) = copy_jail_asset(snapshot_path, &host, lease.uid, lease.gid) {
                let _ = jails.release(id);
                return Err(error);
            }
            Some(JAIL_RESTORE_PATH.to_string())
        } else {
            None
        };
        Ok(PreparedRuntime {
            host_socket: lease.root.join(JAIL_SOCKET_PATH.trim_start_matches('/')),
            socket_argument: PathBuf::from(JAIL_SOCKET_PATH),
            vm_config: VmSpawnConfig {
                kernel_path: PathBuf::from(JAIL_KERNEL_PATH),
                rootfs_path,
                ..vm_config.clone()
            },
            host_rootfs: vm_config
                .rootfs_path
                .as_ref()
                .map(|_| lease.root.join(JAIL_ROOTFS_PATH.trim_start_matches('/'))),
            host_overlay,
            guest_overlay,
            guest_snapshot,
            data_volumes,
            jail: Some(lease),
        })
    }

    fn prepare_data_volumes(
        &self,
        vm_config: &VmSpawnConfig,
    ) -> Result<Vec<PreparedBlockAttachment>, OrchError> {
        if vm_config.data_volumes.is_empty() {
            return Ok(Vec::new());
        }
        vm_config
            .data_volumes
            .iter()
            .map(|volume| {
                let provider = crate::volume_provider::open(&self.config, &volume.provider)?;
                provider
                    .prepare(
                        volume.id,
                        volume.size_bytes,
                        if volume.read_only {
                            AccessMode::ReadOnlyMany
                        } else {
                            AccessMode::ReadWriteOnce
                        },
                        volume.generation,
                    )
                    .map_err(|error| {
                        tracing::error!(volume_id = %volume.id, %error,
                            "prepare runtime volume failed closed");
                        OrchError::Internal(format!("prepare runtime volume {} failed", volume.id))
                    })
            })
            .collect()
    }

    fn release_jail(&self, id: Uuid) -> Result<(), OrchError> {
        match &self.jails {
            Some(jails) => jails.release(id),
            None => Ok(()),
        }
    }

    fn jail_identity(&self, id: Uuid) -> Result<Option<(u32, u32)>, OrchError> {
        match &self.jails {
            Some(jails) => jails.identity(id),
            None => Ok(None),
        }
    }

    fn host_path_for_vmm(&self, id: Uuid, path: &str) -> PathBuf {
        match &self.jails {
            Some(jails) if Path::new(path).is_absolute() => {
                jails.root_for(id).join(path.trim_start_matches('/'))
            }
            _ => PathBuf::from(path),
        }
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
    fn cgroup_args(
        &self,
        id: Uuid,
        shape: ResourceShape,
        runtime: &PreparedRuntime,
    ) -> Result<Vec<String>, OrchError> {
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
        let mut args = vec![
            "--cgroup".to_string(),
            path.display().to_string(),
            "--cgroup-pids-max".to_string(),
            self.config.vm_cgroup_pids_max.to_string(),
            "--cgroup-cpu-max".to_string(),
            format!("{cpu_millis}m"),
            "--cgroup-memory-max".to_string(),
            format!("{max_mib}M"),
        ];
        if let Some(io_max) = self.cgroup_io_max_arg(runtime)? {
            args.push("--cgroup-io-max".to_string());
            args.push(io_max);
        }
        Ok(args)
    }

    #[cfg(target_os = "linux")]
    fn cgroup_io_max_arg(&self, runtime: &PreparedRuntime) -> Result<Option<String>, OrchError> {
        self.cgroup_io_max_for_paths_and_files(
            runtime.host_rootfs.as_deref(),
            runtime.host_overlay.as_deref(),
            &runtime
                .data_volumes
                .iter()
                .map(|volume| &volume.file)
                .collect::<Vec<_>>(),
        )
    }

    #[cfg(any(target_os = "linux", test))]
    fn cgroup_io_max_for_paths(
        &self,
        rootfs: Option<&Path>,
        overlay: Option<&Path>,
    ) -> Result<Option<String>, OrchError> {
        self.cgroup_io_max_for_paths_and_files(rootfs, overlay, &[])
    }

    #[cfg(any(target_os = "linux", test))]
    fn cgroup_io_max_for_paths_and_files(
        &self,
        rootfs: Option<&Path>,
        overlay: Option<&Path>,
        data_volumes: &[&File],
    ) -> Result<Option<String>, OrchError> {
        use std::collections::BTreeSet;

        let quota = &self.config.vm_io_quota;
        if !quota.is_configured() {
            return Ok(None);
        }

        let mut devices = BTreeSet::new();
        if let Some(rootfs) = rootfs {
            let metadata = std::fs::metadata(rootfs).map_err(|error| {
                OrchError::Internal(format!(
                    "stat VM rootfs {} for cgroup io.max: {error}",
                    rootfs.display()
                ))
            })?;
            devices.insert(cgroup_device_number(&metadata)?);
            let overlay = overlay.ok_or_else(|| {
                OrchError::Internal("rootfs-backed VM is missing an overlay path".into())
            })?;
            let overlay_backing = if overlay.exists() {
                overlay
            } else {
                overlay.parent().ok_or_else(|| {
                    OrchError::Internal(format!(
                        "overlay path {} has no backing directory",
                        overlay.display()
                    ))
                })?
            };
            let metadata = std::fs::metadata(overlay_backing).map_err(|error| {
                OrchError::Internal(format!(
                    "stat VM overlay backing {} for cgroup io.max: {error}",
                    overlay_backing.display()
                ))
            })?;
            devices.insert(cgroup_device_number(&metadata)?);
        }
        for volume in data_volumes {
            devices.insert(cgroup_device_number(&volume.metadata().map_err(
                |error| {
                    OrchError::Internal(format!(
                        "inspect persistent volume for cgroup io.max: {error}"
                    ))
                },
            )?)?);
        }
        if devices.is_empty() {
            return Ok(None);
        }

        let suffix = [
            quota.read_bps_max.map(|value| format!(" rbps={value}")),
            quota.write_bps_max.map(|value| format!(" wbps={value}")),
            quota.read_iops_max.map(|value| format!(" riops={value}")),
            quota.write_iops_max.map(|value| format!(" wiops={value}")),
        ]
        .into_iter()
        .flatten()
        .collect::<String>();
        Ok((!suffix.is_empty()).then(|| {
            devices
                .into_iter()
                .map(|device| format!("{device}{suffix}"))
                .collect::<Vec<_>>()
                .join("\n")
        }))
    }

    #[cfg(not(target_os = "linux"))]
    fn cgroup_io_max_arg(&self, _runtime: &PreparedRuntime) -> Result<Option<String>, OrchError> {
        Ok(None)
    }

    fn readopted_cgroup_limit_plan(&self, record: &VmRecord) -> Result<CgroupLimitPlan, OrchError> {
        let cpu_quota = u64::from(record.vcpus)
            .checked_mul(100_000)
            .ok_or_else(|| OrchError::BadRequest("vCPU cgroup limit overflow".into()))?;
        let max_mib = record
            .memory_mib
            .checked_add(record.memory_mib / 2)
            .and_then(|value| value.checked_add(256))
            .ok_or_else(|| OrchError::BadRequest("memory cgroup limit overflow".into()))?;
        let memory_max = max_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| OrchError::BadRequest("memory cgroup byte limit overflow".into()))?;
        let io_max = self.readopted_cgroup_io_max(record)?;
        Ok(CgroupLimitPlan {
            cpu_max: Some(format!("{cpu_quota} 100000")),
            cpu_weight: Some(NORMAL_CGROUP_CPU_WEIGHT),
            memory_max: Some(memory_max),
            pids_max: Some(self.config.vm_cgroup_pids_max),
            io_weight: self.config.vm_io_quota.is_configured().then_some(100),
            io_max,
            ..CgroupLimitPlan::default()
        })
    }

    #[cfg(any(target_os = "linux", test))]
    fn readopted_cgroup_io_max(&self, record: &VmRecord) -> Result<Option<String>, OrchError> {
        let layout = record.runtime_layout.as_ref().ok_or_else(|| {
            OrchError::Internal(format!(
                "VM {} has no runtime layout for cgroup recovery",
                record.id
            ))
        })?;
        let rootfs = record.rootfs_path.as_ref().map(|rootfs| {
            layout
                .jail_path
                .as_ref()
                .map(|jail| Path::new(jail).join(JAIL_ROOTFS_PATH.trim_start_matches('/')))
                .unwrap_or_else(|| PathBuf::from(rootfs))
        });
        let overlay = layout.overlay_path.as_deref().map(PathBuf::from);
        self.cgroup_io_max_for_paths(rootfs.as_deref(), overlay.as_deref())
    }

    #[cfg(all(not(target_os = "linux"), not(test)))]
    fn readopted_cgroup_io_max(&self, _record: &VmRecord) -> Result<Option<String>, OrchError> {
        Ok(None)
    }

    fn reconcile_readopted_cgroup(&self, record: &VmRecord, pid: u32) -> Result<(), OrchError> {
        let Some(path) = self.exact_vm_cgroup_path(record.id) else {
            return Ok(());
        };
        let parent = self
            .config
            .vm_cgroup_parent
            .as_deref()
            .map(Path::new)
            .ok_or_else(|| OrchError::Internal("VM cgroup parent disappeared".into()))?;
        validate_owned_vm_cgroup(parent, &path, record.id, pid).map_err(|error| {
            OrchError::Internal(format!(
                "verify exact cgroup ownership for adopted VM {}: {error}",
                record.id
            ))
        })?;
        let plan = self.readopted_cgroup_limit_plan(record)?;
        apply_and_verify_cgroup_limits(&path, &plan).map_err(|error| {
            OrchError::Internal(format!(
                "reapply cgroup limits for adopted VM {} in {}: {error}",
                record.id,
                path.display()
            ))
        })
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

    pub(crate) fn disk_pressure_snapshot(&self) -> DiskPressureSnapshot {
        self.disk_pressure.snapshot()
    }

    pub(crate) fn refresh_disk_pressure(&self) -> Result<DiskPressureSnapshot, OrchError> {
        self.disk_pressure.refresh()
    }

    pub(crate) fn disk_sweep_interval(&self) -> Duration {
        self.disk_pressure.sweep_interval()
    }

    fn reserve_snapshot_space(
        &self,
        id: Uuid,
        memory_mib: u64,
        has_overlay: bool,
    ) -> Result<DiskReservation, OrchError> {
        let ram_bytes = memory_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| OrchError::BadRequest("snapshot RAM estimate overflow".into()))?;
        let overlay_bytes = if has_overlay {
            let path = PathBuf::from(self.overlay_path_for(id));
            match std::fs::metadata(&path) {
                Ok(metadata) => metadata.blocks().saturating_mul(512),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => {
                    return Err(OrchError::Internal(format!(
                        "estimate snapshot overlay {}: {error}",
                        path.display()
                    )))
                }
            }
        } else {
            0
        };
        let scratch_path = match &self.jails {
            Some(jails) => jails.root_for(id).join("tmp"),
            None => std::env::temp_dir(),
        };
        let mut growth = vec![
            PathGrowth {
                path: scratch_path,
                bytes: ram_bytes,
                inodes: 1,
            },
            PathGrowth {
                path: self.snapshot_ram_staging_path(),
                bytes: ram_bytes,
                inodes: 1,
            },
            PathGrowth {
                path: self
                    .config
                    .socket_dir
                    .join("snapshots")
                    .join(".integrity-reservation"),
                bytes: ram_bytes
                    .saturating_add(overlay_bytes)
                    .div_ceil(u64::from(tarit_proto::INTEGRITY_CHUNK_SIZE))
                    .saturating_mul(32)
                    .saturating_add(4096),
                inodes: 1,
            },
        ];
        if has_overlay {
            growth.push(PathGrowth {
                path: self.snapshot_overlay_staging_path(),
                bytes: overlay_bytes,
                inodes: 1,
            });
        }
        self.disk_pressure.reserve_growth("VM snapshot", growth)
    }

    pub(crate) fn reserve_artifact_localization(
        &self,
        directory: PathBuf,
        bytes: u64,
        inodes: u64,
    ) -> Result<DiskReservation, OrchError> {
        self.disk_pressure.reserve_growth(
            "artifact localization",
            [PathGrowth {
                path: directory,
                bytes,
                inodes,
            }],
        )
    }

    pub(crate) fn sweep_owned_artifacts(
        &self,
        mut references: ArtifactReferences,
    ) -> Result<GcReport, OrchError> {
        // Hold the authoritative runtime registry through deletion. Creation
        // registers before materializing artifacts, and teardown unregisters
        // only after deletion, so every live jail/overlay is continuously
        // protected even while moving between booting, warm, and running maps.
        let owners = self
            .artifact_owners
            .lock()
            .map_err(|_| OrchError::Internal("artifact ownership registry poisoned".into()))?;
        references.active_vm_ids.extend(owners.iter().copied());
        references.runtime_paths.extend(
            owners
                .iter()
                .map(|id| PathBuf::from(self.overlay_path_for(*id))),
        );
        // Publication holds this registry while staging names are atomically
        // renamed into the GC namespace. Holding the same lock through the
        // sweep closes the rename-to-registration race.
        let in_progress = self
            .in_progress_artifacts
            .lock()
            .map_err(|_| OrchError::Internal("artifact publication registry poisoned".into()))?;
        references.runtime_paths.extend(in_progress.iter().cloned());
        if let Ok(golden) = self.golden_artifacts.lock() {
            references
                .runtime_paths
                .extend(golden.iter().map(|artifact| artifact.path().to_path_buf()));
        }
        let report = crate::disk::sweep_owned_artifacts(
            &self.config.socket_dir,
            self.config
                .vm_jail
                .as_ref()
                .map(|jail| jail.base_dir.as_path()),
            &references,
            self.disk_pressure.artifact_min_age(),
        )?;
        if let Some(jails) = &self.jails {
            jails.reconcile_present()?;
        }
        self.disk_pressure.record_sweep(&report);
        self.disk_pressure.refresh()?;
        Ok(report)
    }

    fn publish_artifacts(
        &self,
        artifacts: &mut [(OwnedArtifact, PathBuf)],
    ) -> Result<Vec<PathBuf>, OrchError> {
        let mut registered = self
            .in_progress_artifacts
            .lock()
            .map_err(|_| OrchError::Internal("artifact publication registry poisoned".into()))?;
        let paths = artifacts
            .iter()
            .map(|(_, destination)| destination.clone())
            .collect::<Vec<_>>();
        registered.extend(paths.iter().cloned());
        for (artifact, destination) in artifacts.iter_mut() {
            if let Err(error) = artifact.publish(destination) {
                for path in &paths {
                    registered.remove(path);
                }
                return Err(OrchError::Internal(format!(
                    "publish snapshot artifact {}: {error}",
                    destination.display()
                )));
            }
        }
        Ok(paths)
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
        self.disk_pressure.ensure_admission(match purpose {
            SpawnPurpose::Live => "VM create or restore",
            SpawnPurpose::Refill => "warm-pool refill",
        })?;
        let _gate = self.boot_gate.lock().await;
        if self.is_shutting_down() {
            return Err(self.shutdown_error());
        }

        if self
            .booting
            .lock()
            .map_err(|_| OrchError::Internal("supervisor booting lock poisoned".into()))?
            .contains_key(&id)
        {
            return Err(OrchError::Conflict(format!(
                "VM {id} already has a registered boot"
            )));
        }

        let control = Arc::new(BootControl::new(purpose));
        let socket_path = self.socket_path_for(id);
        self.register_artifact_owner(id)?;
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
            let _ = self.release_artifact_owner(id);
            return Err(error);
        }
        // Reserve capacity before the durable registration so capacity
        // rejections leave no durable trace: the admission loop retries the
        // same id, and a leftover Error tombstone from a rejected attempt
        // would make the re-registration fail with an incarnation conflict.
        if let Err(error) = self.scheduler.try_reserve(id, shape) {
            self.complete_booting(id, &control, Ok(()));
            self.release_artifact_owner(id)?;
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
            self.release_artifact_owner(id)?;
            return Err(error);
        }
        Ok(BootTicket {
            id,
            control,
            purpose,
            shape,
        })
    }

    /// Wait for a boot already registered for this VM, or recognize the narrow
    /// handoff window after the runtime moved to `running`. The boot entry and
    /// its completion signal share one control object, so callers cannot mistake
    /// an unrelated leaked scheduler reservation for the active incarnation.
    pub(crate) async fn wait_for_registered_boot_or_running(
        &self,
        id: Uuid,
    ) -> Result<bool, OrchError> {
        let control = self
            .booting
            .lock()
            .map_err(|_| OrchError::Internal("supervisor booting lock poisoned".into()))?
            .get(&id)
            .map(|booting| Arc::clone(&booting.control));
        let Some(control) = control else {
            return self
                .running
                .lock()
                .map_err(|_| OrchError::Internal("supervisor lock poisoned".into()))
                .map(|running| running.contains_key(&id));
        };
        tokio::task::spawn_blocking(move || control.wait_for_completion())
            .await
            .map_err(|error| OrchError::Internal(format!("wait for registered boot: {error}")))??;
        Ok(true)
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
        let control = tasks.remove(&existing_key).ok_or_else(|| {
            OrchError::Internal(format!(
                "owned task key {existing_key} disappeared during warm transfer"
            ))
        })?;
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
    fn pause_after_warm_dequeue_for_test(&self) -> SpawnAttachmentPause {
        let pause = SpawnAttachmentPause::default();
        *self.warm_handoff_pause.lock().unwrap() = Some(pause.clone());
        pause
    }

    #[cfg(test)]
    fn wait_after_warm_dequeue_before_running_insert(&self) {
        let pause = self
            .warm_handoff_pause
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
        self.register_artifact_owner(id)?;
        let mut booting = self.booting.lock().map_err(|_| {
            let _ = self.release_artifact_owner(id);
            OrchError::Internal("supervisor booting lock poisoned".into())
        })?;
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
            let _ = self.release_artifact_owner(ticket.id);
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
        self.register_artifact_owner(id)?;
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
        if let Err(error) = self.teardown_vm(id, vm) {
            let cleanup = error.to_string();
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
            return error;
        }
        let mut cleanup_failures = Vec::new();
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

    fn cleanup_boot_failure_without_process(
        &self,
        id: Uuid,
        control: &Arc<BootControl>,
        cause: OrchError,
    ) -> OrchError {
        let mut cleanup_failures = Vec::new();
        if let Some(net) = &self.net {
            if let Err(error) = net.teardown_vm_id(id) {
                cleanup_failures.push(format!("teardown network allocation: {error}"));
            }
        }
        if let Err(error) = self.release_jail(id) {
            cleanup_failures.push(format!("release staged jail: {error}"));
        }
        if cleanup_failures.is_empty() {
            self.complete_booting(id, control, Ok(()));
            let _ = self.release_artifact_owner(id);
            if control.purpose == SpawnPurpose::Refill {
                self.release_reservation_after_cleanup(id);
            }
            cause
        } else {
            let cleanup = cleanup_failures.join("; ");
            let error = OrchError::Internal(format!(
                "{cause}; cleanup retained booting VM {id} for retry: {cleanup}"
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
        if let Some(process) = booting_vm.process.as_ref() {
            self.teardown_vm(
                id,
                &RunningVm::new(
                    process.pid,
                    booting_vm.socket_path.clone(),
                    process.clone(),
                    None,
                ),
            )?;
        }
        let mut failures = Vec::new();
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
        runtime: &PreparedRuntime,
        net_alloc: Option<NetAlloc>,
    ) -> Result<RunningVm, OrchError> {
        let id = ticket.id;
        let socket_path = runtime.host_socket.clone();
        let _ = std::fs::remove_file(&socket_path);
        let cgroup_args = self.cgroup_args(id, ticket.shape, runtime)?;
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
            return Err(self.cleanup_boot_failure_without_process(
                id,
                &ticket.control,
                self.shutdown_error(),
            ));
        }
        let mut command = Command::new(&self.config.vmm_bin);
        command
            .arg("serve")
            .arg("--socket")
            .arg(&runtime.socket_argument)
            .args(&cgroup_args);
        if let Some(jail) = &runtime.jail {
            command
                .arg("--jail")
                .arg(&jail.root)
                .arg("--uid")
                .arg(jail.uid.to_string())
                .arg("--gid")
                .arg(jail.gid.to_string())
                .arg("--seccomp");
            // `std::env::temp_dir()` caches the inherited TMPDIR. Once the VMM
            // has chrooted, a host-absolute value is both unreachable and an
            // information leak. prepare_jail_layout owns `/tmp` to the exact
            // jail identity, so force every VMM scratch path into that tree.
            command.env("TMPDIR", "/tmp").env_remove("XDG_RUNTIME_DIR");
            if let Some(jail_config) = &self.config.vm_jail {
                if jail_config.pid_namespace {
                    command.arg("--pid-namespace");
                }
                if jail_config.network_namespace {
                    command.arg("--isolate-network");
                }
            }
        }
        #[cfg(target_os = "linux")]
        let inherited_tap = match net_alloc
            .as_ref()
            .map(|allocation| open_inherited_tap(&allocation.tap))
            .transpose()
        {
            Ok(fd) => fd,
            Err(error) => {
                drop(boot_gate);
                return Err(self.cleanup_boot_failure_without_process(id, &ticket.control, error));
            }
        };
        #[cfg(target_os = "linux")]
        let inherited_kvm = if runtime.jail.is_some() {
            match open_inherited_kvm() {
                Ok(file) => Some(file),
                Err(error) => {
                    drop(boot_gate);
                    return Err(self.cleanup_boot_failure_without_process(
                        id,
                        &ticket.control,
                        error,
                    ));
                }
            }
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        if let Some(file) = &inherited_kvm {
            let raw_fd = file.as_raw_fd();
            command.env("VMM_KVM_FD", raw_fd.to_string());
            // SAFETY: this runs only in the VMM child and clears CLOEXEC on the
            // exact `/dev/kvm` descriptor opened and verified above.
            unsafe {
                command.pre_exec(move || clear_cloexec_for_child(raw_fd));
            }
        }
        #[cfg(target_os = "linux")]
        if let (Some(allocation), Some(fd)) = (&net_alloc, &inherited_tap) {
            let raw_fd = fd.as_raw_fd();
            command.env("VMM_TAP_FDS", format!("{}={raw_fd}", allocation.tap));
            // SAFETY: this runs after fork in the VMM child. It changes only
            // the explicitly inherited TAP descriptor before exec.
            unsafe {
                command.pre_exec(move || clear_cloexec_for_child(raw_fd));
            }
        }
        #[cfg(target_os = "linux")]
        if !runtime.data_volumes.is_empty() {
            let raw_fds = runtime
                .data_volumes
                .iter()
                .map(|volume| volume.file.as_raw_fd())
                .collect::<Vec<_>>();
            // SAFETY: this executes in the VMM child after fork. It changes
            // only the verified volume descriptors retained by PreparedRuntime
            // so they survive exec without exposing provider paths to the jail.
            unsafe {
                command.pre_exec(move || {
                    for raw_fd in &raw_fds {
                        clear_cloexec_for_child(*raw_fd)?;
                    }
                    Ok(())
                });
            }
        }
        let child = match command
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
                return Err(self.cleanup_boot_failure_without_process(
                    id,
                    &ticket.control,
                    OrchError::Internal(format!("spawn vmm: {error}")),
                ));
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
        let vm = RunningVm::new(pid, socket_path, process, net_alloc);
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
        let runtime = match self.prepare_runtime(id, vm_config, None) {
            Ok(runtime) => runtime,
            Err(error) => {
                self.complete_booting(id, &ticket.control, Ok(()));
                let _ = self.release_artifact_owner(id);
                if ticket.purpose == SpawnPurpose::Refill {
                    self.release_reservation_after_cleanup(id);
                }
                return Err(error);
            }
        };
        let net_alloc = match &self.net {
            Some(provisioner) => {
                match provisioner.provision_with_owner_and_policy(
                    id,
                    self.jail_identity(id)?,
                    &vm_config.egress_allowlist,
                    vm_config.egress_allow_existing,
                ) {
                    Ok(allocation) => Some(allocation),
                    Err(error) => {
                        let cause = match error {
                            error @ OrchError::Overloaded { .. } => error,
                            error => OrchError::Internal(format!("net provision: {error}")),
                        };
                        return Err(self.cleanup_boot_failure_without_process(
                            id,
                            &ticket.control,
                            cause,
                        ));
                    }
                }
            }
            None => None,
        };
        let base_vm = self.spawn_server_for_boot(&ticket, &runtime, net_alloc)?;
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

        let vm = base_vm;
        if !boot_can_publish(&ticket.control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(id, &ticket.control, &vm, self.shutdown_error()));
        }

        let vmm_config = build_vmm_config(
            &runtime.vm_config,
            vm.net.as_ref(),
            runtime.guest_overlay.clone(),
            &runtime.data_volumes,
        );
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

    fn require_restored_guest_ready_and_network(
        &self,
        booted: BootedVm,
    ) -> Result<BootedVm, OrchError> {
        let booted = self.require_booted_guest_ready(booted, "restore")?;
        if !boot_can_publish(&booted.control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(
                booted.id,
                &booted.control,
                &booted.vm,
                self.shutdown_error(),
            ));
        }
        if let Some(allocation) = &booted.vm.net {
            let repaired = rebind_restored_guest_network(&booted.vm.socket_path, allocation)
                .and_then(|()| verify_restored_guest_connectivity(allocation));
            if let Err(error) = repaired {
                return Err(self.cleanup_boot_failure(
                    booted.id,
                    &booted.control,
                    &booted.vm,
                    error,
                ));
            }
        }
        if !boot_can_publish(&booted.control, self.is_shutting_down()) {
            return Err(self.cleanup_boot_failure(
                booted.id,
                &booted.control,
                &booted.vm,
                self.shutdown_error(),
            ));
        }
        Ok(booted)
    }

    /// Restore a VM from a node-local snapshot file: spawn a fresh `vmm serve`,
    /// send Restore, and register the resumed VM. Host network bindings are
    /// always replaced with a fresh allocation; saved tap/IP bindings are never
    /// reused across VM incarnations. Before returning a publishable VM, the
    /// restored guest address and default route are rebound to that allocation
    /// and both guest configuration and host-to-guest connectivity are checked.
    fn spawn_and_restore(
        &self,
        ticket: BootTicket,
        snapshot_path: &str,
        overlay: RestoreOverlay,
        vm_config: &VmSpawnConfig,
        shape: ResourceShape,
        integrity: Option<VerifiedSnapshotIntegrity>,
    ) -> Result<BootedVm, OrchError> {
        let id = ticket.id;
        debug_assert_eq!(ticket.shape, shape);
        debug_assert_eq!(vm_config.resource_shape(), shape);
        let runtime = match self.prepare_runtime(id, vm_config, Some(Path::new(snapshot_path))) {
            Ok(runtime) => runtime,
            Err(error) => {
                self.complete_booting(id, &ticket.control, Ok(()));
                let _ = self.release_artifact_owner(id);
                if ticket.purpose == SpawnPurpose::Refill {
                    self.release_reservation_after_cleanup(id);
                }
                return Err(error);
            }
        };
        let (memory_integrity, overlay_integrity, integrity_chunk_size) = match integrity {
            Some(integrity) => {
                let manifest_path = if let Some(jail) = &runtime.jail {
                    let destination = jail
                        .root
                        .join(JAIL_RESTORE_INTEGRITY_PATH.trim_start_matches('/'));
                    if let Err(error) = copy_jail_asset(
                        Path::new(&integrity.manifest_path),
                        &destination,
                        jail.uid,
                        jail.gid,
                    ) {
                        return Err(self.cleanup_boot_failure_without_process(
                            id,
                            &ticket.control,
                            error,
                        ));
                    }
                    JAIL_RESTORE_INTEGRITY_PATH.to_string()
                } else {
                    integrity.manifest_path
                };
                (
                    Some(tarit_vmm_client::MemoryIntegrity {
                        manifest_path,
                        manifest_sha256: integrity.manifest_sha256,
                    }),
                    integrity.overlay,
                    integrity.chunk_size,
                )
            }
            None => (None, None, 0),
        };
        let net_alloc = match &self.net {
            Some(provisioner) => {
                match provisioner.provision_with_owner_and_policy(
                    id,
                    self.jail_identity(id)?,
                    &vm_config.egress_allowlist,
                    vm_config.egress_allow_existing,
                ) {
                    Ok(allocation) => Some(allocation),
                    Err(error) => {
                        return Err(self.cleanup_boot_failure_without_process(
                            id,
                            &ticket.control,
                            error,
                        ));
                    }
                }
            }
            None => None,
        };
        let base_vm = self.spawn_server_for_boot(&ticket, &runtime, net_alloc)?;
        if let Err(error) =
            self.wait_for_socket_or_cancellation(&base_vm.socket_path, &ticket.control)
        {
            return Err(self.cleanup_boot_failure(id, &ticket.control, &base_vm, error));
        }
        // Same readiness boundary as boot_vm: restore-based refills must run at
        // refill CPU weight or warm replenishment competes with leased VMs.
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

        // The Restore RPC receives only the new UUID-scoped destination. For a
        // durable snapshot this destination is seeded from SnapshotRecord's
        // owned disk artifact; the source overlay path serialized in RAM state
        // is never reopened.
        let overlay = match overlay {
            RestoreOverlay::None => None,
            RestoreOverlay::Seeded(source) => {
                let destination = runtime
                    .host_overlay
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(self.overlay_path_for(id)));
                if let Err(error) = copy_private_artifact(&source, &destination) {
                    return Err(self.cleanup_boot_failure(
                        id,
                        &ticket.control,
                        &base_vm,
                        OrchError::Internal(format!("seed private restore disk upper: {error}")),
                    ));
                }
                if let Some(jail) = &runtime.jail {
                    if let Err(error) = set_jail_owner_mode(&destination, jail.uid, jail.gid, 0o600)
                    {
                        return Err(self.cleanup_boot_failure(
                            id,
                            &ticket.control,
                            &base_vm,
                            error,
                        ));
                    }
                }
                if let Some(integrity) = overlay_integrity.as_ref() {
                    let destination_artifact =
                        OwnedArtifact::capture(&destination).map_err(|error| {
                            self.cleanup_boot_failure(
                                id,
                                &ticket.control,
                                &base_vm,
                                OrchError::Internal(format!(
                                    "capture seeded restore disk upper: {error}"
                                )),
                            )
                        })?;
                    if let Err(error) = verify_file_artifact_integrity(
                        &destination_artifact._file,
                        integrity,
                        integrity_chunk_size,
                        "seeded restore disk",
                    ) {
                        return Err(self.cleanup_boot_failure(
                            id,
                            &ticket.control,
                            &base_vm,
                            error,
                        ));
                    }
                }
                runtime
                    .guest_overlay
                    .clone()
                    .or_else(|| Some(destination.display().to_string()))
            }
        };

        let vm = base_vm;
        let net_override = vm
            .net
            .as_ref()
            .map(net_config_for_allocation)
            .into_iter()
            .collect::<Vec<_>>();

        // A restore can legitimately spend longer than the control client's
        // default five-second I/O timeout servicing lazy faults and waiting
        // for the guest clone-repair barrier. Keep the request bounded by the
        // lifecycle deadline used by the other snapshot operations.
        let client = VmmClient::new(&vm.socket_path).with_request_timeout(LIFECYCLE_OP_TIMEOUT);
        let restore_path = runtime.guest_snapshot.as_deref().unwrap_or(snapshot_path);
        let volume_override = runtime_volume_configs(&runtime.data_volumes);
        if let Err(e) = client.restore_with_resource_overrides(
            restore_path,
            overlay.clone(),
            Some(net_override),
            Some(volume_override),
            tarit_vmm_client::RestoreMemoryPolicy::Auto,
            memory_integrity,
        ) {
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
        self.require_restored_guest_ready_and_network(BootedVm {
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
        vm_config: VmSpawnConfig,
        shape: ResourceShape,
        integrity: VerifiedSnapshotIntegrity,
    ) -> Result<BootedVm, OrchError> {
        let overlay = snapshot_overlay_path
            .map(PathBuf::from)
            .map(RestoreOverlay::Seeded)
            .unwrap_or(RestoreOverlay::None);
        self.spawn_and_restore(
            ticket,
            &snapshot_path,
            overlay,
            &vm_config,
            shape,
            Some(integrity),
        )
    }

    /// Boot one warm-pool VM of `class` and park it in the warm queue. The boot
    /// happens without the warm lock held; only the final enqueue takes it.
    /// Block until the guest agent answers its transport-level no-op, so we
    /// never park a still-booting VM. The empty command is handled directly by
    /// the agent and deliberately does not require `/bin/sh`; distroless OCI
    /// guests are valid even though ordinary shell execution returns 127.
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
                match client.exec("", exec_timeout_ms) {
                    Ok((0, _, _, _)) => Ok(true),
                    Ok((code, _, _, _)) => {
                        Err(format!("agent readiness probe exited with status {code}"))
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
    ) -> Result<GoldenBundle, OrchError> {
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
    ) -> Result<GoldenBundle, OrchError> {
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
        let _reservation =
            match self.reserve_snapshot_space(id, spec.memory_mib, spec.rootfs_path.is_some()) {
                Ok(reservation) => reservation,
                Err(error) => {
                    return Err(self.cleanup_boot_failure(id, &booted.control, &booted.vm, error));
                }
            };
        let socket_path = booted.vm.socket_path.clone();
        let snapshot_vmm_path = match tokio::task::spawn_blocking(move || {
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
        let snapshot_host_path = self.host_path_for_vmm(id, &snapshot_vmm_path);
        let scratch_snapshot = OwnedArtifact::capture(&snapshot_host_path).map_err(|error| {
            self.cleanup_boot_failure(
                id,
                &booted.control,
                &booted.vm,
                OrchError::Internal(format!("capture golden snapshot scratch: {error}")),
            )
        })?;
        let expected_uid = self
            .jail_identity(id)?
            .map(|(uid, _)| uid)
            .unwrap_or_else(|| unsafe { libc::geteuid() });
        let durable_snapshot_staging = self.snapshot_ram_staging_path();
        let durable_snapshot_path = self.snapshot_ram_path();
        let durable_snapshot = copy_private_artifact_owned(
            &snapshot_host_path,
            &durable_snapshot_staging,
            expected_uid,
        )
        .map_err(|error| {
            self.cleanup_boot_failure(
                id,
                &booted.control,
                &booted.vm,
                OrchError::Internal(format!("copy golden RAM snapshot: {error}")),
            )
        })?;
        let durable_overlay = match self.overlay_path_for_config(id, &spec) {
            Some(source) => {
                let staging = self.snapshot_overlay_staging_path();
                let destination = self.snapshot_overlay_path();
                match copy_private_artifact_owned(Path::new(&source), &staging, expected_uid) {
                    Ok(artifact) => Some((artifact, destination)),
                    Err(error) => {
                        let _ = durable_snapshot.remove();
                        return Err(self.cleanup_boot_failure(
                            id,
                            &booted.control,
                            &booted.vm,
                            OrchError::Internal(format!("copy golden disk upper: {error}")),
                        ));
                    }
                }
            }
            None => None,
        };
        let overlay_path = durable_overlay
            .as_ref()
            .map(|(_, path)| path.display().to_string());
        let mut publications = vec![(durable_snapshot, durable_snapshot_path.clone())];
        publications.extend(durable_overlay);
        if task.is_cancelled() {
            cleanup_golden_artifacts(publications.into_iter().map(|(artifact, _)| artifact));
            return Err(self.discard_booted_vm(booted));
        }
        let client = VmmClient::new(&booted.vm.socket_path);
        if let Err(error) = client.release_scratch(&snapshot_vmm_path, scratch_snapshot.identity())
        {
            cleanup_golden_artifacts(publications.into_iter().map(|(artifact, _)| artifact));
            return Err(self.cleanup_boot_failure(
                id,
                &booted.control,
                &booted.vm,
                OrchError::Vmm(format!(
                    "release golden scratch {snapshot_vmm_path}: {error}"
                )),
            ));
        }
        if let Err(error) = scratch_snapshot.remove() {
            tracing::warn!(
                path = %snapshot_host_path.display(),
                "released golden snapshot scratch cleanup failed: {error}"
            );
        }
        let registered_paths = match self.publish_artifacts(&mut publications) {
            Ok(paths) => paths,
            Err(error) => {
                cleanup_golden_artifacts(publications.into_iter().map(|(artifact, _)| artifact));
                return Err(self.cleanup_boot_failure(id, &booted.control, &booted.vm, error));
            }
        };
        let mut artifacts = publications
            .into_iter()
            .map(|(artifact, _)| artifact)
            .collect::<Vec<_>>();
        let artifact_keys = artifacts
            .iter()
            .map(|artifact| (artifact.path.clone(), artifact.identity()))
            .collect::<Vec<_>>();
        self.remember_golden_artifacts(&mut artifacts, &registered_paths);
        if task.is_cancelled() {
            cleanup_golden_artifacts(self.take_golden_artifacts(&artifact_keys));
            return Err(self.discard_booted_vm(booted));
        }
        self.finish_booted_vm(id, booted.control, &booted.vm)?;
        Ok(GoldenBundle {
            snapshot_path: durable_snapshot_path.display().to_string(),
            overlay_path,
        })
    }

    /// Restore one warm-pool VM from an existing golden snapshot and park it.
    pub(crate) async fn spawn_warm_restore(
        self: Arc<Self>,
        class: WarmClass,
        bundle: GoldenBundle,
    ) -> Result<(), OrchError> {
        let id = Uuid::new_v4();
        let worker = Arc::clone(&self);
        self.run_owned_task(id, SpawnPurpose::Refill, move |task| async move {
            worker
                .spawn_warm_restore_owned(id, class, bundle, &task)
                .await
        })
        .await
    }

    async fn spawn_warm_restore_owned(
        self: Arc<Self>,
        id: Uuid,
        class: WarmClass,
        bundle: GoldenBundle,
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
        let shape = spec.resource_shape();
        let restore_spec = spec.clone();
        let booted = tokio::task::spawn_blocking(move || {
            let (snapshot_path, overlay) = bundle.into_restore_parts();
            worker.spawn_and_restore(ticket, &snapshot_path, overlay, &restore_spec, shape, None)
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
            warm.remove(pos).ok_or_else(|| {
                OrchError::Internal(format!(
                    "selected warm VM {candidate_id} vanished during transfer"
                ))
            })?
        };
        let pid = taken.vm.pid;
        let socket = taken.vm.socket_path.clone();
        self.configure_leased_cgroup(candidate_id, pid);
        #[cfg(test)]
        self.wait_after_warm_dequeue_before_running_insert();
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

    /// Number of warm VMs currently parked for this exact boot configuration.
    pub fn warm_count(&self, want: &VmSpawnConfig) -> usize {
        self.warm
            .lock()
            .map(|w| w.iter().filter(|x| &x.spec == want).count())
            .unwrap_or(0)
    }

    fn remember_golden_artifacts(
        &self,
        artifacts: &mut Vec<OwnedArtifact>,
        registered_paths: &[PathBuf],
    ) {
        let mut in_progress = self
            .in_progress_artifacts
            .lock()
            .unwrap_or_else(|poisoned| {
                tracing::error!("artifact publication registry poisoned while committing golden");
                poisoned.into_inner()
            });
        let mut registered = self.golden_artifacts.lock().unwrap_or_else(|poisoned| {
            tracing::error!("golden artifact lock poisoned while committing golden");
            poisoned.into_inner()
        });
        registered.append(artifacts);
        for path in registered_paths {
            in_progress.remove(path);
        }
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

    fn owned_runtime_ids(&self) -> Result<HashSet<Uuid>, OrchError> {
        let mut ids = HashSet::new();
        if let Some(jails) = &self.jails {
            ids.extend(jails.ids()?);
        }
        if let Some(parent) = self.config.vm_cgroup_parent.as_ref().map(Path::new) {
            if parent.exists() {
                for entry in std::fs::read_dir(parent).map_err(|error| {
                    OrchError::Internal(format!(
                        "scan configured VM cgroup parent {}: {error}",
                        parent.display()
                    ))
                })? {
                    let entry = entry.map_err(|error| {
                        OrchError::Internal(format!(
                            "scan configured VM cgroup parent {}: {error}",
                            parent.display()
                        ))
                    })?;
                    if let Some(id) = entry
                        .file_name()
                        .to_str()
                        .and_then(|name| name.strip_prefix("tarit-"))
                        .and_then(|id| Uuid::parse_str(id).ok())
                    {
                        ids.insert(id);
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            let executable =
                ExpectedExecutable::resolve(&self.config.vmm_bin).map_err(|error| {
                    OrchError::Internal(format!(
                        "resolve configured VMM executable {} during warm VMM recovery: {error}",
                        self.config.vmm_bin.display()
                    ))
                })?;
            let mut allowed_uids = HashSet::from([unsafe { libc::geteuid() }]);
            if let Some(jails) = &self.jails {
                allowed_uids.extend(jails.uids()?);
            }
            let processes = proc_pid_entries(Path::new("/proc")).map_err(|error| {
                OrchError::Internal(format!("scan /proc for unpersisted warm VMMs: {error}"))
            })?;
            for (pid, proc_dir) in processes {
                let cmdline =
                    match verified_process_cmdline(pid, &proc_dir, &executable, &allowed_uids) {
                        Ok(cmdline) => cmdline,
                        Err(error) => {
                            tracing::debug!(
                                pid,
                                reason = %error,
                                "skip unverifiable proc entry during warm VMM discovery"
                            );
                            continue;
                        }
                    };
                for arg in cmdline.split(|byte| *byte == 0) {
                    let path = Path::new(std::ffi::OsStr::from_bytes(arg));
                    if path.parent() != Some(self.config.socket_dir.as_path()) {
                        continue;
                    }
                    if let Some(id) = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(|name| name.strip_suffix(".sock"))
                        .and_then(|id| Uuid::parse_str(id).ok())
                    {
                        ids.insert(id);
                    }
                }
            }
        }
        Ok(ids)
    }

    #[cfg(target_os = "linux")]
    fn owned_processes_for(&self, id: Uuid) -> Result<Vec<ManagedProcess>, OrchError> {
        let socket_path = self.socket_path_for(id);
        let legacy_socket_path = self.config.socket_dir.join(format!("{id}.sock"));
        let jail_root = self.jails.as_ref().map(|jails| jails.root_for(id));
        let jail_uid = self.jail_identity(id)?.map(|(uid, _)| uid);
        let mut allowed_uids = HashSet::from([unsafe { libc::geteuid() }]);
        allowed_uids.extend(jail_uid);
        let executable = ExpectedExecutable::resolve(&self.config.vmm_bin).map_err(|error| {
            OrchError::Internal(format!(
                "resolve configured VMM executable {} during VMM recovery: {error}",
                self.config.vmm_bin.display()
            ))
        })?;
        let mut cgroup_pids = HashSet::new();
        if let Some(cgroup) = self.exact_vm_cgroup_path(id) {
            let procs = cgroup.join("cgroup.procs");
            match std::fs::read_to_string(&procs) {
                Ok(contents) => {
                    cgroup_pids = parse_cgroup_processes(&contents, &procs).map_err(|error| {
                        OrchError::Internal(format!(
                            "parse owned VM cgroup processes {}: {error}",
                            procs.display()
                        ))
                    })?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(OrchError::Internal(format!(
                        "read owned VM cgroup processes {}: {error}",
                        procs.display()
                    )))
                }
            }
        }

        let mut candidate_pids = cgroup_pids.clone();
        let processes = proc_pid_entries(Path::new("/proc"))
            .map_err(|error| OrchError::Internal(format!("scan /proc for owned VMMs: {error}")))?;
        for (pid, proc_dir) in processes {
            let cmdline = match verified_process_cmdline(pid, &proc_dir, &executable, &allowed_uids)
            {
                Ok(cmdline) => cmdline,
                Err(error) => {
                    tracing::debug!(
                        pid,
                        reason = %error,
                        vm = %id,
                        "skip unverifiable proc entry during owned VMM discovery"
                    );
                    continue;
                }
            };
            let args = cmdline.split(|byte| *byte == 0).collect::<Vec<_>>();
            let current_socket_match = args
                .iter()
                .any(|arg| *arg == socket_path.as_os_str().as_bytes());
            let legacy_socket_match = args
                .iter()
                .any(|arg| *arg == legacy_socket_path.as_os_str().as_bytes());
            let jail_match = jail_root.as_ref().is_some_and(|root| {
                args.iter().any(|arg| *arg == JAIL_SOCKET_PATH.as_bytes())
                    && args.iter().any(|arg| *arg == root.as_os_str().as_bytes())
            });
            if current_socket_match || legacy_socket_match || jail_match {
                candidate_pids.insert(pid);
            }
        }
        let mut processes = Vec::new();
        for pid in candidate_pids {
            let pidfd = match pidfd_open(pid) {
                Ok(pidfd) => pidfd,
                Err(error) if is_process_gone(&error) => continue,
                Err(error) => {
                    return Err(OrchError::Internal(format!(
                        "pin owned VMM {pid} for startup reconciliation: {error}"
                    )))
                }
            };
            let verified = verify_owned_vmm(
                pid,
                &socket_path,
                jail_root.as_deref(),
                &self.config.vmm_bin,
                jail_uid,
            )
            .or_else(|current_reason| {
                if legacy_socket_path == socket_path {
                    return Err(current_reason);
                }
                match current_reason {
                    ProcessVerificationError::Rejected(_) => verify_owned_vmm(
                        pid,
                        &legacy_socket_path,
                        None,
                        &self.config.vmm_bin,
                        None,
                    )
                    .map_err(|legacy_reason| {
                        if legacy_reason.is_gone() {
                            legacy_reason
                        } else {
                            ProcessVerificationError::Rejected(format!(
                                "current-layout verification failed: {current_reason}; legacy-layout verification failed: {legacy_reason}"
                            ))
                        }
                    }),
                    other => Err(other),
                }
            });
            match verified {
                Ok(()) => processes.push(ManagedProcess::adopted(pid, pidfd)),
                Err(error) if error.is_gone() => continue,
                Err(reason) => {
                    return Err(OrchError::Internal(format!(
                    "owned VMM candidate {pid} for VM {id} failed ownership verification: {reason}"
                )))
                }
            }
        }
        Ok(processes)
    }

    #[cfg(not(target_os = "linux"))]
    fn owned_processes_for(&self, _id: Uuid) -> Result<Vec<ManagedProcess>, OrchError> {
        Ok(Vec::new())
    }

    fn terminate_owned_processes(
        &self,
        id: Uuid,
        keep_pid: Option<u32>,
    ) -> Result<usize, OrchError> {
        let processes = self.owned_processes_for(id)?;
        let mut terminated = 0;
        for process in processes {
            if keep_pid == Some(process.pid) {
                continue;
            }
            #[cfg(target_os = "linux")]
            if self
                .config
                .vm_jail
                .as_ref()
                .is_some_and(|jail| jail.pid_namespace)
            {
                if let Some(launcher_pid) = keep_pid {
                    match process_parent_pid(process.pid) {
                        Ok(parent_pid) if parent_pid == launcher_pid => continue,
                        Ok(_) => {}
                        Err(error) if is_process_gone(&error) => continue,
                        Err(error) => {
                            return Err(OrchError::Internal(format!(
                                "inspect owned VMM {} parent process: {error}",
                                process.pid
                            )));
                        }
                    }
                }
            }
            process.kill_wait()?;
            terminated += 1;
        }
        Ok(terminated)
    }

    fn cleanup_uncommitted_runtime(&self, id: Uuid) -> Result<usize, OrchError> {
        self.protect_artifact_owner(id)?;
        let terminated = self.terminate_owned_processes(id, None)?;
        let remaining = self.owned_processes_for(id)?;
        if !remaining.is_empty() {
            return Err(OrchError::Internal(format!(
                "owned VMM processes for {id} remain alive after startup termination"
            )));
        }

        let mut failures = Vec::new();
        if let Err(error) = remove_file_if_present(&self.socket_path_for(id)) {
            failures.push(format!("remove VMM socket: {error}"));
        }
        let legacy_socket = self.config.socket_dir.join(format!("{id}.sock"));
        if legacy_socket != self.socket_path_for(id) {
            if let Err(error) = remove_file_if_present(&legacy_socket) {
                failures.push(format!("remove legacy VMM socket: {error}"));
            }
        }
        let overlay_path = PathBuf::from(self.overlay_path_for(id));
        if self.jails.is_none() {
            if let Err(error) = remove_file_if_present(&overlay_path) {
                failures.push(format!("remove VMM overlay: {error}"));
            }
        }
        let legacy_overlay = self
            .config
            .socket_dir
            .join("overlays")
            .join(format!("{id}.cow"));
        if legacy_overlay != overlay_path {
            if let Err(error) = remove_file_if_present(&legacy_overlay) {
                failures.push(format!("remove legacy VMM overlay: {error}"));
            }
        }
        if let Some(path) = self.exact_vm_cgroup_path(id) {
            if let Err(error) = remove_cgroup_dir_after_exit(&path) {
                failures.push(format!(
                    "remove exact VM cgroup {} after confirmed death: {error}",
                    path.display()
                ));
            }
        }
        if let Err(error) = self.release_jail(id) {
            failures.push(format!(
                "release VM jail identity after confirmed death: {error}"
            ));
        }
        if let Some(net) = &self.net {
            if let Err(error) = net.teardown_vm_id(id) {
                failures.push(format!("teardown recovered network allocation: {error}"));
            }
        }
        self.scheduler.release(id);
        if failures.is_empty() {
            self.release_artifact_owner(id)?;
            Ok(terminated)
        } else {
            Err(OrchError::Internal(failures.join("; ")))
        }
    }

    pub(crate) fn reconcile_legacy_creating_runtime(&self, id: Uuid) -> Result<usize, OrchError> {
        self.cleanup_uncommitted_runtime(id)
    }

    fn reconcile_unpersisted_owned_runtimes(
        &self,
        durable_active_ids: &HashSet<Uuid>,
    ) -> Result<(), OrchError> {
        for id in self
            .owned_runtime_ids()?
            .difference(durable_active_ids)
            .copied()
            .collect::<Vec<_>>()
        {
            let terminated = self.cleanup_uncommitted_runtime(id)?;
            tracing::warn!(
                vm = %id,
                terminated,
                "removed unpersisted owned VMM runtime before artifact GC"
            );
        }
        Ok(())
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
    ) -> Result<Vec<ReadoptWarning>, OrchError> {
        let durable_active_ids = records
            .iter()
            .filter(|record| {
                record.host_id == self.config.host_id
                    && matches!(
                        record.status,
                        VmStatus::Creating
                            | VmStatus::Running
                            | VmStatus::Paused
                            | VmStatus::Suspended
                    )
            })
            .map(|record| record.id)
            .collect::<HashSet<_>>();
        let mut failed = Vec::new();
        for record in records.iter_mut() {
            match self.readopt_one(record).await {
                Ok(true) => {
                    tracing::info!(vm = %record.id, pid = record.pid.unwrap_or(0),
                        "re-adopted running VM after restart");
                }
                Ok(false) => {}
                Err(ReadoptFailure::Unadoptable(reason)) => {
                    tracing::warn!(vm = %record.id, reason = %reason,
                        "cannot re-adopt VM after restart; owned runtime was contained and the record will be marked failed");
                    failed.push(ReadoptWarning {
                        id: record.id,
                        reason,
                    });
                }
                Err(ReadoptFailure::Fatal(reason)) => {
                    return Err(OrchError::Internal(format!(
                        "startup reconciliation failed to contain owned VMM {}: {reason}",
                        record.id
                    )));
                }
            }
        }
        self.reconcile_unpersisted_owned_runtimes(&durable_active_ids)?;
        Ok(failed)
    }

    /// Attempt to re-adopt a single persisted VM. Returns `Ok(true)` on success,
    /// `Ok(false)` when the record is not a locally running VM (nothing to do),
    /// and `Err` when the VM existed here but can no longer be controlled. When a
    /// VMM is positively identified as ours but cannot be managed, it is
    /// terminated through its pinned pidfd so no unmanaged VMM is left running.
    async fn readopt_one(self: &Arc<Self>, record: &mut VmRecord) -> Result<bool, ReadoptFailure> {
        if record.host_id != self.config.host_id {
            return Ok(false);
        }
        if matches!(
            record.status,
            VmStatus::Creating | VmStatus::Running | VmStatus::Paused | VmStatus::Suspended
        ) {
            self.protect_artifact_owner(record.id)
                .map_err(|error| ReadoptFailure::Fatal(error.to_string()))?;
        }
        if matches!(
            record.status,
            VmStatus::Creating | VmStatus::Running | VmStatus::Paused | VmStatus::Suspended
        ) {
            let persisted = record.runtime_layout.as_ref().ok_or_else(|| {
                ReadoptFailure::Fatal(
                    "persisted active VM has no runtime layout after legacy migration; drain required before recovery and artifact GC"
                        .into(),
                )
            })?;
            let expected = self.expected_runtime_layout(record);
            if persisted != &expected {
                self.contain_layout_conflict(record, persisted)
                    .map_err(ReadoptFailure::Fatal)?;
                return Err(ReadoptFailure::Unadoptable(format!(
                    "persisted runtime layout conflicts with current configuration (persisted={persisted:?}, expected={expected:?}); identified VMM was terminated before artifact GC"
                )));
            }
        }
        if record.status == VmStatus::Creating {
            let terminated = self
                .cleanup_uncommitted_runtime(record.id)
                .map_err(|error| {
                    ReadoptFailure::Fatal(format!(
                        "contain interrupted Creating runtime before identity release: {error}"
                    ))
                })?;
            return Err(ReadoptFailure::Unadoptable(format!(
                "creating lifecycle was interrupted before publication; terminated {terminated} owned VMM process(es)"
            )));
        }
        if !matches!(
            record.status,
            VmStatus::Running | VmStatus::Paused | VmStatus::Suspended
        ) {
            return Ok(false);
        }
        let pid = match record.pid {
            Some(pid) => pid,
            None => {
                self.cleanup_uncommitted_runtime(record.id)
                    .map_err(|error| {
                        ReadoptFailure::Fatal(format!(
                            "persisted VM has no PID and owned runtime containment failed: {error}"
                        ))
                    })?;
                return Err(ReadoptFailure::Unadoptable(
                    "persisted VM has no PID".into(),
                ));
            }
        };
        if !valid_process_id(pid) {
            self.cleanup_uncommitted_runtime(record.id)
                .map_err(|error| {
                    ReadoptFailure::Fatal(format!(
                        "persisted VM has invalid PID {pid} and owned runtime containment failed: {error}"
                    ))
                })?;
            return Err(ReadoptFailure::Unadoptable(format!(
                "persisted VM has invalid positive PID {pid}"
            )));
        }
        let socket_path = match record.socket_path.as_deref() {
            Some(path) => PathBuf::from(path),
            None => {
                self.cleanup_uncommitted_runtime(record.id)
                    .map_err(|error| {
                        ReadoptFailure::Fatal(format!(
                            "persisted VM has no control socket and owned runtime containment failed: {error}"
                        ))
                    })?;
                return Err(ReadoptFailure::Unadoptable(
                    "persisted VM has no control socket path".into(),
                ));
            }
        };
        // Pin the process before any /proc inspection so the PID cannot be
        // recycled between verification and adoption. If the process is already
        // gone there is nothing to adopt.
        let pidfd = match pidfd_open(pid) {
            Ok(pidfd) => pidfd,
            Err(error) if is_process_gone(&error) => {
                self.cleanup_uncommitted_runtime(record.id)
                    .map_err(|cleanup| {
                        ReadoptFailure::Fatal(format!(
                            "persisted VMM {pid} is gone but owned runtime containment failed: {cleanup}"
                        ))
                    })?;
                return Err(ReadoptFailure::Unadoptable(format!(
                    "pin VMM {pid} for adoption: {error}"
                )));
            }
            Err(error) => {
                return Err(ReadoptFailure::Fatal(format!(
                    "cannot pin possibly-live VMM {pid}; preserving its identity lease: {error}"
                )))
            }
        };
        // Confirm identity while pinned. A failure here means the process is not
        // our VMM (or is already gone), so it must not be signalled.
        let jail_root = record
            .runtime_layout
            .as_ref()
            .and_then(|layout| layout.jail_path.as_deref())
            .map(Path::new);
        let jail_uid = self
            .jail_identity(record.id)
            .map_err(|error| ReadoptFailure::Fatal(error.to_string()))?
            .map(|(uid, _)| uid);
        if let Err(reason) =
            verify_owned_vmm(pid, &socket_path, jail_root, &self.config.vmm_bin, jail_uid)
        {
            self.cleanup_uncommitted_runtime(record.id)
                .map_err(|cleanup| {
                    ReadoptFailure::Fatal(format!(
                        "{reason}; containment of any separately owned runtime failed: {cleanup}"
                    ))
                })?;
            return Err(ReadoptFailure::Unadoptable(reason.to_string()));
        }
        self.terminate_owned_processes(record.id, Some(pid))
            .map_err(|error| {
                ReadoptFailure::Fatal(format!(
                    "terminate duplicate owned VMM processes while preserving PID {pid}: {error}"
                ))
            })?;
        let process = ManagedProcess::adopted(pid, pidfd);
        // Identity is confirmed. Any failure below leaves a live, taritd-owned
        // VMM that the control plane cannot manage, so terminate it through the
        // pinned pidfd before marking the VM terminal.
        let recovered: Result<Option<NetAlloc>, String> = 'recover: {
            if let Err(error) = self.reconcile_readopted_cgroup(record, pid) {
                break 'recover Err(error.to_string());
            }
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
                return Err(self.fence_readopt_failure(record.id, &vm, reason));
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

    fn fence_readopt_failure(&self, id: Uuid, vm: &RunningVm, reason: String) -> ReadoptFailure {
        match self.teardown_vm(id, vm) {
            Ok(()) => ReadoptFailure::Unadoptable(reason),
            Err(error) => ReadoptFailure::Fatal(format!(
                "{reason}; clean up identified VMM {id} after adoption failed: {error}"
            )),
        }
    }

    fn contain_layout_conflict(
        &self,
        record: &VmRecord,
        persisted: &VmRuntimeLayout,
    ) -> Result<(), String> {
        let pid = record.pid.ok_or_else(|| {
            "runtime layout changed but the persisted active VM has no PID; startup is blocked before GC because safe containment cannot be proven".to_string()
        })?;
        let socket_path = record.socket_path.as_deref().map(Path::new).ok_or_else(|| {
            "runtime layout changed but the persisted active VM has no control socket; startup is blocked before GC because safe containment cannot be proven".to_string()
        })?;
        let jail_uid = match persisted.jail_path.as_deref().map(Path::new) {
            Some(root) => {
                let marker = read_jail_marker(root).map_err(|error| {
                    format!(
                        "read persisted jail ownership for layout-conflicting VMM {pid}: {error}"
                    )
                })?;
                if marker.vm_id != record.id {
                    return Err(format!(
                        "persisted jail {} belongs to {}, not {}",
                        root.display(),
                        marker.vm_id,
                        record.id
                    ));
                }
                Some(marker.uid)
            }
            None => None,
        };
        match pidfd_open(pid) {
            Ok(pidfd) => {
                verify_owned_vmm(
                    pid,
                    socket_path,
                    persisted.jail_path.as_deref().map(Path::new),
                    &self.config.vmm_bin,
                    jail_uid,
                )
                .map_err(|error| {
                    format!(
                        "runtime layout changed and PID {pid} could not be identified safely; startup is blocked before GC: {error}"
                    )
                })?;
                graceful_stop_vmm(socket_path);
                ManagedProcess::adopted(pid, pidfd)
                    .kill_wait()
                    .map_err(|error| {
                        format!("terminate layout-conflicting VMM {pid} before GC: {error}")
                    })?;
            }
            Err(error) if is_process_gone(&error) => {}
            Err(error) => {
                return Err(format!(
                    "pin layout-conflicting VMM {pid} before GC: {error}"
                ))
            }
        }
        if let Some(net) = &self.net {
            net.teardown_vm_id(record.id).map_err(|error| {
                format!(
                    "terminate layout-conflicting VMM {pid}, but failed to tear down its recovered network before GC: {error}"
                )
            })?;
        }
        self.release_artifact_owner(record.id)
            .map_err(|error| format!("release contained runtime ownership: {error}"))?;
        Ok(())
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
                    .and_then(|index| warm.remove(index).map(|vm| (index, vm)))
            };
            if let Some((index, warm)) = warm {
                let client = VmmClient::new(&warm.vm.socket_path);
                let _ = client.stop();
                if let Err(error) = self.teardown_vm(id, &warm.vm) {
                    let mut retained = self.warm.lock().map_err(|_| {
                        OrchError::Internal(format!(
                            "warm VM {id} teardown failed ({error}) and supervisor could not retain it for retry"
                        ))
                    })?;
                    let index = index.min(retained.len());
                    retained.insert(index, warm);
                    return Err(error);
                }
                return Ok(());
            }
            if let Some(net) = &self.net {
                net.teardown_vm_id(id)?;
            }
            self.release_jail(id)?;
            self.release_artifact_owner(id)?;
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
                match client.exec("", exec_timeout_ms) {
                    Ok((0, _, _, _)) => Ok(true),
                    Ok((code, _, _, _)) => {
                        Err(format!("resume agent probe exited with status {code}"))
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
        let client = self.client_for(id)?.with_request_timeout(STATUS_OP_TIMEOUT);
        client.status().map_err(|e| OrchError::Vmm(e.to_string()))
    }

    pub fn set_balloon_vm(
        &self,
        id: Uuid,
        target_mib: u64,
    ) -> Result<(u64, u64, u32, u32), OrchError> {
        self.client_for(id)?
            .set_balloon(target_mib)
            .map_err(|error| OrchError::Vmm(format!("set balloon: {error}")))
    }

    pub fn balloon_vm(&self, id: Uuid) -> Result<(u64, u64, u32, u32), OrchError> {
        self.client_for(id)?
            .balloon()
            .map_err(|error| OrchError::Vmm(format!("get balloon: {error}")))
    }

    pub fn exec_vm(
        &self,
        id: Uuid,
        command: &str,
        timeout_ms: u64,
    ) -> Result<(i32, String, String, u64), OrchError> {
        // The guest is allowed to run for the full `timeout_ms`; the transport
        // deadline must outlive it or long commands fail with a spurious
        // socket timeout (EAGAIN) while the guest is still working.
        let client = self
            .client_for(id)?
            .with_request_timeout(Duration::from_millis(timeout_ms) + EXEC_OP_MARGIN);
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
        overlay_path: Option<PathBuf>,
        memory_mib: u64,
    ) -> Result<SnapshotBundle, OrchError> {
        let has_overlay = overlay_path.is_some();
        let _reservation = self.reserve_snapshot_space(id, memory_mib, has_overlay)?;
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
        let scratch_snapshot_host_path = self.host_path_for_vmm(id, &scratch_snapshot_path);
        // Keep the VMM's cleanup token armed until every member of the bundle
        // has been captured. A failed disk copy therefore cannot publish a
        // RAM-only snapshot.
        let scratch_ram_artifact = match OwnedArtifact::capture(&scratch_snapshot_host_path) {
            Ok(artifact) => artifact,
            Err(error) => {
                return Err(compensate_snapshot_pause(
                    &client,
                    resume_after,
                    OrchError::Internal(format!("capture RAM snapshot ownership: {error}")),
                ));
            }
        };
        let expected_uid = self
            .jail_identity(id)?
            .map(|(uid, _)| uid)
            .unwrap_or_else(|| unsafe { libc::geteuid() });
        let overlay_artifact = if has_overlay {
            let source = overlay_path.expect("has_overlay is derived from overlay_path");
            let staging = self.snapshot_overlay_staging_path();
            let destination = self.snapshot_overlay_path();
            match copy_private_artifact_owned(&source, &staging, expected_uid) {
                Ok(artifact) => Some((artifact, destination)),
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
                if let Some((artifact, _)) = overlay_artifact {
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

        let ram_staging = self.snapshot_ram_staging_path();
        let ram_destination = self.snapshot_ram_path();
        let ram_artifact = match copy_private_artifact_owned(
            &scratch_snapshot_host_path,
            &ram_staging,
            expected_uid,
        ) {
            Ok(artifact) => artifact,
            Err(error) => {
                if let Some((artifact, _)) = overlay_artifact {
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
            if let Some((artifact, _)) = overlay_artifact {
                let _ = artifact.remove();
            }
            return Err(OrchError::Vmm(format!("claim RAM snapshot: {error}")));
        }
        match scratch_ram_artifact.remove() {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                path = %scratch_snapshot_host_path.display(),
                "released VMM RAM scratch path no longer names the captured inode"
            ),
            Err(error) => tracing::warn!(
                path = %scratch_snapshot_host_path.display(),
                "released VMM RAM scratch cleanup deferred to VMM GC: {error}"
            ),
        }

        let overlay_path = overlay_artifact
            .as_ref()
            .map(|(_, destination)| destination.display().to_string());
        let mut publications = vec![(ram_artifact, ram_destination.clone())];
        publications.extend(overlay_artifact);
        let registered_paths = match self.publish_artifacts(&mut publications) {
            Ok(paths) => paths,
            Err(error) => {
                for (artifact, _) in publications {
                    let _ = artifact.remove();
                }
                return Err(error);
            }
        };
        let bundle = SnapshotBundle {
            snapshot_path: ram_destination.display().to_string(),
            overlay_path,
            live_stats: None,
            artifacts: publications
                .into_iter()
                .map(|(artifact, _)| artifact)
                .collect(),
            precomputed_integrity: None,
            in_progress_artifacts: Arc::clone(&self.in_progress_artifacts),
            registered_paths,
        };
        Ok(bundle)
    }

    /// Capture RAM, device state, and the optional writable disk upper from one
    /// atomic live-snapshot boundary, then move the immutable pair into the
    /// orchestrator's durable namespace after the source VM has resumed.
    pub(crate) fn live_snapshot_bundle_vm(
        &self,
        id: Uuid,
        memory_mib: u64,
        expects_overlay: bool,
    ) -> Result<SnapshotBundle, OrchError> {
        let _reservation = self.reserve_snapshot_space(id, memory_mib, expects_overlay)?;
        let client = self.lifecycle_client_for(id)?;
        let (
            scratch_ram_vmm_path,
            scratch_overlay_vmm_path,
            scratch_integrity_vmm_path,
            live_stats,
        ) = client
            .live_snapshot_unreleased()
            .map_err(|error| OrchError::Vmm(format!("atomic live snapshot: {error}")))?;
        if scratch_overlay_vmm_path.is_some() != expects_overlay {
            return Err(OrchError::Vmm(format!(
                "atomic live snapshot disk mismatch: expected overlay={expects_overlay}, VMM returned overlay={}",
                scratch_overlay_vmm_path.is_some()
            )));
        }

        let scratch_ram_host_path = self.host_path_for_vmm(id, &scratch_ram_vmm_path);
        let scratch_ram = OwnedArtifact::capture(&scratch_ram_host_path).map_err(|error| {
            OrchError::Internal(format!("capture live RAM scratch ownership: {error}"))
        })?;
        let scratch_overlay = match scratch_overlay_vmm_path.as_deref() {
            Some(path) => {
                let host_path = self.host_path_for_vmm(id, path);
                Some((
                    path.to_string(),
                    host_path.clone(),
                    OwnedArtifact::capture(&host_path).map_err(|error| {
                        OrchError::Internal(format!("capture live disk scratch ownership: {error}"))
                    })?,
                ))
            }
            None => None,
        };
        let expected_uid = self
            .jail_identity(id)?
            .map(|(uid, _)| uid)
            .unwrap_or_else(|| unsafe { libc::geteuid() });
        let scratch_integrity_host_path = self.host_path_for_vmm(id, &scratch_integrity_vmm_path);
        let scratch_integrity = capture_private_artifact_owned(
            &scratch_integrity_host_path,
            expected_uid,
        )
        .map_err(|error| {
            OrchError::Internal(format!("capture live integrity scratch ownership: {error}"))
        })?;

        let ram_staging = self.snapshot_ram_staging_path();
        let ram_destination = self.snapshot_ram_path();
        let ram_artifact =
            copy_private_artifact_owned(&scratch_ram_host_path, &ram_staging, expected_uid)
                .map_err(|error| {
                    OrchError::Internal(format!("claim durable live RAM snapshot: {error}"))
                })?;
        let overlay_artifact = match scratch_overlay.as_ref() {
            Some((_, host_path, _)) => {
                let staging = self.snapshot_overlay_staging_path();
                let destination = self.snapshot_overlay_path();
                match copy_private_artifact_owned(host_path, &staging, expected_uid) {
                    Ok(artifact) => Some((artifact, destination)),
                    Err(error) => {
                        let _ = ram_artifact.remove();
                        return Err(OrchError::Internal(format!(
                            "claim durable live disk snapshot: {error}"
                        )));
                    }
                }
            }
            None => None,
        };

        let cleanup_durable = |ram: OwnedArtifact, overlay: Option<(OwnedArtifact, PathBuf)>| {
            let _ = ram.remove();
            if let Some((overlay, _)) = overlay {
                let _ = overlay.remove();
            }
        };
        if let Err(error) = client.release_scratch(&scratch_ram_vmm_path, scratch_ram.identity()) {
            cleanup_durable(ram_artifact, overlay_artifact);
            return Err(OrchError::Vmm(format!("claim live RAM scratch: {error}")));
        }
        if let Err(error) = scratch_ram.remove() {
            tracing::warn!(
                path = %scratch_ram_host_path.display(),
                "released live RAM scratch cleanup deferred to VMM GC: {error}"
            );
        }
        if let Some((vmm_path, host_path, scratch)) = scratch_overlay {
            if let Err(error) = client.release_scratch(&vmm_path, scratch.identity()) {
                cleanup_durable(ram_artifact, overlay_artifact);
                return Err(OrchError::Vmm(format!("claim live disk scratch: {error}")));
            }
            if let Err(error) = scratch.remove() {
                tracing::warn!(
                    path = %host_path.display(),
                    "released live disk scratch cleanup deferred to VMM GC: {error}"
                );
            }
        }
        if let Err(error) =
            client.release_scratch(&scratch_integrity_vmm_path, scratch_integrity.identity())
        {
            cleanup_durable(ram_artifact, overlay_artifact);
            return Err(OrchError::Vmm(format!(
                "claim live integrity scratch: {error}"
            )));
        }

        let overlay_path = overlay_artifact
            .as_ref()
            .map(|(_, destination)| destination.display().to_string());
        let mut publications = vec![(ram_artifact, ram_destination.clone())];
        publications.extend(overlay_artifact);
        let registered_paths = match self.publish_artifacts(&mut publications) {
            Ok(paths) => paths,
            Err(error) => {
                for (artifact, _) in publications {
                    let _ = artifact.remove();
                }
                let _ = scratch_integrity.remove();
                return Err(error);
            }
        };
        Ok(SnapshotBundle {
            snapshot_path: ram_destination.display().to_string(),
            overlay_path,
            live_stats: Some(live_stats),
            artifacts: publications
                .into_iter()
                .map(|(artifact, _)| artifact)
                .collect(),
            precomputed_integrity: Some(scratch_integrity),
            in_progress_artifacts: Arc::clone(&self.in_progress_artifacts),
            registered_paths,
        })
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
        // A process stuck in uninterruptible host I/O can remain alive after
        // SIGKILL. Keep every runtime artifact intact in that case: unlinking
        // its socket or overlay, or releasing its jail, cgroup, or network,
        // would make a still-running VM unmanageable and could allow those
        // resources to be reused. The caller retains the RunningVm and retries
        // reconciliation later.
        vm.process.kill_wait().map_err(|error| {
            OrchError::Internal(format!(
                "terminate VMM before releasing runtime resources: {error}"
            ))
        })?;
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
        // The exact child is empty now that the process is confirmed dead.
        // Removing only this UUID-derived child preserves the operator-owned
        // parent cgroup.
        if let Some(path) = self.exact_vm_cgroup_path(id) {
            if let Err(error) = remove_cgroup_dir_after_exit(&path) {
                failures.push(format!(
                    "remove exact VM cgroup {}: {error}",
                    path.display()
                ));
            }
        }
        if let Err(error) = self.release_jail(id) {
            failures.push(format!("remove VM jail: {error}"));
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
            self.release_artifact_owner(id)?;
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

fn jail_guest_path(jail_root: &Path, host_path: &Path) -> Result<String, OrchError> {
    let relative = host_path.strip_prefix(jail_root).map_err(|_| {
        OrchError::Internal(format!(
            "jail asset {} escaped VM jail root {}",
            host_path.display(),
            jail_root.display()
        ))
    })?;
    Ok(Path::new("/").join(relative).to_string_lossy().into_owned())
}

fn prepare_jail_layout(lease: &JailLease) -> Result<(), OrchError> {
    for (relative, mode) in [
        ("run", 0o700),
        ("assets", 0o700),
        ("tmp", 0o700),
        ("dev", 0o500),
        ("dev/net", 0o500),
    ] {
        let path = lease.root.join(relative);
        std::fs::create_dir(&path).map_err(|error| {
            OrchError::Internal(format!(
                "create VM jail directory {}: {error}",
                path.display()
            ))
        })?;
        set_jail_owner_mode(&path, lease.uid, lease.gid, mode)?;
    }
    // KVM is deliberately not present in the jail. The privileged launcher
    // passes a verified, already-open descriptor to the confined VMM instead.
    stage_jail_device(
        Path::new("/dev/net/tun"),
        &lease.root.join("dev/net/tun"),
        lease,
    )?;
    Ok(())
}

fn copy_jail_asset(
    source_path: &Path,
    destination_path: &Path,
    uid: u32,
    gid: u32,
) -> Result<(), OrchError> {
    let mut source_options = OpenOptions::new();
    source_options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let source = source_options.open(source_path).map_err(|error| {
        OrchError::Internal(format!(
            "open jail asset source {}: {error}",
            source_path.display()
        ))
    })?;
    let metadata = source.metadata().map_err(|error| {
        OrchError::Internal(format!(
            "inspect jail asset source {}: {error}",
            source_path.display()
        ))
    })?;
    if !metadata.file_type().is_file() {
        return Err(OrchError::BadRequest(format!(
            "jail asset {} must be a regular file",
            source_path.display()
        )));
    }
    let destination = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(destination_path)
        .map_err(|error| {
            OrchError::Internal(format!(
                "create jail asset {}: {error}",
                destination_path.display()
            ))
        })?;
    let copied = copy_artifact_data(&source, &destination, metadata.len())
        .and_then(|_| destination.sync_all());
    if let Err(error) = copied {
        let _ = std::fs::remove_file(destination_path);
        return Err(OrchError::Internal(format!(
            "copy jail asset {} to {}: {error}",
            source_path.display(),
            destination_path.display()
        )));
    }
    set_jail_owner_mode(destination_path, uid, gid, 0o400)?;
    Ok(())
}

fn set_jail_owner_mode(path: &Path, uid: u32, gid: u32, mode: u32) -> Result<(), OrchError> {
    let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| OrchError::Internal(format!("jail path contains NUL: {}", path.display())))?;
    if unsafe { libc::chown(path_c.as_ptr(), uid, gid) } != 0 {
        return Err(OrchError::Internal(format!(
            "chown jail path {} to {uid}:{gid}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|error| {
        OrchError::Internal(format!("chmod jail path {}: {error}", path.display()))
    })
}

#[cfg(target_os = "linux")]
fn stage_jail_device(
    source: &Path,
    destination: &Path,
    lease: &JailLease,
) -> Result<(), OrchError> {
    let metadata = std::fs::metadata(source).map_err(|error| {
        OrchError::Internal(format!(
            "inspect required jail device {}: {error}",
            source.display()
        ))
    })?;
    let file_type = if metadata.file_type().is_char_device() {
        libc::S_IFCHR
    } else if metadata.file_type().is_block_device() {
        libc::S_IFBLK
    } else {
        return Err(OrchError::Internal(format!(
            "required jail device {} is not a device node",
            source.display()
        )));
    };
    let destination_c =
        std::ffi::CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
            OrchError::Internal(format!(
                "jail device path contains NUL: {}",
                destination.display()
            ))
        })?;
    if unsafe {
        libc::mknod(
            destination_c.as_ptr(),
            file_type | 0o600,
            metadata.rdev() as libc::dev_t,
        )
    } != 0
    {
        return Err(OrchError::Internal(format!(
            "create jail device {}: {}",
            destination.display(),
            std::io::Error::last_os_error()
        )));
    }
    set_jail_owner_mode(destination, lease.uid, lease.gid, 0o600)
}

#[cfg(not(target_os = "linux"))]
fn stage_jail_device(
    _source: &Path,
    _destination: &Path,
    _lease: &JailLease,
) -> Result<(), OrchError> {
    Err(OrchError::Internal(
        "VM jail asset staging is supported only on Linux".into(),
    ))
}

/// Copy an orchestrator-owned artifact without following either leaf path.
/// The destination is created exclusively at mode 0600, then both it and its
/// parent directory are synced before the path can be persisted.
fn copy_private_artifact(
    source_path: &Path,
    destination_path: &Path,
) -> std::io::Result<OwnedArtifact> {
    copy_private_artifact_owned(source_path, destination_path, unsafe { libc::geteuid() })
}

fn capture_private_artifact_owned(
    source_path: &Path,
    expected_uid: u32,
) -> std::io::Result<OwnedArtifact> {
    let artifact = OwnedArtifact::capture(source_path)?;
    let metadata = artifact._file.metadata()?;
    if metadata.uid() != expected_uid || metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} must be owned by uid {expected_uid}, mode 0600, with one link",
                source_path.display()
            ),
        ));
    }
    Ok(artifact)
}

fn copy_private_artifact_owned(
    source_path: &Path,
    destination_path: &Path,
    expected_uid: u32,
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
    if before.uid() != expected_uid || before.nlink() != 1 || before.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} must be owned by uid {expected_uid}, mode 0600, with one link",
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
    // `libc::Ioctl` is c_ulong on glibc and c_int on musl, so spell the constant
    // with that alias to keep the musl release build compiling.
    const FICLONE: libc::Ioctl = 0x4004_9409;
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

fn remove_cgroup_dir_after_exit(path: &Path) -> Result<(), std::io::Error> {
    let deadline = Instant::now() + TEARDOWN_STOP_TIMEOUT;
    loop {
        match remove_dir_if_present(path) {
            Ok(()) => return Ok(()),
            Err(error)
                if error.raw_os_error() == Some(libc::EBUSY) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
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
                last = "guest agent readiness probe did not succeed".to_string();
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

#[cfg(any(target_os = "linux", test))]
fn cgroup_device_number(metadata: &std::fs::Metadata) -> Result<String, OrchError> {
    let raw_device = if metadata.file_type().is_block_device() {
        metadata.rdev()
    } else {
        metadata.dev()
    };
    let device = libc::dev_t::try_from(raw_device)
        .map_err(|_| OrchError::Internal("filesystem device number does not fit dev_t".into()))?;
    let device = format!("{}:{}", libc::major(device), libc::minor(device));
    #[cfg(target_os = "linux")]
    {
        resolve_cgroup_block_device(&device, Path::new("/sys/dev/block"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(device)
    }
}

#[cfg(target_os = "linux")]
fn resolve_cgroup_block_device(device: &str, sys_block: &Path) -> Result<String, OrchError> {
    let sys_device = std::fs::canonicalize(sys_block.join(device)).map_err(|error| {
        OrchError::Internal(format!(
            "resolve filesystem block device {device} for cgroup io.max: {error}"
        ))
    })?;
    if !sys_device.join("partition").exists() {
        return Ok(device.to_string());
    }

    let parent = sys_device.parent().ok_or_else(|| {
        OrchError::Internal(format!(
            "partition block device {device} has no parent device in sysfs"
        ))
    })?;
    let parent_device = std::fs::read_to_string(parent.join("dev"))
        .map_err(|error| {
            OrchError::Internal(format!(
                "read parent block device for partition {device}: {error}"
            ))
        })?
        .trim()
        .to_string();
    let valid = parent_device
        .split_once(':')
        .and_then(|(major, minor)| Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?)))
        .is_some();
    if !valid {
        return Err(OrchError::Internal(format!(
            "invalid parent block device {parent_device:?} for partition {device}"
        )));
    }
    Ok(parent_device)
}

#[cfg(any(target_os = "linux", test))]
fn validate_owned_vm_cgroup(parent: &Path, path: &Path, id: Uuid, pid: u32) -> std::io::Result<()> {
    let expected = parent.join(format!("tarit-{id}"));
    if path != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "derived cgroup {} does not equal expected child {}",
                path.display(),
                expected.display()
            ),
        ));
    }
    #[cfg(not(test))]
    {
        if !parent.is_absolute()
            || !parent.starts_with("/sys/fs/cgroup/")
            || parent == Path::new("/sys/fs/cgroup")
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "configured VM cgroup parent {} is not a dedicated cgroup v2 subtree",
                    parent.display()
                ),
            ));
        }
    }
    let parent_metadata = std::fs::symlink_metadata(parent)?;
    let child_metadata = std::fs::symlink_metadata(path)?;
    let owner = unsafe { libc::geteuid() };
    if !parent_metadata.is_dir()
        || parent_metadata.file_type().is_symlink()
        || parent_metadata.uid() != owner
        || !child_metadata.is_dir()
        || child_metadata.file_type().is_symlink()
        || child_metadata.uid() != owner
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "cgroup parent {} and child {} must be real directories owned by uid {owner}",
                parent.display(),
                path.display()
            ),
        ));
    }
    let canonical_parent = std::fs::canonicalize(parent)?;
    let canonical_child = std::fs::canonicalize(path)?;
    if canonical_child.parent() != Some(canonical_parent.as_path())
        || canonical_child.file_name() != expected.file_name()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "cgroup child {} escapes configured parent {}",
                path.display(),
                parent.display()
            ),
        ));
    }
    #[cfg(not(test))]
    {
        if canonical_parent != parent {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "configured cgroup parent {} contains a symlink or non-canonical component",
                    parent.display()
                ),
            ));
        }
        let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "cgroup path contains NUL")
        })?;
        if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let stats = unsafe { stats.assume_init() };
        if stats.f_type as libc::c_long != libc::CGROUP2_SUPER_MAGIC as libc::c_long {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "configured VM cgroup path is not on cgroup v2",
            ));
        }
    }

    let procs = path.join("cgroup.procs");
    validate_owned_cgroup_control(&procs)?;
    if !valid_process_id(pid) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid adopted VMM PID {pid}"),
        ));
    }
    let owns_pid =
        parse_cgroup_processes(&std::fs::read_to_string(&procs)?, &procs)?.contains(&pid);
    if !owns_pid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "adopted VMM PID {pid} is not a member of exact cgroup {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(all(not(target_os = "linux"), not(test)))]
fn validate_owned_vm_cgroup(
    _parent: &Path,
    _path: &Path,
    _id: Uuid,
    _pid: u32,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "cgroup v2 is only available on Linux",
    ))
}

#[cfg(any(target_os = "linux", test))]
fn validate_owned_cgroup_control(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "cgroup control {} must be a real file owned by taritd",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn apply_and_verify_cgroup_limits(cgroup: &Path, plan: &CgroupLimitPlan) -> std::io::Result<()> {
    for (key, expected) in plan.entries() {
        if key == "io.max" {
            continue;
        }
        let path = cgroup.join(key);
        validate_owned_cgroup_control(&path).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("validate cgroup control {}: {error}", path.display()),
            )
        })?;
        write_single_file(&path, &expected).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("write cgroup control {}: {error}", path.display()),
            )
        })?;
        let actual = std::fs::read_to_string(&path).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("read cgroup control {}: {error}", path.display()),
            )
        })?;
        if !cgroup_value_matches(key, &expected, &actual) {
            return Err(std::io::Error::other(format!(
                "verification failed for {key}: expected {expected:?}, read {actual:?}"
            )));
        }
    }
    let io_max = cgroup.join("io.max");
    if plan.io_max.is_some() || io_max.exists() {
        validate_owned_cgroup_control(&io_max).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("validate cgroup control {}: {error}", io_max.display()),
            )
        })?;
        apply_and_verify_io_limits(&io_max, plan.io_max.as_deref().unwrap_or(""))?;
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn apply_and_verify_io_limits(path: &Path, expected: &str) -> std::io::Result<()> {
    let current = std::fs::read_to_string(path)?;
    let current_limits = parse_io_limits(&current);
    let expected_limits = parse_io_limits(expected);
    for (device, limits) in &current_limits {
        let resets = limits
            .keys()
            .filter(|key| {
                expected_limits
                    .get(device)
                    .and_then(|expected| expected.get(*key))
                    .is_none()
            })
            .map(|key| format!("{key}=max"))
            .collect::<Vec<_>>();
        if !resets.is_empty() {
            write_single_file(path, &format!("{device} {}", resets.join(" ")))?;
        }
    }
    for command in expected
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        write_single_file(path, command)?;
    }

    let actual = std::fs::read_to_string(path)?;
    if !io_limits_match_exact(expected, &actual) {
        return Err(std::io::Error::other(format!(
            "verification failed for io.max: expected {expected:?}, read {actual:?}"
        )));
    }
    Ok(())
}

#[cfg(all(not(target_os = "linux"), not(test)))]
fn apply_and_verify_cgroup_limits(_cgroup: &Path, _plan: &CgroupLimitPlan) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "cgroup v2 is only available on Linux",
    ))
}

#[cfg(any(target_os = "linux", test))]
fn cgroup_value_matches(key: &str, expected: &str, actual: &str) -> bool {
    if key == "io.max" {
        return io_limits_match_exact(expected, actual);
    }
    if key == "io.weight" {
        return actual
            .split_whitespace()
            .next_back()
            .is_some_and(|value| value == expected.trim());
    }
    expected.split_whitespace().eq(actual.split_whitespace())
}

#[cfg(any(target_os = "linux", test))]
fn io_limits_match_exact(expected: &str, actual: &str) -> bool {
    let expected = parse_io_limits(expected);
    let actual = parse_io_limits(actual);
    let expected_present = expected.iter().all(|(device, limits)| {
        actual.get(device).is_some_and(|actual_limits| {
            limits
                .iter()
                .all(|(limit, value)| actual_limits.get(limit) == Some(value))
        })
    });
    expected_present
        && actual.iter().all(|(device, limits)| {
            limits.iter().all(|(limit, value)| {
                value == "max"
                    || expected
                        .get(device)
                        .and_then(|expected_limits| expected_limits.get(limit))
                        == Some(value)
            })
        })
}

#[cfg(any(target_os = "linux", test))]
fn parse_io_limits(contents: &str) -> HashMap<String, HashMap<String, String>> {
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let device = fields.next()?.to_string();
            let limits = fields
                .filter_map(|field| {
                    field
                        .split_once('=')
                        .map(|(key, value)| (key.to_string(), value.to_string()))
                })
                .collect::<HashMap<_, _>>();
            Some((device, limits))
        })
        .collect()
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

#[cfg(any(target_os = "linux", test))]
fn write_single_file(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RestoredGuestNetwork {
    guest_ip: Ipv4Addr,
    gateway: Ipv4Addr,
    prefix: u8,
}

impl TryFrom<&NetAlloc> for RestoredGuestNetwork {
    type Error = OrchError;

    fn try_from(allocation: &NetAlloc) -> Result<Self, Self::Error> {
        let guest_ip = allocation.guest_ip.parse::<Ipv4Addr>().map_err(|error| {
            OrchError::Internal(format!(
                "parse restored guest IPv4 address {:?}: {error}",
                allocation.guest_ip
            ))
        })?;
        let gateway = allocation.host_ip.parse::<Ipv4Addr>().map_err(|error| {
            OrchError::Internal(format!(
                "parse restored guest gateway IPv4 address {:?}: {error}",
                allocation.host_ip
            ))
        })?;
        if allocation.prefix != 30 {
            return Err(OrchError::Internal(format!(
                "restored guest IPv4 prefix {} does not match the allocated /30",
                allocation.prefix
            )));
        }
        if guest_ip == gateway {
            return Err(OrchError::Internal(
                "restored guest IPv4 address equals its gateway".into(),
            ));
        }
        let mask = u32::MAX << (32 - allocation.prefix);
        if u32::from(guest_ip) & mask != u32::from(gateway) & mask {
            return Err(OrchError::Internal(format!(
                "restored guest IPv4 address {guest_ip} and gateway {gateway} are not in the same /{}",
                allocation.prefix
            )));
        }
        Ok(Self {
            guest_ip,
            gateway,
            prefix: allocation.prefix,
        })
    }
}

fn rebind_restored_guest_network(
    socket_path: &Path,
    allocation: &NetAlloc,
) -> Result<(), OrchError> {
    let network = RestoredGuestNetwork::try_from(allocation)?;
    let client = VmmClient::new(socket_path)
        .with_connect_timeout(RESTORE_NETWORK_EXEC_TIMEOUT)
        .with_request_timeout(RESTORE_NETWORK_EXEC_TIMEOUT + EXEC_OP_MARGIN);
    client
        .repair_guest_network(tarit_vmm_client::GuestNetworkRepair {
            addr: network.guest_ip.to_string(),
            prefix: network.prefix,
            gateway: network.gateway.to_string(),
            dns_servers: Vec::new(),
        })
        .map_err(|error| OrchError::Vmm(format!("restore guest network rebind: {error}")))?;

    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn restored_guest_connectivity_command(allocation: &NetAlloc) -> Result<Command, OrchError> {
    let network = RestoredGuestNetwork::try_from(allocation)?;
    let mut command = Command::new("ping");
    command
        .args(["-4", "-n", "-c", "1", "-W", "2", "-I"])
        .arg(&allocation.tap)
        .arg(network.guest_ip.to_string());
    Ok(command)
}

#[cfg(target_os = "linux")]
fn verify_restored_guest_connectivity(allocation: &NetAlloc) -> Result<(), OrchError> {
    let network = RestoredGuestNetwork::try_from(allocation)?;
    let output = restored_guest_connectivity_command(allocation)?
        .output()
        .map_err(|error| {
            OrchError::Internal(format!(
                "probe restored guest {} through {}: {error}",
                network.guest_ip, allocation.tap
            ))
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(OrchError::Vmm(format!(
            "restored guest {} is unreachable through {}: {}",
            network.guest_ip,
            allocation.tap,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(not(target_os = "linux"))]
fn verify_restored_guest_connectivity(_allocation: &NetAlloc) -> Result<(), OrchError> {
    Err(OrchError::Internal(
        "restored guest connectivity verification requires Linux".into(),
    ))
}

fn build_vmm_config(
    cfg: &VmSpawnConfig,
    net: Option<&NetAlloc>,
    overlay: Option<String>,
    data_volumes: &[PreparedBlockAttachment],
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
            inherited_fd: None,
        });
    }
    volumes.extend(runtime_volume_configs(data_volumes));

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

fn runtime_volume_configs(volumes: &[PreparedBlockAttachment]) -> Vec<VolumeConfig> {
    volumes
        .iter()
        .map(|volume| VolumeConfig {
            path: format!("volume:{}", volume.volume_id),
            read_only: volume.read_only,
            overlay: None,
            inherited_fd: Some(volume.file.as_raw_fd()),
        })
        .collect()
}

fn net_config_for_allocation(allocation: &NetAlloc) -> NetConfig {
    NetConfig {
        tap: allocation.tap.clone(),
        guest_mac: None,
        guest_ip: Some(allocation.guest_ip.clone()),
        port_forwards: vec![],
    }
}

fn validate_network_startup_mode(
    enable_net: bool,
    preflight_taps: &[String],
) -> Result<(), OrchError> {
    if !enable_net && !preflight_taps.is_empty() {
        return Err(OrchError::Internal(
            "network-disabled startup refused: contained Tarit TAPs require TARIT_ENABLE_NET=1"
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
    use tarit_volume::BlockVolumeProvider;

    #[cfg(target_os = "linux")]
    #[test]
    fn tap_descriptor_becomes_inheritable_only_in_the_vmm_child() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        // SAFETY: pipe2 returned two new owned descriptors.
        let read_fd = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
        let write_fd = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };
        let raw_fd = read_fd.as_raw_fd();

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("test -e /proc/self/fd/{raw_fd}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: the closure changes only the test descriptor after fork.
        unsafe {
            command.pre_exec(move || clear_cloexec_for_child(raw_fd));
        }
        assert!(command.status().unwrap().success());

        let parent_flags = unsafe { libc::fcntl(read_fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(parent_flags & libc::FD_CLOEXEC, 0);
        drop(write_fd);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_live_vmm_accepts_process_owning_the_socket() {
        let socket = std::env::temp_dir().join(format!("tarit-adopt-{}.sock", Uuid::new_v4()));
        let allowed_uids = HashSet::from([unsafe { libc::geteuid() }]);
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
        let mut result = None;
        for _ in 0..200 {
            result = Some(verify_live_vmm(
                child.id(),
                &socket,
                None,
                Path::new("sh"),
                &allowed_uids,
            ));
            if result.as_ref().is_some_and(Result::is_ok) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        let result = result.expect("verification must be attempted");
        assert!(
            result.is_ok(),
            "owner process must be adoptable: {result:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_live_vmm_rejects_pid_that_does_not_own_the_socket() {
        let socket = std::env::temp_dir().join(format!("tarit-adopt-{}.sock", Uuid::new_v4()));
        let allowed_uids = HashSet::from([unsafe { libc::geteuid() }]);
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn unrelated process");
        let result = verify_live_vmm(child.id(), &socket, None, Path::new("sleep"), &allowed_uids);
        let _ = child.kill();
        let _ = child.wait();
        let error = result.expect_err("a reused PID must not be adopted");
        assert!(
            error.to_string().contains("does not own"),
            "unexpected error: {error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_live_vmm_rejects_unexpected_executable() {
        let socket = std::env::temp_dir().join(format!("tarit-adopt-{}.sock", Uuid::new_v4()));
        let allowed_uids = HashSet::from([unsafe { libc::geteuid() }]);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("read _line")
            .arg("tarit-vmm")
            .arg(&socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn executable-mismatch stand-in");

        let error = verify_live_vmm(child.id(), &socket, None, Path::new("sleep"), &allowed_uids)
            .expect_err("a different executable must never be adopted");

        let _ = child.kill();
        let _ = child.wait();
        assert!(
            error
                .to_string()
                .contains("is not running configured executable"),
            "unexpected error: {error}"
        );
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
        let allowed_uids = HashSet::from([unsafe { libc::geteuid() }]);
        let error = verify_live_vmm(pid, &socket, None, Path::new("true"), &allowed_uids)
            .expect_err("dead PID must not be adopted");
        assert!(error.is_gone(), "unexpected error: {error}");
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

    #[test]
    fn proc_pid_enumeration_skips_nonnumeric_entries_and_zero() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/proc-pid-enumeration-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("123")).unwrap();
        std::fs::create_dir(root.join("0")).unwrap();
        std::fs::create_dir(root.join("12x")).unwrap();
        std::fs::write(root.join("456"), b"not a process directory").unwrap();
        std::fs::write(root.join("fb"), b"framebuffer metadata").unwrap();

        let entries = proc_pid_entries(&root).unwrap();

        assert_eq!(entries, vec![(123, root.join("123"))]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transient_process_disappearance_errors_are_tolerated() {
        for errno in [libc::ENOENT, libc::ESRCH, libc::ENOTDIR] {
            let result =
                tolerate_process_disappearance::<()>(Err(std::io::Error::from_raw_os_error(errno)))
                    .unwrap();
            assert!(result.is_none(), "errno {errno} must be treated as gone");
            assert!(ProcessVerificationError::from_io(
                "inspect transient process",
                std::io::Error::from_raw_os_error(errno),
            )
            .is_gone());
        }

        let denied = tolerate_process_disappearance::<()>(Err(std::io::Error::from_raw_os_error(
            libc::EACCES,
        )));
        assert!(denied.is_err(), "non-transient errors must remain visible");
        assert!(ProcessVerificationError::from_io(
            "inspect protected process",
            std::io::Error::from_raw_os_error(libc::EACCES),
        )
        .is_permission_denied());
    }

    #[test]
    fn cgroup_pid_parsing_rejects_nonnumeric_and_nonpositive_values() {
        let source = Path::new("cgroup.procs");
        assert_eq!(
            parse_cgroup_processes("12\n34\n", source).unwrap(),
            HashSet::from([12, 34])
        );
        assert!(parse_cgroup_processes("12\nfb\n", source).is_err());
        assert!(parse_cgroup_processes("0\n", source).is_err());
        assert!(parse_cgroup_processes(&format!("{}\n", u32::MAX), source).is_err());
    }

    #[test]
    fn process_parent_parsing_reads_proc_status_ppid() {
        let source = Path::new("/proc/42/status");
        assert_eq!(
            parse_process_parent("Name:\tvmm\nPPid:\t17\nUid:\t1000\n", source).unwrap(),
            17
        );
        assert!(parse_process_parent("Name:\tvmm\n", source).is_err());
        assert!(parse_process_parent("PPid:\tnot-a-pid\n", source).is_err());
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
            host_session_id: Uuid::nil(),
            vmm_bin: root.join("vmm-must-not-run"),
            kernel: root.join("kernel"),
            rootfs: root.join("rootfs"),
            socket_dir: root.join("sockets"),
            db_path: root.join("fleet.db"),
            net_state_path: root.join("net-state.json"),
            images_dir: root.join("images"),
            shared_block: None,
            image_admission_policy: crate::image::ImageAdmissionPolicy::default(),
            max_vms: 4,
            max_vcpus: 4,
            max_memory_mib: 1024,
            peer_secret: "peer-secret".into(),
            peer_listen: None,
            peer_tls: None,
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
            vm_jail: None,
            vm_cgroup_pids_max: 1024,
            vm_io_quota: crate::config::VmIoQuotaConfig::default(),
            vm_net_quota: crate::config::VmNetQuotaConfig::default(),
            disk_pressure: crate::config::DiskPressureConfig::default(),
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
            egress_allowlist: Vec::new(),
            egress_allow_existing: false,
            data_volumes: Vec::new(),
        }
    }

    #[test]
    fn jail_identity_leases_are_unique_durable_and_cleaned() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/jail-leases-{}", Uuid::new_v4()));
        let config = crate::config::VmJailConfig {
            base_dir: root.clone(),
            uid_base: 20_000,
            gid_base: 30_000,
            id_count: 4,
            profile: crate::config::SUPPORTED_VM_JAIL_PROFILE.into(),
            seccomp: true,
            pid_namespace: false,
            network_namespace: false,
        };
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        let first;
        {
            let manager = JailManager::new(config.clone()).unwrap();
            first = manager.lease(first_id).unwrap();
            let second = manager.lease(second_id).unwrap();
            assert_ne!((first.uid, first.gid), (second.uid, second.gid));
            assert!(first.root.join(".tarit-jail.json").is_file());
        }

        let recovered = JailManager::new(config).unwrap();
        assert_eq!(
            recovered.identity(first_id).unwrap(),
            Some((first.uid, first.gid))
        );
        recovered.release(first_id).unwrap();
        assert!(!first.root.exists());
        recovered.release(second_id).unwrap();
        assert!(!root.join(format!("tarit-{second_id}")).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn jail_startup_removes_interrupted_staging_directories() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/jail-stage-recovery-{}", Uuid::new_v4()));
        let config = crate::config::VmJailConfig {
            base_dir: root.clone(),
            uid_base: 20_000,
            gid_base: 30_000,
            id_count: 4,
            profile: crate::config::SUPPORTED_VM_JAIL_PROFILE.into(),
            seccomp: true,
            pid_namespace: false,
            network_namespace: false,
        };
        JailManager::new(config.clone()).unwrap();
        let staging = root.join(format!(".tarit-stage-{}", Uuid::new_v4()));
        std::fs::create_dir(&staging).unwrap();
        std::fs::write(staging.join("partial"), b"incomplete").unwrap();

        JailManager::new(config).unwrap();
        assert!(!staging.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn jail_base_rejects_protected_broad_paths() {
        for path in ["/", "/tmp", "/var", "/srv"] {
            let error =
                validate_jail_base_path(Path::new(path)).expect_err("broad path must be rejected");
            assert!(error.to_string().contains("protected broad host path"));
        }
    }

    #[test]
    fn mountinfo_paths_are_decoded_before_mount_root_checks() {
        assert_eq!(
            decode_mountinfo_field(br"/srv/tarit\040jails\134private"),
            b"/srv/tarit jails\\private"
        );
    }

    #[test]
    fn jail_base_rejects_symlinks_and_does_not_chmod_targets() {
        use std::os::unix::fs::symlink;

        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/jail-base-symlink-{}", Uuid::new_v4()));
        let target = root.join("target");
        let link = root.join("jails");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, &link).unwrap();

        assert!(ensure_private_jail_base(&link).is_err());
        assert_eq!(
            std::fs::symlink_metadata(&target).unwrap().mode() & 0o777,
            0o755
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn jail_base_rejects_existing_non_private_or_unclaimed_directories() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/jail-base-existing-{}", Uuid::new_v4()));
        let public = root.join("public");
        std::fs::create_dir_all(&public).unwrap();
        std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_jail_base(&public).is_err());
        assert_eq!(
            std::fs::symlink_metadata(&public).unwrap().mode() & 0o777,
            0o755
        );

        let occupied = root.join("occupied");
        std::fs::create_dir(&occupied).unwrap();
        std::fs::set_permissions(&occupied, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(occupied.join("host-data"), b"keep").unwrap();
        let error = ensure_private_jail_base(&occupied)
            .expect_err("non-empty unclaimed directory must be rejected");
        assert!(error.to_string().contains("refuse to claim non-empty"));
        assert_eq!(std::fs::read(occupied.join("host-data")).unwrap(), b"keep");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn jail_base_creates_and_reopens_private_owned_directory() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/jail-base-private-{}", Uuid::new_v4()));
        let base = root.join("owned/jails");
        ensure_private_jail_base(&base).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&base).unwrap().mode() & 0o777,
            0o700
        );
        assert!(base.join(".tarit-jail-base.json").is_file());
        ensure_private_jail_base(&base).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    fn spawn_jailed_vmm_standin(jail_root: &Path) -> Child {
        let allowed_uids = HashSet::from([unsafe { libc::geteuid() }]);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("read _line")
            .arg("tarit-vmm")
            .arg("serve")
            .arg("--socket")
            .arg(JAIL_SOCKET_PATH)
            .arg("--jail-root")
            .arg(jail_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn jailed VMM stand-in");
        for _ in 0..200 {
            if verify_live_vmm(
                child.id(),
                &jail_root.join(JAIL_SOCKET_PATH.trim_start_matches('/')),
                Some(jail_root),
                Path::new("sh"),
                &allowed_uids,
            )
            .is_ok()
            {
                return child;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("jailed VMM stand-in did not publish its argv");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_reconciliation_terminates_unpersisted_jailed_vmm() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/startup-orphan-jail-{}", Uuid::new_v4()));
        let mut config = supervisor_config(&root);
        config.vmm_bin = PathBuf::from("sh");
        config.vm_jail = Some(crate::config::VmJailConfig {
            base_dir: root.join("jails"),
            uid_base: 20_000,
            gid_base: 30_000,
            id_count: 4,
            profile: crate::config::SUPPORTED_VM_JAIL_PROFILE.into(),
            seccomp: true,
            pid_namespace: false,
            network_namespace: false,
        });
        let supervisor = Arc::new(VmmSupervisor::new(config));
        let id = Uuid::new_v4();
        let lease = supervisor.jails.as_ref().unwrap().lease(id).unwrap();
        let mut child = spawn_jailed_vmm_standin(&lease.root);

        let warnings = test_runtime()
            .block_on(supervisor.readopt_running_vms(&mut []))
            .unwrap();
        assert!(warnings.is_empty());
        child.wait().expect("reap terminated stand-in");
        assert!(!lease.root.exists());
        assert_eq!(supervisor.jail_identity(id).unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_reconciliation_contains_durable_creating_runtime() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/startup-creating-jail-{}", Uuid::new_v4()));
        let mut config = supervisor_config(&root);
        config.vmm_bin = PathBuf::from("sh");
        config.vm_jail = Some(crate::config::VmJailConfig {
            base_dir: root.join("jails"),
            uid_base: 20_000,
            gid_base: 30_000,
            id_count: 4,
            profile: crate::config::SUPPORTED_VM_JAIL_PROFILE.into(),
            seccomp: true,
            pid_namespace: false,
            network_namespace: false,
        });
        let supervisor = Arc::new(VmmSupervisor::new(config));
        let id = Uuid::new_v4();
        let lease = supervisor.jails.as_ref().unwrap().lease(id).unwrap();
        let mut child = spawn_jailed_vmm_standin(&lease.root);
        let mut record = restart_record(&supervisor, id, &supervisor.socket_path_for(id));
        record.status = VmStatus::Creating;
        record.pid = None;
        record.socket_path = None;

        let warnings = test_runtime()
            .block_on(supervisor.readopt_running_vms(std::slice::from_mut(&mut record)))
            .unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, id);
        child.wait().expect("reap terminated stand-in");
        assert!(!lease.root.exists());
        assert_eq!(supervisor.jail_identity(id).unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn restart_layout_change_terminates_persisted_runtime_before_gc() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/restart-layout-change-{}", Uuid::new_v4()));
        let id = Uuid::new_v4();
        let old_jail = root.join("old-jails").join(format!("tarit-{id}"));
        std::fs::create_dir_all(old_jail.join("run")).unwrap();
        write_jail_marker(
            &old_jail,
            &JailMarker {
                version: JAIL_MARKER_VERSION,
                vm_id: id,
                uid: unsafe { libc::geteuid() },
                gid: unsafe { libc::getegid() },
            },
        )
        .unwrap();
        let mut child = spawn_jailed_vmm_standin(&old_jail);

        let mut config = supervisor_config(&root);
        config.vmm_bin = PathBuf::from("sh");
        config.vm_jail = Some(crate::config::VmJailConfig {
            base_dir: root.join("new-jails"),
            uid_base: 20_000,
            gid_base: 30_000,
            id_count: 4,
            profile: crate::config::SUPPORTED_VM_JAIL_PROFILE.into(),
            seccomp: true,
            pid_namespace: false,
            network_namespace: false,
        });
        let supervisor = Arc::new(VmmSupervisor::new(config));
        let socket = old_jail.join(JAIL_SOCKET_PATH.trim_start_matches('/'));
        let mut record = restart_record(&supervisor, id, &socket);
        record.pid = Some(child.id());
        record.runtime_layout = Some(VmRuntimeLayout {
            overlay_path: Some(old_jail.join("assets/rootfs.cow").display().to_string()),
            jail_path: Some(old_jail.display().to_string()),
            artifact_paths: vec![
                socket.display().to_string(),
                old_jail.display().to_string(),
                old_jail.join("assets/rootfs.cow").display().to_string(),
            ],
        });

        let warnings = test_runtime()
            .block_on(supervisor.readopt_running_vms(std::slice::from_mut(&mut record)))
            .unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("runtime layout conflicts"));
        child.wait().expect("reap layout-conflicting stand-in");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restart_missing_runtime_layout_blocks_before_gc() {
        let root = PathBuf::from(format!("target/restart-layout-missing-{}", Uuid::new_v4()));
        let supervisor = Arc::new(VmmSupervisor::new(supervisor_config(&root)));
        let id = Uuid::new_v4();
        let socket = supervisor.socket_path_for(id);
        let mut record = restart_record(&supervisor, id, &socket);
        record.runtime_layout = None;
        let error = test_runtime()
            .block_on(supervisor.readopt_running_vms(std::slice::from_mut(&mut record)))
            .expect_err("missing active runtime layout must block startup");
        assert!(error.to_string().contains("drain required"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unverified_cgroup_process_retains_jail_identity() {
        let root = std::env::current_dir().unwrap().join(format!(
            "target/startup-unverified-cgroup-{}",
            Uuid::new_v4()
        ));
        let cgroup_parent = root.join("cgroups");
        std::fs::create_dir_all(&cgroup_parent).unwrap();
        let mut config = supervisor_config(&root);
        config.vmm_bin = PathBuf::from("sh");
        config.vm_cgroup_parent = Some(cgroup_parent.display().to_string());
        config.vm_jail = Some(crate::config::VmJailConfig {
            base_dir: root.join("jails"),
            uid_base: 20_000,
            gid_base: 30_000,
            id_count: 4,
            profile: crate::config::SUPPORTED_VM_JAIL_PROFILE.into(),
            seccomp: true,
            pid_namespace: false,
            network_namespace: false,
        });
        let supervisor = VmmSupervisor::new(config);
        let id = Uuid::new_v4();
        let lease = supervisor.jails.as_ref().unwrap().lease(id).unwrap();
        let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
        let cgroup = cgroup_parent.join(format!("tarit-{id}"));
        std::fs::create_dir(&cgroup).unwrap();
        std::fs::write(cgroup.join("cgroup.procs"), unrelated.id().to_string()).unwrap();

        let error = supervisor
            .cleanup_uncommitted_runtime(id)
            .expect_err("unverified process must fail closed");
        assert!(error.to_string().contains("failed ownership verification"));
        assert!(unrelated.try_wait().unwrap().is_none());
        assert!(lease.root.exists());
        assert!(supervisor.jail_identity(id).unwrap().is_some());

        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        std::fs::remove_dir_all(root).unwrap();
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
            host_session_id: Uuid::nil(),
            vmm_bin: PathBuf::from("true"),
            kernel: PathBuf::from("kernel"),
            rootfs: PathBuf::from("rootfs"),
            socket_dir: PathBuf::from("target/taritd-supervisor-test/sockets"),
            db_path: PathBuf::from("target/taritd-supervisor-test/fleet.db"),
            net_state_path: PathBuf::from("target/taritd-supervisor-test/net-state.json"),
            images_dir: PathBuf::from("target/taritd-supervisor-test/images"),
            shared_block: None,
            image_admission_policy: crate::image::ImageAdmissionPolicy::default(),
            max_vms: 4,
            max_vcpus: 4,
            max_memory_mib: 1024,
            peer_secret: "peer-secret".into(),
            peer_listen: None,
            peer_tls: None,
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
            vm_jail: None,
            vm_cgroup_pids_max: 1024,
            vm_io_quota: crate::config::VmIoQuotaConfig::default(),
            vm_net_quota: crate::config::VmNetQuotaConfig::default(),
            disk_pressure: crate::config::DiskPressureConfig::default(),
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

    fn restart_record(supervisor: &VmmSupervisor, id: Uuid, socket_path: &Path) -> VmRecord {
        let now = chrono::Utc::now();
        let runtime_layout =
            supervisor.runtime_layout_for_config(id, &spawn_config(true, Some("/rootfs".into())));
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
            rootfs_read_only: true,
            cmdline: DEFAULT_CMDLINE.into(),
            runtime_layout: Some(runtime_layout),
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
        let mut record = restart_record(&supervisor, id, &socket_path);
        let previous_updated_at = record.updated_at;

        test_runtime()
            .block_on(supervisor.reconcile_readopted_status(&mut record))
            .unwrap();

        server.join().unwrap();
        assert_eq!(record.status, VmStatus::Paused);
        assert_eq!(record.revision, 9);
        assert!(
            record.rootfs_read_only,
            "restart reconciliation must preserve the VM's effective mount mode instead of the current host default"
        );
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
        let mut record = restart_record(&supervisor, id, &socket_path);

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
        let mut record = restart_record(&supervisor, id, &socket_path);

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

    #[cfg(target_os = "linux")]
    #[test]
    fn restart_reconciliation_continues_past_a_recoverable_bad_vm_record() {
        let root = PathBuf::from(format!(
            "target/taritd-restart-continues-{}",
            Uuid::new_v4()
        ));
        let good_socket = root.join("good-vmm.sock");
        std::fs::create_dir_all(&root).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&good_socket).unwrap();
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
        let mut config = supervisor_config(&root);
        config.vmm_bin = PathBuf::from("sh");
        let supervisor = Arc::new(VmmSupervisor::new(config));

        let good_id = Uuid::new_v4();
        let good_process = ManagedProcess::new(
            Command::new("sh")
                .arg("-c")
                .arg("read _line")
                .arg("tarit-vmm")
                .arg("serve")
                .arg("--socket")
                .arg(&good_socket)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut good_record = restart_record(&supervisor, good_id, &good_socket);
        good_record.pid = Some(good_process.pid);

        let bad_id = Uuid::new_v4();
        let bad_socket = root.join("missing.sock");
        let bad_process = ManagedProcess::new(
            Command::new("sh")
                .arg("-c")
                .arg("read _line")
                .arg("tarit-vmm")
                .arg("serve")
                .arg("--socket")
                .arg(&bad_socket)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut bad_record = restart_record(&supervisor, bad_id, &bad_socket);
        bad_record.pid = Some(bad_process.pid);
        for (pid, socket) in [
            (good_process.pid, good_socket.as_path()),
            (bad_process.pid, bad_socket.as_path()),
        ] {
            let mut verified = false;
            for _ in 0..200 {
                if verify_live_vmm(
                    pid,
                    socket,
                    None,
                    Path::new("sh"),
                    &HashSet::from([unsafe { libc::geteuid() }]),
                )
                .is_ok()
                {
                    verified = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(verified, "stand-in VMM {pid} did not publish its argv");
        }

        let mut records = vec![good_record, bad_record];
        let failures = test_runtime()
            .block_on(supervisor.readopt_running_vms(&mut records))
            .unwrap();

        server.join().unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].id, bad_id);
        assert_eq!(records[0].status, VmStatus::Paused);
        assert_eq!(records[0].revision, 9);
        assert!(records[1].status == VmStatus::Running);
        supervisor.stop_vm(good_id).unwrap();
        assert!(supervisor.scheduler.release(good_id));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn network_disabled_startup_rejects_contained_taps() {
        assert!(validate_network_startup_mode(false, &[]).is_ok());
        assert!(validate_network_startup_mode(false, &["insta7".into()]).is_err());
        assert!(validate_network_startup_mode(true, &["insta7".into()]).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_io_resolves_partitions_to_parent_block_devices() {
        use std::os::unix::fs::symlink;

        let root = PathBuf::from(format!("target/taritd-sysfs-{}", Uuid::new_v4()));
        let sys_block = root.join("dev/block");
        let disk = root.join("devices/pci/block/nvme0n1");
        let partition = disk.join("nvme0n1p1");
        std::fs::create_dir_all(&sys_block).unwrap();
        std::fs::create_dir_all(&partition).unwrap();
        std::fs::write(disk.join("dev"), "259:0\n").unwrap();
        std::fs::write(partition.join("partition"), "1\n").unwrap();
        symlink(
            std::fs::canonicalize(&partition).unwrap(),
            sys_block.join("259:1"),
        )
        .unwrap();

        assert_eq!(
            resolve_cgroup_block_device("259:1", &sys_block).unwrap(),
            "259:0"
        );
        std::fs::remove_dir_all(root).unwrap();
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
        // Keep the Unix socket below sockaddr_un::sun_path even when the host's
        // configured temporary directory has a long per-user prefix.
        let test_dir = PathBuf::from("/tmp").join(format!(
            "tarit-td-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir(&test_dir).expect("create teardown test directory");
        let root = test_dir.join("state");
        let socket_path = test_dir.join("vmm.sock");
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

        let _ = std::fs::remove_dir_all(test_dir);
    }

    #[test]
    fn teardown_vm_retains_runtime_artifacts_when_process_exit_is_unconfirmed() {
        let test_dir = PathBuf::from("/tmp").join(format!(
            "tarit-retain-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let root = test_dir.join("state");
        let id = Uuid::new_v4();
        let socket_path = test_dir.join("vmm.sock");
        std::fs::create_dir_all(&test_dir).expect("create teardown retention directory");
        std::fs::write(&socket_path, b"owned socket placeholder")
            .expect("create socket placeholder");

        let child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn test VMM process");
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));
        let poisoned_child = Arc::clone(&child);
        let poison = std::thread::spawn(move || {
            let _guard = poisoned_child.lock().expect("lock child before poisoning");
            panic!("poison child lock to simulate unconfirmed process state");
        });
        assert!(poison.join().is_err());

        let process = ManagedProcess {
            pid,
            handle: ProcessHandle::Owned(child),
        };
        let vm = RunningVm::new(pid, socket_path.clone(), process, None);
        let supervisor = VmmSupervisor::new(supervisor_config(&root));
        let overlay_path = PathBuf::from(supervisor.overlay_path_for(id));
        std::fs::create_dir_all(overlay_path.parent().expect("overlay parent"))
            .expect("create overlay directory");
        std::fs::write(&overlay_path, b"owned overlay").expect("create owned overlay");

        let error = supervisor
            .teardown_vm(id, &vm)
            .expect_err("unconfirmed process state must fail closed");

        assert!(error.to_string().contains("terminate VMM before releasing"));
        assert!(socket_path.exists(), "control socket must remain owned");
        assert!(overlay_path.exists(), "overlay must remain owned");

        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
            libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), 0);
        }
        let _ = std::fs::remove_dir_all(test_dir);
    }

    #[test]
    fn stop_vm_retains_a_warm_vm_when_process_exit_is_unconfirmed() {
        let root = PathBuf::from("/tmp").join(format!(
            "tarit-warm-retain-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let id = Uuid::new_v4();
        let child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn warm test VMM process");
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));
        let poisoned_child = Arc::clone(&child);
        let poison = std::thread::spawn(move || {
            let _guard = poisoned_child.lock().expect("lock child before poisoning");
            panic!("poison child lock to simulate unconfirmed process state");
        });
        assert!(poison.join().is_err());

        let process = ManagedProcess {
            pid,
            handle: ProcessHandle::Owned(child),
        };
        let supervisor = VmmSupervisor::new(supervisor_config(&root));
        supervisor
            .warm
            .lock()
            .expect("lock warm pool")
            .push_back(WarmVm {
                id,
                vm: RunningVm::new(pid, PathBuf::new(), process, None),
                spec: spawn_config(false, None),
            });

        let error = supervisor
            .stop_vm(id)
            .expect_err("unconfirmed warm process state must fail closed");

        assert!(error.to_string().contains("terminate VMM before releasing"));
        let retained = supervisor.warm.lock().expect("lock retained warm pool");
        assert_eq!(retained.len(), 1);
        assert_eq!(retained.front().expect("retained warm VM").id, id);
        drop(retained);

        let retained = supervisor
            .warm
            .lock()
            .expect("lock retained warm pool for cleanup")
            .pop_front()
            .expect("remove retained warm VM");
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
            libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), 0);
        }
        drop(retained);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn process_exit_poll_is_bounded_when_the_process_never_exits() {
        let mut polls = 0;
        let exited = poll_process_exit(Duration::ZERO, || {
            polls += 1;
            Ok(false)
        })
        .expect("poll process exit");

        assert!(!exited);
        assert_eq!(polls, 1);
    }

    #[test]
    fn process_exit_poll_returns_immediately_for_an_exited_process() {
        let mut polls = 0;
        let exited = poll_process_exit(Duration::from_secs(1), || {
            polls += 1;
            Ok(true)
        })
        .expect("poll process exit");

        assert!(exited);
        assert_eq!(polls, 1);
    }

    #[test]
    fn process_exit_poll_propagates_observation_failure() {
        let error = poll_process_exit(Duration::from_secs(1), || {
            Err(std::io::Error::other("observation failed"))
        })
        .expect_err("poll failure must propagate");

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
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

        let vmm_config = build_vmm_config(&cfg, None, Some(expected.clone()), &[]);
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
            &[],
        );
        assert!(rw_config.kernel.cmdline.contains("root=/dev/vda rw"));
    }

    #[test]
    fn persistent_volume_is_opened_once_and_rendered_as_an_inherited_descriptor() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/runtime-volume-{}", Uuid::new_v4()));
        let config = supervisor_config(&root);
        let provider = tarit_volume::LocalBlockProvider::open(
            config.images_dir.join("volumes"),
            config.host_id.clone(),
            16 * 1024 * 1024,
        )
        .unwrap();
        let volume_id = Uuid::new_v4();
        provider.create(volume_id, 4 * 1024 * 1024).unwrap();
        let supervisor = VmmSupervisor::new(config);
        let mut cfg = spawn_config(false, None);
        cfg.data_volumes.push(VmDataVolumeConfig {
            id: volume_id,
            provider: "local_block".into(),
            size_bytes: 4 * 1024 * 1024,
            read_only: false,
            generation: 1,
        });

        let runtime = supervisor
            .prepare_runtime(Uuid::new_v4(), &cfg, None)
            .unwrap();
        assert_eq!(runtime.data_volumes.len(), 1);
        let raw_fd = runtime.data_volumes[0].file.as_raw_fd();
        assert_ne!(
            unsafe { libc::fcntl(raw_fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let vmm = build_vmm_config(&runtime.vm_config, None, None, &runtime.data_volumes);
        assert_eq!(vmm.volumes.len(), 1);
        assert_eq!(vmm.volumes[0].path, format!("volume:{volume_id}"));
        assert_eq!(vmm.volumes[0].inherited_fd, Some(raw_fd));
        assert!(!vmm.volumes[0]
            .path
            .contains(root.to_string_lossy().as_ref()));

        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn jailed_restore_layout_uses_a_unique_overlay_path() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/restore-layout-{}", Uuid::new_v4()));
        let mut config = supervisor_config(&root);
        config.vm_jail = Some(crate::config::VmJailConfig {
            base_dir: root.join("jails"),
            uid_base: 20_000,
            gid_base: 30_000,
            id_count: 4,
            profile: crate::config::SUPPORTED_VM_JAIL_PROFILE.into(),
            seccomp: true,
            pid_namespace: true,
            network_namespace: true,
        });
        let supervisor = VmmSupervisor::new(config);
        let id = Uuid::new_v4();
        let cfg = spawn_config(true, Some(PathBuf::from("/rootfs.ext4")));
        let normal = supervisor.runtime_layout_for_config(id, &cfg);
        let restored = supervisor.runtime_layout_for_restore_config(id, &cfg);
        assert_ne!(normal.overlay_path, restored.overlay_path);
        assert!(restored
            .overlay_path
            .as_deref()
            .is_some_and(|path| path.ends_with(&format!("/assets/restored-rootfs-{id}.cow"))));

        let next_id = Uuid::new_v4();
        let next_restored = supervisor.runtime_layout_for_restore_config(next_id, &cfg);
        assert_ne!(restored.overlay_path, next_restored.overlay_path);

        let snapshot_overlay = supervisor
            .restore_overlay_path_for_snapshot(id, Path::new("/private/snapshot-handle.ram"));
        assert!(supervisor.is_valid_restore_overlay_path(id, Path::new(&snapshot_overlay)));
        assert!(!supervisor.is_valid_restore_overlay_path(
            id,
            &root.join(format!(
                "jails/tarit-{id}/assets/restored-rootfs-{id}-BAD.cow"
            ))
        ));
        assert!(!supervisor.is_valid_restore_overlay_path(
            id,
            &root.join(format!(
                "jails/tarit-{id}/elsewhere/restored-rootfs-{id}-0123456789abcdef.cow"
            ))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn jailed_snapshot_restore_passes_an_absolute_overlay_to_vmm() {
        let jail_root = Path::new("/worker/jails/123/root");
        let host_overlay = jail_root.join("assets/restored-rootfs-123.cow");
        assert_eq!(
            jail_guest_path(jail_root, &host_overlay).unwrap(),
            "/assets/restored-rootfs-123.cow"
        );
        assert!(jail_guest_path(jail_root, Path::new("/worker/other.cow")).is_err());
    }

    #[test]
    fn cgroup_limits_follow_exact_reserved_shape() {
        let root = PathBuf::from(format!("target/cgroup-args-{}", Uuid::new_v4()));
        let mut config = supervisor_config(&root);
        config.vm_cgroup_parent = Some("/sys/fs/cgroup/tarit".into());
        config.vm_io_quota = crate::config::VmIoQuotaConfig {
            read_bps_max: Some(1_048_576),
            write_bps_max: Some(2_097_152),
            read_iops_max: None,
            write_iops_max: None,
        };
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("rootfs"), vec![0u8; 4096]).unwrap();
        let supervisor = VmmSupervisor::new(config);
        let id = Uuid::new_v4();
        let spawn_cfg = spawn_config(false, Some(root.join("rootfs")));
        let runtime = supervisor.prepare_runtime(id, &spawn_cfg, None).unwrap();
        for (vcpus, memory_mib, cpu, memory) in [
            (1, 256, "1000m", "640M"),
            (2, 512, "2000m", "1024M"),
            (8, 4096, "8000m", "6400M"),
        ] {
            let args = supervisor
                .cgroup_args(id, ResourceShape::new(vcpus, memory_mib), &runtime)
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
            #[cfg(target_os = "linux")]
            {
                let io_index = args
                    .iter()
                    .position(|arg| arg == "--cgroup-io-max")
                    .unwrap();
                assert!(args[io_index + 1].contains("rbps=1048576"));
                assert!(args[io_index + 1].contains("wbps=2097152"));
                assert_eq!(
                    args[io_index + 1].lines().count(),
                    1,
                    "base and overlay on the same backing device must be deduplicated"
                );
            }
            #[cfg(not(target_os = "linux"))]
            assert!(args.iter().all(|arg| arg != "--cgroup-io-max"));
        }
    }

    #[test]
    fn restart_reapplies_new_and_reduced_cgroup_quotas() {
        let root = PathBuf::from(format!("target/cgroup-readopt-limits-{}", Uuid::new_v4()));
        let cgroup_parent = root.join("cgroups");
        std::fs::create_dir_all(&cgroup_parent).unwrap();
        let mut config = supervisor_config(&root);
        config.vm_cgroup_parent = Some(cgroup_parent.display().to_string());
        config.vm_cgroup_pids_max = 128;
        config.vm_io_quota = crate::config::VmIoQuotaConfig {
            read_bps_max: Some(1_048_576),
            write_bps_max: Some(2_097_152),
            read_iops_max: None,
            write_iops_max: None,
        };
        let supervisor = VmmSupervisor::new(config);
        let id = Uuid::new_v4();
        let rootfs = root.join("rootfs");
        let overlay = PathBuf::from(supervisor.overlay_path_for(id));
        std::fs::create_dir_all(overlay.parent().unwrap()).unwrap();
        std::fs::write(&rootfs, b"rootfs").unwrap();
        std::fs::write(&overlay, b"overlay").unwrap();
        let child = supervisor.exact_vm_cgroup_path(id).unwrap();
        std::fs::create_dir(&child).unwrap();
        for (key, value) in [
            ("cgroup.procs", "4242"),
            ("cpu.max", "900000 100000"),
            ("cpu.weight", "999"),
            ("memory.max", "2147483648"),
            ("pids.max", "999"),
            ("io.weight", "999"),
            ("io.max", "0:0 rbps=9999999 wbps=9999999"),
        ] {
            std::fs::write(child.join(key), value).unwrap();
        }
        let mut record = restart_record(&supervisor, id, &supervisor.socket_path_for(id));
        record.vcpus = 2;
        record.memory_mib = 512;
        record.rootfs_path = Some(rootfs.display().to_string());
        record.runtime_layout = Some(
            supervisor.runtime_layout_for_config(id, &spawn_config(true, Some(rootfs.clone()))),
        );

        supervisor
            .reconcile_readopted_cgroup(&record, 4242)
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(child.join("cpu.max")).unwrap(),
            "200000 100000"
        );
        assert_eq!(
            std::fs::read_to_string(child.join("cpu.weight")).unwrap(),
            "100"
        );
        assert_eq!(
            std::fs::read_to_string(child.join("memory.max")).unwrap(),
            (1024_u64 * 1024 * 1024).to_string()
        );
        assert_eq!(
            std::fs::read_to_string(child.join("pids.max")).unwrap(),
            "128"
        );
        assert_eq!(
            std::fs::read_to_string(child.join("io.weight")).unwrap(),
            "100"
        );
        let io_max = std::fs::read_to_string(child.join("io.max")).unwrap();
        assert!(io_max.contains("rbps=1048576"));
        assert!(io_max.contains("wbps=2097152"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cgroup_recovery_rejects_unsafe_paths_and_missing_controls() {
        let root = PathBuf::from(format!("target/cgroup-readopt-unsafe-{}", Uuid::new_v4()));
        let cgroup_parent = root.join("cgroups");
        let outside = root.join("outside");
        std::fs::create_dir_all(&cgroup_parent).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("cgroup.procs"), "4242").unwrap();
        let mut config = supervisor_config(&root);
        config.vm_cgroup_parent = Some(cgroup_parent.display().to_string());
        let supervisor = VmmSupervisor::new(config);
        let id = Uuid::new_v4();
        let child = supervisor.exact_vm_cgroup_path(id).unwrap();
        std::os::unix::fs::symlink(&outside, &child).unwrap();

        let unsafe_error = validate_owned_vm_cgroup(&cgroup_parent, &child, id, 4242).unwrap_err();
        assert_eq!(unsafe_error.kind(), std::io::ErrorKind::PermissionDenied);

        std::fs::remove_file(&child).unwrap();
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("cgroup.procs"), "4242").unwrap();
        let record = restart_record(&supervisor, id, &supervisor.socket_path_for(id));
        let missing_error = supervisor
            .reconcile_readopted_cgroup(&record, 4242)
            .unwrap_err();
        assert!(missing_error.to_string().contains("cpu.max"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cgroup_recovery_supports_io_weight_io_max_and_cpuset_verification() {
        let root = PathBuf::from(format!(
            "target/cgroup-readopt-full-plan-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        for (key, value) in [
            ("cpu.max", "900000 100000"),
            ("cpu.weight", "999"),
            ("memory.max", "2147483648"),
            ("pids.max", "999"),
            ("io.weight", "999"),
            ("io.max", "8:0 rbps=9999999"),
            ("cpuset.cpus", "2-3"),
            ("cpuset.mems", "1"),
        ] {
            std::fs::write(root.join(key), value).unwrap();
        }
        let plan = CgroupLimitPlan {
            cpu_max: Some("200000 100000".into()),
            cpu_weight: Some(100),
            memory_max: Some(1_073_741_824),
            pids_max: Some(128),
            io_weight: Some(100),
            io_max: Some("8:0 rbps=1048576".into()),
            cpuset_cpus: Some("0-1".into()),
            cpuset_mems: Some("0".into()),
        };

        apply_and_verify_cgroup_limits(&root, &plan).unwrap();

        for (key, expected) in plan.entries() {
            let actual = std::fs::read_to_string(root.join(key)).unwrap();
            assert!(cgroup_value_matches(key, &expected, &actual));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cgroup_recovery_clears_obsolete_io_limits() {
        let root = PathBuf::from(format!("target/cgroup-readopt-clear-io-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        for (key, value) in [
            ("cpu.max", "200000 100000"),
            ("cpu.weight", "100"),
            ("memory.max", "1073741824"),
            ("pids.max", "128"),
            ("io.max", "8:0 rbps=1048576 wbps=2097152"),
        ] {
            std::fs::write(root.join(key), value).unwrap();
        }
        let plan = CgroupLimitPlan {
            cpu_max: Some("200000 100000".into()),
            cpu_weight: Some(100),
            memory_max: Some(1_073_741_824),
            pids_max: Some(128),
            ..CgroupLimitPlan::default()
        };

        apply_and_verify_cgroup_limits(&root, &plan).unwrap();

        let io_max = std::fs::read_to_string(root.join("io.max")).unwrap();
        assert!(io_max.contains("rbps=max"));
        assert!(io_max.contains("wbps=max"));
        assert!(io_limits_match_exact("", &io_max));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_adoption_recovery_fences_the_identified_vmm() {
        let root = PathBuf::from(format!(
            "target/readopt-fence-cgroup-failure-{}",
            Uuid::new_v4()
        ));
        let supervisor = VmmSupervisor::new(supervisor_config(&root));
        let id = Uuid::new_v4();
        supervisor.protect_artifact_owner(id).unwrap();
        let process = ManagedProcess::new(Command::new("sleep").arg("30").spawn().unwrap());
        let pid = process.pid;
        let vm = RunningVm::new(pid, PathBuf::new(), process, None);

        let failure = supervisor.fence_readopt_failure(
            id,
            &vm,
            "injected cgroup reapplication failure".into(),
        );

        assert!(matches!(failure, ReadoptFailure::Unadoptable(_)));
        assert_ne!(unsafe { libc::kill(pid as libc::pid_t, 0) }, 0);
        assert!(!supervisor.artifact_owners.lock().unwrap().contains(&id));
        std::fs::remove_dir_all(root).unwrap();
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
        let worker_cfg = spawn_config(false, Some(supervisor.config.rootfs.clone()));
        let worker = thread::spawn(move || {
            let runtime = worker_supervisor
                .prepare_runtime(id, &worker_cfg, None)
                .unwrap();
            done_tx
                .send(worker_supervisor.spawn_server_for_boot(&ticket, &runtime, None))
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
    fn duplicate_boot_registration_joins_without_replacing_the_owner() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let ticket = test_runtime()
            .block_on(supervisor.begin_boot_with_registration(
                id,
                SpawnPurpose::Live,
                ResourceShape::new(1, 1),
                || async { Ok(()) },
            ))
            .unwrap();

        let duplicate = test_runtime().block_on(supervisor.begin_boot_with_registration(
            id,
            SpawnPurpose::Live,
            ResourceShape::new(1, 1),
            || async { Ok(()) },
        ));
        assert!(matches!(duplicate, Err(OrchError::Conflict(_))));
        assert!(supervisor
            .booting
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|booting| Arc::ptr_eq(&booting.control, &ticket.control)));

        let waiting_supervisor = Arc::clone(&supervisor);
        let waiter = thread::spawn(move || {
            test_runtime().block_on(waiting_supervisor.wait_for_registered_boot_or_running(id))
        });
        ticket.control.complete(Ok(()));
        assert!(waiter.join().unwrap().unwrap());

        supervisor.complete_booting(id, &ticket.control, Ok(()));
        supervisor.release_reservation_after_terminal(id).unwrap();
        assert!(!supervisor.has_retained_boot(id));
    }

    #[test]
    fn late_boot_join_recognizes_runtime_handoff_without_accepting_a_bare_reservation() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        supervisor.reserve_existing_for_test(id);
        assert!(!test_runtime()
            .block_on(supervisor.wait_for_registered_boot_or_running(id))
            .unwrap());

        let process = ManagedProcess::new(Command::new("sleep").arg("30").spawn().unwrap());
        supervisor.running.lock().unwrap().insert(
            id,
            RunningVm::new(process.pid, PathBuf::new(), process, None),
        );
        assert!(test_runtime()
            .block_on(supervisor.wait_for_registered_boot_or_running(id))
            .unwrap());

        supervisor.stop_vm(id).unwrap();
        supervisor.release_reservation_after_terminal(id).unwrap();
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
        supervisor.remember_golden_artifacts(&mut artifacts, &[]);

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
    fn golden_bundle_restores_from_its_captured_disk_upper() {
        let (snapshot, overlay) = GoldenBundle {
            snapshot_path: "golden.ram".into(),
            overlay_path: Some("golden.cow".into()),
        }
        .into_restore_parts();
        assert_eq!(snapshot, "golden.ram");
        assert_eq!(overlay, RestoreOverlay::Seeded(PathBuf::from("golden.cow")));
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
                .spawn_warm_restore(
                    class,
                    GoldenBundle {
                        snapshot_path: "golden.snap".into(),
                        overlay_path: Some("golden.cow".into()),
                    },
                )
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
    fn gc_preserves_live_overlay_during_warm_to_running_handoff() {
        let root = PathBuf::from(format!("target/warm-handoff-overlay-gc-{}", Uuid::new_v4()));
        let mut config = supervisor_config(&root);
        config.disk_pressure.artifact_min_age_secs = 0;
        let supervisor = Arc::new(VmmSupervisor::new(config));
        let id = Uuid::new_v4();
        let spec = spawn_config(false, Some(root.join("rootfs")));
        supervisor.seed_warm_for_test(id, spec.clone()).unwrap();
        let overlay = PathBuf::from(supervisor.overlay_path_for(id));
        std::fs::write(&overlay, b"live upper").unwrap();
        std::fs::set_permissions(&overlay, std::fs::Permissions::from_mode(0o600)).unwrap();
        let pause = supervisor.pause_after_warm_dequeue_for_test();
        let handoff_supervisor = Arc::clone(&supervisor);
        let handoff = thread::spawn(move || {
            test_runtime()
                .block_on(handoff_supervisor.take_warm_with_publication(
                    &spec,
                    &OwnedTaskControl::new(),
                    |_| async { Ok(()) },
                    |vm_id, _, _| async move { Ok::<_, PublicationFailure>(vm_id) },
                ))
                .unwrap()
        });

        pause.wait_until_entered();
        assert_eq!(
            supervisor.warm_count(&spawn_config(false, Some(root.join("rootfs")))),
            0
        );
        assert!(!supervisor.is_running(id));
        let report = supervisor
            .sweep_owned_artifacts(ArtifactReferences::default())
            .unwrap();
        assert_eq!(report.removed_files, 0);
        assert!(
            overlay.exists(),
            "live overlay became collectible in handoff"
        );

        pause.release();
        assert!(matches!(
            handoff.join().unwrap(),
            WarmClaimOutcome::Published(published) if published == id
        ));
        supervisor.stop_vm(id).unwrap();
        assert!(supervisor.scheduler.release(id));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gc_preserves_live_jail_during_warm_to_running_handoff() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/warm-handoff-jail-gc-{}", Uuid::new_v4()));
        let mut config = supervisor_config(&root);
        config.disk_pressure.artifact_min_age_secs = 0;
        config.vm_jail = Some(crate::config::VmJailConfig {
            base_dir: root.join("jails"),
            uid_base: 20_000,
            gid_base: 30_000,
            id_count: 4,
            profile: crate::config::SUPPORTED_VM_JAIL_PROFILE.into(),
            seccomp: true,
            pid_namespace: false,
            network_namespace: false,
        });
        let supervisor = Arc::new(VmmSupervisor::new(config));
        let id = Uuid::new_v4();
        let spec = spawn_config(false, Some(root.join("rootfs")));
        let lease = supervisor.jails.as_ref().unwrap().lease(id).unwrap();
        std::fs::create_dir_all(lease.root.join("assets")).unwrap();
        std::fs::write(lease.root.join("assets/rootfs.cow"), b"live upper").unwrap();
        supervisor.seed_warm_for_test(id, spec.clone()).unwrap();
        let pause = supervisor.pause_after_warm_dequeue_for_test();
        let handoff_supervisor = Arc::clone(&supervisor);
        let handoff = thread::spawn(move || {
            test_runtime()
                .block_on(handoff_supervisor.take_warm_with_publication(
                    &spec,
                    &OwnedTaskControl::new(),
                    |_| async { Ok(()) },
                    |vm_id, _, _| async move { Ok::<_, PublicationFailure>(vm_id) },
                ))
                .unwrap()
        });

        pause.wait_until_entered();
        assert!(!supervisor.is_running(id));
        let report = supervisor
            .sweep_owned_artifacts(ArtifactReferences::default())
            .unwrap();
        assert_eq!(report.removed_jails, 0);
        assert!(
            lease.root.exists(),
            "live jail became collectible in handoff"
        );

        pause.release();
        assert!(matches!(
            handoff.join().unwrap(),
            WarmClaimOutcome::Published(published) if published == id
        ));
        supervisor.stop_vm(id).unwrap();
        assert!(supervisor.scheduler.release(id));
        assert!(!lease.root.exists());
        std::fs::remove_dir_all(root).unwrap();
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
        let handoff_spec = spec.clone();
        let handoff = thread::spawn(move || {
            test_runtime().block_on(handoff_supervisor.take_warm_with_publication(
                &handoff_spec,
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
            supervisor.warm_count(&spec),
            1,
            "a selected warm VM must remain in the warm registry until Creating is registered"
        );
        finish_registration_tx.send(()).unwrap();
        assert!(matches!(
            handoff.join().unwrap().unwrap(),
            WarmClaimOutcome::PreRuntimeFailure(_)
        ));
        assert_eq!(supervisor.warm_count(&spec), 1);
        assert!(!supervisor.is_running(id));
    }

    #[test]
    fn warm_depth_and_claim_match_the_exact_spawn_configuration() {
        let supervisor = test_supervisor();
        let id = Uuid::new_v4();
        let first = spawn_config(false, Some(PathBuf::from("/images/a.ext4")));
        let second = spawn_config(false, Some(PathBuf::from("/images/b.ext4")));
        let process = ManagedProcess::new(Command::new("true").spawn().unwrap());
        supervisor.warm.lock().unwrap().push_back(WarmVm {
            id,
            vm: RunningVm::new(process.pid, PathBuf::from("exact-warm.sock"), process, None),
            spec: first.clone(),
        });

        assert_eq!(supervisor.warm_count(&first), 1);
        assert_eq!(supervisor.warm_count(&second), 0);
        let task = OwnedTaskControl::new();
        let outcome = test_runtime()
            .block_on(supervisor.take_warm_with_publication(
                &second,
                &task,
                |_| async { Ok(()) },
                |_, _, _| async { Ok::<(), PublicationFailure>(()) },
            ))
            .unwrap();
        assert!(matches!(outcome, WarmClaimOutcome::NoMatch));
        assert_eq!(supervisor.warm_count(&first), 1);
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
    fn restored_network_rebind_uses_typed_agent_repair() {
        let socket_path = PathBuf::from(format!(
            "target/taritd-restore-network-{}-{}.sock",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut length = [0_u8; 4];
            stream.read_exact(&mut length).unwrap();
            let mut body = vec![0; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut body).unwrap();
            let request: tarit_vmm_client::ApiRequest = serde_json::from_slice(&body).unwrap();
            assert!(matches!(
                request,
                tarit_vmm_client::ApiRequest::RepairGuestNetwork {
                    network: tarit_vmm_client::GuestNetworkRepair {
                        ref addr,
                        prefix: 30,
                        ref gateway,
                        ref dns_servers,
                    }
                } if addr == "172.16.0.30"
                    && gateway == "172.16.0.29"
                    && dns_servers.is_empty()
            ));
            let response = tarit_vmm_client::ApiResponse::GuestNetworkRepaired;
            let encoded = serde_json::to_vec(&response).unwrap();
            stream
                .write_all(&(encoded.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&encoded).unwrap();
            stream.flush().unwrap();
        });
        let allocation = NetAlloc {
            idx: 7,
            vm_id: Uuid::new_v4(),
            tap: "insta7".into(),
            host_ip: "172.16.0.29".into(),
            guest_ip: "172.16.0.30".into(),
            prefix: 30,
        };
        let connectivity = restored_guest_connectivity_command(&allocation).unwrap();
        assert_eq!(connectivity.get_program(), "ping");
        assert_eq!(
            connectivity
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "-4",
                "-n",
                "-c",
                "1",
                "-W",
                "2",
                "-I",
                "insta7",
                "172.16.0.30"
            ]
        );

        rebind_restored_guest_network(&socket_path, &allocation).unwrap();

        server.join().unwrap();
        std::fs::remove_file(socket_path).unwrap();
    }

    #[test]
    fn restored_network_rebind_rejects_non_ip_input_before_exec() {
        let allocation = NetAlloc {
            idx: 7,
            vm_id: Uuid::new_v4(),
            tap: "insta7".into(),
            host_ip: "172.16.0.29".into(),
            guest_ip: "172.16.0.30; reboot".into(),
            prefix: 30,
        };

        let error =
            rebind_restored_guest_network(Path::new("missing.sock"), &allocation).unwrap_err();

        assert!(error.to_string().contains("parse restored guest IPv4"));
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

    #[test]
    fn integrity_manifest_authenticates_snapshot_metadata_ram_and_disk_chunks() {
        let dir = PathBuf::from(format!("target/snapshot-integrity-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ram_path = dir.join("snapshot.ram");
        let overlay_path = dir.join("snapshot.cow");
        let state = b"serialized-device-and-vcpu-state";
        let memory = vec![0x5a; tarit_proto::INTEGRITY_CHUNK_SIZE as usize * 2];
        let mut header = Vec::new();
        header.extend_from_slice(b"VMSN");
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&(state.len() as u64).to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&(memory.len() as u64).to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(header.len(), 32);
        let mut ram = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&ram_path)
            .unwrap();
        ram.write_all(&header).unwrap();
        ram.write_all(state).unwrap();
        ram.write_all(&memory).unwrap();
        ram.sync_all().unwrap();
        std::fs::write(&overlay_path, b"private disk upper").unwrap();
        std::fs::set_permissions(&overlay_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let artifacts = vec![
            OwnedArtifact::capture(&ram_path).unwrap(),
            OwnedArtifact::capture(&overlay_path).unwrap(),
        ];
        let manifest = snapshot_integrity_manifest(&artifacts, None).unwrap();
        assert_eq!(
            manifest
                .artifact(tarit_proto::ArtifactKind::SnapshotMetadata)
                .unwrap()
                .len,
            (header.len() + state.len()) as u64
        );
        let memory_integrity = manifest.artifact(tarit_proto::ArtifactKind::Ram).unwrap();
        assert_eq!(memory_integrity.chunk_hashes.len(), 2);
        assert!(manifest
            .artifact(tarit_proto::ArtifactKind::Overlay)
            .is_some());
        let encoded = manifest.encode().unwrap();
        assert_eq!(
            tarit_proto::IntegrityManifest::decode(&encoded).unwrap(),
            manifest
        );

        ram.write_all_at(b"tampered", (header.len() + state.len()) as u64)
            .unwrap();
        let tampered = hash_artifact_range(
            &ram,
            tarit_proto::ArtifactKind::Ram,
            (header.len() + state.len()) as u64,
            memory.len() as u64,
            u64::from(tarit_proto::INTEGRITY_CHUNK_SIZE),
        )
        .unwrap();
        assert_ne!(tampered.chunk_hashes, memory_integrity.chunk_hashes);

        drop(artifacts);
        drop(ram);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn precomputed_live_integrity_is_layout_bound_and_preserves_disk_verification() {
        let dir = PathBuf::from(format!(
            "target/precomputed-live-integrity-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ram_path = dir.join("snapshot.ram");
        let overlay_path = dir.join("snapshot.cow");
        let sidecar_path = dir.join("snapshot.precomputed");
        let state = b"live-state";
        let memory = vec![0xa5; tarit_proto::INTEGRITY_CHUNK_SIZE as usize + 17];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"VMSN");
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&(state.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(memory.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(state);
        bytes.extend_from_slice(&memory);
        std::fs::write(&ram_path, &bytes).unwrap();
        std::fs::set_permissions(&ram_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&overlay_path, b"disk-upper").unwrap();
        std::fs::set_permissions(&overlay_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let ram_artifact = OwnedArtifact::capture(&ram_path).unwrap();
        let generated =
            snapshot_integrity_manifest(std::slice::from_ref(&ram_artifact), None).unwrap();
        TEST_RAM_INTEGRITY_HASH_PASSES.with(|passes| passes.set(0));
        std::fs::write(&sidecar_path, generated.encode().unwrap()).unwrap();
        std::fs::set_permissions(&sidecar_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let sidecar = OwnedArtifact::capture(&sidecar_path).unwrap();
        let artifacts = vec![ram_artifact, OwnedArtifact::capture(&overlay_path).unwrap()];
        let adopted = snapshot_integrity_manifest(&artifacts, Some(&sidecar)).unwrap();
        TEST_RAM_INTEGRITY_HASH_PASSES.with(|passes| {
            assert_eq!(
                passes.get(),
                0,
                "precomputed live integrity must avoid a second RAM hash pass"
            );
        });
        assert_eq!(
            adopted.artifact(tarit_proto::ArtifactKind::Ram).unwrap(),
            generated.artifact(tarit_proto::ArtifactKind::Ram).unwrap()
        );
        assert!(adopted
            .artifact(tarit_proto::ArtifactKind::Overlay)
            .is_some());

        OpenOptions::new()
            .write(true)
            .open(&ram_path)
            .unwrap()
            .write_all_at(b"X", 4)
            .unwrap();
        let error = snapshot_integrity_manifest(&artifacts, Some(&sidecar)).unwrap_err();
        assert!(error
            .to_string()
            .contains("does not match snapshot metadata"));

        drop(artifacts);
        drop(sidecar);
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

    #[test]
    fn published_snapshot_is_gc_protected_until_registration_finishes() {
        let root = PathBuf::from(format!(
            "target/snapshot-publication-registry-{}",
            Uuid::new_v4()
        ));
        let mut config = supervisor_config(&root);
        config.disk_pressure.artifact_min_age_secs = 0;
        let supervisor = VmmSupervisor::new(config);
        let staging = supervisor.snapshot_ram_staging_path();
        let destination = supervisor.snapshot_ram_path();
        let artifact = OwnedArtifact::create_private(&staging).unwrap();
        artifact._file.write_all_at(b"snapshot", 0).unwrap();
        artifact._file.sync_all().unwrap();
        let mut publications = vec![(artifact, destination.clone())];

        let registered = supervisor.publish_artifacts(&mut publications).unwrap();
        let report = supervisor
            .sweep_owned_artifacts(ArtifactReferences::default())
            .unwrap();
        assert_eq!(report.removed_files, 0);
        assert!(destination.exists());
        SnapshotBundle {
            snapshot_path: destination.display().to_string(),
            overlay_path: None,
            live_stats: None,
            artifacts: publications
                .into_iter()
                .map(|(artifact, _)| artifact)
                .collect(),
            precomputed_integrity: None,
            in_progress_artifacts: Arc::clone(&supervisor.in_progress_artifacts),
            registered_paths: registered.clone(),
        }
        .persist();
        assert!(
            supervisor
                .in_progress_artifacts
                .lock()
                .unwrap()
                .contains(&destination),
            "durable publication fence must survive a GC pass with stale metadata"
        );

        {
            let mut in_progress = supervisor.in_progress_artifacts.lock().unwrap();
            for path in registered {
                in_progress.remove(&path);
            }
        }
        let report = supervisor
            .sweep_owned_artifacts(ArtifactReferences::default())
            .unwrap();
        assert_eq!(report.removed_files, 1);
        assert!(!destination.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
