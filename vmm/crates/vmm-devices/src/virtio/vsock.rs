//! virtio-vsock MMIO transport, host-side stream bridge.
//!
//! The device implements the virtio-mmio register contract and the packet core
//! needed for a single host-backed stream. A controller can attach a Unix socket
//! backend with [`VirtioVsockMmio::connect_uds`] or provide a custom connector
//! with [`VirtioVsockMmio::set_host_listener`].
//! Scope: this is the device core. It does not spawn a UDS polling loop or use
//! the EVENT queue yet; controller wiring can drive [`VirtioVsockMmio::pump_host_streams`].

use crate::bus::{MmioDevice, MmioError, MmioReadResult, MmioWriteResult};
use crate::persist::{Persist, PersistError};
use crate::virtio::blk_transport::status_bits;
use crate::virtio::regs::{reg, MAGIC};
use crate::virtio::vqueue::{is_valid_queue_size, QueueConfig, VirtQueueProcessor, MAX_QUEUE_SIZE};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmm_memory_backend::dirty::SoftwareDirtyBitmap;

pub const DEVICE_ID_VSOCK: u32 = 19;
pub const VSOCK_HOST_CID: u64 = 2;
pub const VSOCK_HEADER_LEN: usize = 44;

const QUEUE_RX: usize = 0;
const QUEUE_TX: usize = 1;
const QUEUE_EVENT: usize = 2;
const QUEUE_COUNT: usize = 3;
const QUEUE_SIZE_MAX: u32 = MAX_QUEUE_SIZE as u32;
/// Cap on buffered host->guest packets. A hostile guest can drive RST/RESPONSE
/// replies (via bad TX packets) while never posting RX descriptors to drain
/// them; without a cap `pending_rx` grows unbounded and OOMs the VMM. When full
/// we drop new packets — the peer's vsock stack retransmits / tears down.
const MAX_PENDING_RX: usize = 4096;
/// Maximum active stream connections for this device. One vsock device maps to
/// one guest, so this bounds guest-controlled REQUEST fan-out.
const MAX_CONNECTIONS: usize = 1024;
const VIRTIO_MMIO_INT_VRING: u32 = 0x01;
const VSOCK_FEATURES_LOW: u32 = 0;
const VSOCK_FEATURES_HIGH: u32 = 1;
const DEFAULT_BUF_ALLOC: u32 = 256 * 1024;
const MAX_PACKET_BYTES: usize = 64 * 1024;
const HOST_READ_CHUNK: usize = 4096;
const HOST_EPHEMERAL_PORT_START: u32 = 49152;
const HOST_EPHEMERAL_PORT_END: u32 = 65535;

pub mod packet_type {
    pub const STREAM: u16 = 1;
}

pub mod op {
    pub const INVALID: u16 = 0;
    pub const REQUEST: u16 = 1;
    pub const RESPONSE: u16 = 2;
    pub const RST: u16 = 3;
    pub const SHUTDOWN: u16 = 4;
    pub const RW: u16 = 5;
    pub const CREDIT_UPDATE: u16 = 6;
    pub const CREDIT_REQUEST: u16 = 7;
}

pub mod shutdown_flags {
    pub const RCV: u32 = 1;
    pub const SEND: u32 = 2;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VsockPacketHeader {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    pub type_: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

impl VsockPacketHeader {
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < VSOCK_HEADER_LEN {
            return None;
        }
        Some(Self {
            src_cid: u64::from_le_bytes(buf[0..8].try_into().ok()?),
            dst_cid: u64::from_le_bytes(buf[8..16].try_into().ok()?),
            src_port: u32::from_le_bytes(buf[16..20].try_into().ok()?),
            dst_port: u32::from_le_bytes(buf[20..24].try_into().ok()?),
            len: u32::from_le_bytes(buf[24..28].try_into().ok()?),
            type_: u16::from_le_bytes(buf[28..30].try_into().ok()?),
            op: u16::from_le_bytes(buf[30..32].try_into().ok()?),
            flags: u32::from_le_bytes(buf[32..36].try_into().ok()?),
            buf_alloc: u32::from_le_bytes(buf[36..40].try_into().ok()?),
            fwd_cnt: u32::from_le_bytes(buf[40..44].try_into().ok()?),
        })
    }

    pub fn to_bytes(self) -> [u8; VSOCK_HEADER_LEN] {
        let mut out = [0u8; VSOCK_HEADER_LEN];
        out[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        out[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        out[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        out[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        out[24..28].copy_from_slice(&self.len.to_le_bytes());
        out[28..30].copy_from_slice(&self.type_.to_le_bytes());
        out[30..32].copy_from_slice(&self.op.to_le_bytes());
        out[32..36].copy_from_slice(&self.flags.to_le_bytes());
        out[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        out[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VsockPacket {
    pub header: VsockPacketHeader,
    pub data: Vec<u8>,
}

impl VsockPacket {
    pub fn new(mut header: VsockPacketHeader, data: Vec<u8>) -> Self {
        header.len = data.len() as u32;
        Self { header, data }
    }

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        let header = VsockPacketHeader::from_bytes(buf)?;
        let data_len = header.len as usize;
        let packet_len = VSOCK_HEADER_LEN.checked_add(data_len)?;
        if data_len > MAX_PACKET_BYTES || buf.len() < packet_len {
            return None;
        }
        Some(Self {
            header,
            data: buf[VSOCK_HEADER_LEN..packet_len].to_vec(),
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut header = self.header;
        header.len = self.data.len() as u32;
        let packet_len = VSOCK_HEADER_LEN
            .checked_add(self.data.len())
            .unwrap_or(VSOCK_HEADER_LEN);
        let mut out = Vec::with_capacity(packet_len);
        out.extend_from_slice(&header.to_bytes());
        out.extend_from_slice(&self.data);
        out
    }
}

pub trait HostVsockStream: Read + Write + Send {}
impl<T: Read + Write + Send> HostVsockStream for T {}

pub trait HostVsockListener: Send + Sync {
    fn connect(&self, port: u32) -> io::Result<Box<dyn HostVsockStream>>;
}

#[cfg(unix)]
struct UdsHostListener {
    path: PathBuf,
}

#[cfg(unix)]
impl HostVsockListener for UdsHostListener {
    fn connect(&self, _port: u32) -> io::Result<Box<dyn HostVsockStream>> {
        let stream = UnixStream::connect(&self.path)?;
        stream.set_nonblocking(true)?;
        Ok(Box::new(stream))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct QueueState {
    size: u16,
    desc_table_addr: u64,
    avail_ring_addr: u64,
    used_ring_addr: u64,
    ready: bool,
    last_avail_idx: u16,
    last_used_idx: u16,
}

impl QueueState {
    fn valid_size(&self) -> bool {
        is_valid_queue_size(self.size, MAX_QUEUE_SIZE)
    }

    fn set_size(&mut self, size: u32) {
        let Ok(size) = u16::try_from(size) else {
            log::warn!("virtio-vsock: QUEUE_NUM {size} exceeds u16 — rejecting");
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
                "virtio-vsock: invalid QUEUE_NUM {size} (must be power-of-two <= {MAX_QUEUE_SIZE})"
            );
            self.size = 0;
            self.ready = false;
        }
    }

    fn set_ready(&mut self, ready: bool) {
        if ready && !self.valid_size() {
            log::warn!(
                "virtio-vsock: QUEUE_READY ignored for invalid QUEUE_NUM {}",
                self.size
            );
            self.ready = false;
        } else {
            self.ready = ready;
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VsockConnectionState {
    pub guest_cid: u64,
    pub guest_port: u32,
    pub host_port: u32,
    pub fwd_cnt: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VirtioVsockMmioState {
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
    pub connections: Vec<VsockConnectionState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ConnectionKey {
    guest_cid: u64,
    guest_port: u32,
    host_port: u32,
}

impl VsockConnectionState {
    fn key(self) -> ConnectionKey {
        ConnectionKey {
            guest_cid: self.guest_cid,
            guest_port: self.guest_port,
            host_port: self.host_port,
        }
    }
}

struct Connection {
    stream: Box<dyn HostVsockStream>,
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    fwd_cnt: u32,
    established: bool,
}

pub struct VirtioVsockMmio {
    pub irq: u32,
    pub device_id: u32,
    pub vendor_id: u32,
    pub version: u32,
    pub guest_cid: u64,
    status: AtomicU32,
    queue_sel: AtomicU32,
    host_features_sel: AtomicU32,
    guest_features_sel: AtomicU32,
    guest_features_low: AtomicU32,
    guest_features_high: AtomicU32,
    queues: Mutex<Vec<QueueState>>,
    guest_mem: Mutex<Option<Arc<GuestMemoryMmap>>>,
    host_dirty: Mutex<Option<SoftwareDirtyBitmap>>,
    activated: AtomicBool,
    rx_processor: Mutex<Option<VirtQueueProcessor>>,
    tx_processor: Mutex<Option<VirtQueueProcessor>>,
    interrupt_status: AtomicU32,
    host_listener: Mutex<Option<Arc<dyn HostVsockListener>>>,
    connections: Mutex<HashMap<ConnectionKey, Connection>>,
    next_host_port: AtomicU32,
    pending_rx: Mutex<VecDeque<VsockPacket>>,
    #[cfg(target_os = "linux")]
    irq_evt: Mutex<Option<vmm_sys_util::eventfd::EventFd>>,
    pub status_writes: AtomicU64,
    pub notify_count: AtomicU64,
    pub tx_packets: AtomicU64,
    pub rx_packets: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub rx_bytes: AtomicU64,
}

impl VirtioVsockMmio {
    fn fail_device(&self, context: &str) {
        log::error!("virtio-vsock: {context}");
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
            PersistError::Unavailable(format!("virtio-vsock {name} lock is poisoned"))
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

    pub fn new(irq: u32, guest_cid: u64) -> Self {
        Self {
            irq,
            device_id: DEVICE_ID_VSOCK,
            vendor_id: 0,
            version: 2,
            guest_cid,
            status: AtomicU32::new(0),
            queue_sel: AtomicU32::new(0),
            host_features_sel: AtomicU32::new(0),
            guest_features_sel: AtomicU32::new(0),
            guest_features_low: AtomicU32::new(0),
            guest_features_high: AtomicU32::new(0),
            queues: Mutex::new(vec![QueueState::default(); QUEUE_COUNT]),
            guest_mem: Mutex::new(None),
            host_dirty: Mutex::new(None),
            activated: AtomicBool::new(false),
            rx_processor: Mutex::new(None),
            tx_processor: Mutex::new(None),
            interrupt_status: AtomicU32::new(0),
            host_listener: Mutex::new(None),
            connections: Mutex::new(HashMap::new()),
            next_host_port: AtomicU32::new(HOST_EPHEMERAL_PORT_START),
            pending_rx: Mutex::new(VecDeque::new()),
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

    pub fn set_host_listener(&self, listener: Arc<dyn HostVsockListener>) {
        *self.lock_state(&self.host_listener, "host_listener") = Some(listener);
    }

    pub fn active_connection_states(&self) -> Vec<VsockConnectionState> {
        self.lock_state(&self.connections, "connections")
            .iter()
            .map(|(key, conn)| VsockConnectionState {
                guest_cid: key.guest_cid,
                guest_port: key.guest_port,
                host_port: key.host_port,
                fwd_cnt: conn.fwd_cnt,
            })
            .collect()
    }

    #[cfg(unix)]
    pub fn connect_guest_stream(&self, guest_port: u32) -> io::Result<UnixStream> {
        self.ensure_operational()
            .map_err(|_| io::Error::other("virtio-vsock requires reset"))?;
        if self
            .try_lock_state(&self.connections, "connections")
            .map_err(|_| io::Error::other("virtio-vsock requires reset"))?
            .len()
            >= MAX_CONNECTIONS
        {
            return Err(connection_cap_error());
        }

        let (host_stream, device_stream) = UnixStream::pair()?;
        host_stream.set_nonblocking(false)?;
        device_stream.set_nonblocking(true)?;

        let (_key, host_port) = {
            let mut connections = self
                .try_lock_state(&self.connections, "connections")
                .map_err(|_| io::Error::other("virtio-vsock requires reset"))?;
            if connections.len() >= MAX_CONNECTIONS {
                return Err(connection_cap_error());
            }

            let span = HOST_EPHEMERAL_PORT_END - HOST_EPHEMERAL_PORT_START + 1;
            let mut selected = None;
            for _ in 0..span {
                let seq = self.next_host_port.fetch_add(1, Ordering::Relaxed);
                let port = HOST_EPHEMERAL_PORT_START
                    + (seq.wrapping_sub(HOST_EPHEMERAL_PORT_START) % span);
                let key = ConnectionKey {
                    guest_cid: self.guest_cid,
                    guest_port,
                    host_port: port,
                };
                if connections.contains_key(&key) {
                    continue;
                }
                selected = Some((key, port));
                break;
            }
            let (key, port) = selected
                .ok_or_else(|| io::Error::new(ErrorKind::AddrNotAvailable, "no vsock ports"))?;
            connections.insert(
                key,
                Connection {
                    stream: Box::new(device_stream),
                    peer_buf_alloc: DEFAULT_BUF_ALLOC,
                    peer_fwd_cnt: 0,
                    fwd_cnt: 0,
                    established: false,
                },
            );
            (key, port)
        };

        self.enqueue_rx(VsockPacket::new(
            VsockPacketHeader {
                src_cid: VSOCK_HOST_CID,
                dst_cid: self.guest_cid,
                src_port: host_port,
                dst_port: guest_port,
                len: 0,
                type_: packet_type::STREAM,
                op: op::REQUEST,
                flags: 0,
                buf_alloc: DEFAULT_BUF_ALLOC,
                fwd_cnt: 0,
            },
            Vec::new(),
        ))
        .map_err(|_| io::Error::other("virtio-vsock requires reset"))?;
        self.process_rx_queue()
            .map_err(|_| io::Error::other("virtio-vsock requires reset"))?;
        log::info!(
            "vsock: host→guest connect REQUEST host_port={host_port} → guest_port={guest_port}"
        );

        Ok(host_stream)
    }

    pub fn restore_transport_state(&self, state: &VirtioVsockMmioState) {
        self.apply_transport_state(state);
    }

    pub fn reset_restored_connections(
        &self,
        connections: &[VsockConnectionState],
    ) -> Result<usize, MmioError> {
        self.ensure_operational()?;
        if connections.is_empty() {
            return Ok(0);
        }

        {
            let mut live = self.try_lock_state(&self.connections, "connections")?;
            for conn in connections {
                live.remove(&conn.key());
            }
        }

        for conn in connections {
            self.enqueue_rx(host_packet(
                conn.key(),
                op::RST,
                0,
                Vec::new(),
                conn.fwd_cnt,
            ))?;
        }
        self.process_rx_queue()?;
        Ok(connections.len())
    }

    #[cfg(unix)]
    pub fn connect_uds<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        self.set_host_listener(Arc::new(UdsHostListener {
            path: path.as_ref().to_path_buf(),
        }));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn set_irq_evt(&self, evt: vmm_sys_util::eventfd::EventFd) {
        *self.lock_state(&self.irq_evt, "irq_evt") = Some(evt);
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

    fn queue_indices(&self, idx: usize) -> Result<(u16, u16), MmioError> {
        let qs = self.try_lock_state(&self.queues, "queues")?;
        Ok(qs
            .get(idx)
            .map(|q| (q.last_avail_idx, q.last_used_idx))
            .unwrap_or_default())
    }

    fn apply_transport_state(&self, state: &VirtioVsockMmioState) {
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
        *self.lock_state(&self.queues, "queues") = state.queues.clone();
        *self.lock_state(&self.rx_processor, "rx_processor") = None;
        *self.lock_state(&self.tx_processor, "tx_processor") = None;
        self.lock_state(&self.pending_rx, "pending_rx").clear();
        self.lock_state(&self.connections, "connections").clear();
        self.activated.store(state.activated, Ordering::Relaxed);
        self.interrupt_status
            .store(state.interrupt_status, Ordering::SeqCst);
    }

    pub fn process_tx_queue(&self) -> Result<usize, MmioError> {
        self.ensure_operational()?;
        let mem = match self.try_lock_state(&self.guest_mem, "guest_mem")?.clone() {
            Some(mem) => mem,
            None => return Ok(0),
        };
        let dirty = self.try_lock_state(&self.host_dirty, "host_dirty")?.clone();
        let cfg = match self.queue_config(QUEUE_TX)? {
            Some(cfg) => cfg,
            None => return Ok(0),
        };

        let mut proc_guard = self.try_lock_state(&self.tx_processor, "tx_processor")?;
        if proc_guard.is_none() {
            let (last_avail_idx, last_used_idx) = self.queue_indices(QUEUE_TX)?;
            *proc_guard = Some(VirtQueueProcessor::new_with_indices(
                cfg,
                last_avail_idx,
                last_used_idx,
            ));
        } else {
            proc_guard.as_mut().unwrap().update_config(cfg);
        }

        let mut processing_failed = false;
        let processed = proc_guard
            .as_mut()
            .unwrap()
            .process_queue_descriptors_dirty(&mem, dirty.as_ref(), |readable, _writable| {
                if self.is_failed() {
                    return None;
                }
                if let Some(packet) = read_packet_from_descs(&mem, readable) {
                    self.tx_packets.fetch_add(1, Ordering::Relaxed);
                    self.tx_bytes
                        .fetch_add(packet.data.len() as u64, Ordering::Relaxed);
                    if self.handle_tx_packet(packet).is_err() {
                        processing_failed = true;
                        return None;
                    }
                }
                Some(0)
            });

        if processed > 0 {
            self.trigger_interrupt()?;
            self.process_rx_queue()?;
        }
        if processing_failed || self.is_failed() {
            Err(MmioError::Device)
        } else {
            Ok(processed)
        }
    }

    pub fn process_rx_queue(&self) -> Result<usize, MmioError> {
        self.ensure_operational()?;
        let mem = match self.try_lock_state(&self.guest_mem, "guest_mem")?.clone() {
            Some(mem) => mem,
            None => return Ok(0),
        };
        let dirty = self.try_lock_state(&self.host_dirty, "host_dirty")?.clone();
        let cfg = match self.queue_config(QUEUE_RX)? {
            Some(cfg) => cfg,
            None => {
                let pending = self.try_lock_state(&self.pending_rx, "pending_rx")?.len();
                if pending > 0 {
                    log::debug!("vsock rx: RX queue not ready but {pending} pending packets");
                }
                return Ok(0);
            }
        };

        let mut proc_guard = self.try_lock_state(&self.rx_processor, "rx_processor")?;
        if proc_guard.is_none() {
            let (last_avail_idx, last_used_idx) = self.queue_indices(QUEUE_RX)?;
            *proc_guard = Some(VirtQueueProcessor::new_with_indices(
                cfg,
                last_avail_idx,
                last_used_idx,
            ));
        } else {
            proc_guard.as_mut().unwrap().update_config(cfg);
        }

        let mut processing_failed = false;
        let processed = proc_guard
            .as_mut()
            .unwrap()
            .process_queue_descriptors_dirty(&mem, dirty.as_ref(), |_readable, writable| {
                let packet = match self.try_lock_state(&self.pending_rx, "pending_rx") {
                    Ok(pending) => pending.front().cloned(),
                    Err(_) => {
                        processing_failed = true;
                        return None;
                    }
                };
                let packet = match packet {
                    Some(packet) => packet,
                    None => return None,
                };
                let bytes = packet.to_bytes();
                let Some(cap) = writable
                    .iter()
                    .try_fold(0usize, |acc, (_, len)| acc.checked_add(*len as usize))
                else {
                    return Some(0);
                };
                if cap < bytes.len() {
                    return Some(0);
                }
                if !write_desc_chain(&mem, dirty.as_ref(), writable, &bytes) {
                    return Some(0);
                }
                match self.try_lock_state(&self.pending_rx, "pending_rx") {
                    Ok(mut pending) => {
                        pending.pop_front();
                    }
                    Err(_) => {
                        processing_failed = true;
                        return None;
                    }
                }
                self.rx_packets.fetch_add(1, Ordering::Relaxed);
                self.rx_bytes
                    .fetch_add(packet.data.len() as u64, Ordering::Relaxed);
                Some(bytes.len() as u32)
            });

        if processed > 0 {
            self.trigger_interrupt()?;
        }
        let pending = self.try_lock_state(&self.pending_rx, "pending_rx")?.len();
        if pending > 0 {
            log::debug!(
                "vsock rx: delivered {processed}, still {pending} pending (RX bufs short?)"
            );
        } else if processed > 0 {
            log::debug!("vsock rx: delivered {processed} packet(s) to guest");
        }
        if processing_failed || self.is_failed() {
            Err(MmioError::Device)
        } else {
            Ok(processed)
        }
    }

    pub fn pump_host_streams(&self) -> Result<usize, MmioError> {
        self.ensure_operational()?;
        let mut packets = Vec::new();
        let mut closed = Vec::new();
        {
            let mut connections = self.try_lock_state(&self.connections, "connections")?;
            for (key, conn) in connections.iter_mut() {
                if !conn.established {
                    continue;
                }
                let mut buf = [0u8; HOST_READ_CHUNK];
                match conn.stream.read(&mut buf) {
                    Ok(0) => {
                        packets.push(host_packet(
                            *key,
                            op::SHUTDOWN,
                            shutdown_flags::RCV,
                            Vec::new(),
                            conn.fwd_cnt,
                        ));
                        closed.push(*key);
                    }
                    Ok(n) => {
                        // NB: fwd_cnt must be the count of bytes we've received
                        // from the guest (updated in handle_rw), NOT host→guest
                        // bytes. Sending host→guest bytes here made fwd_cnt exceed
                        // the guest's tx_cnt, an impossible credit value that made
                        // the guest reject the packet. Leave fwd_cnt as-is.
                        let fwd_cnt = conn.fwd_cnt;
                        log::debug!("vsock: host→guest {n} bytes (enqueue RW)");
                        packets.push(host_packet(*key, op::RW, 0, buf[..n].to_vec(), fwd_cnt));
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                    Err(err) if err.kind() == ErrorKind::Interrupted => {}
                    Err(_) => {
                        packets.push(host_packet(*key, op::RST, 0, Vec::new(), conn.fwd_cnt));
                        closed.push(*key);
                    }
                }
            }
            for key in closed {
                connections.remove(&key);
            }
        }

        let queued = packets.len();
        for packet in packets {
            self.enqueue_rx(packet)?;
        }
        if queued > 0 {
            self.process_rx_queue()?;
        }
        if self.is_failed() {
            Err(MmioError::Device)
        } else {
            Ok(queued)
        }
    }

    fn enqueue_rx(&self, packet: VsockPacket) -> MmioWriteResult {
        let mut q = self.try_lock_state(&self.pending_rx, "pending_rx")?;
        if q.len() >= MAX_PENDING_RX {
            // Backpressure: drop rather than grow without bound. The guest is
            // not draining RX (no descriptors posted), so buffering more only
            // risks host OOM.
            log::warn!("vsock: pending_rx full ({MAX_PENDING_RX}); dropping packet");
            return Ok(());
        }
        q.push_back(packet);
        Ok(())
    }

    fn handle_tx_packet(&self, packet: VsockPacket) -> MmioWriteResult {
        let header = packet.header;
        if header.type_ != packet_type::STREAM {
            return self.enqueue_rx(self.reply_packet(&header, op::RST, 0, Vec::new(), 0));
        }

        match header.op {
            op::REQUEST => self.handle_request(header),
            op::RESPONSE => self.handle_response(header),
            op::RW => self.handle_rw(header, &packet.data),
            op::CREDIT_UPDATE => self.handle_credit_update(header),
            op::CREDIT_REQUEST => self.handle_credit_request(header),
            op::SHUTDOWN => self.handle_shutdown(header),
            op::RST => self.handle_rst(header),
            op::INVALID => Ok(()),
            _ => self.enqueue_rx(self.reply_packet(&header, op::RST, 0, Vec::new(), 0)),
        }
    }

    fn handle_request(&self, header: VsockPacketHeader) -> MmioWriteResult {
        if header.dst_cid != VSOCK_HOST_CID {
            return self.enqueue_rx(self.reply_packet(&header, op::RST, 0, Vec::new(), 0));
        }

        let listener = self
            .try_lock_state(&self.host_listener, "host_listener")?
            .clone();
        let Some(listener) = listener else {
            return self.enqueue_rx(self.reply_packet(&header, op::RST, 0, Vec::new(), 0));
        };

        let key = key_from_guest(&header);
        {
            let connections = self.try_lock_state(&self.connections, "connections")?;
            if connection_insert_would_exceed_cap(&connections, &key) {
                log::warn!(
                    "vsock: connection cap ({MAX_CONNECTIONS}) reached; rejecting REQUEST guest_port={} host_port={}",
                    header.src_port,
                    header.dst_port
                );
                return self.enqueue_rx(self.reply_packet(&header, op::RST, 0, Vec::new(), 0));
            }
        }

        match listener.connect(header.dst_port) {
            Ok(stream) => {
                let mut connections = self.try_lock_state(&self.connections, "connections")?;
                if connection_insert_would_exceed_cap(&connections, &key) {
                    log::warn!(
                        "vsock: connection cap ({MAX_CONNECTIONS}) reached after connect; rejecting REQUEST guest_port={} host_port={}",
                        header.src_port,
                        header.dst_port
                    );
                    drop(connections);
                    return self.enqueue_rx(self.reply_packet(&header, op::RST, 0, Vec::new(), 0));
                }
                connections.insert(
                    key,
                    Connection {
                        stream,
                        peer_buf_alloc: header.buf_alloc,
                        peer_fwd_cnt: header.fwd_cnt,
                        fwd_cnt: 0,
                        established: true,
                    },
                );
                log::info!(
                    "vsock: connect REQUEST guest_port={} → host_port={}; enqueued RESPONSE",
                    header.src_port,
                    header.dst_port
                );
                self.enqueue_rx(self.reply_packet(&header, op::RESPONSE, 0, Vec::new(), 0))
            }
            Err(_) => self.enqueue_rx(self.reply_packet(&header, op::RST, 0, Vec::new(), 0)),
        }
    }

    fn handle_response(&self, header: VsockPacketHeader) -> MmioWriteResult {
        let key = key_from_guest(&header);
        if let Some(conn) = self
            .try_lock_state(&self.connections, "connections")?
            .get_mut(&key)
        {
            conn.established = true;
            conn.peer_buf_alloc = header.buf_alloc;
            conn.peer_fwd_cnt = header.fwd_cnt;
            log::info!(
                "vsock: host→guest connection established host_port={} guest_port={}",
                header.dst_port,
                header.src_port
            );
        }
        Ok(())
    }

    fn handle_rw(&self, header: VsockPacketHeader, data: &[u8]) -> MmioWriteResult {
        let key = key_from_guest(&header);
        let mut reset = false;
        {
            let mut connections = self.try_lock_state(&self.connections, "connections")?;
            if let Some(conn) = connections.get_mut(&key) {
                log::debug!("vsock: guest→host {} bytes (RW)", data.len());
                if let Err(err) = conn.stream.write_all(data) {
                    log::debug!("vsock write to host stream failed: {err}");
                    reset = true;
                }
                // We forwarded `data.len()` bytes from the guest to the host
                // application; advance fwd_cnt so our credit updates to the guest
                // reflect the freed receive-buffer space (vsock flow control).
                conn.fwd_cnt = conn.fwd_cnt.wrapping_add(data.len() as u32);
                conn.peer_buf_alloc = header.buf_alloc;
                conn.peer_fwd_cnt = header.fwd_cnt;
            } else {
                reset = true;
            }
            if reset {
                connections.remove(&key);
            }
        }
        if reset {
            self.enqueue_rx(self.reply_packet(&header, op::RST, 0, Vec::new(), 0))?;
        }
        Ok(())
    }

    fn handle_credit_update(&self, header: VsockPacketHeader) -> MmioWriteResult {
        let key = key_from_guest(&header);
        if let Some(conn) = self
            .try_lock_state(&self.connections, "connections")?
            .get_mut(&key)
        {
            conn.peer_buf_alloc = header.buf_alloc;
            conn.peer_fwd_cnt = header.fwd_cnt;
        }
        Ok(())
    }

    fn handle_credit_request(&self, header: VsockPacketHeader) -> MmioWriteResult {
        let key = key_from_guest(&header);
        let fwd_cnt = self
            .try_lock_state(&self.connections, "connections")?
            .get(&key)
            .map(|conn| conn.fwd_cnt)
            .unwrap_or(0);
        self.enqueue_rx(self.reply_packet(&header, op::CREDIT_UPDATE, 0, Vec::new(), fwd_cnt))
    }

    fn handle_shutdown(&self, header: VsockPacketHeader) -> MmioWriteResult {
        self.try_lock_state(&self.connections, "connections")?
            .remove(&key_from_guest(&header));
        self.enqueue_rx(self.reply_packet(&header, op::SHUTDOWN, header.flags, Vec::new(), 0))
    }

    fn handle_rst(&self, header: VsockPacketHeader) -> MmioWriteResult {
        self.try_lock_state(&self.connections, "connections")?
            .remove(&key_from_guest(&header));
        Ok(())
    }

    fn reply_packet(
        &self,
        request: &VsockPacketHeader,
        op: u16,
        flags: u32,
        data: Vec<u8>,
        fwd_cnt: u32,
    ) -> VsockPacket {
        VsockPacket::new(
            VsockPacketHeader {
                src_cid: VSOCK_HOST_CID,
                dst_cid: request.src_cid,
                src_port: request.dst_port,
                dst_port: request.src_port,
                len: data.len() as u32,
                type_: packet_type::STREAM,
                op,
                flags,
                buf_alloc: DEFAULT_BUF_ALLOC,
                fwd_cnt,
            },
            data,
        )
    }

    fn reset(&self) {
        self.queues.clear_poison();
        self.guest_mem.clear_poison();
        self.host_dirty.clear_poison();
        self.rx_processor.clear_poison();
        self.tx_processor.clear_poison();
        self.host_listener.clear_poison();
        self.connections.clear_poison();
        self.pending_rx.clear_poison();
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
        *self.lock_state(&self.rx_processor, "rx_processor") = None;
        *self.lock_state(&self.tx_processor, "tx_processor") = None;
        self.lock_state(&self.pending_rx, "pending_rx").clear();
        self.lock_state(&self.connections, "connections").clear();
    }

    fn read_config(&self, off: u64, len: u8) -> u64 {
        let cfg_off = match off.checked_sub(reg::CONFIG) {
            Some(offset) => offset as usize,
            None => return 0,
        };
        let cfg = self.guest_cid.to_le_bytes();
        if cfg_off >= cfg.len() {
            return 0;
        }
        let mut out = [0u8; 8];
        let copy_len = (cfg.len() - cfg_off).min(len as usize).min(8);
        out[..copy_len].copy_from_slice(&cfg[cfg_off..cfg_off + copy_len]);
        u64::from_le_bytes(out)
    }

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

impl Persist for VirtioVsockMmio {
    type State = VirtioVsockMmioState;

    fn save(&self) -> Self::State {
        self.snapshot_state()
            .expect("virtio-vsock state is unavailable")
    }

    fn try_save(&self) -> Result<Self::State, PersistError> {
        self.snapshot_state()
    }

    fn restore(&mut self, state: Self::State) {
        self.apply_transport_state(&state);
    }
}

impl VirtioVsockMmio {
    fn snapshot_state(&self) -> Result<VirtioVsockMmioState, PersistError> {
        if self.is_failed() {
            return Err(PersistError::Unavailable(
                "virtio-vsock is failed and requires reset".into(),
            ));
        }
        let mut queues = self.snapshot_lock(&self.queues, "queues")?.clone();
        if let Some(proc) = self
            .snapshot_lock(&self.rx_processor, "rx_processor")?
            .as_ref()
        {
            if let Some(q) = queues.get_mut(QUEUE_RX) {
                q.last_avail_idx = proc.avail_idx();
                q.last_used_idx = proc.used_idx();
            }
        }
        if let Some(proc) = self
            .snapshot_lock(&self.tx_processor, "tx_processor")?
            .as_ref()
        {
            if let Some(q) = queues.get_mut(QUEUE_TX) {
                q.last_avail_idx = proc.avail_idx();
                q.last_used_idx = proc.used_idx();
            }
        }
        let connections = self
            .snapshot_lock(&self.connections, "connections")?
            .iter()
            .map(|(key, conn)| VsockConnectionState {
                guest_cid: key.guest_cid,
                guest_port: key.guest_port,
                host_port: key.host_port,
                fwd_cnt: conn.fwd_cnt,
            })
            .collect();
        Ok(VirtioVsockMmioState {
            status: self.status.load(Ordering::Relaxed),
            queue_sel: self.queue_sel.load(Ordering::Relaxed),
            host_features_sel: self.host_features_sel.load(Ordering::Relaxed),
            guest_features_sel: self.guest_features_sel.load(Ordering::Relaxed),
            guest_features_low: self.guest_features_low.load(Ordering::Relaxed),
            guest_features_high: self.guest_features_high.load(Ordering::Relaxed),
            queues,
            activated: self.activated.load(Ordering::Relaxed),
            interrupt_status: self.interrupt_status.load(Ordering::SeqCst),
            connections,
        })
    }
}

impl MmioDevice for VirtioVsockMmio {
    fn mmio_read(&self, off: u64, len: u8) -> MmioReadResult {
        if (reg::CONFIG..reg::CONFIG + 8).contains(&off) {
            return Ok(self.read_config(off, len));
        }
        let val = match off {
            reg::MAGIC_VALUE => MAGIC,
            reg::VERSION => self.version,
            reg::DEVICE_ID => self.device_id,
            reg::VENDOR_ID => self.vendor_id,
            reg::HOST_FEATURES => match self.host_features_sel.load(Ordering::Relaxed) {
                0 => VSOCK_FEATURES_LOW,
                1 => VSOCK_FEATURES_HIGH,
                _ => 0,
            },
            reg::QUEUE_NUM_MAX => {
                let sel = self.queue_sel.load(Ordering::Relaxed) as usize;
                if sel < QUEUE_COUNT {
                    QUEUE_SIZE_MAX
                } else {
                    0
                }
            }
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
                if val & status_bits::DRIVER_OK != 0 {
                    self.activated.store(true, Ordering::Relaxed);
                }
            }
            reg::HOST_FEATURES_SEL => self.host_features_sel.store(val, Ordering::Relaxed),
            reg::GUEST_FEATURES_SEL => self.guest_features_sel.store(val, Ordering::Relaxed),
            reg::GUEST_FEATURES => match self.guest_features_sel.load(Ordering::Relaxed) {
                0 => self.guest_features_low.store(val, Ordering::Relaxed),
                1 => self.guest_features_high.store(val, Ordering::Relaxed),
                _ => {}
            },
            reg::QUEUE_SEL => self.queue_sel.store(val, Ordering::Relaxed),
            reg::QUEUE_NUM => self.q_write(|q| q.set_size(val)),
            reg::QUEUE_DESC_LOW => self
                .q_write(|q| q.desc_table_addr = (q.desc_table_addr & !0xFFFF_FFFF) | val as u64),
            reg::QUEUE_DESC_HIGH => self.q_write(|q| {
                q.desc_table_addr = (q.desc_table_addr & 0xFFFF_FFFF) | (val as u64) << 32
            }),
            reg::QUEUE_DRIVER_LOW => self
                .q_write(|q| q.avail_ring_addr = (q.avail_ring_addr & !0xFFFF_FFFF) | val as u64),
            reg::QUEUE_DRIVER_HIGH => self.q_write(|q| {
                q.avail_ring_addr = (q.avail_ring_addr & 0xFFFF_FFFF) | (val as u64) << 32
            }),
            reg::QUEUE_DEVICE_LOW => {
                self.q_write(|q| q.used_ring_addr = (q.used_ring_addr & !0xFFFF_FFFF) | val as u64)
            }
            reg::QUEUE_DEVICE_HIGH => self.q_write(|q| {
                q.used_ring_addr = (q.used_ring_addr & 0xFFFF_FFFF) | (val as u64) << 32
            }),
            reg::QUEUE_READY => self.q_write(|q| q.set_ready(val != 0)),
            reg::QUEUE_NOTIFY => {
                self.notify_count.fetch_add(1, Ordering::Relaxed);
                match val as usize {
                    QUEUE_RX => {
                        return self.process_rx_queue().map(|_| ());
                    }
                    QUEUE_TX => {
                        return self.process_tx_queue().map(|_| ());
                    }
                    QUEUE_EVENT => {}
                    _ => {}
                }
            }
            reg::INTERRUPT_ACK => {
                self.interrupt_status.fetch_and(!val, Ordering::SeqCst);
            }
            _ => {}
        }
        Ok(())
    }
}

fn key_from_guest(header: &VsockPacketHeader) -> ConnectionKey {
    ConnectionKey {
        guest_cid: header.src_cid,
        guest_port: header.src_port,
        host_port: header.dst_port,
    }
}

fn connection_insert_would_exceed_cap(
    connections: &HashMap<ConnectionKey, Connection>,
    key: &ConnectionKey,
) -> bool {
    connections.len() >= MAX_CONNECTIONS && !connections.contains_key(key)
}

fn connection_cap_error() -> io::Error {
    io::Error::new(
        ErrorKind::ConnectionRefused,
        format!("vsock connection cap reached ({MAX_CONNECTIONS})"),
    )
}

fn host_packet(
    key: ConnectionKey,
    op: u16,
    flags: u32,
    data: Vec<u8>,
    fwd_cnt: u32,
) -> VsockPacket {
    VsockPacket::new(
        VsockPacketHeader {
            src_cid: VSOCK_HOST_CID,
            dst_cid: key.guest_cid,
            src_port: key.host_port,
            dst_port: key.guest_port,
            len: data.len() as u32,
            type_: packet_type::STREAM,
            op,
            flags,
            buf_alloc: DEFAULT_BUF_ALLOC,
            fwd_cnt,
        },
        data,
    )
}

fn read_packet_from_descs(mem: &GuestMemoryMmap, descs: &[(u64, u32)]) -> Option<VsockPacket> {
    let total = descs.iter().try_fold(0usize, |acc, (_, len)| {
        let next = acc.checked_add(*len as usize)?;
        (next <= MAX_PACKET_BYTES + VSOCK_HEADER_LEN).then_some(next)
    })?;
    let mut buf = Vec::with_capacity(total);
    for &(addr, len) in descs {
        let start = buf.len();
        let end = start.checked_add(len as usize)?;
        buf.resize(end, 0);
        mem.read_slice(&mut buf[start..end], GuestAddress(addr))
            .ok()?;
    }
    VsockPacket::from_bytes(&buf)
}

fn write_desc_chain(
    mem: &GuestMemoryMmap,
    dirty: Option<&SoftwareDirtyBitmap>,
    descs: &[(u64, u32)],
    data: &[u8],
) -> bool {
    let mut cursor = 0usize;
    for &(addr, len) in descs {
        if cursor >= data.len() {
            break;
        }
        let take = (data.len() - cursor).min(len as usize);
        let Some(next_cursor) = cursor.checked_add(take) else {
            return false;
        };
        if mem
            .write_slice(&data[cursor..next_cursor], GuestAddress(addr))
            .is_err()
        {
            return false;
        }
        if let Some(dirty) = dirty {
            dirty.mark_range(addr, take as u64);
        }
        cursor = next_cursor;
    }
    cursor == data.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::vqueue::{desc_flags, AvailRing, Descriptor, UsedElem};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    const GUEST_CID: u64 = 42;
    const SERVICE_PORT: u32 = 1024;
    const GUEST_PORT: u32 = 49152;
    const QSIZE: u16 = 16;
    const RX_DESC: u64 = 0x10_0000;
    const RX_AVAIL: u64 = 0x10_1000;
    const RX_USED: u64 = 0x10_2000;
    const RX_BUF: u64 = 0x10_3000;
    const TX_DESC: u64 = 0x20_0000;
    const TX_AVAIL: u64 = 0x20_1000;
    const TX_USED: u64 = 0x20_2000;
    const TX_BUF: u64 = 0x20_3000;

    #[derive(Default)]
    struct FakeStreamState {
        inbound: VecDeque<u8>,
        outbound: Vec<u8>,
    }

    #[derive(Clone)]
    struct FakeStream {
        state: Arc<Mutex<FakeStreamState>>,
    }

    impl Read for FakeStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut state = self.state.lock().unwrap();
            if state.inbound.is_empty() {
                return Err(io::Error::from(ErrorKind::WouldBlock));
            }
            let mut n = 0;
            while n < buf.len() {
                let Some(byte) = state.inbound.pop_front() else {
                    break;
                };
                buf[n] = byte;
                n += 1;
            }
            Ok(n)
        }
    }

    impl Write for FakeStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.state.lock().unwrap().outbound.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FakeListener {
        state: Arc<Mutex<FakeStreamState>>,
        connects: AtomicUsize,
    }

    impl HostVsockListener for FakeListener {
        fn connect(&self, _port: u32) -> io::Result<Box<dyn HostVsockStream>> {
            self.connects.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(Box::new(FakeStream {
                state: self.state.clone(),
            }))
        }
    }

    fn new_mem() -> Arc<GuestMemoryMmap> {
        Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4 * 1024 * 1024)]).unwrap())
    }

    #[test]
    fn poisoned_guest_mem_lock_marks_device_failed_without_panicking() {
        let dev = Arc::new(VirtioVsockMmio::new(7, GUEST_CID));
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
        assert!(dev.process_rx_queue().is_err());
        assert!(dev.pump_host_streams().is_err());
        assert!(Persist::try_save(dev.as_ref()).is_err());
        assert!(dev
            .mmio_write(reg::QUEUE_NOTIFY, QUEUE_TX as u64, 4)
            .is_err());

        dev.mmio_write(reg::STATUS, 0, 4).unwrap();
        assert_eq!(dev.current_status(), 0);
        assert!(Persist::try_save(dev.as_ref()).is_ok());
    }

    fn config_queue(dev: &VirtioVsockMmio, idx: usize, desc: u64, avail: u64, used: u64) {
        dev.mmio_write(reg::QUEUE_SEL, idx as u64, 4).unwrap();
        dev.mmio_write(reg::QUEUE_NUM, QSIZE as u64, 4).unwrap();
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

    fn setup_queues(dev: &VirtioVsockMmio, mem: &Arc<GuestMemoryMmap>, rx_count: u16) -> Vec<u64> {
        dev.set_guest_memory(mem.clone());
        config_queue(dev, QUEUE_RX, RX_DESC, RX_AVAIL, RX_USED);
        config_queue(dev, QUEUE_TX, TX_DESC, TX_AVAIL, TX_USED);
        config_queue(dev, QUEUE_EVENT, 0x30_0000, 0x30_1000, 0x30_2000);

        let mut rx_bufs = Vec::new();
        for i in 0..rx_count {
            let buf = RX_BUF + i as u64 * 0x1000;
            rx_bufs.push(buf);
            mem.write_obj(
                Descriptor {
                    addr: buf,
                    len: 0x1000,
                    flags: desc_flags::WRITE,
                    next: 0,
                },
                GuestAddress(RX_DESC + i as u64 * std::mem::size_of::<Descriptor>() as u64),
            )
            .unwrap();
            mem.write_obj(i, GuestAddress(RX_AVAIL + 4 + i as u64 * 2))
                .unwrap();
        }
        mem.write_obj(
            AvailRing {
                flags: 0,
                idx: rx_count,
            },
            GuestAddress(RX_AVAIL),
        )
        .unwrap();
        rx_bufs
    }

    fn guest_packet(src_port: u32, op: u16, data: &[u8]) -> VsockPacket {
        VsockPacket::new(
            VsockPacketHeader {
                src_cid: GUEST_CID,
                dst_cid: VSOCK_HOST_CID,
                src_port,
                dst_port: SERVICE_PORT,
                len: data.len() as u32,
                type_: packet_type::STREAM,
                op,
                flags: 0,
                buf_alloc: DEFAULT_BUF_ALLOC,
                fwd_cnt: 0,
            },
            data.to_vec(),
        )
    }

    fn submit_tx(
        dev: &VirtioVsockMmio,
        mem: &Arc<GuestMemoryMmap>,
        head_idx: u16,
        avail_idx: u16,
        packet: &VsockPacket,
    ) {
        let bytes = packet.to_bytes();
        let pkt_addr = TX_BUF + head_idx as u64 * 0x1000;
        mem.write_slice(&bytes, GuestAddress(pkt_addr)).unwrap();
        mem.write_obj(
            Descriptor {
                addr: pkt_addr,
                len: bytes.len() as u32,
                flags: 0,
                next: 0,
            },
            GuestAddress(TX_DESC + head_idx as u64 * std::mem::size_of::<Descriptor>() as u64),
        )
        .unwrap();
        mem.write_obj(
            head_idx,
            GuestAddress(TX_AVAIL + 4 + (avail_idx % QSIZE) as u64 * 2),
        )
        .unwrap();
        mem.write_obj(
            AvailRing {
                flags: 0,
                idx: avail_idx + 1,
            },
            GuestAddress(TX_AVAIL),
        )
        .unwrap();
        dev.mmio_write(reg::QUEUE_NOTIFY, QUEUE_TX as u64, 4)
            .unwrap();
    }

    fn read_rx_packet(mem: &Arc<GuestMemoryMmap>, addr: u64) -> VsockPacket {
        let mut header_bytes = [0u8; VSOCK_HEADER_LEN];
        mem.read_slice(&mut header_bytes, GuestAddress(addr))
            .unwrap();
        let header = VsockPacketHeader::from_bytes(&header_bytes).unwrap();
        let mut data = vec![0u8; header.len as usize];
        if !data.is_empty() {
            mem.read_slice(&mut data, GuestAddress(addr + VSOCK_HEADER_LEN as u64))
                .unwrap();
        }
        VsockPacket { header, data }
    }

    #[test]
    fn packet_header_encode_decode_round_trip() {
        let header = VsockPacketHeader {
            src_cid: 3,
            dst_cid: VSOCK_HOST_CID,
            src_port: 1234,
            dst_port: 4321,
            len: 99,
            type_: packet_type::STREAM,
            op: op::RW,
            flags: shutdown_flags::RCV | shutdown_flags::SEND,
            buf_alloc: 8192,
            fwd_cnt: 7,
        };
        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), VSOCK_HEADER_LEN);
        assert_eq!(VsockPacketHeader::from_bytes(&bytes), Some(header));
    }

    #[test]
    fn mmio_negotiation_queue_ready_and_reset() {
        let dev = VirtioVsockMmio::new(7, GUEST_CID);
        assert_eq!(dev.mmio_read(reg::MAGIC_VALUE, 4).unwrap(), MAGIC as u64);
        assert_eq!(
            dev.mmio_read(reg::DEVICE_ID, 4).unwrap(),
            DEVICE_ID_VSOCK as u64
        );
        assert_eq!(dev.mmio_read(reg::CONFIG, 4).unwrap(), GUEST_CID);

        dev.mmio_write(reg::STATUS, status_bits::ACKNOWLEDGE as u64, 4)
            .unwrap();
        dev.mmio_write(
            reg::STATUS,
            (status_bits::ACKNOWLEDGE | status_bits::DRIVER) as u64,
            4,
        )
        .unwrap();
        dev.mmio_write(reg::HOST_FEATURES_SEL, 1, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::HOST_FEATURES, 4).unwrap(), 1);
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, 1, 4).unwrap();
        dev.mmio_write(reg::STATUS, status_bits::FEATURES_OK as u64, 4)
            .unwrap();

        let queue_sizes = [64u64, 128, 256];
        for (idx, size) in queue_sizes.iter().copied().enumerate() {
            dev.mmio_write(reg::QUEUE_SEL, idx as u64, 4).unwrap();
            assert_eq!(
                dev.mmio_read(reg::QUEUE_NUM_MAX, 4).unwrap(),
                QUEUE_SIZE_MAX as u64
            );
            dev.mmio_write(reg::QUEUE_NUM, size, 4).unwrap();
            dev.mmio_write(reg::QUEUE_DESC_LOW, 0x1000 + idx as u64, 4)
                .unwrap();
            dev.mmio_write(reg::QUEUE_DRIVER_LOW, 0x2000 + idx as u64, 4)
                .unwrap();
            dev.mmio_write(reg::QUEUE_DEVICE_LOW, 0x3000 + idx as u64, 4)
                .unwrap();
            dev.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();
            assert_eq!(dev.mmio_read(reg::QUEUE_NUM, 4).unwrap(), size);
            assert_eq!(dev.mmio_read(reg::QUEUE_READY, 4).unwrap(), 1);
        }

        dev.mmio_write(reg::STATUS, status_bits::DRIVER_OK as u64, 4)
            .unwrap();
        assert!(dev.is_activated());
        dev.mmio_write(reg::STATUS, 0, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::STATUS, 4).unwrap(), 0);
        assert!(!dev.is_activated());
        dev.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        assert_eq!(dev.mmio_read(reg::QUEUE_READY, 4).unwrap(), 0);
        assert_eq!(dev.mmio_read(reg::QUEUE_NUM, 4).unwrap(), 0);
    }

    #[test]
    fn persist_round_trips_transport_state() {
        let dev = VirtioVsockMmio::new(7, GUEST_CID);
        dev.mmio_write(reg::HOST_FEATURES_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 0, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, 0x1122_3344, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES_SEL, 1, 4).unwrap();
        dev.mmio_write(reg::GUEST_FEATURES, 1, 4).unwrap();
        let queue_sizes = [16u64, 32, 64];
        for (idx, size) in queue_sizes.iter().copied().enumerate() {
            dev.mmio_write(reg::QUEUE_SEL, idx as u64, 4).unwrap();
            dev.mmio_write(reg::QUEUE_NUM, size, 4).unwrap();
            dev.mmio_write(reg::QUEUE_DESC_LOW, 0x1111_0000 + idx as u64, 4)
                .unwrap();
            dev.mmio_write(reg::QUEUE_DESC_HIGH, 0x1 + idx as u64, 4)
                .unwrap();
            dev.mmio_write(reg::QUEUE_DRIVER_LOW, 0x2222_0000 + idx as u64, 4)
                .unwrap();
            dev.mmio_write(reg::QUEUE_DRIVER_HIGH, 0x2 + idx as u64, 4)
                .unwrap();
            dev.mmio_write(reg::QUEUE_DEVICE_LOW, 0x3333_0000 + idx as u64, 4)
                .unwrap();
            dev.mmio_write(reg::QUEUE_DEVICE_HIGH, 0x3 + idx as u64, 4)
                .unwrap();
            dev.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();
        }
        dev.mmio_write(reg::STATUS, status_bits::DRIVER_OK as u64, 4)
            .unwrap();
        dev.trigger_interrupt().unwrap();

        let state = dev.save();
        let mut restored = VirtioVsockMmio::new(7, GUEST_CID);
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
        for (idx, size) in queue_sizes.iter().copied().enumerate() {
            restored.mmio_write(reg::QUEUE_SEL, idx as u64, 4).unwrap();
            assert_eq!(restored.mmio_read(reg::QUEUE_READY, 4).unwrap(), 1);
            assert_eq!(restored.mmio_read(reg::QUEUE_NUM, 4).unwrap(), size);
        }
    }

    #[test]
    fn stream_handshake_rw_shutdown_and_rst() {
        let mem = new_mem();
        let dev = VirtioVsockMmio::new(7, GUEST_CID);
        let rx_bufs = setup_queues(&dev, &mem, 6);
        let state = Arc::new(Mutex::new(FakeStreamState::default()));
        state
            .lock()
            .unwrap()
            .inbound
            .extend(b"pong".iter().copied());
        let listener = Arc::new(FakeListener {
            state: state.clone(),
            connects: AtomicUsize::new(0),
        });
        dev.set_host_listener(listener.clone());

        submit_tx(
            &dev,
            &mem,
            0,
            0,
            &guest_packet(GUEST_PORT, op::REQUEST, &[]),
        );
        assert_eq!(listener.connects.load(AtomicOrdering::Relaxed), 1);
        let response = read_rx_packet(&mem, rx_bufs[0]);
        assert_eq!(response.header.op, op::RESPONSE);
        assert_eq!(response.header.src_cid, VSOCK_HOST_CID);
        assert_eq!(response.header.dst_cid, GUEST_CID);
        assert_eq!(response.header.src_port, SERVICE_PORT);
        assert_eq!(response.header.dst_port, GUEST_PORT);

        let mut credit = guest_packet(GUEST_PORT, op::CREDIT_UPDATE, &[]);
        credit.header.buf_alloc = 1234;
        credit.header.fwd_cnt = 7;
        submit_tx(&dev, &mem, 1, 1, &credit);
        let key = ConnectionKey {
            guest_cid: GUEST_CID,
            guest_port: GUEST_PORT,
            host_port: SERVICE_PORT,
        };
        {
            let connections = dev.connections.lock().unwrap();
            let conn = connections.get(&key).unwrap();
            assert_eq!(conn.peer_buf_alloc, 1234);
            assert_eq!(conn.peer_fwd_cnt, 7);
        }

        submit_tx(&dev, &mem, 2, 2, &guest_packet(GUEST_PORT, op::RW, b"ping"));
        assert_eq!(state.lock().unwrap().outbound, b"ping");

        assert_eq!(dev.pump_host_streams().unwrap(), 1);
        let rw = read_rx_packet(&mem, rx_bufs[1]);
        assert_eq!(rw.header.op, op::RW);
        assert_eq!(rw.data, b"pong");

        let mut shutdown = guest_packet(GUEST_PORT, op::SHUTDOWN, &[]);
        shutdown.header.flags = shutdown_flags::RCV | shutdown_flags::SEND;
        submit_tx(&dev, &mem, 3, 3, &shutdown);
        let shutdown_reply = read_rx_packet(&mem, rx_bufs[2]);
        assert_eq!(shutdown_reply.header.op, op::SHUTDOWN);
        assert_eq!(shutdown_reply.header.flags, shutdown.header.flags);
        assert!(dev.connections.lock().unwrap().is_empty());

        submit_tx(
            &dev,
            &mem,
            4,
            4,
            &guest_packet(GUEST_PORT + 1, op::REQUEST, &[]),
        );
        assert_eq!(listener.connects.load(AtomicOrdering::Relaxed), 2);
        let second_response = read_rx_packet(&mem, rx_bufs[3]);
        assert_eq!(second_response.header.op, op::RESPONSE);
        submit_tx(
            &dev,
            &mem,
            5,
            5,
            &guest_packet(GUEST_PORT + 1, op::RST, &[]),
        );
        assert!(dev.connections.lock().unwrap().is_empty());

        let tx_used_idx: u16 = mem.read_obj(GuestAddress(TX_USED + 2)).unwrap();
        assert_eq!(tx_used_idx, 6);
        let rx_used_idx: u16 = mem.read_obj(GuestAddress(RX_USED + 2)).unwrap();
        assert_eq!(rx_used_idx, 4);
        let used: UsedElem = mem.read_obj(GuestAddress(RX_USED + 4)).unwrap();
        assert_eq!(used.id, 0);
    }

    #[test]
    fn restored_connection_state_queues_rst_on_next_rx_descriptor() {
        let mem = new_mem();
        let dev = VirtioVsockMmio::new(7, GUEST_CID);
        let rx_bufs = setup_queues(&dev, &mem, 4);
        let listener = Arc::new(FakeListener {
            state: Arc::new(Mutex::new(FakeStreamState::default())),
            connects: AtomicUsize::new(0),
        });
        dev.set_host_listener(listener.clone());

        submit_tx(
            &dev,
            &mem,
            0,
            0,
            &guest_packet(GUEST_PORT, op::REQUEST, &[]),
        );
        assert_eq!(read_rx_packet(&mem, rx_bufs[0]).header.op, op::RESPONSE);

        let state = dev.save();
        assert_eq!(state.connections.len(), 1);
        assert_eq!(
            state.connections[0],
            VsockConnectionState {
                guest_cid: GUEST_CID,
                guest_port: GUEST_PORT,
                host_port: SERVICE_PORT,
                fwd_cnt: 0,
            }
        );
        assert_eq!(state.queues[QUEUE_RX].last_avail_idx, 1);
        assert_eq!(state.queues[QUEUE_RX].last_used_idx, 1);

        let restored = VirtioVsockMmio::new(7, GUEST_CID);
        restored.set_guest_memory(mem.clone());
        restored.restore_transport_state(&state);
        assert_eq!(
            restored
                .reset_restored_connections(&state.connections)
                .unwrap(),
            1
        );

        let rst = read_rx_packet(&mem, rx_bufs[1]);
        assert_eq!(rst.header.op, op::RST);
        assert_eq!(rst.header.src_cid, VSOCK_HOST_CID);
        assert_eq!(rst.header.dst_cid, GUEST_CID);
        assert_eq!(rst.header.src_port, SERVICE_PORT);
        assert_eq!(rst.header.dst_port, GUEST_PORT);
        let rx_used_idx: u16 = mem.read_obj(GuestAddress(RX_USED + 2)).unwrap();
        assert_eq!(rx_used_idx, 2);
    }

    #[cfg(unix)]
    #[test]
    fn host_initiated_stream_queues_request_and_waits_for_response() {
        let mem = new_mem();
        let dev = VirtioVsockMmio::new(7, GUEST_CID);
        let rx_bufs = setup_queues(&dev, &mem, 3);

        let mut host = dev.connect_guest_stream(1025).unwrap();
        let request = read_rx_packet(&mem, rx_bufs[0]);
        assert_eq!(request.header.op, op::REQUEST);
        assert_eq!(request.header.src_cid, VSOCK_HOST_CID);
        assert_eq!(request.header.dst_cid, GUEST_CID);
        assert_eq!(request.header.dst_port, 1025);

        host.write_all(b"start").unwrap();
        assert_eq!(dev.pump_host_streams().unwrap(), 0);

        let response = VsockPacket::new(
            VsockPacketHeader {
                src_cid: GUEST_CID,
                dst_cid: VSOCK_HOST_CID,
                src_port: 1025,
                dst_port: request.header.src_port,
                len: 0,
                type_: packet_type::STREAM,
                op: op::RESPONSE,
                flags: 0,
                buf_alloc: DEFAULT_BUF_ALLOC,
                fwd_cnt: 0,
            },
            Vec::new(),
        );
        submit_tx(&dev, &mem, 0, 0, &response);

        assert_eq!(dev.pump_host_streams().unwrap(), 1);
        let rw = read_rx_packet(&mem, rx_bufs[1]);
        assert_eq!(rw.header.op, op::RW);
        assert_eq!(rw.header.src_port, request.header.src_port);
        assert_eq!(rw.header.dst_port, 1025);
        assert_eq!(rw.data, b"start");

        let reply = VsockPacket::new(
            VsockPacketHeader {
                src_cid: GUEST_CID,
                dst_cid: VSOCK_HOST_CID,
                src_port: 1025,
                dst_port: request.header.src_port,
                len: 2,
                type_: packet_type::STREAM,
                op: op::RW,
                flags: 0,
                buf_alloc: DEFAULT_BUF_ALLOC,
                fwd_cnt: 0,
            },
            b"ok".to_vec(),
        );
        submit_tx(&dev, &mem, 1, 1, &reply);
        let mut got = [0u8; 2];
        host.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ok");
    }

    #[test]
    fn guest_requests_are_rejected_at_connection_cap() {
        let dev = VirtioVsockMmio::new(7, GUEST_CID);
        let listener = Arc::new(FakeListener {
            state: Arc::new(Mutex::new(FakeStreamState::default())),
            connects: AtomicUsize::new(0),
        });
        dev.set_host_listener(listener.clone());

        for offset in 0..MAX_CONNECTIONS {
            let request = guest_packet(GUEST_PORT + offset as u32, op::REQUEST, &[]);
            dev.handle_request(request.header).unwrap();
        }

        assert_eq!(dev.connections.lock().unwrap().len(), MAX_CONNECTIONS);
        assert_eq!(
            listener.connects.load(AtomicOrdering::Relaxed),
            MAX_CONNECTIONS
        );

        let denied = guest_packet(GUEST_PORT + MAX_CONNECTIONS as u32, op::REQUEST, &[]);
        dev.handle_request(denied.header).unwrap();

        assert_eq!(dev.connections.lock().unwrap().len(), MAX_CONNECTIONS);
        assert_eq!(
            listener.connects.load(AtomicOrdering::Relaxed),
            MAX_CONNECTIONS
        );
        assert_eq!(
            dev.pending_rx.lock().unwrap().back().unwrap().header.op,
            op::RST
        );
    }

    #[test]
    fn pending_rx_is_bounded_against_oom() {
        // A guest that never posts RX descriptors must not be able to grow
        // pending_rx without bound (host OOM). enqueue_rx drops once full.
        let dev = VirtioVsockMmio::new(7, GUEST_CID);
        for _ in 0..(MAX_PENDING_RX + 100) {
            dev.enqueue_rx(guest_packet(GUEST_PORT, op::RST, &[]))
                .unwrap();
        }
        assert_eq!(dev.pending_rx.lock().unwrap().len(), MAX_PENDING_RX);
    }
}
