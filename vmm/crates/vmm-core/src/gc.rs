//! Garbage collection for VMM-owned scratch files.
//!
//! The sweep is intentionally conservative: a file must match a reserved VMM
//! scratch name, be older than the requested age, be a regular file, and not be
//! open by any process on Linux before it is removed.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Scratch file classes the VMM is allowed to collect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScratchKind {
    LiveSnapshot,
    SuspendSnapshot,
    OwnedOverlay,
}

/// One file removed by a GC sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedScratchFile {
    pub path: PathBuf,
    pub kind: ScratchKind,
}

/// Summary of a GC sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    pub removed: Vec<RemovedScratchFile>,
    pub skipped_open: Vec<PathBuf>,
    pub errors: Vec<(PathBuf, String)>,
}

/// A file this VM explicitly created and may unlink on stop/drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedScratchFile {
    path: PathBuf,
    #[cfg(unix)]
    identity: Option<FileIdentity>,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    // File creation (birth) time distinguishes a replacement from the file we
    // remembered even when the OS reuses the inode number (common on Linux).
    // Unlike ctime/mtime it is stable across content writes, so remembering a
    // file that is later written to (e.g. a CoW overlay) still matches on cleanup.
    created: Option<SystemTime>,
}

impl OwnedScratchFile {
    /// Remember the current file at `path`. If it is later replaced by another
    /// process, `remove` will skip it instead of unlinking someone else's file.
    pub fn remember(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            #[cfg(unix)]
            identity: fs::metadata(&path).ok().map(file_identity),
            path,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the file if it still points at the inode this VM created.
    pub fn remove(&self) -> io::Result<bool> {
        if !self.still_points_to_owned_file()? {
            return Ok(false);
        }
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn still_points_to_owned_file(&self) -> io::Result<bool> {
        #[cfg(unix)]
        {
            let Some(identity) = self.identity else {
                return Ok(false);
            };
            match fs::metadata(&self.path) {
                Ok(metadata) => Ok(file_identity(metadata) == identity),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            }
        }
        #[cfg(not(unix))]
        {
            Ok(self.path.exists())
        }
    }
}

/// Build a VMM-owned overlay path in the reserved GC namespace.
pub fn owned_overlay_path(dir: &Path, index: usize) -> PathBuf {
    static OWNED_OVERLAY_SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = OWNED_OVERLAY_SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(
        "vmm-ov-{}-{ts}-{seq}-{index}.cow",
        std::process::id()
    ))
}

/// Return true only for reserved VMM scratch overlay names.
pub fn is_owned_overlay_name(path: &Path) -> bool {
    path.file_name().and_then(scratch_kind_for_name) == Some(ScratchKind::OwnedOverlay)
}

/// Remove orphaned VMM scratch files in `dir` older than `max_age`.
pub fn gc_scratch_files(dir: &Path, max_age: Duration) -> io::Result<GcReport> {
    let now = SystemTime::now();
    let mut report = GcReport::default();

    for entry in fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                report.errors.push((dir.to_path_buf(), e.to_string()));
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(e) => {
                report.errors.push((path, e.to_string()));
                continue;
            }
        };
        if !file_type.is_file() {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(e) => {
                report.errors.push((path, e.to_string()));
                continue;
            }
        };
        let Some(kind) =
            should_collect_entry(&entry.file_name(), metadata.modified().ok(), now, max_age)
        else {
            continue;
        };
        if has_open_owner(&path) {
            report.skipped_open.push(path);
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => report.removed.push(RemovedScratchFile { path, kind }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => report.errors.push((path, e.to_string())),
        }
    }

    Ok(report)
}

fn should_collect_entry(
    name: &OsStr,
    modified: Option<SystemTime>,
    now: SystemTime,
    max_age: Duration,
) -> Option<ScratchKind> {
    let kind = scratch_kind_for_name(name)?;
    let modified = modified?;
    let age = now.duration_since(modified).ok()?;
    (age >= max_age).then_some(kind)
}

fn scratch_kind_for_name(name: &OsStr) -> Option<ScratchKind> {
    let name = name.to_str()?;
    if name == "vmm-live.snap" || is_live_snapshot_name(name) {
        return Some(ScratchKind::LiveSnapshot);
    }
    if is_suspend_snapshot_name(name) {
        return Some(ScratchKind::SuspendSnapshot);
    }
    if is_owned_overlay_file_name(name) {
        return Some(ScratchKind::OwnedOverlay);
    }
    None
}

fn is_live_snapshot_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("vmm-live-")
        .and_then(|s| s.strip_suffix(".snap"))
    else {
        return false;
    };
    has_pid_timestamp(rest)
}

fn is_suspend_snapshot_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(".vmm-suspend-")
        .and_then(|s| s.strip_suffix(".snap"))
    else {
        return false;
    };
    has_pid_timestamp(rest)
}

fn is_owned_overlay_file_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("vmm-ov-")
        .and_then(|s| s.strip_suffix(".cow"))
    else {
        return false;
    };
    has_pid_timestamp(rest)
}

fn has_pid_timestamp(rest: &str) -> bool {
    let mut parts = rest.splitn(3, '-');
    matches!(
        (parts.next(), parts.next()),
        (Some(pid), Some(ts)) if !pid.is_empty()
            && !ts.is_empty()
            && pid.chars().all(|c| c.is_ascii_digit())
            && ts.chars().all(|c| c.is_ascii_digit())
    )
}

#[cfg(unix)]
fn file_identity(metadata: fs::Metadata) -> FileIdentity {
    FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        created: metadata.created().ok(),
    }
}

#[cfg(target_os = "linux")]
fn has_open_owner(path: &Path) -> bool {
    let Ok(target) = fs::metadata(path) else {
        return false;
    };
    let target = file_identity(target);
    let Ok(proc_dir) = fs::read_dir("/proc") else {
        return false;
    };

    for proc_entry in proc_dir.flatten() {
        let name = proc_entry.file_name();
        if !name.to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = fs::read_dir(proc_entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(metadata) = fs::metadata(fd.path()) else {
                continue;
            };
            if file_identity(metadata) == target {
                return true;
            }
        }
    }

    false
}

#[cfg(not(target_os = "linux"))]
fn has_open_owner(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old() -> SystemTime {
        SystemTime::UNIX_EPOCH
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(10_000)
    }

    #[test]
    fn gc_selects_only_old_vmm_scratch_patterns() {
        let max_age = Duration::from_secs(60);

        assert_eq!(
            should_collect_entry(OsStr::new("vmm-live.snap"), Some(old()), now(), max_age),
            Some(ScratchKind::LiveSnapshot)
        );
        assert_eq!(
            should_collect_entry(
                OsStr::new(".vmm-suspend-123-456.snap"),
                Some(old()),
                now(),
                max_age
            ),
            Some(ScratchKind::SuspendSnapshot)
        );
        assert_eq!(
            should_collect_entry(
                OsStr::new("vmm-ov-123-456.cow"),
                Some(old()),
                now(),
                max_age
            ),
            Some(ScratchKind::OwnedOverlay)
        );

        assert_eq!(
            should_collect_entry(OsStr::new("random.snap"), Some(old()), now(), max_age),
            None
        );
        assert_eq!(
            should_collect_entry(OsStr::new("vmm-123-456.snap"), Some(old()), now(), max_age),
            None
        );
        assert_eq!(
            should_collect_entry(OsStr::new("vmm-ov-user.cow"), Some(old()), now(), max_age),
            None
        );
    }

    #[test]
    fn gc_skips_fresh_scratch_files() {
        let max_age = Duration::from_secs(60);
        let fresh = now() - Duration::from_secs(10);

        assert_eq!(
            should_collect_entry(OsStr::new("vmm-live.snap"), Some(fresh), now(), max_age),
            None
        );
        assert_eq!(
            should_collect_entry(
                OsStr::new(".vmm-suspend-123-456.snap"),
                Some(fresh),
                now(),
                max_age
            ),
            None
        );
        assert_eq!(
            should_collect_entry(
                OsStr::new("vmm-ov-123-456.cow"),
                Some(fresh),
                now(),
                max_age
            ),
            None
        );
    }

    #[test]
    fn owned_scratch_remove_does_not_delete_replaced_path() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-work/gc-owned-replacement")
            .join(format!("{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vmm-live.snap");
        fs::write(&path, b"owned").unwrap();
        let owned = OwnedScratchFile::remember(&path);
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();

        assert!(!owned.remove().unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"replacement");

        fs::remove_dir_all(dir).unwrap();
    }
}
