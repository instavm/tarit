//! Snapshot/restore correctness — snapshot a VM with in-progress compute,
//! restore, assert continuity. Requires KVM; gated.

#![cfg(test)]

#[cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]
use std::path::PathBuf;

#[cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]
fn workspace_path(rel: &str) -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join(rel))
        .unwrap_or_else(|| PathBuf::from(rel))
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]
fn local_test_dir(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        workspace_path("target/test-work").join(format!("{name}-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
#[ignore = "needs KVM + snapshot path (M10)"]
fn snapshot_restore_continuity() {
    // Placeholder: counter + open file + TCP socket survives snapshot/restore.
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]
#[test]
#[ignore = "needs Linux+KVM + guest kernel/rootfs with vmm-agent"]
fn restored_clones_get_private_rootfs_overlays() {
    use vmm_core::config::{KernelConfig, MemoryConfig, VcpuConfig, VmConfig, VolumeConfig};
    use vmm_core::controller::VmmController;

    let kernel = std::env::var("VMM_TEST_KERNEL")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_path("guest/bzImage"));
    let base_rootfs = std::env::var("VMM_TEST_ROOTFS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_path("guest/rootfs.ext4"));
    if !kernel.exists() || !base_rootfs.exists() {
        eprintln!("kernel/rootfs not found — skip");
        return;
    }

    let dir = local_test_dir("restore-clone-overlays");
    let golden_overlay = dir.join("golden.overlay");
    let overlay_a = dir.join("clone-a.overlay");
    let overlay_b = dir.join("clone-b.overlay");
    let original_base = std::fs::read(&base_rootfs).expect("read base rootfs");
    let marker = format!("/restore-clone-marker-{}", std::process::id());

    let config = |overlay: &PathBuf| VmConfig {
        kernel: KernelConfig {
            path: kernel.to_string_lossy().into_owned(),
            cmdline:
                "console=ttyS0 reboot=k panic=1 nokaslr root=/dev/vda rw init=/usr/sbin/vmm-agent"
                    .into(),
            initramfs: None,
        },
        memory: MemoryConfig { size_mib: 256 },
        vcpus: VcpuConfig { count: 1 },
        volumes: vec![VolumeConfig {
            path: base_rootfs.to_string_lossy().into_owned(),
            read_only: true,
            overlay: Some(overlay.to_string_lossy().into_owned()),
        }],
        net: vec![],
    };

    let golden = VmmController::new();
    golden
        .create_live(config(&golden_overlay))
        .expect("boot golden");
    let snap_path = golden.snapshot(false).expect("snapshot golden");
    golden.stop().ok();

    let clone_a = VmmController::new();
    clone_a
        .restore(&snap_path, Some(overlay_a.to_string_lossy().into_owned()))
        .expect("restore clone A");
    let (code, _, _) = clone_a
        .exec(&format!("sh -c 'echo clone-a > {marker} && sync'"), 30_000)
        .expect("write marker in clone A");
    assert_eq!(code, 0, "clone A marker write must succeed");
    clone_a.stop().ok();

    let clone_b = VmmController::new();
    clone_b
        .restore(&snap_path, Some(overlay_b.to_string_lossy().into_owned()))
        .expect("restore clone B");
    let (code, out, _) = clone_b
        .exec(
            &format!("sh -c 'test ! -e {marker} && echo isolated'"),
            30_000,
        )
        .expect("read marker state in clone B");
    assert_eq!(code, 0, "clone B must not see clone A marker: {out}");
    assert!(out.contains("isolated"));
    clone_b.stop().ok();

    assert_eq!(
        std::fs::read(&base_rootfs).expect("reread base rootfs"),
        original_base,
        "base rootfs must stay byte-identical"
    );

    let _ = std::fs::remove_file(&snap_path);
    std::fs::remove_dir_all(dir).unwrap();
}
