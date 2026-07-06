//! virtio-mmio transport for virtio-net: register decode + per-queue
//! processing + tap-fd I/O. Two queues — RX (index 0), TX (index 1).
//!
//! Pattern matches `blk_transport.rs`. The differences are:
//!   * device_id = 1 (virtio-net), not 2.
//!   * Two queues instead of one.
//!   * Device-specific CONFIG space at 0x100..0x106 exposes the MAC.
//!   * On QUEUE_NOTIFY for TX (queue 1) we read packets out of the chain
//!     and `write(tap_fd, ...)`. RX delivery is driven externally by
//!     `inject_rx_packet()` — the I/O loop calls it when a packet arrives
//!     on the tap fd.

use crate::bus::{MmioDevice, MmioReadResult, MmioWriteResult};
use crate::persist::Persist;
use crate::rate_limit::RateLimiter;
use crate::virtio::blk_transport::status_bits;
use crate::virtio::regs::{reg, MAGIC};
use crate::virtio::vqueue::{
    is_valid_queue_size, QueueConfig, VirtQueueProcessor, VirtQueueProcessorState, MAX_QUEUE_SIZE,
};
use serde::{Deserialize, Serialize};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmm_memory_backend::dirty::SoftwareDirtyBitmap;

/// VRING bit in INTERRUPT_STATUS.
const VIRTIO_MMIO_INT_VRING: u32 = 0x01;

/// virtio-net header prepended to every packet (`struct virtio_net_hdr_v1`,
/// virtio 1.x §5.1.6). It is 12 bytes whenever VIRTIO_F_VERSION_1 is
/// negotiated (num_buffers is always present in v1), independent of
/// MRG_RXBUF. This device always negotiates VERSION_1 (virtio-mmio v2
/// requires it), so Linux sets its hdr_len to sizeof(virtio_net_hdr_mrg_rxbuf)
/// = 12. Using the legacy 10-byte size offsets every frame by 2 bytes and
/// corrupts all traffic.
pub const VIRTIO_NET_HDR_LEN: usize = 12;

/// Byte offset of `num_buffers` within `virtio_net_hdr_v1` (last u16).
const VIRTIO_NET_HDR_NUM_BUFFERS_OFF: usize = 10;

/// Max Ethernet frame we accept end-to-end (header + 1500 MTU + slack).
const MAX_FRAME_BYTES: usize = 1600;

/// virtio-net feature bits the device advertises.
pub mod net_features {
    pub const CSUM: u32 = 1 << 0;
    pub const GUEST_CSUM: u32 = 1 << 2;
    pub const MAC: u32 = 1 << 5;
}

const QUEUE_RX: usize = 0;
const QUEUE_TX: usize = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct QueueState {
    size: u16,
    desc_table_addr: u64,
    avail_ring_addr: u64,
    used_ring_addr: u64,
    ready: bool,
}

impl QueueState {
    fn valid_size(&self) -> bool {
        is_valid_queue_size(self.size, MAX_QUEUE_SIZE)
    }

    fn set_size(&mut self, size: u32) {
        let Ok(size) = u16::try_from(size) else {
            log::warn!("virtio-net: QUEUE_NUM {size} exceeds u16 — rejecting");
            self.size = 0;
            self.ready = false;
            return;
        };
        if size == 0 {
            self.size = 0;
            self.ready = false;
            return;
        }
        if is_valid_queue_size(size, MAX_QUEUE_SIZE) {
            self.size = size;
        } else {
            log::warn!(
                "virtio-net: invalid QUEUE_NUM {size} (must be power-of-two <= {MAX_QUEUE_SIZE})"
            );
            self.size = 0;
            self.ready = false;
        }
    }

    fn set_ready(&mut self, ready: bool) {
        if ready && !self.valid_size() {
            log::warn!(
                "virtio-net: QUEUE_READY ignored for invalid QUEUE_NUM {}",
                self.size
            );
            self.ready = false;
        } else {
            self.ready = ready;
        }
    }
}

/// Snapshot state for virtio-net MMIO negotiation and queue configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VirtioNetMmioState {
    pub status: u32,
    pub queue_sel: u32,
    pub host_features_sel: u32,
    pub guest_features_sel: u32,
    pub guest_features: u32,
    queues: Vec<QueueState>,
    pub activated: bool,
    pub interrupt_status: u32,
    #[serde(default)]
    rx_processor: Option<VirtQueueProcessorState>,
    #[serde(default)]
    tx_processor: Option<VirtQueueProcessorState>,
}

/// The virtio-mmio transport for a tap-backed NIC.
pub struct VirtioNetMmio {
    pub irq: u32,
    pub device_id: u32,
    pub vendor_id: u32,
    pub version: u32,
    /// Locally-administered unicast MAC exposed to the guest via CONFIG space.
    pub mac: [u8; 6],
    /// Host-features bitmap (low 32 bits). Driver reads with HOST_FEATURES_SEL=0.
    pub features: u32,
    status: AtomicU32,
    queue_sel: AtomicU32,
    host_features_sel: AtomicU32,
    guest_features_sel: AtomicU32,
    guest_features: AtomicU32,
    /// Per-queue config: index 0 = RX, index 1 = TX.
    queues: Mutex<Vec<QueueState>>,
    guest_mem: Mutex<Option<Arc<GuestMemoryMmap>>>,
    host_dirty: Mutex<Option<SoftwareDirtyBitmap>>,
    activated: AtomicBool,
    /// Persistent virtqueue walkers — one per queue.
    rx_processor: Mutex<Option<VirtQueueProcessor>>,
    tx_processor: Mutex<Option<VirtQueueProcessor>>,
    /// Optional per-device token bucket for packets/sec and bytes/sec caps.
    rate_limiter: Mutex<Option<RateLimiter>>,
    interrupt_status: AtomicU32,
    /// Raw tap fd. Set by the VMM after creating the tap. The transport
    /// borrows it for `write(2)` on TX; ownership stays with the caller.
    tap_fd: Mutex<Option<RawFd>>,
    /// IRQ EventFd — written when we need to interrupt the guest.
    #[cfg(target_os = "linux")]
    irq_evt: Mutex<Option<vmm_sys_util::eventfd::EventFd>>,
    /// Diagnostic counters — read post-boot to verify driver bring-up.
    pub status_writes: AtomicU64,
    pub notify_count: AtomicU64,
    pub tx_packets: AtomicU64,
    pub rx_packets: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub rx_bytes: AtomicU64,
}

impl VirtioNetMmio {
    pub fn new(irq: u32, mac: [u8; 6]) -> Self {
        Self {
            irq,
            device_id: 1,
            vendor_id: 0,
            version: 2,
            mac,
            features: net_features::MAC,
            status: AtomicU32::new(0),
            queue_sel: AtomicU32::new(0),
            host_features_sel: AtomicU32::new(0),
            guest_features_sel: AtomicU32::new(0),
            guest_features: AtomicU32::new(0),
            queues: Mutex::new(vec![QueueState::default(), QueueState::default()]),
            guest_mem: Mutex::new(None),
            host_dirty: Mutex::new(None),
            activated: AtomicBool::new(false),
            rx_processor: Mutex::new(None),
            tx_processor: Mutex::new(None),
            rate_limiter: Mutex::new(None),
            interrupt_status: AtomicU32::new(0),
            tap_fd: Mutex::new(None),
            #[cfg(target_os = "linux")]
            irq_evt: Mutex::new(None),
            status_writes: AtomicU64::new(0),
            notify_count: AtomicU64::new(0),
            tx_packets: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
        }
    }

    pub fn current_status(&self) -> u32 {
        self.status.load(Ordering::Relaxed)
    }
    pub fn is_activated(&self) -> bool {
        self.activated.load(Ordering::Relaxed)
    }

    pub fn set_guest_memory(&self, mem: Arc<GuestMemoryMmap>) {
        *self.guest_mem.lock().unwrap() = Some(mem);
    }

    pub fn set_guest_dirty_tracker(&self, dirty: SoftwareDirtyBitmap) {
        *self.host_dirty.lock().unwrap() = Some(dirty);
    }

    /// Install a per-device rate limiter. Leaving it unset keeps I/O unlimited.
    pub fn set_rate_limiter(&self, rl: RateLimiter) {
        *self.rate_limiter.lock().unwrap() = Some(rl);
    }

    fn rate_limit_allows(&self, bytes: u64) -> bool {
        match self.rate_limiter.lock().unwrap().as_mut() {
            Some(rl) => rl.try_charge(1, bytes),
            None => true,
        }
    }

    /// Hand the transport a tap fd. The fd's lifetime is the caller's; the
    /// transport only uses it for `write(2)` on TX.
    pub fn set_tap_fd(&self, fd: RawFd) {
        *self.tap_fd.lock().unwrap() = Some(fd);
    }

    #[cfg(target_os = "linux")]
    pub fn set_irq_evt(&self, evt: vmm_sys_util::eventfd::EventFd) {
        *self.irq_evt.lock().unwrap() = Some(evt);
    }

    fn trigger_interrupt(&self) {
        self.interrupt_status
            .fetch_or(VIRTIO_MMIO_INT_VRING, Ordering::SeqCst);
        #[cfg(target_os = "linux")]
        if let Some(evt) = self.irq_evt.lock().unwrap().as_ref() {
            let _ = evt.write(1);
        }
    }

    fn queue_config(&self, idx: usize) -> Option<QueueConfig> {
        let qs = self.queues.lock().unwrap();
        let q = qs.get(idx)?;
        if !q.ready || !q.valid_size() {
            return None;
        }
        Some(QueueConfig {
            size: q.size,
            desc_table_addr: q.desc_table_addr,
            avail_ring_addr: q.avail_ring_addr,
            used_ring_addr: q.used_ring_addr,
            ready: q.ready,
        })
    }

    fn queue_config_from_state(q: &QueueState) -> QueueConfig {
        QueueConfig {
            size: q.size,
            desc_table_addr: q.desc_table_addr,
            avail_ring_addr: q.avail_ring_addr,
            used_ring_addr: q.used_ring_addr,
            ready: q.ready,
        }
    }

    /// Drain the TX queue. For each chain (all readable: virtio_net_hdr +
    /// packet bytes) assemble the contiguous packet and `write(tap_fd, ...)`.
    /// Called either from the MMIO QUEUE_NOTIFY path (test/diagnostic) or
    /// from the I/O loop when a TX kick EventFd fires.
    pub fn process_tx_queue(&self) -> usize {
        let mem = match self.guest_mem.lock().unwrap().clone() {
            Some(m) => m,
            None => return 0,
        };
        let dirty = self.host_dirty.lock().unwrap().clone();
        let cfg = match self.queue_config(QUEUE_TX) {
            Some(c) => c,
            None => return 0,
        };
        let tap_fd = *self.tap_fd.lock().unwrap();

        let mut proc_guard = self.tx_processor.lock().unwrap();
        if proc_guard.is_none() {
            *proc_guard = Some(VirtQueueProcessor::new(cfg));
        } else {
            proc_guard.as_mut().unwrap().update_config(cfg);
        }

        let mut total_pkts = 0usize;
        let processed = proc_guard
            .as_mut()
            .unwrap()
            .process_queue_descriptors_dirty(&mem, dirty.as_ref(), |readable, _writable| {
                // Concatenate all readable descs into one buffer; bounds-checked
                // via vm-memory `Bytes`. The leading virtio_net_hdr bytes
                // — drop them before sending on the tap.
                let mut buf = Vec::with_capacity(MAX_FRAME_BYTES);
                for &(addr, len) in readable {
                    if len == 0 {
                        continue;
                    }
                    let Some(new_len) = buf.len().checked_add(len as usize) else {
                        return Some(0);
                    };
                    if new_len > MAX_FRAME_BYTES {
                        return Some(0);
                    }
                    let off = buf.len();
                    buf.resize(new_len, 0);
                    if mem
                        .read_slice(&mut buf[off..new_len], GuestAddress(addr))
                        .is_err()
                    {
                        return Some(0);
                    }
                }
                if buf.len() <= VIRTIO_NET_HDR_LEN {
                    return Some(0);
                }
                let packet = &buf[VIRTIO_NET_HDR_LEN..];

                if !self.rate_limit_allows(packet.len() as u64) {
                    return None;
                }

                if let Some(fd) = tap_fd {
                    // SAFETY: write to a valid open fd. Short writes count as a packet sent.
                    let rc = unsafe {
                        libc::write(fd, packet.as_ptr() as *const libc::c_void, packet.len())
                    };
                    if rc > 0 {
                        total_pkts += 1;
                        self.tx_packets.fetch_add(1, Ordering::Relaxed);
                        self.tx_bytes.fetch_add(rc as u64, Ordering::Relaxed);
                    }
                }
                Some(0) // device wrote nothing back into the chain
            });

        if processed > 0 {
            self.trigger_interrupt();
        }
        total_pkts
    }

    /// Inject a packet received from the host tap into the RX queue.
    /// Walks one writable chain, lays down a zero virtio_net_hdr, then the
    /// packet bytes. Returns true iff a chain was consumed. If no avail
    /// descriptor is ready the packet is dropped (caller should retry later).
    pub fn inject_rx_packet(&self, packet: &[u8]) -> bool {
        let Some(need) = packet.len().checked_add(VIRTIO_NET_HDR_LEN) else {
            return false;
        };
        if need > MAX_FRAME_BYTES {
            return false;
        }
        let mem = match self.guest_mem.lock().unwrap().clone() {
            Some(m) => m,
            None => return false,
        };
        let dirty = self.host_dirty.lock().unwrap().clone();
        let cfg = match self.queue_config(QUEUE_RX) {
            Some(c) => c,
            None => return false,
        };

        let mut proc_guard = self.rx_processor.lock().unwrap();
        if proc_guard.is_none() {
            *proc_guard = Some(VirtQueueProcessor::new(cfg));
        } else {
            proc_guard.as_mut().unwrap().update_config(cfg);
        }

        let mut delivered = false;
        let _ = proc_guard
            .as_mut()
            .unwrap()
            .process_queue_descriptors_dirty(&mem, dirty.as_ref(), |_readable, writable| {
                if delivered {
                    // Only deliver into the first available chain per call.
                    return None;
                }
                // Compute total writable capacity; need at least hdr + packet.
                let Some(cap) = writable
                    .iter()
                    .try_fold(0usize, |acc, (_, len)| acc.checked_add(*len as usize))
                else {
                    return Some(0);
                };
                if cap < need {
                    return Some(0);
                }
                if !self.rate_limit_allows(packet.len() as u64) {
                    return None;
                }
                // Lay down the 12-byte virtio_net_hdr_v1 (zeroed, with
                // num_buffers = 1 for this single-descriptor packet), then
                // the packet bytes, spanning whatever descriptors are needed.
                let mut header = [0u8; VIRTIO_NET_HDR_LEN];
                header[VIRTIO_NET_HDR_NUM_BUFFERS_OFF..].copy_from_slice(&1u16.to_le_bytes());
                let mut to_write: Vec<u8> = Vec::with_capacity(need);
                to_write.extend_from_slice(&header);
                to_write.extend_from_slice(packet);
                let mut cursor = 0usize;
                for &(addr, len) in writable {
                    if cursor >= to_write.len() {
                        break;
                    }
                    let remaining = to_write.len() - cursor;
                    let take = remaining.min(len as usize);
                    let Some(next_cursor) = cursor.checked_add(take) else {
                        return Some(0);
                    };
                    if mem
                        .write_slice(&to_write[cursor..next_cursor], GuestAddress(addr))
                        .is_err()
                    {
                        return Some(0);
                    }
                    if let Some(dirty) = dirty.as_ref() {
                        dirty.mark_range(addr, take as u64);
                    }
                    cursor = next_cursor;
                }
                delivered = true;
                self.rx_packets.fetch_add(1, Ordering::Relaxed);
                self.rx_bytes
                    .fetch_add(packet.len() as u64, Ordering::Relaxed);
                Some(need as u32)
            });

        if delivered {
            self.trigger_interrupt();
        }
        delivered
    }

    /// Read up to 4 bytes from device-specific CONFIG space (MAC at 0x100..0x106).
    fn read_config(&self, off: u64, len: u8) -> u64 {
        // off is relative to the device start; subtract CONFIG base.
        let cfg_off = match off.checked_sub(reg::CONFIG) {
            Some(v) => v as usize,
            None => return 0,
        };
        if cfg_off >= self.mac.len() {
            return 0;
        }
        let mut buf = [0u8; 4];
        let copy_len = (self.mac.len() - cfg_off).min(len as usize).min(4);
        buf[..copy_len].copy_from_slice(&self.mac[cfg_off..cfg_off + copy_len]);
        u32::from_le_bytes(buf) as u64
    }
}

impl Persist for VirtioNetMmio {
    type State = VirtioNetMmioState;

    fn save(&self) -> Self::State {
        VirtioNetMmioState {
            status: self.status.load(Ordering::Relaxed),
            queue_sel: self.queue_sel.load(Ordering::Relaxed),
            host_features_sel: self.host_features_sel.load(Ordering::Relaxed),
            guest_features_sel: self.guest_features_sel.load(Ordering::Relaxed),
            guest_features: self.guest_features.load(Ordering::Relaxed),
            queues: self.queues.lock().unwrap().clone(),
            activated: self.activated.load(Ordering::Relaxed),
            interrupt_status: self.interrupt_status.load(Ordering::SeqCst),
            rx_processor: self
                .rx_processor
                .lock()
                .unwrap()
                .as_ref()
                .map(VirtQueueProcessor::save_state),
            tx_processor: self
                .tx_processor
                .lock()
                .unwrap()
                .as_ref()
                .map(VirtQueueProcessor::save_state),
        }
    }

    fn restore(&mut self, state: Self::State) {
        self.status.store(state.status, Ordering::Relaxed);
        self.queue_sel.store(state.queue_sel, Ordering::Relaxed);
        self.host_features_sel
            .store(state.host_features_sel, Ordering::Relaxed);
        self.guest_features_sel
            .store(state.guest_features_sel, Ordering::Relaxed);
        self.guest_features
            .store(state.guest_features, Ordering::Relaxed);
        *self.queues.lock().unwrap() = state.queues;
        self.activated.store(state.activated, Ordering::Relaxed);
        self.interrupt_status
            .store(state.interrupt_status, Ordering::SeqCst);
        let queues = self.queues.lock().unwrap();
        let rx_config = queues
            .get(QUEUE_RX)
            .map(Self::queue_config_from_state)
            .unwrap_or_default();
        let tx_config = queues
            .get(QUEUE_TX)
            .map(Self::queue_config_from_state)
            .unwrap_or_default();
        drop(queues);
        *self.rx_processor.lock().unwrap() = state
            .rx_processor
            .map(|p| VirtQueueProcessor::from_state(rx_config, p));
        *self.tx_processor.lock().unwrap() = state
            .tx_processor
            .map(|p| VirtQueueProcessor::from_state(tx_config, p));
    }
}

impl Persist for Arc<VirtioNetMmio> {
    type State = VirtioNetMmioState;

    fn save(&self) -> Self::State {
        self.as_ref().save()
    }

    fn restore(&mut self, state: Self::State) {
        self.status.store(state.status, Ordering::Relaxed);
        self.queue_sel.store(state.queue_sel, Ordering::Relaxed);
        self.host_features_sel
            .store(state.host_features_sel, Ordering::Relaxed);
        self.guest_features_sel
            .store(state.guest_features_sel, Ordering::Relaxed);
        self.guest_features
            .store(state.guest_features, Ordering::Relaxed);
        *self.queues.lock().unwrap() = state.queues;
        self.activated.store(state.activated, Ordering::Relaxed);
        self.interrupt_status
            .store(state.interrupt_status, Ordering::SeqCst);
        let queues = self.queues.lock().unwrap();
        let rx_config = queues
            .get(QUEUE_RX)
            .map(VirtioNetMmio::queue_config_from_state)
            .unwrap_or_default();
        let tx_config = queues
            .get(QUEUE_TX)
            .map(VirtioNetMmio::queue_config_from_state)
            .unwrap_or_default();
        drop(queues);
        *self.rx_processor.lock().unwrap() = state
            .rx_processor
            .map(|p| VirtQueueProcessor::from_state(rx_config, p));
        *self.tx_processor.lock().unwrap() = state
            .tx_processor
            .map(|p| VirtQueueProcessor::from_state(tx_config, p));
    }
}

impl MmioDevice for VirtioNetMmio {
    fn mmio_read(&self, off: u64, len: u8) -> MmioReadResult {
        // CONFIG (MAC) space: 0x100..0x108
        if (reg::CONFIG..reg::CONFIG + 16).contains(&off) {
            return Ok(self.read_config(off, len));
        }
        let val = match off {
            reg::MAGIC_VALUE => MAGIC,
            reg::VERSION => self.version,
            reg::DEVICE_ID => self.device_id,
            reg::VENDOR_ID => self.vendor_id,
            reg::HOST_FEATURES => {
                if self.host_features_sel.load(Ordering::Relaxed) == 0 {
                    self.features
                } else {
                    // virtio 1.x bit (32): VERSION_1. Bit 32 is in the high word.
                    1 << 0
                }
            }
            reg::QUEUE_NUM_MAX => MAX_QUEUE_SIZE as u32,
            reg::STATUS => self.status.load(Ordering::Relaxed),
            reg::INTERRUPT_STATUS => self.interrupt_status.load(Ordering::SeqCst),
            reg::CONFIG_GENERATION => 0,
            reg::QUEUE_NUM => {
                let qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                qs.get(sel).map(|q| q.size as u32).unwrap_or(0)
            }
            reg::QUEUE_READY => {
                let qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                qs.get(sel)
                    .map(|q| u32::from(q.ready && q.valid_size()))
                    .unwrap_or(0)
            }
            reg::QUEUE_DESC_LOW => self.q_addr(|q| q.desc_table_addr) as u32,
            reg::QUEUE_DESC_HIGH => (self.q_addr(|q| q.desc_table_addr) >> 32) as u32,
            reg::QUEUE_DRIVER_LOW => self.q_addr(|q| q.avail_ring_addr) as u32,
            reg::QUEUE_DRIVER_HIGH => (self.q_addr(|q| q.avail_ring_addr) >> 32) as u32,
            reg::QUEUE_DEVICE_LOW => self.q_addr(|q| q.used_ring_addr) as u32,
            reg::QUEUE_DEVICE_HIGH => (self.q_addr(|q| q.used_ring_addr) >> 32) as u32,
            _ => 0,
        };
        Ok(val as u64)
    }

    fn mmio_write(&self, off: u64, val: u64, _len: u8) -> MmioWriteResult {
        let val = val as u32;
        match off {
            reg::STATUS => {
                self.status.store(val, Ordering::Relaxed);
                self.status_writes.fetch_add(1, Ordering::Relaxed);
                if val & status_bits::DRIVER_OK != 0 {
                    self.activated.store(true, Ordering::Relaxed);
                }
            }
            reg::HOST_FEATURES_SEL => {
                self.host_features_sel.store(val, Ordering::Relaxed);
            }
            reg::GUEST_FEATURES_SEL => {
                self.guest_features_sel.store(val, Ordering::Relaxed);
            }
            reg::GUEST_FEATURES if self.guest_features_sel.load(Ordering::Relaxed) == 0 => {
                self.guest_features.store(val, Ordering::Relaxed);
            }
            reg::QUEUE_SEL => {
                self.queue_sel.store(val, Ordering::Relaxed);
            }
            reg::QUEUE_NUM => self.q_write(|q| q.set_size(val)),
            reg::QUEUE_DESC_LOW => {
                self.q_write(|q| q.desc_table_addr = (q.desc_table_addr & !0xFFFFFFFF) | val as u64)
            }
            reg::QUEUE_DESC_HIGH => self.q_write(|q| {
                q.desc_table_addr = (q.desc_table_addr & 0xFFFFFFFF) | (val as u64) << 32
            }),
            reg::QUEUE_DRIVER_LOW => {
                self.q_write(|q| q.avail_ring_addr = (q.avail_ring_addr & !0xFFFFFFFF) | val as u64)
            }
            reg::QUEUE_DRIVER_HIGH => self.q_write(|q| {
                q.avail_ring_addr = (q.avail_ring_addr & 0xFFFFFFFF) | (val as u64) << 32
            }),
            reg::QUEUE_DEVICE_LOW => {
                self.q_write(|q| q.used_ring_addr = (q.used_ring_addr & !0xFFFFFFFF) | val as u64)
            }
            reg::QUEUE_DEVICE_HIGH => self.q_write(|q| {
                q.used_ring_addr = (q.used_ring_addr & 0xFFFFFFFF) | (val as u64) << 32
            }),
            reg::QUEUE_READY => self.q_write(|q| q.set_ready(val != 0)),
            reg::QUEUE_NOTIFY => {
                self.notify_count.fetch_add(1, Ordering::Relaxed);
                // val is the queue index that was kicked.
                if val as usize == QUEUE_TX {
                    self.process_tx_queue();
                }
                // RX kicks: nothing for the device to do — the driver just
                // refreshed avail; we'll see the new descriptors next time
                // we inject a packet.
            }
            reg::INTERRUPT_ACK => {
                self.interrupt_status.fetch_and(!val, Ordering::SeqCst);
            }
            _ => {}
        }
        Ok(())
    }
}

impl VirtioNetMmio {
    fn q_addr<F: Fn(&QueueState) -> u64>(&self, f: F) -> u64 {
        let qs = self.queues.lock().unwrap();
        let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
        qs.get(sel).map(f).unwrap_or(0)
    }
    fn q_write<F: FnMut(&mut QueueState)>(&self, mut f: F) {
        let mut qs = self.queues.lock().unwrap();
        let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
        if let Some(q) = qs.get_mut(sel) {
            f(q);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{MmioBus, MmioRange};
    use crate::rate_limit::{RateLimitClock, RateLimiter};
    use crate::virtio::vqueue::{AvailRing, Descriptor, UsedElem};
    use std::sync::atomic::AtomicU64;
    use vm_memory::GuestMemoryMmap;

    fn new_mem() -> Arc<GuestMemoryMmap> {
        Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4 * 1024 * 1024)]).unwrap())
    }

    #[derive(Debug)]
    struct ManualClock {
        now_ns: AtomicU64,
    }

    impl ManualClock {
        fn new(now_ns: u64) -> Self {
            Self {
                now_ns: AtomicU64::new(now_ns),
            }
        }

        fn advance(&self, delta_ns: u64) {
            self.now_ns.fetch_add(delta_ns, Ordering::Relaxed);
        }
    }

    impl RateLimitClock for ManualClock {
        fn now_ns(&self) -> u64 {
            self.now_ns.load(Ordering::Relaxed)
        }
    }

    fn configure_net_queue(dev: &VirtioNetMmio, queue_idx: u32, desc: u64, avail: u64, used: u64) {
        dev.mmio_write(reg::QUEUE_SEL, queue_idx as u64, 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, 16, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DESC_LOW, desc as u32 as u64, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DESC_HIGH, desc >> 32, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_LOW, avail as u32 as u64, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_HIGH, avail >> 32, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_LOW, used as u32 as u64, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_HIGH, used >> 32, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();
    }

    fn net_used_idx(mem: &GuestMemoryMmap, used: u64) -> u16 {
        mem.read_obj(GuestAddress(used + 2)).unwrap()
    }

    #[test]
    fn magic_and_device_id() {
        let dev = VirtioNetMmio::new(6, [0xAA; 6]);
        assert_eq!(dev.mmio_read(reg::MAGIC_VALUE, 4).unwrap(), MAGIC as u64);
        assert_eq!(dev.mmio_read(reg::DEVICE_ID, 4).unwrap(), 1);
    }

    #[test]
    fn mac_exposed_in_config_space() {
        let dev = VirtioNetMmio::new(6, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        // 4-byte read of low 4 MAC bytes.
        let low = dev.mmio_read(reg::CONFIG, 4).unwrap();
        assert_eq!(low, 0x1200_5452);
        // Byte read of the 5th MAC byte (offset 0x104 → cfg_off=4 → 0x34).
        let b4 = dev.mmio_read(reg::CONFIG + 4, 1).unwrap();
        assert_eq!(b4 & 0xff, 0x34);
        // Byte read of the 6th MAC byte (offset 0x105 → cfg_off=5 → 0x56).
        let b5 = dev.mmio_read(reg::CONFIG + 5, 1).unwrap();
        assert_eq!(b5 & 0xff, 0x56);
    }

    #[test]
    fn features_advertised_include_mac() {
        let dev = VirtioNetMmio::new(6, [0; 6]);
        dev.mmio_write(reg::HOST_FEATURES_SEL, 0, 4).unwrap();
        let bits = dev.mmio_read(reg::HOST_FEATURES, 4).unwrap();
        assert_ne!(bits & net_features::MAC as u64, 0);
    }

    #[test]
    fn two_queues_round_trip_independently() {
        let dev = VirtioNetMmio::new(6, [0; 6]);
        // queue 0 (RX): size 64
        dev.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, 64, 4).unwrap();
        // queue 1 (TX): size 32
        dev.mmio_write(reg::QUEUE_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, 32, 4).unwrap();

        dev.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::QUEUE_NUM, 4).unwrap(), 64);
        dev.mmio_write(reg::QUEUE_SEL, 1, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::QUEUE_NUM, 4).unwrap(), 32);
    }

    #[test]
    fn status_driver_ok_sets_activated() {
        let dev = VirtioNetMmio::new(6, [0; 6]);
        assert!(!dev.is_activated());
        dev.mmio_write(reg::STATUS, status_bits::DRIVER_OK as u64, 4)
            .unwrap();
        assert!(dev.is_activated());
    }

    #[test]
    fn bus_dispatches_net_device() {
        let mut bus = MmioBus::new();
        bus.insert(
            MmioRange::new(0xd000_1000, 0x1000),
            Box::new(VirtioNetMmio::new(6, [0; 6])),
        )
        .unwrap();
        assert_eq!(bus.read(0xd000_1000 + reg::DEVICE_ID, 4).unwrap(), 1);
    }

    /// End-to-end queue round trip: build a single avail TX chain in guest
    /// memory containing virtio_net_hdr + a 4-byte payload, then call
    /// `process_tx_queue`. Without a tap fd the descriptor is still
    /// consumed (handler returns Some(0)) — so the used ring index should
    /// advance and an interrupt should fire.
    #[test]
    fn tx_queue_drains_chain_even_without_tap() {
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0; 6]);
        dev.set_guest_memory(mem.clone());

        // Wire an irq EventFd so trigger_interrupt is a no-op (we just check
        // the counter side-effects).
        #[cfg(target_os = "linux")]
        {
            let evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).expect("evt");
            dev.set_irq_evt(evt);
        }

        // Queue layout in guest mem:
        //   desc table @ 0x10_0000 (size 16 -> 256 bytes)
        //   avail ring @ 0x10_1000
        //   used ring  @ 0x10_2000
        const DESC: u64 = 0x10_0000;
        const AVAIL: u64 = 0x10_1000;
        const USED: u64 = 0x10_2000;
        const PKT: u64 = 0x10_3000;

        let payload = b"PING";
        let mut packet = [0u8; VIRTIO_NET_HDR_LEN + 4];
        packet[VIRTIO_NET_HDR_LEN..].copy_from_slice(payload);
        mem.write_slice(&packet, GuestAddress(PKT)).unwrap();

        let desc = Descriptor {
            addr: PKT,
            len: packet.len() as u32,
            flags: 0,
            next: 0,
        };
        mem.write_obj(desc, GuestAddress(DESC)).unwrap();
        mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(AVAIL))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(AVAIL + 4)).unwrap(); // ring[0] = desc 0

        // Configure TX queue (index 1).
        dev.mmio_write(reg::QUEUE_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, 16, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DESC_LOW, DESC as u32 as u64, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DESC_HIGH, DESC >> 32, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_LOW, AVAIL as u32 as u64, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_HIGH, AVAIL >> 32, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_LOW, USED as u32 as u64, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_HIGH, USED >> 32, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();

        // Kick.
        dev.mmio_write(reg::QUEUE_NOTIFY, 1, 4).unwrap();
        assert_eq!(dev.notify_count.load(Ordering::Relaxed), 1);

        // The used ring should advance.
        let used_idx: u16 = mem.read_obj(GuestAddress(USED + 2)).unwrap();
        assert_eq!(used_idx, 1, "used ring idx didn't advance");
        let used_elem: UsedElem = mem.read_obj(GuestAddress(USED + 4)).unwrap();
        assert_eq!(used_elem.id, 0);
    }

    #[test]
    fn tight_limiter_defers_tx_descriptors() {
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0; 6]);
        let clock = Arc::new(ManualClock::new(0));
        dev.set_guest_memory(mem.clone());
        dev.set_rate_limiter(RateLimiter::new_with_clock(1, 10_000, clock.clone()));

        const DESC: u64 = 0x30_0000;
        const AVAIL: u64 = 0x30_1000;
        const USED: u64 = 0x30_2000;
        const BUF0: u64 = 0x30_3000;
        const BUF1: u64 = 0x30_4000;

        for (idx, buf_addr, payload) in [(0u16, BUF0, b"PING"), (1u16, BUF1, b"PONG")] {
            let mut packet = vec![0u8; VIRTIO_NET_HDR_LEN];
            packet.extend_from_slice(payload);
            mem.write_slice(&packet, GuestAddress(buf_addr)).unwrap();
            mem.write_obj(
                Descriptor {
                    addr: buf_addr,
                    len: packet.len() as u32,
                    flags: 0,
                    next: 0,
                },
                GuestAddress(DESC + u64::from(idx) * std::mem::size_of::<Descriptor>() as u64),
            )
            .unwrap();
            mem.write_obj(idx, GuestAddress(AVAIL + 4 + u64::from(idx) * 2))
                .unwrap();
        }
        mem.write_obj(AvailRing { flags: 0, idx: 2 }, GuestAddress(AVAIL))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(USED)).unwrap();
        mem.write_obj(0u16, GuestAddress(USED + 2)).unwrap();
        configure_net_queue(&dev, QUEUE_TX as u32, DESC, AVAIL, USED);

        dev.process_tx_queue();
        assert_eq!(net_used_idx(&mem, USED), 1);

        dev.process_tx_queue();
        assert_eq!(net_used_idx(&mem, USED), 1);

        clock.advance(1_000_000_000);
        dev.process_tx_queue();
        assert_eq!(net_used_idx(&mem, USED), 2);
    }

    #[test]
    fn persist_round_trips_tx_processor_indices() {
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        let clock = Arc::new(ManualClock::new(0));
        dev.set_guest_memory(mem.clone());
        dev.set_rate_limiter(RateLimiter::new_with_clock(1, 10_000, clock));
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, net_features::MAC as u64, 4)
            .unwrap();

        const DESC: u64 = 0x36_0000;
        const AVAIL: u64 = 0x36_1000;
        const USED: u64 = 0x36_2000;
        const BUF0: u64 = 0x36_3000;
        const BUF1: u64 = 0x36_4000;

        for (idx, buf_addr, payload) in [(0u16, BUF0, b"PING"), (1u16, BUF1, b"PONG")] {
            let mut packet = vec![0u8; VIRTIO_NET_HDR_LEN];
            packet.extend_from_slice(payload);
            mem.write_slice(&packet, GuestAddress(buf_addr)).unwrap();
            mem.write_obj(
                Descriptor {
                    addr: buf_addr,
                    len: packet.len() as u32,
                    flags: 0,
                    next: 0,
                },
                GuestAddress(DESC + u64::from(idx) * std::mem::size_of::<Descriptor>() as u64),
            )
            .unwrap();
            mem.write_obj(idx, GuestAddress(AVAIL + 4 + u64::from(idx) * 2))
                .unwrap();
        }
        mem.write_obj(AvailRing { flags: 0, idx: 2 }, GuestAddress(AVAIL))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(USED)).unwrap();
        mem.write_obj(0u16, GuestAddress(USED + 2)).unwrap();
        configure_net_queue(&dev, QUEUE_TX as u32, DESC, AVAIL, USED);
        dev.mmio_write(reg::STATUS, status_bits::DRIVER_OK as u64, 4)
            .unwrap();

        dev.process_tx_queue();
        assert_eq!(net_used_idx(&mem, USED), 1);

        let state = dev.save();
        let tx_processor = state.tx_processor.expect("tx processor state");
        assert_eq!(tx_processor.last_avail_idx, 1);
        assert_eq!(tx_processor.last_used_idx, 1);

        let mut restored = VirtioNetMmio::new(6, [0; 6]);
        restored.set_guest_memory(mem.clone());
        restored.restore(state);
        assert_eq!(
            restored.mmio_read(reg::STATUS, 4).unwrap(),
            status_bits::DRIVER_OK as u64
        );
        restored
            .mmio_write(reg::QUEUE_SEL, QUEUE_TX as u64, 4)
            .unwrap();
        assert_eq!(restored.mmio_read(reg::QUEUE_NUM, 4).unwrap(), 16);
        assert_eq!(restored.mmio_read(reg::QUEUE_DESC_LOW, 4).unwrap(), DESC);
        assert_eq!(restored.mmio_read(reg::QUEUE_DRIVER_LOW, 4).unwrap(), AVAIL);
        assert_eq!(restored.mmio_read(reg::QUEUE_DEVICE_LOW, 4).unwrap(), USED);
        assert_eq!(restored.save().guest_features, net_features::MAC);

        restored.process_tx_queue();
        assert_eq!(net_used_idx(&mem, USED), 2);
    }

    /// Inject an RX packet and confirm it lands in the writable chain plus
    /// the device wrote a virtio_net_hdr in front of it.
    #[test]
    fn rx_inject_writes_hdr_then_packet() {
        use crate::virtio::vqueue::desc_flags;
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0; 6]);
        dev.set_guest_memory(mem.clone());
        #[cfg(target_os = "linux")]
        {
            let evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).expect("evt");
            dev.set_irq_evt(evt);
        }

        const DESC: u64 = 0x20_0000;
        const AVAIL: u64 = 0x20_1000;
        const USED: u64 = 0x20_2000;
        const BUF: u64 = 0x20_3000;

        // One writable desc, 2048 bytes long.
        mem.write_obj(
            Descriptor {
                addr: BUF,
                len: 2048,
                flags: desc_flags::WRITE,
                next: 0,
            },
            GuestAddress(DESC),
        )
        .unwrap();
        mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(AVAIL))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(AVAIL + 4)).unwrap();

        dev.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, 16, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DESC_LOW, DESC as u32 as u64, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DESC_HIGH, DESC >> 32, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_LOW, AVAIL as u32 as u64, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_HIGH, AVAIL >> 32, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_LOW, USED as u32 as u64, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_HIGH, USED >> 32, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();

        let packet = b"PONG-payload";
        assert!(dev.inject_rx_packet(packet));
        assert_eq!(dev.rx_packets.load(Ordering::Relaxed), 1);

        // Read back the 12-byte virtio_net_hdr_v1 + packet.
        let mut got = vec![0u8; VIRTIO_NET_HDR_LEN + packet.len()];
        mem.read_slice(&mut got, GuestAddress(BUF)).unwrap();
        // Header is zeroed except num_buffers (last u16) = 1.
        let mut expected_hdr = [0u8; VIRTIO_NET_HDR_LEN];
        expected_hdr[VIRTIO_NET_HDR_NUM_BUFFERS_OFF..].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(&got[..VIRTIO_NET_HDR_LEN], &expected_hdr);
        assert_eq!(&got[VIRTIO_NET_HDR_LEN..], packet);

        // used_idx advanced; used_elem.len = hdr+pkt.
        let used_idx: u16 = mem.read_obj(GuestAddress(USED + 2)).unwrap();
        assert_eq!(used_idx, 1);
        let used: UsedElem = mem.read_obj(GuestAddress(USED + 4)).unwrap();
        assert_eq!(used.len as usize, VIRTIO_NET_HDR_LEN + packet.len());
    }

    #[test]
    fn tight_limiter_defers_rx_descriptor() {
        use crate::virtio::vqueue::desc_flags;
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0; 6]);
        let clock = Arc::new(ManualClock::new(0));
        let mut limiter = RateLimiter::new_with_clock(1, 10_000, clock.clone());
        assert!(limiter.try_charge(1, 1));
        dev.set_guest_memory(mem.clone());
        dev.set_rate_limiter(limiter);

        const DESC: u64 = 0x34_0000;
        const AVAIL: u64 = 0x34_1000;
        const USED: u64 = 0x34_2000;
        const BUF: u64 = 0x34_3000;

        mem.write_obj(
            Descriptor {
                addr: BUF,
                len: 2048,
                flags: desc_flags::WRITE,
                next: 0,
            },
            GuestAddress(DESC),
        )
        .unwrap();
        mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(AVAIL))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(AVAIL + 4)).unwrap();
        mem.write_obj(0u16, GuestAddress(USED)).unwrap();
        mem.write_obj(0u16, GuestAddress(USED + 2)).unwrap();
        configure_net_queue(&dev, QUEUE_RX as u32, DESC, AVAIL, USED);

        let packet = b"rate-limited-rx";
        assert!(!dev.inject_rx_packet(packet));
        assert_eq!(net_used_idx(&mem, USED), 0);

        clock.advance(1_000_000_000);
        assert!(dev.inject_rx_packet(packet));
        assert_eq!(net_used_idx(&mem, USED), 1);
    }

    #[test]
    fn persist_round_trips_negotiated_transport_state() {
        let dev = VirtioNetMmio::new(6, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        dev.mmio_write(reg::HOST_FEATURES_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, net_features::MAC as u64, 4)
            .unwrap();

        dev.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, 64, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DESC_LOW, 0x0000_1000, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DESC_HIGH, 0x0000_0001, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_LOW, 0x0000_2000, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_HIGH, 0x0000_0002, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_LOW, 0x0000_3000, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_HIGH, 0x0000_0003, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();

        dev.mmio_write(reg::QUEUE_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, 32, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DESC_LOW, 0x4444_5555, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DESC_HIGH, 0x2222_3333, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_LOW, 0x8888_9999, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_HIGH, 0x6666_7777, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_LOW, 0xcccc_dddd, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_HIGH, 0xaaaa_bbbb, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::STATUS, status_bits::DRIVER_OK as u64, 4)
            .unwrap();
        dev.trigger_interrupt();

        let state = dev.save();
        let mut restored = VirtioNetMmio::new(6, [0; 6]);
        restored.restore(state.clone());

        assert_eq!(restored.save(), state);
        assert_eq!(
            restored.mmio_read(reg::STATUS, 4).unwrap(),
            state.status as u64
        );
        assert_eq!(
            restored.mmio_read(reg::INTERRUPT_STATUS, 4).unwrap(),
            state.interrupt_status as u64
        );
        restored.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        assert_eq!(restored.mmio_read(reg::QUEUE_NUM, 4).unwrap(), 64);
        assert_eq!(restored.mmio_read(reg::QUEUE_READY, 4).unwrap(), 1);
        restored.mmio_write(reg::QUEUE_SEL, 1, 4).unwrap();
        assert_eq!(restored.mmio_read(reg::QUEUE_NUM, 4).unwrap(), 32);
        assert_eq!(restored.mmio_read(reg::QUEUE_READY, 4).unwrap(), 1);
    }
}
