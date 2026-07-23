//! Volume backend selection shared by controller and CLI boot wiring.

use std::path::PathBuf;

use crate::config::VolumeConfig;
use vmm_devices::virtio::blk_backend::{BlkBackend, BlkBackendError};

pub fn open_volume_backend(vol: &VolumeConfig) -> Result<BlkBackend, BlkBackendError> {
    let path = PathBuf::from(&vol.path);
    match vol.overlay.as_deref() {
        Some(overlay) => BlkBackend::open_cow(&path, &PathBuf::from(overlay)),
        None => BlkBackend::open(&path, vol.read_only),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    use vmm_devices::virtio::blk::{req_type, status, BlkReqHeader};

    fn local_test_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-work")
            .join(format!("{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // Overlay validation rejects group/world-writable directories; pin the
        // mode so the test does not depend on the host umask.
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&dir, perms).unwrap();
        dir
    }

    #[test]
    fn plain_volume_uses_read_only_flag() {
        let dir = local_test_dir("plain-volume");
        let path = dir.join("disk.img");
        fs::write(&path, [0xAA; 512]).unwrap();

        let vol = VolumeConfig {
            path: path.to_string_lossy().into_owned(),
            read_only: true,
            overlay: None,
        };

        let backend = open_volume_backend(&vol).unwrap();
        assert!(backend.read_only);
        assert_eq!(backend.sectors, 1);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn overlay_volume_uses_cow_and_leaves_base_unchanged() {
        let dir = local_test_dir("overlay-volume");
        let base = dir.join("base.img");
        let overlay = dir.join("overlay.cow");
        fs::write(&base, [0xAA; 512]).unwrap();

        let vol = VolumeConfig {
            path: base.to_string_lossy().into_owned(),
            read_only: true,
            overlay: Some(overlay.to_string_lossy().into_owned()),
        };

        let mut backend = open_volume_backend(&vol).unwrap();
        assert!(!backend.read_only);
        assert_eq!(backend.sectors, 1);

        let mut write_buf = [0xBB; 512];
        let write = BlkReqHeader {
            req_type: req_type::OUT,
            reserved: 0,
            sector: 0,
        };
        assert_eq!(backend.service(&write, &mut write_buf), status::OK);

        let mut read_buf = [0; 512];
        let read = BlkReqHeader {
            req_type: req_type::IN,
            reserved: 0,
            sector: 0,
        };
        assert_eq!(backend.service(&read, &mut read_buf), status::OK);
        assert_eq!(read_buf, [0xBB; 512]);
        assert_eq!(fs::read(&base).unwrap(), [0xAA; 512]);
        assert!(overlay.exists());

        fs::remove_dir_all(dir).unwrap();
    }
}
