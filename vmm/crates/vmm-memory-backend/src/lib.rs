//! vmm-memory-backend: guest physical memory abstraction.
//!
//! Built on `vm-memory` mmap'd regions. Provides:
//!  - [`GuestMemoryMmap`] wrapper (`GuestMemory`) — the substrate for both KVM
//!    `KVM_SET_USER_MEMORY_REGION` and for snapshot dumps.
//!  - dirty-page tracking plumbing (bitmap + dirty-ring) — Phase 5.
//!  - UFFD (userfaultfd) lazy-restore handler — Phase 3+.
//!
//! See PRD §6: "Memory is the long pole in both snapshot size and restore
//! latency. Design the memory backend first."

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod backend;
pub mod dirty;
pub mod uffd;

#[cfg(all(feature = "kvm", target_os = "linux"))]
pub mod kvm;

#[cfg(all(feature = "kvm", target_os = "linux"))]
pub mod kvm_dirty;

#[cfg(target_os = "linux")]
pub mod uffd_restore;

pub use backend::GuestMemory;
#[cfg(all(feature = "kvm", target_os = "linux"))]
pub use kvm_dirty::{read_dirty_log, KvmDirtyError};
#[cfg(target_os = "linux")]
pub use uffd_restore::{
    madvise_dontneed, start_lazy_restore, start_lazy_restore_in_place, LazyRestore,
    UffdRestoreError,
};
