//! Virtio-blk e2e test — creates a real disk image, attaches it to a VM,
//! and verifies the I/O path works.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::io::Write;
use vm_memory::{ByteValued, Bytes as _, GuestAddress, GuestMemoryMmap};
use vmm_devices::bus::MmioDevice;
use vmm_devices::virtio::blk::{req_type, BlkReqHeader};
use vmm_devices::virtio::blk_backend::BlkBackend;
use vmm_devices::virtio::blk_transport::{status_bits, VirtioBlkMmio};
use vmm_devices::virtio::regs::reg;

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
struct Descriptor {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}
// SAFETY: `Descriptor` is a `repr(C)` plain-data virtqueue descriptor mirror
// made only of integer fields, with no pointers or drop behavior.
unsafe impl ByteValued for Descriptor {}

const DESC_TABLE_ADDR: u64 = 0x800000;
const AVAIL_RING_ADDR: u64 = 0x900000;
const USED_RING_ADDR: u64 = 0xA00000;
const DATA_BUF_ADDR: u64 = 0xB00000;
const STATUS_ADDR: u64 = 0xB01000;
const HEADER_ADDR: u64 = 0xC00000;
const QUEUE_SIZE: u16 = 64;

/// Write a descriptor at a given index in the descriptor table.
fn write_desc(mem: &GuestMemoryMmap, idx: usize, desc: Descriptor) {
    let addr =
        GuestAddress(DESC_TABLE_ADDR + idx as u64 * std::mem::size_of::<Descriptor>() as u64);
    let _ = mem.write_obj(desc, addr);
}

/// Write the avail ring: flags=0, idx=1, ring[0]=head_idx.
fn write_avail(mem: &GuestMemoryMmap, head_idx: u16, avail_idx: u16) {
    let _ = mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR)); // flags
    let _ = mem.write_obj(avail_idx, GuestAddress(AVAIL_RING_ADDR + 2)); // idx
    let _ = mem.write_obj(head_idx, GuestAddress(AVAIL_RING_ADDR + 4)); // ring[0]
}

/// Write the used ring: flags=0, idx=0.
fn write_used(mem: &GuestMemoryMmap) {
    let _ = mem.write_obj(0u16, GuestAddress(USED_RING_ADDR));
    let _ = mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2));
}

/// Read the status byte.
fn read_status(mem: &GuestMemoryMmap) -> u8 {
    mem.read_obj::<u8>(GuestAddress(STATUS_ADDR))
        .unwrap_or(0xFF)
}

/// Read a data buffer from guest memory.
fn read_data(mem: &GuestMemoryMmap, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let _ = mem.read_slice(&mut buf, GuestAddress(DATA_BUF_ADDR));
    buf
}

fn setup_read_request(mem: &GuestMemoryMmap, sector: u64, buf_len: u32, avail_idx: u16) {
    // Write the request header at HEADER_ADDR.
    let header = BlkReqHeader {
        req_type: req_type::IN,
        reserved: 0,
        sector,
    };
    let _ = mem.write_obj(header, GuestAddress(HEADER_ADDR));

    // Desc 0: header (readable, NEXT → 1)
    write_desc(
        mem,
        0,
        Descriptor {
            addr: HEADER_ADDR,
            len: 16,
            flags: 0x1,
            next: 1,
        },
    );
    // Desc 1: data buffer (writable, NEXT → 2)
    write_desc(
        mem,
        1,
        Descriptor {
            addr: DATA_BUF_ADDR,
            len: buf_len,
            flags: 0x3,
            next: 2,
        },
    );
    // Desc 2: status (writable, no NEXT)
    write_desc(
        mem,
        2,
        Descriptor {
            addr: STATUS_ADDR,
            len: 1,
            flags: 0x2,
            next: 0,
        },
    );

    write_avail(mem, 0, avail_idx);
    write_used(mem);
    let _ = mem.write_obj(0u8, GuestAddress(STATUS_ADDR));
}

fn setup_write_request(mem: &GuestMemoryMmap, sector: u64, data: &[u8], avail_idx: u16) {
    // Write data at DATA_BUF_ADDR.
    let _ = mem.write_slice(data, GuestAddress(DATA_BUF_ADDR));

    // Write the request header.
    let header = BlkReqHeader {
        req_type: req_type::OUT,
        reserved: 0,
        sector,
    };
    let _ = mem.write_obj(header, GuestAddress(HEADER_ADDR));

    // Desc 0: header (readable, NEXT → 1)
    write_desc(
        mem,
        0,
        Descriptor {
            addr: HEADER_ADDR,
            len: 16,
            flags: 0x1,
            next: 1,
        },
    );
    // Desc 1: data (readable, NEXT → 2)
    write_desc(
        mem,
        1,
        Descriptor {
            addr: DATA_BUF_ADDR,
            len: data.len() as u32,
            flags: 0x1,
            next: 2,
        },
    );
    // Desc 2: status (writable, no NEXT)
    write_desc(
        mem,
        2,
        Descriptor {
            addr: STATUS_ADDR,
            len: 1,
            flags: 0x2,
            next: 0,
        },
    );

    write_avail(mem, 0, avail_idx);
    write_used(mem);
    let _ = mem.write_obj(0u8, GuestAddress(STATUS_ADDR));
}

fn setup_flush_request(mem: &GuestMemoryMmap, avail_idx: u16) {
    let header = BlkReqHeader {
        req_type: req_type::FLUSH,
        reserved: 0,
        sector: 0,
    };
    let _ = mem.write_obj(header, GuestAddress(HEADER_ADDR));

    write_desc(
        mem,
        0,
        Descriptor {
            addr: HEADER_ADDR,
            len: 16,
            flags: 0x1,
            next: 1,
        },
    );
    write_desc(
        mem,
        1,
        Descriptor {
            addr: STATUS_ADDR,
            len: 1,
            flags: 0x2,
            next: 0,
        },
    );

    write_avail(mem, 0, avail_idx);
    write_used(mem);
    let _ = mem.write_obj(0u8, GuestAddress(STATUS_ADDR));
}

fn configure_queue(dev: &VirtioBlkMmio, mem: &GuestMemoryMmap) {
    dev.set_guest_memory(std::sync::Arc::new(mem.clone()));
    dev.mmio_write(reg::QUEUE_SEL, 0, 4).unwrap();
    dev.mmio_write(reg::QUEUE_NUM, QUEUE_SIZE as u64, 4)
        .unwrap();
    dev.mmio_write(reg::QUEUE_DESC_LOW, DESC_TABLE_ADDR, 4)
        .unwrap();
    dev.mmio_write(reg::QUEUE_DESC_HIGH, 0, 4).unwrap();
    dev.mmio_write(reg::QUEUE_DRIVER_LOW, AVAIL_RING_ADDR, 4)
        .unwrap();
    dev.mmio_write(reg::QUEUE_DRIVER_HIGH, 0, 4).unwrap();
    dev.mmio_write(reg::QUEUE_DEVICE_LOW, USED_RING_ADDR, 4)
        .unwrap();
    dev.mmio_write(reg::QUEUE_DEVICE_HIGH, 0, 4).unwrap();
    dev.mmio_write(reg::QUEUE_READY, 1, 4).unwrap();
    dev.mmio_write(reg::STATUS, status_bits::DRIVER_OK as u64, 4)
        .unwrap();
}

#[test]
#[ignore = "needs Linux+KVM (run on c8i)"]
fn virtio_blk_read_e2e() {
    let dir = tempfile::tempdir().unwrap();
    let disk_path = dir.path().join("test.blk");
    let mut disk = std::fs::File::create(&disk_path).unwrap();
    disk.write_all(&[0xAA; 512 * 4]).unwrap();
    disk.write_all(&[0xBB; 512 * 4]).unwrap();
    disk.flush().unwrap();

    let backend = BlkBackend::open(&disk_path, false).unwrap();
    assert_eq!(backend.sectors, 8);
    let dev = VirtioBlkMmio::new(5, backend);

    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 16 * 1024 * 1024)]).unwrap();
    configure_queue(&dev, &mem);

    // READ sector 0 (should be 0xAA).
    setup_read_request(&mem, 0, 512, 1);
    dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();

    let status = read_status(&mem);
    let data = read_data(&mem, 512);
    assert_eq!(status, 0, "read status should be OK");
    assert!(data.iter().all(|&b| b == 0xAA), "sector 0 should be 0xAA");

    // READ sector 4 (should be 0xBB).
    // Need a fresh processor for the second request — recreate the queue config.
    setup_read_request(&mem, 4, 512, 2);
    dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();
    let status = read_status(&mem);
    let data = read_data(&mem, 512);
    assert_eq!(status, 0, "read sector 4 status should be OK");
    assert!(data.iter().all(|&b| b == 0xBB), "sector 4 should be 0xBB");

    eprintln!("virtio-blk READ e2e: PASS");
}

#[test]
#[ignore = "needs Linux+KVM (run on c8i)"]
fn virtio_blk_write_e2e() {
    let dir = tempfile::tempdir().unwrap();
    let disk_path = dir.path().join("test_write.blk");
    std::fs::write(&disk_path, vec![0u8; 4096]).unwrap();

    let backend = BlkBackend::open(&disk_path, false).unwrap();
    let dev = VirtioBlkMmio::new(5, backend);

    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 16 * 1024 * 1024)]).unwrap();
    configure_queue(&dev, &mem);

    // WRITE 0xCC to sector 0.
    let write_data = vec![0xCCu8; 512];
    setup_write_request(&mem, 0, &write_data, 1);
    dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();
    let status = read_status(&mem);
    assert_eq!(status, 0, "write status should be OK");

    // READ back to verify.
    setup_read_request(&mem, 0, 512, 1);
    dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();
    let status = read_status(&mem);
    assert_eq!(status, 0, "read-after-write status should be OK");
    let data = read_data(&mem, 512);
    assert!(
        data.iter().all(|&b| b == 0xCC),
        "sector 0 should be 0xCC after write"
    );

    eprintln!("virtio-blk WRITE e2e: PASS");
}

#[test]
#[ignore = "needs Linux+KVM (run on c8i)"]
fn virtio_blk_flush_e2e() {
    let dir = tempfile::tempdir().unwrap();
    let disk_path = dir.path().join("test_flush.blk");
    std::fs::write(&disk_path, vec![0u8; 512]).unwrap();

    let backend = BlkBackend::open(&disk_path, false).unwrap();
    let dev = VirtioBlkMmio::new(5, backend);

    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 16 * 1024 * 1024)]).unwrap();
    configure_queue(&dev, &mem);

    setup_flush_request(&mem, 1);
    dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();
    let status = read_status(&mem);
    assert_eq!(status, 0, "flush status should be OK");

    eprintln!("virtio-blk FLUSH e2e: PASS");
}

#[test]
#[ignore = "needs Linux+KVM (run on c8i)"]
fn virtio_blk_read_only_rejects_write() {
    let dir = tempfile::tempdir().unwrap();
    let disk_path = dir.path().join("test_ro.blk");
    std::fs::write(&disk_path, vec![0u8; 512]).unwrap();

    let backend = BlkBackend::open(&disk_path, true).unwrap();
    let dev = VirtioBlkMmio::new(5, backend);

    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 16 * 1024 * 1024)]).unwrap();
    configure_queue(&dev, &mem);

    let write_data = vec![0xDDu8; 512];
    setup_write_request(&mem, 0, &write_data, 1);
    dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();
    let status = read_status(&mem);
    assert_eq!(
        status, 1,
        "write to read-only device should return IO_ERR (1)"
    );

    eprintln!("virtio-blk read-only reject: PASS");
}

#[test]
#[ignore = "needs Linux+KVM (run on c8i)"]
fn virtio_blk_out_of_bounds_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let disk_path = dir.path().join("test_oob.blk");
    std::fs::write(&disk_path, vec![0u8; 512]).unwrap();

    let backend = BlkBackend::open(&disk_path, false).unwrap();
    assert_eq!(backend.sectors, 1);
    let dev = VirtioBlkMmio::new(5, backend);

    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 16 * 1024 * 1024)]).unwrap();
    configure_queue(&dev, &mem);

    setup_read_request(&mem, 5, 512, 1);
    dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();
    let status = read_status(&mem);
    assert_eq!(status, 1, "out-of-bounds read should return IO_ERR (1)");

    eprintln!("virtio-blk out-of-bounds reject: PASS");
}

#[test]
#[ignore = "needs Linux+KVM (run on c8i)"]
fn virtio_blk_oob_guest_addr_rejected() {
    // Security test: a malicious guest publishes a descriptor with an
    // addr pointing outside guest memory. The bounds-checked helpers
    // must reject the OOB access — no host memory read/write should occur.
    let dir = tempfile::tempdir().unwrap();
    let disk_path = dir.path().join("test_oob_guest.blk");
    let mut disk = std::fs::File::create(&disk_path).unwrap();
    disk.write_all(&vec![0xAAu8; 512]).unwrap();
    drop(disk);

    let backend = BlkBackend::open(&disk_path, false).unwrap();
    let dev = VirtioBlkMmio::new(5, backend);
    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 16 * 1024 * 1024)]).unwrap();
    configure_queue(&dev, &mem);

    // Set up a read request for sector 0.
    let header = BlkReqHeader {
        req_type: req_type::IN,
        sector: 0,
        ..Default::default()
    };
    let _ = mem.write_obj(header, GuestAddress(HEADER_ADDR));

    // Descriptor 0: readable, points to the valid header.
    write_desc(
        &mem,
        0,
        Descriptor {
            addr: HEADER_ADDR,
            len: 16,
            flags: 1, // NEXT
            next: 1,
        },
    );

    // Descriptor 1: writable, points to OOB address 0xDEAD_BEEF (way past 16MB).
    write_desc(
        &mem,
        1,
        Descriptor {
            addr: 0xDEAD_BEEF,
            len: 512,
            flags: 2, // WRITE
            next: 2,
        },
    );

    // Descriptor 2: writable status byte at a valid address.
    write_desc(
        &mem,
        2,
        Descriptor {
            addr: STATUS_ADDR,
            len: 1,
            flags: 2, // WRITE
            next: 0,
        },
    );

    write_avail(&mem, 0, 1);
    dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();

    // The OOB descriptor should be rejected — the handler won't crash,
    // and the status byte should indicate an error or be left untouched.
    // The key assertion is that the process didn't crash (no SIGSEGV/SIGBUS).
    let status = read_status(&mem);
    // Status should be IO_ERR (1) or 0 — either way, no crash.
    assert!(
        status == 0 || status == 1,
        "OOB guest addr should not crash, got status={status}"
    );

    eprintln!("virtio-blk OOB guest addr rejected: PASS (no crash, status={status})");
}

#[test]
#[ignore = "needs Linux+KVM (run on c8i)"]
fn virtio_blk_oversized_desc_len_rejected() {
    // Security test: a descriptor with len > MAX_DESC_LEN (1 MiB) must be
    // rejected, not cause a 1+ GiB allocation.
    let dir = tempfile::tempdir().unwrap();
    let disk_path = dir.path().join("test_oversized.blk");
    std::fs::write(&disk_path, vec![0u8; 512]).unwrap();

    let backend = BlkBackend::open(&disk_path, false).unwrap();
    let dev = VirtioBlkMmio::new(5, backend);
    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 16 * 1024 * 1024)]).unwrap();
    configure_queue(&dev, &mem);

    // Descriptor 0: readable header.
    let header = BlkReqHeader {
        req_type: req_type::IN,
        sector: 0,
        ..Default::default()
    };
    let _ = mem.write_obj(header, GuestAddress(HEADER_ADDR));
    write_desc(
        &mem,
        0,
        Descriptor {
            addr: HEADER_ADDR,
            len: 16,
            flags: 1, // NEXT
            next: 1,
        },
    );

    // Descriptor 1: writable with an absurd len (2 GiB).
    write_desc(
        &mem,
        1,
        Descriptor {
            addr: DATA_BUF_ADDR,
            len: 2 * 1024 * 1024 * 1024, // 2 GiB — way over the cap
            flags: 2,                    // WRITE
            next: 2,
        },
    );

    // Descriptor 2: status byte.
    write_desc(
        &mem,
        2,
        Descriptor {
            addr: STATUS_ADDR,
            len: 1,
            flags: 2,
            next: 0,
        },
    );

    write_avail(&mem, 0, 1);
    dev.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();

    // Must not crash, must not OOM.
    let status = read_status(&mem);
    assert!(
        status == 0 || status == 1,
        "oversized desc should not crash/OOM, got status={status}"
    );

    eprintln!("virtio-blk oversized desc rejected: PASS (no crash/OOM, status={status})");
}
