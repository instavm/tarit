//! virtio-blk request parsing + in-VMM backend (PRD §7).
//!
//! PRD §7: "Each volume = one virtio-blk device backed by a host file or
//! block device (raw or qcow-like). One request virtqueue per device;
//! requests follow the standard `virtio_blk_req` (type, sector, data,
//! status)."
//!
//! v1: in-VMM block backend serviced by the event-manager thread. The
//! request *parsing* (header → type/sector, status byte write) is pure
//! arithmetic and host-agnostic; the *servicing* (read/write against the
//! backing file) is Linux+KVM-gated.

use crate::persist::Persist;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// virtio-blk request types (virtio 1.x, §5.2.5).
pub mod req_type {
    pub const IN: u32 = 0; // read
    pub const OUT: u32 = 1; // write
    pub const FLUSH: u32 = 4;
    pub const GET_ID: u32 = 8;
    pub const DISCARD: u32 = 11;
    pub const WRITE_ZEROES: u32 = 13;
}

/// The virtio-blk request header as the guest lays it out in the descriptor
/// chain (virtio 1.x §5.2.5). 16 bytes: type (u32) + reserved (u32) +
/// sector (u64).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[repr(C)]
pub struct BlkReqHeader {
    pub req_type: u32,
    pub reserved: u32,
    pub sector: u64,
}

// SAFETY: `BlkReqHeader` is a `repr(C)` plain-data virtio layout made only of
// integer fields, with no invalid bit patterns.
unsafe impl vm_memory::ByteValued for BlkReqHeader {}

impl BlkReqHeader {
    /// 16 bytes — the fixed header size before the data buffer.
    pub const SIZE: usize = 16;

    /// Parse a header from the raw bytes the guest placed in the first
    /// readable descriptor of the chain. Returns None if the slice is too
    /// short (the guest is malformed / malicious).
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::SIZE {
            return None;
        }
        let req_type = u32::from_le_bytes(b[0..4].try_into().ok()?);
        let reserved = u32::from_le_bytes(b[4..8].try_into().ok()?);
        let sector = u64::from_le_bytes(b[8..16].try_into().ok()?);
        Some(Self {
            req_type,
            reserved,
            sector,
        })
    }

    /// Is this a read (device → guest) request?
    pub fn is_read(&self) -> bool {
        self.req_type == req_type::IN
    }

    /// Is this a write (guest → device) request?
    pub fn is_write(&self) -> bool {
        self.req_type == req_type::OUT || self.req_type == req_type::WRITE_ZEROES
    }
}

/// The status byte the device writes into the last (writable) descriptor
/// of the chain after a request completes (virtio 1.x §5.2.5).
pub mod status {
    pub const OK: u8 = 0;
    pub const IO_ERR: u8 = 1;
    pub const UNSUPP: u8 = 2;
}

/// A parsed virtio-blk request, ready to service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBlkReq {
    pub header: BlkReqHeader,
    /// Byte offset into the backing file this request targets.
    pub file_offset: u64,
    /// Length of the data buffer (in bytes).
    pub data_len: u64,
}

/// Errors from parsing a block request.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BlkParseError {
    #[error("header too short ({0} < {} bytes)", BlkReqHeader::SIZE)]
    HeaderTooShort(usize),
    #[error("unknown request type {0}")]
    UnknownReqType(u32),
    #[error("sector {sector} + sectors_req {n} overflows the backing ({sectors} sectors)")]
    OutOfBounds { sector: u64, n: u64, sectors: u64 },
    #[error("sector * 512 + data_len overflows u64")]
    Overflow,
}

/// Validate a parsed request against the device's `sectors` count and the
/// data-buffer length, returning the byte offset to read/write or an error.
///
/// This is the host-agnostic validation layer; the actual file I/O lives in
/// [`VirtioBlk::service`] (Linux+KVM, M7).
pub fn validate_req(
    header: &BlkReqHeader,
    data_len: u64,
    sectors: u64,
) -> Result<u64, BlkParseError> {
    // Only the well-known request types are accepted.
    match header.req_type {
        req_type::IN
        | req_type::OUT
        | req_type::FLUSH
        | req_type::GET_ID
        | req_type::DISCARD
        | req_type::WRITE_ZEROES => {}
        _ => return Err(BlkParseError::UnknownReqType(header.req_type)),
    }

    if header.req_type == req_type::GET_ID {
        return Ok(0); // GET_ID reads a fixed 20-byte serial string, no offset.
    }
    if header.req_type == req_type::FLUSH {
        return Ok(0); // FLUSH has no data buffer.
    }

    // Translate sector + (data_len / 512) into a bounds check.
    let n_sectors = data_len
        .checked_div(512)
        .ok_or(BlkParseError::Overflow)?
        .max(1);
    if header
        .sector
        .checked_add(n_sectors)
        .is_none_or(|end| end > sectors)
    {
        return Err(BlkParseError::OutOfBounds {
            sector: header.sector,
            n: n_sectors,
            sectors,
        });
    }

    header
        .sector
        .checked_mul(512)
        .ok_or(BlkParseError::Overflow)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlkState {
    pub backing_path: String,
    pub read_only: bool,
    pub sectors: u64,
}

pub struct VirtioBlk {
    pub backing: PathBuf,
    pub read_only: bool,
    pub sectors: u64,
}

impl VirtioBlk {
    pub fn new(backing: PathBuf, read_only: bool, sectors: u64) -> Self {
        Self {
            backing,
            read_only,
            sectors,
        }
    }
}

impl Persist for VirtioBlk {
    type State = BlkState;
    fn save(&self) -> Self::State {
        BlkState {
            backing_path: self.backing.to_string_lossy().to_string(),
            read_only: self.read_only,
            sectors: self.sectors,
        }
    }
    fn restore(&mut self, state: Self::State) {
        self.backing = PathBuf::from(state.backing_path);
        self.read_only = state.read_only;
        self.sectors = state.sectors;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(req_type: u32, sector: u64) -> BlkReqHeader {
        BlkReqHeader {
            req_type,
            reserved: 0,
            sector,
        }
    }

    #[test]
    fn header_round_trips_bytes() {
        let h = hdr(req_type::IN, 1234);
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&h.req_type.to_le_bytes());
        b[8..16].copy_from_slice(&h.sector.to_le_bytes());
        let parsed = BlkReqHeader::from_bytes(&b).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn header_too_short_returns_none() {
        assert!(BlkReqHeader::from_bytes(&[0; 15]).is_none());
    }

    #[test]
    fn read_req_at_edge_validates() {
        // 100 sectors, read 1 sector at the last sector (99).
        let h = hdr(req_type::IN, 99);
        assert_eq!(validate_req(&h, 512, 100).unwrap(), 99 * 512);
    }

    #[test]
    fn read_req_past_end_rejected() {
        let h = hdr(req_type::IN, 100);
        assert_eq!(
            validate_req(&h, 512, 100),
            Err(BlkParseError::OutOfBounds {
                sector: 100,
                n: 1,
                sectors: 100
            })
        );
    }

    #[test]
    fn write_zeroes_treated_as_write() {
        let h = hdr(req_type::WRITE_ZEROES, 5);
        assert!(h.is_write());
        validate_req(&h, 512, 100).unwrap();
    }

    #[test]
    fn unknown_req_type_rejected() {
        let h = hdr(999, 0);
        assert_eq!(
            validate_req(&h, 0, 100),
            Err(BlkParseError::UnknownReqType(999))
        );
    }

    #[test]
    fn flush_and_get_id_skip_bounds_check() {
        let flush = hdr(req_type::FLUSH, 999_999);
        let id = hdr(req_type::GET_ID, 999_999);
        validate_req(&flush, 0, 0).unwrap();
        validate_req(&id, 20, 0).unwrap();
    }

    #[test]
    fn sector_offset_overflow_rejected() {
        // sector near u64::MAX — sector*512 overflows.
        let h = hdr(req_type::IN, u64::MAX / 4);
        assert_eq!(
            validate_req(&h, 512, u64::MAX),
            Err(BlkParseError::Overflow)
        );
    }

    #[test]
    fn persist_round_trip() {
        let b = VirtioBlk::new(PathBuf::from("/dev/null"), true, 42);
        let st = b.save();
        let mut b2 = VirtioBlk::new(PathBuf::from("/x"), false, 0);
        b2.restore(st);
        assert_eq!(b2.backing, PathBuf::from("/dev/null"));
        assert!(b2.read_only);
        assert_eq!(b2.sectors, 42);
    }
}
