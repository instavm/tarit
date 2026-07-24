#![allow(dead_code)] // Each integration-test binary uses a different subset.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use vmm_core::config::{KernelConfig, MemoryConfig, VcpuConfig, VmConfig, VolumeConfig};
use vmm_core::controller::VmmController;

static NEXT_OVERLAY: AtomicU64 = AtomicU64::new(0);

fn required_fixture(env_name: &str) -> PathBuf {
    let path = std::env::var_os(env_name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{env_name} must point to the candidate test fixture"));
    assert!(
        path.is_file(),
        "{env_name} fixture not found at {}",
        path.display()
    );
    path
}

pub fn kernel_path() -> PathBuf {
    required_fixture("VMM_TEST_KERNEL")
}

pub fn rootfs_path() -> PathBuf {
    required_fixture("VMM_TEST_ROOTFS")
}

pub fn private_overlay_path(label: &str) -> PathBuf {
    let overlay_id = NEXT_OVERLAY.fetch_add(1, Ordering::Relaxed);
    let overlay_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/vmm-integration-overlays")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&overlay_dir).expect("create private test overlay directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&overlay_dir, std::fs::Permissions::from_mode(0o700))
            .expect("secure test overlay directory");
    }
    overlay_dir.join(format!("{}-{label}-{overlay_id}.cow", std::process::id()))
}

pub fn agent_vm_config(memory_mib: u64) -> VmConfig {
    VmConfig {
        kernel: KernelConfig {
            path: kernel_path().to_string_lossy().into_owned(),
            cmdline: "earlycon=uart8250,io,0x3f8,115200n8 console=ttyS0 reboot=k panic=1 \
                pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw \
                init=/usr/sbin/vmm-agent"
                .into(),
            initramfs: None,
        },
        memory: MemoryConfig {
            size_mib: memory_mib,
        },
        vcpus: VcpuConfig { count: 1 },
        volumes: vec![VolumeConfig {
            path: rootfs_path().to_string_lossy().into_owned(),
            read_only: true,
            overlay: Some(
                private_overlay_path("create")
                    .to_string_lossy()
                    .into_owned(),
            ),
        }],
        net: vec![],
    }
}

pub fn assert_guest_exec(controller: &VmmController, command: &str, expected: &str) {
    let started = std::time::Instant::now();
    loop {
        match controller.exec(command, 5_000) {
            Ok((code, output, _)) => {
                assert_eq!(code, 0, "guest command failed: {command}: {output}");
                assert!(
                    output.contains(expected),
                    "guest command returned no {expected:?} marker: {output}"
                );
                return;
            }
            Err(error) if started.elapsed() < std::time::Duration::from_secs(90) => {
                eprintln!("guest not command-ready yet: {error}");
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(error) => panic!("guest command failed to execute: {command}: {error}"),
        }
    }
}
