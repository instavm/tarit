//! Real-KVM proof that a checksum-valid snapshot with malformed block state is
//! rejected before VM publication, while the untouched snapshot still restores.

#![cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]

use std::fs;
use std::path::Path;

use vmm_core::controller::{StateBlob, VmmController};

mod test_support;
use test_support::{agent_vm_config, guest_stdout, private_overlay_path};

const HEADER_LEN: usize = 32;

fn retain_snapshot(controller: &VmmController, path: &str) {
    let identity =
        vmm_core::gc::OwnedScratchFile::identity_for(Path::new(path)).expect("snapshot identity");
    controller
        .release_scratch(path, identity)
        .expect("transfer snapshot ownership");
}

#[test]
fn checksum_valid_malformed_block_state_fails_before_publication() {
    let source = VmmController::new();
    source
        .create_live(agent_vm_config(256))
        .expect("boot source");
    assert_eq!(guest_stdout(&source, "printf source-ready"), "source-ready");
    let snapshot = source.snapshot(false).expect("snapshot source");
    retain_snapshot(&source, &snapshot);

    let mut bytes = fs::read(&snapshot).expect("read snapshot");
    assert!(bytes.len() >= HEADER_LEN, "snapshot header is truncated");
    assert_eq!(&bytes[..4], b"VMSN");
    let state_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
    let state_end = HEADER_LEN.checked_add(state_len).expect("state overflow");
    assert!(state_end <= bytes.len(), "state exceeds snapshot");
    let state = &bytes[HEADER_LEN..state_end];
    let (mut blob, trailing) =
        postcard::take_from_bytes::<StateBlob>(state).expect("decode valid snapshot state");
    let block = blob
        .virtio_blk
        .first_mut()
        .expect("live root volume block state");
    assert!(!block.is_empty(), "captured block state is empty");
    block.fill(0xff);
    let mut tampered_state = postcard::to_allocvec(&blob).expect("encode tampered state");
    tampered_state.extend_from_slice(trailing);
    assert_eq!(tampered_state.len(), state_len, "state length changed");
    bytes[HEADER_LEN..state_end].copy_from_slice(&tampered_state);
    bytes[16..20].copy_from_slice(&crc32fast::hash(&tampered_state).to_le_bytes());

    let tampered = format!("{snapshot}.malformed-block");
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    std::io::Write::write_all(
        &mut options.open(&tampered).expect("create tampered snapshot"),
        &bytes,
    )
    .expect("write tampered snapshot");

    let rejected = VmmController::new();
    let error = rejected
        .restore(
            &tampered,
            Some(
                private_overlay_path("malformed-block")
                    .to_string_lossy()
                    .into_owned(),
            ),
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("virtio-blk state 0 is malformed"),
        "unexpected rejection: {error}"
    );
    assert!(rejected.status().is_err(), "rejected VM was published");

    let valid = VmmController::new();
    valid
        .restore(
            &snapshot,
            Some(
                private_overlay_path("valid-after-rejection")
                    .to_string_lossy()
                    .into_owned(),
            ),
        )
        .expect("restore untouched snapshot");
    assert_eq!(
        guest_stdout(&valid, "printf valid-restored"),
        "valid-restored"
    );
    valid.stop().expect("stop valid restore");
    source.stop().expect("stop source");

    fs::remove_file(tampered).expect("remove tampered snapshot");
    fs::remove_file(snapshot).expect("remove retained snapshot");
}
