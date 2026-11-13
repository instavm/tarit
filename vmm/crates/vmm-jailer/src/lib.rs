//! vmm-jailer: host-side confinement of the VMM process.
//!
//! Jailer wrapper: chroot, mount/UTS/IPC/network namespaces,
//! cgroups (CPU/mem/IO/PID limits), drop to unprivileged uid/gid,
//! `--resource-limit` style fd/file caps.
//!
//! seccomp-BPF via `seccompiler`: minimal per-thread syscall
//! allowlists for vCPU vs device threads. The guest pokes virtio queues = the
//! VMM processes attacker-controlled data with `unsafe` Rust, so a tight
//! syscall filter is essential.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod cgroups;
#[cfg(target_os = "linux")]
pub mod executor;
pub mod jailer;
pub mod profile;
pub mod seccomp;

#[cfg(target_os = "linux")]
pub use executor::{jail, launch_pid_namespace, PidNamespaceRole};
pub use jailer::Jailer;
pub use profile::{audit_profile, VmmSeccompProfiles};
pub use seccomp::{SeccompProfile, ThreadKind};

use std::sync::atomic::{AtomicBool, Ordering};

static ENTERED_JAIL: AtomicBool = AtomicBool::new(false);

/// Whether this process confined itself with `jail` (chroot, namespaces,
/// privilege drop all succeeded). Then `/` is the jail root that the
/// privileged launcher chose and prepared before starting the VMM. Only
/// `jail` sets this; nothing outside the process (environment, files) can.
pub fn entered_jail() -> bool {
    ENTERED_JAIL.load(Ordering::Acquire)
}

#[cfg(target_os = "linux")]
fn mark_entered_jail() {
    ENTERED_JAIL.store(true, Ordering::Release);
}
