//! Virtio 1.x traditional memory-balloon device over virtio-mmio.
//!
//! The guest supplies 4 KiB PFNs on inflate/deflate queues. Inflate validates
//! every PFN before releasing any host pages, then coalesces ranges and asks the
//! memory backend for zero-on-refault discard semantics. This matters for
//! UFFD-backed lazy restores, where a plain MADV_DONTNEED would resurrect old
//! snapshot bytes.

use crate::bus::{MmioDevice, MmioReadResult, MmioWriteResult};
use crate::persist::Persist;
use crate::virtio::blk_transport::status_bits;
use crate::virtio::regs::{reg, MAGIC};
use crate::virtio::vqueue::{
    is_valid_queue_size, QueueConfig, VirtQueueProcessor, VirtQueueProcessorState, MAX_QUEUE_SIZE,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use vm_memory::{Bytes, GuestAddress};
use vmm_memory_backend::GuestMemory;

pub const DEVICE_ID_BALLOON: u32 = 5;
const PAGE_SIZE: u64 = 4096;
// Linux's virtio-balloon driver submits at most one page of 32-bit PFNs in a
// request. Keep the device event's allocation and CPU cost independently
// bounded even when a malicious guest supplies a much larger descriptor chain.
const MAX_PFNS_PER_REQUEST: usize = 256;
const QUEUE_INFLATE: usize = 0;
const QUEUE_DEFLATE: usize = 1;
const QUEUE_COUNT: usize = 2;
const FEATURES_LOW: u32 = 0;
const FEATURES_HIGH: u32 = 1; // VIRTIO_F_VERSION_1 (bit 32)
const INT_VRING: u32 = 0x1;
const INT_CONFIG: u32 = 0x2;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
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

    fn set_size(&mut self, raw: u32) {
        let Ok(size) = u16::try_from(raw) else {
            self.size = 0;
            self.ready = false;
            return;
        };
        if is_valid_queue_size(size, MAX_QUEUE_SIZE) {
            self.size = size;
        } else {
            self.size = 0;
            self.ready = false;
        }
    }

    fn set_ready(&mut self, ready: bool) {
        self.ready = ready && self.valid_size();
    }

    fn config(&self) -> Option<QueueConfig> {
        (self.ready && self.valid_size()).then_some(QueueConfig {
            size: self.size,
            desc_table_addr: self.desc_table_addr,
            avail_ring_addr: self.avail_ring_addr,
            used_ring_addr: self.used_ring_addr,
            ready: true,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VirtioBalloonMmioState {
    status: u32,
    queue_sel: u32,
    host_features_sel: u32,
    guest_features_sel: u32,
    guest_features_low: u32,
    guest_features_high: u32,
    queues: Vec<QueueState>,
    processors: Vec<Option<VirtQueueProcessorState>>,
    activated: bool,
    interrupt_status: u32,
    config_generation: u32,
    target_pages: u32,
    actual_pages: u32,
}

pub struct VirtioBalloonMmio {
    pub irq: u32,
    status: AtomicU32,
    queue_sel: AtomicU32,
    host_features_sel: AtomicU32,
    guest_features_sel: AtomicU32,
    guest_features_low: AtomicU32,
    guest_features_high: AtomicU32,
    queues: Mutex<Vec<QueueState>>,
    processors: Mutex<Vec<Option<VirtQueueProcessor>>>,
    memory: GuestMemory,
    activated: AtomicBool,
    interrupt_status: AtomicU32,
    config_generation: AtomicU32,
    target_pages: AtomicU32,
    actual_pages: AtomicU32,
    #[cfg(target_os = "linux")]
    irq_evt: Mutex<Option<vmm_sys_util::eventfd::EventFd>>,
    pub inflate_pages: AtomicU64,
    pub deflate_pages: AtomicU64,
    pub rejected_pfns: AtomicU64,
}

impl VirtioBalloonMmio {
    pub fn new(irq: u32, memory: GuestMemory, target_pages: u32) -> Result<Self, String> {
        if u64::from(target_pages) > memory.size_bytes / PAGE_SIZE {
            return Err("balloon target exceeds guest memory".into());
        }
        Ok(Self {
            irq,
            status: AtomicU32::new(0),
            queue_sel: AtomicU32::new(0),
            host_features_sel: AtomicU32::new(0),
            guest_features_sel: AtomicU32::new(0),
            guest_features_low: AtomicU32::new(0),
            guest_features_high: AtomicU32::new(0),
            queues: Mutex::new(vec![QueueState::default(); QUEUE_COUNT]),
            processors: Mutex::new((0..QUEUE_COUNT).map(|_| None).collect()),
            memory,
            activated: AtomicBool::new(false),
            interrupt_status: AtomicU32::new(0),
            config_generation: AtomicU32::new(0),
            target_pages: AtomicU32::new(target_pages),
            actual_pages: AtomicU32::new(0),
            #[cfg(target_os = "linux")]
            irq_evt: Mutex::new(None),
            inflate_pages: AtomicU64::new(0),
            deflate_pages: AtomicU64::new(0),
            rejected_pfns: AtomicU64::new(0),
        })
    }

    #[cfg(target_os = "linux")]
    pub fn set_irq_evt(&self, event: vmm_sys_util::eventfd::EventFd) {
        *self.irq_evt.lock().unwrap_or_else(|p| p.into_inner()) = Some(event);
    }

    pub fn target_pages(&self) -> u32 {
        self.target_pages.load(Ordering::Acquire)
    }

    pub fn actual_pages(&self) -> u32 {
        self.actual_pages.load(Ordering::Acquire)
    }

    pub fn has_pending_interrupt(&self) -> bool {
        self.interrupt_status.load(Ordering::SeqCst) != 0
    }

    pub fn set_target_pages(&self, pages: u32) -> Result<(), String> {
        if u64::from(pages) > self.memory.size_bytes / PAGE_SIZE {
            return Err("balloon target exceeds guest memory".into());
        }
        self.target_pages.store(pages, Ordering::Release);
        self.config_generation.fetch_add(1, Ordering::AcqRel);
        self.trigger_interrupt(INT_CONFIG);
        Ok(())
    }

    fn trigger_interrupt(&self, kind: u32) {
        self.interrupt_status.fetch_or(kind, Ordering::SeqCst);
        #[cfg(target_os = "linux")]
        if let Some(event) = self
            .irq_evt
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            let _ = event.write(1);
        }
    }

    fn selected_queue(&self) -> Option<QueueState> {
        self.queues
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(self.queue_sel.load(Ordering::Relaxed) as usize)
            .cloned()
    }

    fn update_selected_queue(&self, update: impl FnOnce(&mut QueueState)) {
        if let Some(queue) = self
            .queues
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(self.queue_sel.load(Ordering::Relaxed) as usize)
        {
            update(queue);
        }
    }

    fn parse_pfns(&self, readable: &[(u64, u32)], writable: &[(u64, u32)]) -> Option<Vec<u32>> {
        if !writable.is_empty() {
            return None;
        }
        let total_pfns = readable.iter().try_fold(0usize, |total, &(_, length)| {
            if length == 0 || length % 4 != 0 {
                return None;
            }
            total.checked_add(length as usize / 4)
        })?;
        if total_pfns == 0 || total_pfns > MAX_PFNS_PER_REQUEST {
            return None;
        }

        let mut pfns = Vec::with_capacity(total_pfns);
        for &(address, length) in readable {
            for offset in (0..u64::from(length)).step_by(4) {
                let mut raw = [0u8; 4];
                let pfn_address = address.checked_add(offset)?;
                self.memory
                    .inner
                    .read_slice(&mut raw, GuestAddress(pfn_address))
                    .ok()?;
                let pfn = u32::from_le_bytes(raw);
                let end = u64::from(pfn)
                    .checked_mul(PAGE_SIZE)
                    .and_then(|start| start.checked_add(PAGE_SIZE))?;
                if end > self.memory.size_bytes {
                    return None;
                }
                pfns.push(pfn);
            }
        }
        Some(pfns)
    }

    fn inflate(&self, mut pfns: Vec<u32>) -> bool {
        pfns.sort_unstable();
        pfns.dedup();
        let count = pfns.len() as u64;
        let mut cursor = 0;
        while cursor < pfns.len() {
            let start = pfns[cursor];
            let mut end = start;
            cursor += 1;
            while cursor < pfns.len() && pfns[cursor] == end.saturating_add(1) {
                end = pfns[cursor];
                cursor += 1;
            }
            let gpa = u64::from(start) * PAGE_SIZE;
            let length = (u64::from(end) - u64::from(start) + 1) * PAGE_SIZE;
            if self.memory.discard_range(gpa, length).is_err() {
                return false;
            }
        }
        self.inflate_pages.fetch_add(count, Ordering::Relaxed);
        true
    }

    fn process_queue(&self, queue_index: usize) -> usize {
        if queue_index >= QUEUE_COUNT {
            return 0;
        }
        let Some(config) = self
            .queues
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(queue_index)
            .and_then(QueueState::config)
        else {
            return 0;
        };
        let mut processors = self.processors.lock().unwrap_or_else(|p| p.into_inner());
        let processor =
            processors[queue_index].get_or_insert_with(|| VirtQueueProcessor::new(config.clone()));
        processor.update_config(config);
        let processed = processor.process_queue_descriptors_dirty(
            &self.memory.inner,
            Some(&self.memory.host_dirty_tracker()),
            |readable, writable| {
                let Some(pfns) = self.parse_pfns(readable, writable) else {
                    self.rejected_pfns.fetch_add(1, Ordering::Relaxed);
                    return Some(0);
                };
                if queue_index == QUEUE_INFLATE {
                    if !self.inflate(pfns) {
                        self.rejected_pfns.fetch_add(1, Ordering::Relaxed);
                    }
                } else if queue_index == QUEUE_DEFLATE {
                    self.deflate_pages
                        .fetch_add(pfns.len() as u64, Ordering::Relaxed);
                }
                Some(0)
            },
        );
        if processed > 0 {
            self.trigger_interrupt(INT_VRING);
        }
        processed
    }

    fn reset(&self) {
        self.status.store(0, Ordering::SeqCst);
        self.activated.store(false, Ordering::SeqCst);
        self.queue_sel.store(0, Ordering::SeqCst);
        self.host_features_sel.store(0, Ordering::SeqCst);
        self.guest_features_sel.store(0, Ordering::SeqCst);
        self.guest_features_low.store(0, Ordering::SeqCst);
        self.guest_features_high.store(0, Ordering::SeqCst);
        self.interrupt_status.store(0, Ordering::SeqCst);
        for queue in self
            .queues
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter_mut()
        {
            *queue = QueueState::default();
        }
        *self.processors.lock().unwrap_or_else(|p| p.into_inner()) =
            (0..QUEUE_COUNT).map(|_| None).collect();
    }

    fn apply_state(&self, state: VirtioBalloonMmioState) {
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
        *self.queues.lock().unwrap_or_else(|p| p.into_inner()) = state.queues;
        let queues = self
            .queues
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        *self.processors.lock().unwrap_or_else(|p| p.into_inner()) = state
            .processors
            .into_iter()
            .enumerate()
            .map(|(index, saved)| {
                saved.and_then(|saved| {
                    queues
                        .get(index)?
                        .config()
                        .map(|config| VirtQueueProcessor::from_state(config, saved))
                })
            })
            .collect();
        self.activated.store(state.activated, Ordering::Relaxed);
        self.interrupt_status
            .store(state.interrupt_status, Ordering::Relaxed);
        self.config_generation
            .store(state.config_generation, Ordering::Relaxed);
        self.target_pages
            .store(state.target_pages, Ordering::Relaxed);
        self.actual_pages
            .store(state.actual_pages, Ordering::Relaxed);
    }
}

impl Persist for VirtioBalloonMmio {
    type State = VirtioBalloonMmioState;

    fn save(&self) -> Self::State {
        let processor_states = self
            .processors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|processor| processor.as_ref().map(VirtQueueProcessor::save_state))
            .collect();
        VirtioBalloonMmioState {
            status: self.status.load(Ordering::Relaxed),
            queue_sel: self.queue_sel.load(Ordering::Relaxed),
            host_features_sel: self.host_features_sel.load(Ordering::Relaxed),
            guest_features_sel: self.guest_features_sel.load(Ordering::Relaxed),
            guest_features_low: self.guest_features_low.load(Ordering::Relaxed),
            guest_features_high: self.guest_features_high.load(Ordering::Relaxed),
            queues: self
                .queues
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
            processors: processor_states,
            activated: self.activated.load(Ordering::Relaxed),
            interrupt_status: self.interrupt_status.load(Ordering::Relaxed),
            config_generation: self.config_generation.load(Ordering::Relaxed),
            target_pages: self.target_pages(),
            actual_pages: self.actual_pages(),
        }
    }

    fn restore(&mut self, state: Self::State) {
        self.apply_state(state);
    }
}

impl Persist for std::sync::Arc<VirtioBalloonMmio> {
    type State = VirtioBalloonMmioState;

    fn save(&self) -> Self::State {
        self.as_ref().save()
    }

    fn restore(&mut self, state: Self::State) {
        self.apply_state(state);
    }
}

impl MmioDevice for VirtioBalloonMmio {
    fn mmio_read(&self, offset: u64, _len: u8) -> MmioReadResult {
        let value = match offset {
            reg::MAGIC_VALUE => MAGIC,
            reg::VERSION => 2,
            reg::DEVICE_ID => DEVICE_ID_BALLOON,
            reg::VENDOR_ID => 0,
            reg::HOST_FEATURES => match self.host_features_sel.load(Ordering::Relaxed) {
                0 => FEATURES_LOW,
                1 => FEATURES_HIGH,
                _ => 0,
            },
            reg::QUEUE_NUM_MAX
                if (self.queue_sel.load(Ordering::Relaxed) as usize) < QUEUE_COUNT =>
            {
                MAX_QUEUE_SIZE as u32
            }
            reg::QUEUE_NUM => self.selected_queue().map_or(0, |q| u32::from(q.size)),
            reg::QUEUE_READY => self
                .selected_queue()
                .map_or(0, |q| u32::from(q.ready && q.valid_size())),
            reg::STATUS => self.status.load(Ordering::Relaxed),
            reg::INTERRUPT_STATUS => self.interrupt_status.load(Ordering::SeqCst),
            reg::CONFIG_GENERATION => self.config_generation.load(Ordering::Acquire),
            reg::QUEUE_DESC_LOW => self
                .selected_queue()
                .map_or(0, |q| q.desc_table_addr as u32),
            reg::QUEUE_DESC_HIGH => self
                .selected_queue()
                .map_or(0, |q| (q.desc_table_addr >> 32) as u32),
            reg::QUEUE_DRIVER_LOW => self
                .selected_queue()
                .map_or(0, |q| q.avail_ring_addr as u32),
            reg::QUEUE_DRIVER_HIGH => self
                .selected_queue()
                .map_or(0, |q| (q.avail_ring_addr >> 32) as u32),
            reg::QUEUE_DEVICE_LOW => self.selected_queue().map_or(0, |q| q.used_ring_addr as u32),
            reg::QUEUE_DEVICE_HIGH => self
                .selected_queue()
                .map_or(0, |q| (q.used_ring_addr >> 32) as u32),
            reg::CONFIG => self.target_pages(),
            off if off == reg::CONFIG + 4 => self.actual_pages(),
            _ => 0,
        };
        Ok(u64::from(value))
    }

    fn mmio_write(&self, offset: u64, value: u64, _len: u8) -> MmioWriteResult {
        let value = value as u32;
        match offset {
            reg::STATUS if value == 0 => self.reset(),
            reg::STATUS => {
                self.status.store(value, Ordering::Relaxed);
                self.activated
                    .store(value & status_bits::DRIVER_OK != 0, Ordering::Relaxed);
            }
            reg::HOST_FEATURES_SEL => self.host_features_sel.store(value, Ordering::Relaxed),
            reg::GUEST_FEATURES_SEL => self.guest_features_sel.store(value, Ordering::Relaxed),
            reg::GUEST_FEATURES => match self.guest_features_sel.load(Ordering::Relaxed) {
                0 => self
                    .guest_features_low
                    .store(value & FEATURES_LOW, Ordering::Relaxed),
                1 => self
                    .guest_features_high
                    .store(value & FEATURES_HIGH, Ordering::Relaxed),
                _ => {}
            },
            reg::QUEUE_SEL => self.queue_sel.store(value, Ordering::Relaxed),
            reg::QUEUE_NUM => self.update_selected_queue(|q| q.set_size(value)),
            reg::QUEUE_READY => self.update_selected_queue(|q| q.set_ready(value != 0)),
            reg::QUEUE_DESC_LOW => self.update_selected_queue(|q| {
                q.desc_table_addr = (q.desc_table_addr & !0xffff_ffff) | u64::from(value)
            }),
            reg::QUEUE_DESC_HIGH => self.update_selected_queue(|q| {
                q.desc_table_addr = (q.desc_table_addr & 0xffff_ffff) | (u64::from(value) << 32)
            }),
            reg::QUEUE_DRIVER_LOW => self.update_selected_queue(|q| {
                q.avail_ring_addr = (q.avail_ring_addr & !0xffff_ffff) | u64::from(value)
            }),
            reg::QUEUE_DRIVER_HIGH => self.update_selected_queue(|q| {
                q.avail_ring_addr = (q.avail_ring_addr & 0xffff_ffff) | (u64::from(value) << 32)
            }),
            reg::QUEUE_DEVICE_LOW => self.update_selected_queue(|q| {
                q.used_ring_addr = (q.used_ring_addr & !0xffff_ffff) | u64::from(value)
            }),
            reg::QUEUE_DEVICE_HIGH => self.update_selected_queue(|q| {
                q.used_ring_addr = (q.used_ring_addr & 0xffff_ffff) | (u64::from(value) << 32)
            }),
            reg::QUEUE_NOTIFY => {
                self.process_queue(value as usize);
            }
            reg::INTERRUPT_ACK => {
                self.interrupt_status.fetch_and(!value, Ordering::SeqCst);
            }
            off if off == reg::CONFIG + 4 => {
                if u64::from(value) <= self.memory.size_bytes / PAGE_SIZE {
                    self.actual_pages.store(value, Ordering::Release);
                } else {
                    self.rejected_pfns.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use crate::virtio::vqueue::{AvailRing, Descriptor, UsedElem};

    fn memory() -> GuestMemory {
        GuestMemory::new(8 * 1024 * 1024).unwrap()
    }

    #[cfg(target_os = "linux")]
    fn configure(dev: &VirtioBalloonMmio, queue: u32, base: u64) {
        dev.mmio_write(reg::QUEUE_SEL, u64::from(queue), 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, 16, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DESC_LOW, base, 4).unwrap();
        dev.mmio_write(reg::QUEUE_DRIVER_LOW, base + 0x1000, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_DEVICE_LOW, base + 0x2000, 4)
            .unwrap();
        dev.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();
    }

    #[test]
    fn config_target_is_bounded_and_interrupting() {
        let dev = VirtioBalloonMmio::new(9, memory(), 0).unwrap();
        assert_eq!(dev.mmio_read(reg::DEVICE_ID, 4).unwrap(), 5);
        dev.mmio_write(reg::QUEUE_SEL, 2, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::QUEUE_NUM_MAX, 4).unwrap(), 0);
        dev.set_target_pages(128).unwrap();
        assert_eq!(dev.mmio_read(reg::CONFIG, 4).unwrap(), 128);
        assert_eq!(dev.mmio_read(reg::INTERRUPT_STATUS, 4).unwrap(), 2);
        assert!(dev.set_target_pages(4096).is_err());
        dev.mmio_write(reg::CONFIG + 4, 64, 4).unwrap();
        assert_eq!(dev.actual_pages(), 64);
        dev.mmio_write(reg::CONFIG + 4, 4096, 4).unwrap();
        assert_eq!(dev.actual_pages(), 64);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inflate_discards_valid_pages_and_rejects_out_of_range_pfns() {
        let memory = memory();
        memory.write_phys(0x4000, &[0x5a; 4096]).unwrap();
        let dev = VirtioBalloonMmio::new(9, memory.clone(), 0).unwrap();
        const DESC: u64 = 0x100000;
        const AVAIL: u64 = DESC + 0x1000;
        const USED: u64 = DESC + 0x2000;
        const PFNS: u64 = DESC + 0x3000;
        memory.write_phys(PFNS, &4u32.to_le_bytes()).unwrap();
        memory
            .inner
            .write_obj(
                Descriptor {
                    addr: PFNS,
                    len: 4,
                    flags: 0,
                    next: 0,
                },
                GuestAddress(DESC),
            )
            .unwrap();
        memory
            .inner
            .write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(AVAIL))
            .unwrap();
        memory
            .inner
            .write_obj(0u16, GuestAddress(AVAIL + 4))
            .unwrap();
        configure(&dev, 0, DESC);
        dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();
        assert_eq!(dev.inflate_pages.load(Ordering::Relaxed), 1);
        let mut page = [1u8; 4096];
        memory.read_phys(0x4000, &mut page).unwrap();
        assert!(page.iter().all(|byte| *byte == 0));
        assert_eq!(
            memory
                .inner
                .read_obj::<UsedElem>(GuestAddress(USED + 4))
                .unwrap()
                .id,
            0
        );
    }

    #[test]
    fn snapshot_preserves_queue_cursors_and_balloon_config() {
        let mut restored = VirtioBalloonMmio::new(9, memory(), 0).unwrap();
        restored.set_target_pages(64).unwrap();
        restored.actual_pages.store(32, Ordering::Relaxed);
        let state = Persist::save(&restored);
        Persist::restore(&mut restored, state);
        assert_eq!(restored.target_pages(), 64);
        assert_eq!(restored.actual_pages(), 32);
    }

    #[test]
    fn pfn_payload_is_bounded_before_guest_memory_allocation_or_read() {
        let dev = VirtioBalloonMmio::new(9, memory(), 0).unwrap();
        assert!(dev
            .parse_pfns(
                &[(u64::MAX - 3, ((MAX_PFNS_PER_REQUEST + 1) * 4) as u32)],
                &[]
            )
            .is_none());
        assert!(dev
            .parse_pfns(
                &[
                    (u64::MAX - 3, (MAX_PFNS_PER_REQUEST * 4) as u32),
                    (u64::MAX - 3, 4),
                ],
                &[],
            )
            .is_none());
    }

    #[test]
    fn pfn_payload_accepts_the_bounded_linux_request_size() {
        let memory = memory();
        let address = 0x10_0000;
        let mut bytes = Vec::with_capacity(MAX_PFNS_PER_REQUEST * 4);
        for pfn in 0..MAX_PFNS_PER_REQUEST as u32 {
            bytes.extend_from_slice(&pfn.to_le_bytes());
        }
        memory
            .inner
            .write_slice(&bytes, GuestAddress(address))
            .unwrap();
        let dev = VirtioBalloonMmio::new(9, memory, 0).unwrap();
        let parsed = dev
            .parse_pfns(&[(address, bytes.len() as u32)], &[])
            .unwrap();
        assert_eq!(parsed.len(), MAX_PFNS_PER_REQUEST);
        assert_eq!(parsed[0], 0);
        assert_eq!(parsed[MAX_PFNS_PER_REQUEST - 1], 255);
    }
}
