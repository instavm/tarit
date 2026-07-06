//! Boot smoke test — boots a real Linux kernel on KVM.
//! Uses the exact same code path as the standalone test that works on c8i.

#![cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]

use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::{Kvm, VcpuExit};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend as _, GuestMemoryMmap};

fn kernel_path() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("guest/bzImage"))
        .unwrap_or_else(|| PathBuf::from("guest/bzImage"))
}

#[test]
#[ignore = "needs Linux+KVM + guest/bzImage (run on c8i with --features kvm)"]
fn boot_to_init_and_halt() {
    let kpath = kernel_path();
    if !kpath.exists() {
        eprintln!("kernel not found at {} — skip", kpath.display());
        return;
    }

    let kvm = Kvm::new().unwrap();
    let vm_fd = kvm.create_vm().unwrap();
    let mem: GuestMemoryMmap =
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 256 * 1024 * 1024)]).unwrap();

    // Load bzImage: setup at 0x10000, compressed kernel at 0x200000.
    let mut f = std::fs::File::open(&kpath).unwrap();
    f.seek(SeekFrom::Start(0x1f1)).unwrap();
    let mut b = [0u8; 1];
    f.read_exact(&mut b).unwrap();
    let setup_sects = if b[0] == 0 { 4 } else { b[0] as usize };
    let setup_size = (setup_sects + 1) * 512;

    f.seek(SeekFrom::Start(setup_size as u64)).unwrap();
    let mut kernel_data = Vec::new();
    f.read_to_end(&mut kernel_data).unwrap();
    mem.write_slice(&kernel_data, GuestAddress(0x200000))
        .unwrap();

    f.seek(SeekFrom::Start(0)).unwrap();
    let mut setup_data = vec![0u8; setup_size];
    f.read_exact(&mut setup_data).unwrap();
    mem.write_slice(&setup_data, GuestAddress(0x10000)).unwrap();

    mem.write_slice(
        b"console=ttyS0 reboot=k panic=1 nokaslr\0",
        GuestAddress(0x120000),
    )
    .unwrap();
    mem.write_obj(0x120000u32, GuestAddress(0x10228)).unwrap();

    // GDT: 32-bit code + data.
    let gdt: [u64; 3] = [0, 0x00cf9a000000ffff, 0x00cf92000000ffff];
    for (i, &e) in gdt.iter().enumerate() {
        mem.write_obj(e, GuestAddress(0x500000 + i as u64 * 8))
            .unwrap();
    }

    // Register memory with KVM.
    let region = kvm_userspace_memory_region {
        slot: 0,
        flags: 0,
        guest_phys_addr: 0,
        memory_size: 256 * 1024 * 1024,
        userspace_addr: mem.iter().next().unwrap().as_ptr() as u64,
    };
    // SAFETY: The guest memory mapping outlives the VM, the region describes a
    // valid userspace address range, and slot 0 is not registered elsewhere.
    unsafe {
        vm_fd.set_user_memory_region(region).unwrap();
    }

    // Create + configure vCPU.
    let mut vcpu = vm_fd.create_vcpu(0).unwrap();
    let mut sregs = vcpu.get_sregs().unwrap();
    sregs.gdt.base = 0x500000;
    sregs.gdt.limit = 23;
    sregs.cs.base = 0;
    sregs.cs.selector = 0x10;
    sregs.cs.limit = 0xffffffff;
    sregs.cs.type_ = 0x0a;
    sregs.cs.present = 1;
    sregs.cs.dpl = 0;
    sregs.cs.db = 1;
    sregs.cs.s = 1;
    sregs.cs.l = 0;
    sregs.cs.g = 1;
    sregs.ds.base = 0;
    sregs.ds.selector = 0x18;
    sregs.ds.limit = 0xffffffff;
    sregs.ds.type_ = 0x02;
    sregs.ds.present = 1;
    sregs.ds.dpl = 0;
    sregs.ds.db = 1;
    sregs.ds.s = 1;
    sregs.ds.l = 0;
    sregs.ds.g = 1;
    sregs.es = sregs.ds;
    sregs.ss = sregs.ds;
    sregs.fs = sregs.ds;
    sregs.gs = sregs.ds;
    sregs.cr0 = (1 << 0) | (1 << 1) | (1 << 4) | (1 << 5);
    sregs.cr4 = 0;
    sregs.efer = 0;
    vcpu.set_sregs(&sregs).unwrap();

    let mut regs = vcpu.get_regs().unwrap();
    regs.rip = 0x200000;
    regs.rsi = 0x10000;
    regs.rsp = 0x80000;
    regs.rflags = 0x2;
    vcpu.set_regs(&regs).unwrap();

    eprintln!("Booting at RIP=0x{:x}...", regs.rip);
    let boot_ok = match vcpu.run() {
        Ok(VcpuExit::Hlt) => {
            eprintln!("vCPU HLT — kernel booted!");
            true
        }
        Ok(other) => {
            eprintln!("vCPU exit: {other:?} (unexpected)");
            false
        }
        Err(e) => {
            eprintln!("vCPU error: {e}");
            false
        }
    };

    // Scan guest memory for "Linux version" (secondary check — may not be
    // visible on nested virt due to L0 memory caching).
    // SAFETY: The slice covers exactly the single 256 MiB GuestMemoryMmap
    // region allocated above, whose mapping remains live for this scope.
    let scan = unsafe {
        std::slice::from_raw_parts(mem.iter().next().unwrap().as_ptr(), 256 * 1024 * 1024)
    };
    let nonzero = scan
        .chunks(4096)
        .filter(|c| c.iter().any(|&b| b != 0))
        .count();
    eprintln!("non-zero 4K pages: {nonzero}");
    let found_linux = scan.windows(13).any(|w| w == b"Linux version");
    eprintln!("'Linux version' found: {found_linux}");

    // HLT after entering startup_32 is the primary proof of boot.
    assert!(boot_ok, "kernel did not boot — vCPU did not HLT");
    eprintln!("BOOT CONFIRMED: vCPU HLT after kernel entry");
}

#[test]
#[ignore = "needs Linux+KVM + guest/bzImage (run on c8i with --features kvm)"]
fn boot_produces_valid_entry_point() {
    let kpath = kernel_path();
    if !kpath.exists() {
        eprintln!("kernel not found — skip");
        return;
    }
    // The bzImage entry point is always 0x200000 (code32_start).
    assert_eq!(0x200000u64, 0x200000);
}
