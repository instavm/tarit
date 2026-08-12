use crate::config::DiskPressureConfig;
use serde::Deserialize;
use std::collections::HashSet;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tarit_types::OrchError;
use uuid::Uuid;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct DiskPressureSnapshot {
    pub(crate) pressured: bool,
    pub(crate) used_bytes: u64,
    pub(crate) used_inodes: u64,
    pub(crate) last_removed_files: u64,
    pub(crate) last_removed_jails: u64,
}

#[derive(Debug, Default)]
struct PressureLatch {
    pressured: bool,
}

impl PressureLatch {
    fn update(&mut self, config: &DiskPressureConfig, used_bytes: u64, used_inodes: u64) -> bool {
        let above_high = config.bytes_high.is_some_and(|high| used_bytes >= high)
            || config.inodes_high.is_some_and(|high| used_inodes >= high);
        let below_all_low = config.bytes_low.is_none_or(|low| used_bytes <= low)
            && config.inodes_low.is_none_or(|low| used_inodes <= low);
        self.pressured = if self.pressured {
            !below_all_low
        } else {
            above_high
        };
        self.pressured
    }
}

pub(crate) struct DiskPressure {
    config: DiskPressureConfig,
    filesystem_path: PathBuf,
    latch: Mutex<PressureLatch>,
    snapshot: Mutex<DiskPressureSnapshot>,
}

impl DiskPressure {
    pub(crate) fn new(config: DiskPressureConfig, filesystem_path: PathBuf) -> Self {
        Self {
            config,
            filesystem_path,
            latch: Mutex::new(PressureLatch::default()),
            snapshot: Mutex::new(DiskPressureSnapshot::default()),
        }
    }

    pub(crate) fn sweep_interval(&self) -> Duration {
        Duration::from_secs(self.config.sweep_interval_secs.max(1))
    }

    pub(crate) fn artifact_min_age(&self) -> Duration {
        Duration::from_secs(self.config.artifact_min_age_secs)
    }

    pub(crate) fn refresh(&self) -> Result<DiskPressureSnapshot, OrchError> {
        let (used_bytes, used_inodes) =
            filesystem_usage(&self.filesystem_path).map_err(|error| {
                OrchError::Internal(format!(
                    "measure disk pressure at {}: {error}",
                    self.filesystem_path.display()
                ))
            })?;
        let pressured = self
            .latch
            .lock()
            .map_err(|_| OrchError::Internal("disk pressure latch poisoned".into()))?
            .update(&self.config, used_bytes, used_inodes);
        let mut snapshot = self
            .snapshot
            .lock()
            .map_err(|_| OrchError::Internal("disk pressure state poisoned".into()))?;
        snapshot.pressured = pressured;
        snapshot.used_bytes = used_bytes;
        snapshot.used_inodes = used_inodes;
        Ok(snapshot.clone())
    }

    pub(crate) fn snapshot(&self) -> DiskPressureSnapshot {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_default()
    }

    pub(crate) fn ensure_admission(&self, operation: &str) -> Result<(), OrchError> {
        let snapshot = self.refresh()?;
        if snapshot.pressured {
            Err(OrchError::Overloaded {
                message: format!(
                    "node disk pressure blocks {operation} (used_bytes={}, used_inodes={})",
                    snapshot.used_bytes, snapshot.used_inodes
                ),
                retry_after_secs: self.config.sweep_interval_secs.max(1),
            })
        } else {
            Ok(())
        }
    }

    pub(crate) fn record_sweep(&self, report: &GcReport) {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.last_removed_files = report.removed_files;
            snapshot.last_removed_jails = report.removed_jails;
        }
    }
}

fn filesystem_usage(path: &Path) -> std::io::Result<(u64, u64)> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("filesystem path contains NUL: {}", path.display()),
        )
    })?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stats = unsafe { stats.assume_init() };
    let used_blocks = u64::from(stats.f_blocks.saturating_sub(stats.f_bavail));
    let used_bytes = stats.f_frsize.saturating_mul(used_blocks);
    let used_inodes = u64::from(stats.f_files.saturating_sub(stats.f_favail));
    Ok((used_bytes, used_inodes))
}

#[derive(Debug, Default)]
pub(crate) struct ArtifactReferences {
    pub(crate) active_vm_ids: HashSet<Uuid>,
    pub(crate) snapshot_paths: HashSet<PathBuf>,
    pub(crate) runtime_paths: HashSet<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GcReport {
    pub(crate) removed_files: u64,
    pub(crate) removed_jails: u64,
}

pub(crate) fn sweep_owned_artifacts(
    socket_dir: &Path,
    jail_base: Option<&Path>,
    references: &ArtifactReferences,
    min_age: Duration,
) -> Result<GcReport, OrchError> {
    let mut report = GcReport::default();
    sweep_owned_files(
        &socket_dir.join("overlays"),
        references,
        min_age,
        is_owned_overlay_name,
        &mut report,
    )?;
    sweep_owned_files(
        &socket_dir.join("snapshots"),
        references,
        min_age,
        is_owned_snapshot_name,
        &mut report,
    )?;
    if let Some(jail_base) = jail_base {
        sweep_owned_jails(jail_base, references, min_age, &mut report)?;
    }
    Ok(report)
}

fn sweep_owned_files(
    root: &Path,
    references: &ArtifactReferences,
    min_age: Duration,
    valid_name: fn(&str) -> bool,
    report: &mut GcReport,
) -> Result<(), OrchError> {
    if !root.exists() {
        return Ok(());
    }
    validate_owned_root(root)?;
    let entries = std::fs::read_dir(root)
        .map_err(|error| OrchError::Internal(format!("scan {}: {error}", root.display())))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| OrchError::Internal(format!("scan {}: {error}", root.display())))?;
        let path = entry.path();
        let file_name = entry.file_name();
        if !file_name.to_str().is_some_and(valid_name) {
            continue;
        }
        if references.snapshot_paths.contains(&path)
            || references.runtime_paths.contains(&path)
            || !old_enough(&path, min_age)?
            || !safe_owned_file(&path)?
        {
            continue;
        }
        std::fs::remove_file(&path).map_err(|error| {
            OrchError::Internal(format!("remove owned artifact {}: {error}", path.display()))
        })?;
        report.removed_files = report.removed_files.saturating_add(1);
    }
    Ok(())
}

#[derive(Deserialize)]
struct JailMarker {
    vm_id: Uuid,
}

fn sweep_owned_jails(
    root: &Path,
    references: &ArtifactReferences,
    min_age: Duration,
    report: &mut GcReport,
) -> Result<(), OrchError> {
    if !root.exists() {
        return Ok(());
    }
    validate_owned_root(root)?;
    for entry in std::fs::read_dir(root)
        .map_err(|error| OrchError::Internal(format!("scan {}: {error}", root.display())))?
    {
        let entry = entry
            .map_err(|error| OrchError::Internal(format!("scan {}: {error}", root.display())))?;
        let path = entry.path();
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix("tarit-"))
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            continue;
        };
        if references.active_vm_ids.contains(&id) || !old_enough(&path, min_age)? {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            OrchError::Internal(format!("inspect jail {}: {error}", path.display()))
        })?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            continue;
        }
        let marker_path = path.join(".tarit-jail.json");
        if !safe_owned_file(&marker_path)? {
            continue;
        }
        let marker: JailMarker =
            serde_json::from_slice(&std::fs::read(&marker_path).map_err(|error| {
                OrchError::Internal(format!(
                    "read jail marker {}: {error}",
                    marker_path.display()
                ))
            })?)
            .map_err(|error| {
                OrchError::Internal(format!(
                    "parse jail marker {}: {error}",
                    marker_path.display()
                ))
            })?;
        if marker.vm_id != id {
            continue;
        }
        std::fs::remove_dir_all(&path).map_err(|error| {
            OrchError::Internal(format!("remove owned jail {}: {error}", path.display()))
        })?;
        report.removed_jails = report.removed_jails.saturating_add(1);
    }
    Ok(())
}

fn validate_owned_root(root: &Path) -> Result<(), OrchError> {
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|error| OrchError::Internal(format!("inspect {}: {error}", root.display())))?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(OrchError::Internal(format!(
            "refuse artifact GC in unsafe root {}",
            root.display()
        )));
    }
    Ok(())
}

fn safe_owned_file(path: &Path) -> Result<bool, OrchError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(OrchError::Internal(format!(
                "inspect owned artifact {}: {error}",
                path.display()
            )))
        }
    };
    Ok(metadata.is_file()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.nlink() == 1
        && metadata.mode() & 0o077 == 0)
}

fn old_enough(path: &Path, min_age: Duration) -> Result<bool, OrchError> {
    let modified = std::fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| {
            OrchError::Internal(format!("read artifact age {}: {error}", path.display()))
        })?;
    Ok(SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        >= min_age)
}

fn is_owned_overlay_name(name: &str) -> bool {
    name.strip_suffix(".cow")
        .and_then(|id| Uuid::parse_str(id).ok())
        .is_some()
}

fn is_owned_snapshot_name(name: &str) -> bool {
    name.strip_prefix("bundle-")
        .and_then(|name| name.strip_suffix(".ram"))
        .or_else(|| name.strip_suffix(".cow"))
        .and_then(|id| Uuid::parse_str(id).ok())
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn test_root(label: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!("disk-{label}-{}", Uuid::new_v4()))
    }

    #[test]
    fn pressure_clears_only_below_low_watermarks() {
        let config = DiskPressureConfig {
            bytes_high: Some(100),
            bytes_low: Some(80),
            inodes_high: Some(20),
            inodes_low: Some(10),
            sweep_interval_secs: 1,
            artifact_min_age_secs: 1,
        };
        let mut latch = PressureLatch::default();
        assert!(latch.update(&config, 101, 1));
        assert!(latch.update(&config, 90, 1));
        assert!(!latch.update(&config, 80, 10));
        assert!(latch.update(&config, 1, 21));
        assert!(!latch.update(&config, 1, 10));
    }

    #[test]
    fn pressure_blocks_admission() {
        let root = test_root("admission");
        std::fs::create_dir_all(&root).unwrap();
        let pressure = DiskPressure::new(
            DiskPressureConfig {
                bytes_high: Some(1),
                bytes_low: Some(0),
                inodes_high: None,
                inodes_low: None,
                sweep_interval_secs: 1,
                artifact_min_age_secs: 1,
            },
            root.clone(),
        );
        assert!(matches!(
            pressure.ensure_admission("VM create"),
            Err(OrchError::Overloaded { .. })
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gc_preserves_references_and_ignores_unsafe_or_unowned_names() {
        let root = test_root("ownership");
        let overlays = root.join("overlays");
        let snapshots = root.join("snapshots");
        std::fs::create_dir_all(&overlays).unwrap();
        std::fs::create_dir_all(&snapshots).unwrap();
        std::fs::set_permissions(&overlays, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&snapshots, std::fs::Permissions::from_mode(0o700)).unwrap();

        let stale = overlays.join(format!("{}.cow", Uuid::new_v4()));
        let referenced = snapshots.join(format!("bundle-{}.ram", Uuid::new_v4()));
        let arbitrary = snapshots.join("customer-data");
        let unsafe_link = overlays.join(format!("{}.cow", Uuid::new_v4()));
        for path in [&stale, &referenced, &arbitrary] {
            std::fs::write(path, b"x").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::hard_link(&stale, &unsafe_link).unwrap();

        let mut references = ArtifactReferences::default();
        references.snapshot_paths.insert(referenced.clone());
        let report = sweep_owned_artifacts(&root, None, &references, Duration::ZERO).unwrap();

        assert!(
            stale.exists(),
            "multiply-linked artifacts must be preserved"
        );
        assert!(
            unsafe_link.exists(),
            "multiply-linked artifacts must be preserved"
        );
        assert!(
            referenced.exists(),
            "durable snapshot references must be preserved"
        );
        assert!(arbitrary.exists(), "unknown names must never be removed");
        assert_eq!(report.removed_files, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gc_removes_only_unreferenced_owned_artifacts() {
        let root = test_root("remove");
        let overlays = root.join("overlays");
        let snapshots = root.join("snapshots");
        std::fs::create_dir_all(&overlays).unwrap();
        std::fs::create_dir_all(&snapshots).unwrap();
        std::fs::set_permissions(&overlays, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&snapshots, std::fs::Permissions::from_mode(0o700)).unwrap();
        let overlay = overlays.join(format!("{}.cow", Uuid::new_v4()));
        let snapshot = snapshots.join(format!("{}.cow", Uuid::new_v4()));
        for path in [&overlay, &snapshot] {
            std::fs::write(path, b"x").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let report =
            sweep_owned_artifacts(&root, None, &ArtifactReferences::default(), Duration::ZERO)
                .unwrap();
        assert_eq!(report.removed_files, 2);
        assert!(!overlay.exists());
        assert!(!snapshot.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
