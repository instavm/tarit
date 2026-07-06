//! Snapshot file format — a versioned, CRC'd container.
//!
//! Layout (v1):
//!   ```text
//!   +------------------+
//!   | magic "VMSN"     |  4 bytes
//!   | version: u16     |  2 bytes
//!   | flags: u16       |  2 bytes   (bit0 = diff)
//!   | state_len: u64   |  8 bytes
//!   | state_crc: u32   |  4 bytes
//!   | mem_len: u64     |  8 bytes
//!   | mem_crc: u32     |  4 bytes
//!   +------------------+
//!   | state blob       |  state_len bytes (postcard of device states)
//!   +------------------+
//!   | mem pages        |  mem_len bytes (full or diff)
//!   +------------------+
//!   ```

use serde::{Deserialize, Serialize};

pub const MAGIC: &[u8; 4] = b"VMSN";
pub const VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flags(pub u16);
impl Flags {
    pub const DIFF: u16 = 1 << 0;
    pub fn contains(self, bit: u16) -> bool {
        self.0 & bit != 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub magic: [u8; 4],
    pub version: u16,
    pub flags: u16,
    pub state_len: u64,
    pub state_crc: u32,
    pub mem_len: u64,
    pub mem_crc: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub meta: SnapshotMeta,
    pub state: Vec<u8>,
    pub mem: Vec<u8>,
}

impl SnapshotMeta {
    pub fn new(state_len: u64, state_crc: u32, mem_len: u64, mem_crc: u32, diff: bool) -> Self {
        Self {
            magic: *MAGIC,
            version: VERSION,
            flags: if diff { Flags::DIFF } else { 0 },
            state_len,
            state_crc,
            mem_len,
            mem_crc,
        }
    }

    pub fn is_diff(&self) -> bool {
        Flags(self.flags).contains(Flags::DIFF)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_new_sets_magic_and_version() {
        let m = SnapshotMeta::new(10, 0xAAAA, 20, 0xBBBB, false);
        assert_eq!(m.magic, *MAGIC);
        assert_eq!(m.version, VERSION);
        assert_eq!(m.state_len, 10);
        assert_eq!(m.state_crc, 0xAAAA);
        assert_eq!(m.mem_len, 20);
        assert_eq!(m.mem_crc, 0xBBBB);
        assert!(!m.is_diff());
    }

    #[test]
    fn meta_diff_flag_round_trips() {
        let m = SnapshotMeta::new(0, 0, 0, 0, true);
        assert!(m.is_diff());
        let m2 = SnapshotMeta::new(0, 0, 0, 0, false);
        assert!(!m2.is_diff());
    }

    #[test]
    fn flags_contains_is_bitwise() {
        assert!(Flags(Flags::DIFF).contains(Flags::DIFF));
        assert!(!Flags(0).contains(Flags::DIFF));
        // Composite flags.
        let composite = Flags(Flags::DIFF | 0x10);
        assert!(composite.contains(Flags::DIFF));
        assert!(composite.contains(0x10));
        assert!(!composite.contains(0x20));
    }

    #[test]
    fn meta_serde_round_trip() {
        let m = SnapshotMeta::new(100, 0xDEAD_BEEF, 200, 0xCAFE_F00D, true);
        let s = serde_json::to_string(&m).unwrap();
        let back: SnapshotMeta = serde_json::from_str(&s).unwrap();
        assert_eq!(back.magic, m.magic);
        assert_eq!(back.version, m.version);
        assert_eq!(back.flags, m.flags);
        assert_eq!(back.state_crc, m.state_crc);
        assert_eq!(back.mem_crc, m.mem_crc);
        assert!(back.is_diff());
    }
}
