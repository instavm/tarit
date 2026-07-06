//! seccomp-BPF profiles via `seccompiler` (PRD §10).
//!
//! Two profiles: one for vCPU threads (KVM_RUN path — needs ioctls), one for
//! the device/event thread (epoll, read/write on tap/fd).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadKind {
    Vcpu,
    Device,
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
                // the vCPU thread with SIGSYS, orphaning the guest. Firecracker's
                // vCPU filter allows the same signal/scheduling set; none of
                // these open new fds, map guest RAM, or add IPC surface.
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
                "ioctl".into(),
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
                // The vsock transport can lazily connect a configured Unix
                // socket listener in response to a guest REQUEST. Keep the
                // profile conservative but complete for that current code path.
                "socket".into(),
                "connect".into(),
                "fcntl".into(),
                "getsockopt".into(),
                "setsockopt".into(),
            ],
        }
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
            rules.insert(nr, vec![]);
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
            },
            self.allow.len()
        );
        Ok(())
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
            "ioctl",
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
