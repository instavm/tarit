//! Guest memory backend built on `vm-memory::GuestMemoryMmap`.

use crate::dirty::{DirtyBitmap, SoftwareDirtyBitmap};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::RwLock;
use thiserror::Error;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap, GuestRegionMmap, MmapRegion};

/// Guest physical addresses reserved for virtio MMIO, PCI configuration, and
/// the KVM TSS. RAM above the aperture is packed immediately after low RAM in
/// host memory and snapshot files, but is registered with KVM at 4 GiB.
pub const MMIO_GAP_START: u64 = 0xd000_0000;
pub const MMIO_GAP_END: u64 = 0x1_0000_0000;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory region creation failed: {0}")]
    Region(String),
    #[error("guest memory assembly failed: {0}")]
    Assembly(String),
    #[error("out of bounds: addr=0x{0:x} size={1}")]
    OutOfBounds(u64, u64),
}

/// Guest physical memory backed by one contiguous host mapping.
///
/// Up to 3.25 GiB, the host mapping is registered as one KVM slot at GPA zero.
/// Any remaining RAM is registered as a second slot at GPA 4 GiB. Keeping the
/// host mapping contiguous gives snapshots and UFFD a stable packed byte
/// layout without placing a 768 MiB hole in every artifact.
#[derive(Clone)]
pub struct GuestMemory {
    pub inner: Arc<GuestMemoryMmap>,
    pub size_bytes: u64,
    backing: Arc<MmapRegion>,
    host_dirty: SoftwareDirtyBitmap,
    #[cfg(target_os = "linux")]
    lazy_page_discard: Arc<RwLock<Option<crate::uffd_restore::LazyPageDiscard>>>,
}

impl GuestMemory {
    /// Build guest memory containing `size_bytes` of packed RAM.
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
        let actual_size_usize = usize::try_from(actual_size).map_err(|_| {
            MemoryError::Region(format!("memory size does not fit usize: {actual_size}"))
        })?;
        let backing = Arc::new(
            MmapRegion::new(actual_size_usize)
                .map_err(|e| MemoryError::Assembly(format!("guest memory backing: {e}")))?,
        );
        let low_size = actual_size.min(MMIO_GAP_START);
        let high_size = actual_size - low_size;
        let mut regions = Vec::with_capacity(if high_size == 0 { 1 } else { 2 });
        regions.push(Self::raw_region(
            backing.as_ptr(),
            low_size,
            GuestAddress(0),
        )?);
        if high_size != 0 {
            let low_size_usize = usize::try_from(low_size)
                .map_err(|_| MemoryError::Region("low memory size does not fit usize".into()))?;
            // SAFETY: `low_size < actual_size`, so this points inside the live
            // allocation retained by `backing`.
            let high_ptr = unsafe { backing.as_ptr().add(low_size_usize) };
            regions.push(Self::raw_region(
                high_ptr,
                high_size,
                GuestAddress(MMIO_GAP_END),
            )?);
        }
        let inner = GuestMemoryMmap::from_regions(regions)
            .map_err(|e| MemoryError::Assembly(format!("guest memory regions: {e}")))?;

        Ok(Self {
            inner: Arc::new(inner),
            size_bytes: actual_size,
            backing,
            host_dirty: SoftwareDirtyBitmap::new(),
            #[cfg(target_os = "linux")]
            lazy_page_discard: Arc::new(RwLock::new(None)),
        })
    }

    fn raw_region(
        pointer: *mut u8,
        size: u64,
        guest_base: GuestAddress,
    ) -> Result<GuestRegionMmap, MemoryError> {
        let size = usize::try_from(size)
            .map_err(|_| MemoryError::Region("memory region size does not fit usize".into()))?;
        // SAFETY: every caller supplies a page-aligned subrange of `backing`,
        // which GuestMemory retains for longer than the non-owning region.
        let mapping = unsafe {
            MmapRegion::build_raw(
                pointer,
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_PRIVATE,
            )
        }
        .map_err(|e| MemoryError::Assembly(format!("guest memory region: {e}")))?;
        GuestRegionMmap::new(mapping, guest_base)
            .ok_or_else(|| MemoryError::Assembly("guest memory address overflow".into()))
    }

    /// Raw pointer to the packed host RAM backing.
    ///
    /// SAFETY contract for callers: the returned pointer is valid for reads
    /// and writes of `size_bytes` bytes for as long as this `GuestMemory`
    /// stays alive. Used by the snapshot dumper.
    pub fn as_ptr(&self) -> *const u8 {
        self.backing.as_ptr()
    }

    /// Translate a guest physical address into its packed snapshot/host offset.
    pub fn gpa_to_offset(&self, gpa: u64) -> Option<u64> {
        let low_size = self.size_bytes.min(MMIO_GAP_START);
        if gpa < low_size {
            return Some(gpa);
        }
        let high_size = self.size_bytes - low_size;
        if gpa >= MMIO_GAP_END && gpa - MMIO_GAP_END < high_size {
            return Some(low_size + (gpa - MMIO_GAP_END));
        }
        None
    }

    /// Translate a packed snapshot/host offset into a guest physical address.
    pub fn offset_to_gpa(&self, offset: u64) -> Option<u64> {
        if offset >= self.size_bytes {
            return None;
        }
        let low_size = self.size_bytes.min(MMIO_GAP_START);
        if offset < low_size {
            Some(offset)
        } else {
            Some(MMIO_GAP_END + (offset - low_size))
        }
    }

    fn gpa_range_to_offset(&self, gpa: u64, len: u64) -> Option<u64> {
        let end = gpa.checked_add(len)?;
        let offset = self.gpa_to_offset(gpa)?;
        if len == 0 {
            return Some(offset);
        }
        let last_offset = self.gpa_to_offset(end.checked_sub(1)?)?;
        (last_offset == offset + len - 1).then_some(offset)
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
        if let Some(offset) = self.gpa_range_to_offset(gpa, buf.len() as u64) {
            self.mark_host_dirty(offset, buf.len() as u64);
        }
        Ok(())
    }

    pub fn host_dirty_tracker(&self) -> SoftwareDirtyBitmap {
        self.host_dirty.clone()
    }

    /// Mark a range in the packed snapshot address space dirty.
    pub fn mark_host_dirty(&self, offset: u64, len: u64) {
        self.host_dirty.mark_range(offset, len);
    }

    pub fn drain_host_dirty(&self) -> DirtyBitmap {
        self.host_dirty.drain()
    }

    #[cfg(target_os = "linux")]
    pub fn set_lazy_page_discard(&self, discard: crate::uffd_restore::LazyPageDiscard) {
        *self
            .lazy_page_discard
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(discard);
    }

    /// Return the exclusion fence used by host-side readers of lazily restored
    /// RAM. Holding its write side prevents balloon MADV_DONTNEED calls while
    /// the caller walks pages; the UFFD handler remains unfenced and can serve
    /// every missing-page fault raised by that walk.
    #[cfg(target_os = "linux")]
    pub fn lazy_snapshot_fence(&self) -> Option<Arc<RwLock<()>>> {
        self.lazy_page_discard
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(crate::uffd_restore::LazyPageDiscard::snapshot_fence)
    }

    /// Release page-aligned guest RAM while preserving zero-on-refault
    /// semantics for both fresh anonymous memory and UFFD lazy restores.
    #[cfg(target_os = "linux")]
    pub fn discard_range(&self, gpa: u64, len: u64) -> Result<(), MemoryError> {
        const PAGE_SIZE: u64 = 4096;
        if len == 0 || !gpa.is_multiple_of(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
            return Err(MemoryError::OutOfBounds(gpa, len));
        }
        let offset = self
            .gpa_range_to_offset(gpa, len)
            .ok_or(MemoryError::OutOfBounds(gpa, len))?;
        if let Some(discard) = self
            .lazy_page_discard
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            discard
                .discard(offset as usize, len as usize)
                .map_err(|error| MemoryError::Region(error.to_string()))?;
            self.host_dirty.mark_range(offset, len);
            return Ok(());
        }
        let base = self.as_ptr() as *mut u8;
        let offset = usize::try_from(offset).map_err(|_| MemoryError::OutOfBounds(gpa, len))?;
        let length = usize::try_from(len).map_err(|_| MemoryError::OutOfBounds(gpa, len))?;
        // SAFETY: range validation above proves this subrange is within the
        // live, page-aligned guest mmap owned by `self`.
        crate::uffd_restore::madvise_dontneed(unsafe { base.add(offset) }, length)
            .map_err(|error| MemoryError::Region(error.to_string()))?;
        self.host_dirty.mark_range(offset as u64, len);
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn discard_range(&self, gpa: u64, len: u64) -> Result<(), MemoryError> {
        static ZERO_CHUNK: [u8; 64 * 1024] = [0; 64 * 1024];
        if len == 0 || !gpa.is_multiple_of(4096) || !len.is_multiple_of(4096) {
            return Err(MemoryError::OutOfBounds(gpa, len));
        }
        let end = gpa
            .checked_add(len)
            .ok_or(MemoryError::OutOfBounds(gpa, len))?;
        self.gpa_range_to_offset(gpa, len)
            .ok_or(MemoryError::OutOfBounds(gpa, len))?;
        let mut cursor = gpa;
        while cursor < end {
            let chunk = usize::try_from((end - cursor).min(ZERO_CHUNK.len() as u64))
                .map_err(|_| MemoryError::OutOfBounds(gpa, len))?;
            self.write_phys(cursor, &ZERO_CHUNK[..chunk])?;
            cursor += chunk as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::{GuestMemoryBackend as _, GuestMemoryRegion as _};

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
    fn splits_ram_above_mmio_gap_while_backing_stays_packed() {
        let size = MMIO_GAP_START + 2 * 4096;
        let m = GuestMemory::new(size).expect("split guest memory");
        let regions = m.inner.iter().collect::<Vec<_>>();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].start_addr(), GuestAddress(0));
        assert_eq!(regions[0].len(), MMIO_GAP_START);
        assert_eq!(regions[1].start_addr(), GuestAddress(MMIO_GAP_END));
        assert_eq!(regions[1].len(), 2 * 4096);
        assert_eq!(m.gpa_to_offset(MMIO_GAP_END), Some(MMIO_GAP_START));
        assert_eq!(m.offset_to_gpa(MMIO_GAP_START), Some(MMIO_GAP_END));
        assert_eq!(m.gpa_to_offset(MMIO_GAP_START), None);

        m.write_phys(MMIO_GAP_END, &[0x5a; 16]).unwrap();
        // SAFETY: the packed backing contains `size` live bytes.
        let packed = unsafe { std::slice::from_raw_parts(m.as_ptr(), size as usize) };
        assert_eq!(
            &packed[MMIO_GAP_START as usize..MMIO_GAP_START as usize + 16],
            &[0x5a; 16]
        );
        assert!(m.drain_host_dirty().contains(MMIO_GAP_START));
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
