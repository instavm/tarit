//! Live virtio-mem resize on a running guest: grow, shrink to zero, and check
//! that the guest sees the change and that unplugged RAM leaves host RSS.
//!
//! Needs Linux+KVM, `VMM_TEST_KERNEL` with CONFIG_VIRTIO_MEM +
//! MEMORY_HOTPLUG/HOTREMOVE, and `VMM_TEST_ROOTFS` with the Tarit agent:
//!
//! ```sh
//! VMM_TEST_KERNEL=... VMM_TEST_ROOTFS=... cargo test -p vmm-integration \
//!   --features kvm --test memory_hotplug_live -- --ignored --nocapture
//! ```

#![cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]

use std::time::Duration;
use vmm_core::controller::VmmController;

mod test_support;
use test_support::{agent_vm_config, guest_stdout};

const BOOT_MIB: u64 = 1024;
const MAX_MIB: u64 = 3072;

fn guest_mem_total_mib(controller: &VmmController) -> u64 {
    let out = guest_stdout(controller, "cat /proc/meminfo");
    let kib: u64 = out
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no MemTotal in {out}"));
    kib / 1024
}

fn host_rss_mib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .expect("VmRSS")
        / 1024
}

/// Wait until guest MemTotal is within `slack` MiB below `expected`.
fn await_guest_total(controller: &VmmController, expected: u64, slack: u64) -> u64 {
    let started = std::time::Instant::now();
    loop {
        let total = guest_mem_total_mib(controller);
        if total <= expected && total + slack >= expected {
            return total;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "guest MemTotal {total} MiB never reached ~{expected} MiB"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
#[ignore = "needs Linux+KVM + VMM_TEST_KERNEL/VMM_TEST_ROOTFS with virtio-mem"]
fn live_grow_and_shrink_to_zero() {
    let controller = VmmController::new();
    let mut config = agent_vm_config(MAX_MIB);
    config.memory.boot_size_mib = Some(BOOT_MIB);
    // ZONE_MOVABLE keeps kernel allocations out of hotplugged memory so the
    // shrink below is deterministic.
    config
        .kernel
        .cmdline
        .push_str(" memhp_default_state=online_movable");
    controller.create_live(config).expect("boot hotplug guest");

    let initial = controller.memory_hotplug_status().unwrap();
    assert_eq!(initial.boot_memory_mib, BOOT_MIB);
    assert_eq!(initial.total_size_mib, MAX_MIB - BOOT_MIB);
    assert_eq!(initial.plugged_size_mib, 0);
    let boot_total = guest_mem_total_mib(&controller);
    // Kernel reservations make MemTotal a little below boot RAM.
    assert!(boot_total <= BOOT_MIB && boot_total + 256 >= BOOT_MIB);
    eprintln!(
        "boot: MemTotal {boot_total} MiB, host RSS {} MiB",
        host_rss_mib()
    );

    let grown = controller
        .set_memory_hotplug(1024, Some(30_000))
        .expect("grow to 1024 MiB hotplug");
    assert_eq!(grown.plugged_size_mib, 1024);
    let grown_total = await_guest_total(&controller, boot_total + 1024, 64);
    // Fill most of the new RAM from the guest, then free it again.
    guest_stdout(
        &controller,
        "sh -c 'mkdir -p /tmp/hp && mount -t tmpfs -o size=1200m tmpfs /tmp/hp && \
         dd if=/dev/urandom of=/tmp/hp/fill bs=1M count=900 2>/dev/null; sync'",
    );
    let rss_touched = host_rss_mib();
    guest_stdout(&controller, "sh -c 'rm -f /tmp/hp/fill; umount /tmp/hp'");
    eprintln!("grown: MemTotal {grown_total} MiB, host RSS after fill {rss_touched} MiB");

    let empty = controller
        .set_memory_hotplug(0, Some(60_000))
        .expect("shrink to zero");
    assert_eq!(empty.plugged_size_mib, 0);
    assert_eq!(empty.requested_size_mib, 0);
    let shrunk_total = await_guest_total(&controller, boot_total, 64);
    let rss_after = host_rss_mib();
    eprintln!("shrunk: MemTotal {shrunk_total} MiB, host RSS {rss_after} MiB");
    // At least the unplugged 1 GiB minus whatever the guest kept resident in
    // boot RAM must have been returned to the host.
    assert!(
        rss_after + 512 <= rss_touched,
        "host RSS did not drop after unplug: {rss_touched} -> {rss_after} MiB"
    );

    // Invalid targets fail without side effects.
    assert!(controller.set_memory_hotplug(MAX_MIB, None).is_err());
    assert!(controller.set_memory_hotplug(100, None).is_err());
    assert_eq!(
        controller
            .memory_hotplug_status()
            .unwrap()
            .requested_size_mib,
        0
    );
    controller.stop().expect("stop");
}
