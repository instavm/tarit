//! Snapshot creation — pause vCPUs, collect device state, dump memory.
//!
//! PRD §9a: "pause vCPUs → serialize device state (every device implements a
//! `save`/`Persist`-style trait) + vCPU/KVM state into a small state file,
//! and dump guest RAM to a memory file."

use crate::crc::crc32;
use crate::format::{Snapshot, SnapshotMeta};
use vmm_memory_backend::GuestMemory;

pub struct SnapshotInput<'a> {
    /// Postcard-serialized device + vCPU state.
    pub state_blob: &'a [u8],
    /// Guest RAM (raw pages). Empty for a diff snapshot.
    pub mem: &'a [u8],
    /// True if `mem` is a diff (only dirty pages) rather than a full dump.
    pub diff: bool,
}

pub fn create(input: SnapshotInput) -> Snapshot {
    let state_crc = crc32(input.state_blob);
    let mem_crc = crc32(input.mem);
    let meta = SnapshotMeta::new(
        input.state_blob.len() as u64,
        state_crc,
        input.mem.len() as u64,
        mem_crc,
        input.diff,
    );
    Snapshot {
        meta,
        state: input.state_blob.to_vec(),
        mem: input.mem.to_vec(),
    }
}

/// Dump the guest RAM to a Vec. On Linux this is a direct read from the
/// mmap'd region; on KVM we first pause vCPUs and (for diff) consult the
/// dirty bitmap.
pub fn dump_memory(guest: &GuestMemory) -> Vec<u8> {
    let mut out = Vec::with_capacity(guest.size_bytes as usize);
    // SAFETY: we read from the mmap'd region whose size is `size_bytes`.
    // `GuestMemoryMmap` owns the mapping; reading `[..size]` is in-bounds.
    let slice = unsafe { std::slice::from_raw_parts(guest.as_ptr(), guest.size_bytes as usize) };
    out.extend_from_slice(slice);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_snapshot_roundtrips_meta() {
        let snap = create(SnapshotInput {
            state_blob: b"state",
            mem: b"mem",
            diff: false,
        });
        assert_eq!(snap.meta.magic, *b"VMSN");
        assert_eq!(snap.meta.version, 1);
        assert!(!snap.meta.is_diff());
        assert_eq!(snap.meta.state_len, 5);
        assert_eq!(snap.meta.mem_len, 3);
        assert!(crate::crc::verify(b"state", snap.meta.state_crc));
        assert!(crate::crc::verify(b"mem", snap.meta.mem_crc));
    }

    #[test]
    fn diff_flag_set() {
        let snap = create(SnapshotInput {
            state_blob: b"",
            mem: b"",
            diff: true,
        });
        assert!(snap.meta.is_diff());
    }
}
