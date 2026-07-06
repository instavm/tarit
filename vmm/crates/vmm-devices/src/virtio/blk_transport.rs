//! virtio-mmio transport for virtio-blk: real register decode + virtqueue
//! processing + file I/O on QUEUE_NOTIFY.
//!
//! When the guest writes to QUEUE_NOTIFY, we walk the descriptor ring,
//! extract the virtio_blk_req, call BlkBackend::service(), write the
//! status byte, and update the used ring.

use crate::bus::{MmioDevice, MmioReadResult, MmioWriteResult};
use crate::persist::Persist;
use crate::rate_limit::RateLimiter;
use crate::virtio::blk::{req_type, status, BlkReqHeader};
use crate::virtio::blk_backend::BlkBackend;
use crate::virtio::regs::{reg, MAGIC};
use crate::virtio::vqueue::{
    is_valid_queue_size, QueueConfig, VirtQueueProcessor, VirtQueueProcessorState, MAX_QUEUE_SIZE,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmm_memory_backend::dirty::SoftwareDirtyBitmap;

/// virtio-blk feature bits we advertise (a deliberately minimal set).
/// VIRTIO_F_VERSION_1 (bit 32) is required for virtio-mmio v2 devices.
/// VIRTIO_BLK_F_FLUSH (bit 9) enables flush support.
///
/// We deliberately do NOT advertise VIRTIO_RING_F_EVENT_IDX (bit 29): our
/// virtqueue processor uses simple avail/used-index notification and does not
/// implement the used_event/avail_event suppression that EVENT_IDX requires.
/// Advertising it made the guest complete exactly one request and then stall,
/// because its notification bookkeeping diverged from ours.
const BLK_FEATURES_LOW: u32 = 1 << 9; // FLUSH
const BLK_FEATURES_HIGH: u32 = 1; // bit 32 = VIRTIO_F_VERSION_1

/// virtio interrupt status bits.
const VIRTIO_MMIO_INT_VRING: u32 = 0x01;

/// virtio device status bits (virtio v1.x §4.2.3.1).
pub mod status_bits {
    pub const ACKNOWLEDGE: u32 = 1 << 0;
    pub const DRIVER: u32 = 1 << 1;
    pub const FEATURES_OK: u32 = 1 << 3;
    pub const DRIVER_OK: u32 = 1 << 2;
    pub const DEVICE_NEEDS_RESET: u32 = 1 << 6;
    pub const FAILED: u32 = 1 << 7;
}

/// Queue config stored per queue index.
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
            log::warn!("virtio-blk: QUEUE_NUM {size} exceeds u16 — rejecting");
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
                "virtio-blk: invalid QUEUE_NUM {size} (must be power-of-two <= {MAX_QUEUE_SIZE})"
            );
            self.size = 0;
            self.ready = false;
        }
    }

    fn set_ready(&mut self, ready: bool) {
        if ready && !self.valid_size() {
            log::warn!(
                "virtio-blk: QUEUE_READY ignored for invalid QUEUE_NUM {}",
                self.size
            );
            self.ready = false;
        } else {
            self.ready = ready;
        }
    }
}

/// Snapshot state for virtio-blk MMIO negotiation and queue configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VirtioBlkMmioState {
    pub status: u32,
    pub queue_sel: u32,
    pub host_features_sel: u32,
    pub guest_features_sel: u32,
    pub guest_features_low: u32,
    pub guest_features_high: u32,
    queues: Vec<QueueState>,
    pub activated: bool,
    pub interrupt_status: u32,
    #[serde(default)]
    processor: Option<VirtQueueProcessorState>,
}

/// The virtio-mmio transport for a block device.
/// Owns the BlkBackend and processes virtqueue requests on QUEUE_NOTIFY.
pub struct VirtioBlkMmio {
    pub irq: u32,
    pub device_id: u32,
    pub vendor_id: u32,
    pub version: u32,
    status: AtomicU32,
    queue_sel: AtomicU32,
    /// Feature-bank selector for HOST_FEATURES (0 = low 32, 1 = high 32).
    host_features_sel: AtomicU32,
    /// Feature-bank selector for GUEST_FEATURES.
    guest_features_sel: AtomicU32,
    /// Acknowledged guest features (low 32 bits).
    guest_features_low: AtomicU32,
    /// Acknowledged guest features (high 32 bits).
    guest_features_high: AtomicU32,
    /// Per-queue config (we only use queue 0 for virtio-blk).
    queues: Mutex<Vec<QueueState>>,
    /// The block backend (file-backed I/O).
    backend: Mutex<Option<BlkBackend>>,
    /// Guest memory (set after VM creation).
    guest_mem: Mutex<Option<std::sync::Arc<GuestMemoryMmap>>>,
    host_dirty: Mutex<Option<SoftwareDirtyBitmap>>,
    /// Whether the device has been activated (DRIVER_OK set + queue ready).
    activated: AtomicBool,
    /// Persistent virtqueue processor (tracks last_avail_idx across calls).
    processor: Mutex<Option<VirtQueueProcessor>>,
    /// Optional per-device token bucket for IOPS and bandwidth caps.
    rate_limiter: Mutex<Option<RateLimiter>>,
    /// Interrupt status (set when device raises an interrupt).
    interrupt_status: AtomicU32,
    /// IRQ EventFd — written to when the device needs to interrupt the guest.
    /// Set by the VMM after registering the irqfd with KVM (Linux only).
    #[cfg(target_os = "linux")]
    irq_evt: Mutex<Option<vmm_sys_util::eventfd::EventFd>>,
    /// Diagnostic counter: number of QUEUE_NOTIFY writes received from the
    /// guest. Used by the OCI-boot-to-login probe to distinguish "guest never
    /// kicked the queue" (driver didn't activate) from "queue kicked but
    /// backend errored." Negligible overhead — one relaxed increment per kick.
    pub notify_count: AtomicU64,
    /// Diagnostic counter: number of STATUS writes (driver progress through
    /// ACK → DRIVER → FEATURES_OK → DRIVER_OK). Reading STATUS shows the
    /// *current* bits; this shows the *number of transitions*.
    pub status_writes: AtomicU64,
}

impl VirtioBlkMmio {
    /// Create a new virtio-blk MMIO device with a file-backed backend.
    pub fn new(irq: u32, backend: BlkBackend) -> Self {
        Self {
            irq,
            device_id: 2,
            vendor_id: 0,
            version: 2,
            status: AtomicU32::new(0),
            queue_sel: AtomicU32::new(0),
            host_features_sel: AtomicU32::new(0),
            guest_features_sel: AtomicU32::new(0),
            guest_features_low: AtomicU32::new(0),
            guest_features_high: AtomicU32::new(0),
            queues: Mutex::new(vec![QueueState::default()]),
            backend: Mutex::new(Some(backend)),
            guest_mem: Mutex::new(None),
            host_dirty: Mutex::new(None),
            activated: AtomicBool::new(false),
            processor: Mutex::new(None),
            rate_limiter: Mutex::new(None),
            interrupt_status: AtomicU32::new(0),
            #[cfg(target_os = "linux")]
            irq_evt: Mutex::new(None),
            notify_count: AtomicU64::new(0),
            status_writes: AtomicU64::new(0),
        }
    }

    /// Create a stub device (no backend — for testing the transport).
    pub fn new_stub(irq: u32, device_id: u32) -> Self {
        Self {
            irq,
            device_id,
            vendor_id: 0,
            version: 2,
            status: AtomicU32::new(0),
            queue_sel: AtomicU32::new(0),
            host_features_sel: AtomicU32::new(0),
            guest_features_sel: AtomicU32::new(0),
            guest_features_low: AtomicU32::new(0),
            guest_features_high: AtomicU32::new(0),
            queues: Mutex::new(vec![QueueState::default()]),
            backend: Mutex::new(None),
            guest_mem: Mutex::new(None),
            host_dirty: Mutex::new(None),
            activated: AtomicBool::new(false),
            processor: Mutex::new(None),
            rate_limiter: Mutex::new(None),
            interrupt_status: AtomicU32::new(0),
            #[cfg(target_os = "linux")]
            irq_evt: Mutex::new(None),
            notify_count: AtomicU64::new(0),
            status_writes: AtomicU64::new(0),
        }
    }

    /// Read the current guest-facing STATUS register (post-DRIVER_OK = activated).
    pub fn current_status(&self) -> u32 {
        self.status.load(Ordering::Relaxed)
    }

    /// True once the guest has set DRIVER_OK.
    pub fn is_activated(&self) -> bool {
        self.activated.load(Ordering::Relaxed)
    }

    /// Set the IRQ EventFd (Linux only — called after registering irqfd with KVM).
    #[cfg(target_os = "linux")]
    pub fn set_irq_evt(&self, evt: vmm_sys_util::eventfd::EventFd) {
        *self.irq_evt.lock().unwrap() = Some(evt);
    }

    /// Signal an interrupt to the guest.
    #[cfg(target_os = "linux")]
    fn trigger_interrupt(&self) {
        self.interrupt_status
            .fetch_or(VIRTIO_MMIO_INT_VRING, Ordering::SeqCst);
        if let Some(evt) = self.irq_evt.lock().unwrap().as_ref() {
            let _ = evt.write(1);
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn trigger_interrupt(&self) {
        self.interrupt_status
            .fetch_or(VIRTIO_MMIO_INT_VRING, Ordering::SeqCst);
    }

    /// Set the guest memory (called after KvmVm creation).
    pub fn set_guest_memory(&self, mem: std::sync::Arc<GuestMemoryMmap>) {
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

    fn read_descs(mem: &GuestMemoryMmap, descs: &[(u64, u32)]) -> Option<Vec<u8>> {
        let total_len = descs
            .iter()
            .try_fold(0usize, |acc, (_, len)| acc.checked_add(*len as usize))?;
        let mut data = Vec::with_capacity(total_len);
        for &(addr, len) in descs {
            if len == 0 {
                continue;
            }
            let off = data.len();
            let end = off.checked_add(len as usize)?;
            data.resize(end, 0);
            if mem
                .read_slice(&mut data[off..end], GuestAddress(addr))
                .is_err()
            {
                return None;
            }
        }
        Some(data)
    }

    fn writable_data_len(writable: &[(u64, u32)]) -> Option<usize> {
        writable
            .iter()
            .take(writable.len().saturating_sub(1))
            .try_fold(0usize, |acc, (_, len)| acc.checked_add(*len as usize))
    }

    fn request_bytes(
        header: &BlkReqHeader,
        readable_len: usize,
        writable: &[(u64, u32)],
    ) -> Option<u64> {
        match header.req_type {
            req_type::IN => u64::try_from(Self::writable_data_len(writable)?).ok(),
            req_type::OUT => u64::try_from(readable_len.checked_sub(BlkReqHeader::SIZE)?).ok(),
            req_type::GET_ID => Some(20),
            req_type::FLUSH => Some(0),
            _ => Some(0),
        }
    }

    fn write_writable_data(
        mem: &GuestMemoryMmap,
        dirty: Option<&SoftwareDirtyBitmap>,
        writable: &[(u64, u32)],
        data: &[u8],
    ) -> usize {
        let mut cursor = 0usize;
        for &(addr, len) in writable.iter().take(writable.len().saturating_sub(1)) {
            if cursor >= data.len() {
                break;
            }
            let take = (len as usize).min(data.len() - cursor);
            if take == 0 {
                continue;
            }
            let Some(next_cursor) = cursor.checked_add(take) else {
                break;
            };
            if mem
                .write_slice(&data[cursor..next_cursor], GuestAddress(addr))
                .is_err()
            {
                break;
            }
            if let Some(dirty) = dirty {
                dirty.mark_range(addr, take as u64);
            }
            cursor = next_cursor;
        }
        cursor
    }

    fn write_status(
        mem: &GuestMemoryMmap,
        dirty: Option<&SoftwareDirtyBitmap>,
        writable: &[(u64, u32)],
        st: u8,
    ) -> u32 {
        match writable.last() {
            Some((addr, len)) if *len > 0 && mem.write_obj(st, GuestAddress(*addr)).is_ok() => {
                if let Some(dirty) = dirty {
                    dirty.mark_range(*addr, 1);
                }
                1
            }
            _ => 0,
        }
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

    /// Process the virtqueue when the guest kicks (writes to QUEUE_NOTIFY).
    fn process_queue(&self, _queue_idx: u32) {
        let mem = match self.guest_mem.lock().unwrap().clone() {
            Some(m) => m,
            None => {
                log::debug!("BLK process_queue: guest_mem is None");
                return;
            }
        };
        let dirty = self.host_dirty.lock().unwrap().clone();

        let qs = self.queues.lock().unwrap();
        let Some(q) = qs.first() else {
            log::error!("BLK process_queue: missing queue 0 in restored state");
            return;
        };
        if !q.ready || !q.valid_size() {
            log::debug!(
                "BLK process_queue: queue not ready or invalid (ready={} size={})",
                q.ready,
                q.size
            );
            return;
        }

        let config = QueueConfig {
            size: q.size,
            desc_table_addr: q.desc_table_addr,
            avail_ring_addr: q.avail_ring_addr,
            used_ring_addr: q.used_ring_addr,
            ready: q.ready,
        };
        drop(qs);

        // Get or create the persistent processor.
        let mut proc_guard = self.processor.lock().unwrap();
        if proc_guard.is_none() {
            *proc_guard = Some(VirtQueueProcessor::new(config));
        } else {
            proc_guard.as_mut().unwrap().update_config(config);
        }

        let mut backend_guard = self.backend.lock().unwrap();
        if backend_guard.is_none() {
            return;
        }
        let backend = backend_guard.as_mut().unwrap();

        // Process available requests until the queue is drained or the limiter
        // asks us to defer the next descriptor chain.
        let _count = proc_guard
            .as_mut()
            .unwrap()
            .process_queue_descriptors_dirty(&mem, dirty.as_ref(), |readable, writable| {
                let used_len = (readable.len() + writable.len()) as u32;
                let readable = match Self::read_descs(&mem, readable) {
                    Some(data) => data,
                    None => {
                        if !self.rate_limit_allows(0) {
                            return None;
                        }
                        Self::write_status(&mem, dirty.as_ref(), writable, status::IO_ERR);
                        return Some(used_len);
                    }
                };

                // Parse the virtio-blk request header from the first readable descriptor.
                if readable.len() < 16 {
                    if !self.rate_limit_allows(0) {
                        return None;
                    }
                    Self::write_status(&mem, dirty.as_ref(), writable, status::IO_ERR);
                    return Some(used_len);
                }

                let header = match BlkReqHeader::from_bytes(&readable[..16]) {
                    Some(h) => {
                        log::debug!(
                            "handler: req_type={} sector={} readable.len()={}",
                            h.req_type,
                            h.sector,
                            readable.len()
                        );
                        h
                    }
                    None => {
                        log::warn!(
                            "handler: failed to parse header, readable.len()={}",
                            readable.len()
                        );
                        if !self.rate_limit_allows(0) {
                            return None;
                        }
                        Self::write_status(&mem, dirty.as_ref(), writable, status::IO_ERR);
                        return Some(used_len);
                    }
                };

                let request_bytes = match Self::request_bytes(&header, readable.len(), writable) {
                    Some(bytes) => bytes,
                    None => {
                        if !self.rate_limit_allows(0) {
                            return None;
                        }
                        Self::write_status(&mem, dirty.as_ref(), writable, status::IO_ERR);
                        return Some(used_len);
                    }
                };
                if !self.rate_limit_allows(request_bytes) {
                    return None;
                }

                // For IN (read): the writable buffer is the data buffer + status byte.
                // For OUT (write): the readable data after the header is the data.
                // For FLUSH: no data, just status.
                let st = match header.req_type {
                    req_type::IN => {
                        let Some(data_len) = Self::writable_data_len(writable) else {
                            Self::write_status(&mem, dirty.as_ref(), writable, status::IO_ERR);
                            return Some(used_len);
                        };
                        if data_len == 0 {
                            status::IO_ERR
                        } else {
                            let mut data = vec![0u8; data_len];
                            let st = backend.service(&header, &mut data);
                            log::debug!("IN: service returned status={st}");
                            Self::write_writable_data(&mem, dirty.as_ref(), writable, &data);
                            st
                        }
                    }
                    req_type::OUT => {
                        let data = &readable[16..];
                        let mut data_mut = data.to_vec();
                        backend.service(&header, &mut data_mut)
                    }
                    req_type::FLUSH => backend.service(&header, &mut []),
                    req_type::GET_ID => {
                        let mut id_buf = [0u8; 20];
                        let st = backend.service(&header, &mut id_buf);
                        Self::write_writable_data(&mem, dirty.as_ref(), writable, &id_buf);
                        st
                    }
                    _ => status::UNSUPP,
                };

                Self::write_status(&mem, dirty.as_ref(), writable, st);
                Some(used_len)
            });

        // If we processed any requests, signal an interrupt to the guest.
        if _count > 0 {
            log::debug!("BLK process_queue: processed {_count} request(s)");
            self.trigger_interrupt();
        }
    }
}

impl Persist for VirtioBlkMmio {
    type State = VirtioBlkMmioState;

    fn save(&self) -> Self::State {
        VirtioBlkMmioState {
            status: self.status.load(Ordering::Relaxed),
            queue_sel: self.queue_sel.load(Ordering::Relaxed),
            host_features_sel: self.host_features_sel.load(Ordering::Relaxed),
            guest_features_sel: self.guest_features_sel.load(Ordering::Relaxed),
            guest_features_low: self.guest_features_low.load(Ordering::Relaxed),
            guest_features_high: self.guest_features_high.load(Ordering::Relaxed),
            queues: self.queues.lock().unwrap().clone(),
            activated: self.activated.load(Ordering::Relaxed),
            interrupt_status: self.interrupt_status.load(Ordering::SeqCst),
            processor: self
                .processor
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
        self.guest_features_low
            .store(state.guest_features_low, Ordering::Relaxed);
        self.guest_features_high
            .store(state.guest_features_high, Ordering::Relaxed);
        *self.queues.lock().unwrap() = state.queues;
        self.activated.store(state.activated, Ordering::Relaxed);
        self.interrupt_status
            .store(state.interrupt_status, Ordering::SeqCst);
        let config = self
            .queues
            .lock()
            .unwrap()
            .first()
            .map(Self::queue_config_from_state)
            .unwrap_or_default();
        *self.processor.lock().unwrap() = state
            .processor
            .map(|p| VirtQueueProcessor::from_state(config, p));
    }
}

impl Persist for std::sync::Arc<VirtioBlkMmio> {
    type State = VirtioBlkMmioState;

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
        self.guest_features_low
            .store(state.guest_features_low, Ordering::Relaxed);
        self.guest_features_high
            .store(state.guest_features_high, Ordering::Relaxed);
        *self.queues.lock().unwrap() = state.queues;
        self.activated.store(state.activated, Ordering::Relaxed);
        self.interrupt_status
            .store(state.interrupt_status, Ordering::SeqCst);
        let config = self
            .queues
            .lock()
            .unwrap()
            .first()
            .map(VirtioBlkMmio::queue_config_from_state)
            .unwrap_or_default();
        *self.processor.lock().unwrap() = state
            .processor
            .map(|p| VirtQueueProcessor::from_state(config, p));
    }
}

impl MmioDevice for VirtioBlkMmio {
    fn mmio_read(&self, off: u64, _len: u8) -> MmioReadResult {
        let val = match off {
            reg::MAGIC_VALUE => MAGIC,
            reg::VERSION => self.version,
            reg::DEVICE_ID => self.device_id,
            reg::VENDOR_ID => self.vendor_id,
            reg::HOST_FEATURES => {
                // Return the selected 32-bit page of host features.
                // Page 0 = bits 0-31 (EVENT_IDX, FLUSH, etc.)
                // Page 1 = bits 32-63 (VIRTIO_F_VERSION_1)
                match self.host_features_sel.load(Ordering::Relaxed) {
                    0 => BLK_FEATURES_LOW,
                    1 => BLK_FEATURES_HIGH,
                    _ => 0,
                }
            }
            reg::QUEUE_NUM_MAX => MAX_QUEUE_SIZE as u32,
            reg::QUEUE_READY => {
                // The kernel writes QUEUE_READY=1 then reads it back; returning
                // 0 here (the old `_ => 0` fallthrough) makes Linux >=5.15 tear
                // the queue down, so block I/O never completes and root mount
                // hangs. Report the actual per-queue ready flag.
                let qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    u32::from(qs[sel].ready && qs[sel].valid_size())
                } else {
                    0
                }
            }
            reg::STATUS => self.status.load(Ordering::Relaxed),
            reg::INTERRUPT_STATUS => self.interrupt_status.load(Ordering::SeqCst),
            reg::CONFIG_GENERATION => 0,
            // Device-specific config space (offset 0x100+).
            // virtio-blk config: u64 capacity (sectors) at offset 0,
            // u32 blk_size at offset 8 (optional).
            off if off >= reg::CONFIG => {
                let cfg_off = (off - reg::CONFIG) as usize;
                let backend = self.backend.lock().unwrap();
                match &*backend {
                    Some(b) => {
                        let cap_bytes = b.sectors.to_le_bytes();
                        let blk_size: u32 = 512;
                        let size_bytes = blk_size.to_le_bytes();
                        // Build the config space: [capacity u64][blk_size u32]
                        let mut cfg = [0u8; 12];
                        cfg[..8].copy_from_slice(&cap_bytes);
                        cfg[8..12].copy_from_slice(&size_bytes);
                        // Return the requested 4-byte window.
                        let end = (cfg_off + 4).min(cfg.len());
                        if cfg_off < end {
                            u32::from_le_bytes([
                                cfg[cfg_off],
                                cfg.get(cfg_off + 1).copied().unwrap_or(0),
                                cfg.get(cfg_off + 2).copied().unwrap_or(0),
                                cfg.get(cfg_off + 3).copied().unwrap_or(0),
                            ])
                        } else {
                            0
                        }
                    }
                    None => 0,
                }
            }
            reg::QUEUE_NUM => {
                let qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].size as u32
                } else {
                    0
                }
            }
            reg::QUEUE_DESC_LOW => {
                let qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    (qs[sel].desc_table_addr & 0xFFFFFFFF) as u32
                } else {
                    0
                }
            }
            reg::QUEUE_DESC_HIGH => {
                let qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    (qs[sel].desc_table_addr >> 32) as u32
                } else {
                    0
                }
            }
            reg::QUEUE_DRIVER_LOW => {
                let qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    (qs[sel].avail_ring_addr & 0xFFFFFFFF) as u32
                } else {
                    0
                }
            }
            reg::QUEUE_DRIVER_HIGH => {
                let qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    (qs[sel].avail_ring_addr >> 32) as u32
                } else {
                    0
                }
            }
            reg::QUEUE_DEVICE_LOW => {
                let qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    (qs[sel].used_ring_addr & 0xFFFFFFFF) as u32
                } else {
                    0
                }
            }
            reg::QUEUE_DEVICE_HIGH => {
                let qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    (qs[sel].used_ring_addr >> 32) as u32
                } else {
                    0
                }
            }
            _ => 0,
        };
        log::debug!("BLK-R off=0x{off:x} -> 0x{val:x}");
        Ok(val as u64)
    }

    fn mmio_write(&self, off: u64, val: u64, _len: u8) -> MmioWriteResult {
        let val = val as u32;
        log::debug!("BLK-W off=0x{off:x} val=0x{val:x}");
        match off {
            reg::STATUS => {
                self.status_writes.fetch_add(1, Ordering::Relaxed);
                if val == 0 {
                    // Spec §4.2.3.1: writing 0 triggers a device reset. Clear
                    // all negotiated state so a re-probe starts clean.
                    self.status.store(0, Ordering::SeqCst);
                    self.activated.store(false, Ordering::SeqCst);
                    self.host_features_sel.store(0, Ordering::SeqCst);
                    self.guest_features_sel.store(0, Ordering::SeqCst);
                    self.guest_features_low.store(0, Ordering::SeqCst);
                    self.guest_features_high.store(0, Ordering::SeqCst);
                    self.queue_sel.store(0, Ordering::SeqCst);
                    self.interrupt_status.store(0, Ordering::SeqCst);
                    for q in self.queues.lock().unwrap().iter_mut() {
                        *q = QueueState::default();
                    }
                    *self.processor.lock().unwrap() = None;
                    return Ok(());
                }
                self.status.store(val, Ordering::Relaxed);
                // Check if DRIVER_OK is set → device is activated.
                if val & status_bits::DRIVER_OK != 0 {
                    self.activated.store(true, Ordering::Relaxed);
                }
            }
            reg::HOST_FEATURES_SEL => {
                self.host_features_sel.store(val, Ordering::Relaxed);
            }
            reg::GUEST_FEATURES => {
                // Guest acknowledges features. Store the selected page.
                match self.guest_features_sel.load(Ordering::Relaxed) {
                    0 => {
                        self.guest_features_low.store(val, Ordering::Relaxed);
                    }
                    1 => {
                        self.guest_features_high.store(val, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
            reg::GUEST_FEATURES_SEL => {
                self.guest_features_sel.store(val, Ordering::Relaxed);
            }
            reg::QUEUE_SEL => {
                self.queue_sel.store(val, Ordering::Relaxed);
            }
            reg::QUEUE_NUM => {
                let mut qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].set_size(val);
                }
            }
            reg::QUEUE_DESC_LOW => {
                let mut qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].desc_table_addr = (qs[sel].desc_table_addr & !0xFFFFFFFF) | val as u64;
                }
            }
            reg::QUEUE_DESC_HIGH => {
                let mut qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].desc_table_addr =
                        (qs[sel].desc_table_addr & 0xFFFFFFFF) | (val as u64) << 32;
                }
            }
            reg::QUEUE_DRIVER_LOW => {
                let mut qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].avail_ring_addr = (qs[sel].avail_ring_addr & !0xFFFFFFFF) | val as u64;
                }
            }
            reg::QUEUE_DRIVER_HIGH => {
                let mut qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].avail_ring_addr =
                        (qs[sel].avail_ring_addr & 0xFFFFFFFF) | (val as u64) << 32;
                }
            }
            reg::QUEUE_DEVICE_LOW => {
                let mut qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].used_ring_addr = (qs[sel].used_ring_addr & !0xFFFFFFFF) | val as u64;
                }
            }
            reg::QUEUE_DEVICE_HIGH => {
                let mut qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].used_ring_addr =
                        (qs[sel].used_ring_addr & 0xFFFFFFFF) | (val as u64) << 32;
                }
            }
            reg::QUEUE_READY => {
                let mut qs = self.queues.lock().unwrap();
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].set_ready(val != 0);
                    log::debug!("QUEUE_READY: qs[{sel}].ready = {}", qs[sel].ready);
                }
            }
            reg::QUEUE_NOTIFY => {
                // THE CRITICAL PATH: guest kicked the queue — process I/O.
                self.notify_count.fetch_add(1, Ordering::Relaxed);
                self.process_queue(val);
            }
            reg::INTERRUPT_ACK => {
                // Driver acknowledges interrupt — clear the acknowledged bits.
                self.interrupt_status.fetch_and(!val, Ordering::SeqCst);
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{MmioBus, MmioRange};
    use crate::rate_limit::{RateLimitClock, RateLimiter};
    use crate::virtio::vqueue::{desc_flags, AvailRing, Descriptor};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use vm_memory::GuestMemoryMmap;

    const TEST_DESC: u64 = 0x10_0000;
    const TEST_AVAIL: u64 = 0x10_1000;
    const TEST_USED: u64 = 0x10_2000;
    const TEST_HEADER_BASE: u64 = 0x10_3000;
    const TEST_DATA_BASE: u64 = 0x10_4000;
    const TEST_STATUS_BASE: u64 = 0x10_8000;
    const TEST_QUEUE_SIZE: u16 = 16;

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

    fn test_disk_path(name: &str) -> PathBuf {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"));
        let dir = base.join("vmm-devices-rate-limit-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{}-{}.blk", name, std::process::id()))
    }

    fn new_test_backend(name: &str) -> (BlkBackend, PathBuf) {
        let path = test_disk_path(name);
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        (BlkBackend::open(&path, false).unwrap(), path)
    }

    fn new_test_mem() -> Arc<GuestMemoryMmap> {
        Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4 * 1024 * 1024)]).unwrap())
    }

    fn configure_test_blk_queue(dev: &VirtioBlkMmio, mem: Arc<GuestMemoryMmap>) {
        dev.set_guest_memory(mem);
        dev.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, TEST_QUEUE_SIZE as u64, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DESC_LOW, TEST_DESC, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DESC_HIGH, 0, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_LOW, TEST_AVAIL, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_HIGH, 0, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_LOW, TEST_USED, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_HIGH, 0, 4).unwrap();
        dev.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();
    }

    fn setup_blk_out_requests(mem: &GuestMemoryMmap, count: u16) {
        mem.write_obj(0u16, GuestAddress(TEST_USED)).unwrap();
        mem.write_obj(0u16, GuestAddress(TEST_USED + 2)).unwrap();
        mem.write_obj(
            AvailRing {
                flags: 0,
                idx: count,
            },
            GuestAddress(TEST_AVAIL),
        )
        .unwrap();

        for i in 0..count {
            let head = i * 3;
            let header_addr = TEST_HEADER_BASE + u64::from(i) * 0x100;
            let data_addr = TEST_DATA_BASE + u64::from(i) * 0x1000;
            let status_addr = TEST_STATUS_BASE + u64::from(i) * 0x100;
            let header = BlkReqHeader {
                req_type: req_type::OUT,
                reserved: 0,
                sector: u64::from(i),
            };
            let data = vec![i as u8; 512];

            mem.write_obj(header, GuestAddress(header_addr)).unwrap();
            mem.write_slice(&data, GuestAddress(data_addr)).unwrap();
            mem.write_obj(head, GuestAddress(TEST_AVAIL + 4 + u64::from(i) * 2))
                .unwrap();
            mem.write_obj(
                Descriptor {
                    addr: header_addr,
                    len: BlkReqHeader::SIZE as u32,
                    flags: desc_flags::NEXT,
                    next: head + 1,
                },
                GuestAddress(
                    TEST_DESC + u64::from(head) * std::mem::size_of::<Descriptor>() as u64,
                ),
            )
            .unwrap();
            mem.write_obj(
                Descriptor {
                    addr: data_addr,
                    len: data.len() as u32,
                    flags: desc_flags::NEXT,
                    next: head + 2,
                },
                GuestAddress(
                    TEST_DESC + u64::from(head + 1) * std::mem::size_of::<Descriptor>() as u64,
                ),
            )
            .unwrap();
            mem.write_obj(
                Descriptor {
                    addr: status_addr,
                    len: 1,
                    flags: desc_flags::WRITE,
                    next: 0,
                },
                GuestAddress(
                    TEST_DESC + u64::from(head + 2) * std::mem::size_of::<Descriptor>() as u64,
                ),
            )
            .unwrap();
            mem.write_obj(0xffu8, GuestAddress(status_addr)).unwrap();
        }
    }

    fn used_idx(mem: &GuestMemoryMmap) -> u16 {
        mem.read_obj(GuestAddress(TEST_USED + 2)).unwrap()
    }

    #[test]
    fn magic_reads_back_as_virt() {
        let dev = VirtioBlkMmio::new_stub(5, 2);
        assert_eq!(dev.mmio_read(reg::MAGIC_VALUE, 4).unwrap(), MAGIC as u64);
    }

    #[test]
    fn status_round_trips() {
        let dev = VirtioBlkMmio::new_stub(5, 2);
        dev.mmio_write(reg::STATUS, status_bits::DRIVER_OK as u64, 4)
            .unwrap();
        assert_eq!(
            dev.mmio_read(reg::STATUS, 4).unwrap(),
            status_bits::DRIVER_OK as u64
        );
    }

    #[test]
    fn queue_config_round_trips() {
        let dev = VirtioBlkMmio::new_stub(5, 2);
        // Select queue 0.
        dev.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        // Set queue size.
        dev.mmio_write(reg::QUEUE_NUM, 64, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::QUEUE_NUM, 4).unwrap(), 64);
        // Set desc table addr.
        dev.mmio_write(reg::QUEUE_DESC_LOW, 0x100000, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DESC_HIGH, 0, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::QUEUE_DESC_LOW, 4).unwrap(), 0x100000);
        // Set queue ready.
        dev.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();
    }

    #[test]
    fn bus_dispatches_to_device() {
        let mut bus = MmioBus::new();
        let dev = VirtioBlkMmio::new_stub(5, 2);
        bus.insert(MmioRange::new(0xd000_0000, 0x1000), Box::new(dev))
            .unwrap();
        assert_eq!(
            bus.read(0xd000_0000 + reg::MAGIC_VALUE, 4).unwrap(),
            MAGIC as u64
        );
        assert_eq!(bus.read(0xd000_0000 + reg::DEVICE_ID, 4).unwrap(), 2);
    }

    #[test]
    fn queue_notify_without_mem_doesnt_panic() {
        let dev = VirtioBlkMmio::new_stub(5, 2);
        // Should not panic even without guest memory set.
        dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();
    }

    #[test]
    fn no_limiter_processes_all_blk_requests() {
        let (backend, path) = new_test_backend("blk-unlimited");
        let mem = new_test_mem();
        let dev = VirtioBlkMmio::new(5, backend);
        configure_test_blk_queue(&dev, mem.clone());
        setup_blk_out_requests(&mem, 2);

        dev.process_queue(0);

        assert_eq!(used_idx(&mem), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn tight_limiter_defers_blk_requests_without_consuming_descriptors() {
        let (backend, path) = new_test_backend("blk-limited");
        let mem = new_test_mem();
        let dev = VirtioBlkMmio::new(5, backend);
        let clock = Arc::new(ManualClock::new(0));
        dev.set_rate_limiter(RateLimiter::new_with_clock(1, 4096, clock.clone()));
        configure_test_blk_queue(&dev, mem.clone());
        setup_blk_out_requests(&mem, 2);

        dev.process_queue(0);
        assert_eq!(used_idx(&mem), 1);

        dev.process_queue(0);
        assert_eq!(used_idx(&mem), 1);

        clock.advance(1_000_000_000);
        dev.process_queue(0);
        assert_eq!(used_idx(&mem), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn persist_round_trips_queue_processor_indices() {
        let (backend, path) = new_test_backend("blk-persist-processor");
        let mem = new_test_mem();
        let dev = VirtioBlkMmio::new(5, backend);
        let clock = Arc::new(ManualClock::new(0));
        dev.set_rate_limiter(RateLimiter::new_with_clock(1, 4096, clock));
        dev.mmio_write(reg::HOST_FEATURES_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, 0x2000_0200, 4).unwrap();
        configure_test_blk_queue(&dev, mem.clone());
        dev.mmio_write(reg::STATUS, status_bits::DRIVER_OK as u64, 4)
            .unwrap();
        setup_blk_out_requests(&mem, 2);

        dev.process_queue(0);
        assert_eq!(used_idx(&mem), 1);

        let state = dev.save();
        let processor = state.processor.expect("processor state");
        assert_eq!(processor.last_avail_idx, 1);
        assert_eq!(processor.last_used_idx, 1);

        let backend = BlkBackend::open(&path, false).unwrap();
        let mut restored = VirtioBlkMmio::new(5, backend);
        restored.set_guest_memory(mem.clone());
        restored.restore(state);
        assert_eq!(
            restored.mmio_read(reg::STATUS, 4).unwrap(),
            status_bits::DRIVER_OK as u64
        );
        restored.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        assert_eq!(restored.mmio_read(reg::QUEUE_NUM, 4).unwrap(), 16);
        assert_eq!(
            restored.mmio_read(reg::QUEUE_DESC_LOW, 4).unwrap(),
            TEST_DESC
        );
        assert_eq!(
            restored.mmio_read(reg::QUEUE_DRIVER_LOW, 4).unwrap(),
            TEST_AVAIL
        );
        assert_eq!(
            restored.mmio_read(reg::QUEUE_DEVICE_LOW, 4).unwrap(),
            TEST_USED
        );

        restored.process_queue(0);
        assert_eq!(used_idx(&mem), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn persist_round_trips_negotiated_transport_state() {
        let dev = VirtioBlkMmio::new_stub(5, 2);
        dev.mmio_write(reg::HOST_FEATURES_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, 0x2000_0200, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, 1, 4).unwrap();
        dev.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, 64, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DESC_LOW, 0x5566_7788, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DESC_HIGH, 0x1122_3344, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_LOW, 0xddee_ff00, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_HIGH, 0x99aa_bbcc, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_LOW, 0x89ab_cdef, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_HIGH, 0x0123_4567, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();
        dev.mmio_write(reg::STATUS, status_bits::DRIVER_OK as u64, 4)
            .unwrap();
        dev.trigger_interrupt();

        let state = dev.save();
        let mut restored = VirtioBlkMmio::new_stub(5, 2);
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
    }
}
