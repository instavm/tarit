//! Diff (incremental) snapshot builder.
//!
//! Because the dirty bitmap resets on each snapshot, you can
//! persist only changed pages between checkpoints — cheap, frequent
//! checkpoints for record/replay or time-travel debugging.
//!
//! A diff snapshot records only the pages in a `DirtyBitmap` (relative to a
//! prior full snapshot). Restore = apply base + diffs in order to
//! reconstruct the full memory image byte-for-byte (a base + sequence of
//! diffs, when applied, must reproduce a full snapshot byte-for-byte).

use vmm_memory_backend::dirty::DirtyBitmap;

pub const MAX_DIFF_APPLY_BYTES: u64 = 1 << 40; // 1 TiB
const PAGE: usize = 4096;

/// A single page-delta in a diff snapshot: the GPA of the page + its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageDelta {
    pub gpa: u64,
    pub bytes: Vec<u8>,
}

/// A diff snapshot: the dirty pages (as `PageDelta`s) + the device-state
/// blob at this checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSnapshot {
    pub pages: Vec<PageDelta>,
    pub state: Vec<u8>,
}

/// Build a diff snapshot from a full memory image (`mem_bytes`), a dirty
/// bitmap, and the serialized device state.
///
/// For each dirty PFN, copy the corresponding 4 KiB page out of `mem_bytes`
/// into the diff. The dirty bitmap is *drained* by the caller after this
/// (the dirty bitmap resets on each snapshot).
pub fn build_diff(mem_bytes: &[u8], dirty: &DirtyBitmap, state: Vec<u8>) -> DiffSnapshot {
    let mut pages: Vec<PageDelta> = Vec::new();
    for &pfn in dirty.dirty_pfns() {
        let Some(start) = usize::try_from(pfn)
            .ok()
            .and_then(|pfn| pfn.checked_mul(PAGE))
        else {
            continue;
        };
        if start >= mem_bytes.len() {
            continue; // dirty PFN past the end of memory (shouldn't happen)
        }
        let Some(gpa) = pfn.checked_mul(PAGE as u64) else {
            continue;
        };
        let end = start
            .checked_add(PAGE)
            .map(|end| end.min(mem_bytes.len()))
            .unwrap_or(mem_bytes.len());
        pages.push(PageDelta {
            gpa,
            bytes: mem_bytes[start..end].to_vec(),
        });
    }
    // Deterministic page order (sorted by GPA) — required for the
    // byte-for-byte equivalence check (base + diffs == full snapshot) and
    // for reproducible snapshot files. HashSet iteration is unordered.
    pages.sort_by_key(|p| p.gpa);
    DiffSnapshot { pages, state }
}

/// Apply a sequence of diffs on top of a base memory image, returning the
/// reconstructed full image. Used by the equivalence check.
pub fn apply_diffs(base: &[u8], diffs: &[DiffSnapshot]) -> Vec<u8> {
    try_apply_diffs(base, diffs).unwrap_or_else(|_| base.to_vec())
}

pub fn try_apply_diffs(base: &[u8], diffs: &[DiffSnapshot]) -> Result<Vec<u8>, DiffError> {
    let mut out = base.to_vec();
    for d in diffs {
        for page in &d.pages {
            let start = usize::try_from(page.gpa).map_err(|_| DiffError::GpaTooLarge(page.gpa))?;
            let end = start
                .checked_add(page.bytes.len())
                .ok_or(DiffError::RangeOverflow {
                    gpa: page.gpa,
                    len: page.bytes.len(),
                })?;
            let end_u64 = u64::try_from(end).map_err(|_| DiffError::ResizeTooLarge {
                requested: u64::MAX,
                max: MAX_DIFF_APPLY_BYTES,
            })?;
            if end_u64 > MAX_DIFF_APPLY_BYTES {
                return Err(DiffError::ResizeTooLarge {
                    requested: end_u64,
                    max: MAX_DIFF_APPLY_BYTES,
                });
            }
            if end > out.len() {
                out.try_reserve(end - out.len())
                    .map_err(|_| DiffError::Allocation { requested: end_u64 })?;
                out.resize(end, 0);
            }
            out[start..end].copy_from_slice(&page.bytes);
        }
    }
    Ok(out)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DiffError {
    #[error("diff GPA too large for host usize: {0}")]
    GpaTooLarge(u64),
    #[error("diff page range overflows: gpa={gpa} len={len}")]
    RangeOverflow { gpa: u64, len: usize },
    #[error("diff apply would resize memory to {requested} bytes (max {max})")]
    ResizeTooLarge { requested: u64, max: u64 },
    #[error("diff apply allocation failed for {requested} bytes")]
    Allocation { requested: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_diff_copies_only_dirty_pages() {
        // 8 pages of memory. Mark pages 1 and 3 dirty.
        let mem: Vec<u8> = (0..8 * 4096).map(|i| (i % 256) as u8).collect();
        let mut dirty = DirtyBitmap::new();
        dirty.mark(0x1000); // page 1
        dirty.mark(0x3000); // page 3

        let diff = build_diff(&mem, &dirty, b"state".to_vec());
        assert_eq!(diff.pages.len(), 2);
        assert_eq!(diff.pages[0].gpa, 0x1000);
        assert_eq!(diff.pages[1].gpa, 0x3000);
        // The page bytes must match the source memory at those offsets.
        assert_eq!(&diff.pages[0].bytes, &mem[0x1000..0x2000]);
        assert_eq!(&diff.pages[1].bytes, &mem[0x3000..0x4000]);
    }

    #[test]
    fn build_diff_empty_dirty_gives_no_pages() {
        let mem = vec![0u8; 8 * 4096];
        let dirty = DirtyBitmap::new();
        let diff = build_diff(&mem, &dirty, vec![]);
        assert!(diff.pages.is_empty());
    }

    #[test]
    fn apply_diffs_reproduces_full_image() {
        // Invariant: base + diffs == a full snapshot, byte-for-byte.
        let base = vec![0xAA; 8 * 4096];
        let mut kvm_dirty1 = DirtyBitmap::new();
        kvm_dirty1.mark(0x1000); // page 1, dirtied by the guest/vCPU
        let mut host_dirty1 = DirtyBitmap::new();
        host_dirty1.mark(0x3000); // page 3, written directly by the VMM/device
        let mut dirty1 = kvm_dirty1.clone();
        dirty1.merge(&host_dirty1);
        let mut dirty2 = DirtyBitmap::new();
        dirty2.mark(0x5000); // page 5

        // Pretend the "full" memory has page 1 = 0xBB, page 3 = 0xDD
        // (host-written, absent from KVM dirty logging), page 5 = 0xCC.
        let mut full = base.clone();
        for b in &mut full[0x1000..0x2000] {
            *b = 0xBB;
        }
        for b in &mut full[0x3000..0x4000] {
            *b = 0xDD;
        }
        for b in &mut full[0x5000..0x6000] {
            *b = 0xCC;
        }

        let d1 = build_diff(&full, &dirty1, vec![]);
        assert!(
            d1.pages.iter().any(|p| p.gpa == 0x3000),
            "host-written page missing from diff"
        );
        let d2 = build_diff(&full, &dirty2, vec![]);
        let reconstructed = apply_diffs(&base, &[d1, d2]);

        assert_eq!(reconstructed, full);
    }

    #[test]
    fn apply_diffs_grows_if_page_past_end() {
        // A diff that writes a page past the end of base should grow the
        // output (sparse memory).
        let base = vec![0u8; 4096];
        let diff = DiffSnapshot {
            pages: vec![PageDelta {
                gpa: 0x2000,
                bytes: vec![0xFF; 4096],
            }],
            state: vec![],
        };
        let out = apply_diffs(&base, &[diff]);
        assert_eq!(out.len(), 0x3000);
        assert!(out[0x2000..0x3000].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn try_apply_diffs_rejects_oversized_resize() {
        let diff = DiffSnapshot {
            pages: vec![PageDelta {
                gpa: MAX_DIFF_APPLY_BYTES,
                bytes: vec![0xFF],
            }],
            state: vec![],
        };
        assert!(matches!(
            try_apply_diffs(&[], &[diff]),
            Err(DiffError::ResizeTooLarge { .. })
        ));
    }

    #[test]
    fn try_apply_diffs_rejects_range_overflow() {
        let diff = DiffSnapshot {
            pages: vec![PageDelta {
                gpa: usize::MAX as u64,
                bytes: vec![0xFF; 2],
            }],
            state: vec![],
        };
        let result = try_apply_diffs(&[], &[diff]);
        assert!(matches!(result, Err(DiffError::RangeOverflow { .. })));
    }
}
