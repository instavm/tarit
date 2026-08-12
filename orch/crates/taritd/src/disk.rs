use crate::config::DiskPressureConfig;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tarit_types::OrchError;
use uuid::Uuid;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct DiskPressureSnapshot {
    pub(crate) pressured: bool,
    pub(crate) used_bytes: u64,
    pub(crate) used_inodes: u64,
    pub(crate) reserved_bytes: u64,
    pub(crate) reserved_inodes: u64,
    pub(crate) roots: Vec<DiskPressureRootSnapshot>,
    pub(crate) last_removed_files: u64,
    pub(crate) last_removed_jails: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct DiskPressureRootSnapshot {
    pub(crate) path: String,
    pub(crate) pressured: bool,
    pub(crate) used_bytes: u64,
    pub(crate) used_inodes: u64,
    pub(crate) reserved_bytes: u64,
    pub(crate) reserved_inodes: u64,
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
    roots: Mutex<Vec<PressureRoot>>,
    snapshot: Mutex<DiskPressureSnapshot>,
}

#[derive(Debug)]
struct PressureRoot {
    path: PathBuf,
    device: u64,
    latch: PressureLatch,
    reserved_bytes: u64,
    reserved_inodes: u64,
}

pub(crate) struct DiskReservation {
    pressure: Arc<DiskPressure>,
    growth: Vec<FilesystemGrowth>,
    released: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FilesystemGrowth {
    device: u64,
    bytes: u64,
    inodes: u64,
}

pub(crate) struct PathGrowth {
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
    pub(crate) inodes: u64,
}

impl DiskPressure {
    pub(crate) fn new(
        config: DiskPressureConfig,
        filesystem_paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, OrchError> {
        let mut roots = Vec::new();
        for path in filesystem_paths {
            let metadata = std::fs::metadata(&path).map_err(|error| {
                OrchError::Internal(format!(
                    "inspect disk-pressure root {}: {error}",
                    path.display()
                ))
            })?;
            let device = metadata.dev();
            if roots
                .iter()
                .any(|root: &PressureRoot| root.device == device)
            {
                continue;
            }
            roots.push(PressureRoot {
                path,
                device,
                latch: PressureLatch::default(),
                reserved_bytes: 0,
                reserved_inodes: 0,
            });
        }
        if roots.is_empty() {
            return Err(OrchError::Internal(
                "disk pressure requires at least one owned-artifact filesystem".into(),
            ));
        }
        Ok(Self {
            config,
            roots: Mutex::new(roots),
            snapshot: Mutex::new(DiskPressureSnapshot::default()),
        })
    }

    pub(crate) fn sweep_interval(&self) -> Duration {
        Duration::from_secs(self.config.sweep_interval_secs.max(1))
    }

    pub(crate) fn artifact_min_age(&self) -> Duration {
        Duration::from_secs(self.config.artifact_min_age_secs)
    }

    pub(crate) fn refresh(&self) -> Result<DiskPressureSnapshot, OrchError> {
        let mut roots = self
            .roots
            .lock()
            .map_err(|_| OrchError::Internal("disk pressure roots poisoned".into()))?;
        let mut root_snapshots = Vec::with_capacity(roots.len());
        for root in roots.iter_mut() {
            let (used_bytes, used_inodes) = filesystem_usage(&root.path).map_err(|error| {
                OrchError::Internal(format!(
                    "measure disk pressure at {}: {error}",
                    root.path.display()
                ))
            })?;
            let pressured = root.latch.update(
                &self.config,
                used_bytes.saturating_add(root.reserved_bytes),
                used_inodes.saturating_add(root.reserved_inodes),
            );
            root_snapshots.push(DiskPressureRootSnapshot {
                path: root.path.display().to_string(),
                pressured,
                used_bytes,
                used_inodes,
                reserved_bytes: root.reserved_bytes,
                reserved_inodes: root.reserved_inodes,
            });
        }
        drop(roots);
        let mut snapshot = self
            .snapshot
            .lock()
            .map_err(|_| OrchError::Internal("disk pressure state poisoned".into()))?;
        snapshot.pressured = root_snapshots.iter().any(|root| root.pressured);
        snapshot.used_bytes = root_snapshots
            .iter()
            .fold(0u64, |total, root| total.saturating_add(root.used_bytes));
        snapshot.used_inodes = root_snapshots
            .iter()
            .fold(0u64, |total, root| total.saturating_add(root.used_inodes));
        snapshot.reserved_bytes = root_snapshots.iter().fold(0u64, |total, root| {
            total.saturating_add(root.reserved_bytes)
        });
        snapshot.reserved_inodes = root_snapshots.iter().fold(0u64, |total, root| {
            total.saturating_add(root.reserved_inodes)
        });
        snapshot.roots = root_snapshots;
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
                    "node disk pressure blocks {operation} on {}",
                    pressured_roots(&snapshot)
                ),
                retry_after_secs: self.config.sweep_interval_secs.max(1),
            })
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    pub(crate) fn reserve(
        self: &Arc<Self>,
        operation: &str,
        bytes: u64,
        inodes: u64,
    ) -> Result<DiskReservation, OrchError> {
        let paths = self
            .roots
            .lock()
            .map_err(|_| OrchError::Internal("disk pressure roots poisoned".into()))?
            .iter()
            .map(|root| PathGrowth {
                path: root.path.clone(),
                bytes,
                inodes,
            })
            .collect::<Vec<_>>();
        self.reserve_growth(operation, paths)
    }

    pub(crate) fn reserve_growth(
        self: &Arc<Self>,
        operation: &str,
        paths: impl IntoIterator<Item = PathGrowth>,
    ) -> Result<DiskReservation, OrchError> {
        let mut roots = self
            .roots
            .lock()
            .map_err(|_| OrchError::Internal("disk pressure roots poisoned".into()))?;
        let mut requested = Vec::new();
        for growth in paths {
            let device = filesystem_device(&growth.path).map_err(|error| {
                OrchError::Internal(format!(
                    "resolve disk reservation filesystem for {}: {error}",
                    growth.path.display()
                ))
            })?;
            requested.push(FilesystemGrowth {
                device,
                bytes: growth.bytes,
                inodes: growth.inodes,
            });
        }
        let growth = aggregate_filesystem_growth(requested);
        for requested in &growth {
            let root = roots
                .iter()
                .find(|root| root.device == requested.device)
                .ok_or_else(|| {
                    OrchError::Internal(format!(
                        "disk reservation for untracked filesystem device {}",
                        requested.device
                    ))
                })?;
            let space = filesystem_space(&root.path).map_err(|error| {
                OrchError::Internal(format!(
                    "measure disk pressure at {}: {error}",
                    root.path.display()
                ))
            })?;
            let projected_bytes = space
                .used_bytes
                .saturating_add(root.reserved_bytes)
                .saturating_add(requested.bytes);
            let projected_inodes = space
                .used_inodes
                .saturating_add(root.reserved_inodes)
                .saturating_add(requested.inodes);
            let exceeds_reservable_space = root.reserved_bytes.saturating_add(requested.bytes)
                > space.available_bytes
                || root.reserved_inodes.saturating_add(requested.inodes) > space.available_inodes;
            if exceeds_reservable_space
                || self
                    .config
                    .bytes_high
                    .is_some_and(|high| projected_bytes >= high)
                || self
                    .config
                    .inodes_high
                    .is_some_and(|high| projected_inodes >= high)
            {
                return Err(OrchError::Overloaded {
                    message: format!(
                        "node disk pressure blocks {operation} at {} (projected_bytes={projected_bytes}, projected_inodes={projected_inodes})",
                        root.path.display()
                    ),
                    retry_after_secs: self.config.sweep_interval_secs.max(1),
                });
            }
        }
        for requested in &growth {
            let root = roots
                .iter_mut()
                .find(|root| root.device == requested.device)
                .expect("validated reservation device disappeared");
            root.reserved_bytes = root.reserved_bytes.saturating_add(requested.bytes);
            root.reserved_inodes = root.reserved_inodes.saturating_add(requested.inodes);
        }
        drop(roots);
        if let Err(error) = self.refresh() {
            if let Ok(mut roots) = self.roots.lock() {
                for requested in &growth {
                    if let Some(root) = roots
                        .iter_mut()
                        .find(|root| root.device == requested.device)
                    {
                        root.reserved_bytes = root.reserved_bytes.saturating_sub(requested.bytes);
                        root.reserved_inodes =
                            root.reserved_inodes.saturating_sub(requested.inodes);
                    }
                }
            }
            return Err(error);
        }
        Ok(DiskReservation {
            pressure: Arc::clone(self),
            growth,
            released: false,
        })
    }

    pub(crate) fn record_sweep(&self, report: &GcReport) {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.last_removed_files = report.removed_files;
            snapshot.last_removed_jails = report.removed_jails;
        }
    }
}

impl DiskReservation {
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if let Ok(mut roots) = self.pressure.roots.lock() {
            for requested in &self.growth {
                if let Some(root) = roots
                    .iter_mut()
                    .find(|root| root.device == requested.device)
                {
                    root.reserved_bytes = root.reserved_bytes.saturating_sub(requested.bytes);
                    root.reserved_inodes = root.reserved_inodes.saturating_sub(requested.inodes);
                }
            }
        }
        if let Err(error) = self.pressure.refresh() {
            tracing::warn!(%error, "refresh disk pressure after reservation release failed");
        }
    }
}

fn aggregate_filesystem_growth(
    growth: impl IntoIterator<Item = FilesystemGrowth>,
) -> Vec<FilesystemGrowth> {
    let mut aggregated = HashMap::<u64, (u64, u64)>::new();
    for item in growth {
        let totals = aggregated.entry(item.device).or_default();
        totals.0 = totals.0.saturating_add(item.bytes);
        totals.1 = totals.1.saturating_add(item.inodes);
    }
    let mut aggregated = aggregated
        .into_iter()
        .map(|(device, (bytes, inodes))| FilesystemGrowth {
            device,
            bytes,
            inodes,
        })
        .collect::<Vec<_>>();
    aggregated.sort_by_key(|growth| growth.device);
    aggregated
}

fn filesystem_device(path: &Path) -> std::io::Result<u64> {
    let mut candidate = Some(path);
    while let Some(current) = candidate {
        match std::fs::metadata(current) {
            Ok(metadata) => return Ok(metadata.dev()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                candidate = current.parent();
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{} has no existing ancestor", path.display()),
    ))
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        self.release();
    }
}

fn pressured_roots(snapshot: &DiskPressureSnapshot) -> String {
    snapshot
        .roots
        .iter()
        .filter(|root| root.pressured)
        .map(|root| {
            format!(
                "{} (used_bytes={}, reserved_bytes={}, used_inodes={}, reserved_inodes={})",
                root.path,
                root.used_bytes,
                root.reserved_bytes,
                root.used_inodes,
                root.reserved_inodes
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

struct FilesystemSpace {
    used_bytes: u64,
    used_inodes: u64,
    available_bytes: u64,
    available_inodes: u64,
}

fn filesystem_space(path: &Path) -> std::io::Result<FilesystemSpace> {
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
    Ok(FilesystemSpace {
        used_bytes,
        used_inodes,
        available_bytes: stats.f_frsize.saturating_mul(u64::from(stats.f_bavail)),
        available_inodes: u64::from(stats.f_favail),
    })
}

fn filesystem_usage(path: &Path) -> std::io::Result<(u64, u64)> {
    let space = filesystem_space(path)?;
    Ok((space.used_bytes, space.used_inodes))
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
            [root.clone()],
        )
        .unwrap();
        assert!(matches!(
            pressure.ensure_admission("VM create"),
            Err(OrchError::Overloaded { .. })
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reservation_checks_and_reports_each_distinct_artifact_filesystem() {
        let root = test_root("reservation");
        let same_device = root.join("jails");
        std::fs::create_dir_all(&same_device).unwrap();
        let (used_bytes, _) = filesystem_usage(&root).unwrap();
        let pressure = Arc::new(
            DiskPressure::new(
                DiskPressureConfig {
                    bytes_high: Some(used_bytes.saturating_add(1024 * 1024 * 1024)),
                    bytes_low: Some(used_bytes),
                    inodes_high: None,
                    inodes_low: None,
                    sweep_interval_secs: 1,
                    artifact_min_age_secs: 1,
                },
                [root.clone(), same_device],
            )
            .unwrap(),
        );
        let reservation = pressure.reserve("snapshot", 400 * 1024 * 1024, 2).unwrap();
        let snapshot = pressure.snapshot();
        assert_eq!(
            snapshot.roots.len(),
            1,
            "same-device roots are deduplicated"
        );
        assert_eq!(snapshot.reserved_bytes, 400 * 1024 * 1024);
        assert!(pressure.reserve("snapshot", 700 * 1024 * 1024, 2).is_err());
        drop(reservation);
        assert_eq!(pressure.snapshot().reserved_bytes, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_growth_aggregates_peak_bytes_on_one_filesystem() {
        let growth = aggregate_filesystem_growth([
            FilesystemGrowth {
                device: 7,
                bytes: 256,
                inodes: 1,
            },
            FilesystemGrowth {
                device: 7,
                bytes: 256,
                inodes: 1,
            },
            FilesystemGrowth {
                device: 7,
                bytes: 64,
                inodes: 1,
            },
        ]);
        assert_eq!(
            growth,
            vec![FilesystemGrowth {
                device: 7,
                bytes: 576,
                inodes: 3,
            }]
        );
    }

    #[test]
    fn snapshot_growth_reserves_each_filesystem_independently() {
        let growth = aggregate_filesystem_growth([
            FilesystemGrowth {
                device: 1,
                bytes: 256,
                inodes: 1,
            },
            FilesystemGrowth {
                device: 2,
                bytes: 256,
                inodes: 1,
            },
            FilesystemGrowth {
                device: 2,
                bytes: 64,
                inodes: 1,
            },
        ]);
        assert_eq!(
            growth,
            vec![
                FilesystemGrowth {
                    device: 1,
                    bytes: 256,
                    inodes: 1,
                },
                FilesystemGrowth {
                    device: 2,
                    bytes: 320,
                    inodes: 2,
                },
            ]
        );
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
        let staging = snapshots.join(format!(".stage-{}.ram", Uuid::new_v4()));
        let arbitrary = snapshots.join("customer-data");
        let unsafe_link = overlays.join(format!("{}.cow", Uuid::new_v4()));
        for path in [&stale, &referenced, &staging, &arbitrary] {
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
        assert!(
            staging.exists(),
            "in-progress staging names must never be GC eligible"
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
