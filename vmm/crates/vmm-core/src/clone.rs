//! Clone fan-out — restore N instances from one base snapshot.
//!
//! PaaS use case: snapshot a VM with Node.js installed, then stamp out
//! 100 clones for burst traffic. Each clone gets:
//! - Independent memory (UFFD lazy fault from the shared snapshot mmap)
//! - Independent disk (sparse CoW overlay)
//! - Independent network (unique MAC + netns + tap)
//! - Independent PRNG (virtio-rng re-seeds from /dev/urandom)
//! - Independent clock (fresh kvmclock base)
//!
//! Target: <10ms per clone for the hand-off (UFFD returns immediately;
//! pages fault in on demand during guest execution).

use crate::config::NetConfig;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
use std::time::Instant;

/// A single clone's configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneSpec {
    pub id: String,
    pub snapshot_path: String,
    /// CoW overlay path for this clone's disk.
    pub overlay_path: Option<String>,
    /// Network config (unique MAC + tap per clone).
    pub net: Option<NetConfig>,
}

/// Result of cloning N instances.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneResult {
    pub cloned: Vec<CloneSpec>,
    pub total_ms: u64,
    pub per_clone_ms: f64,
}

/// Build clone specs for N instances from a base snapshot.
///
/// Each clone gets:
/// - A unique ID: `{base_id}-{i}`
/// - A CoW overlay path over the base volume
/// - A unique MAC: `02:00:00:00:HI:LO`
/// - A unique tap name: `{base_id}{i}tap0`
pub fn build_clone_specs(
    base_id: &str,
    snapshot_path: &str,
    base_volume: Option<&str>,
    n: u32,
    overlay_dir: &str,
) -> Vec<CloneSpec> {
    let mut specs = Vec::with_capacity(n as usize);
    for i in 0..n {
        let mac = format!("02:00:00:00:{:02x}:{:02x}", (i >> 8) & 0xff, i & 0xff);
        let overlay = base_volume.map(|_| {
            crate::gc::owned_overlay_path(Path::new(overlay_dir), i as usize)
                .to_string_lossy()
                .into_owned()
        });

        let net = NetConfig {
            tap: format!("{base_id}{i}tap0"),
            guest_mac: Some(mac),
            guest_ip: Some(format!("172.16.{}.{}", i / 256, i % 256)),
            port_forwards: vec![],
        };

        specs.push(CloneSpec {
            id: format!("{base_id}-{i}"),
            snapshot_path: snapshot_path.to_string(),
            overlay_path: overlay,
            net: Some(net),
        });
    }
    specs
}

/// Clone fan-out: restore N instances from a base snapshot.
///
/// This is the PaaS "burst of 100" path. Each clone:
/// 1. Creates a fresh KvmVm (new kvmclock → clock reset → CRNG re-seed)
/// 2. UFFD-registers the memory with the snapshot file (lazy fault-in)
/// 3. Creates a sparse CoW overlay for the disk
/// 4. Sets up a unique tap + netns
///
/// The actual VM boot happens lazily — pages fault in on demand during
/// guest execution. The hand-off (creating the VM + registering UFFD)
/// is <10ms per clone.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
pub fn clone_fanout(
    controller: &crate::controller::VmmController,
    specs: &[CloneSpec],
    _base_volume: Option<&str>,
) -> CloneResult {
    let start = Instant::now();
    let mut cloned = Vec::new();

    for spec in specs {
        // Restore from the base snapshot. Each restore creates a fresh
        // sparse CoW overlay when `overlay_path` is set, so the clone never
        // reuses the golden snapshot's saved upper layer.
        //
        // Each restore also creates a fresh
        // KvmVm with its own kvmclock → the guest detects the clock jump
        // and re-seeds its CRNG from virtio-rng.
        match controller.restore(&spec.snapshot_path, spec.overlay_path.clone()) {
            Ok(()) => {
                log::info!(
                    "clone {}: restored (overlay={})",
                    spec.id,
                    spec.overlay_path.as_deref().unwrap_or("none")
                );
                cloned.push(spec.clone());
            }
            Err(e) => {
                log::warn!("clone {}: failed: {e}", spec.id);
                if let Some(overlay) = spec.overlay_path.as_deref() {
                    let _ = std::fs::remove_file(overlay);
                }
            }
        }
    }

    let total_ms = start.elapsed().as_millis() as u64;
    let per_clone_ms = if cloned.is_empty() {
        0.0
    } else {
        total_ms as f64 / cloned.len() as f64
    };

    CloneResult {
        cloned,
        total_ms,
        per_clone_ms,
    }
}

/// Create a copy-on-write overlay from a base volume using `copy_file_range`
/// (Linux). On filesystems that support reflinks (btrfs, XFS), this creates
/// a true CoW copy. On others, it falls back to a full copy.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub fn create_cow_overlay(base_path: &str, overlay_path: &str) -> Result<(), String> {
    use std::fs;
    use std::os::unix::io::AsRawFd;

    let src = fs::File::open(base_path).map_err(|e| format!("open base: {e}"))?;
    let dst = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(overlay_path)
        .map_err(|e| format!("create overlay: {e}"))?;

    let src_size = src.metadata().map_err(|e| format!("metadata: {e}"))?.len();

    // Use copy_file_range for efficient CoW on supported filesystems.
    // SAFETY: `src` and `dst` are valid file descriptors, offsets are null so
    // the kernel uses and updates each fd's current offset, and `src_size` comes
    // from the source file metadata.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_copy_file_range,
            src.as_raw_fd(),
            std::ptr::null::<i64>(),
            dst.as_raw_fd(),
            std::ptr::null::<i64>(),
            src_size as usize,
            0u32,
        )
    };

    if ret < 0 {
        // Fallback: regular copy.
        let mut src = fs::File::open(base_path).map_err(|e| format!("reopen: {e}"))?;
        std::io::copy(
            &mut src,
            &mut fs::File::create(overlay_path).map_err(|e| format!("recreate: {e}"))?,
        )
        .map_err(|e| format!("copy: {e}"))?;
    }

    log::info!("CoW overlay: {base_path} → {overlay_path} ({src_size} bytes)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_specs_creates_n_clones() {
        let specs = build_clone_specs("base", "/snap.bin", Some("/rootfs.ext4"), 5, "/tmp");
        assert_eq!(specs.len(), 5);
        assert_eq!(specs[0].id, "base-0");
        assert_eq!(specs[4].id, "base-4");
    }

    #[test]
    fn each_clone_has_unique_mac_and_tap() {
        let specs = build_clone_specs("base", "/snap.bin", None, 3, "/tmp");
        let macs: Vec<_> = specs
            .iter()
            .map(|s| s.net.as_ref().unwrap().guest_mac.clone())
            .collect();
        let taps: Vec<_> = specs
            .iter()
            .map(|s| s.net.as_ref().unwrap().tap.clone())
            .collect();
        assert_eq!(
            macs.iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
        assert_eq!(
            taps.iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
    }

    #[test]
    fn clone_macs_are_local_unicast() {
        let specs = build_clone_specs("base", "/snap.bin", None, 256, "/tmp");
        for spec in &specs {
            let mac = spec.net.as_ref().unwrap().guest_mac.as_ref().unwrap();
            // First octet 0x02 = locally-administered unicast.
            assert!(mac.starts_with("02:00:00:00:"));
        }
    }

    #[test]
    fn clone_ips_are_unique() {
        let specs = build_clone_specs("base", "/snap.bin", None, 300, "/tmp");
        let ips: Vec<_> = specs
            .iter()
            .map(|s| s.net.as_ref().unwrap().guest_ip.clone())
            .collect();
        assert_eq!(
            ips.iter().collect::<std::collections::HashSet<_>>().len(),
            300
        );
    }

    #[test]
    fn overlay_paths_set_when_volume_provided() {
        let specs = build_clone_specs("base", "/snap.bin", Some("/rootfs.ext4"), 2, "/tmp");
        assert!(specs[0].overlay_path.is_some());
        let overlay = specs[0].overlay_path.as_ref().unwrap();
        assert!(overlay.contains("/vmm-ov-"));
        assert!(overlay.ends_with("-0.cow"));
    }

    #[test]
    fn overlay_paths_none_when_no_volume() {
        let specs = build_clone_specs("base", "/snap.bin", None, 2, "/tmp");
        assert!(specs[0].overlay_path.is_none());
    }
}
