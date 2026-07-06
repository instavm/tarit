//! CRC32 wrapper for the state-file integrity check (PRD §9a, §12.6).
//!
//! "Firecracker validates the state file with a CRC and persists devices via
//! a `Persist` trait; we mirror this." Tampering with the state file must be
//! detected and refused, not executed in an invalid state.

use crc32fast::Hasher;

/// Compute CRC32 of `bytes`.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut h = Hasher::new();
    h.update(bytes);
    h.finalize()
}

/// Verify that `bytes` matches the expected `expected` CRC; returns true if OK.
pub fn verify(bytes: &[u8], expected: u32) -> bool {
    crc32(bytes) == expected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_round_trip() {
        let data = b"hello vmm";
        let h = crc32(data);
        assert!(verify(data, h));
        assert!(!verify(b"tampered", h));
    }

    #[test]
    fn empty_input() {
        let _ = crc32(b"");
    }
}
