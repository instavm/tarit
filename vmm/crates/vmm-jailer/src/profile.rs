//! seccomp-BPF profile compilation (PRD §10, §12.6).
//!
//! PRD §10: "seccomp-BPF via `seccompiler` — minimal per-thread syscall
//! allowlists for vCPU vs device threads. The guest pokes virtio queues =
//! the VMM processes attacker-controlled data with `unsafe` Rust, so a
//! tight syscall filter is essential."
//!
//! PRD §12.6: "attempt disallowed syscalls from vCPU/device threads; assert
//! the process is killed/denied."
//!
//! This module owns the *profile data model* (which syscalls each thread
//! kind may call). The actual `seccompiler` BPF compilation is Linux-only;
//! on non-Linux the model is unit-testable as data.

use crate::seccomp::{SeccompProfile, ThreadKind};
use serde::{Deserialize, Serialize};

/// A pre-baked profile set for the whole VMM (vCPU thread + device thread).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmmSeccompProfiles {
    pub vcpu: SeccompProfile,
    pub device: SeccompProfile,
}

impl VmmSeccompProfiles {
    /// The minimal, conservative profile set for a microVM VMM.
    ///
    /// vCPU thread: the KVM_RUN path — ioctls, futex, signal return.
    /// Device thread: epoll, read/write on fds, eventfd, ioctl.
    ///
    /// Both threads share a small common set (rt_sigprocmask, sched_yield,
    /// futex, clock_gettime, rt_sigreturn, exit, exit_group). The real
    /// Firecracker profiles are tighter and kernel-version-dependent; this
    /// is the scaffold shape.
    pub fn minimal() -> Self {
        Self {
            vcpu: SeccompProfile::vcpu(),
            device: SeccompProfile::device(),
        }
    }

    /// Look up the profile for a thread kind.
    pub fn for_thread(&self, kind: ThreadKind) -> &SeccompProfile {
        match kind {
            ThreadKind::Vcpu => &self.vcpu,
            ThreadKind::Device => &self.device,
        }
    }
}

/// Validate that a profile's allowlist doesn't include obviously-dangerous
/// syscalls. Returns the list of *rejected* entries (empty = OK).
///
/// The denylist is the PRD §10 "tight syscall filter" intent: the jailed
/// VMM should never be able to fork, exec, ptrace, mount, or open by path
/// after seccomp is installed.
pub fn audit_profile(profile: &SeccompProfile) -> Vec<String> {
    let deny = [
        "execve",
        "execveat",
        "fork",
        "vfork",
        "clone",
        "clone3",
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
        "unlinkat",
        "rename",
        "renameat",
        "renameat2",
        "mkdir",
        "mkdirat",
        "rmdir",
        "symlink",
        "symlinkat",
        "link",
        "linkat",
        "mknod",
        "mknodat",
        "chmod",
        "fchmodat",
        "chown",
        "fchownat",
        "setuid",
        "setgid",
        "setreuid",
        "setregid",
        "setresuid",
        "setresgid",
        "setgroups",
        "keyctl",
        "add_key",
        "request_key",
        "perf_event_open",
        "bpf",
        "userfaultfd",
        "kexec_load",
        "reboot",
        "init_module",
        "finit_module",
        "delete_module",
    ];
    profile
        .allow
        .iter()
        .filter(|s| deny.contains(&s.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_profiles_are_nonempty() {
        let p = VmmSeccompProfiles::minimal();
        // On Linux the vcpu/device profiles carry real syscall allowlists;
        // on non-Linux they're empty stubs (seccomp is Linux-only).
        #[cfg(target_os = "linux")]
        {
            assert!(!p.vcpu.allow.is_empty());
            assert!(!p.device.allow.is_empty());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = p; // stub profiles are empty
        }
    }

    #[test]
    fn for_thread_dispatches_correctly() {
        let p = VmmSeccompProfiles::minimal();
        assert_eq!(p.for_thread(ThreadKind::Vcpu).kind, ThreadKind::Vcpu);
        assert_eq!(p.for_thread(ThreadKind::Device).kind, ThreadKind::Device);
    }

    #[test]
    fn audit_rejects_dangerous_syscalls() {
        let mut bad = SeccompProfile::vcpu();
        bad.allow.push("execve".into());
        bad.allow.push("ptrace".into());
        bad.allow.push("mount".into());
        let rejected = audit_profile(&bad);
        assert_eq!(rejected.len(), 3);
        assert!(rejected.contains(&"execve".to_string()));
        assert!(rejected.contains(&"ptrace".to_string()));
        assert!(rejected.contains(&"mount".to_string()));
    }

    #[test]
    fn audit_passes_clean_profile() {
        let p = SeccompProfile::vcpu();
        assert!(audit_profile(&p).is_empty());
    }

    #[test]
    fn profiles_serialize_round_trip() {
        let p = VmmSeccompProfiles::minimal();
        let s = serde_json::to_string(&p).unwrap();
        let back: VmmSeccompProfiles = serde_json::from_str(&s).unwrap();
        assert_eq!(back.vcpu.allow.len(), p.vcpu.allow.len());
    }
}
