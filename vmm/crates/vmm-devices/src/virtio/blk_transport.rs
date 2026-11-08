//! virtio-mmio transport for virtio-blk: real register decode + virtqueue
//! queue processing and a file-backed data plane.
//!
//! When the guest writes to QUEUE_NOTIFY, we walk the descriptor ring,
//! extract the virtio_blk_req, call BlkBackend::service(), write the
//! status byte, and update the used ring.

use crate::bus::{MmioDevice, MmioError, MmioReadResult, MmioWriteResult};
use crate::persist::{Persist, PersistError};
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
/// VIRTIO_RING_F_EVENT_IDX (bit 29): the queue processor honours the driver's
/// used_event and publishes avail_event (re-checking the ring after it), so
/// the guest interrupts and kicks only when the other side asks. (Offering it
/// before the processor wrote avail_event left the guest waiting for
/// permission to kick after its first request.)
/// VIRTIO_BLK_F_SIZE_MAX (bit 1) caps every data segment at the virtqueue's
/// per-descriptor limit. Without it Linux sends one segment per request of
/// any size, and a segment over that limit was rejected and never completed,
/// hanging the guest's I/O under large sequential writes.
const BLK_FEATURES_LOW: u32 = (1 << 9) | VIRTIO_BLK_F_SIZE_MAX | VIRTIO_RING_F_EVENT_IDX; // FLUSH + SIZE_MAX + EVENT_IDX
const VIRTIO_RING_F_EVENT_IDX: u32 = 1 << 29;
const VIRTIO_BLK_F_SIZE_MAX: u32 = 1 << 1;
/// VIRTIO_BLK_F_RO (bit 5): offered only for a read-only backend, so the guest
/// marks the disk read-only instead of discovering it through write errors.
const VIRTIO_BLK_F_RO: u32 = 1 << 5;
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
    #[cfg(feature = "test-failpoints")]
    delayed_services: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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
    /// Permanently fail the transport after its isolated queue worker exits
    /// unexpectedly. Snapshot capture then fails closed instead of serializing
    /// a VM whose storage queue can no longer make progress.
    pub fn fail_worker(&self, context: &str) {
        self.fail_device(context);
    }

    #[cfg(feature = "test-failpoints")]
    pub fn set_test_service_delay(&self, delay: std::time::Duration) -> MmioWriteResult {
        let mut backend = self.try_lock_state(&self.backend, "backend")?;
        let backend = backend.as_mut().ok_or(MmioError::Device)?;
        backend.set_test_service_delay(delay);
        Ok(())
    }

    #[cfg(feature = "test-failpoints")]
    pub fn test_delayed_services(&self) -> usize {
        self.delayed_services.load(Ordering::SeqCst)
    }

    fn fail_device(&self, context: &str) {
        log::error!("virtio-blk: {context}");
        self.status.fetch_or(
            status_bits::DEVICE_NEEDS_RESET | status_bits::FAILED,
            Ordering::SeqCst,
        );
        self.activated.store(false, Ordering::SeqCst);
    }

    fn is_failed(&self) -> bool {
        self.status.load(Ordering::SeqCst) & (status_bits::DEVICE_NEEDS_RESET | status_bits::FAILED)
            != 0
    }

    fn ensure_operational(&self) -> MmioWriteResult {
        if self.is_failed() {
            Err(MmioError::Device)
        } else {
            Ok(())
        }
    }

    fn try_lock_state<'a, T>(
        &self,
        mutex: &'a Mutex<T>,
        name: &'static str,
    ) -> Result<std::sync::MutexGuard<'a, T>, MmioError> {
        mutex.lock().map_err(|_| {
            self.fail_device(&format!("poisoned {name} lock"));
            MmioError::Device
        })
    }

    fn snapshot_lock<'a, T>(
        &self,
        mutex: &'a Mutex<T>,
        name: &'static str,
    ) -> Result<std::sync::MutexGuard<'a, T>, PersistError> {
        mutex.lock().map_err(|_| {
            self.fail_device(&format!("poisoned {name} lock"));
            PersistError::Unavailable(format!("virtio-blk {name} lock is poisoned"))
        })
    }

    fn snapshot_state(&self) -> Result<VirtioBlkMmioState, PersistError> {
        if self.is_failed() {
            return Err(PersistError::Unavailable(
                "virtio-blk is failed and requires reset".into(),
            ));
        }
        Ok(VirtioBlkMmioState {
            status: self.status.load(Ordering::Relaxed),
            queue_sel: self.queue_sel.load(Ordering::Relaxed),
            host_features_sel: self.host_features_sel.load(Ordering::Relaxed),
            guest_features_sel: self.guest_features_sel.load(Ordering::Relaxed),
            guest_features_low: self.guest_features_low.load(Ordering::Relaxed),
            guest_features_high: self.guest_features_high.load(Ordering::Relaxed),
            queues: self.snapshot_lock(&self.queues, "queues")?.clone(),
            activated: self.activated.load(Ordering::Relaxed),
            interrupt_status: self.interrupt_status.load(Ordering::SeqCst),
            processor: self
                .snapshot_lock(&self.processor, "processor")?
                .as_ref()
                .map(VirtQueueProcessor::save_state),
        })
    }

    fn reset(&self) {
        self.queues.clear_poison();
        self.backend.clear_poison();
        self.guest_mem.clear_poison();
        self.host_dirty.clear_poison();
        self.processor.clear_poison();
        self.rate_limiter.clear_poison();
        #[cfg(target_os = "linux")]
        self.irq_evt.clear_poison();
        self.status.store(0, Ordering::SeqCst);
        self.activated.store(false, Ordering::SeqCst);
        self.host_features_sel.store(0, Ordering::SeqCst);
        self.guest_features_sel.store(0, Ordering::SeqCst);
        self.guest_features_low.store(0, Ordering::SeqCst);
        self.guest_features_high.store(0, Ordering::SeqCst);
        self.queue_sel.store(0, Ordering::SeqCst);
        self.interrupt_status.store(0, Ordering::SeqCst);
        for q in self.lock_state(&self.queues, "queues").iter_mut() {
            *q = QueueState::default();
        }
        *self.lock_state(&self.processor, "processor") = None;
    }

    fn lock_state<'a, T>(
        &self,
        mutex: &'a Mutex<T>,
        name: &'static str,
    ) -> std::sync::MutexGuard<'a, T> {
        match mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                self.fail_device(&format!("poisoned {name} lock"));
                poisoned.into_inner()
            }
        }
    }

    /// Create a new virtio-blk MMIO device with a file-backed backend.
    pub fn new(irq: u32, backend: BlkBackend) -> Self {
        #[cfg(feature = "test-failpoints")]
        let delayed_services = backend.test_delayed_services_counter();
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
            #[cfg(feature = "test-failpoints")]
            delayed_services,
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
            #[cfg(feature = "test-failpoints")]
            delayed_services: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
        *self.lock_state(&self.irq_evt, "irq_evt") = Some(evt);
    }

    /// Fire the interrupt again if one is still unacknowledged. A restore
    /// brings back this status register but not the interrupt itself: one
    /// raised just before the snapshot captured the vCPUs exists nowhere
    /// else, and the guest would wait for it forever. The guest reads the
    /// status before acting, so a repeated interrupt is harmless.
    pub fn reassert_pending_interrupt(&self) {
        #[cfg(target_os = "linux")]
        {
            if self.interrupt_status.load(Ordering::SeqCst) == 0 {
                return;
            }
            if let Ok(guard) = self.try_lock_state(&self.irq_evt, "irq_evt") {
                if let Some(evt) = guard.as_ref() {
                    let _ = evt.write(1);
                }
            }
        }
    }

    /// Signal an interrupt to the guest.
    #[cfg(target_os = "linux")]
    fn trigger_interrupt(&self) -> MmioWriteResult {
        self.interrupt_status
            .fetch_or(VIRTIO_MMIO_INT_VRING, Ordering::SeqCst);
        if let Some(evt) = self.try_lock_state(&self.irq_evt, "irq_evt")?.as_ref() {
            let _ = evt.write(1);
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn trigger_interrupt(&self) -> MmioWriteResult {
        self.interrupt_status
            .fetch_or(VIRTIO_MMIO_INT_VRING, Ordering::SeqCst);
        Ok(())
    }

    /// Set the guest memory (called after KvmVm creation).
    pub fn set_guest_memory(&self, mem: std::sync::Arc<GuestMemoryMmap>) {
        *self.lock_state(&self.guest_mem, "guest_mem") = Some(mem);
    }

    pub fn set_guest_dirty_tracker(&self, dirty: SoftwareDirtyBitmap) {
        *self.lock_state(&self.host_dirty, "host_dirty") = Some(dirty);
    }

    /// Install a per-device rate limiter. Leaving it unset keeps I/O unlimited.
    pub fn set_rate_limiter(&self, rl: RateLimiter) {
        *self.lock_state(&self.rate_limiter, "rate_limiter") = Some(rl);
    }

    fn rate_limit_allows(&self, bytes: u64) -> Result<bool, MmioError> {
        Ok(
            match self
                .try_lock_state(&self.rate_limiter, "rate_limiter")?
                .as_mut()
            {
                Some(rl) => rl.try_charge(1, bytes),
                None => true,
            },
        )
    }

    /// Copy only the fixed header, even if the guest combines it with a
    /// multi-megabyte data buffer or splits it across readable descriptors.
    fn read_header(mem: &GuestMemoryMmap, descs: &[(u64, u32)]) -> Option<BlkReqHeader> {
        let mut header = [0u8; BlkReqHeader::SIZE];
        let mut copied = 0;
        for &(addr, len) in descs {
            let take = (len as usize).min(header.len() - copied);
            if take == 0 {
                continue;
            }
            mem.read_slice(&mut header[copied..copied + take], GuestAddress(addr))
                .ok()?;
            copied += take;
            if copied == header.len() {
                return BlkReqHeader::from_bytes(&header);
            }
        }
        None
    }

    /// The bounded descriptor metadata for an OUT payload, excluding its header.
    fn readable_data(descs: &[(u64, u32)]) -> Vec<(u64, u32)> {
        let mut skip = BlkReqHeader::SIZE as u32;
        descs
            .iter()
            .filter_map(|&(addr, len)| {
                let skipped = skip.min(len);
                skip -= skipped;
                (len > skipped).then_some((addr + u64::from(skipped), len - skipped))
            })
            .collect()
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
                dirty.mark_guest_range(addr, take as u64);
            }
            cursor = next_cursor;
        }
        cursor
    }

    /// Transfer data directly between validated guest ranges and the backend.
    /// Vector metadata is bounded by the queue's descriptor count times the
    /// trusted memory-region count; payload bytes are never allocated or copied.
    fn service_guest_data(
        mem: &GuestMemoryMmap,
        dirty: Option<&SoftwareDirtyBitmap>,
        backend: &mut BlkBackend,
        header: &BlkReqHeader,
        data: &[(u64, u32)],
        write: bool,
    ) -> Option<u8> {
        use vm_memory::GuestMemoryBackend;

        let max_iov = data.len().checked_mul(mem.num_regions())?;
        let mut guards = Vec::with_capacity(data.len());
        let mut iov = Vec::with_capacity(data.len());
        for &(addr, len) in data {
            for slice in mem.get_slices(GuestAddress(addr), len as usize) {
                let slice = slice.ok()?;
                if iov.len() == max_iov {
                    return None;
                }
                let guard = slice.ptr_guard_mut();
                iov.push(libc::iovec {
                    iov_base: guard.as_ptr().cast(),
                    iov_len: slice.len(),
                });
                guards.push(guard);
            }
        }
        // SAFETY: these raw pointers cover validated guest RAM from `mem`,
        // kept mapped by the guards. No Rust references to guest data are
        // created; the backend completes I/O before the used ring is published.
        let st = unsafe {
            if write {
                backend.service_write_from(header, &iov)
            } else {
                backend.service_read_into(header, &iov)
            }
        };
        drop(guards);
        if !write {
            if let Some(dirty) = dirty {
                for &(addr, len) in data {
                    dirty.mark_guest_range(addr, u64::from(len));
                }
            }
        }
        Some(st)
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
                    dirty.mark_guest_range(*addr, 1);
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

    /// Process the virtqueue when the guest kicks. Production KVM guests route
    /// QUEUE_NOTIFY to the block I/O worker; direct MMIO callers retain this
    /// entry point as a portable fallback.
    pub fn process_queue(&self, _queue_idx: u32) -> MmioWriteResult {
        self.ensure_operational()?;
        let mem = match self.try_lock_state(&self.guest_mem, "guest_mem")?.clone() {
            Some(m) => m,
            None => {
                log::debug!("BLK process_queue: guest_mem is None");
                return Ok(());
            }
        };
        let dirty = self.try_lock_state(&self.host_dirty, "host_dirty")?.clone();

        let qs = self.try_lock_state(&self.queues, "queues")?;
        let Some(q) = qs.first() else {
            log::error!("BLK process_queue: missing queue 0 in restored state");
            return Ok(());
        };
        if !q.ready || !q.valid_size() {
            log::debug!(
                "BLK process_queue: queue not ready or invalid (ready={} size={})",
                q.ready,
                q.size
            );
            return Ok(());
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
        let mut proc_guard = self.try_lock_state(&self.processor, "processor")?;
        if proc_guard.is_none() {
            *proc_guard = Some(VirtQueueProcessor::new(config));
        } else {
            proc_guard.as_mut().unwrap().update_config(config);
        }
        let event_idx =
            self.guest_features_low.load(Ordering::Relaxed) & VIRTIO_RING_F_EVENT_IDX != 0;
        proc_guard.as_mut().unwrap().set_event_idx(event_idx);
        let old_used = proc_guard.as_ref().unwrap().last_used_idx();

        let mut backend_guard = self.try_lock_state(&self.backend, "backend")?;
        if backend_guard.is_none() {
            return Ok(());
        }
        let backend = backend_guard.as_mut().unwrap();
        let mut processing_failed = false;

        // Process available requests until the queue is drained or the limiter
        // asks us to defer the next descriptor chain.
        let _count = proc_guard.as_mut().unwrap().process_queue_chains_dirty(
            &mem,
            dirty.as_ref(),
            |chain| {
                let (readable, writable) = (chain.readable, chain.writable);
                let header = Self::read_header(&mem, readable);
                let readable_len = readable
                    .iter()
                    .try_fold(0usize, |total, (_, len)| total.checked_add(*len as usize));
                let request_bytes = header
                    .as_ref()
                    .and_then(|header| Self::request_bytes(header, readable_len?, writable));
                let negotiated_size_max =
                    self.guest_features_low.load(Ordering::Relaxed) & VIRTIO_BLK_F_SIZE_MAX != 0;
                let invalid_size = negotiated_size_max
                    && readable
                        .iter()
                        .chain(writable)
                        .any(|&(_, len)| len > crate::virtio::vqueue::MAX_DESC_LEN);
                let streamable = header
                    .as_ref()
                    .is_some_and(|header| matches!(header.req_type, req_type::IN | req_type::OUT));
                let invalid_used_len = header
                    .as_ref()
                    .is_some_and(|header| header.req_type == req_type::IN)
                    && request_bytes.is_some_and(|len| len >= u64::from(u32::MAX));
                if request_bytes.is_none()
                    || invalid_size
                    || invalid_used_len
                    || (chain.over_limit && !streamable)
                {
                    match self.rate_limit_allows(0) {
                        Ok(true) => {}
                        Ok(false) => return None,
                        Err(_) => {
                            processing_failed = true;
                            return None;
                        }
                    }
                    return Some(Self::write_status(
                        &mem,
                        dirty.as_ref(),
                        writable,
                        status::IO_ERR,
                    ));
                }
                let header = header.unwrap();
                let request_bytes = request_bytes.unwrap();
                match self.rate_limit_allows(request_bytes) {
                    Ok(true) => {}
                    Ok(false) => return None,
                    Err(_) => {
                        processing_failed = true;
                        return None;
                    }
                }

                // For IN (read): the writable buffer is the data buffer + status byte.
                // For OUT (write): the readable data after the header is the data.
                // For FLUSH: no data, just status.
                let mut data_written = 0;
                let st = match header.req_type {
                    req_type::IN => {
                        if request_bytes == 0 {
                            status::IO_ERR
                        } else {
                            let data = &writable[..writable.len().saturating_sub(1)];
                            let st = Self::service_guest_data(
                                &mem,
                                dirty.as_ref(),
                                backend,
                                &header,
                                data,
                                false,
                            )
                            .unwrap_or(status::IO_ERR);
                            if st == status::OK {
                                data_written = request_bytes as u32;
                            }
                            st
                        }
                    }
                    req_type::OUT => {
                        let data = Self::readable_data(readable);
                        Self::service_guest_data(
                            &mem,
                            dirty.as_ref(),
                            backend,
                            &header,
                            &data,
                            true,
                        )
                        .unwrap_or(status::IO_ERR)
                    }
                    req_type::FLUSH => backend.service(&header, &mut []),
                    req_type::GET_ID => {
                        let mut id_buf = [0u8; 20];
                        let st = backend.service(&header, &mut id_buf);
                        data_written =
                            Self::write_writable_data(&mem, dirty.as_ref(), writable, &id_buf)
                                as u32;
                        st
                    }
                    _ => status::UNSUPP,
                };

                Some(data_written + Self::write_status(&mem, dirty.as_ref(), writable, st))
            },
        );

        // If we processed any requests, signal an interrupt to the guest.
        if _count > 0 {
            log::debug!("BLK process_queue: processed {_count} request(s)");
            if proc_guard
                .as_ref()
                .unwrap()
                .needs_notification(&mem, old_used)
            {
                self.trigger_interrupt()?;
            }
        }
        if processing_failed || self.is_failed() {
            Err(MmioError::Device)
        } else {
            Ok(())
        }
    }
}

impl Persist for VirtioBlkMmio {
    type State = VirtioBlkMmioState;

    fn save(&self) -> Self::State {
        self.snapshot_state()
            .expect("virtio-blk state is unavailable")
    }

    fn try_save(&self) -> Result<Self::State, PersistError> {
        self.snapshot_state()
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
        *self.lock_state(&self.queues, "queues") = state.queues;
        self.activated.store(state.activated, Ordering::Relaxed);
        self.interrupt_status
            .store(state.interrupt_status, Ordering::SeqCst);
        let config = self
            .lock_state(&self.queues, "queues")
            .first()
            .map(Self::queue_config_from_state)
            .unwrap_or_default();
        *self.lock_state(&self.processor, "processor") = state
            .processor
            .map(|p| VirtQueueProcessor::from_state(config, p));
    }
}

impl Persist for std::sync::Arc<VirtioBlkMmio> {
    type State = VirtioBlkMmioState;

    fn save(&self) -> Self::State {
        self.as_ref().save()
    }

    fn try_save(&self) -> Result<Self::State, PersistError> {
        self.as_ref().try_save()
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
        *self.lock_state(&self.queues, "queues") = state.queues;
        self.activated.store(state.activated, Ordering::Relaxed);
        self.interrupt_status
            .store(state.interrupt_status, Ordering::SeqCst);
        let config = self
            .lock_state(&self.queues, "queues")
            .first()
            .map(VirtioBlkMmio::queue_config_from_state)
            .unwrap_or_default();
        *self.lock_state(&self.processor, "processor") = state
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
                    0 => {
                        let backend = self.lock_state(&self.backend, "backend");
                        if backend.as_ref().is_some_and(|b| b.read_only) {
                            BLK_FEATURES_LOW | VIRTIO_BLK_F_RO
                        } else {
                            BLK_FEATURES_LOW
                        }
                    }
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
                let qs = self.lock_state(&self.queues, "queues");
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
                let backend = self.lock_state(&self.backend, "backend");
                match &*backend {
                    Some(b) => {
                        // virtio-blk config space (virtio 1.x §5.2.4):
                        // [capacity u64][size_max u32]. Fields for features
                        // we do not offer (seg_max, geometry, blk_size, ...)
                        // read as zero.
                        let mut cfg = [0u8; 12];
                        cfg[..8].copy_from_slice(&b.sectors.to_le_bytes());
                        cfg[8..12]
                            .copy_from_slice(&crate::virtio::vqueue::MAX_DESC_LEN.to_le_bytes());
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
                let qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].size as u32
                } else {
                    0
                }
            }
            reg::QUEUE_DESC_LOW => {
                let qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    (qs[sel].desc_table_addr & 0xFFFFFFFF) as u32
                } else {
                    0
                }
            }
            reg::QUEUE_DESC_HIGH => {
                let qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    (qs[sel].desc_table_addr >> 32) as u32
                } else {
                    0
                }
            }
            reg::QUEUE_DRIVER_LOW => {
                let qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    (qs[sel].avail_ring_addr & 0xFFFFFFFF) as u32
                } else {
                    0
                }
            }
            reg::QUEUE_DRIVER_HIGH => {
                let qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    (qs[sel].avail_ring_addr >> 32) as u32
                } else {
                    0
                }
            }
            reg::QUEUE_DEVICE_LOW => {
                let qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    (qs[sel].used_ring_addr & 0xFFFFFFFF) as u32
                } else {
                    0
                }
            }
            reg::QUEUE_DEVICE_HIGH => {
                let qs = self.lock_state(&self.queues, "queues");
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
        if self.is_failed() && !(off == reg::STATUS && val == 0) {
            return Err(MmioError::Device);
        }
        match off {
            reg::STATUS => {
                self.status_writes.fetch_add(1, Ordering::Relaxed);
                if val == 0 {
                    self.reset();
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
                let mut qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].set_size(val);
                }
            }
            reg::QUEUE_DESC_LOW => {
                let mut qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].desc_table_addr = (qs[sel].desc_table_addr & !0xFFFFFFFF) | val as u64;
                }
            }
            reg::QUEUE_DESC_HIGH => {
                let mut qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].desc_table_addr =
                        (qs[sel].desc_table_addr & 0xFFFFFFFF) | (val as u64) << 32;
                }
            }
            reg::QUEUE_DRIVER_LOW => {
                let mut qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].avail_ring_addr = (qs[sel].avail_ring_addr & !0xFFFFFFFF) | val as u64;
                }
            }
            reg::QUEUE_DRIVER_HIGH => {
                let mut qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].avail_ring_addr =
                        (qs[sel].avail_ring_addr & 0xFFFFFFFF) | (val as u64) << 32;
                }
            }
            reg::QUEUE_DEVICE_LOW => {
                let mut qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].used_ring_addr = (qs[sel].used_ring_addr & !0xFFFFFFFF) | val as u64;
                }
            }
            reg::QUEUE_DEVICE_HIGH => {
                let mut qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].used_ring_addr =
                        (qs[sel].used_ring_addr & 0xFFFFFFFF) | (val as u64) << 32;
                }
            }
            reg::QUEUE_READY => {
                let mut qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < qs.len() {
                    qs[sel].set_ready(val != 0);
                    log::debug!("QUEUE_READY: qs[{sel}].ready = {}", qs[sel].ready);
                }
            }
            reg::QUEUE_NOTIFY => {
                // THE CRITICAL PATH: guest kicked the queue — process I/O.
                self.notify_count.fetch_add(1, Ordering::Relaxed);
                return self.process_queue(val);
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

    #[test]
    fn poisoned_guest_mem_lock_marks_device_failed_without_panicking() {
        let (backend, path) = new_test_backend("poisoned-guest-mem");
        let dev = Arc::new(VirtioBlkMmio::new(5, backend));
        let poisoned = Arc::clone(&dev);
        assert!(std::thread::spawn(move || {
            let _guard = poisoned.guest_mem.lock().unwrap();
            panic!("poison guest_mem");
        })
        .join()
        .is_err());

        dev.set_guest_memory(Arc::new(
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20_000)]).unwrap(),
        ));
        assert_ne!(dev.current_status() & status_bits::FAILED, 0);
        assert!(dev.process_queue(0).is_err());
        assert!(Persist::try_save(dev.as_ref()).is_err());
        assert!(dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).is_err());

        dev.mmio_write(reg::STATUS, 0, 4).unwrap();
        assert_eq!(dev.current_status(), 0);
        assert!(Persist::try_save(dev.as_ref()).is_ok());

        let _ = std::fs::remove_file(path);
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

        dev.process_queue(0).unwrap();

        assert_eq!(used_idx(&mem), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(feature = "test-failpoints")]
    #[test]
    fn delayed_service_counters_are_isolated_per_device() {
        let (backend_a, path_a) = new_test_backend("blk-delay-a");
        let (backend_b, path_b) = new_test_backend("blk-delay-b");
        let mem_a = new_test_mem();
        let mem_b = new_test_mem();
        let dev_a = Arc::new(VirtioBlkMmio::new(5, backend_a));
        let dev_b = VirtioBlkMmio::new(6, backend_b);
        configure_test_blk_queue(&dev_a, mem_a.clone());
        configure_test_blk_queue(&dev_b, mem_b.clone());
        setup_blk_out_requests(&mem_a, 1);
        setup_blk_out_requests(&mem_b, 1);
        dev_a
            .set_test_service_delay(std::time::Duration::from_millis(100))
            .unwrap();

        let delayed = Arc::clone(&dev_a);
        let worker = std::thread::spawn(move || delayed.process_queue(0));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while dev_a.test_delayed_services() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "delayed request did not reach its device-local failpoint"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(dev_a.test_delayed_services(), 1);
        assert_eq!(dev_b.test_delayed_services(), 0);

        worker.join().unwrap().unwrap();
        assert_eq!(dev_a.test_delayed_services(), 0);
        assert_eq!(dev_b.test_delayed_services(), 0);
        dev_b.process_queue(0).unwrap();
        std::fs::remove_file(path_a).unwrap();
        std::fs::remove_file(path_b).unwrap();
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

        dev.process_queue(0).unwrap();
        assert_eq!(used_idx(&mem), 1);

        dev.process_queue(0).unwrap();
        assert_eq!(used_idx(&mem), 1);

        clock.advance(1_000_000_000);
        dev.process_queue(0).unwrap();
        assert_eq!(used_idx(&mem), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn size_max_matches_the_descriptor_limit_at_its_spec_offset() {
        let (backend, path) = new_test_backend("blk-size-max");
        let dev = VirtioBlkMmio::new(5, backend);
        dev.mmio_write(reg::HOST_FEATURES_SEL, 0, 4).unwrap();
        let features = dev.mmio_read(reg::HOST_FEATURES, 4).unwrap();
        assert_ne!(features & u64::from(VIRTIO_BLK_F_SIZE_MAX), 0);
        assert_eq!(
            dev.mmio_read(reg::CONFIG + 8, 4).unwrap(),
            u64::from(crate::virtio::vqueue::MAX_DESC_LEN)
        );
        // 4096-byte test disk = 8 sectors.
        assert_eq!(dev.mmio_read(reg::CONFIG, 4).unwrap(), 8);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn restored_legacy_large_io_transfers_payload_and_charges_full_request() {
        use crate::virtio::vqueue::{UsedElem, MAX_DESC_LEN};

        const DATA: u64 = 0x20_0000;
        // Single-segment I/O observed before SIZE_MAX, and a chain whose
        // individual segments fit but whose total exceeds the chain limit.
        for (segments, segment_len) in [(1u16, 1_220_608u32), (4, MAX_DESC_LEN)] {
            for request in [req_type::IN, req_type::OUT] {
                for event_idx in [false, true] {
                    let payload_len = segments as usize * segment_len as usize;
                    let expected: Vec<u8> = (0..payload_len).map(|i| (i / 4096) as u8).collect();
                    let (_, path) = new_test_backend("blk-legacy-large-io");
                    std::fs::write(
                        &path,
                        if request == req_type::IN {
                            expected.clone()
                        } else {
                            vec![0x33; payload_len]
                        },
                    )
                    .unwrap();
                    let backend = BlkBackend::open(&path, false).unwrap();
                    let mem = Arc::new(
                        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 8 * 1024 * 1024)])
                            .unwrap(),
                    );
                    let old = VirtioBlkMmio::new(5, backend);
                    configure_test_blk_queue(&old, mem.clone());
                    old.mmio_write(reg::GUEST_FEATURES_SEL, 0, 4).unwrap();
                    old.mmio_write(
                        reg::GUEST_FEATURES,
                        u64::from(
                            (1 << 9)
                                | if event_idx {
                                    VIRTIO_RING_F_EVENT_IDX
                                } else {
                                    0
                                },
                        ),
                        4,
                    )
                    .unwrap();
                    old.mmio_write(reg::GUEST_FEATURES_SEL, 1, 4).unwrap();
                    old.mmio_write(reg::GUEST_FEATURES, 1, 4).unwrap();
                    let state = old.save();
                    assert_eq!(state.guest_features_low & VIRTIO_BLK_F_SIZE_MAX, 0);
                    drop(old);

                    let mut restored =
                        VirtioBlkMmio::new(5, BlkBackend::open(&path, false).unwrap());
                    restored.restore(state);
                    restored.set_guest_memory(mem.clone());
                    let dirty = SoftwareDirtyBitmap::new();
                    restored.set_guest_dirty_tracker(dirty.clone());
                    let clock = Arc::new(ManualClock::new(0));
                    restored.set_rate_limiter(RateLimiter::new_with_clock(
                        1,
                        payload_len as u64 / 2,
                        clock.clone(),
                    ));

                    mem.write_obj(
                        BlkReqHeader {
                            req_type: request,
                            reserved: 0,
                            sector: 0,
                        },
                        GuestAddress(TEST_HEADER_BASE),
                    )
                    .unwrap();
                    let data = if request == req_type::OUT {
                        expected.clone()
                    } else {
                        vec![0x5a; payload_len]
                    };
                    mem.write_slice(&data, GuestAddress(DATA)).unwrap();
                    let mut descriptors = vec![Descriptor {
                        addr: TEST_HEADER_BASE,
                        len: BlkReqHeader::SIZE as u32,
                        flags: desc_flags::NEXT,
                        next: 1,
                    }];
                    for i in 0..segments {
                        descriptors.push(Descriptor {
                            addr: DATA + u64::from(i) * u64::from(segment_len),
                            len: segment_len,
                            flags: desc_flags::NEXT
                                | if request == req_type::IN {
                                    desc_flags::WRITE
                                } else {
                                    0
                                },
                            next: i + 2,
                        });
                    }
                    descriptors.push(Descriptor {
                        addr: TEST_STATUS_BASE,
                        len: 1,
                        flags: desc_flags::WRITE,
                        next: 0,
                    });
                    for (i, descriptor) in descriptors.iter().enumerate() {
                        mem.write_obj(*descriptor, GuestAddress(TEST_DESC + i as u64 * 16))
                            .unwrap();
                    }
                    mem.write_obj(0xffu8, GuestAddress(TEST_STATUS_BASE))
                        .unwrap();
                    mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(TEST_AVAIL))
                        .unwrap();
                    mem.write_obj(0u16, GuestAddress(TEST_AVAIL + 4)).unwrap();

                    restored.process_queue(0).unwrap();
                    assert_eq!(used_idx(&mem), 1, "legacy I/O must complete");
                    assert_eq!(
                        mem.read_obj::<u8>(GuestAddress(TEST_STATUS_BASE)).unwrap(),
                        status::OK
                    );
                    let used: UsedElem = mem.read_obj(GuestAddress(TEST_USED + 4)).unwrap();
                    assert_eq!(
                        (used.id, used.len),
                        (
                            0,
                            if request == req_type::IN {
                                payload_len as u32 + 1
                            } else {
                                1
                            }
                        )
                    );
                    assert_ne!(restored.mmio_read(reg::INTERRUPT_STATUS, 4).unwrap(), 0);
                    let mut after = vec![0; payload_len];
                    mem.read_slice(&mut after, GuestAddress(DATA)).unwrap();
                    assert_eq!(
                        after, expected,
                        "guest payload must match the completed I/O"
                    );
                    assert_eq!(std::fs::read(&path).unwrap(), expected, "disk payload");
                    let changed = dirty.drain();
                    assert!(changed.contains(TEST_STATUS_BASE));
                    assert!(changed.contains(TEST_USED));
                    assert_eq!(changed.contains(DATA), request == req_type::IN);
                    assert_eq!(
                        changed.contains(DATA + payload_len as u64 - 1),
                        request == req_type::IN
                    );
                    if event_idx {
                        let avail_event = TEST_USED + 4 + u64::from(TEST_QUEUE_SIZE) * 8;
                        assert_eq!(mem.read_obj::<u16>(GuestAddress(avail_event)).unwrap(), 1);
                    }

                    // A request larger than the bucket borrows its full byte
                    // cost. One second restores the IOPS token, but leaves
                    // no bytes for the next request; two seconds permit it.
                    setup_blk_out_requests(&mem, 1);
                    mem.write_obj(2u16, GuestAddress(TEST_AVAIL + 2)).unwrap();
                    mem.write_obj(0u16, GuestAddress(TEST_AVAIL + 6)).unwrap();
                    restored.process_queue(0).unwrap();
                    assert_eq!(
                        restored
                            .processor
                            .lock()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .used_idx(),
                        1
                    );
                    clock.advance(1_000_000_000);
                    restored.process_queue(0).unwrap();
                    assert_eq!(
                        restored
                            .processor
                            .lock()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .used_idx(),
                        1,
                        "refilled IOPS must not bypass the large request's byte debt"
                    );
                    clock.advance(1_000_000_000);
                    restored.process_queue(0).unwrap();
                    assert_eq!(used_idx(&mem), 2);
                    assert_eq!(
                        mem.read_obj::<u8>(GuestAddress(TEST_STATUS_BASE)).unwrap(),
                        status::OK
                    );
                    drop(restored);
                    std::fs::remove_file(path).unwrap();
                }
            }
        }
    }

    #[test]
    fn split_header_large_write_honors_negotiated_size_max() {
        const DATA: u64 = 0x20_0000;
        let payload = vec![0xa7; 1_220_608];
        for negotiated in [false, true] {
            let mut disk = tempfile::NamedTempFile::new().unwrap();
            std::io::Write::write_all(&mut disk, &vec![0x11; payload.len()]).unwrap();
            let backend = BlkBackend::open(disk.path(), false).unwrap();
            let dev = VirtioBlkMmio::new(5, backend);
            let mem = Arc::new(
                GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4 * 1024 * 1024)]).unwrap(),
            );
            configure_test_blk_queue(&dev, mem.clone());
            dev.mmio_write(
                reg::GUEST_FEATURES,
                if negotiated {
                    u64::from(VIRTIO_BLK_F_SIZE_MAX)
                } else {
                    0
                },
                4,
            )
            .unwrap();
            mem.write_obj(
                BlkReqHeader {
                    req_type: req_type::OUT,
                    reserved: 0,
                    sector: 0,
                },
                GuestAddress(TEST_HEADER_BASE),
            )
            .unwrap();
            // Split the header in two; its second half shares the large
            // payload descriptor. Only those eight bytes may be copied.
            mem.write_obj(0u64, GuestAddress(DATA - 8)).unwrap();
            mem.write_slice(&payload, GuestAddress(DATA)).unwrap();
            for (i, descriptor) in [
                Descriptor {
                    addr: TEST_HEADER_BASE,
                    len: 8,
                    flags: desc_flags::NEXT,
                    next: 1,
                },
                Descriptor {
                    addr: DATA - 8,
                    len: payload.len() as u32 + 8,
                    flags: desc_flags::NEXT,
                    next: 2,
                },
                Descriptor {
                    addr: TEST_STATUS_BASE,
                    len: 1,
                    flags: desc_flags::WRITE,
                    next: 0,
                },
            ]
            .iter()
            .enumerate()
            {
                mem.write_obj(*descriptor, GuestAddress(TEST_DESC + i as u64 * 16))
                    .unwrap();
            }
            mem.write_obj(0xffu8, GuestAddress(TEST_STATUS_BASE))
                .unwrap();
            mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(TEST_AVAIL))
                .unwrap();
            mem.write_obj(0u16, GuestAddress(TEST_AVAIL + 4)).unwrap();
            dev.process_queue(0).unwrap();
            assert_eq!(used_idx(&mem), 1);
            assert_eq!(
                mem.read_obj::<u8>(GuestAddress(TEST_STATUS_BASE)).unwrap(),
                if negotiated {
                    status::IO_ERR
                } else {
                    status::OK
                }
            );
            assert_eq!(
                std::fs::read(disk.path()).unwrap(),
                if negotiated {
                    vec![0x11; payload.len()]
                } else {
                    payload.clone()
                }
            );
        }
    }

    #[test]
    fn read_only_backends_offer_the_ro_feature() {
        let (backend, path) = new_test_backend("blk-ro-feature");
        drop(backend);
        let host_features_low = |read_only| {
            let dev = VirtioBlkMmio::new(5, BlkBackend::open(&path, read_only).unwrap());
            dev.mmio_write(reg::HOST_FEATURES_SEL, 0, 4).unwrap();
            dev.mmio_read(reg::HOST_FEATURES, 4).unwrap()
        };
        assert_eq!(host_features_low(false), u64::from(BLK_FEATURES_LOW));
        assert_eq!(
            host_features_low(true),
            u64::from(BLK_FEATURES_LOW | VIRTIO_BLK_F_RO)
        );
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

        dev.process_queue(0).unwrap();
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

        restored.process_queue(0).unwrap();
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
        dev.trigger_interrupt().unwrap();

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

    #[cfg(target_os = "linux")]
    #[test]
    fn restored_pending_interrupt_is_raised_again() {
        // A completion the guest has not acknowledged survives in the saved
        // status register only.
        let source = VirtioBlkMmio::new_stub(5, 2);
        source
            .interrupt_status
            .store(VIRTIO_MMIO_INT_VRING, Ordering::SeqCst);
        let state = source.try_save().expect("save");

        let mut restored = VirtioBlkMmio::new_stub(5, 2);
        let evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).expect("evt");
        restored.set_irq_evt(evt.try_clone().expect("clone evt"));
        restored.restore(state);
        assert!(evt.read().is_err(), "restore alone raises nothing");
        restored.reassert_pending_interrupt();
        assert_eq!(evt.read().expect("interrupt raised"), 1);
        assert_eq!(
            restored.mmio_read(reg::INTERRUPT_STATUS, 4).unwrap(),
            u64::from(VIRTIO_MMIO_INT_VRING)
        );

        // Once the guest acknowledges it, nothing is raised again.
        restored
            .mmio_write(reg::INTERRUPT_ACK, u64::from(VIRTIO_MMIO_INT_VRING), 4)
            .unwrap();
        restored.reassert_pending_interrupt();
        assert!(evt.read().is_err());
    }
}
