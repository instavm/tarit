//! A booted guest on a real TAP: traffic flows both ways, and every queue
//! kick reaches the device's I/O thread through KVM. A kick KVM does not
//! route exits to the vCPU, which then does the device's work itself.

#![cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]

use std::process::Command;
use vmm_core::config::NetConfig;
use vmm_core::controller::VmmController;

mod test_support;
use test_support::{agent_vm_config, guest_stdout};

const TAP: &str = "vmt-guest";

fn ip(args: &str) -> bool {
    Command::new("ip")
        .args(args.split_whitespace())
        .status()
        .is_ok_and(|status| status.success())
}

/// Deletes the TAP when the test ends, even on failure.
struct TapGuard;

impl Drop for TapGuard {
    fn drop(&mut self) {
        ip(&format!("link delete {TAP}"));
    }
}

#[test]
#[ignore = "needs Linux+KVM, CAP_NET_ADMIN (run under unshare -Urn) + VMM_TEST_KERNEL/VMM_TEST_ROOTFS"]
fn guest_traffic_never_exits_to_the_vmm() {
    ip(&format!("link delete {TAP}"));
    if !ip(&format!("tuntap add dev {TAP} mode tap")) {
        eprintln!("skipping: cannot create a TAP here (needs CAP_NET_ADMIN)");
        return;
    }
    let _tap = TapGuard;
    assert!(ip(&format!("addr add 10.77.0.1/24 dev {TAP}")));
    assert!(ip(&format!("link set {TAP} up")));

    let mut config = agent_vm_config(256);
    config.net = vec![NetConfig {
        tap: TAP.into(),
        guest_mac: None,
        guest_ip: Some("10.77.0.2".into()),
        port_forwards: vec![],
    }];
    let controller = VmmController::new();
    controller
        .create_live(config)
        .expect("boot guest with a TAP");

    let out = guest_stdout(
        &controller,
        "sh -c 'ping -c 50 -i 0.01 -W 2 10.77.0.1 >/dev/null && echo reachable'",
    );
    assert_eq!(
        out.trim(),
        "reachable",
        "guest reaches the host over the TAP"
    );
    let counters = controller.net_counters();
    controller.stop().ok();

    assert_eq!(counters.len(), 1);
    let net = counters[0];
    assert!(net.tx_packets >= 50 && net.rx_packets >= 50, "{net:?}");
    assert_eq!(
        net.vcpu_kicks, 0,
        "queue kicks must reach the I/O thread through KVM: {net:?}"
    );
}
