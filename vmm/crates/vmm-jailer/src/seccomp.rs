//! seccomp-BPF profiles via `seccompiler`.
//!
//! Two profiles: one for vCPU threads (KVM_RUN path — needs ioctls), one for
//! the device/event thread (epoll, read/write on tap/fd).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadKind {
    Vcpu,
    Device,
    Block,
    Vsock,
}
/// `TUNSETOFFLOAD` (`_IOW('T', 208, unsigned int)`): the one TAP ioctl the
/// network worker may issue, to apply the offloads its guest negotiated.
pub const TUNSETOFFLOAD: u64 = 0x4004_54d0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeccompProfile {
    pub kind: ThreadKind,
    pub allow: Vec<String>,
}

impl SeccompProfile {
    pub fn vcpu() -> Self {
        Self {
            kind: ThreadKind::Vcpu,
            allow: vec![
                "ioctl".into(),
                "read".into(),
                "write".into(),
                "futex".into(),
                "clock_gettime".into(),
                "rt_sigreturn".into(),
                "mmap".into(),
                "munmap".into(),
                "brk".into(),
                "close".into(),
                "dup".into(),
                "exit".into(),
                "exit_group".into(),
                // vCPU pause-spin + idle HLT both use `thread::sleep`,
                // which compiles to nanosleep (or clock_nanosleep on
                // newer glibc). Without these the first pause()/HLT
                // after seccomp install kills the vCPU thread with SIGSYS.
                "nanosleep".into(),
                "clock_nanosleep".into(),
                // glibc lazy-loads thread-local storage and may madvise
                // the stack on the first `thread::sleep` call.
                "madvise".into(),
                // A restored guest's virtio-balloon MMIO notification is
                // processed on this thread. Lazy-page discard checks
                // residency before MADV_DONTNEED so it never waits on an
                // unresolved UFFD fault.
                "mincore".into(),
                // Rust's owned-fd drop path checks that the KVM vCPU fd is
                // still valid with fcntl(F_GETFD) immediately before close.
                // Permit only that read-only query; general fcntl operations
                // (duplication, flag mutation, and locking) stay denied.
                "fcntl".into(),
                // Rust runtime + glibc need these during normal operation and
                // especially during a panic unwind: the stack-overflow guard
                // (sigaltstack), TLS/guard pages (mprotect/mremap), signal setup
                // (rt_sigaction/rt_sigprocmask), spin backoff (sched_yield),
                // signal-interrupted syscall restart, RNG seeding, and gettid.
                // Without them the first such call after seccomp install kills
                // the vCPU thread with SIGSYS, orphaning the guest. Even a
                // minimal vCPU allowlist needs this signal/scheduling set; none
                // of these open new fds, map guest RAM, or add IPC surface.
                "sigaltstack".into(),
                "mprotect".into(),
                "rt_sigaction".into(),
                "rt_sigprocmask".into(),
                "sched_yield".into(),
                "restart_syscall".into(),
                "mremap".into(),
                "getrandom".into(),
                "gettid".into(),
            ],
        }
    }

    pub fn device() -> Self {
        Self {
            kind: ThreadKind::Device,
            allow: vec![
                "epoll_create1".into(),
                "epoll_ctl".into(),
                "epoll_wait".into(),
                "epoll_pwait".into(),
                "poll".into(),
                "ppoll".into(),
                "read".into(),
                "write".into(),
                "readv".into(),
                "writev".into(),
                "recvfrom".into(),
                "sendto".into(),
                "recvmsg".into(),
                "sendmsg".into(),
                "eventfd2".into(),
                "futex".into(),
                "close".into(),
                "dup".into(),
                "mmap".into(),
                "munmap".into(),
                "mprotect".into(),
                "mremap".into(),
                "rt_sigreturn".into(),
                "rt_sigaction".into(),
                "rt_sigprocmask".into(),
                "sigaltstack".into(),
                "exit".into(),
                "exit_group".into(),
                "nanosleep".into(),
                "clock_nanosleep".into(),
                "sched_yield".into(),
                "restart_syscall".into(),
                "getrandom".into(),
                "madvise".into(),
                "brk".into(),
                "gettid".into(),
                "getpid".into(),
                "clock_gettime".into(),
                // Only TUNSETOFFLOAD; see rules_for_syscall.
                "ioctl".into(),
            ],
        }
    }

    /// Block queue worker. It can poll eventfds and perform positional I/O or
    /// durable flushes on descriptors opened before confinement, but cannot
    /// create sockets, open paths, issue ioctls, or use network syscalls.
    pub fn block() -> Self {
        Self {
            kind: ThreadKind::Block,
            allow: vec![
                "poll".into(),
                "ppoll".into(),
                "read".into(),
                "write".into(),
                "pread64".into(),
                "pwrite64".into(),
                "lseek".into(),
                "fdatasync".into(),
                "fsync".into(),
                "futex".into(),
                "close".into(),
                "mmap".into(),
                "munmap".into(),
                "mprotect".into(),
                "mremap".into(),
                "rt_sigreturn".into(),
                "rt_sigaction".into(),
                "rt_sigprocmask".into(),
                "sigaltstack".into(),
                "exit".into(),
                "exit_group".into(),
                "nanosleep".into(),
                "clock_nanosleep".into(),
                "sched_yield".into(),
                "restart_syscall".into(),
                "getrandom".into(),
                "madvise".into(),
                "brk".into(),
                "gettid".into(),
                "getpid".into(),
                "clock_gettime".into(),
            ],
        }
    }

    /// Device profile for the virtio-vsock pump. Unlike the network data
    /// plane, this thread must lazily create host Unix streams. Its `socket`
    /// syscall is argument-filtered to AF_UNIX/SOCK_STREAM at compile time.
    pub fn vsock() -> Self {
        let mut profile = Self::device();
        profile.kind = ThreadKind::Vsock;
        profile.allow.extend(
            [
                "socket",
                "connect",
                "fcntl",
                "getsockopt",
                "setsockopt",
                "ioctl",
            ]
            .into_iter()
            .map(str::to_owned),
        );
        profile
    }
}

/// Path-opening syscalls that confined threads must never complete, but that
/// glibc issues lazily from inside malloc/free (for example `free()` reads
/// /proc/sys/vm/overcommit_memory the first time any thread trims a non-main
/// arena heap). Killing the thread there leaves allocator and device mutexes
/// held forever and silently deadlocks the VM, so these fail with EACCES
/// instead. glibc treats the failed open as "use the conservative default".
#[cfg(target_os = "linux")]
const PATH_OPEN_SYSCALLS: [i64; 3] = [libc::SYS_open, libc::SYS_openat, libc::SYS_openat2];

#[cfg(target_os = "linux")]
impl SeccompProfile {
    /// Install the seccomp filter for this profile.
    /// Must be called from the thread that will be filtered.
    pub fn install(&self) -> Result<(), String> {
        use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};
        use std::convert::TryInto;

        let arch: seccompiler::TargetArch = std::env::consts::ARCH
            .try_into()
            .map_err(|e| format!("arch: {e:?}"))?;

        let mut rules: std::collections::BTreeMap<i64, Vec<seccompiler::SeccompRule>> =
            std::collections::BTreeMap::new();

        for name in &self.allow {
            let nr = self.syscall_nr(name)?;
            rules.insert(nr, self.rules_for_syscall(name)?);
        }
        // Let path opens through this allowlist only so the errno filter below
        // decides them. The kernel applies the most restrictive verdict across
        // stacked filters, and ERRNO outranks ALLOW, so they never succeed.
        for nr in PATH_OPEN_SYSCALLS {
            rules.entry(nr).or_default();
        }

        let filter: BpfProgram =
            SeccompFilter::new(rules, SeccompAction::KillThread, SeccompAction::Allow, arch)
                .map_err(|e| format!("seccomp filter: {e}"))?
                .try_into()
                .map_err(|e| format!("seccomp BPF: {e}"))?;

        let deny_paths: BpfProgram = SeccompFilter::new(
            PATH_OPEN_SYSCALLS
                .into_iter()
                .map(|nr| (nr, Vec::new()))
                .collect(),
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EACCES as u32),
            arch,
        )
        .map_err(|e| format!("seccomp path filter: {e}"))?
        .try_into()
        .map_err(|e| format!("seccomp path BPF: {e}"))?;

        // Install the errno filter first so there is no window in which the
        // allowlist admits a path open on its own.
        seccompiler::apply_filter(&deny_paths).map_err(|e| format!("seccomp path apply: {e}"))?;
        seccompiler::apply_filter(&filter).map_err(|e| format!("seccomp apply: {e}"))?;

        log::info!(
            "seccomp: installed {} profile ({} syscalls allowed)",
            match self.kind {
                ThreadKind::Vcpu => "vCPU",
                ThreadKind::Device => "device",
                ThreadKind::Block => "block",
                ThreadKind::Vsock => "vsock",
            },
            self.allow.len()
        );
        Ok(())
    }

    fn rules_for_syscall(&self, name: &str) -> Result<Vec<seccompiler::SeccompRule>, String> {
        use seccompiler::{SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompRule};

        let conditions = match (self.kind, name) {
            // Linux ioctl request numbers encode the subsystem in bits 8..15.
            // The vCPU owns only its VcpuFd, so allowing KVM-family requests is
            // sufficient for KVM_RUN and the KVM_GET/SET state used by pause,
            // snapshot, and restore, while rejecting arbitrary host ioctls.
            (ThreadKind::Vcpu, "ioctl") => vec![SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::MaskedEq(0xff00),
                0xae00,
            )
            .map_err(|e| format!("ioctl condition: {e}"))?],
            (ThreadKind::Vcpu, "fcntl") => vec![SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                libc::F_GETFD as u64,
            )
            .map_err(|e| format!("fcntl condition: {e}"))?],
            // Rust may OR CLOEXEC/NONBLOCK into the socket type, so mask only
            // the low socket-type nibble and separately require AF_UNIX and
            // protocol 0. This prevents the guest-facing pump from creating
            // Internet sockets even if it is compromised.
            (ThreadKind::Vsock, "socket") => vec![
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Eq,
                    libc::AF_UNIX as u64,
                )
                .map_err(|e| format!("socket domain condition: {e}"))?,
                SeccompCondition::new(
                    1,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::MaskedEq(0xf),
                    libc::SOCK_STREAM as u64,
                )
                .map_err(|e| format!("socket type condition: {e}"))?,
                SeccompCondition::new(2, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, 0)
                    .map_err(|e| format!("socket protocol condition: {e}"))?,
            ],
            // The network worker applies the offloads a guest negotiated to
            // its TAP. Allow exactly TUNSETOFFLOAD, which only changes the
            // offload flags of a TAP fd (and fails on any other fd).
            (ThreadKind::Device, "ioctl") => vec![SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                TUNSETOFFLOAD,
            )
            .map_err(|e| format!("ioctl condition: {e}"))?],
            // std's UnixStream::set_nonblocking issues ioctl(FIONBIO) on Linux;
            // the pump calls it for every lazily connected host stream. Allow
            // exactly that request and nothing else. The cast is kept because
            // libc types FIONBIO differently across gnu (c_ulong) and musl
            // (c_int) targets.
            #[allow(clippy::unnecessary_cast)]
            (ThreadKind::Vsock, "ioctl") => vec![SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                libc::FIONBIO as u64,
            )
            .map_err(|e| format!("ioctl condition: {e}"))?],
            _ => return Ok(Vec::new()),
        };
        Ok(vec![
            SeccompRule::new(conditions).map_err(|e| format!("seccomp rule: {e}"))?
        ])
    }

    fn syscall_nr(&self, name: &str) -> Result<i64, String> {
        match name {
            "ioctl" => Ok(libc::SYS_ioctl),
            "read" => Ok(libc::SYS_read),
            "write" => Ok(libc::SYS_write),
            "futex" => Ok(libc::SYS_futex),
            "clock_gettime" => Ok(libc::SYS_clock_gettime),
            "rt_sigreturn" => Ok(libc::SYS_rt_sigreturn),
            "mmap" => Ok(libc::SYS_mmap),
            "munmap" => Ok(libc::SYS_munmap),
            "brk" => Ok(libc::SYS_brk),
            "close" => Ok(libc::SYS_close),
            "dup" => Ok(libc::SYS_dup),
            "exit" => Ok(libc::SYS_exit),
            "exit_group" => Ok(libc::SYS_exit_group),
            "epoll_create1" => Ok(libc::SYS_epoll_create1),
            "epoll_wait" => Ok(libc::SYS_epoll_wait),
            "epoll_pwait" => Ok(libc::SYS_epoll_pwait),
            "epoll_ctl" => Ok(libc::SYS_epoll_ctl),
            "poll" => Ok(libc::SYS_poll),
            "ppoll" => Ok(libc::SYS_ppoll),
            "eventfd2" => Ok(libc::SYS_eventfd2),
            "pread64" => Ok(libc::SYS_pread64),
            "pwrite64" => Ok(libc::SYS_pwrite64),
            "recvfrom" => Ok(libc::SYS_recvfrom),
            "sendto" => Ok(libc::SYS_sendto),
            "recvmsg" => Ok(libc::SYS_recvmsg),
            "sendmsg" => Ok(libc::SYS_sendmsg),
            "rt_sigaction" => Ok(libc::SYS_rt_sigaction),
            "rt_sigprocmask" => Ok(libc::SYS_rt_sigprocmask),
            "alarm" => Ok(libc::SYS_alarm),
            "nanosleep" => Ok(libc::SYS_nanosleep),
            "clock_nanosleep" => Ok(libc::SYS_clock_nanosleep),
            "madvise" => Ok(libc::SYS_madvise),
            "mincore" => Ok(libc::SYS_mincore),
            "lseek" => Ok(libc::SYS_lseek),
            "fdatasync" => Ok(libc::SYS_fdatasync),
            "fsync" => Ok(libc::SYS_fsync),
            "fstat" => Ok(libc::SYS_fstat),
            "openat" => Ok(libc::SYS_openat),
            "readv" => Ok(libc::SYS_readv),
            "writev" => Ok(libc::SYS_writev),
            "sigaltstack" => Ok(libc::SYS_sigaltstack),
            "mprotect" => Ok(libc::SYS_mprotect),
            "sched_yield" => Ok(libc::SYS_sched_yield),
            "restart_syscall" => Ok(libc::SYS_restart_syscall),
            "mremap" => Ok(libc::SYS_mremap),
            "getrandom" => Ok(libc::SYS_getrandom),
            "gettid" => Ok(libc::SYS_gettid),
            "getpid" => Ok(libc::SYS_getpid),
            "tgkill" => Ok(libc::SYS_tgkill),
            "tkill" => Ok(libc::SYS_tkill),
            "socket" => Ok(libc::SYS_socket),
            "connect" => Ok(libc::SYS_connect),
            "fcntl" => Ok(libc::SYS_fcntl),
            "getsockopt" => Ok(libc::SYS_getsockopt),
            "setsockopt" => Ok(libc::SYS_setsockopt),
            _ => Err(format!("unknown syscall: {name}")),
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl SeccompProfile {
    pub fn install(&self) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vcpu_profile_has_vcpu_kind() {
        let p = SeccompProfile::vcpu();
        assert_eq!(p.kind, ThreadKind::Vcpu);
    }

    #[test]
    fn device_profile_has_device_kind() {
        let p = SeccompProfile::device();
        assert_eq!(p.kind, ThreadKind::Device);
    }

    #[test]
    fn block_profile_has_no_path_network_or_ioctl_authority() {
        let profile = SeccompProfile::block();
        assert_eq!(profile.kind, ThreadKind::Block);
        for denied in [
            "open", "openat", "socket", "connect", "ioctl", "recvfrom", "sendto",
        ] {
            assert!(!profile.allow.contains(&denied.to_string()), "{denied}");
        }
        for required in [
            "poll",
            "read",
            "write",
            "pread64",
            "pwrite64",
            "lseek",
            "fdatasync",
            "fsync",
        ] {
            assert!(profile.allow.contains(&required.to_string()), "{required}");
        }
    }

    #[test]
    fn vsock_profile_is_the_only_device_profile_with_socket_creation() {
        let device = SeccompProfile::device();
        let vsock = SeccompProfile::vsock();
        assert_eq!(vsock.kind, ThreadKind::Vsock);
        assert!(!device.allow.contains(&"socket".to_string()));
        assert!(!device.allow.contains(&"connect".to_string()));
        // The network worker may issue one ioctl, TUNSETOFFLOAD (below).
        assert!(device.allow.contains(&"ioctl".to_string()));
        assert!(vsock.allow.contains(&"socket".to_string()));
        assert!(vsock.allow.contains(&"connect".to_string()));
        // set_nonblocking on lazily connected host streams needs ioctl(FIONBIO).
        assert!(vsock.allow.contains(&"ioctl".to_string()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sensitive_syscalls_compile_to_argument_filtered_rules() {
        assert_eq!(
            SeccompProfile::vcpu()
                .rules_for_syscall("ioctl")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            SeccompProfile::vcpu()
                .rules_for_syscall("fcntl")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            SeccompProfile::vsock()
                .rules_for_syscall("socket")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            SeccompProfile::vsock()
                .rules_for_syscall("ioctl")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            SeccompProfile::device()
                .rules_for_syscall("ioctl")
                .unwrap()
                .len(),
            1
        );
        assert!(SeccompProfile::device()
            .rules_for_syscall("read")
            .unwrap()
            .is_empty());
    }

    /// Under the network worker's profile TUNSETOFFLOAD reaches the kernel:
    /// on a non-TAP fd it fails with ENOTTY and the thread keeps running.
    #[cfg(target_os = "linux")]
    #[test]
    fn device_profile_allows_tunsetoffload() {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors pipe2 returns.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let errno = std::thread::spawn(move || {
            SeccompProfile::device().install().expect("install profile");
            // SAFETY: TUNSETOFFLOAD takes its flags by value; on a pipe the
            // kernel rejects it without side effects.
            let rc = unsafe { libc::ioctl(fds[0], TUNSETOFFLOAD as _, 0 as libc::c_ulong) };
            assert_eq!(rc, -1);
            std::io::Error::last_os_error().raw_os_error()
        })
        .join()
        .expect("network worker profile must allow TUNSETOFFLOAD");
        assert_eq!(errno, Some(libc::ENOTTY));
        for fd in fds {
            // SAFETY: both descriptors came from pipe2 above.
            unsafe { libc::close(fd) };
        }
    }

    /// A confined thread that opens a path (as glibc does lazily inside
    /// free()) must get EACCES and keep running, not be killed while it may
    /// hold locks other threads need.
    #[cfg(target_os = "linux")]
    #[test]
    fn confined_threads_survive_path_opens_with_eacces() {
        for profile in [
            SeccompProfile::vcpu(),
            SeccompProfile::device(),
            SeccompProfile::block(),
            SeccompProfile::vsock(),
        ] {
            let kind = profile.kind;
            let result = std::thread::spawn(move || {
                profile.install().expect("install profile");
                let path = c"/proc/sys/vm/overcommit_memory";
                // SAFETY: `path` is a valid NUL-terminated C string.
                let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
                let open_errno = std::io::Error::last_os_error().raw_os_error();
                // SAFETY: as above; AT_FDCWD resolves relative to the cwd.
                let fd_at = unsafe {
                    libc::openat(
                        libc::AT_FDCWD,
                        path.as_ptr(),
                        libc::O_RDONLY | libc::O_CLOEXEC,
                    )
                };
                let openat_errno = std::io::Error::last_os_error().raw_os_error();
                (fd, open_errno, fd_at, openat_errno)
            })
            .join()
            .unwrap_or_else(|_| panic!("{kind:?} thread did not survive a path open"));
            assert_eq!(result.0, -1, "{kind:?} open must fail");
            assert_eq!(result.1, Some(libc::EACCES), "{kind:?} open errno");
            assert_eq!(result.2, -1, "{kind:?} openat must fail");
            assert_eq!(result.3, Some(libc::EACCES), "{kind:?} openat errno");
        }
    }

    #[test]
    fn thread_kind_eq() {
        assert_eq!(ThreadKind::Vcpu, ThreadKind::Vcpu);
        assert_ne!(ThreadKind::Vcpu, ThreadKind::Device);
    }

    #[test]
    fn profile_round_trips_json() {
        let p = SeccompProfile::vcpu();
        let s = serde_json::to_string(&p).unwrap();
        let back: SeccompProfile = serde_json::from_str(&s).unwrap();
        assert_eq!(back.kind, p.kind);
        assert_eq!(back.allow.len(), p.allow.len());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn vcpu_profile_covers_kvm_and_lazy_balloon_syscalls() {
        let p = SeccompProfile::vcpu();
        for syscall in ["ioctl", "fcntl", "madvise", "mincore"] {
            assert!(p.allow.contains(&syscall.to_string()), "{syscall}");
            assert!(p.syscall_nr(syscall).is_ok(), "{syscall}");
        }
    }

    #[test]
    fn device_profile_allows_epoll() {
        let p = SeccompProfile::device();
        assert!(p.allow.contains(&"epoll_wait".to_string()));
    }

    #[test]
    fn device_profile_covers_guest_facing_io_loop_syscalls() {
        let p = SeccompProfile::device();
        for syscall in [
            "epoll_create1",
            "epoll_ctl",
            "epoll_wait",
            "epoll_pwait",
            "poll",
            "ppoll",
            "read",
            "write",
            "readv",
            "writev",
            "recvfrom",
            "sendto",
            "recvmsg",
            "sendmsg",
            "eventfd2",
            "futex",
            "close",
            "dup",
            "mmap",
            "munmap",
            "mprotect",
            "mremap",
            "rt_sigreturn",
            "rt_sigaction",
            "rt_sigprocmask",
            "sigaltstack",
            "exit",
            "exit_group",
            "nanosleep",
            "clock_nanosleep",
            "sched_yield",
            "restart_syscall",
            "getrandom",
            "madvise",
            "brk",
            "gettid",
            "getpid",
            "clock_gettime",
        ] {
            assert!(p.allow.contains(&syscall.to_string()), "{syscall}");
        }
    }
}
