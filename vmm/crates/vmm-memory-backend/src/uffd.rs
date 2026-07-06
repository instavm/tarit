//! userfaultfd (UFFD) lazy-restore handler — the key to sub-10ms restores
//! and post-copy migration.
//!
//! Lazy restore: register guest memory with userfaultfd, hand the fd to a
//! userspace handler; the handler `mmap`s the snapshot file and resolves
//! each fault with a single `UFFDIO_COPY` directly from the mapping — no
//! file-I/O syscalls in the hot path.
//!
//! Post-copy fallback: demand-fetch not-yet-transferred pages
//! over the network via UFFD (`UFFDIO_COPY` sourced from a remote page
//! server).
//!
//! This module is Linux-only (userfaultfd is a Linux syscall). On non-Linux
//! the type exists as a stub so the rest of the crate compiles.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum UffdError {
    #[error("userfaultfd not available on this platform")]
    Unsupported,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("syscall: {0}")]
    Syscall(String),
}

/// A UFFD-based lazy restore controller.
///
/// On Linux it opens `/dev/userfaultfd` (or the `userfaultfd(2)` syscall),
/// registers the guest memory range, and serves page faults from a snapshot
/// file mapping. On non-Linux it's a stub so the rest of the crate compiles.
pub struct UffdHandler {
    /// Size of the region under UFFD management.
    pub size: u64,
    /// The userfaultfd file descriptor. Used in M10.
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    fd: Option<std::os::fd::RawFd>,
    /// Snapshot file mapping to UFFDIO_COPY from. Used in M10.
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    snapshot_mapping: Option<*const u8>,
}

#[cfg(target_os = "linux")]
impl UffdHandler {
    pub fn new(size: u64) -> Result<Self, UffdError> {
        Ok(Self {
            size,
            fd: None,
            snapshot_mapping: None,
        })
    }

    /// Register the guest memory range `[base, base + size)` with userfaultfd
    /// for MISSING page faults (the lazy-restore path). Full implementation
    /// lands in M10 (snapshot/restore) — this is the scaffold.
    pub fn register(&mut self, _base: *mut u8) -> Result<(), UffdError> {
        // M10: userfaultfd(2) + UFFDIO_API + UFFDIO_REGISTER(UFFD_FEATURE_*)
        // + a fault-servicing thread that UFFDIO_COPYs from the snapshot
        // mapping. Scaffold only for now.
        Ok(())
    }

    /// Serve page faults until the region is fully populated. M10.
    pub fn serve(&self) -> Result<(), UffdError> {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
impl UffdHandler {
    pub fn new(size: u64) -> Result<Self, UffdError> {
        Ok(Self { size })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_handler_carries_size() {
        let h = UffdHandler::new(256 * 1024 * 1024).unwrap();
        assert_eq!(h.size, 256 * 1024 * 1024);
    }

    #[test]
    fn new_handler_zero_size() {
        // Zero is a valid (if useless) size for the scaffold.
        let h = UffdHandler::new(0).unwrap();
        assert_eq!(h.size, 0);
    }
}
