//! seccomp-BPF profiles via `seccompiler`.
//!
//! Two profiles: one for vCPU threads (KVM_RUN path — needs ioctls), one for
//! the device/event thread (epoll, read/write on tap/fd).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadKind {
    Vcpu,
    Device,
    Vsock,
}

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
                // virtio-blk MMIO exits are handled in the vCPU thread.
                // The backend does pread64/pwrite64/lseek for file I/O.
                "pread64".into(),
                "pwrite64".into(),
                "lseek".into(),
                "fdatasync".into(),
                // CoW-overlay and plain-blk FLUSH call File::sync_all() = fsync
                // for durability; without it the first FLUSH on a write-heavy
                // guest rootfs kills the vCPU thread with SIGSYS (seccomp).
                "fsync".into(),
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
            ["socket", "connect", "fcntl", "getsockopt", "setsockopt"]
                .into_iter()
                .map(str::to_owned),
        );
        profile
    }
}

#[cfg(target_os = "linux")]
impl SeccompProfile {
    /// Install the seccomp filter for this profile.
    /// Must be called from the thread that will be filtered.
    pub fn install(&self) -> Result<(), String> {
        use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};
        use std::convert::TryInto;

        let mut rules: std::collections::BTreeMap<i64, Vec<seccompiler::SeccompRule>> =
            std::collections::BTreeMap::new();

        for name in &self.allow {
            let nr = self.syscall_nr(name)?;
            rules.insert(nr, self.rules_for_syscall(name)?);
        }

        let filter: BpfProgram = SeccompFilter::new(
            rules,
            SeccompAction::KillThread,
            SeccompAction::Allow,
            std::env::consts::ARCH
                .try_into()
                .map_err(|e| format!("arch: {e:?}"))?,
        )
        .map_err(|e| format!("seccomp filter: {e}"))?
        .try_into()
        .map_err(|e| format!("seccomp BPF: {e}"))?;

        seccompiler::apply_filter(&filter).map_err(|e| format!("seccomp apply: {e}"))?;

        log::info!(
            "seccomp: installed {} profile ({} syscalls allowed)",
            match self.kind {
                ThreadKind::Vcpu => "vCPU",
                ThreadKind::Device => "device",
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
    fn vsock_profile_is_the_only_device_profile_with_socket_creation() {
        let device = SeccompProfile::device();
        let vsock = SeccompProfile::vsock();
        assert_eq!(vsock.kind, ThreadKind::Vsock);
        assert!(!device.allow.contains(&"socket".to_string()));
        assert!(!device.allow.contains(&"connect".to_string()));
        assert!(!device.allow.contains(&"ioctl".to_string()));
        assert!(vsock.allow.contains(&"socket".to_string()));
        assert!(vsock.allow.contains(&"connect".to_string()));
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
            SeccompProfile::vsock()
                .rules_for_syscall("socket")
                .unwrap()
                .len(),
            1
        );
        assert!(SeccompProfile::device()
            .rules_for_syscall("read")
            .unwrap()
            .is_empty());
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
    fn vcpu_profile_allows_ioctl() {
        let p = SeccompProfile::vcpu();
        assert!(p.allow.contains(&"ioctl".to_string()));
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
