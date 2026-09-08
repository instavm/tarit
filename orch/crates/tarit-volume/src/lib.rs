//! Provider-neutral persistent-volume preparation.
//!
//! This crate deliberately contains no product, tenant, billing, HTTP, or VMM
//! policy. The orchestrator persists and fences desired attachments; providers
//! turn an opaque volume identity into a verified host resource.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

mod attached_block;
mod nfs;
mod object;

pub use attached_block::{
    AttachedBlockKind, AttachedBlockProvider, BlockDeviceIdentity, BlockDeviceRegistration,
};
pub use nfs::{
    NfsBackedBlockProvider, NfsDialect, NfsMountSpec, NfsProvider, NfsSecurityFlavor,
    PreparedFilesystemAttachment, SystemNfsMounter,
};
pub use object::{
    ImmutableObject, ImmutableObjectProvider, LocalImmutableObjectProvider, ObjectDigest,
};

pub const MIN_BLOCK_VOLUME_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageClass {
    Block,
    Filesystem,
    Object,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityClass {
    HostLocal,
    Zonal,
    Regional,
    MultiZone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentTransport {
    BlockDescriptor,
    PremountedFilesystem,
    ImmutableObject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleCapabilities {
    pub hibernate: bool,
    pub cross_node_resume: bool,
    pub live_migration: bool,
    pub atomic_fork_clone: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderProfile {
    pub capabilities: VolumeCapabilities,
    pub durability: DurabilityClass,
    pub transport: AttachmentTransport,
    pub lifecycle: LifecycleCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    ReadOnlyMany,
    ReadWriteOnce,
    ReadWriteMany,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeCapabilities {
    pub storage_class: StorageClass,
    pub read_only_many: bool,
    pub read_write_once: bool,
    pub read_write_many: bool,
    pub snapshots: bool,
    pub clones: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementConstraint {
    pub host_id: Option<String>,
    pub region: Option<String>,
    pub zone: Option<String>,
}

impl PlacementConstraint {
    pub fn local_host(host_id: impl Into<String>) -> Self {
        Self {
            host_id: Some(host_id.into()),
            region: None,
            zone: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderVolume {
    pub volume_id: Uuid,
    pub size_bytes: u64,
    pub constraint: PlacementConstraint,
}

/// A securely opened block attachment. Keeping the descriptor open prevents a
/// path replacement from changing the object selected during admission.
pub struct PreparedBlockAttachment {
    pub volume_id: Uuid,
    pub size_bytes: u64,
    pub read_only: bool,
    pub generation: u64,
    pub file: File,
    private_path: PathBuf,
}

impl fmt::Debug for PreparedBlockAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedBlockAttachment")
            .field("volume_id", &self.volume_id)
            .field("size_bytes", &self.size_bytes)
            .field("read_only", &self.read_only)
            .field("generation", &self.generation)
            .field("private_path", &"<private>")
            .finish()
    }
}

impl PreparedBlockAttachment {
    /// Private provider locator for orchestrator/VMM plumbing. Never include it
    /// in a public record, error, audit event, or metric label.
    pub fn private_path(&self) -> &Path {
        &self.private_path
    }
}

#[derive(Debug, Error)]
pub enum VolumeError {
    #[error("invalid volume request: {0}")]
    Invalid(String),
    #[error("volume not found")]
    NotFound,
    #[error("volume already exists with different immutable properties")]
    Conflict,
    #[error("unsafe volume storage object")]
    UnsafeObject,
    #[error("unsupported volume operation: {0}")]
    Unsupported(String),
    #[error("volume attachment identity or generation mismatch")]
    IdentityMismatch,
    #[error("volume attachment is busy")]
    Busy,
    #[error("volume provider operation timed out")]
    Timeout,
    #[error("volume provider I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub trait BlockVolumeProvider: Send + Sync {
    fn provider_name(&self) -> &'static str;
    fn capabilities(&self) -> VolumeCapabilities;
    fn create(&self, volume_id: Uuid, size_bytes: u64) -> Result<ProviderVolume, VolumeError>;
    fn delete(&self, volume_id: Uuid) -> Result<(), VolumeError>;
    fn prepare(
        &self,
        volume_id: Uuid,
        size_bytes: u64,
        mode: AccessMode,
        generation: u64,
    ) -> Result<PreparedBlockAttachment, VolumeError>;

    /// Clone a volume while the orchestrator holds the source VM's device-I/O
    /// quiescence boundary. Implementations must create an independent
    /// point-in-time child or fail; sharing the writable source and dense-copy
    /// fallbacks are forbidden. Replaying the exact source/child generations
    /// and size must return the previously completed clone. A pre-existing
    /// child that cannot prove that exact origin must fail with `Conflict`.
    fn clone_quiesced(
        &self,
        _source_volume_id: Uuid,
        _source_generation: u64,
        _child_volume_id: Uuid,
        _child_generation: u64,
        _size_bytes: u64,
    ) -> Result<ProviderVolume, VolumeError> {
        Err(VolumeError::Unsupported(format!(
            "{} does not support atomic fork clones",
            self.provider_name()
        )))
    }

    /// Remove only the exact unpublished clone identified by its provenance.
    /// Absence is an idempotent success. The caller must fence the operation
    /// against attachment, publication, and another capture before cleanup.
    fn discard_clone(
        &self,
        _source_volume_id: Uuid,
        _source_generation: u64,
        _child_volume_id: Uuid,
        _child_generation: u64,
        _size_bytes: u64,
    ) -> Result<(), VolumeError> {
        Err(VolumeError::Unsupported(format!(
            "{} does not support verified clone cleanup",
            self.provider_name()
        )))
    }
}

/// A single-host provider useful for development, bare-metal installations,
/// and the same attachment path used after a cloud block device is attached.
/// Cluster placement must honor the returned exact-host constraint.
#[derive(Debug, Clone)]
pub struct LocalBlockProvider {
    root: PathBuf,
    host_id: String,
    max_size_bytes: u64,
    atomic_reflink: bool,
}

impl LocalBlockProvider {
    pub fn open(
        root: impl Into<PathBuf>,
        host_id: impl Into<String>,
        max_size_bytes: u64,
    ) -> Result<Self, VolumeError> {
        if max_size_bytes < MIN_BLOCK_VOLUME_BYTES {
            return Err(VolumeError::Invalid(
                "maximum local volume size is below one MiB".into(),
            ));
        }
        let host_id = host_id.into();
        if host_id.is_empty() {
            return Err(VolumeError::Invalid("host_id is empty".into()));
        }
        let root = root.into();
        ensure_private_root(&root)?;
        let atomic_reflink = supports_atomic_reflink(&root);
        Ok(Self {
            root,
            host_id,
            max_size_bytes,
            atomic_reflink,
        })
    }

    fn path(&self, volume_id: Uuid) -> PathBuf {
        self.root.join(format!("{volume_id}.block"))
    }

    fn validate_size(&self, size_bytes: u64) -> Result<(), VolumeError> {
        if !(MIN_BLOCK_VOLUME_BYTES..=self.max_size_bytes).contains(&size_bytes) {
            return Err(VolumeError::Invalid(format!(
                "block volume size must be between {MIN_BLOCK_VOLUME_BYTES} and {} bytes",
                self.max_size_bytes
            )));
        }
        Ok(())
    }

    fn open_existing(
        &self,
        path: &Path,
        read_only: bool,
        expected_size: Option<u64>,
    ) -> Result<File, VolumeError> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(!read_only)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(VolumeError::NotFound)
            }
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                return Err(VolumeError::UnsafeObject)
            }
            Err(error) => return Err(error.into()),
        };
        validate_owned_file(&file, expected_size)?;
        Ok(file)
    }

    fn remove_opened(&self, path: &Path, file: &File) -> Result<(), VolumeError> {
        let opened = file.metadata()?;
        let current = fs::symlink_metadata(path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                VolumeError::NotFound
            } else {
                VolumeError::Io(error)
            }
        })?;
        if !current.file_type().is_file()
            || current.dev() != opened.dev()
            || current.ino() != opened.ino()
        {
            return Err(VolumeError::UnsafeObject);
        }
        fs::remove_file(path)?;
        sync_directory(&self.root)?;
        Ok(())
    }
}

impl BlockVolumeProvider for LocalBlockProvider {
    fn provider_name(&self) -> &'static str {
        "local_block"
    }

    fn capabilities(&self) -> VolumeCapabilities {
        VolumeCapabilities {
            storage_class: StorageClass::Block,
            read_only_many: true,
            read_write_once: true,
            read_write_many: false,
            snapshots: false,
            clones: self.atomic_reflink,
        }
    }

    fn create(&self, volume_id: Uuid, size_bytes: u64) -> Result<ProviderVolume, VolumeError> {
        self.validate_size(size_bytes)?;
        let path = self.path(volume_id);
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let existing = self.open_existing(&path, false, Some(size_bytes));
                return match existing {
                    Ok(_) => Ok(ProviderVolume {
                        volume_id,
                        size_bytes,
                        constraint: PlacementConstraint::local_host(self.host_id.clone()),
                    }),
                    Err(VolumeError::UnsafeObject) => Err(VolumeError::UnsafeObject),
                    Err(_) => Err(VolumeError::Conflict),
                };
            }
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = (|| -> io::Result<()> {
            file.set_len(size_bytes)?;
            file.sync_all()?;
            sync_directory(&self.root)
        })() {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error.into());
        }
        validate_owned_file(&file, Some(size_bytes))?;
        Ok(ProviderVolume {
            volume_id,
            size_bytes,
            constraint: PlacementConstraint::local_host(self.host_id.clone()),
        })
    }

    fn delete(&self, volume_id: Uuid) -> Result<(), VolumeError> {
        let path = self.path(volume_id);
        let file = self.open_existing(&path, false, None)?;
        self.remove_opened(&path, &file)
    }

    fn prepare(
        &self,
        volume_id: Uuid,
        size_bytes: u64,
        mode: AccessMode,
        generation: u64,
    ) -> Result<PreparedBlockAttachment, VolumeError> {
        self.validate_size(size_bytes)?;
        if generation == 0 {
            return Err(VolumeError::Invalid(
                "attachment generation must be positive".into(),
            ));
        }
        if mode == AccessMode::ReadWriteMany {
            return Err(VolumeError::Invalid(
                "local block volumes do not support read-write-many".into(),
            ));
        }
        let read_only = mode == AccessMode::ReadOnlyMany;
        let path = self.path(volume_id);
        let file = self.open_existing(&path, read_only, Some(size_bytes))?;
        Ok(PreparedBlockAttachment {
            volume_id,
            size_bytes,
            read_only,
            generation,
            file,
            private_path: path,
        })
    }

    fn clone_quiesced(
        &self,
        source_volume_id: Uuid,
        source_generation: u64,
        child_volume_id: Uuid,
        child_generation: u64,
        size_bytes: u64,
    ) -> Result<ProviderVolume, VolumeError> {
        self.validate_size(size_bytes)?;
        if source_volume_id == child_volume_id {
            return Err(VolumeError::Invalid(
                "source and child volume identities must differ".into(),
            ));
        }
        if source_generation == 0 || child_generation == 0 {
            return Err(VolumeError::Invalid(
                "source and child generations must be positive".into(),
            ));
        }
        if !self.atomic_reflink {
            return Err(VolumeError::Unsupported(
                "local atomic fork clones require a verified reflink filesystem".into(),
            ));
        }
        let provenance = clone_provenance(
            source_volume_id,
            source_generation,
            child_volume_id,
            child_generation,
            size_bytes,
        );
        let child_path = self.path(child_volume_id);
        match self.open_existing(&child_path, true, Some(size_bytes)) {
            Ok(existing) => {
                validate_clone_provenance(&existing, &provenance)?;
                existing.sync_all()?;
                sync_directory(&self.root)?;
            }
            Err(VolumeError::NotFound) => clone_local_reflink(
                &self.open_existing(&self.path(source_volume_id), true, Some(size_bytes))?,
                &child_path,
                &self.root,
                &provenance,
            )?,
            Err(error) => return Err(error),
        }
        Ok(ProviderVolume {
            volume_id: child_volume_id,
            size_bytes,
            constraint: PlacementConstraint::local_host(self.host_id.clone()),
        })
    }
    fn discard_clone(
        &self,
        source_volume_id: Uuid,
        source_generation: u64,
        child_volume_id: Uuid,
        child_generation: u64,
        size_bytes: u64,
    ) -> Result<(), VolumeError> {
        self.validate_size(size_bytes)?;
        if source_volume_id == child_volume_id || source_generation == 0 || child_generation == 0 {
            return Err(VolumeError::Invalid(
                "invalid clone cleanup identity".into(),
            ));
        }
        let path = self.path(child_volume_id);
        let file = match self.open_existing(&path, true, Some(size_bytes)) {
            Ok(file) => file,
            Err(VolumeError::NotFound) => {
                sync_directory(&self.root)?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        validate_clone_provenance(
            &file,
            &clone_provenance(
                source_volume_id,
                source_generation,
                child_volume_id,
                child_generation,
                size_bytes,
            ),
        )?;
        self.remove_opened(&path, &file)
    }
}

fn clone_provenance(
    source_volume_id: Uuid,
    source_generation: u64,
    child_volume_id: Uuid,
    child_generation: u64,
    size_bytes: u64,
) -> Vec<u8> {
    format!(
        "v1:{source_volume_id}:{source_generation}:{child_volume_id}:{child_generation}:{size_bytes}"
    )
    .into_bytes()
}

#[cfg(target_os = "linux")]
fn supports_atomic_reflink(root: &Path) -> bool {
    const BTRFS_SUPER_MAGIC: libc::c_long = 0x9123_683e;
    let Ok(path) = std::ffi::CString::new(root.as_os_str().as_bytes()) else {
        return false;
    };
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and `filesystem` points to writable,
    // correctly sized storage initialized by statfs on success.
    let result = unsafe { libc::statfs(path.as_ptr(), filesystem.as_mut_ptr()) };
    result == 0 && unsafe { filesystem.assume_init() }.f_type == BTRFS_SUPER_MAGIC
}

#[cfg(not(target_os = "linux"))]
fn supports_atomic_reflink(_root: &Path) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn clone_local_reflink(
    source: &File,
    destination: &Path,
    root: &Path,
    provenance: &[u8],
) -> Result<(), VolumeError> {
    const FICLONE: libc::Ioctl = 0x4004_9409;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_TMPFILE);
    // An unnamed inode disappears automatically on a crash before publication.
    // No retry can observe a partial clone or an object without provenance.
    let child = options.open(root)?;
    // SAFETY: both descriptors remain open for the ioctl. The destination is
    // the private unnamed inode and the source was identity-checked above.
    if unsafe { libc::ioctl(child.as_raw_fd(), FICLONE, source.as_raw_fd()) } != 0 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOTTY) => {
                VolumeError::Unsupported(
                    "filesystem refused the required atomic reflink clone".into(),
                )
            }
            _ => VolumeError::Io(error),
        });
    }
    set_clone_provenance(&child, provenance)?;
    child.sync_all()?;
    let source_fd_path = std::ffi::CString::new(format!("/proc/self/fd/{}", child.as_raw_fd()))
        .expect("numeric descriptor contains no NUL");
    let destination_path = std::ffi::CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| VolumeError::Invalid("clone destination contains NUL".into()))?;
    // SAFETY: both paths are NUL-terminated and the unnamed inode remains
    // open. Following this process-owned procfs FD avoids requiring
    // CAP_DAC_READ_SEARCH for AT_EMPTY_PATH. linkat never replaces a target.
    if unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            source_fd_path.as_ptr(),
            libc::AT_FDCWD,
            destination_path.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return Err(if error.kind() == io::ErrorKind::AlreadyExists {
            VolumeError::Conflict
        } else {
            VolumeError::Io(error)
        });
    }
    // A directory-sync error leaves a complete, identifiable clone. Replay
    // re-syncs it rather than deleting an ambiguously durable publication.
    sync_directory(root)?;
    validate_owned_file(&child, Some(source.metadata()?.len()))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn clone_local_reflink(
    _source: &File,
    _destination: &Path,
    _root: &Path,
    _provenance: &[u8],
) -> Result<(), VolumeError> {
    Err(VolumeError::Unsupported(
        "local atomic fork clones require Linux FICLONE".into(),
    ))
}

#[cfg(target_os = "linux")]
fn set_clone_provenance(file: &File, provenance: &[u8]) -> Result<(), VolumeError> {
    const NAME: &[u8] = b"user.tarit.clone_origin\0";
    // SAFETY: NAME is NUL-terminated, provenance remains valid for the call,
    // and the file descriptor is owned by this process.
    if unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            NAME.as_ptr().cast(),
            provenance.as_ptr().cast(),
            provenance.len(),
            libc::XATTR_CREATE,
        )
    } != 0
    {
        return Err(VolumeError::Io(io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_clone_provenance(file: &File, expected: &[u8]) -> Result<(), VolumeError> {
    const NAME: &[u8] = b"user.tarit.clone_origin\0";
    let mut actual = vec![0u8; expected.len()];
    // SAFETY: NAME is NUL-terminated and `actual` is a writable buffer of the
    // supplied length for the duration of the call.
    let length = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            NAME.as_ptr().cast(),
            actual.as_mut_ptr().cast(),
            actual.len(),
        )
    };
    if length < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENODATA | libc::ERANGE) => Err(VolumeError::Conflict),
            _ => Err(VolumeError::Io(error)),
        };
    }
    if usize::try_from(length).ok() != Some(expected.len()) || actual != expected {
        return Err(VolumeError::Conflict);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_clone_provenance(_file: &File, _expected: &[u8]) -> Result<(), VolumeError> {
    Err(VolumeError::Unsupported(
        "local atomic fork clones require Linux extended attributes".into(),
    ))
}

fn ensure_private_root(root: &Path) -> Result<(), VolumeError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err(VolumeError::UnsafeObject);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root)?;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
            let metadata = fs::symlink_metadata(root)?;
            if !metadata.file_type().is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err(VolumeError::UnsafeObject);
            }
            sync_directory(root.parent().ok_or_else(|| {
                VolumeError::Invalid("volume root must have a parent directory".into())
            })?)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn validate_owned_file(file: &File, expected_size: Option<u64>) -> Result<(), VolumeError> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o177 != 0
    {
        return Err(VolumeError::UnsafeObject);
    }
    if expected_size.is_some_and(|size| metadata.len() != size) {
        return Err(VolumeError::Conflict);
    }
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?
        .sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::symlink;

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "tarit-volume-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    #[test]
    fn local_block_is_idempotent_private_sparse_and_durable() {
        let root = test_root("create");
        let provider = LocalBlockProvider::open(&root, "host-a", 8 * MIN_BLOCK_VOLUME_BYTES)
            .expect("provider");
        let id = Uuid::new_v4();
        let size = 4 * MIN_BLOCK_VOLUME_BYTES;
        let first = provider.create(id, size).expect("create");
        let replay = provider.create(id, size).expect("idempotent replay");
        assert_eq!(first, replay);
        assert_eq!(first.constraint.host_id.as_deref(), Some("host-a"));
        let metadata = fs::metadata(provider.path(id)).unwrap();
        assert_eq!(metadata.len(), size);
        assert_eq!(metadata.mode() & 0o777, 0o600);

        let mut attachment = provider
            .prepare(id, size, AccessMode::ReadWriteOnce, 7)
            .expect("prepare");
        attachment.file.write_all(b"durable-data").unwrap();
        attachment.file.sync_all().unwrap();
        attachment.file.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = vec![0; 12];
        attachment.file.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"durable-data");
        assert!(!format!("{attachment:?}").contains(root.to_str().unwrap()));

        drop(attachment);
        provider.delete(id).expect("delete");
        assert!(!provider.path(id).exists());
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn local_block_rejects_wrong_size_rw_many_and_zero_generation() {
        let root = test_root("modes");
        let provider = LocalBlockProvider::open(&root, "host-a", 8 * MIN_BLOCK_VOLUME_BYTES)
            .expect("provider");
        let id = Uuid::new_v4();
        provider.create(id, MIN_BLOCK_VOLUME_BYTES).unwrap();
        assert!(matches!(
            provider.create(id, 2 * MIN_BLOCK_VOLUME_BYTES),
            Err(VolumeError::Conflict)
        ));
        assert!(matches!(
            provider.prepare(id, MIN_BLOCK_VOLUME_BYTES, AccessMode::ReadWriteMany, 1),
            Err(VolumeError::Invalid(_))
        ));
        assert!(matches!(
            provider.prepare(id, MIN_BLOCK_VOLUME_BYTES, AccessMode::ReadWriteOnce, 0),
            Err(VolumeError::Invalid(_))
        ));
        provider.delete(id).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn local_fork_clone_is_independent_or_explicitly_unsupported() {
        check_local_fork_clone(false);
    }

    #[test]
    #[ignore = "requires TMPDIR on a private Linux btrfs filesystem"]
    fn local_fork_clone_requires_reflink_and_preserves_replay() {
        check_local_fork_clone(true);
    }

    fn check_local_fork_clone(require_reflink: bool) {
        let root = test_root("fork-clone");
        let provider = LocalBlockProvider::open(&root, "host-a", 8 * MIN_BLOCK_VOLUME_BYTES)
            .expect("provider");
        if require_reflink {
            assert!(
                provider.capabilities().clones,
                "reflink lane must execute a clone"
            );
        }
        let source_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let size = 2 * MIN_BLOCK_VOLUME_BYTES;
        provider.create(source_id, size).unwrap();
        let mut source = provider
            .prepare(source_id, size, AccessMode::ReadWriteOnce, 3)
            .unwrap();
        source.file.write_all(b"source-before-fork").unwrap();
        source.file.sync_all().unwrap();
        drop(source);

        if provider.capabilities().clones {
            #[cfg(target_os = "linux")]
            {
                let failed_child = provider.path(Uuid::new_v4());
                let source = provider
                    .prepare(source_id, size, AccessMode::ReadOnlyMany, 3)
                    .unwrap();
                // Exceed Linux's xattr value limit after the data clone, before
                // publication. Failure must leave no named staging object.
                assert!(
                    clone_local_reflink(&source.file, &failed_child, &root, &vec![0; 65_537],)
                        .is_err()
                );
                assert!(!failed_child.exists());
                assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
            }
            let cloned = provider
                .clone_quiesced(source_id, 3, child_id, 1, size)
                .expect("reflink clone");
            assert_eq!(cloned.volume_id, child_id);
            let replayed = provider
                .clone_quiesced(source_id, 3, child_id, 1, size)
                .expect("exact clone replay");
            assert_eq!(replayed, cloned);
            assert!(matches!(
                provider.clone_quiesced(source_id, 4, child_id, 1, size),
                Err(VolumeError::Conflict)
            ));

            let mut child = provider
                .prepare(child_id, size, AccessMode::ReadWriteOnce, 1)
                .unwrap();
            let mut inherited = vec![0; b"source-before-fork".len()];
            child.file.read_exact(&mut inherited).unwrap();
            assert_eq!(&inherited, b"source-before-fork");
            child.file.seek(SeekFrom::Start(0)).unwrap();
            child.file.write_all(b"child-independent").unwrap();
            child.file.sync_all().unwrap();
            drop(child);

            let mut changed_source = provider
                .prepare(source_id, size, AccessMode::ReadWriteOnce, 3)
                .unwrap();
            let mut source_bytes = vec![0; b"source-before-fork".len()];
            changed_source.file.read_exact(&mut source_bytes).unwrap();
            assert_eq!(&source_bytes, b"source-before-fork");
            changed_source.file.seek(SeekFrom::Start(0)).unwrap();
            changed_source
                .file
                .write_all(b"source-after-fork!")
                .unwrap();
            changed_source.file.sync_all().unwrap();
            drop(changed_source);
            provider
                .clone_quiesced(source_id, 3, child_id, 1, size)
                .expect("replay must preserve the existing child, not reclone the source");
            let mut replayed_child = provider
                .prepare(child_id, size, AccessMode::ReadOnlyMany, 1)
                .unwrap();
            let mut replayed_bytes = vec![0; b"child-independent".len()];
            replayed_child.file.read_exact(&mut replayed_bytes).unwrap();
            assert_eq!(&replayed_bytes, b"child-independent");
            drop(replayed_child);

            let mut source = provider
                .prepare(source_id, size, AccessMode::ReadOnlyMany, 3)
                .unwrap();
            let mut unchanged = vec![0; b"source-before-fork".len()];
            source.file.read_exact(&mut unchanged).unwrap();
            assert_eq!(&unchanged, b"source-after-fork!");
            drop(source);
            for (origin, generation, child_generation, bytes) in [
                (Uuid::new_v4(), 3, 1, size),
                (source_id, 4, 1, size),
                (source_id, 3, 2, size),
                (source_id, 3, 1, size + MIN_BLOCK_VOLUME_BYTES),
            ] {
                assert!(provider
                    .discard_clone(origin, generation, child_id, child_generation, bytes)
                    .is_err());
                assert!(provider.path(child_id).exists());
            }
            provider
                .discard_clone(source_id, 3, child_id, 1, size)
                .unwrap();
            provider
                .discard_clone(source_id, 3, child_id, 1, size)
                .unwrap();
            assert!(!provider.path(child_id).exists());
            provider.create(child_id, size).unwrap();
            assert!(provider
                .discard_clone(source_id, 3, child_id, 1, size)
                .is_err());
            assert!(
                provider.path(child_id).exists(),
                "replacement volume must survive stale cleanup"
            );
            provider.delete(child_id).unwrap();
        } else {
            assert!(matches!(
                provider.clone_quiesced(source_id, 3, child_id, 1, size),
                Err(VolumeError::Unsupported(_))
            ));
            assert!(!provider.path(child_id).exists());
        }

        provider.delete(source_id).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn clone_cleanup_preserves_a_replaced_inode() {
        let root = test_root("clone-cleanup-replacement");
        let provider =
            LocalBlockProvider::open(&root, "host-a", 8 * MIN_BLOCK_VOLUME_BYTES).unwrap();
        let id = Uuid::new_v4();
        provider.create(id, MIN_BLOCK_VOLUME_BYTES).unwrap();
        let path = provider.path(id);
        let opened = provider.open_existing(&path, true, None).unwrap();
        provider.delete(id).unwrap();
        provider.create(id, MIN_BLOCK_VOLUME_BYTES).unwrap();
        assert!(matches!(
            provider.remove_opened(&path, &opened),
            Err(VolumeError::UnsafeObject)
        ));
        assert!(path.exists());
        drop(opened);
        provider.delete(id).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn local_block_rejects_symlink_and_hardlink_substitution() {
        let root = test_root("links");
        let provider = LocalBlockProvider::open(&root, "host-a", 8 * MIN_BLOCK_VOLUME_BYTES)
            .expect("provider");
        let symlink_id = Uuid::new_v4();
        let outside = root
            .parent()
            .unwrap()
            .join(format!("outside-{}", Uuid::new_v4()));
        fs::write(&outside, vec![0; MIN_BLOCK_VOLUME_BYTES as usize]).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&outside, provider.path(symlink_id)).unwrap();
        assert!(matches!(
            provider.prepare(
                symlink_id,
                MIN_BLOCK_VOLUME_BYTES,
                AccessMode::ReadOnlyMany,
                1
            ),
            Err(VolumeError::UnsafeObject)
        ));
        fs::remove_file(provider.path(symlink_id)).unwrap();

        let hardlink_id = Uuid::new_v4();
        fs::hard_link(&outside, provider.path(hardlink_id)).unwrap();
        assert!(matches!(
            provider.prepare(
                hardlink_id,
                MIN_BLOCK_VOLUME_BYTES,
                AccessMode::ReadOnlyMany,
                1
            ),
            Err(VolumeError::UnsafeObject)
        ));
        fs::remove_file(provider.path(hardlink_id)).unwrap();
        fs::remove_file(outside).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn provider_requires_a_private_owned_root() {
        let root = test_root("root-mode");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            LocalBlockProvider::open(&root, "host-a", MIN_BLOCK_VOLUME_BYTES),
            Err(VolumeError::UnsafeObject)
        ));
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
