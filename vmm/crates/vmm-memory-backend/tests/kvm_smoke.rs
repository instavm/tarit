//! KVM smoke test — verifies /dev/kvm is usable in the test environment.
//!
//! This test actually opens /dev/kvm, creates a VM, and creates a vCPU —
//! proving the KVM path works end-to-end on the host. It does NOT boot a
//! guest (that needs a kernel image + the full boot path, see boot_smoke.rs);
//! it just proves the ioctl layer is functional.
//!
//! Run with: cargo test -p vmm-memory-backend --features kvm -- --include-ignored

#![cfg(all(feature = "kvm", target_os = "linux"))]

use kvm_ioctls::Kvm;
use vmm_memory_backend::GuestMemory;

#[test]
#[ignore = "needs /dev/kvm access (run on a real Linux+KVM host)"]
fn kvm_opens_and_creates_vm() {
    // Open /dev/kvm — the most basic KVM capability check.
    let kvm = Kvm::new().expect("open /dev/kvm");
    let vm_fd = kvm.create_vm().expect("KVM_CREATE_VM");

    // Create guest memory (4 MiB, enough for a smoke test).
    let mem = GuestMemory::new(4 * 1024 * 1024).expect("4 MiB guest memory");

    // Register it with KVM — this is the M3 wiring.
    let slots = mem.register(&vm_fd, 0).expect("KVM_SET_USER_MEMORY_REGION");
    assert!(!slots.is_empty());

    // Create a vCPU — the M5 wiring.
    let _vcpu = vm_fd.create_vcpu(0).expect("KVM_CREATE_VCPU");
}

#[test]
#[ignore = "needs /dev/kvm access (run on a real Linux+KVM host)"]
fn kvm_dirty_log_returns_empty_for_fresh_vm() {
    // KVM_GET_DIRTY_LOG must work on a freshly-created VM (returns
    // an empty bitmap — no pages dirtied yet).
    use vmm_memory_backend::kvm_dirty::read_dirty_log;

    let kvm = Kvm::new().expect("open /dev/kvm");
    let vm_fd = kvm.create_vm().expect("KVM_CREATE_VM");
    let mem = GuestMemory::new(4 * 1024 * 1024).expect("4 MiB");
    let slots = mem
        .register_with_dirty_logging(&vm_fd)
        .expect("register with KVM_MEM_LOG_DIRTY_PAGES");

    let dirty = read_dirty_log(&vm_fd, &mem, &slots).expect("KVM_GET_DIRTY_LOG");
    assert!(dirty.is_empty(), "fresh VM should have no dirty pages");
}
