//! Real-KVM proof that restored vsock streams reset and redial before payload.

#![cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use vmm_core::controller::{StateBlob, VmmController};

mod test_support;
use test_support::{agent_vm_config, guest_stdout, private_overlay_path};

const FULL_HEADER_LEN: usize = 32;

fn retain_snapshot(controller: &VmmController, path: &str) {
    let identity = vmm_core::gc::OwnedScratchFile::identity_for(std::path::Path::new(path))
        .expect("snapshot identity");
    controller
        .release_scratch(path, identity)
        .expect("transfer snapshot ownership");
}

fn captured_vsock_connections(path: &str) -> usize {
    let bytes = fs::read(path).expect("read snapshot");
    assert!(bytes.len() >= FULL_HEADER_LEN, "short snapshot header");
    let state_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
    let state_end = FULL_HEADER_LEN
        .checked_add(state_len)
        .expect("state length overflow");
    let (state, _) = postcard::take_from_bytes::<StateBlob>(&bytes[FULL_HEADER_LEN..state_end])
        .expect("decode snapshot state");
    state
        .vsock
        .expect("live snapshot omitted vsock state")
        .connections
        .len()
}

#[test]
#[ignore = "needs Linux+KVM + VMM_TEST_KERNEL/VMM_TEST_ROOTFS"]
fn restored_vsock_resets_before_clone_repair_and_payload() {
    std::env::remove_var("VMM_VSOCK_EXEC");

    let source = Arc::new(VmmController::new());
    source
        .create_live(agent_vm_config(256))
        .expect("boot vsock source");
    assert_eq!(guest_stdout(&source, "printf vsock-ready"), "vsock-ready");

    let exec_source = Arc::clone(&source);
    let active_exec = std::thread::spawn(move || {
        exec_source.exec("printf vsock-before; sleep 2; printf -- '-after'", 10_000)
    });
    std::thread::sleep(Duration::from_millis(200));
    let snapshot = source
        .snapshot(false)
        .expect("snapshot with active vsock stream");
    retain_snapshot(&source, &snapshot);
    assert_eq!(
        captured_vsock_connections(&snapshot),
        1,
        "snapshot did not capture the active vsock generation"
    );

    let (code, output, stderr, _) = active_exec
        .join()
        .expect("join source vsock exec")
        .expect("source vsock exec survives snapshot");
    assert_eq!(code, 0, "source vsock exec failed: {stderr}");
    assert_eq!(output, "vsock-before-after");

    let restored = VmmController::new();
    restored
        .restore(
            &snapshot,
            Some(
                private_overlay_path("vsock-ordering-restore")
                    .to_string_lossy()
                    .into_owned(),
            ),
        )
        .expect("restore must complete reset, redial, and clone repair");
    assert_eq!(
        guest_stdout(&restored, "printf post-redial-payload"),
        "post-redial-payload"
    );
    assert_eq!(guest_stdout(&source, "printf source-live"), "source-live");

    restored.stop().expect("stop restored guest");
    source.stop().expect("stop source guest");
    fs::remove_file(snapshot).expect("remove retained vsock snapshot");
}
