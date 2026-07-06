//! virtio-mmio transport for virtio-rng: register decode + one entropy queue.

use crate::bus::{MmioDevice, MmioReadResult, MmioWriteResult};
use crate::persist::Persist;
use crate::virtio::blk_transport::status_bits;
use crate::virtio::regs::{reg, MAGIC};
use crate::virtio::rng::{VirtioRng, DEVICE_ID_RNG};
use crate::virtio::vqueue::{is_valid_queue_size, QueueConfig, VirtQueueProcessor, MAX_QUEUE_SIZE};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmm_memory_backend::dirty::SoftwareDirtyBitmap;

const RNG_FEATURES_LOW: u32 = 0;
const RNG_FEATURES_HIGH: u32 = 1; // bit 32 = VIRTIO_F_VERSION_1
const VIRTIO_MMIO_INT_VRING: u32 = 0x01;
const QUEUE_RNG: usize = 0;

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
            log::warn!("virtio-rng: QUEUE_NUM {size} exceeds u16 — rejecting");
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
                "virtio-rng: invalid QUEUE_NUM {size} (must be power-of-two <= {MAX_QUEUE_SIZE})"
            );
            self.size = 0;
            self.ready = false;
        }
    }

    fn set_ready(&mut self, ready: bool) {
        if ready && !self.valid_size() {
            log::warn!(
                "virtio-rng: QUEUE_READY ignored for invalid QUEUE_NUM {}",
                self.size
            );
            self.ready = false;
        } else {
            self.ready = ready;
        }
    }
}

/// Snapshot state for virtio-rng MMIO negotiation and queue configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VirtioRngMmioState {
    pub status: u32,
    pub queue_sel: u32,
    pub host_features_sel: u32,
    pub guest_features_sel: u32,
    pub guest_features_low: u32,
    pub guest_features_high: u32,
    queues: Vec<QueueState>,
    pub activated: bool,
    pub interrupt_status: u32,
}

/// The virtio-mmio transport for an entropy device.
pub struct VirtioRngMmio {
    pub irq: u32,
    pub device_id: u32,
    pub vendor_id: u32,
    pub version: u32,
    status: AtomicU32,
    queue_sel: AtomicU32,
    host_features_sel: AtomicU32,
    guest_features_sel: AtomicU32,
    guest_features_low: AtomicU32,
    guest_features_high: AtomicU32,
    queues: Mutex<Vec<QueueState>>,
    rng: Mutex<VirtioRng>,
    guest_mem: Mutex<Option<Arc<GuestMemoryMmap>>>,
    host_dirty: Mutex<Option<SoftwareDirtyBitmap>>,
    activated: AtomicBool,
    processor: Mutex<Option<VirtQueueProcessor>>,
    interrupt_status: AtomicU32,
    #[cfg(target_os = "linux")]
    irq_evt: Mutex<Option<vmm_sys_util::eventfd::EventFd>>,
    pub notify_count: AtomicU64,
    pub status_writes: AtomicU64,
}

impl VirtioRngMmio {
    pub fn new(irq: u32, rng: VirtioRng) -> Self {
        Self {
            irq,
            device_id: DEVICE_ID_RNG,
            vendor_id: 0,
            version: 2,
            status: AtomicU32::new(0),
            queue_sel: AtomicU32::new(0),
            host_features_sel: AtomicU32::new(0),
            guest_features_sel: AtomicU32::new(0),
            guest_features_low: AtomicU32::new(0),
            guest_features_high: AtomicU32::new(0),
            queues: Mutex::new(vec![QueueState::default()]),
            rng: Mutex::new(rng),
            guest_mem: Mutex::new(None),
            host_dirty: Mutex::new(None),
            activated: AtomicBool::new(false),
            processor: Mutex::new(None),
            interrupt_status: AtomicU32::new(0),
            #[cfg(target_os = "linux")]
            irq_evt: Mutex::new(None),
            notify_count: AtomicU64::new(0),
            status_writes: AtomicU64::new(0),
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

    fn process_queue(&self, queue_idx: u32) -> usize {
        if queue_idx as usize != QUEUE_RNG {
            return 0;
        }
        let mem = match self.guest_mem.lock().unwrap().clone() {
            Some(m) => m,
            None => return 0,
        };
        let dirty = self.host_dirty.lock().unwrap().clone();
        let cfg = match self.queue_config(QUEUE_RNG) {
            Some(c) => c,
            None => return 0,
        };

        let mut proc_guard = self.processor.lock().unwrap();
        if proc_guard.is_none() {
            *proc_guard = Some(VirtQueueProcessor::new(cfg));
        } else {
            proc_guard.as_mut().unwrap().update_config(cfg);
        }

        let mut rng = self.rng.lock().unwrap();
        let processed = proc_guard
            .as_mut()
            .unwrap()
            .process_queue_descriptors_dirty(&mem, dirty.as_ref(), |_readable, writable| {
                let mut used_len = 0u32;
                for &(addr, len) in writable {
                    if len == 0 {
                        continue;
                    }
                    let mut entropy = vec![0u8; len as usize];
                    if rng.fill_entropy(&mut entropy).is_err() {
                        return Some(used_len);
                    }
                    if mem.write_slice(&entropy, GuestAddress(addr)).is_err() {
                        return Some(used_len);
                    }
                    if let Some(dirty) = dirty.as_ref() {
                        dirty.mark_range(addr, len as u64);
                    }
                    let Some(next_used_len) = used_len.checked_add(len) else {
                        return Some(used_len);
                    };
                    used_len = next_used_len;
                }
                Some(used_len)
            });

        if processed > 0 {
            self.trigger_interrupt();
        }
        processed
    }

    fn reset(&self) {
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
    }

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

impl Persist for VirtioRngMmio {
    type State = VirtioRngMmioState;

    fn save(&self) -> Self::State {
        VirtioRngMmioState {
            status: self.status.load(Ordering::Relaxed),
            queue_sel: self.queue_sel.load(Ordering::Relaxed),
            host_features_sel: self.host_features_sel.load(Ordering::Relaxed),
            guest_features_sel: self.guest_features_sel.load(Ordering::Relaxed),
            guest_features_low: self.guest_features_low.load(Ordering::Relaxed),
            guest_features_high: self.guest_features_high.load(Ordering::Relaxed),
            queues: self.queues.lock().unwrap().clone(),
            activated: self.activated.load(Ordering::Relaxed),
            interrupt_status: self.interrupt_status.load(Ordering::SeqCst),
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
        *self.processor.lock().unwrap() = None;
    }
}

impl MmioDevice for VirtioRngMmio {
    fn mmio_read(&self, off: u64, _len: u8) -> MmioReadResult {
        let val = match off {
            reg::MAGIC_VALUE => MAGIC,
            reg::VERSION => self.version,
            reg::DEVICE_ID => self.device_id,
            reg::VENDOR_ID => self.vendor_id,
            reg::HOST_FEATURES => match self.host_features_sel.load(Ordering::Relaxed) {
                0 => RNG_FEATURES_LOW,
                1 => RNG_FEATURES_HIGH,
                _ => 0,
            },
            reg::QUEUE_NUM_MAX => MAX_QUEUE_SIZE as u32,
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
            reg::STATUS => self.status.load(Ordering::Relaxed),
            reg::INTERRUPT_STATUS => self.interrupt_status.load(Ordering::SeqCst),
            reg::CONFIG_GENERATION => 0,
            reg::QUEUE_DESC_LOW => self.q_addr(|q| q.desc_table_addr) as u32,
            reg::QUEUE_DESC_HIGH => (self.q_addr(|q| q.desc_table_addr) >> 32) as u32,
            reg::QUEUE_DRIVER_LOW => self.q_addr(|q| q.avail_ring_addr) as u32,
            reg::QUEUE_DRIVER_HIGH => (self.q_addr(|q| q.avail_ring_addr) >> 32) as u32,
            reg::QUEUE_DEVICE_LOW => self.q_addr(|q| q.used_ring_addr) as u32,
            reg::QUEUE_DEVICE_HIGH => (self.q_addr(|q| q.used_ring_addr) >> 32) as u32,
            off if off >= reg::CONFIG => 0,
            _ => 0,
        };
        Ok(val as u64)
    }

    fn mmio_write(&self, off: u64, val: u64, _len: u8) -> MmioWriteResult {
        let val = val as u32;
        match off {
            reg::STATUS => {
                self.status_writes.fetch_add(1, Ordering::Relaxed);
                if val == 0 {
                    self.reset();
                    return Ok(());
                }
                self.status.store(val, Ordering::Relaxed);
                if val & status_bits::DRIVER_OK != 0 {
                    self.activated.store(true, Ordering::Relaxed);
                }
            }
            reg::HOST_FEATURES_SEL => {
                self.host_features_sel.store(val, Ordering::Relaxed);
            }
            reg::GUEST_FEATURES => match self.guest_features_sel.load(Ordering::Relaxed) {
                0 => {
                    self.guest_features_low.store(val, Ordering::Relaxed);
                }
                1 => {
                    self.guest_features_high.store(val, Ordering::Relaxed);
                }
                _ => {}
            },
            reg::GUEST_FEATURES_SEL => {
                self.guest_features_sel.store(val, Ordering::Relaxed);
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
                self.process_queue(val);
            }
            reg::INTERRUPT_ACK => {
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
    use crate::virtio::vqueue::{desc_flags, AvailRing, Descriptor, UsedElem};

    fn new_dev() -> VirtioRngMmio {
        VirtioRngMmio::new(7, VirtioRng::new())
    }

    fn new_mem() -> Arc<GuestMemoryMmap> {
        Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4 * 1024 * 1024)]).unwrap())
    }

    fn configure_queue(dev: &VirtioRngMmio, desc: u64, avail: u64, used: u64, size: u16) {
        dev.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, size as u64, 4).unwrap();
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

    #[test]
    fn mmio_register_negotiation_and_reset() {
        let dev = new_dev();

        assert_eq!(dev.mmio_read(reg::MAGIC_VALUE, 4).unwrap(), MAGIC as u64);
        assert_eq!(dev.mmio_read(reg::VERSION, 4).unwrap(), 2);
        assert_eq!(
            dev.mmio_read(reg::DEVICE_ID, 4).unwrap(),
            DEVICE_ID_RNG as u64
        );
        assert_eq!(dev.mmio_read(reg::QUEUE_NUM_MAX, 4).unwrap(), 256);

        dev.mmio_write(reg::HOST_FEATURES_SEL, 0, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::HOST_FEATURES, 4).unwrap(), 0);
        dev.mmio_write(reg::HOST_FEATURES_SEL, 1, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::HOST_FEATURES, 4).unwrap(), 1);

        dev.mmio_write(reg::GUEST_FEATURES_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, 0x55aa, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, 1, 4).unwrap();

        dev.mmio_write(reg::STATUS, status_bits::ACKNOWLEDGE as u64, 4)
            .unwrap();
        dev.mmio_write(
            reg::STATUS,
            (status_bits::ACKNOWLEDGE | status_bits::DRIVER) as u64,
            4,
        )
        .unwrap();
        dev.mmio_write(
            reg::STATUS,
            (status_bits::ACKNOWLEDGE | status_bits::DRIVER | status_bits::FEATURES_OK) as u64,
            4,
        )
        .unwrap();
        assert_eq!(
            dev.mmio_read(reg::STATUS, 4).unwrap(),
            (status_bits::ACKNOWLEDGE | status_bits::DRIVER | status_bits::FEATURES_OK) as u64
        );

        configure_queue(&dev, 0x1000, 0x2000, 0x3000, 64);
        assert_eq!(dev.mmio_read(reg::QUEUE_READY, 4).unwrap(), 1);
        assert_eq!(dev.mmio_read(reg::QUEUE_NUM, 4).unwrap(), 64);

        dev.trigger_interrupt();
        assert_eq!(dev.mmio_read(reg::INTERRUPT_STATUS, 4).unwrap(), 1);
        dev.mmio_write(reg::INTERRUPT_ACK, 1, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::INTERRUPT_STATUS, 4).unwrap(), 0);
        dev.trigger_interrupt();

        dev.mmio_write(reg::STATUS, 0, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::STATUS, 4).unwrap(), 0);
        assert!(!dev.is_activated());
        assert_eq!(dev.mmio_read(reg::INTERRUPT_STATUS, 4).unwrap(), 0);
        assert_eq!(dev.mmio_read(reg::QUEUE_READY, 4).unwrap(), 0);
        assert_eq!(dev.mmio_read(reg::QUEUE_NUM, 4).unwrap(), 0);
        dev.mmio_write(reg::HOST_FEATURES_SEL, 1, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::HOST_FEATURES, 4).unwrap(), 1);
    }

    #[test]
    fn bus_dispatches_rng_device() {
        let mut bus = MmioBus::new();
        bus.insert(MmioRange::new(0xd000_2000, 0x1000), Box::new(new_dev()))
            .unwrap();
        assert_eq!(
            bus.read(0xd000_2000 + reg::DEVICE_ID, 4).unwrap(),
            DEVICE_ID_RNG as u64
        );
    }

    #[test]
    fn processing_fills_writable_descriptor_and_updates_used_ring() {
        let mem = new_mem();
        let dev = new_dev();
        dev.set_guest_memory(mem.clone());

        const DESC: u64 = 0x10_0000;
        const AVAIL: u64 = 0x10_1000;
        const USED: u64 = 0x10_2000;
        const BUF: u64 = 0x10_3000;
        const BUF_LEN: usize = 64;

        mem.write_obj(
            Descriptor {
                addr: BUF,
                len: BUF_LEN as u32,
                flags: desc_flags::WRITE,
                next: 0,
            },
            GuestAddress(DESC),
        )
        .unwrap();
        mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(AVAIL))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(AVAIL + 4)).unwrap();

        configure_queue(&dev, DESC, AVAIL, USED, 16);
        dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();

        let used_idx: u16 = mem.read_obj(GuestAddress(USED + 2)).unwrap();
        assert_eq!(used_idx, 1);
        let used: UsedElem = mem.read_obj(GuestAddress(USED + 4)).unwrap();
        assert_eq!(used.id, 0);
        assert_eq!(used.len, BUF_LEN as u32);
        assert_eq!(dev.mmio_read(reg::INTERRUPT_STATUS, 4).unwrap(), 1);

        let mut got = [0u8; BUF_LEN];
        mem.read_slice(&mut got, GuestAddress(BUF)).unwrap();
        assert!(got.iter().any(|&b| b != 0));
    }

    #[test]
    fn guest_probe_sequence_negotiates_sets_queue_and_notifies() {
        let mem = new_mem();
        let dev = new_dev();
        dev.set_guest_memory(mem.clone());

        const DESC: u64 = 0x18_0000;
        const AVAIL: u64 = 0x18_1000;
        const USED: u64 = 0x18_2000;
        const BUF: u64 = 0x18_3000;
        const BUF_LEN: usize = 32;

        mem.write_obj(
            Descriptor {
                addr: BUF,
                len: BUF_LEN as u32,
                flags: desc_flags::WRITE,
                next: 0,
            },
            GuestAddress(DESC),
        )
        .unwrap();
        mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(AVAIL))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(AVAIL + 4)).unwrap();

        dev.mmio_write(reg::STATUS, status_bits::ACKNOWLEDGE as u64, 4)
            .unwrap();
        dev.mmio_write(
            reg::STATUS,
            (status_bits::ACKNOWLEDGE | status_bits::DRIVER) as u64,
            4,
        )
        .unwrap();

        dev.mmio_write(reg::HOST_FEATURES_SEL, 0, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::HOST_FEATURES, 4).unwrap(), 0);
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, 0, 4).unwrap();
        dev.mmio_write(reg::HOST_FEATURES_SEL, 1, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::HOST_FEATURES, 4).unwrap(), 1);
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, 1, 4).unwrap();

        dev.mmio_write(
            reg::STATUS,
            (status_bits::ACKNOWLEDGE | status_bits::DRIVER | status_bits::FEATURES_OK) as u64,
            4,
        )
        .unwrap();
        assert_ne!(
            dev.mmio_read(reg::STATUS, 4).unwrap() & status_bits::FEATURES_OK as u64,
            0
        );

        configure_queue(&dev, DESC, AVAIL, USED, 16);
        assert_eq!(dev.mmio_read(reg::QUEUE_READY, 4).unwrap(), 1);

        dev.mmio_write(
            reg::STATUS,
            (status_bits::ACKNOWLEDGE
                | status_bits::DRIVER
                | status_bits::FEATURES_OK
                | status_bits::DRIVER_OK) as u64,
            4,
        )
        .unwrap();
        assert!(dev.is_activated());

        let notify = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4)
        }));
        assert!(notify.is_ok(), "QUEUE_NOTIFY must not panic");
        notify.unwrap().unwrap();

        assert_eq!(dev.notify_count.load(Ordering::Relaxed), 1);
        assert_eq!(dev.mmio_read(reg::INTERRUPT_STATUS, 4).unwrap(), 1);
        let used_idx: u16 = mem.read_obj(GuestAddress(USED + 2)).unwrap();
        assert_eq!(used_idx, 1);
        let used: UsedElem = mem.read_obj(GuestAddress(USED + 4)).unwrap();
        assert_eq!(used.id, 0);
        assert_eq!(used.len, BUF_LEN as u32);
    }

    #[test]
    fn persist_round_trips_negotiated_transport_state() {
        let dev = new_dev();
        dev.mmio_write(reg::HOST_FEATURES_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, 0x1234_5678, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, 1, 4).unwrap();
        dev.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, 128, 4).unwrap();
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
        let mut restored = new_dev();
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
        assert_eq!(restored.mmio_read(reg::QUEUE_NUM, 4).unwrap(), 128);
        assert_eq!(restored.mmio_read(reg::QUEUE_READY, 4).unwrap(), 1);
    }

    #[test]
    fn out_of_bounds_descriptor_is_rejected_without_panic() {
        let mem = new_mem();
        let dev = new_dev();
        dev.set_guest_memory(mem.clone());

        const DESC: u64 = 0x20_0000;
        const AVAIL: u64 = 0x20_1000;
        const USED: u64 = 0x20_2000;

        mem.write_obj(
            Descriptor {
                addr: 0xf000_0000,
                len: 512,
                flags: desc_flags::WRITE,
                next: 0,
            },
            GuestAddress(DESC),
        )
        .unwrap();
        mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(AVAIL))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(AVAIL + 4)).unwrap();

        configure_queue(&dev, DESC, AVAIL, USED, 16);
        let notify = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4)
        }));
        assert!(notify.is_ok(), "OOB descriptor must not panic");
        notify.unwrap().unwrap();

        let used_idx: u16 = mem.read_obj(GuestAddress(USED + 2)).unwrap();
        assert_eq!(used_idx, 0);
    }

    #[test]
    fn readable_descriptor_is_consumed_without_entropy_write() {
        let mem = new_mem();
        let dev = new_dev();
        dev.set_guest_memory(mem.clone());

        const DESC: u64 = 0x28_0000;
        const AVAIL: u64 = 0x28_1000;
        const USED: u64 = 0x28_2000;
        const BUF: u64 = 0x28_3000;
        const BUF_LEN: usize = 16;

        let original = [0xa5u8; BUF_LEN];
        mem.write_slice(&original, GuestAddress(BUF)).unwrap();
        mem.write_obj(
            Descriptor {
                addr: BUF,
                len: BUF_LEN as u32,
                flags: 0,
                next: 0,
            },
            GuestAddress(DESC),
        )
        .unwrap();
        mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(AVAIL))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(AVAIL + 4)).unwrap();

        configure_queue(&dev, DESC, AVAIL, USED, 16);
        dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();

        let mut got = [0u8; BUF_LEN];
        mem.read_slice(&mut got, GuestAddress(BUF)).unwrap();
        assert_eq!(got, original);

        let used_idx: u16 = mem.read_obj(GuestAddress(USED + 2)).unwrap();
        assert_eq!(used_idx, 1);
        let used: UsedElem = mem.read_obj(GuestAddress(USED + 4)).unwrap();
        assert_eq!(used.len, 0);
    }
}
