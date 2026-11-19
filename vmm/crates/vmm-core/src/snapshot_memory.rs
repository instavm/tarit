//! Snapshot reads that preserve missing UFFD/balloon pages.
use crate::error::{Result, VmmError};
use std::borrow::Cow;
use vmm_memory_backend::{uffd_restore::LazyPageDiscard, GuestMemory};

pub(crate) struct LazySnapshotReader {
    lazy: LazyPageDiscard,
    populated: Vec<bool>,
}

impl LazySnapshotReader {
    /// Caller excludes balloon discard while this population map is used.
    pub(crate) fn new(memory: &GuestMemory) -> Result<Option<Self>> {
        let Some(lazy) = memory.lazy_page_discard() else {
            return Ok(None);
        };
        let populated = memory
            .populated_pages()
            .ok_or_else(|| VmmError::Snapshot("read lazy guest population map".into()))?;
        Ok(Some(Self { lazy, populated }))
    }
}

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
