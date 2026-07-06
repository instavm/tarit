//! virtio-net E2E tests against a *real* TAP device.
//!
//! These tests do not bring up a guest kernel — that path is blocked on
//! nested-virt by the same MMIO-coalescing issue as virtio-blk (see
//! docs/remaining_work.md). Instead they exercise the full host-side data
//! plane: TAP creation → MMIO transport → io_loop thread → kernel netif,
//! verified with an AF_PACKET sniffer/injector.
//!
//! Requires Linux + root (sudo cargo test) for tap creation and AF_PACKET.
//! On c8i this runs under `sudo -E cargo test --features kvm`.

#![cfg(target_os = "linux")]

use std::os::fd::AsRawFd;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vm_memory::{Bytes as _, GuestAddress, GuestMemoryMmap};
use vmm_devices::bus::MmioDevice;
use vmm_devices::virtio::net_io_loop::spawn_net_io_loop;
use vmm_devices::virtio::net_transport::{VirtioNetMmio, VIRTIO_NET_HDR_LEN};
use vmm_devices::virtio::regs::reg;
use vmm_devices::virtio::vqueue::{desc_flags, AvailRing, Descriptor};
use vmm_net::tap::Tap;

const MAC: [u8; 6] = [0x02, 0x00, 0x00, 0xab, 0xcd, 0xef];

// Queue layout. Distinct addresses for RX (queue 0) and TX (queue 1) so a
// single 4 MiB guest memory holds both.
const RX_DESC: u64 = 0x10_0000;
const RX_AVAIL: u64 = 0x10_1000;
const RX_USED: u64 = 0x10_2000;
const RX_BUF: u64 = 0x10_3000;

const TX_DESC: u64 = 0x20_0000;
const TX_AVAIL: u64 = 0x20_1000;
const TX_USED: u64 = 0x20_2000;
const TX_BUF: u64 = 0x20_3000;

fn tap_up(name: &str) -> bool {
    Command::new("ip")
        .args(["link", "set", name, "up"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn tap_delete(name: &str) {
    let _ = Command::new("ip").args(["link", "delete", name]).status();
}

fn ifindex(name: &str) -> Option<i32> {
    let n = std::ffi::CString::new(name).unwrap();
    // SAFETY: `n` is a NUL-terminated interface name whose pointer remains
    // valid for the duration of the libc call.
    let idx = unsafe { libc::if_nametoindex(n.as_ptr()) };
    if idx == 0 {
        None
    } else {
        Some(idx as i32)
    }
}

fn close_fd(fd: libc::c_int) {
    // SAFETY: `fd` was returned by a successful libc `socket` call in this test.
    let rc = unsafe { libc::close(fd) };
    if rc != 0 {
        eprintln!("close({fd}) failed: {}", std::io::Error::last_os_error());
    }
}

fn configure_queues(device: &Arc<VirtioNetMmio>) {
    // RX queue 0
    device.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
    device.mmio_write(reg::QUEUE_NUM, 16, 4).unwrap();
    device.mmio_write(reg::QUEUE_DESC_LOW, RX_DESC, 4).unwrap();
    device.mmio_write(reg::QUEUE_DESC_HIGH, 0, 4).unwrap();
    device
        .mmio_write(reg::QUEUE_DRIVER_LOW, RX_AVAIL, 4)
        .unwrap();
    device.mmio_write(reg::QUEUE_DRIVER_HIGH, 0, 4).unwrap();
    device
        .mmio_write(reg::QUEUE_DEVICE_LOW, RX_USED, 4)
        .unwrap();
    device.mmio_write(reg::QUEUE_DEVICE_HIGH, 0, 4).unwrap();
    device.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();
    // TX queue 1
    device.mmio_write(reg::QUEUE_SEL, 1, 4).unwrap();
    device.mmio_write(reg::QUEUE_NUM, 16, 4).unwrap();
    device.mmio_write(reg::QUEUE_DESC_LOW, TX_DESC, 4).unwrap();
    device.mmio_write(reg::QUEUE_DESC_HIGH, 0, 4).unwrap();
    device
        .mmio_write(reg::QUEUE_DRIVER_LOW, TX_AVAIL, 4)
        .unwrap();
    device.mmio_write(reg::QUEUE_DRIVER_HIGH, 0, 4).unwrap();
    device
        .mmio_write(reg::QUEUE_DEVICE_LOW, TX_USED, 4)
        .unwrap();
    device.mmio_write(reg::QUEUE_DEVICE_HIGH, 0, 4).unwrap();
    device.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();
}

/// Bind an AF_PACKET capture socket to `name`. Captures every frame.
fn af_packet_capture(name: &str) -> std::io::Result<i32> {
    let proto = (libc::ETH_P_ALL as u16).to_be() as i32;
    // SAFETY: Arguments are valid constants for creating an AF_PACKET socket;
    // the returned fd is checked before use.
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_NONBLOCK, proto) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let idx = ifindex(name).ok_or_else(|| std::io::Error::other("ifindex"))?;
    // SAFETY: `sockaddr_ll` is a plain C struct and all fields used by bind are
    // initialized below before the call.
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
    sll.sll_ifindex = idx;
    // SAFETY: `fd` is a valid socket, and `sll` points to an initialized
    // sockaddr_ll with the matching length.
    let rc = unsafe {
        libc::bind(
            fd,
            &sll as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_ll>() as u32,
        )
    };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        close_fd(fd);
        return Err(e);
    }
    Ok(fd)
}

/// Build a minimum-length (60-byte) Ethernet frame containing `magic` as
/// its payload. Returns the full frame.
fn ethernet_frame(dst: [u8; 6], src: [u8; 6], magic: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(60);
    f.extend_from_slice(&dst);
    f.extend_from_slice(&src);
    f.extend_from_slice(&[0x08, 0x00]); // EtherType IPv4
    f.extend_from_slice(magic);
    while f.len() < 60 {
        f.push(0);
    }
    f
}

#[test]
#[ignore = "needs Linux + root tap creation (run on c8i with sudo)"]
fn net_tap_lifecycle() {
    let name = "vmt-life";
    tap_delete(name);
    let tap = Tap::create(name).expect("Tap::create");
    assert!(tap_up(name), "ip link up {name}");
    assert!(ifindex(name).is_some(), "{name} should exist in netns");

    let mem =
        Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4 * 1024 * 1024)]).unwrap());
    let device = Arc::new(VirtioNetMmio::new(7, MAC));
    device.set_guest_memory(mem.clone());
    device.set_tap_fd(tap.fd);
    let irq = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap();
    device.set_irq_evt(irq);
    let kick = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap();

    let mut io_loop = spawn_net_io_loop(device.clone(), tap.fd, kick.as_raw_fd()).unwrap();
    // Let the epoll loop spin a few times.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(device.tx_packets.load(Ordering::Relaxed), 0);
    assert_eq!(device.rx_packets.load(Ordering::Relaxed), 0);

    io_loop.stop();
    tap_delete(name);
    eprintln!("net_tap_lifecycle: PASS");
}

#[test]
#[ignore = "needs Linux + root tap + AF_PACKET (run on c8i with sudo)"]
fn net_egress_through_real_tap() {
    let name = "vmt-egr";
    tap_delete(name);
    let tap = Tap::create(name).expect("Tap::create");
    assert!(tap_up(name), "ip link up {name}");

    let mem =
        Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4 * 1024 * 1024)]).unwrap());
    let device = Arc::new(VirtioNetMmio::new(7, MAC));
    device.set_guest_memory(mem.clone());
    device.set_tap_fd(tap.fd);
    let irq = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap();
    device.set_irq_evt(irq);
    let kick = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap();

    configure_queues(&device);

    // Start sniffing BEFORE we kick — we want to catch the frame the moment
    // the io_loop writes it to the tap fd.
    let sniffer = af_packet_capture(name).expect("af_packet_capture");

    let mut io_loop = spawn_net_io_loop(device.clone(), tap.fd, kick.as_raw_fd()).unwrap();

    // Build TX descriptor: 10-byte zero virtio_net_hdr + Ethernet frame.
    let magic = b"VMM-EGRESS";
    let frame = ethernet_frame([0xff; 6], MAC, magic);
    let mut packet = vec![0u8; VIRTIO_NET_HDR_LEN];
    packet.extend_from_slice(&frame);

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

    // Kick the TX queue.
    kick.write(1).expect("write tx kick");

    // Read from AF_PACKET until we see our magic, or timeout.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = vec![0u8; 2048];
    let mut found = false;
    while Instant::now() < deadline {
        // SAFETY: `sniffer` is a valid nonblocking socket, and `buf` is valid
        // writable memory for `buf.len()` bytes during the call.
        let rc =
            unsafe { libc::recv(sniffer, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if rc > 0 {
            let got = &buf[..rc as usize];
            if got.windows(magic.len()).any(|w| w == magic) {
                found = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let tx_packets = device.tx_packets.load(Ordering::Relaxed);

    io_loop.stop();
    close_fd(sniffer);
    tap_delete(name);

    assert!(
        found,
        "egress frame magic not seen on AF_PACKET (tx_packets={tx_packets})"
    );
    assert_eq!(tx_packets, 1, "exactly one tx packet expected");
    eprintln!("net_egress_through_real_tap: PASS");
}

#[test]
#[ignore = "needs Linux + root tap + AF_PACKET (run on c8i with sudo)"]
fn net_ingress_through_real_tap() {
    let name = "vmt-ing";
    tap_delete(name);
    let tap = Tap::create(name).expect("Tap::create");
    assert!(tap_up(name), "ip link up {name}");
    let idx = ifindex(name).expect("ifindex");

    let mem =
        Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4 * 1024 * 1024)]).unwrap());
    let device = Arc::new(VirtioNetMmio::new(7, MAC));
    device.set_guest_memory(mem.clone());
    device.set_tap_fd(tap.fd);
    let irq = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap();
    device.set_irq_evt(irq);
    let kick = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap();

    configure_queues(&device);

    // Pre-arm one RX descriptor pointing at RX_BUF.
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

    let mut io_loop = spawn_net_io_loop(device.clone(), tap.fd, kick.as_raw_fd()).unwrap();

    // Inject a frame via AF_PACKET sendto on vmtap0. The kernel transmits it
    // out the netif, which means it becomes readable on the userspace tap fd
    // — exactly what the io_loop is polling for.
    // SAFETY: Arguments are valid constants for creating an AF_PACKET socket;
    // the returned fd is checked before use.
    let tx_sock = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, 0) };
    assert!(
        tx_sock >= 0,
        "AF_PACKET socket: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: `sockaddr_ll` is a plain C struct and all fields used by sendto
    // are initialized below before the call.
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_protocol = 0; // header is in the frame
    sll.sll_ifindex = idx;

    let magic = b"VMM-INGRESS";
    let src_mac = [0x02, 0x00, 0x00, 0x12, 0x34, 0x56];
    let frame = ethernet_frame(MAC, src_mac, magic);
    // SAFETY: `tx_sock` is a valid socket, `frame` is readable for its length,
    // and `sll` points to an initialized sockaddr_ll with the matching length.
    let rc = unsafe {
        libc::sendto(
            tx_sock,
            frame.as_ptr() as *const libc::c_void,
            frame.len(),
            0,
            &sll as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_ll>() as u32,
        )
    };
    assert!(
        rc == frame.len() as isize,
        "sendto returned {rc}, errno={}",
        std::io::Error::last_os_error()
    );

    // Wait for the io_loop to drain the tap and inject into the RX queue.
    let deadline = Instant::now() + Duration::from_secs(3);
    while device.rx_packets.load(Ordering::Relaxed) == 0 {
        if Instant::now() > deadline {
            io_loop.stop();
            close_fd(tx_sock);
            tap_delete(name);
            panic!("timed out waiting for RX inject");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Verify the frame landed in guest RX memory (after the 10-byte net_hdr).
    let mut got = vec![0u8; VIRTIO_NET_HDR_LEN + frame.len()];
    mem.read_slice(&mut got, GuestAddress(RX_BUF)).unwrap();
    // virtio_net_hdr_v1: base fields are zero; num_buffers (the last u16) is 1
    // for this single-descriptor RX injection.
    assert_eq!(
        &got[..VIRTIO_NET_HDR_LEN - 2],
        &[0u8; VIRTIO_NET_HDR_LEN - 2]
    );
    assert_eq!(
        &got[VIRTIO_NET_HDR_LEN - 2..VIRTIO_NET_HDR_LEN],
        &1u16.to_le_bytes()
    );
    assert_eq!(
        &got[VIRTIO_NET_HDR_LEN..VIRTIO_NET_HDR_LEN + frame.len()],
        &frame[..]
    );

    io_loop.stop();
    close_fd(tx_sock);
    tap_delete(name);
    eprintln!("net_ingress_through_real_tap: PASS");
}
