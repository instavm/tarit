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

use crate::bus::{MmioDevice, MmioError, MmioReadResult, MmioWriteResult};
use crate::persist::{Persist, PersistError};
use crate::rate_limit::RateLimiter;
use crate::virtio::blk_transport::status_bits;
use crate::virtio::regs::{reg, MAGIC};
use crate::virtio::vqueue::{
    is_valid_queue_size, QueueConfig, VirtQueueProcessor, VirtQueueProcessorState,
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

/// Queue size offered for virtio-net. With receive TSO and no mergeable
/// buffers Linux posts each receive buffer as a ~19-descriptor chain, so a
/// 256-entry ring holds only about 13 and runs dry between refills; 1024
/// entries hold about 53.
pub const NET_QUEUE_SIZE_MAX: u16 = 1024;

/// Max Ethernet frame we accept end-to-end (header + 1500 MTU + slack).
const MAX_FRAME_BYTES: usize = 1600;

/// Max frame with offloads: the 65550-byte packet virtio-net allows with TSO,
/// plus the header.
pub const MAX_GSO_FRAME_BYTES: usize = VIRTIO_NET_HDR_LEN + 65550;

/// virtio-net feature bits (virtio 1.x §5.1.3).
pub mod net_features {
    pub const CSUM: u32 = 1 << 0;
    pub const GUEST_CSUM: u32 = 1 << 1;
    pub const MAC: u32 = 1 << 5;
    pub const GUEST_TSO4: u32 = 1 << 7;
    pub const GUEST_TSO6: u32 = 1 << 8;
    pub const HOST_TSO4: u32 = 1 << 11;
    pub const HOST_TSO6: u32 = 1 << 12;
    /// VIRTIO_NET_F_MRG_RXBUF: a received frame may span several receive
    /// buffers, so the driver posts small single-page buffers instead of a
    /// 64 KiB chain per frame when it accepts segmentation offload.
    pub const MRG_RXBUF: u32 = 1 << 15;
    /// VIRTIO_RING_F_EVENT_IDX: interrupts and kicks only when the other
    /// side asks for them.
    pub const RING_EVENT_IDX: u32 = 1 << 29;
    /// Offered only on a TAP whose frames carry a virtio-net header, which
    /// is what carries checksum and segmentation state between the guest and
    /// the host stack.
    pub const OFFLOADS: u32 = CSUM | GUEST_CSUM | GUEST_TSO4 | GUEST_TSO6 | HOST_TSO4 | HOST_TSO6;
}

/// `virtio_net_hdr` flags and GSO types the receive path checks.
const VIRTIO_NET_HDR_F_NEEDS_CSUM: u8 = 1;
const VIRTIO_NET_HDR_GSO_NONE: u8 = 0;
const VIRTIO_NET_HDR_GSO_TCPV4: u8 = 1;
const VIRTIO_NET_HDR_GSO_TCPV6: u8 = 4;
const VIRTIO_NET_HDR_GSO_ECN: u8 = 0x80;

/// TAP offload flags (`TUN_F_*`) for the receive offloads a guest accepted.
pub fn tap_offloads_for(guest_features: u32) -> u32 {
    const TUN_F_CSUM: u32 = 0x01;
    const TUN_F_TSO4: u32 = 0x02;
    const TUN_F_TSO6: u32 = 0x04;
    if guest_features & net_features::GUEST_CSUM == 0 {
        // Segmentation offloads require checksum offload.
        return 0;
    }
    let mut flags = TUN_F_CSUM;
    if guest_features & net_features::GUEST_TSO4 != 0 {
        flags |= TUN_F_TSO4;
    }
    if guest_features & net_features::GUEST_TSO6 != 0 {
        flags |= TUN_F_TSO6;
    }
    flags
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
        is_valid_queue_size(self.size, NET_QUEUE_SIZE_MAX)
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
        if is_valid_queue_size(size, NET_QUEUE_SIZE_MAX) {
            self.size = size;
        } else {
            log::warn!(
                "virtio-net: invalid QUEUE_NUM {size} (must be power-of-two <= {NET_QUEUE_SIZE_MAX})"
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

/// Read `out.len()` bytes at byte offset `at` of a descriptor chain.
#[cfg(target_os = "linux")]
fn read_chain(
    mem: &GuestMemoryMmap,
    chain: &[(u64, u32)],
    mut at: usize,
    out: &mut [u8],
) -> Result<(), ()> {
    let mut done = 0;
    for &(addr, len) in chain {
        let len = len as usize;
        if at >= len {
            at -= len;
            continue;
        }
        let take = (len - at).min(out.len() - done);
        mem.read_slice(&mut out[done..done + take], GuestAddress(addr + at as u64))
            .map_err(|_| ())?;
        done += take;
        at = 0;
        if done == out.len() {
            return Ok(());
        }
    }
    Err(())
}

/// Write `data` at byte offset `at` of a descriptor chain.
#[cfg(target_os = "linux")]
fn write_chain(
    mem: &GuestMemoryMmap,
    chain: &[(u64, u32)],
    mut at: usize,
    data: &[u8],
) -> Result<(), ()> {
    let mut done = 0;
    for &(addr, len) in chain {
        let len = len as usize;
        if at >= len {
            at -= len;
            continue;
        }
        let take = (len - at).min(data.len() - done);
        mem.write_slice(&data[done..done + take], GuestAddress(addr + at as u64))
            .map_err(|_| ())?;
        done += take;
        at = 0;
        if done == data.len() {
            return Ok(());
        }
    }
    Err(())
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
    /// The tap's frames carry a virtio-net header (`IFF_VNET_HDR`), so the
    /// device offers offloads and passes the header through unchanged.
    vnet_hdr: AtomicBool,
    /// TAP offloads (`TUN_F_*`) to apply, set when the guest negotiates and
    /// applied by the network worker, the only thread allowed that ioctl.
    /// `u32::MAX` means nothing is pending.
    pending_offloads: AtomicU32,
    /// RX entries were added that the driver asked to be interrupted for;
    /// raised by the next `signal_rx`.
    rx_irq_pending: AtomicBool,
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
    /// Permanently fail the transport after its isolated queue worker exits or
    /// cannot acknowledge quiescence. A later snapshot must not serialize a
    /// VM whose network worker can still mutate memory or cannot make progress.
    pub fn fail_worker(&self, context: &str) {
        self.fail_device(context);
    }

    fn fail_device(&self, context: &str) {
        log::error!("virtio-net: {context}");
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
            PersistError::Unavailable(format!("virtio-net {name} lock is poisoned"))
        })
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
            vnet_hdr: AtomicBool::new(false),
            pending_offloads: AtomicU32::new(u32::MAX),
            rx_irq_pending: AtomicBool::new(false),
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

    /// Hand the transport a tap fd and whether its frames carry a virtio-net
    /// header (`Tap::vnet_hdr`), which decides the frame format on the fd.
    /// The fd's lifetime is the caller's; the transport only uses it for
    /// `write(2)` on TX.
    pub fn set_tap_fd(&self, fd: RawFd, vnet_hdr: bool) {
        *self.lock_state(&self.tap_fd, "tap_fd") = Some(fd);
        self.set_vnet_hdr(vnet_hdr);
    }

    /// Record that the tap's frames carry a virtio-net header. Must be set
    /// before the guest reads the device features.
    pub fn set_vnet_hdr(&self, vnet_hdr: bool) {
        self.vnet_hdr.store(vnet_hdr, Ordering::Relaxed);
    }

    pub fn vnet_hdr(&self) -> bool {
        self.vnet_hdr.load(Ordering::Relaxed)
    }

    /// Low feature word offered to the guest.
    pub fn host_features(&self) -> u32 {
        let features = self.features | net_features::RING_EVENT_IDX | net_features::MRG_RXBUF;
        if self.vnet_hdr() {
            features | net_features::OFFLOADS
        } else {
            features
        }
    }

    fn event_idx(&self) -> bool {
        self.guest_features.load(Ordering::Relaxed) & net_features::RING_EVENT_IDX != 0
    }

    fn mergeable(&self) -> bool {
        self.guest_features.load(Ordering::Relaxed) & net_features::MRG_RXBUF != 0
    }

    /// Offload features the guest negotiated.
    pub fn negotiated_offloads(&self) -> u32 {
        self.guest_features.load(Ordering::Relaxed) & net_features::OFFLOADS
    }

    fn request_tap_offloads(&self, guest_features: u32) {
        if self.vnet_hdr() {
            self.pending_offloads
                .store(tap_offloads_for(guest_features), Ordering::Release);
        }
    }

    /// TAP offloads the network worker should apply, if any are pending.
    pub fn take_offload_request(&self) -> Option<u32> {
        match self.pending_offloads.swap(u32::MAX, Ordering::AcqRel) {
            u32::MAX => None,
            flags => Some(flags),
        }
    }

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
        if self.interrupt_status.load(Ordering::SeqCst) == 0 {
            return;
        }
        #[cfg(target_os = "linux")]
        if let Ok(guard) = self.try_lock_state(&self.irq_evt, "irq_evt") {
            if let Some(evt) = guard.as_ref() {
                let _ = evt.write(1);
            }
        }
    }

    fn trigger_interrupt(&self) -> MmioWriteResult {
        self.interrupt_status
            .fetch_or(VIRTIO_MMIO_INT_VRING, Ordering::SeqCst);
        #[cfg(target_os = "linux")]
        if let Some(evt) = self.try_lock_state(&self.irq_evt, "irq_evt")?.as_ref() {
            let _ = evt.write(1);
        }
        Ok(())
    }

    fn queue_config(&self, idx: usize) -> Result<Option<QueueConfig>, MmioError> {
        let qs = self.try_lock_state(&self.queues, "queues")?;
        let Some(q) = qs.get(idx) else {
            return Ok(None);
        };
        if !q.ready || !q.valid_size() {
            return Ok(None);
        }
        Ok(Some(QueueConfig {
            size: q.size,
            desc_table_addr: q.desc_table_addr,
            avail_ring_addr: q.avail_ring_addr,
            used_ring_addr: q.used_ring_addr,
            ready: q.ready,
        }))
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
    pub fn process_tx_queue(&self) -> Result<usize, MmioError> {
        self.ensure_operational()?;
        let mem = match self.try_lock_state(&self.guest_mem, "guest_mem")?.clone() {
            Some(m) => m,
            None => return Ok(0),
        };
        let dirty = self.try_lock_state(&self.host_dirty, "host_dirty")?.clone();
        let cfg = match self.queue_config(QUEUE_TX)? {
            Some(c) => c,
            None => return Ok(0),
        };
        let tap_fd = *self.try_lock_state(&self.tap_fd, "tap_fd")?;
        // With a virtio-net header on the tap, frames (up to 64 KiB with TSO)
        // go out with the guest's header; otherwise the header is dropped.
        let vnet_hdr = self.vnet_hdr();
        let max_frame = if vnet_hdr {
            MAX_GSO_FRAME_BYTES
        } else {
            MAX_FRAME_BYTES
        };

        let mut proc_guard = self.try_lock_state(&self.tx_processor, "tx_processor")?;
        if proc_guard.is_none() {
            *proc_guard = Some(VirtQueueProcessor::new(cfg));
        } else {
            proc_guard.as_mut().unwrap().update_config(cfg);
        }
        proc_guard.as_mut().unwrap().set_event_idx(self.event_idx());
        let old_used = proc_guard.as_ref().unwrap().last_used_idx();

        let mut total_pkts = 0usize;
        let mut processing_failed = false;
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
                    if new_len > max_frame {
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
                let wire: &[u8] = if vnet_hdr { &buf } else { packet };

                match self.rate_limit_allows(packet.len() as u64) {
                    Ok(true) => {}
                    Ok(false) => return None,
                    Err(_) => {
                        processing_failed = true;
                        return None;
                    }
                }

                if let Some(fd) = tap_fd {
                    // SAFETY: write to a valid open fd. Short writes count as a packet sent.
                    let rc = unsafe {
                        libc::write(fd, wire.as_ptr() as *const libc::c_void, wire.len())
                    };
                    if rc > 0 {
                        total_pkts += 1;
                        self.tx_packets.fetch_add(1, Ordering::Relaxed);
                        self.tx_bytes.fetch_add(rc as u64, Ordering::Relaxed);
                    }
                }
                Some(0) // device wrote nothing back into the chain
            });

        if processed > 0
            && proc_guard
                .as_ref()
                .unwrap()
                .needs_notification(&mem, old_used)
        {
            self.trigger_interrupt()?;
        }
        if processing_failed || self.is_failed() {
            Err(MmioError::Device)
        } else {
            Ok(total_pkts)
        }
    }

    /// Inject a packet received from the host tap into the RX queue.
    /// Walks one writable chain, lays down a zero virtio_net_hdr, then the
    /// packet bytes. Returns true iff a chain was consumed. If no avail
    /// descriptor is ready the packet is dropped (caller should retry later).
    pub fn inject_rx_packet(&self, packet: &[u8]) -> Result<bool, MmioError> {
        self.inject_rx(None, packet, true)
    }

    /// [`Self::inject_rx_packet`] without interrupting the guest; the
    /// caller batches frames and calls [`Self::signal_rx`] once.
    pub fn inject_rx_packet_quiet(&self, packet: &[u8]) -> Result<bool, MmioError> {
        self.inject_rx(None, packet, false)
    }

    /// Interrupt the guest for frames delivered by the `_quiet` and
    /// `inject_rx_vnet_frame` calls.
    pub fn signal_rx(&self) -> Result<(), MmioError> {
        if self.rx_irq_pending.swap(false, Ordering::AcqRel) {
            self.trigger_interrupt()?;
        }
        Ok(())
    }

    /// Deliver a frame read from a tap with a virtio-net header (`frame` is
    /// header + packet). The header passes through, so offloads the guest
    /// negotiated (partial checksums, TSO) reach it intact. A frame using an
    /// offload the guest did not accept (possible only while the tap still
    /// has the previous driver's offloads) is dropped. Does not interrupt
    /// the guest; call [`Self::signal_rx`] after a batch.
    pub fn inject_rx_vnet_frame(&self, frame: &[u8]) -> Result<bool, MmioError> {
        if frame.len() <= VIRTIO_NET_HDR_LEN {
            return Ok(false);
        }
        let mut header = [0u8; VIRTIO_NET_HDR_LEN];
        header.copy_from_slice(&frame[..VIRTIO_NET_HDR_LEN]);
        let accepted = self.guest_features.load(Ordering::Relaxed);
        let needs_csum = header[0] & VIRTIO_NET_HDR_F_NEEDS_CSUM != 0;
        let gso_allowed = match header[1] & !VIRTIO_NET_HDR_GSO_ECN {
            VIRTIO_NET_HDR_GSO_NONE => true,
            VIRTIO_NET_HDR_GSO_TCPV4 => accepted & net_features::GUEST_TSO4 != 0,
            VIRTIO_NET_HDR_GSO_TCPV6 => accepted & net_features::GUEST_TSO6 != 0,
            _ => false,
        };
        if (needs_csum && accepted & net_features::GUEST_CSUM == 0)
            || !gso_allowed
            || header[1] & VIRTIO_NET_HDR_GSO_ECN != 0
        {
            log::debug!("virtio-net: dropping a frame with offloads the guest did not accept");
            return Ok(false);
        }
        header[VIRTIO_NET_HDR_NUM_BUFFERS_OFF..].copy_from_slice(&1u16.to_le_bytes());
        self.inject_rx(Some(header), &frame[VIRTIO_NET_HDR_LEN..], false)
    }

    /// Read frames from a tap carrying a virtio-net header straight into the
    /// guest's receive buffers (one copy, as the kernel writes them there),
    /// up to `max_frames`, without interrupting the guest. Returns how many
    /// were delivered and whether the tap ran dry; `None` when this path does
    /// not apply (no vnet header, a rate limiter, or no guest memory yet).
    #[cfg(target_os = "linux")]
    pub fn rx_from_vnet_tap(
        &self,
        tap_fd: RawFd,
        max_frames: usize,
    ) -> Result<Option<(usize, bool)>, MmioError> {
        use vm_memory::GuestMemoryBackend;

        self.ensure_operational()?;
        if !self.vnet_hdr()
            || self
                .try_lock_state(&self.rate_limiter, "rate_limiter")?
                .is_some()
        {
            return Ok(None);
        }
        let Some(mem) = self.try_lock_state(&self.guest_mem, "guest_mem")?.clone() else {
            return Ok(None);
        };
        let dirty = self.try_lock_state(&self.host_dirty, "host_dirty")?.clone();
        let Some(cfg) = self.queue_config(QUEUE_RX)? else {
            return Ok(Some((0, false)));
        };
        let accepted = self.guest_features.load(Ordering::Relaxed);
        let mut proc_guard = self.try_lock_state(&self.rx_processor, "rx_processor")?;
        let processor = match proc_guard.as_mut() {
            Some(processor) => {
                processor.update_config(cfg);
                processor
            }
            None => proc_guard.insert(VirtQueueProcessor::new(cfg)),
        };
        processor.set_event_idx(self.event_idx());
        let old_used = processor.last_used_idx();

        if self.mergeable() {
            let (delivered, tap_empty) =
                self.rx_mergeable_from_tap(processor, &mem, dirty.as_ref(), tap_fd, max_frames);
            if delivered > 0 && processor.needs_notification(&mem, old_used) {
                self.rx_irq_pending.store(true, Ordering::Release);
            }
            drop(proc_guard);
            if self.is_failed() {
                return Err(MmioError::Device);
            }
            return Ok(Some((delivered, tap_empty)));
        }

        let mut delivered = 0usize;
        let mut tap_empty = false;
        processor.process_queue_descriptors_dirty(&mem, dirty.as_ref(), |_readable, writable| {
            if delivered == max_frames {
                return None;
            }
            // Host addresses of the chain's buffers; each descriptor lies in
            // one guest memory region or the chain is unusable.
            let mut guards = Vec::with_capacity(writable.len());
            let mut iovecs = Vec::with_capacity(writable.len());
            for &(addr, len) in writable {
                let Ok(slice) = mem.get_slice(GuestAddress(addr), len as usize) else {
                    return Some(0);
                };
                let guard = slice.ptr_guard_mut();
                iovecs.push(libc::iovec {
                    iov_base: guard.as_ptr() as *mut libc::c_void,
                    iov_len: len as usize,
                });
                guards.push(guard);
            }
            // SAFETY: every iovec points into guest memory kept mapped by its
            // guard for the duration of the call.
            let rc = unsafe { libc::readv(tap_fd, iovecs.as_ptr(), iovecs.len() as libc::c_int) };
            drop(guards);
            if rc < 0 {
                // Nothing ready (or a transient error): keep the chain.
                tap_empty = true;
                return None;
            }
            let len = rc as usize;
            if len <= VIRTIO_NET_HDR_LEN {
                return Some(0);
            }
            if let Some(dirty) = dirty.as_ref() {
                let mut left = len;
                for &(addr, size) in writable {
                    let take = left.min(size as usize);
                    dirty.mark_guest_range(addr, take as u64);
                    left -= take;
                    if left == 0 {
                        break;
                    }
                }
            }
            let mut header = [0u8; VIRTIO_NET_HDR_LEN];
            if read_chain(&mem, writable, 0, &mut header).is_err() {
                return Some(0);
            }
            let needs_csum = header[0] & VIRTIO_NET_HDR_F_NEEDS_CSUM != 0;
            let gso_ok = match header[1] {
                VIRTIO_NET_HDR_GSO_NONE => true,
                VIRTIO_NET_HDR_GSO_TCPV4 => accepted & net_features::GUEST_TSO4 != 0,
                VIRTIO_NET_HDR_GSO_TCPV6 => accepted & net_features::GUEST_TSO6 != 0,
                _ => false,
            };
            if (needs_csum && accepted & net_features::GUEST_CSUM == 0) || !gso_ok {
                // Only while the tap still has a previous driver's offloads.
                log::debug!("virtio-net: dropping a frame with offloads the guest did not accept");
                return Some(0);
            }
            if write_chain(
                &mem,
                writable,
                VIRTIO_NET_HDR_NUM_BUFFERS_OFF,
                &1u16.to_le_bytes(),
            )
            .is_err()
            {
                return Some(0);
            }
            delivered += 1;
            self.rx_packets.fetch_add(1, Ordering::Relaxed);
            self.rx_bytes
                .fetch_add((len - VIRTIO_NET_HDR_LEN) as u64, Ordering::Relaxed);
            Some(len as u32)
        });
        if delivered > 0 && processor.needs_notification(&mem, old_used) {
            self.rx_irq_pending.store(true, Ordering::Release);
        }
        drop(proc_guard);
        if self.is_failed() {
            return Err(MmioError::Device);
        }
        Ok(Some((delivered, tap_empty)))
    }

    /// Whether a received frame's header uses only offloads the guest
    /// accepted. A tap holds the previous driver's offloads until the new
    /// one's are applied, so this can briefly fail.
    fn rx_header_allowed(&self, header: &[u8; VIRTIO_NET_HDR_LEN]) -> bool {
        let accepted = self.guest_features.load(Ordering::Relaxed);
        let needs_csum = header[0] & VIRTIO_NET_HDR_F_NEEDS_CSUM != 0;
        let gso_ok = match header[1] {
            VIRTIO_NET_HDR_GSO_NONE => true,
            VIRTIO_NET_HDR_GSO_TCPV4 => accepted & net_features::GUEST_TSO4 != 0,
            VIRTIO_NET_HDR_GSO_TCPV6 => accepted & net_features::GUEST_TSO6 != 0,
            _ => false,
        };
        (!needs_csum || accepted & net_features::GUEST_CSUM != 0) && gso_ok
    }

    /// Largest frame, header included, the guest can be handed: a
    /// segmentation-offload frame once it accepted one, else one MTU.
    fn rx_frame_limit(&self) -> usize {
        let accepted = self.guest_features.load(Ordering::Relaxed);
        if accepted & (net_features::GUEST_TSO4 | net_features::GUEST_TSO6) != 0 {
            MAX_GSO_FRAME_BYTES
        } else {
            MAX_FRAME_BYTES
        }
    }

    /// [`Self::rx_from_vnet_tap`] with mergeable receive buffers: each frame
    /// is read straight into as many posted buffers as it fills, and only
    /// once they can hold the largest frame the tap may return (a frame
    /// cut short would be corrupt).
    #[cfg(target_os = "linux")]
    fn rx_mergeable_from_tap(
        &self,
        processor: &mut VirtQueueProcessor,
        mem: &GuestMemoryMmap,
        dirty: Option<&SoftwareDirtyBitmap>,
        tap_fd: RawFd,
        max_frames: usize,
    ) -> (usize, bool) {
        use vm_memory::GuestMemoryBackend;

        let limit = self.rx_frame_limit();
        let mut delivered = 0usize;
        let mut tap_empty = false;
        let mut buffers: Vec<(u64, u32)> = Vec::new();
        let mut capacities: Vec<usize> = Vec::new();
        let mut lens: Vec<u32> = Vec::new();
        loop {
            let mut waiting = false;
            while delivered < max_frames {
                buffers.clear();
                capacities.clear();
                let mut room = 0usize;
                for chain in processor.peek_writable(mem, limit) {
                    buffers.extend_from_slice(&chain.buffers);
                    capacities.push(chain.capacity);
                    room += chain.capacity;
                    if room >= limit {
                        break;
                    }
                }
                if room < limit {
                    waiting = true;
                    break;
                }
                let mut guards = Vec::with_capacity(buffers.len());
                let mut iovecs = Vec::with_capacity(buffers.len());
                for &(addr, len) in &buffers {
                    let Ok(slice) = mem.get_slice(GuestAddress(addr), len as usize) else {
                        break;
                    };
                    let guard = slice.ptr_guard_mut();
                    iovecs.push(libc::iovec {
                        iov_base: guard.as_ptr() as *mut libc::c_void,
                        iov_len: len as usize,
                    });
                    guards.push(guard);
                }
                if iovecs.len() < buffers.len() {
                    // Validated buffers that do not map.
                    self.fail_device("receive buffer outside guest memory");
                    break;
                }
                // SAFETY: every iovec points into guest memory kept mapped by
                // its guard for the duration of the call.
                let rc =
                    unsafe { libc::readv(tap_fd, iovecs.as_ptr(), iovecs.len() as libc::c_int) };
                drop(guards);
                if rc < 0 {
                    tap_empty = true;
                    break;
                }
                let len = rc as usize;
                // The buffers stay posted for the next frame if this one is
                // empty or uses offloads the guest did not accept.
                let mut header = [0u8; VIRTIO_NET_HDR_LEN];
                if len <= VIRTIO_NET_HDR_LEN
                    || read_chain(mem, &buffers, 0, &mut header).is_err()
                    || !self.rx_header_allowed(&header)
                {
                    log::debug!(
                        "virtio-net: dropping an empty frame or one with unaccepted offloads"
                    );
                    continue;
                }
                lens.clear();
                let mut left = len;
                for &capacity in &capacities {
                    if left == 0 {
                        break;
                    }
                    let take = left.min(capacity);
                    lens.push(take as u32);
                    left -= take;
                }
                if write_chain(
                    mem,
                    &buffers,
                    VIRTIO_NET_HDR_NUM_BUFFERS_OFF,
                    &(lens.len() as u16).to_le_bytes(),
                )
                .is_err()
                {
                    continue;
                }
                if let Some(dirty) = dirty {
                    let mut left = len;
                    for &(addr, size) in &buffers {
                        let take = left.min(size as usize);
                        dirty.mark_guest_range(addr, take as u64);
                        left -= take;
                        if left == 0 {
                            break;
                        }
                    }
                }
                processor.use_peeked(mem, dirty, &lens);
                delivered += 1;
                self.rx_packets.fetch_add(1, Ordering::Relaxed);
                self.rx_bytes
                    .fetch_add((len - VIRTIO_NET_HDR_LEN) as u64, Ordering::Relaxed);
            }
            // Buffers posted after the kick request was read would not kick.
            if !processor.publish_used(mem, dirty) || !waiting || self.is_failed() {
                return (delivered, tap_empty);
            }
        }
    }

    /// Copy one frame into as many mergeable receive buffers as it needs.
    /// `Ok(false)` while the posted buffers cannot hold it yet.
    fn inject_rx_mergeable(
        &self,
        processor: &mut VirtQueueProcessor,
        mem: &GuestMemoryMmap,
        dirty: Option<&SoftwareDirtyBitmap>,
        header: [u8; VIRTIO_NET_HDR_LEN],
        packet: &[u8],
    ) -> Result<bool, MmioError> {
        let need = VIRTIO_NET_HDR_LEN + packet.len();
        loop {
            let room: usize = processor
                .peek_writable(mem, need)
                .iter()
                .map(|chain| chain.capacity)
                .sum();
            if room >= need {
                break;
            }
            if !processor.publish_used(mem, dirty) {
                return Ok(false);
            }
        }
        match self.rate_limit_allows(packet.len() as u64) {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(_) => return Err(MmioError::Device),
        }
        let mut lens = Vec::new();
        let mut left = need;
        for chain in processor.peek_writable(mem, need) {
            if left == 0 {
                break;
            }
            let take = left.min(chain.capacity);
            lens.push(take as u32);
            left -= take;
        }
        let mut header = header;
        header[VIRTIO_NET_HDR_NUM_BUFFERS_OFF..]
            .copy_from_slice(&(lens.len() as u16).to_le_bytes());
        let mut sources = [&header[..], packet].into_iter();
        let mut source: &[u8] = sources.next().unwrap_or_default();
        'copy: for chain in processor.peek_writable(mem, need).iter().take(lens.len()) {
            for &(addr, len) in &chain.buffers {
                let mut at = addr;
                let mut room = len as usize;
                while room > 0 {
                    while source.is_empty() {
                        match sources.next() {
                            Some(next) => source = next,
                            None => break 'copy,
                        }
                    }
                    let take = room.min(source.len());
                    if mem.write_slice(&source[..take], GuestAddress(at)).is_err() {
                        self.fail_device("receive buffer write failed");
                        return Err(MmioError::Device);
                    }
                    if let Some(dirty) = dirty {
                        dirty.mark_guest_range(at, take as u64);
                    }
                    source = &source[take..];
                    room -= take;
                    at += take as u64;
                }
            }
        }
        processor.use_peeked(mem, dirty, &lens);
        processor.publish_used(mem, dirty);
        self.rx_packets.fetch_add(1, Ordering::Relaxed);
        self.rx_bytes
            .fetch_add(packet.len() as u64, Ordering::Relaxed);
        Ok(true)
    }

    fn inject_rx(
        &self,
        header: Option<[u8; VIRTIO_NET_HDR_LEN]>,
        packet: &[u8],
        notify: bool,
    ) -> Result<bool, MmioError> {
        self.ensure_operational()?;
        let Some(need) = packet.len().checked_add(VIRTIO_NET_HDR_LEN) else {
            return Ok(false);
        };
        let max_frame = if header.is_some() {
            MAX_GSO_FRAME_BYTES
        } else {
            MAX_FRAME_BYTES
        };
        if need > max_frame {
            return Ok(false);
        }
        let mem = match self.try_lock_state(&self.guest_mem, "guest_mem")?.clone() {
            Some(m) => m,
            None => return Ok(false),
        };
        let dirty = self.try_lock_state(&self.host_dirty, "host_dirty")?.clone();
        let cfg = match self.queue_config(QUEUE_RX)? {
            Some(c) => c,
            None => return Ok(false),
        };

        let mut proc_guard = self.try_lock_state(&self.rx_processor, "rx_processor")?;
        if proc_guard.is_none() {
            *proc_guard = Some(VirtQueueProcessor::new(cfg));
        } else {
            proc_guard.as_mut().unwrap().update_config(cfg);
        }
        proc_guard.as_mut().unwrap().set_event_idx(self.event_idx());
        let old_used = proc_guard.as_ref().unwrap().last_used_idx();

        if self.mergeable() {
            let header = header.unwrap_or_default();
            let processor = proc_guard.as_mut().unwrap();
            let delivered =
                self.inject_rx_mergeable(processor, &mem, dirty.as_ref(), header, packet)?;
            if delivered && processor.needs_notification(&mem, old_used) {
                if notify {
                    self.trigger_interrupt()?;
                } else {
                    self.rx_irq_pending.store(true, Ordering::Release);
                }
            }
            return Ok(delivered);
        }

        let mut delivered = false;
        let mut processing_failed = false;
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
                match self.rate_limit_allows(packet.len() as u64) {
                    Ok(true) => {}
                    Ok(false) => return None,
                    Err(_) => {
                        processing_failed = true;
                        return None;
                    }
                }
                // Lay down the 12-byte virtio_net_hdr_v1 (the tap's, or zeroed,
                // with num_buffers = 1 for this single-chain packet), then
                // the packet bytes, spanning whatever descriptors are needed.
                let header = header.unwrap_or_else(|| {
                    let mut header = [0u8; VIRTIO_NET_HDR_LEN];
                    header[VIRTIO_NET_HDR_NUM_BUFFERS_OFF..].copy_from_slice(&1u16.to_le_bytes());
                    header
                });
                // Copy header then packet straight into the chain.
                let mut sources = [&header[..], packet].into_iter();
                let mut source: &[u8] = sources.next().unwrap_or_default();
                for &(addr, len) in writable {
                    let mut at = addr;
                    let mut room = len as usize;
                    while room > 0 {
                        while source.is_empty() {
                            match sources.next() {
                                Some(next) => source = next,
                                None => break,
                            }
                        }
                        if source.is_empty() {
                            break;
                        }
                        let take = room.min(source.len());
                        if mem.write_slice(&source[..take], GuestAddress(at)).is_err() {
                            return Some(0);
                        }
                        if let Some(dirty) = dirty.as_ref() {
                            dirty.mark_guest_range(at, take as u64);
                        }
                        source = &source[take..];
                        room -= take;
                        at += take as u64;
                    }
                }
                delivered = true;
                self.rx_packets.fetch_add(1, Ordering::Relaxed);
                self.rx_bytes
                    .fetch_add(packet.len() as u64, Ordering::Relaxed);
                Some(need as u32)
            });

        if delivered
            && proc_guard
                .as_ref()
                .unwrap()
                .needs_notification(&mem, old_used)
        {
            if notify {
                self.trigger_interrupt()?;
            } else {
                self.rx_irq_pending.store(true, Ordering::Release);
            }
        }
        if processing_failed || self.is_failed() {
            Err(MmioError::Device)
        } else {
            Ok(delivered)
        }
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

    fn snapshot_state(&self) -> Result<VirtioNetMmioState, PersistError> {
        if self.is_failed() {
            return Err(PersistError::Unavailable(
                "virtio-net is failed and requires reset".into(),
            ));
        }
        Ok(VirtioNetMmioState {
            status: self.status.load(Ordering::Relaxed),
            queue_sel: self.queue_sel.load(Ordering::Relaxed),
            host_features_sel: self.host_features_sel.load(Ordering::Relaxed),
            guest_features_sel: self.guest_features_sel.load(Ordering::Relaxed),
            guest_features: self.guest_features.load(Ordering::Relaxed),
            queues: self.snapshot_lock(&self.queues, "queues")?.clone(),
            activated: self.activated.load(Ordering::Relaxed),
            interrupt_status: self.interrupt_status.load(Ordering::SeqCst),
            rx_processor: self
                .snapshot_lock(&self.rx_processor, "rx_processor")?
                .as_ref()
                .map(VirtQueueProcessor::save_state),
            tx_processor: self
                .snapshot_lock(&self.tx_processor, "tx_processor")?
                .as_ref()
                .map(VirtQueueProcessor::save_state),
        })
    }

    fn reset(&self) {
        self.queues.clear_poison();
        self.guest_mem.clear_poison();
        self.host_dirty.clear_poison();
        self.rx_processor.clear_poison();
        self.tx_processor.clear_poison();
        self.rate_limiter.clear_poison();
        self.tap_fd.clear_poison();
        #[cfg(target_os = "linux")]
        self.irq_evt.clear_poison();
        self.status.store(0, Ordering::SeqCst);
        self.activated.store(false, Ordering::SeqCst);
        // The driver went away; stop the tap sending offloaded frames.
        self.request_tap_offloads(0);
        self.host_features_sel.store(0, Ordering::SeqCst);
        self.guest_features_sel.store(0, Ordering::SeqCst);
        self.guest_features.store(0, Ordering::SeqCst);
        self.queue_sel.store(0, Ordering::SeqCst);
        self.interrupt_status.store(0, Ordering::SeqCst);
        for queue in self.lock_state(&self.queues, "queues").iter_mut() {
            *queue = QueueState::default();
        }
        *self.lock_state(&self.rx_processor, "rx_processor") = None;
        *self.lock_state(&self.tx_processor, "tx_processor") = None;
    }
}

impl Persist for VirtioNetMmio {
    type State = VirtioNetMmioState;

    fn save(&self) -> Self::State {
        self.snapshot_state()
            .expect("virtio-net state is unavailable")
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
        self.guest_features
            .store(state.guest_features, Ordering::Relaxed);
        *self.lock_state(&self.queues, "queues") = state.queues;
        self.activated.store(state.activated, Ordering::Relaxed);
        if state.activated {
            // The new tap starts with no offloads; give it the guest's.
            self.request_tap_offloads(state.guest_features);
        }
        self.interrupt_status
            .store(state.interrupt_status, Ordering::SeqCst);
        let queues = self.lock_state(&self.queues, "queues");
        let rx_config = queues
            .get(QUEUE_RX)
            .map(Self::queue_config_from_state)
            .unwrap_or_default();
        let tx_config = queues
            .get(QUEUE_TX)
            .map(Self::queue_config_from_state)
            .unwrap_or_default();
        drop(queues);
        *self.lock_state(&self.rx_processor, "rx_processor") = state
            .rx_processor
            .map(|p| VirtQueueProcessor::from_state(rx_config, p));
        *self.lock_state(&self.tx_processor, "tx_processor") = state
            .tx_processor
            .map(|p| VirtQueueProcessor::from_state(tx_config, p));
    }
}

impl Persist for Arc<VirtioNetMmio> {
    type State = VirtioNetMmioState;

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
        self.guest_features
            .store(state.guest_features, Ordering::Relaxed);
        *self.lock_state(&self.queues, "queues") = state.queues;
        self.activated.store(state.activated, Ordering::Relaxed);
        if state.activated {
            // The new tap starts with no offloads; give it the guest's.
            self.request_tap_offloads(state.guest_features);
        }
        self.interrupt_status
            .store(state.interrupt_status, Ordering::SeqCst);
        let queues = self.lock_state(&self.queues, "queues");
        let rx_config = queues
            .get(QUEUE_RX)
            .map(VirtioNetMmio::queue_config_from_state)
            .unwrap_or_default();
        let tx_config = queues
            .get(QUEUE_TX)
            .map(VirtioNetMmio::queue_config_from_state)
            .unwrap_or_default();
        drop(queues);
        *self.lock_state(&self.rx_processor, "rx_processor") = state
            .rx_processor
            .map(|p| VirtQueueProcessor::from_state(rx_config, p));
        *self.lock_state(&self.tx_processor, "tx_processor") = state
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
                    self.host_features()
                } else {
                    // virtio 1.x bit (32): VERSION_1. Bit 32 is in the high word.
                    1 << 0
                }
            }
            reg::QUEUE_NUM_MAX => NET_QUEUE_SIZE_MAX as u32,
            reg::STATUS => self.status.load(Ordering::Relaxed),
            reg::INTERRUPT_STATUS => self.interrupt_status.load(Ordering::SeqCst),
            reg::CONFIG_GENERATION => 0,
            reg::QUEUE_NUM => {
                let qs = self.lock_state(&self.queues, "queues");
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                qs.get(sel).map(|q| q.size as u32).unwrap_or(0)
            }
            reg::QUEUE_READY => {
                let qs = self.lock_state(&self.queues, "queues");
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
                if val & status_bits::DRIVER_OK != 0
                    && !self.activated.swap(true, Ordering::Relaxed)
                {
                    self.request_tap_offloads(self.guest_features.load(Ordering::Relaxed));
                }
            }
            reg::HOST_FEATURES_SEL => {
                self.host_features_sel.store(val, Ordering::Relaxed);
            }
            reg::GUEST_FEATURES_SEL => {
                self.guest_features_sel.store(val, Ordering::Relaxed);
            }
            reg::GUEST_FEATURES if self.guest_features_sel.load(Ordering::Relaxed) == 0 => {
                // A driver may only accept features the device offered.
                self.guest_features
                    .store(val & self.host_features(), Ordering::Relaxed);
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
                    return self.process_tx_queue().map(|_| ());
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
        let qs = self.lock_state(&self.queues, "queues");
        let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
        qs.get(sel).map(f).unwrap_or(0)
    }
    fn q_write<F: FnMut(&mut QueueState)>(&self, mut f: F) {
        let mut qs = self.lock_state(&self.queues, "queues");
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

    /// A non-blocking datagram socketpair stands in for a TAP with a
    /// virtio-net header: each read returns one whole frame, and an empty
    /// one returns EAGAIN.
    #[cfg(target_os = "linux")]
    fn frame_pipe() -> (RawFd, RawFd) {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors socketpair returns.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0);
        (fds[0], fds[1])
    }

    #[cfg(target_os = "linux")]
    fn close_fds(fds: &[RawFd]) {
        for fd in fds {
            // SAFETY: each descriptor came from frame_pipe and is closed once.
            unsafe { libc::close(*fd) };
        }
    }

    /// Activate `dev` with the guest accepting `accepted` (masked to what
    /// the device offers).
    fn negotiate(dev: &VirtioNetMmio, accepted: u32) {
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, accepted as u64, 4)
            .unwrap();
        dev.mmio_write(reg::STATUS, status_bits::DRIVER_OK as u64, 4)
            .unwrap();
    }

    #[test]
    fn offloads_are_offered_only_with_a_vnet_header() {
        let dev = VirtioNetMmio::new(6, [0; 6]);
        assert_eq!(dev.host_features() & net_features::OFFLOADS, 0);
        assert_ne!(dev.host_features() & net_features::RING_EVENT_IDX, 0);
        dev.set_vnet_hdr(true);
        assert_eq!(
            dev.host_features() & net_features::OFFLOADS,
            net_features::OFFLOADS
        );
        // A driver cannot accept what was not offered.
        let plain = VirtioNetMmio::new(6, [0; 6]);
        negotiate(&plain, u32::MAX);
        assert_eq!(plain.negotiated_offloads(), 0);
        assert_eq!(
            plain.guest_features.load(Ordering::Relaxed),
            plain.host_features()
        );
    }

    #[test]
    fn tap_offloads_follow_the_features_the_guest_accepted() {
        assert_eq!(tap_offloads_for(0), 0);
        assert_eq!(
            tap_offloads_for(net_features::GUEST_TSO4),
            0,
            "TSO needs csum"
        );
        assert_eq!(tap_offloads_for(net_features::GUEST_CSUM), 0x01);
        assert_eq!(
            tap_offloads_for(net_features::GUEST_CSUM | net_features::GUEST_TSO4),
            0x03
        );
        assert_eq!(tap_offloads_for(net_features::OFFLOADS), 0x07);

        let dev = VirtioNetMmio::new(6, [0; 6]);
        dev.set_vnet_hdr(true);
        assert_eq!(dev.take_offload_request(), None);
        negotiate(&dev, net_features::GUEST_CSUM | net_features::GUEST_TSO4);
        assert_eq!(dev.take_offload_request(), Some(0x03));
        assert_eq!(dev.take_offload_request(), None, "taken once");
        // A reset stops the tap sending offloaded frames.
        dev.mmio_write(reg::STATUS, 0, 4).unwrap();
        assert_eq!(dev.take_offload_request(), Some(0));

        // Without a vnet header nothing is ever requested.
        let plain = VirtioNetMmio::new(6, [0; 6]);
        negotiate(&plain, u32::MAX);
        assert_eq!(plain.take_offload_request(), None);
    }

    #[test]
    fn restore_asks_the_new_tap_for_the_guests_offloads() {
        let source = VirtioNetMmio::new(6, [0; 6]);
        source.set_vnet_hdr(true);
        negotiate(&source, net_features::OFFLOADS);
        let state = source.save();

        let mut restored = VirtioNetMmio::new(6, [0; 6]);
        restored.set_vnet_hdr(true);
        restored.restore(state);
        assert_eq!(restored.negotiated_offloads(), net_features::OFFLOADS);
        assert_eq!(restored.take_offload_request(), Some(0x07));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tx_with_a_vnet_header_sends_the_guest_header_with_the_frame() {
        use crate::virtio::vqueue::desc_flags;
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0; 6]);
        dev.set_guest_memory(mem.clone());
        dev.set_vnet_hdr(true);
        let (tap, peer) = frame_pipe();
        dev.set_tap_fd(tap, true);
        negotiate(&dev, net_features::OFFLOADS);
        const DESC: u64 = 0x10_0000;
        const AVAIL: u64 = 0x10_1000;
        const USED: u64 = 0x10_2000;
        const BUF: u64 = 0x10_3000;
        // A TSO frame: header with NEEDS_CSUM and GSO_TCPV4, then payload.
        let mut header = [0u8; VIRTIO_NET_HDR_LEN];
        header[0] = VIRTIO_NET_HDR_F_NEEDS_CSUM;
        header[1] = VIRTIO_NET_HDR_GSO_TCPV4;
        header[4] = 0xb4; // gso_size 1460 (little endian)
        header[5] = 0x05;
        let payload = vec![0x5au8; 3000];
        mem.write_slice(&header, GuestAddress(BUF)).unwrap();
        mem.write_slice(&payload, GuestAddress(BUF + 0x100))
            .unwrap();
        mem.write_obj(
            Descriptor {
                addr: BUF,
                len: VIRTIO_NET_HDR_LEN as u32,
                flags: desc_flags::NEXT,
                next: 1,
            },
            GuestAddress(DESC),
        )
        .unwrap();
        mem.write_obj(
            Descriptor {
                addr: BUF + 0x100,
                len: payload.len() as u32,
                flags: 0,
                next: 0,
            },
            GuestAddress(DESC + 16),
        )
        .unwrap();
        mem.write_obj(0u16, GuestAddress(AVAIL + 4)).unwrap();
        mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(AVAIL))
            .unwrap();
        configure_net_queue(&dev, 1, DESC, AVAIL, USED);

        assert_eq!(dev.process_tx_queue().unwrap(), 1);
        let mut wire = vec![0u8; 8192];
        // SAFETY: `wire` is writable for its length.
        let n = unsafe { libc::read(peer, wire.as_mut_ptr() as *mut libc::c_void, wire.len()) };
        assert_eq!(n as usize, VIRTIO_NET_HDR_LEN + payload.len());
        assert_eq!(&wire[..VIRTIO_NET_HDR_LEN], &header);
        assert_eq!(&wire[VIRTIO_NET_HDR_LEN..n as usize], &payload[..]);
        close_fds(&[tap, peer]);
    }

    /// RX queue of two-descriptor chains (12-byte header buffer, then a
    /// data buffer), the layout Linux uses for large receive buffers.
    #[cfg(target_os = "linux")]
    fn post_rx_chains(
        dev: &VirtioNetMmio,
        mem: &GuestMemoryMmap,
        chains: u16,
        data_len: u32,
    ) -> (u64, u64) {
        use crate::virtio::vqueue::desc_flags;
        const DESC: u64 = 0x20_0000;
        const AVAIL: u64 = 0x20_1000;
        const USED: u64 = 0x20_2000;
        const BUF: u64 = 0x21_0000;
        for chain in 0..chains as u64 {
            let head = (chain * 2) as u16;
            let buf = BUF + chain * 0x2_0000;
            mem.write_obj(
                Descriptor {
                    addr: buf,
                    len: VIRTIO_NET_HDR_LEN as u32,
                    flags: desc_flags::WRITE | desc_flags::NEXT,
                    next: head + 1,
                },
                GuestAddress(DESC + head as u64 * 16),
            )
            .unwrap();
            mem.write_obj(
                Descriptor {
                    addr: buf + 0x100,
                    len: data_len,
                    flags: desc_flags::WRITE,
                    next: 0,
                },
                GuestAddress(DESC + (head as u64 + 1) * 16),
            )
            .unwrap();
            mem.write_obj(head, GuestAddress(AVAIL + 4 + chain * 2))
                .unwrap();
        }
        mem.write_obj(
            AvailRing {
                flags: 0,
                idx: chains,
            },
            GuestAddress(AVAIL),
        )
        .unwrap();
        configure_net_queue(dev, 0, DESC, AVAIL, USED);
        (BUF, USED)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rx_from_a_vnet_tap_lands_in_guest_buffers_with_its_header() {
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0; 6]);
        dev.set_guest_memory(mem.clone());
        dev.set_vnet_hdr(true);
        let evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap();
        dev.set_irq_evt(evt.try_clone().unwrap());
        negotiate(&dev, net_features::OFFLOADS);
        let (buf, used) = post_rx_chains(&dev, &mem, 2, 4096);
        let (tap, peer) = frame_pipe();

        // DATA_VALID (the host already checked the checksum) passes through.
        let mut frame = vec![0u8; VIRTIO_NET_HDR_LEN];
        frame[0] = 2;
        frame.extend_from_slice(&[0xc3u8; 1000]);
        // SAFETY: `frame` is readable for its length.
        assert_eq!(
            unsafe { libc::write(peer, frame.as_ptr() as *const libc::c_void, frame.len()) },
            frame.len() as isize
        );

        assert_eq!(dev.rx_from_vnet_tap(tap, 64).unwrap(), Some((1, true)));
        assert_eq!(net_used_idx(&mem, used), 1);
        let entry: UsedElem = mem.read_obj(GuestAddress(used + 4)).unwrap();
        assert_eq!(entry.len as usize, frame.len());
        let mut header = [0u8; VIRTIO_NET_HDR_LEN];
        mem.read_slice(&mut header, GuestAddress(buf)).unwrap();
        assert_eq!(header[0], 2);
        assert_eq!(
            u16::from_le_bytes([header[10], header[11]]),
            1,
            "num_buffers"
        );
        let mut data = vec![0u8; 1000];
        mem.read_slice(&mut data, GuestAddress(buf + 0x100))
            .unwrap();
        assert!(data.iter().all(|b| *b == 0xc3));
        // One interrupt for the batch, raised by signal_rx.
        assert!(evt.read().is_err());
        dev.signal_rx().unwrap();
        assert_eq!(evt.read().unwrap(), 1);
        close_fds(&[tap, peer]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rx_drops_offloads_the_guest_did_not_accept() {
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0; 6]);
        dev.set_guest_memory(mem.clone());
        dev.set_vnet_hdr(true);
        // Checksum offload only: no TSO.
        negotiate(&dev, net_features::GUEST_CSUM);
        let (_, used) = post_rx_chains(&dev, &mem, 2, 4096);
        let (tap, peer) = frame_pipe();
        let mut frame = vec![0u8; VIRTIO_NET_HDR_LEN];
        frame[1] = VIRTIO_NET_HDR_GSO_TCPV4;
        frame.extend_from_slice(&[1u8; 100]);
        // SAFETY: `frame` is readable for its length.
        unsafe { libc::write(peer, frame.as_ptr() as *const libc::c_void, frame.len()) };
        assert_eq!(dev.rx_from_vnet_tap(tap, 64).unwrap(), Some((0, true)));
        let entry: UsedElem = mem.read_obj(GuestAddress(used + 4)).unwrap();
        assert_eq!(entry.len, 0, "the buffer returns empty");
        assert_eq!(dev.rx_packets.load(Ordering::Relaxed), 0);
        // The copying path refuses the same frame.
        assert!(!dev.inject_rx_vnet_frame(&frame).unwrap());
        close_fds(&[tap, peer]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn event_idx_interrupts_only_when_the_driver_asks() {
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0; 6]);
        dev.set_guest_memory(mem.clone());
        dev.set_vnet_hdr(true);
        let evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap();
        dev.set_irq_evt(evt.try_clone().unwrap());
        negotiate(&dev, net_features::OFFLOADS | net_features::RING_EVENT_IDX);
        let (_, used) = post_rx_chains(&dev, &mem, 4, 4096);
        const AVAIL: u64 = 0x20_1000;
        // used_event = 2: interrupt once the third used entry is added.
        mem.write_obj(2u16, GuestAddress(AVAIL + 4 + 2 * 16))
            .unwrap();
        let (tap, peer) = frame_pipe();
        let mut frame = vec![0u8; VIRTIO_NET_HDR_LEN];
        frame.extend_from_slice(&[7u8; 64]);
        let send = || {
            // SAFETY: `frame` is readable for its length.
            unsafe { libc::write(peer, frame.as_ptr() as *const libc::c_void, frame.len()) };
        };
        send();
        send();
        assert_eq!(dev.rx_from_vnet_tap(tap, 64).unwrap(), Some((2, true)));
        dev.signal_rx().unwrap();
        assert!(
            evt.read().is_err(),
            "used index 2 has not passed used_event 2"
        );
        send();
        assert_eq!(dev.rx_from_vnet_tap(tap, 64).unwrap(), Some((1, true)));
        dev.signal_rx().unwrap();
        assert_eq!(evt.read().unwrap(), 1, "used index 3 passed used_event");
        // The device asks to be kicked when the driver passes what it used.
        let avail_event: u16 = mem.read_obj(GuestAddress(used + 4 + 8 * 16)).unwrap();
        assert_eq!(avail_event, 3);
        close_fds(&[tap, peer]);
    }

    /// `count` single-descriptor receive buffers of `len` bytes, the layout
    /// Linux posts with mergeable buffers. Returns the buffer base (buffer
    /// `i` at `base + i * 0x1000`) and the used ring address.
    fn post_mergeable_buffers(
        dev: &VirtioNetMmio,
        mem: &GuestMemoryMmap,
        count: u16,
        len: u32,
    ) -> (u64, u64) {
        use crate::virtio::vqueue::desc_flags;
        const DESC: u64 = 0x20_0000;
        const AVAIL: u64 = 0x20_1000;
        const USED: u64 = 0x20_2000;
        const BUF: u64 = 0x21_0000;
        for i in 0..count as u64 {
            mem.write_obj(
                Descriptor {
                    addr: BUF + i * 0x1000,
                    len,
                    flags: desc_flags::WRITE,
                    next: 0,
                },
                GuestAddress(DESC + i * 16),
            )
            .unwrap();
            mem.write_obj(i as u16, GuestAddress(AVAIL + 4 + i * 2))
                .unwrap();
        }
        mem.write_obj(
            AvailRing {
                flags: 0,
                idx: count,
            },
            GuestAddress(AVAIL),
        )
        .unwrap();
        configure_net_queue(dev, 0, DESC, AVAIL, USED);
        (BUF, USED)
    }

    fn publish_rx_buffers(mem: &GuestMemoryMmap, count: u16) {
        mem.write_obj(
            AvailRing {
                flags: 0,
                idx: count,
            },
            GuestAddress(0x20_1000),
        )
        .unwrap();
    }

    fn used_entry(mem: &GuestMemoryMmap, used: u64, slot: u64) -> (u32, u32) {
        let entry: UsedElem = mem.read_obj(GuestAddress(used + 4 + slot * 8)).unwrap();
        (entry.id, entry.len)
    }

    fn num_buffers(mem: &GuestMemoryMmap, buf: u64) -> u16 {
        mem.read_obj(GuestAddress(buf + VIRTIO_NET_HDR_NUM_BUFFERS_OFF as u64))
            .unwrap()
    }

    #[test]
    fn mergeable_buffers_are_offered_and_negotiated() {
        let dev = VirtioNetMmio::new(6, [0; 6]);
        assert_ne!(dev.host_features() & net_features::MRG_RXBUF, 0);
        negotiate(&dev, net_features::MRG_RXBUF);
        assert!(dev.mergeable());
        negotiate(&dev, 0);
        assert!(!dev.mergeable());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mergeable_rx_from_a_tap_spreads_a_frame_over_buffers() {
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0; 6]);
        dev.set_guest_memory(mem.clone());
        dev.set_vnet_hdr(true);
        negotiate(
            &dev,
            net_features::GUEST_CSUM | net_features::MRG_RXBUF | net_features::RING_EVENT_IDX,
        );
        let (buf, used) = post_mergeable_buffers(&dev, &mem, 4, 1024);
        let (tap, peer) = frame_pipe();
        let mut frame = vec![0u8; VIRTIO_NET_HDR_LEN];
        frame.extend((0..1500u32).map(|i| i as u8));
        // SAFETY: `frame` is readable for its length.
        unsafe { libc::write(peer, frame.as_ptr() as *const libc::c_void, frame.len()) };

        assert_eq!(dev.rx_from_vnet_tap(tap, 64).unwrap(), Some((1, true)));
        assert_eq!(net_used_idx(&mem, used), 2);
        assert_eq!(used_entry(&mem, used, 0), (0, 1024));
        assert_eq!(used_entry(&mem, used, 1), (1, 1512 - 1024));
        assert_eq!(num_buffers(&mem, buf), 2);
        let mut got = vec![0u8; 1512];
        mem.read_slice(&mut got[..1024], GuestAddress(buf)).unwrap();
        mem.read_slice(&mut got[1024..], GuestAddress(buf + 0x1000))
            .unwrap();
        assert_eq!(&got[VIRTIO_NET_HDR_LEN..], &frame[VIRTIO_NET_HDR_LEN..]);
        assert_eq!(dev.rx_bytes.load(Ordering::Relaxed), 1500);
        close_fds(&[tap, peer]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mergeable_rx_waits_until_buffers_hold_the_largest_frame() {
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0; 6]);
        dev.set_guest_memory(mem.clone());
        dev.set_vnet_hdr(true);
        negotiate(
            &dev,
            net_features::GUEST_CSUM | net_features::MRG_RXBUF | net_features::RING_EVENT_IDX,
        );
        let (buf, used) = post_mergeable_buffers(&dev, &mem, 4, 1024);
        publish_rx_buffers(&mem, 1);
        let (tap, peer) = frame_pipe();
        let mut frame = vec![0u8; VIRTIO_NET_HDR_LEN];
        frame.extend_from_slice(&[9u8; 100]);
        // SAFETY: `frame` is readable for its length.
        unsafe { libc::write(peer, frame.as_ptr() as *const libc::c_void, frame.len()) };

        // 1024 bytes cannot hold an MTU frame: the frame stays in the tap
        // and the driver is asked to kick once it posts past buffer 1.
        assert_eq!(dev.rx_from_vnet_tap(tap, 64).unwrap(), Some((0, false)));
        assert_eq!(net_used_idx(&mem, used), 0);
        let avail_event: u16 = mem.read_obj(GuestAddress(used + 4 + 16 * 8)).unwrap();
        assert_eq!(avail_event, 1);

        publish_rx_buffers(&mem, 2);
        // Delivered; the one buffer left is too small to read the next
        // frame into, so the tap is not read again (nor known to be empty).
        assert_eq!(dev.rx_from_vnet_tap(tap, 64).unwrap(), Some((1, false)));
        assert_eq!(used_entry(&mem, used, 0), (0, 112));
        assert_eq!(num_buffers(&mem, buf), 1);

        // With receive TSO accepted the bar is a 64 KiB frame.
        let tso = VirtioNetMmio::new(6, [0; 6]);
        tso.set_guest_memory(mem.clone());
        tso.set_vnet_hdr(true);
        negotiate(&tso, net_features::OFFLOADS | net_features::MRG_RXBUF);
        post_mergeable_buffers(&tso, &mem, 16, 1024);
        // SAFETY: `frame` is readable for its length.
        unsafe { libc::write(peer, frame.as_ptr() as *const libc::c_void, frame.len()) };
        assert_eq!(tso.rx_from_vnet_tap(tap, 64).unwrap(), Some((0, false)));
        close_fds(&[tap, peer]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mergeable_rx_keeps_buffers_for_a_frame_it_drops() {
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0; 6]);
        dev.set_guest_memory(mem.clone());
        dev.set_vnet_hdr(true);
        negotiate(&dev, net_features::GUEST_CSUM | net_features::MRG_RXBUF);
        let (_, used) = post_mergeable_buffers(&dev, &mem, 4, 1024);
        let (tap, peer) = frame_pipe();
        let mut tso_frame = vec![0u8; VIRTIO_NET_HDR_LEN];
        tso_frame[1] = VIRTIO_NET_HDR_GSO_TCPV4;
        tso_frame.extend_from_slice(&[1u8; 100]);
        let mut plain = vec![0u8; VIRTIO_NET_HDR_LEN];
        plain.extend_from_slice(&[2u8; 100]);
        for frame in [&tso_frame, &plain] {
            // SAFETY: `frame` is readable for its length.
            unsafe { libc::write(peer, frame.as_ptr() as *const libc::c_void, frame.len()) };
        }
        assert_eq!(dev.rx_from_vnet_tap(tap, 64).unwrap(), Some((1, true)));
        assert_eq!(net_used_idx(&mem, used), 1, "only the accepted frame");
        assert_eq!(used_entry(&mem, used, 0), (0, 112));
        close_fds(&[tap, peer]);
    }

    #[test]
    fn mergeable_copy_path_spreads_a_packet_or_waits() {
        let mem = new_mem();
        let dev = VirtioNetMmio::new(6, [0; 6]);
        dev.set_guest_memory(mem.clone());
        negotiate(&dev, net_features::MRG_RXBUF);
        let (buf, used) = post_mergeable_buffers(&dev, &mem, 3, 1024);
        publish_rx_buffers(&mem, 1);
        let packet: Vec<u8> = (0..1500u32).map(|i| (i * 7) as u8).collect();
        assert!(
            !dev.inject_rx_packet(&packet).unwrap(),
            "1024 bytes cannot hold it"
        );
        assert_eq!(net_used_idx(&mem, used), 0);

        publish_rx_buffers(&mem, 3);
        assert!(dev.inject_rx_packet(&packet).unwrap());
        assert_eq!(net_used_idx(&mem, used), 2);
        assert_eq!(used_entry(&mem, used, 0), (0, 1024));
        assert_eq!(used_entry(&mem, used, 1), (1, 1512 - 1024));
        assert_eq!(num_buffers(&mem, buf), 2);
        let mut got = vec![0u8; 1512];
        mem.read_slice(&mut got[..1024], GuestAddress(buf)).unwrap();
        mem.read_slice(&mut got[1024..], GuestAddress(buf + 0x1000))
            .unwrap();
        assert_eq!(&got[VIRTIO_NET_HDR_LEN..], &packet[..]);
    }

    #[test]
    fn worker_failure_marks_transport_failed_and_unsnapshotable() {
        let dev = VirtioNetMmio::new(6, [0x02, 0, 0, 0, 0, 1]);

        dev.fail_worker("test worker exit");

        assert_ne!(dev.current_status() & status_bits::FAILED, 0);
        assert!(Persist::try_save(&dev).is_err());
    }

    #[test]
    fn poisoned_guest_mem_lock_marks_device_failed_without_panicking() {
        let dev = Arc::new(VirtioNetMmio::new(6, [0xAA; 6]));
        let poisoned = Arc::clone(&dev);
        assert!(std::thread::spawn(move || {
            let _guard = poisoned.guest_mem.lock().unwrap();
            panic!("poison guest_mem");
        })
        .join()
        .is_err());

        dev.set_guest_memory(new_mem());
        assert_ne!(dev.current_status() & status_bits::FAILED, 0);
        assert!(dev.process_tx_queue().is_err());
        assert!(dev.inject_rx_packet(&[0u8; 64]).is_err());
        assert!(Persist::try_save(dev.as_ref()).is_err());
        assert!(dev
            .mmio_write(reg::QUEUE_NOTIFY, QUEUE_TX as u64, 4)
            .is_err());

        dev.mmio_write(reg::STATUS, 0, 4).unwrap();
        assert_eq!(dev.current_status(), 0);
        assert!(Persist::try_save(dev.as_ref()).is_ok());
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

        dev.process_tx_queue().unwrap();
        assert_eq!(net_used_idx(&mem, USED), 1);

        dev.process_tx_queue().unwrap();
        assert_eq!(net_used_idx(&mem, USED), 1);

        clock.advance(1_000_000_000);
        dev.process_tx_queue().unwrap();
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

        dev.process_tx_queue().unwrap();
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

        restored.process_tx_queue().unwrap();
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
        assert!(dev.inject_rx_packet(packet).unwrap());
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
        assert!(!dev.inject_rx_packet(packet).unwrap());
        assert_eq!(net_used_idx(&mem, USED), 0);

        clock.advance(1_000_000_000);
        assert!(dev.inject_rx_packet(packet).unwrap());
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
        dev.trigger_interrupt().unwrap();

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

    #[cfg(target_os = "linux")]
    #[test]
    fn restored_pending_interrupt_is_raised_again() {
        let dev = VirtioNetMmio::new(6, [0; 6]);
        let evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).expect("evt");
        dev.set_irq_evt(evt.try_clone().expect("clone evt"));
        dev.reassert_pending_interrupt();
        assert!(evt.read().is_err(), "nothing pending, nothing raised");
        dev.interrupt_status.store(1, Ordering::SeqCst);
        dev.reassert_pending_interrupt();
        assert_eq!(evt.read().expect("interrupt raised"), 1);
    }
}
