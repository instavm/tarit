//! virtio-queue descriptor chain parsing + validation.
//!
//! Malformed, looping, or overlapping descriptor chains must be rejected
//! (fuzz-seeded). We delegate to
//! `virtio-queue`'s `Queue` for the real ring walking; this module provides
//! host-agnostic chain-validation primitives that are unit-testable without
//! KVM and a fuzz-seed corpus for the malformed-chain rejection tests.

use thiserror::Error;

/// Max descriptors in a chain we'll follow (a conservative cap).
pub const MAX_CHAIN_LEN: usize = 32;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum QueueError {
    #[error("descriptor chain too long (> {0}); possible malicious guest")]
    ChainTooLong(usize),
    #[error("looping descriptor chain at index {0}")]
    Loop(u16),
    #[error("chain head {0} is marked device-readable only (no writable buffer)")]
    NoWritable(u16),
    #[error("empty queue / no available descriptors")]
    Empty,
}

/// A descriptor as the guest lays it out in the descriptor table
/// (virtio v1.x §2.6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    /// Index of the next descriptor in the chain, or 0 when `flags & NEXT == 0`.
    pub next: u16,
}

pub mod flags {
    pub const NEXT: u16 = 1 << 0;
    pub const WRITE: u16 = 1 << 1;
    pub const INDIRECT: u16 = 1 << 2;
}

/// Follow a chain starting at `head`, calling `read_desc(idx) -> Descriptor`
/// for each entry, and return the list of visited indices in order.
///
/// Validates:
///   - chain length ≤ [`MAX_CHAIN_LEN`]
///   - no loops (no index visited twice)
///   - the chain has at least one writable descriptor
///
/// This is the host-agnostic core of what virtio-queue's `pop_descriptor_chain`
/// does; extracted here so the malformed-chain rejection tests
/// run on macOS without KVM.
pub fn validate_chain<F>(head: u16, mut read_desc: F) -> Result<Vec<u16>, QueueError>
where
    F: FnMut(u16) -> Option<Descriptor>,
{
    let mut visited = Vec::with_capacity(MAX_CHAIN_LEN);
    let mut has_writable = false;
    let mut idx = head;

    loop {
        // Loop detection: if we've already seen this index, the chain cycles.
        if visited.contains(&idx) {
            return Err(QueueError::Loop(idx));
        }
        if visited.len() >= MAX_CHAIN_LEN {
            return Err(QueueError::ChainTooLong(visited.len()));
        }

        let desc = read_desc(idx).ok_or(QueueError::Empty)?;
        if desc.flags & flags::WRITE != 0 {
            has_writable = true;
        }
        visited.push(idx);

        if desc.flags & flags::NEXT == 0 {
            break;
        }
        idx = desc.next;
    }

    if !has_writable {
        return Err(QueueError::NoWritable(head));
    }
    Ok(visited)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn table(entries: &[(u64, u32, u16, u16)]) -> HashMap<u16, Descriptor> {
        entries
            .iter()
            .enumerate()
            .map(|(i, &(addr, len, flags, next))| {
                (
                    i as u16,
                    Descriptor {
                        addr,
                        len,
                        flags,
                        next,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn simple_two_descriptor_chain_validates() {
        // Index 0: readable, NEXT → 1. Index 1: writable, no NEXT.
        let t = table(&[
            (0x1000, 0x10, flags::NEXT, 1),
            (0x2000, 0x10, flags::WRITE, 0),
        ]);
        let chain = validate_chain(0, |i| t.get(&i).copied()).unwrap();
        assert_eq!(chain, vec![0, 1]);
    }

    #[test]
    fn loop_is_rejected() {
        // 0 → 1 → 0 (cycle).
        let t = table(&[
            (0x1000, 0x10, flags::NEXT | flags::WRITE, 1),
            (0x2000, 0x10, flags::NEXT | flags::WRITE, 0),
        ]);
        assert_eq!(
            validate_chain(0, |i| t.get(&i).copied()),
            Err(QueueError::Loop(0))
        );
    }

    #[test]
    fn too_long_chain_is_rejected() {
        // A chain longer than MAX_CHAIN_LEN: 0 → 1 → 2 → ... → MAX+1, each
        // writable to avoid the NoWritable error.
        let mut t = HashMap::new();
        for i in 0..(MAX_CHAIN_LEN + 5) as u16 {
            t.insert(
                i,
                Descriptor {
                    addr: 0x1000 + i as u64 * 0x10,
                    len: 0x10,
                    flags: flags::NEXT | flags::WRITE,
                    next: i + 1,
                },
            );
        }
        assert!(matches!(
            validate_chain(0, |i| t.get(&i).copied()),
            Err(QueueError::ChainTooLong(_))
        ));
    }

    #[test]
    fn chain_with_no_writable_buffer_rejected() {
        // Every descriptor readable-only.
        let t = table(&[
            (0x1000, 0x10, flags::NEXT, 1),
            (0x2000, 0x10, 0, 0), // no WRITE
        ]);
        assert_eq!(
            validate_chain(0, |i| t.get(&i).copied()),
            Err(QueueError::NoWritable(0))
        );
    }

    #[test]
    fn single_writable_descriptor_chain_validates() {
        let t = table(&[(0x1000, 0x10, flags::WRITE, 0)]);
        let chain = validate_chain(0, |i| t.get(&i).copied()).unwrap();
        assert_eq!(chain, vec![0]);
    }

    /// A mini property test: random chains must either validate (no loop,
    /// length ≤ MAX, has writable) or return a specific error — never panic.
    #[test]
    fn random_chains_never_panic() {
        use std::collections::HashMap;
        // Deterministic pseudo-random (no rand dep): LCG with a fixed seed.
        let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };

        for _ in 0..1000 {
            let table_size = 1 + (next() % 8) as u16;
            let mut t = HashMap::new();
            for i in 0..table_size {
                let flags = if next() % 2 == 0 { flags::WRITE } else { 0 }
                    | if next() % 3 == 0 { flags::NEXT } else { 0 };
                let next_idx = if flags & flags::NEXT != 0 {
                    (next() % table_size as u64) as u16
                } else {
                    0
                };
                t.insert(
                    i,
                    Descriptor {
                        addr: 0x1000 + i as u64 * 0x10,
                        len: 0x10,
                        flags,
                        next: next_idx,
                    },
                );
            }
            let head = (next() % table_size as u64) as u16;
            // Must return Ok or Err, never panic.
            let _ = validate_chain(head, |i| t.get(&i).copied());
        }
    }

    /// A long-but-non-looping chain exactly at the limit must validate.
    #[test]
    fn chain_at_max_length_validates() {
        let mut t = HashMap::new();
        for i in 0..(MAX_CHAIN_LEN as u16) {
            t.insert(
                i,
                Descriptor {
                    addr: 0x1000 + i as u64 * 0x10,
                    len: 0x10,
                    flags: if i < (MAX_CHAIN_LEN - 1) as u16 {
                        flags::NEXT | flags::WRITE
                    } else {
                        flags::WRITE
                    },
                    next: i + 1,
                },
            );
        }
        let chain = validate_chain(0, |i| t.get(&i).copied()).unwrap();
        assert_eq!(chain.len(), MAX_CHAIN_LEN);
    }
}
