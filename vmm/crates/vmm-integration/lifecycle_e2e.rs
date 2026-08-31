//! E2E lifecycle test — boot via API → snapshot → restore → verify.
//! Also measures cold boot latency (design target <125ms for 100ms PaaS).

#![cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]

use std::sync::Arc;
use std::time::{Duration, Instant};
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
fn e2e_boot_snapshot_restore() {
    let controller = VmmController::new();

    // 1. Boot (Create).
    let t0 = Instant::now();
    controller
        .create_live(agent_vm_config(256))
        .expect("boot live guest");
    assert_guest_exec(
        &controller,
        "bash -c 'echo lifecycle-create-ok'",
        "lifecycle-create-ok",
    );
    let boot_ms = t0.elapsed().as_millis();
    eprintln!("Boot: {boot_ms}ms");

    // 2. Snapshot.
    let t1 = Instant::now();
    let snap_path = controller.snapshot(false).expect("snapshot");
    retain_snapshot(&controller, &snap_path);
    let snap_ms = t1.elapsed().as_millis();
    let snap_size = std::fs::metadata(&snap_path).map(|m| m.len()).unwrap_or(0);
    eprintln!("Snapshot: {snap_ms}ms, {snap_size} bytes");

    // 3. Restore.
    let t2 = Instant::now();
    controller
        .restore(
            &snap_path,
            Some(
                private_overlay_path("lifecycle-restore")
                    .to_string_lossy()
                    .into_owned(),
            ),
        )
        .expect("restore");
    assert_guest_exec(
        &controller,
        "bash -c 'echo lifecycle-restore-ok'",
        "lifecycle-restore-ok",
    );
    let restore_ms = t2.elapsed().as_millis();
    eprintln!("Restore: {restore_ms}ms");

    // 4. Verify the snapshot file is valid (read header + check magic).
    let snap_bytes = std::fs::read(&snap_path).unwrap();
    assert!(snap_bytes.len() >= 32, "snapshot header must fit");
    assert_eq!(
        &snap_bytes[..4],
        b"VMSN",
        "snapshot magic must match canonical VMSN header"
    );
    let version = u16::from_le_bytes(snap_bytes[4..6].try_into().unwrap());
    let flags = u16::from_le_bytes(snap_bytes[6..8].try_into().unwrap());
    let state_len = u64::from_le_bytes(snap_bytes[8..16].try_into().unwrap()) as usize;
    let mem_len = u64::from_le_bytes(snap_bytes[20..28].try_into().unwrap()) as usize;
    assert!(
        32 + state_len <= snap_bytes.len(),
        "snapshot state must fit in file"
    );
    assert!(mem_len > 0, "snapshot must contain memory");
    eprintln!(
        "Snapshot validated: magic=OK, version={version}, flags=0x{flags:x}, state={state_len}B, mem={mem_len}B"
    );

    // 5. Cleanup.
    controller.stop().ok();
    let _ = std::fs::remove_file(&snap_path);

    eprintln!("=== E2E: boot={boot_ms}ms snapshot={snap_ms}ms restore={restore_ms}ms ===");
    assert!(boot_ms > 0, "boot must take some time");
    assert!(snap_size > 0, "snapshot must be non-empty");
}

#[test]
#[ignore = "needs Linux+KVM + VMM_TEST_KERNEL/VMM_TEST_ROOTFS"]
fn serial_inflight_snapshot_restore() {
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

#[test]
#[ignore = "needs Linux+KVM + VMM_TEST_KERNEL/VMM_TEST_ROOTFS"]
fn perf_cold_boot_latency() {
    let mut times = Vec::new();
    for i in 0..5 {
        let controller = VmmController::new();
        // 1. Boot the VM.
        let start = Instant::now();
        controller
            .create_live(agent_vm_config(256))
            .expect("boot live guest");
        assert_guest_exec(
            &controller,
            "bash -c 'echo cold-boot-runtime-ok'",
            "cold-boot-runtime-ok",
        );
        let boot_to_exec = start.elapsed();
        eprintln!("boot-to-exec {i}: {boot_to_exec:?}");
        times.push(boot_to_exec);

        controller.stop().ok();
    }

    times.sort();
    let p50 = times[times.len() / 2];
    let p99 = times[times.len() - 1];
    eprintln!("Cold boot (to exec echo) p50: {:?}", p50);
    eprintln!("Cold boot (to exec echo) p99: {:?}", p99);
    eprintln!("Design target: <125ms (bare metal). Nested virt may be slower.");
}

#[test]
#[ignore = "needs Linux+KVM + VMM_TEST_KERNEL/VMM_TEST_ROOTFS"]
fn stability_soak_10_cycles() {
    for i in 0..10 {
        let controller = VmmController::new();
        controller
            .create_live(agent_vm_config(256))
            .expect("boot live guest");
        assert_guest_exec(
            &controller,
            "bash -c 'echo soak-create-ok'",
            "soak-create-ok",
        );
        let snap_path = controller.snapshot(false).expect("snapshot");
        retain_snapshot(&controller, &snap_path);
        controller
            .restore(
                &snap_path,
                Some(
                    private_overlay_path("soak-restore")
                        .to_string_lossy()
                        .into_owned(),
                ),
            )
            .expect("restore");
        assert_guest_exec(
            &controller,
            "bash -c 'echo soak-restore-ok'",
            "soak-restore-ok",
        );
        controller.stop().ok();
        let _ = std::fs::remove_file(&snap_path);
        eprintln!("soak cycle {i}: OK");
    }
    eprintln!("10 boot/snapshot/restore cycles: all passed");
}
