//! Provider-neutral persistent-volume preparation.
//!
//! This crate deliberately contains no product, tenant, billing, HTTP, or VMM
//! policy. The orchestrator persists and fences desired attachments; providers
//! turn an opaque volume identity into a verified host resource.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

mod attached_block;
#[cfg(feature = "cloud-object-store")]
mod cloud_object;
mod nfs;
mod object;

pub use attached_block::{
    AttachedBlockKind, AttachedBlockProvider, BlockDeviceIdentity, BlockDeviceRegistration,
};
#[cfg(feature = "cloud-object-store")]
pub use cloud_object::RemoteImmutableObjectProvider;
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
}

/// A single-host provider useful for development, bare-metal installations,
/// and the same attachment path used after a cloud block device is attached.
/// Cluster placement must honor the returned exact-host constraint.
#[derive(Debug, Clone)]
pub struct LocalBlockProvider {
    root: PathBuf,
    host_id: String,
    max_size_bytes: u64,
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
        Ok(Self {
            root,
            host_id,
            max_size_bytes,
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
            clones: false,
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
        let opened = file.metadata()?;
        let current = fs::symlink_metadata(&path).map_err(|error| {
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
        fs::remove_file(&path)?;
        sync_directory(&self.root)?;
        Ok(())
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
