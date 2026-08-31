//! Real-KVM proof that delayed host storage cannot occupy a vCPU thread and
//! that snapshot quiescence waits for in-flight block completion.

#![cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    feature = "kvm",
    feature = "test-failpoints"
))]

use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vmm_core::config::VolumeConfig;
use vmm_core::controller::VmmController;

mod test_support;
use test_support::{agent_vm_config, guest_stdout};

fn wait_for_delayed_service(controller: &VmmController, volume_index: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while controller
        .test_block_delayed_services(volume_index)
        .expect("read delayed block request count")
        == 0
    {
        assert!(
            Instant::now() < deadline,
            "delayed block request never started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn delayed_volume_io_isolated_from_vcpu_and_quiesced_for_snapshot() {
    let mut data = tempfile::NamedTempFile::new().expect("create data volume");
    data.as_file_mut()
        .set_len(4 * 1024 * 1024)
        .expect("size data volume");
    data.as_file_mut()
        .seek(SeekFrom::Start(0))
        .expect("seek data volume");
    data.as_file_mut()
        .write_all(&vec![0xa5; 1024])
        .expect("seed data volume");
    data.as_file_mut().sync_all().expect("sync data volume");

    let mut config = agent_vm_config(512);
    config.volumes.push(VolumeConfig {
        path: data.path().to_string_lossy().into_owned(),
        read_only: false,
        overlay: None,
        inherited_fd: None,
    });

    let controller = Arc::new(VmmController::new());
    controller.create_live(config).expect("boot VM");
    assert_eq!(
        guest_stdout(
            &controller,
            "dd if=/dev/vdb of=/dev/null bs=512 count=1 2>/dev/null; printf ready"
        ),
        "ready"
    );

    controller
        .set_test_block_service_delay(1, Duration::from_millis(750))
        .expect("enable volume delay");
    let writer_controller = Arc::clone(&controller);
    let writer = std::thread::spawn(move || {
        writer_controller.exec(
            "dd if=/dev/zero of=/dev/vdb bs=512 count=1 conv=fsync 2>/dev/null; printf write-one",
            15_000,
        )
    });
    wait_for_delayed_service(&controller, 1);

    let pause_started = Instant::now();
    controller
        .test_vcpu_pause_round_trip()
        .expect("vCPU pause round trip during delayed storage");
    let pause_elapsed = pause_started.elapsed();
    assert!(
        pause_elapsed < Duration::from_millis(250),
        "vCPU control path waited on the delayed storage backend: {:?}",
        pause_elapsed
    );
    eprintln!("vCPU pause/resume during delayed storage: {pause_elapsed:?}");
    let (code, stdout, stderr, _) = writer.join().expect("join writer").expect("writer exec");
    assert_eq!(code, 0, "writer failed: {stderr}");
    assert_eq!(stdout, "write-one");

    let snapshot_writer_controller = Arc::clone(&controller);
    let snapshot_writer = std::thread::spawn(move || {
        snapshot_writer_controller.exec(
            "dd if=/dev/zero of=/dev/vdb bs=512 count=1 seek=1 conv=fsync 2>/dev/null; printf write-two",
            15_000,
        )
    });
    wait_for_delayed_service(&controller, 1);
    let snapshot_started = Instant::now();
    let snapshot = controller
        .snapshot(false)
        .expect("snapshot with in-flight delayed block request");
    let snapshot_elapsed = snapshot_started.elapsed();
    assert!(
        snapshot_elapsed >= Duration::from_millis(500),
        "snapshot did not wait for in-flight storage: {:?}",
        snapshot_elapsed
    );
    eprintln!("snapshot quiescence during delayed storage: {snapshot_elapsed:?}");
    let (code, stdout, stderr, _) = snapshot_writer
        .join()
        .expect("join snapshot writer")
        .expect("snapshot writer exec");
    assert_eq!(code, 0, "snapshot writer failed: {stderr}");
    assert_eq!(stdout, "write-two");
    controller
        .set_test_block_service_delay(1, Duration::ZERO)
        .expect("disable volume delay");
    controller.stop().expect("stop VM");

    let mut persisted = [0xff_u8; 1024];
    data.as_file_mut()
        .seek(SeekFrom::Start(0))
        .expect("seek persisted data");
    data.as_file_mut()
        .read_exact(&mut persisted)
        .expect("read persisted data");
    assert_eq!(persisted, [0_u8; 1024]);
    assert!(
        !std::path::Path::new(&snapshot).exists(),
        "owned snapshot was not cleaned up on stop"
    );
}

#[test]
fn storage_quiescence_timeout_fails_snapshot_and_resumes_source() {
    let mut data = tempfile::NamedTempFile::new().expect("create data volume");
    data.as_file_mut()
        .set_len(4 * 1024 * 1024)
        .expect("size data volume");

    let mut config = agent_vm_config(512);
    config.volumes.push(VolumeConfig {
        path: data.path().to_string_lossy().into_owned(),
        read_only: false,
        overlay: None,
        inherited_fd: None,
    });

    let controller = Arc::new(VmmController::new());
    controller.create_live(config).expect("boot VM");
    assert_eq!(
        guest_stdout(&controller, "printf source-ready"),
        "source-ready"
    );

    controller
        .set_test_block_service_delay(1, Duration::from_millis(6_500))
        .expect("enable blocking volume delay");
    let writer_controller = Arc::clone(&controller);
    let writer = std::thread::spawn(move || {
        writer_controller.exec(
            "dd if=/dev/zero of=/dev/vdb bs=512 count=1 conv=fsync 2>/dev/null; printf delayed-write",
            20_000,
        )
    });
    wait_for_delayed_service(&controller, 1);

    let snapshot_started = Instant::now();
    let error = controller
        .snapshot(false)
        .expect_err("snapshot unexpectedly ignored block-worker timeout");
    let snapshot_elapsed = snapshot_started.elapsed();
    assert!(
        error
            .to_string()
            .contains("block I/O worker quiescence timed out"),
        "unexpected snapshot failure: {error}"
    );
    assert!(
        (Duration::from_millis(4_800)..Duration::from_secs(6)).contains(&snapshot_elapsed),
        "block-worker timeout was not bounded at five seconds: {snapshot_elapsed:?}"
    );

    assert_eq!(
        guest_stdout(&controller, "echo source-resumed"),
        "source-resumed\n",
        "snapshot failure left the source vCPU paused"
    );
    let (code, stdout, stderr, _) = writer
        .join()
        .expect("join delayed writer")
        .expect("delayed writer exec");
    assert_eq!(code, 0, "delayed writer failed: {stderr}");
    assert_eq!(stdout, "delayed-write");
    controller.stop().expect("stop VM after quiescence timeout");
}
