//! Virtio-mem over MMIO for hotplug-ready VMs and templates.
//!
//! Boot RAM is ordinary low memory and is never part of this device. The
//! device owns a fixed aperture at [`REGION_START`] (4 GiB) of 128 MiB blocks.
//! The host sets a *requested size* (the hotplug amount, excluding boot RAM);
//! the guest driver plugs or unplugs blocks until its plugged size matches.
//! Unplugged blocks are discarded (`MADV_DONTNEED`, coordinated with lazy UFFD
//! restore through [`GuestMemory::discard_range`]) so host RSS drops.
//!
//! No UNPLUGGED_INACCESSIBLE feature is advertised. The entire private aperture
//! stays mapped and KVM-registered, including unplugged blocks; it is reserved
//! against host admission/cgroups, so guests cannot escape their maximum
//! reservation by bypassing the driver. New RAM becomes Linux System RAM only
//! when the virtio-mem driver plugs and onlines it.
//!
//! Request semantics follow the virtio-mem specification (virtio v1.2 §5.15)
//! and Linux's `include/uapi/linux/virtio_mem.h`:
//! * PLUG: every block in range must be unplugged (else ERROR), and the result
//!   may not exceed the requested size (else NACK).
//! * UNPLUG: every block in range must be plugged (else ERROR). Allowed even
//!   when it takes the plugged size below the requested size.
//! * UNPLUG_ALL: unplugs and discards every plugged block. Linux sends it only
//!   from driver probe when the device reports plugged memory (a fresh guest
//!   kernel after reboot/kexec); a restored guest never re-probes, so restore
//!   keeps its plugged blocks. Boot RAM is outside the aperture and is never
//!   touched.
//! * STATE: PLUGGED / UNPLUGGED / MIXED from the bitmap.
//!
//! A discard failure answers BUSY and leaves the bitmap unchanged; the driver
//! retries, and re-discarding an already discarded block is harmless.

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

const REQ_PLUG: u16 = 0;
const REQ_UNPLUG: u16 = 1;
const REQ_UNPLUG_ALL: u16 = 2;
const REQ_STATE: u16 = 3;
const RESP_ACK: u16 = 0;
const RESP_NACK: u16 = 1;
const RESP_BUSY: u16 = 2;
const RESP_ERROR: u16 = 3;
const STATE_PLUGGED: u16 = 0;
const STATE_UNPLUGGED: u16 = 1;
const STATE_MIXED: u16 = 2;

/// Point-in-time hotplug sizes, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtioMemStatus {
    /// Device block size (the plug/unplug granularity).
    pub block_size: u64,
    /// Fixed RAM below the MMIO gap, outside the device.
    pub boot_size: u64,
    /// Size of the hotplug aperture (maximum - boot).
    pub region_size: u64,
    /// Blocks the guest driver has plugged and not unplugged.
    pub plugged_size: u64,
    /// Host-requested hotplug size the driver converges towards.
    pub requested_size: u64,
}

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

    /// Grow-only total-RAM target (boot + hotplug), as used by restore.
    pub fn set_target_total_mib(&self, total_mib: u64) -> Result<(), String> {
        self.prepare_restore_target_total_mib(total_mib)?;
        self.reassert_pending_interrupt();
        Ok(())
    }

    /// Size of the hotplug aperture in bytes.
    pub fn region_size(&self) -> u64 {
        self.memory.size_bytes - self.memory.low_size_bytes
    }

    pub fn status(&self) -> VirtioMemStatus {
        // Read both sizes under the bitmap lock so they describe one instant
        // with respect to guest requests.
        let _plugged = self.plugged.lock().unwrap_or_else(|p| p.into_inner());
        VirtioMemStatus {
            block_size: BLOCK_SIZE,
            boot_size: self.memory.low_size_bytes,
            region_size: self.region_size(),
            plugged_size: u64::from(self.actual_pages()) * PAGE_SIZE,
            requested_size: u64::from(self.target_pages()) * PAGE_SIZE,
        }
    }

    /// Unplugged blocks as coalesced `(offset, len)` ranges of packed guest RAM
    /// (the offsets of snapshot images and [`GuestMemory::as_ptr`]), in order.
    /// The driver must not write unplugged memory (virtio v1.2 §5.15) and the
    /// device discards it on unplug, so full snapshots store these ranges as
    /// zeros without reading them.
    pub fn unplugged_ranges(&self) -> Vec<(u64, u64)> {
        let plugged = self.plugged.lock().unwrap_or_else(|p| p.into_inner());
        let base = self.memory.low_size_bytes;
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for (index, _) in plugged.iter().enumerate().filter(|(_, p)| !**p) {
            let start = base + index as u64 * BLOCK_SIZE;
            match ranges.last_mut() {
                Some((run_start, run_len)) if *run_start + *run_len == start => {
                    *run_len += BLOCK_SIZE;
                }
                _ => ranges.push((start, BLOCK_SIZE)),
            }
        }
        ranges
    }

    /// Live resize of a running guest: set the requested hotplug size in
    /// bytes (excluding boot RAM). Growing and shrinking are both allowed;
    /// `0` asks the driver to unplug everything. The value must be a multiple
    /// of [`BLOCK_SIZE`] and at most [`Self::region_size`]. Always bumps the
    /// config generation and raises the config-change interrupt, so repeating
    /// the current value re-kicks a driver that is backing off after a failed
    /// unplug.
    pub fn set_requested_size(&self, bytes: u64) -> Result<(), String> {
        const MIB: u64 = 1024 * 1024;
        let region = self.region_size();
        if bytes > region {
            return Err(format!(
                "requested hotplug size {} MiB exceeds the {} MiB virtio-mem region",
                bytes / MIB,
                region / MIB
            ));
        }
        if !bytes.is_multiple_of(BLOCK_SIZE) {
            return Err(format!(
                "requested hotplug size must be a multiple of the {} MiB block size",
                BLOCK_SIZE / MIB
            ));
        }
        let pages = u32::try_from(bytes / PAGE_SIZE)
            .map_err(|_| "requested hotplug size overflows the device".to_string())?;
        {
            // Serialize with in-flight PLUG checks, which read the target
            // while holding the bitmap lock.
            let _plugged = self.plugged.lock().unwrap_or_else(|p| p.into_inner());
            self.target_pages.store(pages, Ordering::Release);
            self.config_generation.fetch_add(1, Ordering::AcqRel);
        }
        self.trigger_interrupt(INT_CONFIG);
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
        let result: u16 = if kind == REQ_UNPLUG_ALL {
            // Discard only plugged blocks: unplugged ones hold no guest data
            // the driver relies on, and discarding the whole aperture would
            // mark every page host-dirty (bloating the next diff snapshot) and
            // walk every page under lazy restore.
            if self.discard_plugged_runs(&plugged).is_err() {
                RESP_BUSY
            } else {
                plugged.fill(false);
                self.actual_pages.store(0, Ordering::Release);
                RESP_ACK
            }
        } else if let Some(range) = range.filter(|_| count != 0) {
            let present = plugged[range.clone()].iter().filter(|p| **p).count();
            match kind {
                REQ_PLUG if present != 0 => RESP_ERROR,
                REQ_PLUG
                    if ((total + count) as u64) * BLOCK_SIZE
                        > u64::from(self.target_pages()) * PAGE_SIZE =>
                {
                    RESP_NACK
                }
                REQ_PLUG => {
                    plugged[range].fill(true);
                    self.actual_pages.store(
                        ((total + count) as u64 * BLOCK_SIZE / PAGE_SIZE) as u32,
                        Ordering::Release,
                    );
                    RESP_ACK
                }
                REQ_UNPLUG if present != count => RESP_ERROR,
                REQ_UNPLUG => {
                    if self.discard_blocks(range.start, count).is_err() {
                        RESP_BUSY
                    } else {
                        plugged[range].fill(false);
                        self.actual_pages.store(
                            ((total - count) as u64 * BLOCK_SIZE / PAGE_SIZE) as u32,
                            Ordering::Release,
                        );
                        RESP_ACK
                    }
                }
                REQ_STATE => {
                    let state: u16 = if present == count {
                        STATE_PLUGGED
                    } else if present == 0 {
                        STATE_UNPLUGGED
                    } else {
                        STATE_MIXED
                    };
                    reply[8..10].copy_from_slice(&state.to_le_bytes());
                    RESP_ACK
                }
                _ => RESP_ERROR,
            }
        } else {
            RESP_ERROR
        };
        reply[0..2].copy_from_slice(&result.to_le_bytes());
        reply
    }

    /// Release the host backing of `count` blocks starting at block `first`.
    /// `GuestMemory::discard_range` marks the packed range host-dirty (so diff
    /// snapshots record the zeroed pages) and, under lazy restore, records the
    /// pages as discarded so the UFFD handler resolves later faults with zeroes
    /// instead of resurrecting snapshot bytes, under the snapshot fence.
    fn discard_blocks(&self, first: usize, count: usize) -> Result<(), String> {
        self.memory
            .discard_range(
                REGION_START + first as u64 * BLOCK_SIZE,
                count as u64 * BLOCK_SIZE,
            )
            .map_err(|error| error.to_string())
    }

    fn discard_plugged_runs(&self, plugged: &[bool]) -> Result<(), String> {
        let mut index = 0;
        while index < plugged.len() {
            if !plugged[index] {
                index += 1;
                continue;
            }
            let start = index;
            while index < plugged.len() && plugged[index] {
                index += 1;
            }
            self.discard_blocks(start, index - start)?;
        }
        Ok(())
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
    /// Validate a saved device state against the restored maximum. Plugged
    /// may exceed requested: a snapshot can be taken while the guest is still
    /// unplugging after a live shrink (or could not unplug unmovable memory).
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
    /// 2 boot blocks + a 6-block (768 MiB) aperture.
    fn wide_dev() -> VirtioMemMmio {
        VirtioMemMmio::new(
            10,
            GuestMemory::new_hotplug(8 * BLOCK_SIZE, 2 * BLOCK_SIZE).unwrap(),
        )
        .unwrap()
    }

    fn block(index: u64) -> u64 {
        REGION_START + index * BLOCK_SIZE
    }

    fn resp(reply: [u8; 10]) -> u16 {
        u16::from_le_bytes([reply[0], reply[1]])
    }

    fn state_of(d: &VirtioMemMmio, first: u64, blocks: u16) -> u16 {
        let reply = d.request(&request(REQ_STATE, block(first), blocks));
        assert_eq!(resp(reply), RESP_ACK);
        u16::from_le_bytes([reply[8], reply[9]])
    }

    #[cfg(target_os = "linux")]
    fn page_is_resident(d: &VirtioMemMmio, gpa: u64) -> bool {
        let offset = d.memory.gpa_to_offset(gpa).unwrap() as usize;
        let mut residency = 0u8;
        // SAFETY: `offset` is a page-aligned offset inside the live mapping;
        // mincore writes exactly one byte for one page.
        let rc = unsafe {
            libc::mincore(
                d.memory.as_ptr().add(offset) as *mut libc::c_void,
                PAGE_SIZE as usize,
                &mut residency,
            )
        };
        assert_eq!(rc, 0, "mincore: {}", std::io::Error::last_os_error());
        residency & 1 == 1
    }

    #[test]
    fn live_grow_shrink_and_unplug_follow_the_requested_size() {
        let d = wide_dev();
        assert_eq!(
            d.status(),
            VirtioMemStatus {
                block_size: BLOCK_SIZE,
                boot_size: 2 * BLOCK_SIZE,
                region_size: 6 * BLOCK_SIZE,
                plugged_size: 0,
                requested_size: 0,
            }
        );
        // Grow: plug up to the requested size, then NACK beyond it.
        d.set_requested_size(3 * BLOCK_SIZE).unwrap();
        assert_eq!(resp(d.request(&request(REQ_PLUG, block(0), 3))), RESP_ACK);
        assert_eq!(resp(d.request(&request(REQ_PLUG, block(3), 1))), RESP_NACK);
        assert_eq!(state_of(&d, 0, 3), STATE_PLUGGED);
        assert_eq!(state_of(&d, 2, 2), STATE_MIXED);
        assert_eq!(state_of(&d, 3, 3), STATE_UNPLUGGED);

        // Shrink: allowed live; plugged stays until the guest unplugs.
        d.set_requested_size(BLOCK_SIZE).unwrap();
        assert_eq!(d.status().requested_size, BLOCK_SIZE);
        assert_eq!(d.status().plugged_size, 3 * BLOCK_SIZE);
        assert_eq!(
            d.mmio_read(reg::CONFIG + 48, 8).unwrap(),
            BLOCK_SIZE,
            "requested_size config field"
        );
        assert_eq!(
            d.mmio_read(reg::CONFIG + 40, 8).unwrap(),
            3 * BLOCK_SIZE,
            "plugged_size config field"
        );
        assert_eq!(resp(d.request(&request(REQ_UNPLUG, block(1), 2))), RESP_ACK);
        assert_eq!(d.status().plugged_size, BLOCK_SIZE);
        assert_eq!(state_of(&d, 1, 2), STATE_UNPLUGGED);
        // Unplugging blocks that are not (all) plugged is an error and
        // changes nothing.
        let before = d.save();
        assert_eq!(
            resp(d.request(&request(REQ_UNPLUG, block(0), 2))),
            RESP_ERROR
        );
        assert_eq!(d.save(), before);
        // Plugging is NACKed while at or above the requested size.
        assert_eq!(resp(d.request(&request(REQ_PLUG, block(1), 1))), RESP_NACK);

        // Shrink to zero: the driver unplugs the rest block by block.
        d.set_requested_size(0).unwrap();
        assert_eq!(resp(d.request(&request(REQ_UNPLUG, block(0), 1))), RESP_ACK);
        assert_eq!(d.status().plugged_size, 0);
        assert_eq!(state_of(&d, 0, 6), STATE_UNPLUGGED);

        // Grow again after a full shrink.
        d.set_requested_size(6 * BLOCK_SIZE).unwrap();
        assert_eq!(resp(d.request(&request(REQ_PLUG, block(0), 6))), RESP_ACK);
        assert_eq!(d.status().plugged_size, 6 * BLOCK_SIZE);
    }

    #[test]
    fn requested_size_outside_the_aperture_or_misaligned_is_rejected() {
        let d = wide_dev();
        d.set_requested_size(2 * BLOCK_SIZE).unwrap();
        let before = d.save();
        for bytes in [
            7 * BLOCK_SIZE,
            u64::MAX,
            BLOCK_SIZE / 2,
            BLOCK_SIZE + PAGE_SIZE,
            1,
        ] {
            let error = d.set_requested_size(bytes).unwrap_err();
            assert!(
                error.contains("exceeds") || error.contains("multiple"),
                "{error}"
            );
            assert_eq!(d.save(), before, "rejected target must not change state");
        }
        d.set_requested_size(6 * BLOCK_SIZE).unwrap();
        d.set_requested_size(0).unwrap();
    }

    #[test]
    fn set_requested_size_raises_a_config_change() {
        let d = wide_dev();
        let generation = d.mmio_read(reg::CONFIG_GENERATION, 4).unwrap();
        d.set_requested_size(BLOCK_SIZE).unwrap();
        assert_eq!(
            d.mmio_read(reg::INTERRUPT_STATUS, 4).unwrap() & u64::from(INT_CONFIG),
            u64::from(INT_CONFIG)
        );
        assert_ne!(d.mmio_read(reg::CONFIG_GENERATION, 4).unwrap(), generation);
        d.mmio_write(reg::INTERRUPT_ACK, u64::from(INT_CONFIG), 4)
            .unwrap();
        assert!(!d.has_pending_interrupt());
        // Re-sending the same value re-kicks the driver.
        d.set_requested_size(BLOCK_SIZE).unwrap();
        assert!(d.has_pending_interrupt());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn set_requested_size_signals_the_irq_eventfd() {
        use vmm_sys_util::eventfd::EventFd;
        let d = wide_dev();
        let irq = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        d.set_irq_evt(irq.try_clone().unwrap());
        d.set_requested_size(2 * BLOCK_SIZE).unwrap();
        assert_eq!(irq.read().unwrap(), 1);
        assert!(d.set_requested_size(3).is_err());
        assert_eq!(
            irq.read().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn unplugged_ranges_are_coalesced_packed_offsets() {
        let d = wide_dev(); // 2 blocks of boot RAM, 6-block aperture
        let boot = d.memory.low_size_bytes;
        assert_eq!(d.unplugged_ranges(), vec![(boot, 6 * BLOCK_SIZE)]);

        d.set_requested_size(3 * BLOCK_SIZE).unwrap();
        assert_eq!(resp(d.request(&request(REQ_PLUG, block(1), 2))), RESP_ACK);
        assert_eq!(resp(d.request(&request(REQ_PLUG, block(5), 1))), RESP_ACK);
        assert_eq!(
            d.unplugged_ranges(),
            vec![(boot, BLOCK_SIZE), (boot + 3 * BLOCK_SIZE, 2 * BLOCK_SIZE)]
        );
        // The packed offsets are where the blocks live in host memory.
        assert_eq!(
            d.memory.gpa_to_offset(block(3)),
            Some(boot + 3 * BLOCK_SIZE)
        );

        assert_eq!(resp(d.request(&request(REQ_PLUG, block(0), 1))), RESP_NACK);
        d.set_requested_size(6 * BLOCK_SIZE).unwrap();
        for index in [0, 3, 4] {
            assert_eq!(
                resp(d.request(&request(REQ_PLUG, block(index), 1))),
                RESP_ACK
            );
        }
        assert!(d.unplugged_ranges().is_empty());
    }

    #[test]
    fn unplug_all_discards_only_plugged_blocks_and_never_boot_ram() {
        let d = wide_dev();
        d.set_requested_size(2 * BLOCK_SIZE).unwrap();
        assert_eq!(resp(d.request(&request(REQ_PLUG, block(0), 2))), RESP_ACK);
        d.memory.write_phys(0x10_0000, &[0x11; 16]).unwrap();
        d.memory.write_phys(block(1) + 0x2000, &[0x22; 16]).unwrap();
        // The driver must not use unplugged memory, but nothing prevents a
        // guest from touching it; UNPLUG_ALL does not need to scrub it.
        d.memory.write_phys(block(4), &[0x44; 16]).unwrap();
        d.memory.drain_host_dirty();

        assert_eq!(resp(d.request(&request(REQ_UNPLUG_ALL, 0, 0))), RESP_ACK);
        let status = d.status();
        assert_eq!(status.plugged_size, 0);
        assert_eq!(
            status.requested_size,
            2 * BLOCK_SIZE,
            "UNPLUG_ALL keeps the request"
        );
        assert_eq!(state_of(&d, 0, 6), STATE_UNPLUGGED);

        let mut bytes = [0xff; 16];
        d.memory.read_phys(block(1) + 0x2000, &mut bytes).unwrap();
        assert_eq!(bytes, [0; 16], "plugged block was discarded");
        d.memory.read_phys(0x10_0000, &mut bytes).unwrap();
        assert_eq!(bytes, [0x11; 16], "boot RAM is outside the aperture");
        d.memory.read_phys(block(4), &mut bytes).unwrap();
        assert_eq!(bytes, [0x44; 16], "unplugged block was not rediscarded");

        let dirty = d.memory.drain_host_dirty();
        let boot = d.memory.low_size_bytes;
        assert!(dirty.contains(boot));
        assert!(dirty.contains(boot + BLOCK_SIZE + 0x2000));
        assert!(!dirty.contains(boot + 4 * BLOCK_SIZE));
        assert!(!dirty.contains(0x10_0000));
        assert_eq!(dirty.len() as u64, 2 * BLOCK_SIZE / PAGE_SIZE);

        // Nothing plugged: still ACKed, nothing discarded.
        assert_eq!(resp(d.request(&request(REQ_UNPLUG_ALL, 0, 0))), RESP_ACK);
        assert!(d.memory.drain_host_dirty().is_empty());
    }

    #[test]
    fn device_reset_keeps_plugged_blocks_for_the_next_driver_probe() {
        // Spec: plugged state survives device reset. A re-probing driver (new
        // guest kernel) sees plugged_size != 0 and issues UNPLUG_ALL; a
        // restored guest never re-probes and keeps its blocks.
        let d = wide_dev();
        d.set_requested_size(BLOCK_SIZE).unwrap();
        assert_eq!(resp(d.request(&request(REQ_PLUG, block(2), 1))), RESP_ACK);
        d.memory.write_phys(block(2), &[0x5a; 8]).unwrap();
        d.mmio_write(reg::STATUS, 0, 4).unwrap();
        assert_eq!(d.mmio_read(reg::CONFIG + 40, 8).unwrap(), BLOCK_SIZE);
        assert_eq!(d.status().requested_size, BLOCK_SIZE);
        let mut bytes = [0; 8];
        d.memory.read_phys(block(2), &mut bytes).unwrap();
        assert_eq!(bytes, [0x5a; 8]);

        let restored = wide_dev();
        restored.restore_checked(d.save()).unwrap();
        assert_eq!(state_of(&restored, 2, 1), STATE_PLUGGED);
        assert_eq!(restored.status().plugged_size, BLOCK_SIZE);
    }

    #[test]
    fn snapshot_round_trips_after_unplug_and_mid_shrink() {
        let d = wide_dev();
        d.set_requested_size(4 * BLOCK_SIZE).unwrap();
        assert_eq!(resp(d.request(&request(REQ_PLUG, block(0), 4))), RESP_ACK);
        d.set_requested_size(BLOCK_SIZE).unwrap();
        assert_eq!(resp(d.request(&request(REQ_UNPLUG, block(1), 1))), RESP_ACK);
        // Mid-shrink: three blocks still plugged against a one-block request.
        let saved = d.save();
        saved.validate_layout(8 * BLOCK_SIZE).unwrap();
        let restored = wide_dev();
        restored.restore_checked(saved.clone()).unwrap();
        assert_eq!(restored.status(), d.status());
        assert_eq!(state_of(&restored, 0, 1), STATE_PLUGGED);
        assert_eq!(state_of(&restored, 1, 1), STATE_UNPLUGGED);
        assert_eq!(state_of(&restored, 2, 2), STATE_PLUGGED);
        // The restored device keeps converging the same way.
        assert_eq!(
            resp(restored.request(&request(REQ_UNPLUG, block(2), 2))),
            RESP_ACK
        );
        assert_eq!(restored.status().plugged_size, BLOCK_SIZE);

        // A forged plugged count that disagrees with the bitmap is rejected.
        let mut forged = saved;
        forged.actual_pages += (BLOCK_SIZE / PAGE_SIZE) as u32;
        assert!(wide_dev().restore_checked(forged).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unplug_returns_resident_backing_to_the_host() {
        let d = wide_dev();
        d.set_requested_size(2 * BLOCK_SIZE).unwrap();
        assert_eq!(resp(d.request(&request(REQ_PLUG, block(0), 2))), RESP_ACK);
        let touched = [block(0), block(0) + 0x10_0000, block(1) + 0x3000];
        for gpa in touched {
            d.memory.write_phys(gpa, &[0xa5; 64]).unwrap();
            assert!(page_is_resident(&d, gpa));
        }
        d.set_requested_size(BLOCK_SIZE).unwrap();
        assert_eq!(resp(d.request(&request(REQ_UNPLUG, block(1), 1))), RESP_ACK);
        assert!(page_is_resident(&d, block(0)));
        assert!(!page_is_resident(&d, block(1) + 0x3000));
        assert_eq!(resp(d.request(&request(REQ_UNPLUG_ALL, 0, 0))), RESP_ACK);
        for gpa in touched {
            assert!(!page_is_resident(&d, gpa), "{gpa:#x} still resident");
        }
    }

    /// UNPLUG of a lazily restored block must go through the UFFD discard
    /// bitmap: a later fault returns zeroes, never the snapshot bytes, for
    /// both a page faulted in before the unplug and one never touched.
    /// Requires userfaultfd; skips where the kernel or sandbox denies it.
    #[cfg(target_os = "linux")]
    #[test]
    fn unplug_under_lazy_restore_never_resurrects_snapshot_bytes() {
        use std::io::{Seek, SeekFrom, Write};
        use vmm_memory_backend::{start_lazy_restore_with_integrity, UffdRestoreError};

        let memory = GuestMemory::new_hotplug(4 * BLOCK_SIZE, 2 * BLOCK_SIZE).unwrap();
        let len = memory.size_bytes;
        let boot = memory.low_size_bytes;
        let mut file = tempfile::tempfile().unwrap();
        file.set_len(len).unwrap();
        for packed in [boot, boot + PAGE_SIZE] {
            file.seek(SeekFrom::Start(packed)).unwrap();
            file.write_all(&[0x5a; PAGE_SIZE as usize]).unwrap();
        }
        file.sync_all().unwrap();
        let lazy = match start_lazy_restore_with_integrity(
            memory.as_ptr() as *mut u8,
            len as usize,
            &file,
            0,
            len,
            Some(memory.host_dirty_tracker()),
            None,
        ) {
            Ok(lazy) => lazy,
            Err(UffdRestoreError::Userfaultfd(error))
                if matches!(error.raw_os_error(), Some(libc::EPERM) | Some(libc::ENOSYS)) =>
            {
                eprintln!("skipping: userfaultfd unavailable ({error})");
                return;
            }
            Err(error) => panic!("start lazy restore: {error}"),
        };
        memory.set_lazy_page_discard(lazy.page_discard());
        let d = VirtioMemMmio::new(10, memory.clone()).unwrap();
        d.set_requested_size(BLOCK_SIZE).unwrap();
        assert_eq!(resp(d.request(&request(REQ_PLUG, block(0), 1))), RESP_ACK);
        let mut bytes = [0; 8];
        d.memory.read_phys(block(0), &mut bytes).unwrap();
        assert_eq!(bytes, [0x5a; 8], "lazy fault serves snapshot bytes");

        d.set_requested_size(0).unwrap();
        d.memory.drain_host_dirty();
        assert_eq!(resp(d.request(&request(REQ_UNPLUG, block(0), 1))), RESP_ACK);
        for gpa in [block(0), block(0) + PAGE_SIZE] {
            d.memory.read_phys(gpa, &mut bytes).unwrap();
            assert_eq!(bytes, [0; 8], "{gpa:#x} resurrected snapshot bytes");
        }
        assert!(d.memory.drain_host_dirty().contains(boot + PAGE_SIZE));
        drop(d);
        drop(lazy);
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
