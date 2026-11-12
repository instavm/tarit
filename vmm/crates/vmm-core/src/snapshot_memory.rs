//! Snapshot reads that preserve missing UFFD/balloon pages.
use crate::error::{Result, VmmError};
#[cfg(feature = "boot")]
use std::borrow::Cow;
#[cfg(feature = "boot")]
use vmm_memory_backend::uffd_restore::LazyPageDiscard;
use vmm_memory_backend::GuestMemory;

#[cfg(feature = "boot")]
pub(crate) struct LazySnapshotReader {
    lazy: LazyPageDiscard,
    populated: Vec<bool>,
}

#[cfg(feature = "boot")]
impl LazySnapshotReader {
    /// Caller excludes balloon discard while this population map is used.
    ///
    /// `None` when RAM is not lazily restored, and also when the page map
    /// cannot be read (a jail without /proc): the snapshot then reads RAM
    /// through the mapping, as it did before this reader existed. That faults
    /// the missing pages in, which costs host memory but never correctness,
    /// so it must not fail the snapshot.
    pub(crate) fn new(memory: &GuestMemory) -> Option<Self> {
        let lazy = memory.lazy_page_discard()?;
        let Some(populated) = memory.populated_pages() else {
            log::warn!(
                "snapshot: guest page map unreadable; reading lazily restored RAM in full"
            );
            return None;
        };
        Some(Self { lazy, populated })
    }
}

#[cfg(feature = "boot")]
pub(crate) fn read_snapshot_bytes<'a>(
    memory: &'a [u8],
    reader: Option<&LazySnapshotReader>,
    start: usize,
    end: usize,
) -> Result<Cow<'a, [u8]>> {
    if start > end || end > memory.len() {
        return Err(VmmError::Snapshot(
            "snapshot memory range exceeds RAM".into(),
        ));
    }
    match reader {
        None => Ok(Cow::Borrowed(&memory[start..end])),
        Some(reader) => {
            let mut bytes = vec![0; end - start];
            reader
                .lazy
                .read_snapshot_range(start, &mut bytes, &reader.populated)
                .map_err(|error| {
                    VmmError::Snapshot(format!("read lazy snapshot range: {error}"))
                })?;
            Ok(Cow::Owned(bytes))
        }
    }
}

/// Capture at most one bounded live chunk under exclusion. The returned value
/// performs all restore-source I/O after this function releases the fence.
pub(crate) fn capture_live_range(
    memory: &GuestMemory,
    start: usize,
    end: usize,
) -> Result<vmm_memory_backend::uffd_restore::CapturedSnapshotRange> {
    let len = end
        .checked_sub(start)
        .filter(|len| *len <= 2 * 1024 * 1024)
        .ok_or_else(|| VmmError::Snapshot("live capture exceeds bounded chunk".into()))?;
    let lazy = memory
        .lazy_page_discard()
        .ok_or_else(|| VmmError::Snapshot("lazy capture without restore source".into()))?;
    let fence = lazy.snapshot_fence();
    let _guard = fence
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let populated = memory
        .populated_pages_range(start, len)
        .ok_or_else(|| VmmError::Snapshot("read bounded lazy population map".into()))?;
    lazy.capture_snapshot_range(start, len, populated)
        .map_err(|error| VmmError::Snapshot(format!("capture lazy snapshot range: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires permitted userfaultfd; run explicitly on Linux"]
    fn owned_live_capture_allows_balloon_before_output_and_preserves_bytes() {
        use std::io::Write as _;
        use std::sync::{mpsc, Arc};
        use std::time::Duration;
        const PAGE: usize = 4096;
        const LEN: usize = 16 * PAGE;
        let memory = Arc::new(GuestMemory::new(LEN as u64).unwrap());
        // SAFETY: live aligned mapping owned by memory.
        assert_eq!(
            unsafe {
                libc::madvise(
                    memory.as_ptr() as *mut libc::c_void,
                    LEN,
                    libc::MADV_NOHUGEPAGE,
                )
            },
            0
        );
        let mut source = tempfile::tempfile().unwrap();
        source.write_all(&vec![0x5a; LEN]).unwrap();
        let lazy = vmm_memory_backend::uffd_restore::start_lazy_restore(
            memory.as_ptr() as *mut u8,
            LEN,
            &source,
            0,
            LEN as u64,
            None,
        )
        .expect("explicit UFFD test requires userfaultfd");
        memory.set_lazy_page_discard(lazy.page_discard());
        memory.write_phys(PAGE as u64, &[0xa5; PAGE]).unwrap();
        memory
            .discard_range((2 * PAGE) as u64, PAGE as u64)
            .unwrap();
        let capture = capture_live_range(&memory, 0, LEN).unwrap();
        // Keep the owned capture alive, as with blocked restore-source/output I/O.
        // Discard must finish before that downstream work is allowed to proceed.
        let (sent, received) = mpsc::channel();
        let worker_memory = Arc::clone(&memory);
        let worker = std::thread::spawn(move || {
            sent.send(worker_memory.discard_range(PAGE as u64, PAGE as u64))
                .unwrap();
        });
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("balloon waited for snapshot output")
            .unwrap();
        worker.join().unwrap();
        let bytes = capture.finish().unwrap();
        assert_eq!(&bytes[PAGE..2 * PAGE], &[0xa5; PAGE]);
        assert_eq!(&bytes[2 * PAGE..3 * PAGE], &[0; PAGE]);
        let populated = memory.populated_pages().unwrap();
        assert!(
            !populated[1] && !populated[2],
            "finishing output must not undo discard"
        );
        // The discard is replayed by the next dirty round, yielding zero.
        let after = capture_live_range(&memory, PAGE, 2 * PAGE)
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(after, vec![0; PAGE]);
        assert!(memory.drain_host_dirty().len() >= 2);
        memory.clear_lazy_page_discard();
        drop(lazy);
    }
}
