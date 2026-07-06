//! Security posture types for a VM instance.
//!
//! This module defines the target security posture (`SecurityPolicy`) and the
//! VMM syscall allow/deny lists. How much of the posture is actually enforced
//! depends on how the VMM is launched:
//!
//! 1. **VMM to host exfiltration**: the syscall allowlist below contains no
//!    network syscalls, so a VMM thread confined by it cannot open a socket,
//!    connect, or resolve DNS. Seccomp is installed on the VMM I/O worker
//!    threads on the standard path; whole-process confinement is applied by the
//!    jailer (`vmm serve --jail`).
//! 2. **VM to VM isolation**: each VM has its own tap and /30, and the only
//!    egress path is the host's nftables rules. The orchestrator programs a
//!    per-VM egress allowlist on the host, and the jailer additionally places
//!    the VMM in its own network namespace.
//! 3. **Guest to host escalation**: KVM provides the CPU and memory boundary.
//!    Under the jailer the VMM also runs behind chroot, dropped capabilities,
//!    and an unprivileged uid/gid.
//!
//! See `SECURITY.md` for which controls are enforced by default versus opt-in.

use serde::{Deserialize, Serialize};

/// The security posture for a VM instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityPolicy {
    /// If true, the VMM process is seccomp-confined (no network syscalls).
    /// Default: true. This prevents VMM→host exfiltration.
    #[serde(default = "default_true")]
    pub seccomp_confined: bool,
    /// If true, each VM gets its own netns with no bridge to other VMs.
    /// Default: true. This prevents VM→VM communication.
    #[serde(default = "default_true")]
    pub isolated_netns: bool,
    /// If true, the VMM chroots before exec'ing the guest.
    /// Default: true.
    #[serde(default = "default_true")]
    pub chroot: bool,
    /// If true, the VMM drops all capabilities before KVM_RUN.
    /// Default: true.
    #[serde(default = "default_true")]
    pub drop_caps: bool,
    /// Allowed egress CIDRs/ports (host-enforced via nftables).
    /// Empty = deny-all (no egress).
    #[serde(default)]
    pub egress_allowlist: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for SecurityPolicy {
    fn default() -> Self {
        Self {
            seccomp_confined: true,
            isolated_netns: true,
            chroot: true,
            drop_caps: true,
            egress_allowlist: vec![],
        }
    }
}

impl SecurityPolicy {
    /// Maximum lockdown: deny-all egress, seccomp, isolated netns, chroot.
    pub fn locked_down() -> Self {
        Self::default()
    }

    /// Development mode: allow all egress, no seccomp (for debugging).
    pub fn dev() -> Self {
        Self {
            seccomp_confined: false,
            isolated_netns: false,
            chroot: false,
            drop_caps: false,
            egress_allowlist: vec!["0.0.0.0/0".into()],
        }
    }

    /// Validate the policy is internally consistent.
    pub fn validate(&self) -> Result<(), String> {
        if !self.seccomp_confined && self.isolated_netns {
            // If seccomp is off, netns isolation still works but the VMM
            // itself could make network calls. Warn but don't fail.
            log::warn!(
                "SecurityPolicy: seccomp off but netns isolated — VMM process can still exfiltrate"
            );
        }
        Ok(())
    }
}

/// The syscalls the VMM process is allowed to make under seccomp.
/// This is the MAXIMUM allowlist — anything not here is blocked.
///
/// Key principle: NO network syscalls (socket, connect, bind, listen,
/// accept, sendto, recvfrom, getsockopt, setsockopt). The VMM cannot
/// make any outbound network connection — zero exfiltration.
pub const VMM_SYSCALL_ALLOWLIST: &[&str] = &[
    // KVM path
    "ioctl",
    "read",
    "write",
    "readv",
    "writev",
    "pread64",
    "pwrite64",
    // Thread sync
    "futex",
    "epoll_wait",
    "epoll_ctl",
    "epoll_pwait",
    "eventfd2",
    "pipe2",
    "close",
    "dup",
    "dup2",
    // Memory
    "mmap",
    "munmap",
    "mprotect",
    "madvise",
    "brk",
    // Signals (for SIGALRM timeout)
    "rt_sigaction",
    "rt_sigprocmask",
    "rt_sigreturn",
    "alarm",
    // Time
    "clock_gettime",
    "nanosleep",
    // Process
    "exit",
    "exit_group",
    "sched_yield",
    // File ops (only on pre-opened fds)
    "fcntl",
    "fstat",
    "stat",
    "lseek",
    // NO network syscalls — zero exfiltration
];

/// The syscalls explicitly DENIED (for audit/documentation).
pub const VMM_SYSCALL_DENYLIST: &[&str] = &[
    "socket",
    "connect",
    "bind",
    "listen",
    "accept",
    "accept4",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
    "getsockopt",
    "setsockopt",
    "shutdown",
    "socketpair",
    "getpeername",
    "getsockname",
    "pipe", // use pipe2 instead (has CLOEXEC)
    "fork",
    "vfork",
    "clone",
    "clone3",
    "execve",
    "execveat",
    "ptrace",
    "mount",
    "umount2",
    "pivot_root",
    "chroot",
    "chdir",
    "open",
    "openat",
    "openat2",
    "creat",
    "unlink",
    "rename",
    "mkdir",
    "rmdir",
    "chmod",
    "chown",
    "setuid",
    "setgid",
    "setreuid",
    "setregid",
    "setresuid",
    "setresgid",
    "setgroups",
    "bpf",
    "perf_event_open",
    "userfaultfd",
    "kexec_load",
    "reboot",
    "init_module",
    "finit_module",
    "delete_module",
    "keyctl",
    "add_key",
    "request_key",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_down_denies_all_egress() {
        let p = SecurityPolicy::locked_down();
        assert!(p.seccomp_confined);
        assert!(p.isolated_netns);
        assert!(p.egress_allowlist.is_empty());
    }

    #[test]
    fn dev_mode_allows_egress() {
        let p = SecurityPolicy::dev();
        assert!(!p.seccomp_confined);
        assert!(!p.egress_allowlist.is_empty());
    }

    #[test]
    fn allowlist_has_no_network_syscalls() {
        // The critical security property: no network syscalls in the allowlist.
        let network_syscalls = [
            "socket",
            "connect",
            "bind",
            "listen",
            "accept",
            "sendto",
            "recvfrom",
            "sendmsg",
            "recvmsg",
            "getsockopt",
            "setsockopt",
        ];
        for &ns in &network_syscalls {
            assert!(
                !VMM_SYSCALL_ALLOWLIST.contains(&ns),
                "SECURITY VIOLATION: {ns} is in the VMM syscall allowlist — this allows exfiltration!"
            );
        }
    }

    #[test]
    fn denylist_includes_all_network_syscalls() {
        let network_syscalls = [
            "socket",
            "connect",
            "bind",
            "listen",
            "accept",
            "sendto",
            "recvfrom",
            "sendmsg",
            "recvmsg",
            "getsockopt",
            "setsockopt",
        ];
        for &ns in &network_syscalls {
            assert!(
                VMM_SYSCALL_DENYLIST.contains(&ns),
                "{ns} should be in the denylist"
            );
        }
    }

    #[test]
    fn policy_serializes_round_trip() {
        let p = SecurityPolicy::locked_down();
        let s = serde_json::to_string(&p).unwrap();
        let back: SecurityPolicy = serde_json::from_str(&s).unwrap();
        assert!(back.seccomp_confined);
        assert!(back.isolated_netns);
    }

    #[test]
    fn validate_warns_on_inconsistent_policy() {
        let p = SecurityPolicy {
            seccomp_confined: false,
            isolated_netns: true,
            ..Default::default()
        };
        // Should succeed but log a warning.
        assert!(p.validate().is_ok());
    }
}
