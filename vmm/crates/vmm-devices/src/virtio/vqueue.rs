//! Virtqueue processing — walks the guest's descriptor ring and dispatches
//! requests to the device backend (blk, net, etc.).

// Not gated to Linux — the queue walker is pure memory operations that work
// on any platform (for unit testing). The actual I/O backends are Linux-only.

use serde::{Deserialize, Serialize};
use vm_memory::{
    Address, ByteValued, Bytes as _, GuestAddress, GuestMemoryBackend, GuestMemoryMmap,
};
use vmm_memory_backend::dirty::SoftwareDirtyBitmap;

/// Maximum virtio-mmio queue size this VMM offers to guests.
pub const MAX_QUEUE_SIZE: u16 = 256;

/// Maximum per-descriptor length (1 MiB). Descriptors exceeding this are
/// rejected to prevent guest-driven OOM allocation attacks.
const MAX_DESC_LEN: u32 = 1024 * 1024;

/// Maximum total bytes across all descriptors in a single chain (4 MiB).
const MAX_CHAIN_BYTES: usize = 4 * 1024 * 1024;

/// Validate a guest-selected QUEUE_NUM against the device-advertised cap.
pub fn is_valid_queue_size(size: u16, max_size: u16) -> bool {
    size != 0 && size <= max_size && size.is_power_of_two()
}

/// Read a ByteValued type from guest memory via bounds-checked vm-memory API.
fn read_obj_mem<T: ByteValued>(mem: &GuestMemoryMmap, addr: GuestAddress) -> Option<T> {
    mem.read_obj(addr).ok()
}

/// Write a ByteValued type to guest memory via bounds-checked vm-memory API.
fn write_obj_mem<T: ByteValued>(
    mem: &GuestMemoryMmap,
    dirty: Option<&SoftwareDirtyBitmap>,
    val: T,
    addr: GuestAddress,
) -> bool {
    if mem.write_obj(val, addr).is_ok() {
        if let Some(dirty) = dirty {
            dirty.mark_range(addr.raw_value(), std::mem::size_of::<T>() as u64);
        }
        true
    } else {
        false
    }
}

/// Read a slice from guest memory via bounds-checked vm-memory API.
fn read_slice_mem(mem: &GuestMemoryMmap, buf: &mut [u8], addr: GuestAddress) -> bool {
    mem.read_slice(buf, addr).is_ok()
}

/// Write a slice to guest memory via bounds-checked vm-memory API.
fn write_slice_mem(
    mem: &GuestMemoryMmap,
    dirty: Option<&SoftwareDirtyBitmap>,
    buf: &[u8],
    addr: GuestAddress,
) -> bool {
    if mem.write_slice(buf, addr).is_ok() {
        if let Some(dirty) = dirty {
            dirty.mark_range(addr.raw_value(), buf.len() as u64);
        }
        true
    } else {
        false
    }
}

fn checked_add_u64(lhs: u64, rhs: u64, what: &str) -> Option<u64> {
    match lhs.checked_add(rhs) {
        Some(v) => Some(v),
        None => {
            log::warn!("vqueue: {what} overflow — rejecting");
            None
        }
    }
}

fn checked_mul_u64(lhs: u64, rhs: u64, what: &str) -> Option<u64> {
    match lhs.checked_mul(rhs) {
        Some(v) => Some(v),
        None => {
            log::warn!("vqueue: {what} overflow — rejecting");
            None
        }
    }
}

fn checked_guest_addr(base: u64, offset: u64, what: &str) -> Option<GuestAddress> {
    checked_add_u64(base, offset, what).map(GuestAddress)
}

fn guest_range_is_valid(mem: &GuestMemoryMmap, addr: u64, len: u64, what: &str) -> bool {
    if len == 0 {
        return true;
    }

    let Ok(len_usize) = usize::try_from(len) else {
        log::warn!("vqueue: {what} length {len} does not fit usize — rejecting");
        return false;
    };
    let Some(last_offset) = len.checked_sub(1) else {
        return true;
    };
    if checked_add_u64(addr, last_offset, what).is_none() {
        return false;
    }
    if !GuestMemoryBackend::check_range(mem, GuestAddress(addr), len_usize) {
        log::warn!("vqueue: {what} at 0x{addr:x} len={len} is outside guest memory");
        return false;
    }
    true
}

/// The virtio descriptor (16 bytes, from virtio 1.x §2.6.5).
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

// SAFETY: `Descriptor` is a `repr(C)` plain-data virtio layout made only of
// integer fields, with no invalid bit patterns.
unsafe impl ByteValued for Descriptor {}

/// virtio descriptor flags.
pub mod desc_flags {
    pub const NEXT: u16 = 1 << 0;
    pub const WRITE: u16 = 1 << 1;
    pub const INDIRECT: u16 = 1 << 2;
}

/// The avail ring header (virtio 1.x §2.7.6).
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct AvailRing {
    pub flags: u16,
    pub idx: u16,
}

// SAFETY: `AvailRing` is a `repr(C)` pair of integer fields with no invalid bit
// patterns, matching the virtio avail ring header layout.
unsafe impl ByteValued for AvailRing {}

/// The used ring header (virtio 1.x §2.7.8).
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct UsedRing {
    pub flags: u16,
    pub idx: u16,
}

// SAFETY: `UsedRing` is a `repr(C)` pair of integer fields with no invalid bit
// patterns, matching the virtio used ring header layout.
unsafe impl ByteValued for UsedRing {}

/// A used ring entry.
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct UsedElem {
    pub id: u32,
    pub len: u32,
}

// SAFETY: `UsedElem` is a `repr(C)` pair of integer fields with no invalid bit
// patterns, matching the virtio used ring entry layout.
unsafe impl ByteValued for UsedElem {}

/// The configuration the guest writes to set up a virtqueue.
#[derive(Debug, Clone, Default)]
pub struct QueueConfig {
    /// Size of the queue (power of 2, max 256 for virtio-mmio).
    pub size: u16,
    /// Guest physical address of the descriptor table.
    pub desc_table_addr: u64,
    /// Guest physical address of the avail ring.
    pub avail_ring_addr: u64,
    /// Guest physical address of the used ring.
    pub used_ring_addr: u64,
    /// Whether the guest has enabled the queue (DRIVER_OK + queue_ready).
    pub ready: bool,
}

impl QueueConfig {
    fn has_valid_size(&self) -> bool {
        is_valid_queue_size(self.size, MAX_QUEUE_SIZE)
    }
}

struct DescriptorChain {
    readable: Vec<(u64, u32)>,
    writable: Vec<(u64, u32)>,
    len: u32,
}

/// A virtqueue processor. Walks the avail ring, processes descriptor chains,
/// and updates the used ring.
pub struct VirtQueueProcessor {
    config: QueueConfig,
    last_avail_idx: u16,
    last_used_idx: u16,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VirtQueueProcessorState {
    pub last_avail_idx: u16,
    pub last_used_idx: u16,
}

impl VirtQueueProcessor {
    pub fn new(config: QueueConfig) -> Self {
        Self::new_with_indices(config, 0, 0)
    }

    pub fn new_with_indices(config: QueueConfig, last_avail_idx: u16, last_used_idx: u16) -> Self {
        Self {
            config,
            last_avail_idx,
            last_used_idx,
        }
    }

    pub fn from_state(config: QueueConfig, state: VirtQueueProcessorState) -> Self {
        Self {
            config,
            last_avail_idx: state.last_avail_idx,
            last_used_idx: state.last_used_idx,
        }
    }

    pub fn save_state(&self) -> VirtQueueProcessorState {
        VirtQueueProcessorState {
            last_avail_idx: self.last_avail_idx,
            last_used_idx: self.last_used_idx,
        }
    }

    /// Update the queue config (called when the guest writes to the
    /// queue config registers). Does NOT reset indices — the processor
    /// tracks its position across calls.
    pub fn update_config(&mut self, config: QueueConfig) {
        self.config = config;
    }

    fn queue_is_usable(&self, mem: &GuestMemoryMmap, caller: &str) -> bool {
        if !self.config.ready || self.config.size == 0 {
            log::debug!(
                "{caller}: not ready or size=0 (ready={}, size={})",
                self.config.ready,
                self.config.size
            );
            return false;
        }
        if !self.config.has_valid_size() {
            log::warn!(
                "{caller}: invalid queue size {} (must be power-of-two <= {MAX_QUEUE_SIZE})",
                self.config.size
            );
            return false;
        }

        let desc_bytes = match checked_mul_u64(
            self.config.size as u64,
            std::mem::size_of::<Descriptor>() as u64,
            "descriptor table size",
        ) {
            Some(v) => v,
            None => return false,
        };
        let avail_bytes = match checked_add_u64(
            4,
            match checked_mul_u64(self.config.size as u64, 2, "avail ring size") {
                Some(v) => v,
                None => return false,
            },
            "avail ring size",
        ) {
            Some(v) => v,
            None => return false,
        };
        let used_bytes = match checked_add_u64(
            4,
            match checked_mul_u64(
                self.config.size as u64,
                std::mem::size_of::<UsedElem>() as u64,
                "used ring size",
            ) {
                Some(v) => v,
                None => return false,
            },
            "used ring size",
        ) {
            Some(v) => v,
            None => return false,
        };

        guest_range_is_valid(
            mem,
            self.config.desc_table_addr,
            desc_bytes,
            "descriptor table",
        ) && guest_range_is_valid(mem, self.config.avail_ring_addr, avail_bytes, "avail ring")
            && guest_range_is_valid(mem, self.config.used_ring_addr, used_bytes, "used ring")
    }

    fn avail_entry_addr(&self) -> Option<GuestAddress> {
        let ring_slot = (self.last_avail_idx % self.config.size) as u64;
        let ring_off = checked_mul_u64(ring_slot, 2, "avail ring entry offset")?;
        let off = checked_add_u64(4, ring_off, "avail ring entry offset")?;
        checked_guest_addr(self.config.avail_ring_addr, off, "avail ring entry address")
    }

    fn desc_addr(&self, desc_idx: u16) -> Option<GuestAddress> {
        let off = checked_mul_u64(
            desc_idx as u64,
            std::mem::size_of::<Descriptor>() as u64,
            "descriptor table offset",
        )?;
        checked_guest_addr(self.config.desc_table_addr, off, "descriptor address")
    }

    fn used_entry_addr(&self) -> Option<GuestAddress> {
        let ring_slot = (self.last_used_idx % self.config.size) as u64;
        let ring_off = checked_mul_u64(
            ring_slot,
            std::mem::size_of::<UsedElem>() as u64,
            "used ring entry offset",
        )?;
        let off = checked_add_u64(4, ring_off, "used ring entry offset")?;
        checked_guest_addr(self.config.used_ring_addr, off, "used ring entry address")
    }

    fn used_idx_addr(&self) -> Option<GuestAddress> {
        checked_guest_addr(self.config.used_ring_addr, 2, "used ring idx address")
    }

    fn walk_descriptor_chain(
        &self,
        mem: &GuestMemoryMmap,
        head_desc_idx: u16,
    ) -> Option<DescriptorChain> {
        if head_desc_idx >= self.config.size {
            log::warn!(
                "vqueue: head descriptor {head_desc_idx} >= queue size {} — rejecting chain",
                self.config.size
            );
            return None;
        }

        let mut desc_idx = head_desc_idx;
        let mut readable = Vec::new();
        let mut writable = Vec::new();
        let mut visited = vec![false; self.config.size as usize];
        let mut chain_bytes = 0usize;
        let mut chain_len = 0usize;

        loop {
            if desc_idx >= self.config.size {
                log::warn!(
                    "vqueue: desc_idx {desc_idx} >= queue size {} — rejecting chain",
                    self.config.size
                );
                return None;
            }
            let visited_slot = desc_idx as usize;
            if visited[visited_slot] {
                log::warn!("vqueue: descriptor loop at index {desc_idx} — rejecting chain");
                return None;
            }
            if chain_len >= self.config.size as usize {
                log::warn!(
                    "vqueue: descriptor chain exceeds queue size {} — rejecting chain",
                    self.config.size
                );
                return None;
            }
            visited[visited_slot] = true;

            let desc_addr = self.desc_addr(desc_idx)?;
            let desc: Descriptor = match read_obj_mem::<Descriptor>(mem, desc_addr) {
                Some(d) => d,
                None => {
                    log::warn!("vqueue: descriptor read failed at index {desc_idx}");
                    return None;
                }
            };

            if desc.flags & desc_flags::INDIRECT != 0 {
                log::warn!("vqueue: indirect descriptors are not negotiated — rejecting chain");
                return None;
            }
            if desc.len > MAX_DESC_LEN {
                log::warn!(
                    "vqueue: desc.len {} exceeds max {} — rejecting chain",
                    desc.len,
                    MAX_DESC_LEN
                );
                return None;
            }

            let next_chain_bytes = match chain_bytes.checked_add(desc.len as usize) {
                Some(v) => v,
                None => {
                    log::warn!("vqueue: descriptor chain byte count overflow — rejecting chain");
                    return None;
                }
            };
            if next_chain_bytes > MAX_CHAIN_BYTES {
                log::warn!(
                    "vqueue: chain total {next_chain_bytes} exceeds max {MAX_CHAIN_BYTES} — rejecting"
                );
                return None;
            }
            chain_bytes = next_chain_bytes;

            if !guest_range_is_valid(mem, desc.addr, desc.len as u64, "descriptor buffer") {
                return None;
            }

            if desc.flags & desc_flags::WRITE != 0 {
                writable.push((desc.addr, desc.len));
            } else {
                readable.push((desc.addr, desc.len));
            }
            chain_len += 1;

            if desc.flags & desc_flags::NEXT == 0 {
                break;
            }
            desc_idx = desc.next;
        }

        Some(DescriptorChain {
            readable,
            writable,
            len: chain_len as u32,
        })
    }

    fn reject_available_chain(&mut self) {
        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
    }

    /// Process all available requests on the queue. Returns the number
    /// of requests processed.
    ///
    /// For each descriptor chain:
    /// 1. Read the descriptor at the avail ring index
    /// 2. Walk the chain (following NEXT flags)
    /// 3. Classify descriptors: readable (request header + data) vs writable (status + data)
    /// 4. Call `handler` with the readable data and writable buffers
    /// 5. Write the status to the writable status descriptor
    /// 6. Update the used ring
    pub fn process_queue<F>(&mut self, mem: &GuestMemoryMmap, handler: F) -> usize
    where
        F: FnMut(&[u8], &mut [u8]) -> u8,
    {
        self.process_queue_dirty(mem, None, handler)
    }

    pub fn process_queue_dirty<F>(
        &mut self,
        mem: &GuestMemoryMmap,
        dirty: Option<&SoftwareDirtyBitmap>,
        mut handler: F,
    ) -> usize
    where
        F: FnMut(&[u8], &mut [u8]) -> u8,
    {
        if !self.queue_is_usable(mem, "process_queue") {
            return 0;
        }

        let mut processed = 0;

        // Read the avail ring header.
        let avail_addr = GuestAddress(self.config.avail_ring_addr);
        let avail: AvailRing = match read_obj_mem::<AvailRing>(mem, avail_addr) {
            Some(a) => a,
            None => {
                log::warn!(
                    "process_queue: failed to read avail ring at 0x{:x}",
                    self.config.avail_ring_addr
                );
                return 0;
            }
        };

        // Process all new available entries.
        while self.last_avail_idx != avail.idx {
            // Get the descriptor index from the avail ring.
            // avail ring layout: [flags: u16][idx: u16][ring: u16 * size]
            let avail_entry_addr = match self.avail_entry_addr() {
                Some(addr) => addr,
                None => break,
            };
            let head_desc_idx: u16 = match read_obj_mem::<u16>(mem, avail_entry_addr) {
                Some(v) => v,
                None => break,
            };

            // Walk the descriptor chain starting at head_desc_idx.
            let chain = match self.walk_descriptor_chain(mem, head_desc_idx) {
                Some(chain) => chain,
                None => {
                    self.reject_available_chain();
                    continue;
                }
            };

            // For virtio-blk: readable[0] = request header, readable[1..] = data (for write)
            //                  writable[0..-1] = data (for read), writable[-1] = status byte.
            // The handler receives: (readable_data, writable_data) and returns a status byte.

            // Collect readable data.
            let mut readable_data: Vec<u8> = Vec::new();
            for (addr, len) in &chain.readable {
                let mut buf = vec![0u8; *len as usize];
                if !read_slice_mem(mem, &mut buf, GuestAddress(*addr)) {
                    log::warn!("vqueue: read_slice OOB at 0x{addr:x} len={len} — dropping");
                    readable_data.clear();
                    break;
                }
                readable_data.extend_from_slice(&buf);
            }

            // Collect writable data (all but the last descriptor, which is the status byte).
            let mut writable_data: Vec<u8> = Vec::new();
            let status_addr: u64 = if !chain.writable.is_empty() {
                chain.writable.last().map(|(a, _)| *a).unwrap_or(0)
            } else {
                0
            };

            for (addr, len) in chain
                .writable
                .iter()
                .take(chain.writable.len().saturating_sub(1))
            {
                let mut buf = vec![0u8; *len as usize];
                if !read_slice_mem(mem, &mut buf, GuestAddress(*addr)) {
                    log::warn!(
                        "vqueue: read_slice OOB (writable) at 0x{addr:x} len={len} — dropping"
                    );
                    writable_data.clear();
                    break;
                }
                writable_data.extend_from_slice(&buf);
            }

            // Call the handler.
            let status_byte = handler(&readable_data, &mut writable_data);

            // Write the writable data back to guest memory.
            let mut offset = 0u64;
            for (addr, len) in chain
                .writable
                .iter()
                .take(chain.writable.len().saturating_sub(1))
            {
                let Some(desc_end) = checked_add_u64(offset, *len as u64, "writable data offset")
                else {
                    break;
                };
                let end = desc_end.min(writable_data.len() as u64);
                if offset < end
                    && !write_slice_mem(
                        mem,
                        dirty,
                        &writable_data[offset as usize..end as usize],
                        GuestAddress(*addr),
                    )
                {
                    log::warn!("vqueue: write_slice OOB at 0x{addr:x} — dropping");
                }
                offset = end;
            }

            // Write the status byte.
            if status_addr != 0
                && !write_obj_mem(mem, dirty, status_byte, GuestAddress(status_addr))
            {
                log::warn!("vqueue: write status OOB at 0x{status_addr:x}");
            }

            // Update the used ring. Layout: `{ flags: u16, idx: u16, ring[] }`,
            // so the ring entries start at base + 4 (after flags + idx).
            let used_elem = UsedElem {
                id: head_desc_idx as u32,
                len: chain.len,
            };
            let used_entry_addr = match self.used_entry_addr() {
                Some(addr) => addr,
                None => break,
            };
            let _ = write_obj_mem(mem, dirty, used_elem, used_entry_addr);
            self.last_used_idx = self.last_used_idx.wrapping_add(1);

            self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
            processed += 1;
        }

        // Publish the used ring index. The UsedRing struct lays out as
        // `{ flags: u16, idx: u16, ring[] }` so the idx field is at +2 from
        // the base. Writing to +0 (flags) was a long-standing bug that no
        // existing test caught because the in-VMM tests only count handler
        // invocations, not the published idx. A real Linux driver reads
        // used.idx and would never see new entries.
        if let Some(used_idx_addr) = self.used_idx_addr() {
            let _ = write_obj_mem(mem, dirty, self.last_used_idx, used_idx_addr);
        }

        processed
    }

    /// Get the last used index (for interrupt injection).
    pub fn used_idx(&self) -> u16 {
        self.last_used_idx
    }

    pub fn avail_idx(&self) -> u16 {
        self.last_avail_idx
    }

    /// Process all available chains, exposing raw descriptor lists to the
    /// handler instead of collecting them into a status-terminated byte
    /// buffer. Suited to virtio-net where:
    /// - TX queue: all-readable chains, the handler should drain them into
    ///   the host tap and return 0 as `used_len`.
    /// - RX queue: all-writable chains, the handler should write incoming
    ///   packet bytes into the chain and return the byte count.
    ///
    /// The handler receives `(readable_descs, writable_descs)` as guest-
    /// physical `(addr, len)` pairs and returns the number of bytes written
    /// into the chain (RX) or 0 (TX). This becomes `UsedElem.len`.
    ///
    /// If `handler` returns `None`, the chain is skipped: the avail-ring
    /// cursor does NOT advance, the used ring is NOT updated, and the next
    /// call retries the same chain. Use this on RX when no packet is
    /// available yet — the descriptor must stay in-flight for the next
    /// inbound frame.
    pub fn process_queue_descriptors<F>(&mut self, mem: &GuestMemoryMmap, handler: F) -> usize
    where
        F: FnMut(&[(u64, u32)], &[(u64, u32)]) -> Option<u32>,
    {
        self.process_queue_descriptors_dirty(mem, None, handler)
    }

    pub fn process_queue_descriptors_dirty<F>(
        &mut self,
        mem: &GuestMemoryMmap,
        dirty: Option<&SoftwareDirtyBitmap>,
        mut handler: F,
    ) -> usize
    where
        F: FnMut(&[(u64, u32)], &[(u64, u32)]) -> Option<u32>,
    {
        if !self.queue_is_usable(mem, "process_queue_descriptors") {
            return 0;
        }

        let avail_addr = GuestAddress(self.config.avail_ring_addr);
        let avail: AvailRing = match read_obj_mem::<AvailRing>(mem, avail_addr) {
            Some(a) => a,
            None => return 0,
        };

        let mut processed = 0;
        while self.last_avail_idx != avail.idx {
            let avail_entry_addr = match self.avail_entry_addr() {
                Some(addr) => addr,
                None => break,
            };
            let head_desc_idx: u16 = match read_obj_mem::<u16>(mem, avail_entry_addr) {
                Some(v) => v,
                None => break,
            };

            let chain = match self.walk_descriptor_chain(mem, head_desc_idx) {
                Some(chain) => chain,
                None => {
                    self.reject_available_chain();
                    continue;
                }
            };

            let used_len = match handler(&chain.readable, &chain.writable) {
                Some(n) => n,
                None => break, // leave chain in-flight, try again next call
            };

            let used_elem = UsedElem {
                id: head_desc_idx as u32,
                len: used_len,
            };
            let used_entry_addr = match self.used_entry_addr() {
                Some(addr) => addr,
                None => break,
            };
            let _ = write_obj_mem(mem, dirty, used_elem, used_entry_addr);
            self.last_used_idx = self.last_used_idx.wrapping_add(1);
            self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
            processed += 1;
        }

        // Publish the used ring index at +2 (after the flags field).
        if let Some(used_idx_addr) = self.used_idx_addr() {
            let _ = write_obj_mem(mem, dirty, self.last_used_idx, used_idx_addr);
        }
        processed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_config_default() {
        let c = QueueConfig::default();
        assert!(!c.ready);
        assert_eq!(c.size, 0);
    }

    #[test]
    fn processor_starts_with_zero_indices() {
        let p = VirtQueueProcessor::new(QueueConfig {
            size: 64,
            desc_table_addr: 0x100000,
            avail_ring_addr: 0x200000,
            used_ring_addr: 0x300000,
            ready: true,
        });
        assert_eq!(p.last_avail_idx, 0);
        assert_eq!(p.last_used_idx, 0);
    }

    #[test]
    fn process_queue_returns_zero_when_not_ready() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1024 * 1024)]).unwrap();
        let mut p = VirtQueueProcessor::new(QueueConfig::default());
        let count = p.process_queue(&mem, |_, _| 0);
        assert_eq!(count, 0);
    }

    #[test]
    fn invalid_queue_size_is_rejected_before_use() {
        let (mem, desc_table, avail, used) = setup_queue_env();
        let desc = Descriptor {
            addr: 0x1000,
            len: 1,
            flags: 0,
            next: 0,
        };
        let _ = mem.write_obj(desc, GuestAddress(desc_table));

        let mut p = VirtQueueProcessor::new(QueueConfig {
            size: 3,
            desc_table_addr: desc_table,
            avail_ring_addr: avail,
            used_ring_addr: used,
            ready: true,
        });

        let count = p.process_queue(&mem, |_, _| {
            panic!("non-power-of-two queue size must be rejected");
        });
        assert_eq!(count, 0);
        assert_eq!(p.avail_idx(), 0);
        assert_eq!(p.used_idx(), 0);
    }

    #[test]
    fn used_idx_tracks_processed() {
        let p = VirtQueueProcessor::new(QueueConfig {
            size: 64,
            desc_table_addr: 0,
            avail_ring_addr: 0,
            used_ring_addr: 0,
            ready: true,
        });
        assert_eq!(p.used_idx(), 0);
    }

    // --- Security tests for descriptor bounds and length caps ---

    /// Helper: set up a 1 MiB guest memory region with a valid avail ring
    /// pointing at a descriptor table. Returns (mem, desc_table_addr, avail_addr, used_addr).
    fn setup_queue_env() -> (GuestMemoryMmap, u64, u64, u64) {
        let mem_size = 1024 * 1024;
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size)]).unwrap();
        let desc_table_addr: u64 = 0x10000;
        let avail_ring_addr: u64 = 0x20000;
        let used_ring_addr: u64 = 0x30000;

        // Write avail ring header: flags=0, idx=1 (one entry available).
        let avail = AvailRing { flags: 0, idx: 1 };
        let _ = mem.write_obj(avail, GuestAddress(avail_ring_addr));
        // Write avail ring entry [0] = descriptor index 0.
        let _ = mem.write_obj(0u16, GuestAddress(avail_ring_addr + 4));

        (mem, desc_table_addr, avail_ring_addr, used_ring_addr)
    }

    #[test]
    fn process_queue_dirty_marks_host_written_pages() {
        let (mem, desc_table, avail, used) = setup_queue_env();
        let data_addr = 0x0ff0;
        let status_addr = 0x4000;
        let desc0 = Descriptor {
            addr: data_addr,
            len: 32,
            flags: desc_flags::WRITE | desc_flags::NEXT,
            next: 1,
        };
        let desc1 = Descriptor {
            addr: status_addr,
            len: 1,
            flags: desc_flags::WRITE,
            next: 0,
        };
        let _ = mem.write_obj(desc0, GuestAddress(desc_table));
        let _ = mem.write_obj(desc1, GuestAddress(desc_table + 16));

        let dirty = SoftwareDirtyBitmap::new();
        let mut p = VirtQueueProcessor::new(QueueConfig {
            size: 8,
            desc_table_addr: desc_table,
            avail_ring_addr: avail,
            used_ring_addr: used,
            ready: true,
        });
        let count = p.process_queue_dirty(&mem, Some(&dirty), |_readable, writable| {
            writable.fill(0x5a);
            0
        });

        assert_eq!(count, 1);
        let dirty = dirty.drain();
        assert!(dirty.contains(data_addr));
        assert!(dirty.contains(data_addr + 31));
        assert!(dirty.contains(status_addr));
        assert!(dirty.contains(used + 4));
        assert!(dirty.contains(used + 2));
    }

    #[test]
    fn oob_desc_addr_is_rejected() {
        // A descriptor with addr pointing outside guest memory must not cause a
        // host OOB access. The bounds-checked helpers should return false/None
        // and the request is dropped.
        let (mem, desc_table, avail, used) = setup_queue_env();

        // Descriptor: addr = 0xDEAD_BEEF (way past the 1 MiB region), len=512, WRITE flag.
        let desc = Descriptor {
            addr: 0xDEAD_BEEF,
            len: 512,
            flags: desc_flags::WRITE,
            next: 0,
        };
        let _ = mem.write_obj(desc, GuestAddress(desc_table));

        let mut p = VirtQueueProcessor::new(QueueConfig {
            size: 8,
            desc_table_addr: desc_table,
            avail_ring_addr: avail,
            used_ring_addr: used,
            ready: true,
        });

        let count = p.process_queue(&mem, |_readable, _writable| {
            panic!("malformed descriptor must be rejected before handler");
        });
        assert_eq!(count, 0);
        assert_eq!(p.avail_idx(), 1);
        assert_eq!(p.used_idx(), 0);
    }

    #[test]
    fn oversized_desc_len_is_rejected() {
        // A descriptor with len > MAX_DESC_LEN must be rejected.
        let (mem, desc_table, avail, used) = setup_queue_env();

        let desc = Descriptor {
            addr: 0x1000,
            len: MAX_DESC_LEN + 1, // just over the cap
            flags: 0,              // readable
            next: 0,
        };
        let _ = mem.write_obj(desc, GuestAddress(desc_table));

        let mut p = VirtQueueProcessor::new(QueueConfig {
            size: 8,
            desc_table_addr: desc_table,
            avail_ring_addr: avail,
            used_ring_addr: used,
            ready: true,
        });

        let count = p.process_queue(&mem, |_, _| {
            panic!("oversized descriptor must be rejected before handler");
        });
        assert_eq!(count, 0);
        assert_eq!(p.avail_idx(), 1);
        assert_eq!(p.used_idx(), 0);
    }

    #[test]
    fn desc_idx_out_of_bounds_is_rejected() {
        // desc.next pointing past queue size must be rejected.
        let (mem, desc_table, avail, used) = setup_queue_env();

        // First descriptor: NEXT flag, next = 0xFFFF (way past size=8).
        let desc0 = Descriptor {
            addr: 0x1000,
            len: 64,
            flags: desc_flags::NEXT,
            next: 0xFFFF,
        };
        let _ = mem.write_obj(desc0, GuestAddress(desc_table));

        let mut p = VirtQueueProcessor::new(QueueConfig {
            size: 8,
            desc_table_addr: desc_table,
            avail_ring_addr: avail,
            used_ring_addr: used,
            ready: true,
        });

        // Should not crash, should not read OOB, and should not call handler.
        let count = p.process_queue(&mem, |_, _| {
            panic!("out-of-range next descriptor must be rejected");
        });
        assert_eq!(count, 0);
        assert_eq!(p.avail_idx(), 1);
        assert_eq!(p.used_idx(), 0);
    }

    #[test]
    fn descriptor_loop_is_rejected() {
        let (mem, desc_table, avail, used) = setup_queue_env();

        let desc0 = Descriptor {
            addr: 0x1000,
            len: 1,
            flags: desc_flags::NEXT,
            next: 1,
        };
        let desc1 = Descriptor {
            addr: 0x1001,
            len: 1,
            flags: desc_flags::NEXT,
            next: 0,
        };
        let _ = mem.write_obj(desc0, GuestAddress(desc_table));
        let _ = mem.write_obj(desc1, GuestAddress(desc_table + 16));

        let mut p = VirtQueueProcessor::new(QueueConfig {
            size: 8,
            desc_table_addr: desc_table,
            avail_ring_addr: avail,
            used_ring_addr: used,
            ready: true,
        });

        let count = p.process_queue(&mem, |_, _| {
            panic!("looping descriptor chain must be rejected");
        });
        assert_eq!(count, 0);
        assert_eq!(p.avail_idx(), 1);
        assert_eq!(p.used_idx(), 0);
    }

    #[test]
    fn descriptor_address_overflow_is_rejected() {
        let (mem, desc_table, avail, used) = setup_queue_env();

        let desc = Descriptor {
            addr: u64::MAX,
            len: 2,
            flags: 0,
            next: 0,
        };
        let _ = mem.write_obj(desc, GuestAddress(desc_table));

        let mut p = VirtQueueProcessor::new(QueueConfig {
            size: 8,
            desc_table_addr: desc_table,
            avail_ring_addr: avail,
            used_ring_addr: used,
            ready: true,
        });

        let count = p.process_queue(&mem, |_, _| {
            panic!("overflowing descriptor address must be rejected");
        });
        assert_eq!(count, 0);
        assert_eq!(p.avail_idx(), 1);
        assert_eq!(p.used_idx(), 0);
    }

    #[test]
    fn descriptor_api_rejects_loop_before_handler() {
        let (mem, desc_table, avail, used) = setup_queue_env();
        let desc = Descriptor {
            addr: 0x1000,
            len: 1,
            flags: desc_flags::NEXT | desc_flags::WRITE,
            next: 0,
        };
        let _ = mem.write_obj(desc, GuestAddress(desc_table));

        let mut p = VirtQueueProcessor::new(QueueConfig {
            size: 8,
            desc_table_addr: desc_table,
            avail_ring_addr: avail,
            used_ring_addr: used,
            ready: true,
        });

        let count = p.process_queue_descriptors(&mem, |_, _| {
            panic!("looping descriptor chain must be rejected before handler");
        });
        assert_eq!(count, 0);
        assert_eq!(p.avail_idx(), 1);
        assert_eq!(p.used_idx(), 0);
    }

    #[test]
    fn valid_descriptor_chain_succeeds() {
        // Positive test: a well-formed descriptor chain must work correctly.
        let (mem, desc_table, avail, used) = setup_queue_env();

        // Write "HELLO" at 0x1000 for the readable descriptor.
        let data = b"HELLO";
        let _ = mem.write_slice(data, GuestAddress(0x1000));

        // Descriptor: addr=0x1000, len=5, readable (no flags).
        let desc = Descriptor {
            addr: 0x1000,
            len: 5,
            flags: 0,
            next: 0,
        };
        let _ = mem.write_obj(desc, GuestAddress(desc_table));

        let mut p = VirtQueueProcessor::new(QueueConfig {
            size: 8,
            desc_table_addr: desc_table,
            avail_ring_addr: avail,
            used_ring_addr: used,
            ready: true,
        });

        let count = p.process_queue(&mem, |readable, _| {
            assert_eq!(readable, b"HELLO");
            0
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn chain_total_bytes_cap_enforced() {
        // A chain of descriptors whose total bytes exceed MAX_CHAIN_BYTES must
        // be rejected.
        let (mem, desc_table, avail, used) = setup_queue_env();

        // Chain of 5 descriptors, each MAX_DESC_LEN (1 MiB) = 5 MiB total > 4 MiB cap.
        for i in 0..5u16 {
            let desc = Descriptor {
                addr: 0x1000,
                len: MAX_DESC_LEN,
                flags: if i < 4 { desc_flags::NEXT } else { 0 },
                next: if i < 4 { i + 1 } else { 0 },
            };
            let _ = mem.write_obj(desc, GuestAddress(desc_table + i as u64 * 16));
        }

        let mut p = VirtQueueProcessor::new(QueueConfig {
            size: 8,
            desc_table_addr: desc_table,
            avail_ring_addr: avail,
            used_ring_addr: used,
            ready: true,
        });

        let count = p.process_queue(&mem, |_, _| {
            panic!("over-large descriptor chain must be rejected before handler");
        });
        assert_eq!(count, 0);
        assert_eq!(p.avail_idx(), 1);
        assert_eq!(p.used_idx(), 0);
    }

    #[test]
    fn oob_write_to_guest_is_rejected() {
        // A writable descriptor with OOB addr must not cause a host OOB write.
        let (mem, desc_table, avail, used) = setup_queue_env();

        // Writable descriptor with OOB addr.
        let desc = Descriptor {
            addr: 0xF000_0000, // way past 1 MiB
            len: 512,
            flags: desc_flags::WRITE,
            next: 0,
        };
        let _ = mem.write_obj(desc, GuestAddress(desc_table));

        let mut p = VirtQueueProcessor::new(QueueConfig {
            size: 8,
            desc_table_addr: desc_table,
            avail_ring_addr: avail,
            used_ring_addr: used,
            ready: true,
        });

        // Should not crash or invoke the handler.
        let count = p.process_queue(&mem, |_, _| {
            panic!("OOB writable descriptor must be rejected before handler");
        });
        assert_eq!(count, 0);
        assert_eq!(p.avail_idx(), 1);
        assert_eq!(p.used_idx(), 0);
    }

    #[test]
    fn property_test_random_chains_never_panic() {
        // Fuzz the virtqueue walker with random descriptor chains.
        // The walker must never panic or access OOB, regardless of input.
        let (mem, desc_table, avail, used) = setup_queue_env();

        // Simple LCG for reproducible randomness.
        let mut state: u32 = 12345;
        let lcg = |s: &mut u32| -> u32 {
            *s = s.wrapping_mul(1103515245).wrapping_add(12345);
            *s
        };

        for iter in 0..1000 {
            // Reset avail ring to point at one new descriptor chain.
            let avail_hdr = AvailRing { flags: 0, idx: 1 };
            let _ = mem.write_obj(avail_hdr, GuestAddress(avail));
            let _ = mem.write_obj(0u16, GuestAddress(avail + 4)); // entry[0] = desc 0

            // Write a random chain of descriptors starting at index 0.
            let chain_len = (lcg(&mut state) % 8) as u16;
            for i in 0..chain_len {
                let addr = (lcg(&mut state) % 0x200000) as u64; // some in-range, some OOB
                let len = lcg(&mut state) % (MAX_DESC_LEN + 100); // some over the cap
                let has_next = i < chain_len - 1;
                let next = if has_next { i + 1 } else { 0 };
                let flags = if has_next { desc_flags::NEXT } else { 0 };
                let desc = Descriptor {
                    addr,
                    len,
                    flags,
                    next,
                };
                let _ = mem.write_obj(desc, GuestAddress(desc_table + i as u64 * 16));
            }

            let mut p = VirtQueueProcessor::new(QueueConfig {
                size: 8,
                desc_table_addr: desc_table,
                avail_ring_addr: avail,
                used_ring_addr: used,
                ready: true,
            });

            // Must not panic.
            let _ = p.process_queue(&mem, |_, _| 0);

            // Reset the queue for the next iteration.
            if iter % 100 == 0 {
                log::debug!("property test iter {iter}");
            }
        }
    }
}
