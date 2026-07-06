//! Dirty-page tracking — the foundation for diff snapshots (§9a), live
//! snapshots (§9c), and live migration (§9d).
//!
//! Two KVM mechanisms (PRD §9c):
//!   - `KVM_GET_DIRTY_LOG` bitmap — classic write-protect-and-scan.
//!   - `KVM_CAP_DIRTY_LOG_RING` — per-vCPU dirty ring buffer; scales better
//!     to large memory and lower pause windows.
//!
//! Off-KVM, we expose a pure-Rust bitmap used by the snapshot diff logic so
//! the bookkeeping is unit-testable without a kernel.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

const PAGE_SIZE: usize = 4096;

/// A simple dirty-page set over a flat guest physical address space.
///
/// On KVM this is populated from `KVM_GET_DIRTY_LOG` / the dirty ring; off-KVM
/// it is populated by the snapshot's diff layer to record which pages changed
/// between two snapshots.
#[derive(Debug, Clone, Default)]
pub struct DirtyBitmap {
    /// Set of dirty guest PFNs (gpa / PAGE_SIZE).
    dirty: HashSet<u64>,
}

impl DirtyBitmap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the page containing `gpa` dirty.
    pub fn mark(&mut self, gpa: u64) {
        self.dirty.insert(gpa / PAGE_SIZE as u64);
    }

    /// Mark a byte range dirty (rounds outward to whole pages).
    pub fn mark_range(&mut self, gpa: u64, len: u64) {
        if len == 0 {
            return;
        }
        let Some(end_gpa) = gpa.checked_add(len) else {
            return;
        };
        let start = gpa / PAGE_SIZE as u64;
        let end = end_gpa.div_ceil(PAGE_SIZE as u64);
        for pfn in start..end {
            self.dirty.insert(pfn);
        }
    }

    pub fn contains(&self, gpa: u64) -> bool {
        self.dirty.contains(&(gpa / PAGE_SIZE as u64))
    }

    pub fn dirty_pfns(&self) -> &HashSet<u64> {
        &self.dirty
    }

    pub fn len(&self) -> usize {
        self.dirty.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dirty.is_empty()
    }

    /// Remove and return the set of dirty PFNs — used after a snapshot round
    /// copies them out, so the next diff captures only subsequent writes.
    pub fn drain(&mut self) -> HashSet<u64> {
        std::mem::take(&mut self.dirty)
    }

    /// Union another bitmap into this one.
    pub fn merge(&mut self, other: &DirtyBitmap) {
        for pfn in &other.dirty {
            self.dirty.insert(*pfn);
        }
    }

    /// Populate this bitmap from a KVM-style dirty-log byte array for a
    /// region starting at `region_start_gpa`. Bit `i` in the byte array
    /// (bit `byte*8 + bit_in_byte`) corresponds to page
    /// `region_start_gpa / PAGE_SIZE + i`. This is the host-agnostic core
    /// of [`crate::kvm_dirty::read_dirty_log`], extracted so it's
    /// unit-testable without KVM.
    pub fn from_kvm_log(region_start_gpa: u64, log: &[u8]) -> Self {
        let mut b = DirtyBitmap::new();
        for (byte_idx, &v) in log.iter().enumerate() {
            if v == 0 {
                continue;
            }
            for bit in 0..8u32 {
                if v & (1 << bit) != 0 {
                    let pfn_offset = (byte_idx * 8 + bit as usize) as u64;
                    let gpa = region_start_gpa + pfn_offset * PAGE_SIZE as u64;
                    b.mark(gpa);
                }
            }
        }
        b
    }
}

/// Thread-safe software dirty bitmap for pages written by the host/VMM.
///
/// KVM dirty logging only records writes performed by guest vCPUs. Virtio
/// devices, guest-control channels, and UFFD restore copy bytes directly into
/// the guest mmap, so they must mark this bitmap explicitly for diff snapshots.
#[derive(Debug, Clone, Default)]
pub struct SoftwareDirtyBitmap {
    dirty: Arc<Mutex<DirtyBitmap>>,
}

impl SoftwareDirtyBitmap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark(&self, gpa: u64) {
        self.dirty.lock().unwrap().mark(gpa);
    }

    pub fn mark_range(&self, gpa: u64, len: u64) {
        if len == 0 {
            return;
        }
        self.dirty.lock().unwrap().mark_range(gpa, len);
    }

    pub fn snapshot(&self) -> DirtyBitmap {
        self.dirty.lock().unwrap().clone()
    }

    pub fn drain(&self) -> DirtyBitmap {
        let dirty = self.dirty.lock().unwrap().drain();
        DirtyBitmap { dirty }
    }

    pub fn is_empty(&self) -> bool {
        self.dirty.lock().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_and_check() {
        let mut b = DirtyBitmap::new();
        b.mark(0x1000);
        b.mark(0x2fff);
        assert!(b.contains(0x1000));
        assert!(b.contains(0x2fff));
        assert!(!b.contains(0x3000));
        assert_eq!(b.len(), 2); // pages 1 and 2
    }

    #[test]
    fn mark_range_rounds_outward() {
        let mut b = DirtyBitmap::new();
        b.mark_range(0x1000, 1); // exactly page 1
        assert_eq!(b.len(), 1);
        b.mark_range(0x1000, 0x2000); // pages 1 and 2
        assert_eq!(b.len(), 2);
        b.mark_range(0x0fff, 2); // crosses into page 0 and page 1
        assert_eq!(b.len(), 3);
    }

    #[test]
    fn mark_range_rejects_overflow() {
        let mut b = DirtyBitmap::new();
        b.mark_range(u64::MAX - 1, 4);
        assert!(b.is_empty());
    }

    #[test]
    fn drain_resets() {
        let mut b = DirtyBitmap::new();
        b.mark(0x5000);
        let taken = b.drain();
        assert_eq!(taken.len(), 1);
        assert!(b.is_empty());
    }

    #[test]
    fn merge_unions() {
        let mut a = DirtyBitmap::new();
        a.mark(0x1000);
        let mut b = DirtyBitmap::new();
        b.mark(0x2000);
        a.merge(&b);
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn from_kvm_log_decodes_bits_to_pfns() {
        // Region starts at GPA 0. A dirty log byte 0b00000101 means pages
        // 0 and 2 are dirty (bits 0 and 2).
        let b = DirtyBitmap::from_kvm_log(0, &[0b0000_0101]);
        assert!(b.contains(0x0000)); // page 0
        assert!(b.contains(0x2000)); // page 2
        assert!(!b.contains(0x1000)); // page 1 clean
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn from_kvm_log_handles_nonzero_region_start() {
        // Region starts at GPA 0x10_0000 (1 MiB). Dirty log byte 0b1 at
        // byte 0 means page (1MiB / 4KiB) = 256 is dirty.
        let b = DirtyBitmap::from_kvm_log(0x10_0000, &[0b1]);
        assert!(b.contains(0x10_0000)); // page 256
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn from_kvm_log_skips_zero_bytes() {
        // A long zero run followed by a dirty byte.
        let mut log = vec![0u8; 1000];
        log.push(0b1);
        let b = DirtyBitmap::from_kvm_log(0, &log);
        // The dirty page is at byte 1000, bit 0 → page 8000.
        assert_eq!(b.len(), 1);
        assert!(b.contains(8000 * 4096));
    }

    #[test]
    fn software_dirty_bitmap_marks_and_drains_ranges() {
        let tracker = SoftwareDirtyBitmap::new();
        tracker.mark_range(0x0fff, 2);
        let snap = tracker.snapshot();
        assert!(snap.contains(0));
        assert!(snap.contains(0x1000));

        let drained = tracker.drain();
        assert_eq!(drained.len(), 2);
        assert!(tracker.is_empty());
    }
}
