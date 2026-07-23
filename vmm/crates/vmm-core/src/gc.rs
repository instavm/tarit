//! Garbage collection for VMM-owned scratch files.
//!
//! The sweep is intentionally conservative: a file must match a reserved VMM
//! scratch name, be older than the requested age, be a regular file, and not be
//! open by any process on Linux before it is removed.

use std::ffi::OsStr;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tarit_proto::ScratchIdentity;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Scratch file classes the VMM is allowed to collect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScratchKind {
    Snapshot,
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
#[derive(Debug)]
pub struct OwnedScratchFile {
    path: PathBuf,
    identity: Option<ScratchIdentity>,
    // Keep the creator's descriptor open for the whole ownership lifetime. This
    // both pins the identity and lets GC detect active artifacts on Linux.
    _file: File,
}

impl OwnedScratchFile {
    /// Atomically create a private regular file and retain its creation identity.
    ///
    /// Existing paths are never adopted: callers can own a file only when this
    /// call created it.
    pub fn create_new(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        Self::from_owned_file(path, file)
    }

    /// Adopt an existing private file supplied by the orchestrator. The open
    /// descriptor and a second path identity check make this an exact-inode
    /// claim; symlinks, hard links, foreign owners, and broad modes are refused.
    #[cfg(unix)]
    pub fn adopt_private(path: impl Into<PathBuf>) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;

        let path = path.into();
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = options.open(&path)?;
        let metadata = file.metadata()?;
        let effective_uid = unsafe { libc::geteuid() };
        let mode = metadata.mode() & 0o777;
        if !metadata.file_type().is_file()
            || metadata.uid() != effective_uid
            || metadata.nlink() != 1
            || mode & 0o077 != 0
            || mode & 0o600 != 0o600
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} must be a private regular file owned by uid {effective_uid}",
                    path.display()
                ),
            ));
        }
        let path_metadata = fs::symlink_metadata(&path)?;
        if !path_metadata.file_type().is_file()
            || file_identity(path_metadata) != file_identity(metadata)
        {
            return Err(io::Error::other(format!(
                "{} changed while ownership was claimed",
                path.display()
            )));
        }
        file.sync_all()?;
        Self::from_owned_file(path, file)
    }

    fn from_owned_file(path: PathBuf, file: File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            #[cfg(unix)]
            identity: Some(file_identity(metadata)),
            #[cfg(not(unix))]
            identity: None,
            path,
            _file: file,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn identity(&self) -> Option<ScratchIdentity> {
        self.identity.clone()
    }

    /// Read an existing regular file's identity without claiming ownership.
    pub fn identity_for(path: &Path) -> io::Result<ScratchIdentity> {
        #[cfg(unix)]
        {
            let metadata = fs::symlink_metadata(path)?;
            if !metadata.file_type().is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a regular file", path.display()),
                ));
            }
            Ok(file_identity(metadata))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "scratch identities require Unix",
            ))
        }
    }

    /// Return whether `identity` is this owned file and the path still names it.
    pub fn matches_identity(&self, identity: &ScratchIdentity) -> bool {
        self.identity.as_ref() == Some(identity)
            && self.still_points_to_owned_file().unwrap_or(false)
    }

    pub fn file(&self) -> &File {
        &self._file
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
            let Some(identity) = self.identity.as_ref() else {
                return Ok(false);
            };
            match fs::symlink_metadata(&self.path) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    Ok(file_identity(metadata) == *identity)
                }
                Ok(_) => Ok(false),
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
    if is_snapshot_name(name) {
        return Some(ScratchKind::Snapshot);
    }
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

fn is_snapshot_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("vmm-snap-")
        .and_then(|s| s.strip_suffix(".snap"))
    else {
        return false;
    };
    has_pid_timestamp(rest)
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
fn file_identity(metadata: fs::Metadata) -> ScratchIdentity {
    let (created_secs, created_nanos) = metadata
        .created()
        .ok()
        .and_then(|created| {
            created
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .and_then(|duration| {
                    i64::try_from(duration.as_secs())
                        .ok()
                        .map(|secs| (Some(secs), Some(duration.subsec_nanos())))
                })
        })
        .unwrap_or((None, None));
    ScratchIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        created_secs,
        created_nanos,
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
            should_collect_entry(
                OsStr::new("vmm-snap-123-456.snap"),
                Some(old()),
                now(),
                max_age
            ),
            Some(ScratchKind::Snapshot)
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
        assert_eq!(
            should_collect_entry(
                OsStr::new("bundle-123e4567-e89b-12d3-a456-426614174000.ram"),
                Some(old()),
                now(),
                max_age
            ),
            None,
            "taritd-owned snapshot bundle names must never be VMM GC candidates"
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
        let owned = OwnedScratchFile::create_new(&path).unwrap();
        fs::write(&path, b"owned").unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();

        assert!(!owned.remove().unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"replacement");

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn owned_scratch_identity_must_match_for_release() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-work/gc-owned-release")
            .join(format!("{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vmm-snap-123-456.snap");
        let owned = OwnedScratchFile::create_new(&path).unwrap();
        let identity = owned.identity().expect("new file has an identity");

        assert!(owned.matches_identity(&identity));
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(!owned.matches_identity(&identity));
        assert_eq!(fs::read(&path).unwrap(), b"replacement");

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_owned_snapshot_cleanup_removes_partial_file() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-work/gc-partial-snapshot")
            .join(format!("{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vmm-snap-123-456.snap");
        let owned = OwnedScratchFile::create_new(&path).unwrap();
        fs::write(&path, b"partial snapshot").unwrap();

        assert!(owned.remove().unwrap());
        assert!(!path.exists(), "partial owned snapshots must be removed");

        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn adopting_private_scratch_rejects_links_and_broad_modes() {
        use std::os::unix::fs::PermissionsExt;

        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-work/gc-adopt-private")
            .join(format!("{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let broad = dir.join("broad.cow");
        fs::write(&broad, b"data").unwrap();
        fs::set_permissions(&broad, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(OwnedScratchFile::adopt_private(&broad).is_err());

        let linked = dir.join("linked.cow");
        let second_link = dir.join("linked-again.cow");
        fs::write(&linked, b"data").unwrap();
        fs::set_permissions(&linked, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&linked, &second_link).unwrap();
        assert!(OwnedScratchFile::adopt_private(&linked).is_err());

        let target = dir.join("target.cow");
        let symlink = dir.join("symlink.cow");
        fs::write(&target, b"data").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, &symlink).unwrap();
        assert!(OwnedScratchFile::adopt_private(&symlink).is_err());

        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_owned_artifact_is_protected_from_gc() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-work/gc-open-owner")
            .join(format!("{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vmm-snap-123-456.snap");
        let owned = OwnedScratchFile::create_new(&path).unwrap();

        assert!(has_open_owner(&path));

        owned.remove().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
