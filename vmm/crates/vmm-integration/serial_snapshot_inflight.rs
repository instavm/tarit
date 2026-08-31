//! Real-KVM UART state validation across snapshot, pause, and restore.

#![cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]

use std::sync::Arc;
use std::time::Duration;

use vmm_core::controller::VmmController;

mod test_support;
use test_support::{agent_vm_config, assert_guest_exec, private_overlay_path};

fn retain_snapshot(controller: &VmmController, path: &str) {
    let identity = vmm_core::gc::OwnedScratchFile::identity_for(std::path::Path::new(path))
        .expect("snapshot identity");
    controller
        .release_scratch(path, identity)
        .expect("transfer snapshot ownership");
}

#[test]
#[ignore = "needs Linux+KVM + VMM_TEST_KERNEL/VMM_TEST_ROOTFS"]
fn serial_inflight_snapshot_restore() {
    // This test has its own process so the transport override cannot affect
    // concurrently running integration tests.
    std::env::set_var("VMM_VSOCK_EXEC", "0");

    let source = Arc::new(VmmController::new());
    source
        .create_live(agent_vm_config(256))
        .expect("boot serial source");
    assert_guest_exec(&source, "printf serial-ready", "serial-ready");

    let exec_source = Arc::clone(&source);
    let active_exec = std::thread::spawn(move || {
        exec_source.exec(
            "i=0; while [ \"$i\" -lt 300 ]; do printf x; i=$((i+1)); sleep 0.01; done; printf serial-tail",
            30_000,
        )
    });
    std::thread::sleep(Duration::from_millis(250));
    let snapshot = source
        .snapshot(false)
        .expect("snapshot during serial transmission");
    retain_snapshot(&source, &snapshot);

    let (code, output, stderr, _) = active_exec
        .join()
        .expect("join serial exec")
        .expect("serial exec survives snapshot pause");
    assert_eq!(code, 0, "serial exec failed: {stderr}");
    let output = output.trim_end();
    assert_eq!(output.len(), 311, "serial output was lost or duplicated");
    assert!(output.ends_with("serial-tail"));

    let pause_source = Arc::clone(&source);
    let pause_exec =
        std::thread::spawn(move || pause_source.exec("sleep 1; printf pause-tail", 10_000));
    std::thread::sleep(Duration::from_millis(100));
    source.pause().expect("pause during serial exec");
    source.resume().expect("resume during serial exec");
    let (code, output, stderr, _) = pause_exec
        .join()
        .expect("join pause serial exec")
        .expect("serial exec survives bare pause/resume");
    assert_eq!(code, 0, "pause serial exec failed: {stderr}");
    assert_eq!(output.trim_end(), "pause-tail");

    let restored = VmmController::new();
    restored
        .restore(
            &snapshot,
            Some(
                private_overlay_path("serial-restore")
                    .to_string_lossy()
                    .into_owned(),
            ),
        )
        .expect("restore serial snapshot");
    assert_guest_exec(&restored, "printf restored-serial-ok", "restored-serial-ok");

    restored.stop().expect("stop restored guest");
    source.stop().expect("stop source guest");
    std::fs::remove_file(snapshot).expect("remove retained serial snapshot");
}
