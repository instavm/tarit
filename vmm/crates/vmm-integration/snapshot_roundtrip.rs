//! Snapshot/restore correctness — snapshot a VM with in-progress compute,
//! restore, assert continuity. Requires KVM; gated.

#![cfg(test)]

#[cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]
use std::path::{Path, PathBuf};

#[cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]
mod test_support;

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

#[cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]
struct RestoreCloneArtifacts {
    dir: PathBuf,
    snapshots: Vec<PathBuf>,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]
impl Drop for RestoreCloneArtifacts {
    fn drop(&mut self) {
        for snapshot in &self.snapshots {
            let _ = std::fs::remove_file(snapshot);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
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

    let kernel = test_support::kernel_path();
    let base_rootfs = test_support::rootfs_path();

    let dir = local_test_dir("restore-clone-overlays");
    let mut artifacts = RestoreCloneArtifacts {
        dir: dir.clone(),
        snapshots: Vec::new(),
    };
    let golden_overlay = dir.join("golden.overlay");
    let overlay_a = dir.join("clone-a.overlay");
    let overlay_b = dir.join("clone-b.overlay");
    let original_base = std::fs::read(&base_rootfs).expect("read base rootfs");
    let marker = format!("/restore-clone-marker-{}", std::process::id());

    let config = |overlay: &PathBuf| VmConfig {
        kernel: KernelConfig {
            path: kernel.to_string_lossy().into_owned(),
            cmdline: "earlycon=uart8250,io,0x3f8,115200n8 console=ttyS0 reboot=k panic=1 \
                pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw \
                init=/usr/sbin/vmm-agent"
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
    let (code, _, _) = golden
        .exec("true", 30_000)
        .expect("golden must be command-ready before snapshot");
    assert_eq!(code, 0, "golden readiness command must succeed");
    let snap_path = golden.snapshot(false).expect("snapshot golden");
    let snapshot_identity = vmm_core::gc::OwnedScratchFile::identity_for(Path::new(&snap_path))
        .expect("snapshot identity");
    golden
        .release_scratch(&snap_path, snapshot_identity)
        .expect("transfer golden snapshot ownership");
    artifacts.snapshots.push(PathBuf::from(&snap_path));
    golden.stop().ok();
    let original_golden_overlay =
        std::fs::read(&golden_overlay).expect("read reusable golden overlay");

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
    let (code, out, _) = clone_b
        .exec("printf clone-b", 30_000)
        .expect("execute command in clone B");
    assert_eq!(code, 0, "clone B command must succeed: {out}");
    assert!(out.contains("clone-b"));
    clone_b.stop().ok();

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let golden_inode = std::fs::metadata(&golden_overlay)
            .expect("golden overlay metadata")
            .ino();
        assert_ne!(
            std::fs::metadata(&overlay_a)
                .expect("clone A overlay metadata")
                .ino(),
            golden_inode,
            "clone A must not share the golden writable backing file"
        );
        assert_ne!(
            std::fs::metadata(&overlay_b)
                .expect("clone B overlay metadata")
                .ino(),
            golden_inode,
            "clone B must not share the golden writable backing file"
        );
        assert_ne!(
            std::fs::metadata(&overlay_a)
                .expect("clone A overlay metadata")
                .ino(),
            std::fs::metadata(&overlay_b)
                .expect("clone B overlay metadata")
                .ino(),
            "clones must not share a writable backing file"
        );
    }

    assert_eq!(
        std::fs::read(&base_rootfs).expect("reread base rootfs"),
        original_base,
        "base rootfs must stay byte-identical"
    );
    assert_eq!(
        std::fs::read(&golden_overlay).expect("reread reusable golden overlay"),
        original_golden_overlay,
        "golden writable disk state must stay byte-identical after clones run"
    );
}
