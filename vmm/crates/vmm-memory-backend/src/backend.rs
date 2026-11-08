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

/// A run of packed RAM that is a private mapping of a file. Until the guest
/// writes a page of it, that page's contents are the file's bytes.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub struct FileBackedRange {
    pub ram_offset: u64,
    pub len: u64,
    pub file: Arc<std::fs::File>,
    pub file_offset: u64,
}

/// Non-overlapping file-backed runs keyed by RAM offset, plus one shared
/// handle per mapped file so layered chain restores do not hold a descriptor
/// per run.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct FileRanges {
    runs: std::collections::BTreeMap<u64, (u64, Arc<std::fs::File>, u64)>,
    files: std::collections::HashMap<(u64, u64), Arc<std::fs::File>>,
}

#[cfg(target_os = "linux")]
impl FileRanges {
    /// Drop `start..start + len` from every run, splitting partial overlaps.
    fn forget(&mut self, start: u64, len: u64) {
        let end = start + len;
        let overlapping: Vec<u64> = self
            .runs
            .range(..end)
            .filter(|(run_start, (run_len, _, _))| **run_start + *run_len > start)
            .map(|(run_start, _)| *run_start)
            .collect();
        for run_start in overlapping {
            let (run_len, file, file_offset) = self.runs.remove(&run_start).expect("listed run");
            let run_end = run_start + run_len;
            if run_start < start {
                self.runs.insert(
                    run_start,
                    (start - run_start, Arc::clone(&file), file_offset),
                );
            }
            if run_end > end {
                self.runs.insert(
                    end,
                    (
                        run_end - end,
                        Arc::clone(&file),
                        file_offset + (end - run_start),
                    ),
                );
            }
        }
        self.files.retain(|_, file| Arc::strong_count(file) > 1);
    }

    fn record(
        &mut self,
        start: u64,
        len: u64,
        file: &std::fs::File,
        file_offset: u64,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        let shared = match self.files.get(&(metadata.dev(), metadata.ino())) {
            Some(shared) => Arc::clone(shared),
            None => {
                let shared = Arc::new(file.try_clone()?);
                self.files
                    .insert((metadata.dev(), metadata.ino()), Arc::clone(&shared));
                shared
            }
        };
        self.forget(start, len);
        self.runs.insert(start, (len, shared, file_offset));
        Ok(())
    }
}

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
    pub low_size_bytes: u64,
    backing: Arc<MmapRegion>,
    host_dirty: SoftwareDirtyBitmap,
    #[cfg(target_os = "linux")]
    lazy_page_discard: Arc<RwLock<Option<crate::uffd_restore::LazyPageDiscard>>>,
    /// Set once the backing is a private mapping of a snapshot file. Discards
    /// must then replace pages with anonymous memory, because MADV_DONTNEED on
    /// a private file mapping would refault the snapshot's bytes, not zeros.
    #[cfg(target_os = "linux")]
    file_backed: Arc<std::sync::atomic::AtomicBool>,
    /// Which RAM runs are currently private mappings of which file.
    #[cfg(target_os = "linux")]
    file_ranges: Arc<std::sync::Mutex<FileRanges>>,
}

impl GuestMemory {
    /// Build guest memory containing `size_bytes` of packed RAM.
    pub fn new(size_bytes: u64) -> Result<Self, MemoryError> {
        Self::new_with_layout(size_bytes, false, None)
    }

    /// Build guest memory with huge pages (2 MiB). Reduces TLB misses during
    /// the page-fault storm of UFFD lazy restore (E2B reports 5x faster
    /// first read). Requires `vm.nr_hugepages > 0` on the host.
    pub fn new_hugepages(size_bytes: u64) -> Result<Self, MemoryError> {
        Self::new_with_layout(size_bytes, true, None)
    }

    /// Fixed boot RAM below the MMIO aperture, with a virtio-mem region at 4GiB.
    /// The maximum backing is reserved up front; it is not all boot-time RAM.
    pub fn new_hotplug(size_bytes: u64, boot_bytes: u64) -> Result<Self, MemoryError> {
        if boot_bytes == 0
            || boot_bytes > MMIO_GAP_START
            || boot_bytes >= size_bytes
            || !boot_bytes.is_multiple_of(128 * 1024 * 1024)
            || !size_bytes.is_multiple_of(128 * 1024 * 1024)
        {
            return Err(MemoryError::Region(
                "invalid virtio-mem memory layout".into(),
            ));
        }
        Self::new_with_layout(size_bytes, false, Some(boot_bytes))
    }

    fn new_with_layout(
        size_bytes: u64,
        huge_pages: bool,
        boot_bytes: Option<u64>,
    ) -> Result<Self, MemoryError> {
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
        let low_size = boot_bytes.unwrap_or_else(|| actual_size.min(MMIO_GAP_START));
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
            low_size_bytes: low_size,
            backing,
            host_dirty: SoftwareDirtyBitmap::new(),
            #[cfg(target_os = "linux")]
            lazy_page_discard: Arc::new(RwLock::new(None)),
            #[cfg(target_os = "linux")]
            file_backed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(target_os = "linux")]
            file_ranges: Arc::default(),
        })
    }

    /// Replace the whole RAM backing with a private copy-on-write mapping of
    /// `file` starting at `offset`, the packed RAM image of a full snapshot.
    ///
    /// Pages are faulted in by the kernel from the page cache on first touch,
    /// so restore cost does not depend on RAM size, and every VM restored from
    /// the same file shares the untouched pages. Guest writes stay private.
    /// The file must not be modified while any mapping of it is live.
    ///
    /// Must be called before the memory is registered with KVM or handed to
    /// devices.
    #[cfg(target_os = "linux")]
    pub fn map_file_private(&self, file: &std::fs::File, offset: u64) -> Result<(), MemoryError> {
        self.map_file_range_private(file, offset, 0, self.size_bytes)
    }

    /// Like [`Self::map_file_private`], but replaces only the packed RAM range
    /// `ram_offset..ram_offset + len` with `file` from `file_offset`. Used to
    /// layer page runs of incremental snapshots over a mapped base image.
    #[cfg(target_os = "linux")]
    pub fn map_file_range_private(
        &self,
        file: &std::fs::File,
        file_offset: u64,
        ram_offset: u64,
        len: u64,
    ) -> Result<(), MemoryError> {
        use std::os::fd::AsRawFd;

        if !file_offset.is_multiple_of(4096)
            || !ram_offset.is_multiple_of(4096)
            || !len.is_multiple_of(4096)
            || len == 0
        {
            return Err(MemoryError::Region(format!(
                "snapshot RAM range is not page aligned: file {file_offset}, ram {ram_offset}, len {len}"
            )));
        }
        ram_offset
            .checked_add(len)
            .filter(|end| *end <= self.size_bytes)
            .ok_or(MemoryError::OutOfBounds(ram_offset, len))?;
        let len = usize::try_from(len).map_err(|_| MemoryError::OutOfBounds(ram_offset, len))?;
        let ram_offset = usize::try_from(ram_offset)
            .map_err(|_| MemoryError::OutOfBounds(ram_offset, len as u64))?;
        let offset = libc::off_t::try_from(file_offset)
            .map_err(|_| MemoryError::Region("snapshot RAM offset overflows off_t".into()))?;
        // SAFETY: the range was checked to lie inside the live reservation
        // owned by `self.backing`; MAP_FIXED atomically replaces it with the
        // file mapping at the same address, so every pointer derived from the
        // backing stays valid. The mapping is unmapped by `backing`'s Drop.
        let mapped = unsafe {
            libc::mmap(
                self.backing.as_ptr().add(ram_offset).cast(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_FIXED | libc::MAP_NORESERVE,
                file.as_raw_fd(),
                offset,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(MemoryError::Region(format!(
                "map snapshot RAM: {}",
                std::io::Error::last_os_error()
            )));
        }
        self.file_backed
            .store(true, std::sync::atomic::Ordering::Release);
        self.file_ranges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(ram_offset as u64, len as u64, file, file_offset)
            .map_err(|error| {
                MemoryError::Region(format!("record snapshot RAM mapping: {error}"))
            })?;
        Ok(())
    }

    /// RAM runs that are currently private mappings of a file, in RAM order.
    /// A page in one of these runs that the guest has not written still holds
    /// the file's bytes at the corresponding offset.
    #[cfg(target_os = "linux")]
    pub fn file_backed_ranges(&self) -> Vec<FileBackedRange> {
        self.file_ranges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .runs
            .iter()
            .map(|(ram_offset, (len, file, file_offset))| FileBackedRange {
                ram_offset: *ram_offset,
                len: *len,
                file: Arc::clone(file),
                file_offset: *file_offset,
            })
            .collect()
    }

    /// Whether RAM is a private mapping of a snapshot file.
    #[cfg(target_os = "linux")]
    pub fn is_file_backed(&self) -> bool {
        self.file_backed.load(std::sync::atomic::Ordering::Acquire)
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
        let low_size = self.low_size_bytes;
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
        let low_size = self.low_size_bytes;
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

    /// Tracker shared with devices that write guest RAM directly. They hold
    /// only guest physical addresses and must mark through
    /// [`SoftwareDirtyBitmap::mark_guest_range`], which honours this memory's
    /// low/high split (including a virtio-mem boot/aperture layout).
    /// [`SoftwareDirtyBitmap::mark_range`] on the same tracker takes packed
    /// backing offsets (UFFD restore, suspend).
    pub fn host_dirty_tracker(&self) -> SoftwareDirtyBitmap {
        self.host_dirty
            .with_guest_layout(self.low_size_bytes, self.size_bytes)
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

    /// Forget a UFFD lazy-restore discard hook once RAM no longer depends on it.
    #[cfg(target_os = "linux")]
    pub fn clear_lazy_page_discard(&self) {
        *self
            .lazy_page_discard
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// The UFFD lazy-restore hook while RAM still depends on it.
    #[cfg(target_os = "linux")]
    pub fn lazy_page_discard(&self) -> Option<crate::uffd_restore::LazyPageDiscard> {
        self.lazy_page_discard
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Which pages of guest RAM the kernel has populated (present or swapped
    /// out), from /proc/self/pagemap: in anonymous memory every other page is
    /// zero, in lazily restored memory every other page still comes from the
    /// snapshot. `None` when the map cannot be read.
    #[cfg(target_os = "linux")]
    pub fn populated_pages(&self) -> Option<Vec<bool>> {
        use std::os::unix::fs::FileExt;
        const PAGE: usize = 4096;
        const PRESENT: u64 = 1 << 63;
        const SWAPPED: u64 = 1 << 62;
        let addr = self.as_ptr() as usize;
        let len = usize::try_from(self.size_bytes).ok()?;
        if !addr.is_multiple_of(PAGE) {
            return None;
        }
        let pagemap = std::fs::File::open("/proc/self/pagemap").ok()?;
        let mut raw = vec![0u8; len / PAGE * 8];
        pagemap
            .read_exact_at(&mut raw, (addr / PAGE * 8) as u64)
            .ok()?;
        Some(
            raw.as_chunks::<8>()
                .0
                .iter()
                .map(|entry| u64::from_le_bytes(*entry) & (PRESENT | SWAPPED) != 0)
                .collect(),
        )
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

    /// Ask the host to back guest RAM at `gpa..gpa + len` with transparent
    /// huge pages (rounded out to 2 MiB). Meant for the guest kernel's image
    /// before it is loaded: the host then allocates a few 2 MiB pages instead
    /// of thousands of 4 KiB ones, and KVM can map the kernel with huge
    /// pages. Advice only: where THP is unavailable this does nothing.
    #[cfg(target_os = "linux")]
    pub fn advise_huge_pages(&self, gpa: u64, len: u64) -> bool {
        const HUGE: u64 = 2 * 1024 * 1024;
        let start = gpa / HUGE * HUGE;
        let Some(end) = gpa
            .checked_add(len)
            .and_then(|end| end.checked_add(HUGE - 1))
            .map(|end| end / HUGE * HUGE)
        else {
            return false;
        };
        let Some(offset) = self.gpa_range_to_offset(start, end - start) else {
            return false;
        };
        let (Ok(offset), Ok(len)) = (usize::try_from(offset), usize::try_from(end - start)) else {
            return false;
        };
        // SAFETY: the range lies inside this live guest mapping (checked by
        // gpa_range_to_offset); MADV_HUGEPAGE changes no contents.
        unsafe {
            libc::madvise(
                (self.as_ptr() as *mut u8).add(offset).cast(),
                len,
                libc::MADV_HUGEPAGE,
            ) == 0
        }
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
        if self.is_file_backed() {
            // SAFETY: range validation above proves this page-aligned subrange
            // is within the live guest mapping owned by `self`. MAP_FIXED
            // swaps it for fresh anonymous memory in place, so it refaults as
            // zeros instead of the snapshot's bytes; KVM's MMU notifier drops
            // any stale translations.
            let mapped = unsafe {
                libc::mmap(
                    base.add(offset).cast(),
                    length,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE,
                    -1,
                    0,
                )
            };
            if mapped == libc::MAP_FAILED {
                return Err(MemoryError::Region(format!(
                    "discard file-backed RAM: {}",
                    std::io::Error::last_os_error()
                )));
            }
            self.file_ranges
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .forget(offset as u64, len);
            self.host_dirty.mark_range(offset as u64, len);
            return Ok(());
        }
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

    #[cfg(target_os = "linux")]
    #[test]
    fn huge_page_advice_stays_inside_guest_ram() {
        let mem = GuestMemory::new(64 * 1024 * 1024).unwrap();
        // Rounded out to 2 MiB, still inside RAM: advised (or a no-op where
        // the host has no THP, which madvise reports as an error).
        let thp = std::path::Path::new("/sys/kernel/mm/transparent_hugepage/enabled").exists();
        assert_eq!(mem.advise_huge_pages(0x100_0000 + 123, 0x30_0000), thp);
        assert!(
            !mem.advise_huge_pages(60 * 1024 * 1024, 8 * 1024 * 1024),
            "past the end"
        );
        assert!(!mem.advise_huge_pages(u64::MAX - 10, 100), "overflow");
    }
    use vm_memory::{GuestMemoryBackend as _, GuestMemoryRegion as _};

    #[test]
    fn hotplug_layout_and_dma_dirty_tracking_preserve_packed_offsets() {
        let boot = 256 * 1024 * 1024;
        let m = GuestMemory::new_hotplug(2 * boot, boot).unwrap();
        assert_eq!(m.gpa_to_offset(boot), None);
        assert_eq!(m.gpa_to_offset(MMIO_GAP_START), None);
        assert_eq!(m.gpa_to_offset(MMIO_GAP_END + 4096), Some(boot + 4096));
        m.host_dirty_tracker()
            .mark_guest_range(MMIO_GAP_END + 4095, 2);
        let dirty = m.drain_host_dirty();
        assert_eq!(dirty.len(), 2);
        assert!(dirty.contains(boot));
        assert!(dirty.contains(boot + 4096));
        // UFFD uses packed offsets while DMA uses GPAs. Both must hit the same
        // backing bitmap, including below the conventional 3.25GiB split.
        m.host_dirty_tracker().mark_range(boot + 8192, 1);
        assert!(m.drain_host_dirty().contains(boot + 8192));
        m.host_dirty_tracker().mark_guest_range(boot, 4096);
        assert!(m.drain_host_dirty().is_empty());
        m.write_phys(MMIO_GAP_END, &[0xa5; 16]).unwrap();
        let mut bytes = [0; 16];
        m.read_phys(MMIO_GAP_END, &mut bytes).unwrap();
        assert_eq!(bytes, [0xa5; 16]);
    }

    #[test]
    fn invalid_hotplug_apertures_are_rejected_before_mapping() {
        for (max, boot) in [
            (0, 0),
            (256 << 20, 256 << 20),
            (512 << 20, 129 << 20),
            (5 << 30, 4 << 30),
        ] {
            assert!(GuestMemory::new_hotplug(max, boot).is_err());
        }
    }

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
    fn device_dirty_tracker_records_high_ram_by_packed_offset() {
        let m = GuestMemory::new(MMIO_GAP_START + 2 * 4096).expect("split guest memory");
        let tracker = m.host_dirty_tracker();
        tracker.mark_guest_range(MMIO_GAP_END, 16);
        tracker.mark_guest_range(MMIO_GAP_START - 4096, 1);

        let dirty = m.drain_host_dirty();
        assert!(dirty.contains(MMIO_GAP_START));
        assert!(dirty.contains(MMIO_GAP_START - 4096));
        assert!(!dirty.contains(MMIO_GAP_END));
        assert_eq!(dirty.len(), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_backed_ram_is_private_shared_until_written_and_discards_to_zero() {
        use std::io::Write as _;

        let mut image = vec![0u8; 8192 + 3 * 4096];
        image[8192..8192 + 4096].fill(0x11);
        image[8192 + 4096..8192 + 8192].fill(0x22);
        image[8192 + 8192..].fill(0x33);
        let mut file = tempfile::tempfile().expect("snapshot file");
        file.write_all(&image).unwrap();

        let m = GuestMemory::new(3 * 4096).expect("guest memory");
        assert!(m.map_file_private(&file, 4097).is_err(), "unaligned offset");
        m.map_file_private(&file, 8192).expect("map snapshot RAM");
        assert!(m.is_file_backed());

        let mut page = [0u8; 4096];
        m.read_phys(0, &mut page).unwrap();
        assert!(page.iter().all(|&b| b == 0x11));

        // Guest writes are private copy-on-write and never reach the file.
        m.write_phys(4096, &[0xee; 8]).unwrap();
        let mut on_disk = vec![0u8; 8];
        std::os::unix::fs::FileExt::read_exact_at(&file, &mut on_disk, 8192 + 4096).unwrap();
        assert_eq!(on_disk, [0x22; 8]);

        // A balloon discard refaults zeros, never the snapshot's bytes.
        m.discard_range(2 * 4096, 4096).unwrap();
        m.read_phys(2 * 4096, &mut page).unwrap();
        assert!(page.iter().all(|&b| b == 0));
        m.read_phys(0, &mut page).unwrap();
        assert!(page.iter().all(|&b| b == 0x11), "neighbours untouched");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_backed_ranges_follow_layering_and_discards() {
        use std::io::Write as _;

        let mut base = tempfile::tempfile().expect("base image");
        base.write_all(&vec![0x11u8; 16 * 4096]).unwrap();
        let mut diff = tempfile::tempfile().expect("diff image");
        diff.write_all(&vec![0x22u8; 4 * 4096]).unwrap();
        let m = GuestMemory::new(16 * 4096).expect("guest memory");
        m.map_file_private(&base, 0).unwrap();
        // A chain restore layers a newer run over the base.
        m.map_file_range_private(&diff, 4096, 4 * 4096, 2 * 4096)
            .unwrap();
        // A balloon discard makes a run anonymous again.
        m.discard_range(10 * 4096, 2 * 4096).unwrap();

        let runs: Vec<(u64, u64, u64)> = m
            .file_backed_ranges()
            .iter()
            .map(|r| (r.ram_offset / 4096, r.len / 4096, r.file_offset / 4096))
            .collect();
        assert_eq!(
            runs,
            vec![(0, 4, 0), (4, 2, 1), (6, 4, 6), (12, 4, 12)],
            "base split around the diff run and the discarded hole"
        );
        let ranges = m.file_backed_ranges();
        assert!(
            Arc::ptr_eq(&ranges[0].file, &ranges[2].file),
            "one handle per file"
        );
        // Every recorded run really holds its file's bytes.
        for run in &ranges {
            let mut page = [0u8; 4096];
            m.read_phys(run.ram_offset, &mut page).unwrap();
            let mut expected = [0u8; 4096];
            std::os::unix::fs::FileExt::read_exact_at(&*run.file, &mut expected, run.file_offset)
                .unwrap();
            assert_eq!(page, expected);
        }
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
