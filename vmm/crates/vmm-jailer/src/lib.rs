//! vmm-jailer: host-side confinement of the VMM process.
//!
//! Jailer wrapper: chroot, PID/mount/network/user namespaces,
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
pub use executor::jail;
pub use jailer::Jailer;
pub use profile::{audit_profile, VmmSeccompProfiles};
pub use seccomp::{SeccompProfile, ThreadKind};
