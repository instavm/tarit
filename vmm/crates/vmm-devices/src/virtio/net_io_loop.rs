//! virtio-net I/O loop: a thread that owns the tap fd + a TX kick EventFd
//! and shuttles packets between the host tap and the virtio queues.
//!
//! Two events drive it (epoll-based):
//!   * tap fd readable → read up to one frame → `inject_rx_packet()`.
//!   * tx kick EventFd readable → `process_tx_queue()`.
//!
//! Linux-only. The transport itself is portable for unit-testability, but
//! the actual data plane needs epoll + raw read/write on the tap fd.

#![cfg(target_os = "linux")]

use crate::virtio::net_transport::VirtioNetMmio;
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

const MAX_FRAME: usize = 1600;

/// Handle returned by [`spawn_net_io_loop`]. Dropping it stops the thread.
pub struct NetIoLoop {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Caller may inspect packet counts via the device after stop.
    pub device: Arc<VirtioNetMmio>,
}

impl NetIoLoop {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for NetIoLoop {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawn the I/O loop. `tap_fd` must be non-blocking and stay open for the
/// lifetime of the returned handle. `tx_kick_fd` is an EventFd registered
/// with KVM as the ioeventfd for the TX queue's QUEUE_NOTIFY register, so
/// guest kicks land here instead of trapping into the vCPU.
pub fn spawn_net_io_loop(
    device: Arc<VirtioNetMmio>,
    tap_fd: RawFd,
    tx_kick_fd: RawFd,
) -> io::Result<NetIoLoop> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let device_t = device.clone();

    let handle = std::thread::Builder::new()
        .name("virtio-net-io".into())
        .spawn(move || {
            run(stop_t, device_t, tap_fd, tx_kick_fd);
        })?;

    Ok(NetIoLoop {
        stop,
        handle: Some(handle),
        device,
    })
}

fn run(stop: Arc<AtomicBool>, device: Arc<VirtioNetMmio>, tap_fd: RawFd, tx_kick_fd: RawFd) {
    // SAFETY: epoll_create1 has no pointer arguments; flags are a valid libc
    // constant, and errors are handled from the returned fd.
    let ep = match unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) } {
        fd if fd >= 0 => fd,
        _ => {
            log::error!(
                "net_io_loop: epoll_create1 failed: {}",
                io::Error::last_os_error()
            );
            return;
        }
    };

    let add = |fd: RawFd, tag: u64| -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: tag,
        };
        // SAFETY: `ev` points to a live epoll_event for the duration of the
        // syscall; invalid fds are reported by epoll_ctl and handled.
        let rc = unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, fd, &mut ev) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    if let Err(e) = add(tap_fd, 1) {
        log::error!("net_io_loop: epoll add tap: {e}");
        // SAFETY: `ep` is the fd returned by epoll_create1 above and is owned
        // by this function on this error path.
        unsafe { libc::close(ep) };
        return;
    }
    if let Err(e) = add(tx_kick_fd, 2) {
        log::error!("net_io_loop: epoll add tx_kick: {e}");
        // SAFETY: `ep` is the fd returned by epoll_create1 above and is owned
        // by this function on this error path.
        unsafe { libc::close(ep) };
        return;
    }

    if let Err(e) = vmm_jailer::seccomp::SeccompProfile::device().install() {
        log::error!("net_io_loop: seccomp install failed; refusing guest I/O: {e}");
        // SAFETY: `ep` is the fd returned by epoll_create1 above and is owned
        // by this function on this error path.
        unsafe { libc::close(ep) };
        return;
    }

    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];
    let mut buf = [0u8; MAX_FRAME];

    while !stop.load(Ordering::Relaxed) {
        // 100 ms timeout so we can observe the stop flag promptly.
        // SAFETY: `events` is a valid writable array, and the maxevents value
        // matches its length. Errors are handled from the return value.
        let n = unsafe { libc::epoll_wait(ep, events.as_mut_ptr(), events.len() as i32, 100) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("net_io_loop: epoll_wait: {err}");
            break;
        }
        for ev in &events[..n as usize] {
            match ev.u64 {
                1 => drain_tap(tap_fd, &device, &mut buf),
                2 => drain_kick(tx_kick_fd, &device),
                _ => {}
            }
        }
    }

    // SAFETY: `ep` is the epoll fd owned by this function and is no longer used.
    unsafe { libc::close(ep) };
}

/// Pull all available frames from the tap (it's non-blocking) and push them
/// to the guest RX queue. Stops at EAGAIN.
fn drain_tap(tap_fd: RawFd, device: &Arc<VirtioNetMmio>, buf: &mut [u8]) {
    loop {
        // SAFETY: `buf` is a valid writable slice for `buf.len()` bytes; invalid
        // or non-ready fds are reported by read and handled below.
        let rc = unsafe { libc::read(tap_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                return;
            }
            log::warn!("net_io_loop: tap read: {err}");
            return;
        }
        if rc == 0 {
            return;
        }
        let frame = &buf[..rc as usize];
        if !device.inject_rx_packet(frame) {
            log::debug!(
                "net_io_loop: RX queue full — dropping {}-byte frame",
                frame.len()
            );
            return;
        }
    }
}

/// Consume the TX kick EventFd counter and process whatever the guest
/// queued. Multiple notifications collapse into one — that's fine.
fn drain_kick(tx_kick_fd: RawFd, device: &Arc<VirtioNetMmio>) {
    let mut counter = [0u8; 8];
    // SAFETY: `counter` is a valid writable 8-byte eventfd counter buffer;
    // invalid or empty fds are reported by read and handled below.
    let rc = unsafe {
        libc::read(
            tx_kick_fd,
            counter.as_mut_ptr() as *mut libc::c_void,
            counter.len(),
        )
    };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if !matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) {
            log::warn!("net_io_loop: tx_kick read: {err}");
        }
    }
    device.process_tx_queue();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::MmioDevice;
    use crate::virtio::regs::reg;
    use crate::virtio::vqueue::{desc_flags, AvailRing, Descriptor};
    use std::os::fd::AsRawFd;
    use std::sync::Arc;
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    /// Stand up an io_loop with a Unix socketpair impersonating a tap, an
    /// EventFd for TX kicks, and a real virtio-net transport. Verify:
    ///   1. TX path: guest queues a frame, we kick, the loop drains and
    ///      writes to the "tap" fd; we read it back on the peer side.
    ///   2. RX path: we write a frame to the peer side; the loop reads it
    ///      and injects into the guest RX queue.
    #[test]
    fn tx_and_rx_through_socketpair() {
        // socketpair as a tap stand-in.
        let mut fds = [0i32; 2];
        // SAFETY: `fds` points to two valid i32 slots for socketpair to fill;
        // arguments are valid AF_UNIX datagram socket constants.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_DGRAM | libc::SOCK_NONBLOCK,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0, "socketpair failed");
        let host_fd = fds[0];
        let dev_fd = fds[1];

        // Device + memory.
        let mem =
            Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4 * 1024 * 1024)]).unwrap());
        let device = Arc::new(VirtioNetMmio::new(7, [0x02, 0, 0, 0, 0, 1]));
        device.set_guest_memory(mem.clone());
        device.set_tap_fd(dev_fd);

        let irq_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).expect("irq evt");
        device.set_irq_evt(irq_evt.try_clone().unwrap());

        let tx_kick = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).expect("tx kick");
        let tx_kick_fd = tx_kick.as_raw_fd();

        // --- Set up TX queue (idx 1) ---
        const TX_DESC: u64 = 0x10_0000;
        const TX_AVAIL: u64 = 0x10_1000;
        const TX_USED: u64 = 0x10_2000;
        const TX_BUF: u64 = 0x10_3000;
        // virtio_net_hdr (10 bytes zero) + payload "HELLO".
        let payload = b"HELLO";
        let mut packet = vec![0u8; super::super::net_transport::VIRTIO_NET_HDR_LEN];
        packet.extend_from_slice(payload);
        mem.write_slice(&packet, GuestAddress(TX_BUF)).unwrap();
        mem.write_obj(
            Descriptor {
                addr: TX_BUF,
                len: packet.len() as u32,
                flags: 0,
                next: 0,
            },
            GuestAddress(TX_DESC),
        )
        .unwrap();
        mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(TX_AVAIL))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(TX_AVAIL + 4)).unwrap();

        device.mmio_write(reg::QUEUE_SEL, 1, 4).unwrap();
        device.mmio_write(reg::QUEUE_NUM, 16, 4).unwrap();
        device
            .mmio_write(reg::QUEUE_DESC_LOW, TX_DESC as u32 as u64, 4)
            .unwrap();
        device
            .mmio_write(reg::QUEUE_DESC_HIGH, TX_DESC >> 32, 4)
            .unwrap();
        device
            .mmio_write(reg::QUEUE_DRIVER_LOW, TX_AVAIL as u32 as u64, 4)
            .unwrap();
        device
            .mmio_write(reg::QUEUE_DRIVER_HIGH, TX_AVAIL >> 32, 4)
            .unwrap();
        device
            .mmio_write(reg::QUEUE_DEVICE_LOW, TX_USED as u32 as u64, 4)
            .unwrap();
        device
            .mmio_write(reg::QUEUE_DEVICE_HIGH, TX_USED >> 32, 4)
            .unwrap();
        device.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();

        // --- Set up RX queue (idx 0) ---
        const RX_DESC: u64 = 0x20_0000;
        const RX_AVAIL: u64 = 0x20_1000;
        const RX_USED: u64 = 0x20_2000;
        const RX_BUF: u64 = 0x20_3000;
        mem.write_obj(
            Descriptor {
                addr: RX_BUF,
                len: 2048,
                flags: desc_flags::WRITE,
                next: 0,
            },
            GuestAddress(RX_DESC),
        )
        .unwrap();
        mem.write_obj(AvailRing { flags: 0, idx: 1 }, GuestAddress(RX_AVAIL))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(RX_AVAIL + 4)).unwrap();

        device.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
        device.mmio_write(reg::QUEUE_NUM, 16, 4).unwrap();
        device
            .mmio_write(reg::QUEUE_DESC_LOW, RX_DESC as u32 as u64, 4)
            .unwrap();
        device
            .mmio_write(reg::QUEUE_DESC_HIGH, RX_DESC >> 32, 4)
            .unwrap();
        device
            .mmio_write(reg::QUEUE_DRIVER_LOW, RX_AVAIL as u32 as u64, 4)
            .unwrap();
        device
            .mmio_write(reg::QUEUE_DRIVER_HIGH, RX_AVAIL >> 32, 4)
            .unwrap();
        device
            .mmio_write(reg::QUEUE_DEVICE_LOW, RX_USED as u32 as u64, 4)
            .unwrap();
        device
            .mmio_write(reg::QUEUE_DEVICE_HIGH, RX_USED >> 32, 4)
            .unwrap();
        device.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();

        // Start the loop.
        let mut io_loop = spawn_net_io_loop(device.clone(), dev_fd, tx_kick_fd).unwrap();

        // --- TX: kick the TX queue. ---
        tx_kick.write(1).expect("write kick");
        // Read from the host side of the socketpair — the loop should have
        // written the payload (without the net_hdr).
        let mut recv = [0u8; 256];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let n = loop {
            let recv_ptr = recv.as_mut_ptr() as *mut libc::c_void;
            let recv_len = recv.len();
            // SAFETY: `recv` is a valid writable slice for `recv.len()` bytes;
            // socket readiness/errors are reflected in the return value.
            let rc = unsafe { libc::read(host_fd, recv_ptr, recv_len) };
            if rc > 0 {
                break rc as usize;
            }
            if std::time::Instant::now() > deadline {
                panic!("timed out waiting for TX frame");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(&recv[..n], payload);
        assert_eq!(device.tx_packets.load(Ordering::Relaxed), 1);

        // --- RX: write a frame on the host side; the loop should inject. ---
        let inbound = b"INBOUND-FRAME";
        // SAFETY: `inbound` is a valid readable slice for `inbound.len()` bytes;
        // socket write errors are reflected in the return value.
        let rc = unsafe {
            libc::write(
                host_fd,
                inbound.as_ptr() as *const libc::c_void,
                inbound.len(),
            )
        };
        assert!(rc > 0);

        // Wait for the rx packet to be injected.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while device.rx_packets.load(Ordering::Relaxed) == 0 {
            if std::time::Instant::now() > deadline {
                panic!("timed out waiting for RX inject");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Verify the frame landed in the guest RX buffer.
        let mut got = vec![0u8; super::super::net_transport::VIRTIO_NET_HDR_LEN + inbound.len()];
        mem.read_slice(&mut got, GuestAddress(RX_BUF)).unwrap();
        assert_eq!(
            &got[super::super::net_transport::VIRTIO_NET_HDR_LEN..],
            inbound
        );

        // Stop and clean up.
        io_loop.stop();
        // SAFETY: these fds were returned by socketpair in this test and are no
        // longer used after the loop has stopped.
        unsafe {
            libc::close(host_fd);
            libc::close(dev_fd);
        }
    }
}
