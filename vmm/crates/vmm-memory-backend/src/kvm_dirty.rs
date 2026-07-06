//! KVM dirty-page tracking — the ioctl layer.
//!
//! PRD §9c: "Two KVM tracking mechanisms (choose/support both):
//!   - `KVM_GET_DIRTY_LOG` bitmap — classic write-protect-and-scan. Simple;
//!     the dirty bitmap is reset on snapshot so the next diff captures only
//!     subsequent writes (this is exactly how Firecracker's diff snapshots
//!     work).
//!   - `KVM_CAP_DIRTY_LOG_RING` — per-vCPU dirty ring buffer."
//!
//! This module is the ioctl plumbing; the pure-Rust [`DirtyBitmap`] that
//! holds the result is in [`crate::dirty`] (host-agnostic, unit-tested).

#![cfg(all(feature = "kvm", target_os = "linux"))]

use crate::backend::GuestMemory;
use crate::dirty::DirtyBitmap;
use kvm_ioctls::VmFd;
use vm_memory::{Address, GuestMemoryBackend as _, GuestMemoryRegion};

/// Errors from KVM dirty-log operations.
#[derive(Debug, thiserror::Error)]
pub enum KvmDirtyError {
    #[error("KVM_GET_DIRTY_LOG failed for slot {slot}: {e}")]
    GetDirtyLog { slot: u32, e: kvm_ioctls::Error },
    #[error("KVM_CAP_DIRTY_LOG_RING unavailable: {0}")]
    NoDirtyRing(kvm_ioctls::Error),
}

/// Read the dirty bitmap for every registered slot and merge into one
/// [`DirtyBitmap`]. PRD §9c: this is the "classic write-protect-and-scan"
/// path used by diff snapshots.
///
/// After this call KVM resets its internal bitmap for each slot, so the
/// next read captures only pages dirtied *after* this point — exactly the
/// diff-snapshot semantics (PRD §9a: "the dirty bitmap resets on each
/// snapshot").
pub fn read_dirty_log(
    vm: &VmFd,
    mem: &GuestMemory,
    slots: &[u32],
) -> Result<DirtyBitmap, KvmDirtyError> {
    let mut bitmap = DirtyBitmap::new();
    for (slot_idx, region) in mem.inner.iter().enumerate() {
        let slot = slots.get(slot_idx).copied().unwrap_or(slot_idx as u32);
        let region_start = region.start_addr().raw_value();
        let region_len = region.len();

        // KVM_GET_DIRTY_LOG returns a bitmap with one bit per page (4 KiB).
        // kvm-ioctls's `get_dirty_log` returns `Vec<u64>` — each word holds 64
        // page bits (LSB = lowest page). The old code walked it as bytes and
        // only read bits 0..8 of each word, dropping 56 of every 64 pages, so
        // diff snapshots missed most dirtied guest pages and restore was
        // inconsistent (guest fault/stall on resume).
        let dirty = vm
            .get_dirty_log(slot, region_len as usize)
            .map_err(|e| KvmDirtyError::GetDirtyLog { slot, e })?;

        // Walk the bitmap; for each set bit, mark the corresponding PFN.
        for (word_idx, &w) in dirty.iter().enumerate() {
            if w == 0 {
                continue;
            }
            for bit in 0..64 {
                if w & (1u64 << bit) != 0 {
                    let pfn_offset = (word_idx * 64 + bit) as u64;
                    let gpa = region_start + pfn_offset * 4096;
                    bitmap.mark(gpa);
                }
            }
        }
    }
    Ok(bitmap)
}
