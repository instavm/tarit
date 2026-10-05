//! Virtio-mem over MMIO for new hotplug-ready templates.
//! No UNPLUGGED_INACCESSIBLE feature is advertised. The entire private aperture
//! is mapped and reserved against host admission/cgroups, including unplugged
//! blocks; guests cannot escape their maximum reservation by bypassing the driver.
//! New RAM becomes Linux System RAM only when the virtio-mem driver plugs and
//! onlines it. The public API supports grow-only targets.

use crate::bus::{MmioDevice, MmioReadResult, MmioWriteResult};
use crate::persist::Persist;
use crate::virtio::blk_transport::status_bits;
use crate::virtio::regs::{reg, MAGIC};
use crate::virtio::vqueue::{
    is_valid_queue_size, QueueConfig, VirtQueueProcessor, VirtQueueProcessorState, MAX_QUEUE_SIZE,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use vmm_memory_backend::GuestMemory;

pub const DEVICE_ID_MEM: u32 = 24;
const PAGE_SIZE: u64 = 4096;
/// Linux memory sections and our device blocks are both 128MiB. A fixed
/// aperture avoids moving any guest address across restore.
pub const BLOCK_SIZE: u64 = 128 * 1024 * 1024;
pub const REGION_START: u64 = 0x1_0000_0000;
const QUEUE_COUNT: usize = 1;
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
pub struct VirtioMemMmioState {
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
    pub boot_bytes: u64,
    pub maximum_bytes: u64,
    plugged: Vec<bool>,
    actual_pages: u32,
}

pub struct VirtioMemMmio {
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
    plugged: Mutex<Vec<bool>>,
    activated: AtomicBool,
    interrupt_status: AtomicU32,
    config_generation: AtomicU32,
    target_pages: AtomicU32,
    actual_pages: AtomicU32,
    #[cfg(target_os = "linux")]
    irq_evt: Mutex<Option<vmm_sys_util::eventfd::EventFd>>,
}

impl VirtioMemMmio {
    pub fn new(irq: u32, memory: GuestMemory) -> Result<Self, String> {
        let region = memory.size_bytes - memory.low_size_bytes;
        if region == 0
            || !region.is_multiple_of(BLOCK_SIZE)
            || !memory.low_size_bytes.is_multiple_of(BLOCK_SIZE)
        {
            return Err("invalid virtio-mem aperture".into());
        }
        let plugged = Mutex::new(vec![false; (region / BLOCK_SIZE) as usize]);
        let target_pages = 0;
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
            plugged,
            activated: AtomicBool::new(false),
            interrupt_status: AtomicU32::new(0),
            config_generation: AtomicU32::new(0),
            target_pages: AtomicU32::new(target_pages),
            actual_pages: AtomicU32::new(0),
            #[cfg(target_os = "linux")]
            irq_evt: Mutex::new(None),
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

    /// User-visible requests only grow. Device unplug requests are still
    /// supported for driver reset/recovery; they do not reduce the reservation.
    pub fn set_target_total_mib(&self, total_mib: u64) -> Result<(), String> {
        self.prepare_restore_target_total_mib(total_mib)?;
        self.reassert_pending_interrupt();
        Ok(())
    }

    /// Stage a restore target before vCPUs start, without injecting an IRQ into
    /// an IRQCHIP/LAPIC whose saved state has not yet been restored. The caller
    /// must call `reassert_pending_interrupt` after all restored vCPUs are live.
    pub fn prepare_restore_target_total_mib(&self, total_mib: u64) -> Result<(), String> {
        let total = total_mib
            .checked_mul(1024 * 1024)
            .ok_or("memory target overflow")?;
        if total < self.memory.low_size_bytes
            || total > self.memory.size_bytes
            || !total.is_multiple_of(BLOCK_SIZE)
        {
            return Err("memory target must be aligned and within boot..maximum RAM".into());
        }
        let pages = ((total - self.memory.low_size_bytes) / PAGE_SIZE) as u32;
        if pages < self.target_pages() {
            return Err("memory shrink is unsupported".into());
        }
        self.target_pages.store(pages, Ordering::Release);
        self.config_generation.fetch_add(1, Ordering::AcqRel);
        self.interrupt_status.fetch_or(INT_CONFIG, Ordering::SeqCst);
        Ok(())
    }

    /// Reassert saved ring/config causes, including when no target override was
    /// supplied. Snapshot state preserves causes, not the host eventfd counter.
    pub fn reassert_pending_interrupt(&self) {
        #[cfg(target_os = "linux")]
        if let Some(event) = self
            .irq_evt
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .filter(|_| self.has_pending_interrupt())
        {
            let _ = event.write(1);
        }
    }

    pub fn target_total_mib(&self) -> u64 {
        (self.memory.low_size_bytes + u64::from(self.target_pages()) * PAGE_SIZE) / (1024 * 1024)
    }

    pub fn plugged_total_mib(&self) -> u64 {
        (self.memory.low_size_bytes + u64::from(self.actual_pages()) * PAGE_SIZE) / (1024 * 1024)
    }

    pub fn restore_checked(&self, state: VirtioMemMmioState) -> Result<(), String> {
        state.validate_layout(self.memory.size_bytes)?;
        if state.boot_bytes != self.memory.low_size_bytes {
            return Err("virtio-mem boot layout mismatch".into());
        }
        self.apply_state(state);
        Ok(())
    }

    fn trigger_interrupt(&self, kind: u32) {
        self.interrupt_status.fetch_or(kind, Ordering::SeqCst);
        self.reassert_pending_interrupt();
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

    fn configuration(&self) -> [u8; 56] {
        let mut cfg = [0; 56];
        let region = self.memory.size_bytes - self.memory.low_size_bytes;
        for (offset, value) in [
            (0, BLOCK_SIZE),
            (16, REGION_START),
            (24, region),
            (32, region),
            (40, u64::from(self.actual_pages()) * PAGE_SIZE),
            (48, u64::from(self.target_pages()) * PAGE_SIZE),
        ] {
            cfg[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        cfg
    }

    fn request(&self, req: &[u8; 24]) -> [u8; 10] {
        let kind = u16::from_le_bytes(req[0..2].try_into().unwrap());
        let address = u64::from_le_bytes(req[8..16].try_into().unwrap());
        let count = u16::from_le_bytes(req[16..18].try_into().unwrap()) as usize;
        let mut reply = [0u8; 10];
        let mut plugged = self.plugged.lock().unwrap_or_else(|p| p.into_inner());
        let total = plugged.iter().filter(|p| **p).count();
        let first = address
            .checked_sub(REGION_START)
            .filter(|n| n.is_multiple_of(BLOCK_SIZE))
            .and_then(|n| usize::try_from(n / BLOCK_SIZE).ok());
        let range = first.and_then(|n| {
            n.checked_add(count)
                .filter(|end| *end <= plugged.len())
                .map(|end| n..end)
        });
        let result: u16 = if kind == 2 {
            // Initial driver probe can issue UNPLUG_ALL. Keep the fixed aperture
            // usable, and zero memory before acknowledging any later reset.
            if total != 0
                && self
                    .memory
                    .discard_range(
                        REGION_START,
                        self.memory.size_bytes - self.memory.low_size_bytes,
                    )
                    .is_err()
            {
                2
            } else {
                plugged.fill(false);
                self.actual_pages.store(0, Ordering::Release);
                0
            }
        } else if let Some(range) = range.filter(|_| count != 0) {
            let present = plugged[range.clone()].iter().filter(|p| **p).count();
            match kind {
                0 if present != 0 => 3,
                0 if ((total + count) as u64) * BLOCK_SIZE
                    > u64::from(self.target_pages()) * PAGE_SIZE =>
                {
                    1
                }
                0 => {
                    plugged[range].fill(true);
                    self.actual_pages.store(
                        ((total + count) as u64 * BLOCK_SIZE / PAGE_SIZE) as u32,
                        Ordering::Release,
                    );
                    0
                }
                1 if present != count => 3,
                1 => {
                    if self
                        .memory
                        .discard_range(address, count as u64 * BLOCK_SIZE)
                        .is_err()
                    {
                        2
                    } else {
                        plugged[range].fill(false);
                        self.actual_pages.store(
                            ((total - count) as u64 * BLOCK_SIZE / PAGE_SIZE) as u32,
                            Ordering::Release,
                        );
                        0
                    }
                }
                3 => {
                    let state: u16 = if present == count {
                        0
                    } else if present == 0 {
                        1
                    } else {
                        2
                    };
                    reply[8..10].copy_from_slice(&state.to_le_bytes());
                    0
                }
                _ => 3,
            }
        } else {
            3
        };
        reply[0..2].copy_from_slice(&result.to_le_bytes());
        reply
    }

    fn process_queue(&self, queue_index: usize) -> usize {
        if queue_index != 0 {
            return 0;
        }
        let Some(config) = self
            .queues
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .first()
            .and_then(|q| q.config())
        else {
            return 0;
        };
        let mut processors = self.processors.lock().unwrap_or_else(|p| p.into_inner());
        let processor =
            processors[0].get_or_insert_with(|| VirtQueueProcessor::new(config.clone()));
        processor.update_config(config);
        let processed = processor.process_queue_descriptors_dirty(
            &self.memory.inner,
            Some(&self.memory.host_dirty_tracker()),
            |readable, writable| {
                // Validate every buffer before changing device state. Fixed wire
                // sizes bound work even for hostile descriptor chains.
                if readable.len() != 1
                    || readable[0].1 != 24
                    || writable.len() != 1
                    || writable[0].1 < 10
                {
                    return Some(0);
                }
                let mut req = [0u8; 24];
                let mut probe = [0u8; 10];
                if self.memory.read_phys(readable[0].0, &mut req).is_err()
                    || self.memory.read_phys(writable[0].0, &mut probe).is_err()
                {
                    return Some(0);
                }
                let reply = self.request(&req);
                if self.memory.write_phys(writable[0].0, &reply).is_err() {
                    return Some(0);
                }
                Some(10)
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

    fn apply_state(&self, state: VirtioMemMmioState) {
        *self.plugged.lock().unwrap_or_else(|p| p.into_inner()) = state.plugged;
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

impl Persist for VirtioMemMmio {
    type State = VirtioMemMmioState;

    fn save(&self) -> Self::State {
        let processor_states = self
            .processors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|processor| processor.as_ref().map(VirtQueueProcessor::save_state))
            .collect();
        VirtioMemMmioState {
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
            boot_bytes: self.memory.low_size_bytes,
            maximum_bytes: self.memory.size_bytes,
            plugged: self
                .plugged
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
            actual_pages: self.actual_pages(),
        }
    }

    fn restore(&mut self, state: Self::State) {
        self.apply_state(state);
    }
}

impl Persist for std::sync::Arc<VirtioMemMmio> {
    type State = VirtioMemMmioState;

    fn save(&self) -> Self::State {
        self.as_ref().save()
    }

    fn restore(&mut self, state: Self::State) {
        self.apply_state(state);
    }
}

impl MmioDevice for VirtioMemMmio {
    fn mmio_read(&self, offset: u64, len: u8) -> MmioReadResult {
        if offset >= reg::CONFIG {
            let start = (offset - reg::CONFIG) as usize;
            let cfg = self.configuration();
            let mut result = [0u8; 8];
            if !matches!(len, 1 | 2 | 4 | 8) {
                return Ok(0);
            }
            if let Some(bytes) = cfg.get(start..start.saturating_add(len as usize)) {
                result[..len as usize].copy_from_slice(bytes);
            }
            return Ok(u64::from_le_bytes(result));
        }
        let value = match offset {
            reg::MAGIC_VALUE => MAGIC,
            reg::VERSION => 2,
            reg::DEVICE_ID => DEVICE_ID_MEM,
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
                0 => self.guest_features_low.store(0, Ordering::Relaxed),
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
            reg::QUEUE_NOTIFY if self.activated.load(Ordering::Acquire) => {
                self.process_queue(value as usize);
            }
            reg::INTERRUPT_ACK => {
                self.interrupt_status.fetch_and(!value, Ordering::SeqCst);
            }
            _ => {}
        }
        Ok(())
    }
}

impl VirtioMemMmioState {
    pub fn validate_layout(&self, maximum: u64) -> Result<(), String> {
        if self.maximum_bytes != maximum
            || self.boot_bytes == 0
            || self.boot_bytes >= maximum
            || !self.boot_bytes.is_multiple_of(BLOCK_SIZE)
            || !maximum.is_multiple_of(BLOCK_SIZE)
            || self.plugged.len() as u64 != (maximum - self.boot_bytes) / BLOCK_SIZE
            || u64::from(self.target_pages) * PAGE_SIZE > maximum - self.boot_bytes
            || !(u64::from(self.target_pages) * PAGE_SIZE).is_multiple_of(BLOCK_SIZE)
            || self.actual_pages as u64 * PAGE_SIZE
                != self.plugged.iter().filter(|p| **p).count() as u64 * BLOCK_SIZE
            || self.actual_pages > self.target_pages
            || self.queues.len() != 1
            || self.processors.len() != 1
        {
            return Err("invalid virtio-mem snapshot layout/state".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn restore_defers_irq_and_replays_saved_pending_causes() {
        use vmm_sys_util::eventfd::EventFd;
        let d = dev();
        let irq = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        d.set_irq_evt(irq.try_clone().unwrap());
        d.prepare_restore_target_total_mib(512).unwrap();
        assert_eq!(d.target_total_mib(), 512);
        assert!(d.has_pending_interrupt());
        assert_eq!(
            irq.read().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let saved = d.save();
        d.reassert_pending_interrupt();
        assert_eq!(irq.read().unwrap(), 1);
        d.mmio_write(reg::INTERRUPT_ACK, u64::from(INT_CONFIG), 4)
            .unwrap();
        d.reassert_pending_interrupt();
        assert_eq!(
            irq.read().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );

        // Restoring a pending notification must replay it even without a new
        // target: the fresh host eventfd starts empty.
        let restored = dev();
        restored.set_irq_evt(irq.try_clone().unwrap());
        restored.restore_checked(saved).unwrap();
        assert_eq!(
            irq.read().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        restored.reassert_pending_interrupt();
        assert_eq!(irq.read().unwrap(), 1);
    }
    fn dev() -> VirtioMemMmio {
        VirtioMemMmio::new(
            10,
            GuestMemory::new_hotplug(4 * BLOCK_SIZE, 2 * BLOCK_SIZE).unwrap(),
        )
        .unwrap()
    }
    fn request(kind: u16, address: u64, blocks: u16) -> [u8; 24] {
        let mut r = [0; 24];
        r[..2].copy_from_slice(&kind.to_le_bytes());
        r[8..16].copy_from_slice(&address.to_le_bytes());
        r[16..18].copy_from_slice(&blocks.to_le_bytes());
        r
    }
    #[test]
    fn invalid_requests_leave_plugged_state_unchanged_and_unplug_clears_ram() {
        let d = dev();
        d.set_target_total_mib(512).unwrap();
        assert_eq!(d.request(&request(0, REGION_START, 1))[0], 0);
        for req in [
            request(0, REGION_START, 1),
            request(1, REGION_START + BLOCK_SIZE, 1),
            request(0, u64::MAX, u16::MAX),
            request(0, REGION_START, 0),
            request(99, REGION_START, 1),
        ] {
            let before = d.save();
            assert_eq!(d.request(&req)[0], 3);
            assert_eq!(d.save(), before);
        }
        d.memory.write_phys(REGION_START, &[0xa5; 16]).unwrap();
        d.memory.drain_host_dirty();
        assert_eq!(d.request(&request(1, REGION_START, 1))[0], 0);
        let mut bytes = [1; 16];
        d.memory.read_phys(REGION_START, &mut bytes).unwrap();
        assert_eq!(bytes, [0; 16]);
        assert!(d
            .memory
            .drain_host_dirty()
            .contains(d.memory.low_size_bytes));
        assert_eq!(d.plugged_total_mib(), 256);
        assert_eq!(d.target_total_mib(), 512);
    }

    #[test]
    fn queue_notification_uses_notified_queue_and_dirty_tracks_high_dma() {
        use crate::virtio::vqueue::{Descriptor, UsedElem};
        use vm_memory::{Bytes, GuestAddress};
        let d = dev();
        d.set_target_total_mib(512).unwrap();
        let base = REGION_START;
        let req = base + 0x3000;
        let reply = base + 0x4000;
        d.memory
            .write_phys(req, &request(0, REGION_START, 1))
            .unwrap();
        for (index, desc) in [
            Descriptor {
                addr: req,
                len: 24,
                flags: 1,
                next: 1,
            },
            Descriptor {
                addr: reply,
                len: 10,
                flags: 2,
                next: 0,
            },
        ]
        .into_iter()
        .enumerate()
        {
            d.memory
                .inner
                .write_obj(desc, GuestAddress(base + index as u64 * 16))
                .unwrap();
        }
        d.memory
            .write_phys(base + 0x1000, &[0, 0, 1, 0, 0, 0])
            .unwrap();
        for (reg, val) in [
            (reg::QUEUE_SEL, 0),
            (reg::QUEUE_NUM, 8),
            (reg::QUEUE_DESC_LOW, base as u32),
            (reg::QUEUE_DESC_HIGH, 1),
            (reg::QUEUE_DRIVER_LOW, 0x1000),
            (reg::QUEUE_DRIVER_HIGH, 1),
            (reg::QUEUE_DEVICE_LOW, 0x2000),
            (reg::QUEUE_DEVICE_HIGH, 1),
            (reg::QUEUE_READY, 1),
        ] {
            d.mmio_write(reg, u64::from(val), 4).unwrap();
        }
        d.memory.drain_host_dirty();
        d.mmio_write(reg::QUEUE_SEL, 99, 4).unwrap();
        assert_eq!(d.process_queue(0), 1);
        let used: UsedElem = d
            .memory
            .inner
            .read_obj(GuestAddress(base + 0x2004))
            .unwrap();
        assert_eq!(used.len, 10);
        assert_eq!(d.plugged_total_mib(), 384);
        let dirty = d.memory.drain_host_dirty();
        assert!(dirty.contains(d.memory.low_size_bytes + 0x4000));
        assert!(dirty.contains(d.memory.low_size_bytes + 0x2000));
        let restored = VirtioMemMmio::new(10, d.memory.clone()).unwrap();
        restored.restore_checked(d.save()).unwrap();
        assert_eq!(restored.process_queue(0), 0);
    }

    #[test]
    fn growth_is_bounded_and_snapshot_preserves_plugged_blocks() {
        let d = dev();
        assert_eq!(d.request(&request(0, REGION_START, 1))[0], 1);
        d.set_target_total_mib(512).unwrap();
        assert_eq!(d.request(&request(0, REGION_START, 1))[0], 0);
        assert_eq!(d.plugged_total_mib(), 384);
        assert!(d.set_target_total_mib(384).is_err());
        assert!(d.set_target_total_mib(640).is_err());
        assert_eq!(d.request(&request(0, REGION_START + 1, 1))[0], 3);
        assert_eq!(
            d.request(&request(0, REGION_START + 2 * BLOCK_SIZE, 1))[0],
            3
        );
        let restored = dev();
        restored.restore_checked(d.save()).unwrap();
        assert_eq!(restored.target_total_mib(), 512);
        assert_eq!(restored.plugged_total_mib(), 384);
        assert_eq!(restored.request(&request(3, REGION_START, 2))[8], 2);
        assert_eq!(
            restored.request(&request(0, REGION_START + BLOCK_SIZE, 1))[0],
            0
        );
        assert_eq!(restored.plugged_total_mib(), 512);
    }
    #[test]
    fn config_uses_virtio_mem_wire_offsets_and_rejects_forged_state() {
        let d = dev();
        assert_eq!(d.mmio_read(reg::DEVICE_ID, 4).unwrap(), 24);
        assert_eq!(d.mmio_read(reg::CONFIG + 16, 8).unwrap(), REGION_START);
        assert_eq!(d.mmio_read(reg::CONFIG + 24, 8).unwrap(), 2 * BLOCK_SIZE);
        let mut state = d.save();
        state.plugged.push(true);
        assert!(d.restore_checked(state).is_err());
    }
}
