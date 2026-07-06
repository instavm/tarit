//! Restore — create a fresh VMM, load device + vCPU state from the state
//! file, back guest RAM with the memory file.
//!
//! PRD §9a: two strategies — eager copy (read whole mem file in) or lazy via
//! UFFD (register guest memory with userfaultfd; an external page-fault
//! handler resolves each fault with a single UFFDIO_COPY from the snapshot
//! mapping — no file-I/O syscalls in the hot path).

use crate::crc::verify;
use crate::format::Snapshot;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RestoreError {
    #[error("magic mismatch: expected {expected:?}, got {got:?}")]
    BadMagic { expected: [u8; 4], got: [u8; 4] },
    #[error("version {0} not supported")]
    BadVersion(u16),
    #[error("state crc mismatch: expected {expected}, got {got}")]
    StateCrc { expected: u32, got: u32 },
    #[error("mem crc mismatch: expected {expected}, got {got}")]
    MemCrc { expected: u32, got: u32 },
    #[error("state length mismatch: expected {expected}, got {got}")]
    StateLength { expected: u64, got: u64 },
    #[error("mem length mismatch: expected {expected}, got {got}")]
    MemLength { expected: u64, got: u64 },
}

pub fn validate(snap: &Snapshot) -> Result<(), RestoreError> {
    if snap.meta.magic != *crate::format::MAGIC {
        return Err(RestoreError::BadMagic {
            expected: *crate::format::MAGIC,
            got: snap.meta.magic,
        });
    }
    if snap.meta.version != crate::format::VERSION {
        return Err(RestoreError::BadVersion(snap.meta.version));
    }
    let state_len = u64::try_from(snap.state.len()).map_err(|_| RestoreError::StateLength {
        expected: snap.meta.state_len,
        got: u64::MAX,
    })?;
    if snap.meta.state_len != state_len {
        return Err(RestoreError::StateLength {
            expected: snap.meta.state_len,
            got: state_len,
        });
    }
    let mem_len = u64::try_from(snap.mem.len()).map_err(|_| RestoreError::MemLength {
        expected: snap.meta.mem_len,
        got: u64::MAX,
    })?;
    if snap.meta.mem_len != mem_len {
        return Err(RestoreError::MemLength {
            expected: snap.meta.mem_len,
            got: mem_len,
        });
    }
    if !verify(&snap.state, snap.meta.state_crc) {
        return Err(RestoreError::StateCrc {
            expected: snap.meta.state_crc,
            got: crate::crc::crc32(&snap.state),
        });
    }
    if !verify(&snap.mem, snap.meta.mem_crc) {
        return Err(RestoreError::MemCrc {
            expected: snap.meta.mem_crc,
            got: crate::crc::crc32(&snap.mem),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{create, SnapshotInput};

    #[test]
    fn valid_snapshot_passes() {
        let snap = create(SnapshotInput {
            state_blob: b"ok",
            mem: b"ok",
            diff: false,
        });
        validate(&snap).unwrap();
    }

    #[test]
    fn tampered_state_rejected() {
        let mut snap = create(SnapshotInput {
            state_blob: b"ok",
            mem: b"ok",
            diff: false,
        });
        // Flip bits without changing the length, so the CRC check (not the
        // length check) is what rejects it (PRD §12.6).
        snap.state = b"no".to_vec();
        assert!(matches!(
            validate(&snap),
            Err(RestoreError::StateCrc { .. })
        ));
    }

    #[test]
    fn tampered_magic_rejected() {
        let mut snap = create(SnapshotInput {
            state_blob: b"ok",
            mem: b"ok",
            diff: false,
        });
        snap.meta.magic = *b"XXXX";
        assert!(matches!(
            validate(&snap),
            Err(RestoreError::BadMagic { .. })
        ));
    }

    #[test]
    fn tampered_mem_rejected() {
        // PRD §12.6: "corrupt state file / flip bits → restore must detect
        // (CRC) and refuse." This covers the memory half.
        let mut snap = create(SnapshotInput {
            state_blob: b"ok",
            mem: b"original",
            diff: false,
        });
        // Same-length bit-flip so the CRC check (not the length check) rejects it.
        snap.mem = b"tampered".to_vec();
        assert!(matches!(validate(&snap), Err(RestoreError::MemCrc { .. })));
    }

    #[test]
    fn mismatched_lengths_rejected_before_crc() {
        let mut snap = create(SnapshotInput {
            state_blob: b"state",
            mem: b"mem",
            diff: false,
        });
        snap.meta.state_len += 1;
        assert!(matches!(
            validate(&snap),
            Err(RestoreError::StateLength { .. })
        ));

        let mut snap = create(SnapshotInput {
            state_blob: b"state",
            mem: b"mem",
            diff: false,
        });
        snap.meta.mem_len += 1;
        assert!(matches!(
            validate(&snap),
            Err(RestoreError::MemLength { .. })
        ));
    }

    #[test]
    fn unsupported_version_rejected() {
        let mut snap = create(SnapshotInput {
            state_blob: b"ok",
            mem: b"ok",
            diff: false,
        });
        snap.meta.version = 999;
        assert!(matches!(
            validate(&snap),
            Err(RestoreError::BadVersion(999))
        ));
    }

    #[test]
    fn diff_snapshot_validates() {
        let snap = create(SnapshotInput {
            state_blob: b"diff-state",
            mem: b"diff-pages",
            diff: true,
        });
        assert!(snap.meta.is_diff());
        validate(&snap).unwrap();
    }

    #[test]
    fn large_snapshot_validates() {
        // A realistic-sized state + mem: 1 MiB state, 4 MiB mem.
        let state = vec![0xAB; 1024 * 1024];
        let mem = vec![0xCD; 4 * 1024 * 1024];
        let snap = create(SnapshotInput {
            state_blob: &state,
            mem: &mem,
            diff: false,
        });
        assert_eq!(snap.meta.state_len, 1024 * 1024);
        assert_eq!(snap.meta.mem_len, 4 * 1024 * 1024);
        validate(&snap).unwrap();
    }
}
