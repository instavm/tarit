//! Checkpoints against a base: a full snapshot that pauses only for pages
//! changed since the VM's last snapshot (or the one it was restored from).
//! The output must be the same file a plain full snapshot would write.

#![cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]

use std::path::Path;
use vmm_core::controller::VmmController;
use vmm_core::state::VmState;

mod test_support;
use test_support::{agent_vm_config, guest_stdout};

const SEED: &str = "sh -c 'head -c 24M /dev/urandom > /tmp/ram.bin && \
    sha256sum /tmp/ram.bin | cut -c1-16 > /root/ram.sum && rm -f /root/m.* && sync'";
const CHECK: &str = "sh -c 'ls /root | grep \"^m\\.\" | sort | tr \"\\n\" \" \"; \
    [ \"$(sha256sum /tmp/ram.bin | cut -c1-16)\" = \"$(cat /root/ram.sum)\" ] && echo ram-ok'";

/// Snapshot files a test created, removed when it ends (even on failure).
#[derive(Default)]
struct Files(Vec<String>);

impl Drop for Files {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Take a snapshot that also captures the disk, so it restores after the
/// source VM is gone, and take ownership of every file it wrote.
fn take(controller: &VmmController, base: Option<&str>, resume: bool, files: &mut Files) -> String {
    let (path, disks) = controller
        .snapshot_with_options(false, true, base, resume)
        .expect("snapshot");
    for file in std::iter::once(&path).chain(&disks) {
        let identity = vmm_core::gc::OwnedScratchFile::identity_for(Path::new(file))
            .expect("snapshot identity");
        controller
            .release_scratch(file, identity)
            .expect("transfer snapshot ownership");
        files.0.push(file.clone());
    }
    path
}

/// The memory image of a full snapshot: everything after header and state.
fn memory(path: &str) -> Vec<u8> {
    let bytes = std::fs::read(path).expect("read snapshot");
    assert_eq!(&bytes[..4], b"VMSN");
    assert_eq!(
        u16::from_le_bytes([bytes[6], bytes[7]]),
        0,
        "a full snapshot"
    );
    let state_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
    let mem_len = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;
    let mem = bytes[32 + state_len..].to_vec();
    assert_eq!(mem.len(), mem_len, "memory image fills the file");
    mem
}

fn state(controller: &VmmController) -> VmState {
    controller.status().expect("status").state
}

fn mark(controller: &VmmController, n: u32) {
    guest_stdout(controller, &format!("sh -c 'echo x > /root/m.{n} && sync'"));
}

fn check(controller: &VmmController, expected: &str) {
    let seen = guest_stdout(controller, CHECK);
    assert_eq!(
        seen.split_whitespace().collect::<Vec<_>>().join(" "),
        expected
    );
}

fn restore(path: &str) -> VmmController {
    // The snapshot captured its disk; restore gives it a private overlay.
    let controller = VmmController::new();
    controller.restore(path, None).expect("restore");
    controller
}

#[test]
#[ignore = "needs Linux+KVM + VMM_TEST_KERNEL/VMM_TEST_ROOTFS"]
fn snapshot_from_a_base_matches_a_full_snapshot_and_restores() {
    let mut files = Files::default();
    let controller = VmmController::new();
    let mut config = agent_vm_config(256);
    config.vcpus.count = 2;
    controller.create_live(config).expect("boot live guest");
    guest_stdout(&controller, SEED);
    mark(&controller, 1);
    let a = take(&controller, None, false, &mut files);
    mark(&controller, 2);

    // While paused: B against base A, then C as a plain full snapshot of the
    // same memory. Neither changes the paused state.
    controller.pause().expect("pause");
    let b = take(&controller, Some(&a), false, &mut files);
    assert_eq!(state(&controller), VmState::Paused, "no resume requested");
    let c = take(&controller, None, false, &mut files);
    assert_eq!(state(&controller), VmState::Paused);
    assert!(
        memory(&b) == memory(&c),
        "base-assisted memory equals a full snapshot"
    );

    // resume: a VM the caller paused runs again once RAM is captured.
    controller.resume().expect("resume");
    mark(&controller, 3);
    controller.pause().expect("pause");
    let d = take(&controller, Some(&c), true, &mut files);
    assert_eq!(
        state(&controller),
        VmState::Running,
        "resumed after capture"
    );
    mark(&controller, 4);
    // A base that is not the VM's last snapshot falls back to a full one.
    let stale = take(&controller, Some(&a), false, &mut files);
    controller.stop().ok();

    for (path, expected) in [
        (&b, "m.1 m.2 ram-ok"),
        (&d, "m.1 m.2 m.3 ram-ok"),
        (&stale, "m.1 m.2 m.3 m.4 ram-ok"),
    ] {
        let restored = restore(path);
        check(&restored, expected);
        restored.stop().ok();
    }
}

#[test]
#[ignore = "needs Linux+KVM + VMM_TEST_KERNEL/VMM_TEST_ROOTFS"]
fn first_checkpoint_after_restore_uses_the_restored_file_even_once_deleted() {
    let mut files = Files::default();
    let source = VmmController::new();
    source
        .create_live(agent_vm_config(256))
        .expect("boot live guest");
    guest_stdout(&source, SEED);
    mark(&source, 1);
    let a = take(&source, None, false, &mut files);
    source.stop().ok();

    // Restore from a private copy, then delete it: the VMM holds the file
    // open and still uses it as the base of its first checkpoint.
    let copy = format!("{a}.restore");
    std::fs::copy(&a, &copy).expect("copy snapshot");
    let restored = restore(&copy);
    std::fs::remove_file(&copy).expect("delete restored file");
    mark(&restored, 2);
    restored.pause().expect("pause");
    let from_restore = take(&restored, None, false, &mut files);
    let full = take(&restored, None, false, &mut files);
    assert!(
        memory(&from_restore) == memory(&full),
        "checkpoint from the restored file equals a full snapshot"
    );
    restored.stop().ok();

    let again = restore(&from_restore);
    check(&again, "m.1 m.2 ram-ok");
    // virtio-rng keeps serving after a restore (its queue state is part of
    // the snapshot); before, hwrng reads in a restored guest never returned.
    let bytes = guest_stdout(&again, "sh -c 'timeout 10 head -c 64 /dev/hwrng | wc -c'");
    assert_eq!(bytes.trim(), "64", "hwrng read after restore");
    again.stop().ok();
}

#[test]
#[ignore = "needs Linux+KVM + VMM_TEST_KERNEL/VMM_TEST_ROOTFS"]
fn checkpoint_after_lazy_restore_matches_a_full_snapshot() {
    let mut files = Files::default();
    let source = VmmController::new();
    source
        .create_live(agent_vm_config(256))
        .expect("boot live guest");
    guest_stdout(&source, SEED);
    mark(&source, 1);
    let a = take(&source, None, false, &mut files);
    source.stop().ok();

    let restored = VmmController::new();
    let lazy = restored.restore_with_resource_overrides(
        &a,
        None,
        None,
        None,
        tarit_proto::RestoreMemoryPolicy::Lazy,
        None,
    );
    if let Err(error) = lazy {
        eprintln!("skipping: lazy (UFFD) restore unavailable here: {error}");
        return;
    }
    // Reads fault pages in through UFFD; a write dirties one more.
    check(&restored, "m.1 ram-ok");
    mark(&restored, 2);
    restored.pause().expect("pause");
    let from_restore = take(&restored, None, false, &mut files);
    let full = take(&restored, None, false, &mut files);
    assert!(memory(&from_restore) == memory(&full));
    restored.stop().ok();

    let again = restore(&from_restore);
    check(&again, "m.1 m.2 ram-ok");
    again.stop().ok();
}

/// Bytes of `path` that hold data (not holes).
fn allocated(path: &str) -> u64 {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(path).expect("open snapshot");
    let (mut at, mut data) = (0i64, 0u64);
    loop {
        // SAFETY: plain lseek on a descriptor this function owns.
        let start = unsafe { libc::lseek(file.as_raw_fd(), at, libc::SEEK_DATA) };
        if start < 0 {
            return data;
        }
        // SAFETY: as above.
        let end = unsafe { libc::lseek(file.as_raw_fd(), start, libc::SEEK_HOLE) };
        data += (end - start) as u64;
        at = end;
    }
}

fn resident_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("statm");
    let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

#[test]
#[ignore = "needs Linux+KVM + VMM_TEST_KERNEL/VMM_TEST_ROOTFS"]
fn live_snapshot_after_lazy_restore_takes_untouched_ram_from_the_snapshot() {
    let mut files = Files::default();
    let source = VmmController::new();
    source
        .create_live(agent_vm_config(256))
        .expect("boot live guest");
    guest_stdout(&source, SEED);
    mark(&source, 1);
    let a = take(&source, None, false, &mut files);
    source.stop().ok();

    let restored = VmmController::new();
    let lazy = restored.restore_with_resource_overrides(
        &a,
        None,
        None,
        None,
        tarit_proto::RestoreMemoryPolicy::Lazy,
        None,
    );
    if let Err(error) = lazy {
        eprintln!("skipping: lazy (UFFD) restore unavailable here: {error}");
        return;
    }
    mark(&restored, 2);
    let before = resident_bytes();
    let result = restored
        .live_snapshot(vmm_core::live_snapshot::LiveSnapshotConfig::default())
        .expect("live snapshot of a lazily restored guest");
    let grown = resident_bytes().saturating_sub(before);
    for file in std::iter::once(&result.snapshot_path)
        .chain(result.overlay_path.iter())
        .chain(result.integrity_path.iter())
    {
        let identity = vmm_core::gc::OwnedScratchFile::identity_for(Path::new(file))
            .expect("snapshot identity");
        restored
            .release_scratch(file, identity)
            .expect("transfer snapshot ownership");
        files.0.push(file.clone());
    }
    restored.stop().ok();
    // Untouched RAM came from the snapshot file, not through the mapping:
    // the copy neither faulted all 256 MiB into the guest nor wrote the
    // zero pages out.
    assert!(
        grown < 64 << 20,
        "the snapshot faulted {grown} bytes of restored RAM in"
    );
    let data = allocated(&result.snapshot_path);
    assert!(
        data < 128 << 20,
        "the snapshot of a mostly idle 256 MiB guest holds {data} bytes"
    );

    let again = restore(&result.snapshot_path);
    check(&again, "m.1 m.2 ram-ok");
    again.stop().ok();
}
