//! Boot smoke tests for the configured production guest kernel.

#![cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]

use vmm_core::controller::VmmController;
use vmm_loader::load;
use vmm_memory_backend::GuestMemory;

mod test_support;
use test_support::{agent_vm_config, assert_guest_exec, kernel_path};

#[test]
#[ignore = "needs Linux+KVM + VMM_TEST_KERNEL/VMM_TEST_ROOTFS"]
fn boot_to_init_and_halt() {
    let controller = VmmController::new();
    controller
        .create_live(agent_vm_config(256))
        .expect("candidate kernel must boot a live guest");
    assert_guest_exec(
        &controller,
        "bash -c 'echo kernel-boot-runtime-ok'",
        "kernel-boot-runtime-ok",
    );
    controller.stop().expect("candidate VM must stop cleanly");
}

#[test]
#[ignore = "needs VMM_TEST_KERNEL"]
fn loader_accepts_candidate_kernel() {
    let memory = GuestMemory::new(256 * 1024 * 1024).expect("guest memory");
    let loaded = load(
        &memory.inner,
        kernel_path(),
        "console=ttyS0 reboot=k panic=1 nokaslr",
        None,
        memory.size_bytes,
    )
    .expect("load candidate kernel");

    assert!(loaded.entry > 0, "kernel entry point must be nonzero");
    assert!(
        loaded.kernel_end > loaded.entry,
        "loaded kernel range must be nonempty"
    );
}
