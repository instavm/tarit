//! Guest memory backend built on `vm-memory::GuestMemoryMmap`.

use crate::dirty::{DirtyBitmap, SoftwareDirtyBitmap};
use std::sync::Arc;
use thiserror::Error;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend as _, GuestMemoryMmap};

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory region creation failed: {0}")]
    Region(String),
    #[error("guest memory assembly failed: {0}")]
    Assembly(String),
    #[error("out of bounds: addr=0x{0:x} size={1}")]
    OutOfBounds(u64, u64),
}

/// A flat guest physical address space backed by one or more mmap'd regions.
///
/// v1 uses a single contiguous region starting at GPA 0 — the minimal case.
/// VMMs with PCI support use multiple regions to work around the
/// x86_64 PCI hole; we add that when (if) we add PCI, which v1 does not.
#[derive(Clone)]
pub struct GuestMemory {
    pub inner: Arc<GuestMemoryMmap>,
    pub size_bytes: u64,
    host_dirty: SoftwareDirtyBitmap,
}

impl GuestMemory {
    /// Build a single-region guest memory of `size_bytes` starting at GPA 0.
    pub fn new(size_bytes: u64) -> Result<Self, MemoryError> {
        Self::new_with_flags(size_bytes, false)
    }

    /// Build guest memory with huge pages (2 MiB). Reduces TLB misses during
    /// the page-fault storm of UFFD lazy restore (E2B reports 5x faster
    /// first read). Requires `vm.nr_hugepages > 0` on the host.
    pub fn new_hugepages(size_bytes: u64) -> Result<Self, MemoryError> {
        Self::new_with_flags(size_bytes, true)
    }

    fn new_with_flags(size_bytes: u64, huge_pages: bool) -> Result<Self, MemoryError> {
        if size_bytes == 0 || !size_bytes.is_multiple_of(4096) {
            return Err(MemoryError::Region(format!(
                "size must be a non-zero multiple of 4096, got {size_bytes}"
            )));
        }
        // For huge pages, round up to 2 MiB boundary.
        let actual_size = if huge_pages {
            let hp_size = 2 * 1024 * 1024u64;
            if !size_bytes.is_multiple_of(hp_size) {
                ((size_bytes / hp_size) + 1) * hp_size
            } else {
                size_bytes
            }
        } else {
            size_bytes
        };
        let inner = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), actual_size as usize)])
            .map_err(|e| MemoryError::Assembly(format!("guest memory: {e}")))?;

        Ok(Self {
            inner: Arc::new(inner),
            size_bytes: actual_size,
            host_dirty: SoftwareDirtyBitmap::new(),
        })
    }

    /// Raw pointer to the start of the first mmap'd region.
    ///
    /// SAFETY contract for callers: the returned pointer is valid for reads
    /// and writes of `size_bytes` bytes for as long as this `GuestMemory`
    /// stays alive. Used by the snapshot dumper.
    pub fn as_ptr(&self) -> *const u8 {
        // vm-memory's `GuestMemory::iter` yields `&Self::R` (= `&GuestRegionMmap`).
        // Our v1 single-region layout means the first region is the whole thing.
        self.inner
            .iter()
            .next()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut())
    }

    /// Read `buf.len()` bytes from guest physical address `gpa`.
    /// Returns Err if the read is out of bounds.
    pub fn read_phys(&self, gpa: u64, buf: &mut [u8]) -> Result<(), MemoryError> {
        self.inner
            .read_slice(buf, GuestAddress(gpa))
            .map_err(|_| MemoryError::OutOfBounds(gpa, buf.len() as u64))
    }

    /// Write `buf` to guest physical address `gpa`.
    pub fn write_phys(&self, gpa: u64, buf: &[u8]) -> Result<(), MemoryError> {
        self.inner
            .write_slice(buf, GuestAddress(gpa))
            .map_err(|_| MemoryError::OutOfBounds(gpa, buf.len() as u64))?;
        self.mark_host_dirty(gpa, buf.len() as u64);
        Ok(())
    }

    pub fn host_dirty_tracker(&self) -> SoftwareDirtyBitmap {
        self.host_dirty.clone()
    }

    pub fn mark_host_dirty(&self, gpa: u64, len: u64) {
        self.host_dirty.mark_range(gpa, len);
    }

    pub fn drain_host_dirty(&self) -> DirtyBitmap {
        self.host_dirty.drain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_small_guest_memory() {
        let m = GuestMemory::new(4096).expect("4K");
        assert_eq!(m.size_bytes, 4096);
    }

    #[test]
    fn rejects_unaligned_size() {
        assert!(GuestMemory::new(100).is_err());
        assert!(GuestMemory::new(0).is_err());
    }

    #[test]
    fn builds_typical_256mib() {
        let m = GuestMemory::new(256 * 1024 * 1024).expect("256MiB");
        assert_eq!(m.size_bytes, 256 * 1024 * 1024);
    }

    #[test]
    fn write_phys_marks_host_dirty_pages() {
        let m = GuestMemory::new(3 * 4096).expect("12K");
        m.write_phys(0x0fff, &[1, 2]).unwrap();

        let dirty = m.drain_host_dirty();
        assert!(dirty.contains(0));
        assert!(dirty.contains(0x1000));
        assert_eq!(dirty.len(), 2);
        assert!(m.drain_host_dirty().is_empty());
    }
}
