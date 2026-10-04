//! Real-KVM validation for Linux KVM paravirtual MSR snapshot state.

#![cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use vmm_core::controller::{StateBlob, VmmController};
use vmm_core::live_snapshot::LiveSnapshotConfig;
use vmm_core::vcpu_setup::VcpuFullState;

mod test_support;
use test_support::{agent_vm_config, guest_stdout, private_overlay_path};

const FULL_HEADER_LEN: usize = 32;
const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;
const MSR_KVM_ASYNC_PF_EN: u32 = 0x4b56_4d02;
const MSR_KVM_STEAL_TIME: u32 = 0x4b56_4d03;
const MSR_KVM_PV_EOI_EN: u32 = 0x4b56_4d04;
const MSR_KVM_ASYNC_PF_INT: u32 = 0x4b56_4d06;

fn snapshot_vcpu_msrs(path: &str) -> Vec<BTreeMap<u32, u64>> {
    let bytes = fs::read(path).expect("read snapshot");
    assert!(bytes.len() >= FULL_HEADER_LEN, "short snapshot header");
    assert_eq!(&bytes[..4], b"VMSN", "snapshot magic");
    let state_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
    let state_end = FULL_HEADER_LEN
        .checked_add(state_len)
        .expect("state length overflow");
    assert!(state_end <= bytes.len(), "state blob exceeds snapshot");
    let (state, _) = postcard::take_from_bytes::<StateBlob>(&bytes[FULL_HEADER_LEN..state_end])
        .expect("decode snapshot state");

    let mut encoded_vcpus = vec![state.vcpu_full.expect("BSP full state")];
    encoded_vcpus.extend(state.vcpu_full_aps);
    encoded_vcpus
        .into_iter()
        .map(|encoded| {
            postcard::from_bytes::<VcpuFullState>(&encoded)
                .expect("decode full vCPU state")
                .msrs
                .into_iter()
                .collect()
        })
        .collect()
}

fn assert_pv_state(msrs: &[BTreeMap<u32, u64>], expected_enabled: &[bool]) {
    assert_eq!(msrs.len(), 2, "expected BSP and one AP state");
    assert_eq!(msrs.len(), expected_enabled.len());
    for (vcpu, values) in msrs.iter().enumerate() {
        let get = |index| {
            *values
                .get(&index)
                .unwrap_or_else(|| panic!("vCPU {vcpu} snapshot omitted KVM MSR {index:#010x}"))
        };
        let system_time = get(MSR_KVM_SYSTEM_TIME_NEW);
        let async_pf = get(MSR_KVM_ASYNC_PF_EN);
        let steal_time = get(MSR_KVM_STEAL_TIME);
        let pv_eoi = get(MSR_KVM_PV_EOI_EN);
        let async_pf_vector = get(MSR_KVM_ASYNC_PF_INT);
        assert!(
            (0x20..=0xff).contains(&(async_pf_vector & 0xff)),
            "vCPU {vcpu} async-PF interrupt vector is invalid: {async_pf_vector:#x}"
        );
        if expected_enabled[vcpu] {
            assert_eq!(system_time & 1, 1, "vCPU {vcpu} pvclock disabled");
            assert_eq!(async_pf & 1, 1, "vCPU {vcpu} async-PF disabled");
            assert_eq!(steal_time & 1, 1, "vCPU {vcpu} steal-time disabled");
            assert_eq!(pv_eoi & 1, 1, "vCPU {vcpu} PV-EOI disabled");
        } else {
            for (name, value) in [
                ("pvclock", system_time),
                ("async-PF", async_pf),
                ("steal-time", steal_time),
                ("PV-EOI", pv_eoi),
            ] {
                assert_eq!(value, 0, "vCPU {vcpu} {name} unexpectedly enabled");
            }
        }
    }
}

fn run_offline_restore_online() {
    let mut config = agent_vm_config(512);
    config.vcpus.count = 2;

    let source = Arc::new(VmmController::new());
    source.create_live(config).expect("boot source");
    assert_eq!(guest_stdout(&source, "nproc").trim(), "2");
    let kvm_clock_available = guest_stdout(
        &source,
        "if grep -qw kvm-clock /sys/devices/system/clocksource/clocksource0/available_clocksource; then echo yes; else echo no; fi",
    );
    assert_eq!(kvm_clock_available.trim(), "yes");
    assert_eq!(
        guest_stdout(
            &source,
            "echo 0 > /sys/devices/system/cpu/cpu1/online; cat /sys/devices/system/cpu/cpu1/online",
        )
        .trim(),
        "0"
    );
    let exec_source = Arc::clone(&source);
    let active_exec = std::thread::spawn(move || {
        exec_source.exec(
            "printf vsock-before-snapshot; sleep 2; printf -- '-after-snapshot'",
            10_000,
        )
    });
    std::thread::sleep(Duration::from_millis(200));
    let snapshot = source.snapshot(false).expect("snapshot source");
    let (exit_code, stdout, stderr, _) = active_exec
        .join()
        .expect("join active vsock exec")
        .expect("active vsock exec must survive the ordinary-snapshot quiesce window");
    assert_eq!(exit_code, 0, "active vsock exec failed: {stderr}");
    assert_eq!(stdout, "vsock-before-snapshot-after-snapshot");
    let identity = vmm_core::gc::OwnedScratchFile::identity_for(std::path::Path::new(&snapshot))
        .expect("snapshot identity");
    source
        .release_scratch(&snapshot, identity)
        .expect("retain snapshot");
    assert_pv_state(&snapshot_vcpu_msrs(&snapshot), &[true, false]);

    let restored = VmmController::new();
    restored
        .restore(
            &snapshot,
            Some(
                private_overlay_path("pv-offline")
                    .to_string_lossy()
                    .into_owned(),
            ),
        )
        .expect("restore snapshot");
    assert_eq!(guest_stdout(&restored, "nproc").trim(), "1");
    let restored_offline_snapshot = restored
        .snapshot(false)
        .expect("snapshot restored guest with AP offline");
    assert_pv_state(
        &snapshot_vcpu_msrs(&restored_offline_snapshot),
        &[true, false],
    );
    assert_eq!(
        guest_stdout(
            &restored,
            "echo 1 > /sys/devices/system/cpu/cpu1/online; nproc",
        )
        .trim(),
        "2"
    );
    let before_jiffies = guest_stdout(&restored, "awk '/^cpu1 /{print $2+$4}' /proc/stat")
        .trim()
        .parse::<u64>()
        .expect("pre-work cpu1 jiffies");
    guest_stdout(
        &restored,
        "taskset -c 1 timeout 2 sh -c 'while :; do :; done' || test $? = 124",
    );
    let after_jiffies = guest_stdout(&restored, "awk '/^cpu1 /{print $2+$4}' /proc/stat")
        .trim()
        .parse::<u64>()
        .expect("post-restore cpu1 jiffies");
    assert!(
        after_jiffies > before_jiffies,
        "restored AP did not make progress"
    );
    let restored = Arc::new(restored);
    let live_exec_controller = Arc::clone(&restored);
    let live_exec = std::thread::spawn(move || {
        live_exec_controller.exec(
            "printf vsock-before-live-snapshot; sleep 2; printf -- '-after-live-snapshot'",
            10_000,
        )
    });
    std::thread::sleep(Duration::from_millis(200));
    let restored_snapshot = restored
        .live_snapshot(LiveSnapshotConfig::default())
        .expect("live snapshot restored guest");
    let (exit_code, stdout, stderr, _) = live_exec
        .join()
        .expect("join live-snapshot vsock exec")
        .expect("active vsock exec must survive the live-snapshot drain window");
    assert_eq!(exit_code, 0, "live-snapshot vsock exec failed: {stderr}");
    assert_eq!(stdout, "vsock-before-live-snapshot-after-live-snapshot");
    assert_pv_state(
        &snapshot_vcpu_msrs(&restored_snapshot.snapshot_path),
        &[true, true],
    );

    restored.stop().expect("stop restored guest");
    source.stop().expect("stop source guest");
    fs::remove_file(&snapshot).expect("remove retained source snapshot");
}

#[test]
#[ignore = "needs Linux+KVM + VMM_TEST_KERNEL/VMM_TEST_ROOTFS"]
fn pv_msr_enabled_and_offline_snapshot_restore() {
    run_offline_restore_online();
}
