use crate::{
    AccessMode, BlockVolumeProvider, PlacementConstraint, PreparedBlockAttachment, ProviderVolume,
    StorageClass, VolumeCapabilities, VolumeError, MIN_BLOCK_VOLUME_BYTES,
};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachedBlockKind {
    AwsEbs,
    AzureDisk,
    Raw,
}

impl AttachedBlockKind {
    fn provider_name(self) -> &'static str {
        match self {
            Self::AwsEbs => "aws_ebs",
            Self::AzureDisk => "azure_disk",
            Self::Raw => "attached_block",
        }
    }
}

/// Stable identity expected after a control-plane attach. Linux major/minor is
/// always checked as a per-attachment fence. AWS additionally checks the Nitro
/// NVMe serial; Azure checks the durable `/dev/disk/azure/data/by-lun/<lun>`
/// alias instead of unstable sdX/nvmeX enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockDeviceIdentity {
    AwsEbsVolumeId(String),
    AzureLun(u16),
    LinuxDevice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDeviceRegistration {
    pub volume_id: Uuid,
    pub stable_path: PathBuf,
    pub size_bytes: u64,
    pub generation: u64,
    pub device_major: u64,
    pub device_minor: u64,
    pub identity: BlockDeviceIdentity,
}

#[derive(Debug, Clone)]
pub struct AttachedBlockProvider {
    kind: AttachedBlockKind,
    host_id: String,
    region: Option<String>,
    zone: Option<String>,
    registrations: Arc<RwLock<HashMap<Uuid, BlockDeviceRegistration>>>,
}

impl AttachedBlockProvider {
    pub fn new(
        kind: AttachedBlockKind,
        host_id: impl Into<String>,
        region: Option<String>,
        zone: Option<String>,
    ) -> Result<Self, VolumeError> {
        let host_id = host_id.into();
        if host_id.is_empty() {
            return Err(VolumeError::Invalid("host_id is empty".into()));
        }
        Ok(Self {
            kind,
            host_id,
            region,
            zone,
            registrations: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub fn inspect_registration(
        &self,
        volume_id: Uuid,
        stable_path: impl Into<PathBuf>,
        size_bytes: u64,
        generation: u64,
        identity: BlockDeviceIdentity,
    ) -> Result<BlockDeviceRegistration, VolumeError> {
        if size_bytes < MIN_BLOCK_VOLUME_BYTES || generation == 0 {
            return Err(VolumeError::Invalid(
                "attached block size and generation must be positive".into(),
            ));
        }
        let stable_path = stable_path.into();
        validate_stable_path(self.kind, &stable_path, &identity)?;
        let file = open_block_device(&stable_path, true)?;
        let metadata = file.metadata()?;
        let actual_size = block_device_size(&file)?;
        if actual_size != size_bytes {
            return Err(VolumeError::IdentityMismatch);
        }
        let (device_major, device_minor) = device_numbers(&metadata);
        verify_provider_identity(
            self.kind,
            &stable_path,
            device_major,
            device_minor,
            &identity,
        )?;
        Ok(BlockDeviceRegistration {
            volume_id,
            stable_path,
            size_bytes,
            generation,
            device_major,
            device_minor,
            identity,
        })
    }

    pub fn register(&self, registration: BlockDeviceRegistration) -> Result<(), VolumeError> {
        let inspected = self.inspect_registration(
            registration.volume_id,
            registration.stable_path.clone(),
            registration.size_bytes,
            registration.generation,
            registration.identity.clone(),
        )?;
        if inspected.device_major != registration.device_major
            || inspected.device_minor != registration.device_minor
        {
            return Err(VolumeError::IdentityMismatch);
        }
        let mut registrations = self
            .registrations
            .write()
            .map_err(|_| VolumeError::Io(std::io::Error::other("registration lock poisoned")))?;
        match registrations.get(&registration.volume_id) {
            Some(existing) if existing == &registration => Ok(()),
            Some(_) => Err(VolumeError::Conflict),
            None => {
                registrations.insert(registration.volume_id, registration);
                Ok(())
            }
        }
    }

    pub fn unregister(&self, volume_id: Uuid, generation: u64) -> Result<(), VolumeError> {
        let mut registrations = self
            .registrations
            .write()
            .map_err(|_| VolumeError::Io(std::io::Error::other("registration lock poisoned")))?;
        match registrations.get(&volume_id) {
            None => Err(VolumeError::NotFound),
            Some(record) if record.generation != generation => Err(VolumeError::IdentityMismatch),
            Some(_) => {
                registrations.remove(&volume_id);
                Ok(())
            }
        }
    }

    pub fn placement_constraint(&self) -> PlacementConstraint {
        PlacementConstraint {
            host_id: Some(self.host_id.clone()),
            region: self.region.clone(),
            zone: self.zone.clone(),
        }
    }
}

impl BlockVolumeProvider for AttachedBlockProvider {
    fn provider_name(&self) -> &'static str {
        self.kind.provider_name()
    }

    fn capabilities(&self) -> VolumeCapabilities {
        VolumeCapabilities {
            storage_class: StorageClass::Block,
            read_only_many: true,
            read_write_once: true,
            read_write_many: false,
            snapshots: matches!(
                self.kind,
                AttachedBlockKind::AwsEbs | AttachedBlockKind::AzureDisk
            ),
            clones: matches!(
                self.kind,
                AttachedBlockKind::AwsEbs | AttachedBlockKind::AzureDisk
            ),
        }
    }

    fn create(&self, _volume_id: Uuid, _size_bytes: u64) -> Result<ProviderVolume, VolumeError> {
        Err(VolumeError::Unsupported(format!(
            "{} creation requires its cloud control-plane reconciler",
            self.provider_name()
        )))
    }

    fn delete(&self, _volume_id: Uuid) -> Result<(), VolumeError> {
        Err(VolumeError::Unsupported(format!(
            "{} deletion requires its cloud control-plane reconciler",
            self.provider_name()
        )))
    }

    fn prepare(
        &self,
        volume_id: Uuid,
        size_bytes: u64,
        mode: AccessMode,
        generation: u64,
    ) -> Result<PreparedBlockAttachment, VolumeError> {
        if mode == AccessMode::ReadWriteMany {
            return Err(VolumeError::Unsupported(
                "shared writable block requires explicit provider fencing and reservations".into(),
            ));
        }
        let registration = self
            .registrations
            .read()
            .map_err(|_| VolumeError::Io(std::io::Error::other("registration lock poisoned")))?
            .get(&volume_id)
            .cloned()
            .ok_or(VolumeError::NotFound)?;
        if registration.size_bytes != size_bytes || registration.generation != generation {
            return Err(VolumeError::IdentityMismatch);
        }
        let file = open_block_device(&registration.stable_path, mode == AccessMode::ReadOnlyMany)?;
        let metadata = file.metadata()?;
        let (major, minor) = device_numbers(&metadata);
        if major != registration.device_major
            || minor != registration.device_minor
            || block_device_size(&file)? != size_bytes
        {
            return Err(VolumeError::IdentityMismatch);
        }
        verify_provider_identity(
            self.kind,
            &registration.stable_path,
            major,
            minor,
            &registration.identity,
        )?;
        Ok(PreparedBlockAttachment {
            volume_id,
            size_bytes,
            read_only: mode == AccessMode::ReadOnlyMany,
            generation,
            file,
            private_path: registration.stable_path,
        })
    }
}

fn validate_stable_path(
    kind: AttachedBlockKind,
    path: &Path,
    identity: &BlockDeviceIdentity,
) -> Result<(), VolumeError> {
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(VolumeError::Invalid(
            "stable block device path must be absolute and NUL-free".into(),
        ));
    }
    match (kind, identity) {
        (AttachedBlockKind::AwsEbs, BlockDeviceIdentity::AwsEbsVolumeId(id))
            if valid_cloud_id(id, "vol-") => {}
        (AttachedBlockKind::AzureDisk, BlockDeviceIdentity::AzureLun(lun)) => {
            let suffix = PathBuf::from("/dev/disk/azure/data/by-lun").join(lun.to_string());
            if path != suffix {
                return Err(VolumeError::Invalid(
                    "Azure Disk must use /dev/disk/azure/data/by-lun/<lun>".into(),
                ));
            }
        }
        (AttachedBlockKind::Raw, BlockDeviceIdentity::LinuxDevice) => {}
        _ => {
            return Err(VolumeError::Invalid(
                "device identity does not match provider".into(),
            ))
        }
    }
    Ok(())
}

fn valid_cloud_id(value: &str, prefix: &str) -> bool {
    value.starts_with(prefix)
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn open_block_device(path: &Path, read_only: bool) -> Result<File, VolumeError> {
    let canonical = path.canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            VolumeError::NotFound
        } else {
            VolumeError::Io(error)
        }
    })?;
    if !canonical.starts_with("/dev") {
        return Err(VolumeError::UnsafeObject);
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(!read_only)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(&canonical)?;
    if !file.metadata()?.file_type().is_block_device() {
        return Err(VolumeError::UnsafeObject);
    }
    Ok(file)
}

fn device_numbers(metadata: &std::fs::Metadata) -> (u64, u64) {
    let rdev = metadata.rdev() as libc::dev_t;
    (libc::major(rdev) as u64, libc::minor(rdev) as u64)
}

#[cfg(target_os = "linux")]
fn block_device_size(file: &File) -> Result<u64, VolumeError> {
    // libc's ioctl request type is c_ulong on glibc/uclibc and c_int on
    // musl/Android. Using its target-specific alias keeps this call portable.
    const BLKGETSIZE64: libc::Ioctl = 0x8008_1272_u32 as libc::Ioctl;
    let mut size = 0_u64;
    // SAFETY: BLKGETSIZE64 writes one u64 to the supplied valid pointer and
    // `file` is held open for the duration of the ioctl.
    let result = unsafe {
        libc::ioctl(
            std::os::fd::AsRawFd::as_raw_fd(file),
            BLKGETSIZE64,
            &mut size,
        )
    };
    if result < 0 {
        return Err(VolumeError::Io(std::io::Error::last_os_error()));
    }
    Ok(size)
}

#[cfg(not(target_os = "linux"))]
fn block_device_size(_file: &File) -> Result<u64, VolumeError> {
    Err(VolumeError::Unsupported(
        "attached block devices require Linux".into(),
    ))
}

fn verify_provider_identity(
    kind: AttachedBlockKind,
    stable_path: &Path,
    major: u64,
    minor: u64,
    identity: &BlockDeviceIdentity,
) -> Result<(), VolumeError> {
    match (kind, identity) {
        (AttachedBlockKind::Raw, BlockDeviceIdentity::LinuxDevice) => Ok(()),
        (AttachedBlockKind::AzureDisk, BlockDeviceIdentity::AzureLun(lun)) => {
            let expected = PathBuf::from("/dev/disk/azure/data/by-lun").join(lun.to_string());
            if stable_path == expected {
                Ok(())
            } else {
                Err(VolumeError::IdentityMismatch)
            }
        }
        (AttachedBlockKind::AwsEbs, BlockDeviceIdentity::AwsEbsVolumeId(expected)) => {
            let serial_path =
                PathBuf::from(format!("/sys/dev/block/{major}:{minor}/device/serial"));
            let serial =
                std::fs::read_to_string(serial_path).map_err(|_| VolumeError::IdentityMismatch)?;
            let normalize = |value: &str| value.trim().replace('-', "").to_ascii_lowercase();
            if normalize(&serial) == normalize(expected) {
                Ok(())
            } else {
                Err(VolumeError::IdentityMismatch)
            }
        }
        _ => Err(VolumeError::IdentityMismatch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::FileExt;

    #[test]
    fn provider_specific_stable_paths_fail_closed() {
        let provider = AttachedBlockProvider::new(
            AttachedBlockKind::AzureDisk,
            "host-a",
            Some("eastus".into()),
            Some("1".into()),
        )
        .unwrap();
        assert_eq!(provider.provider_name(), "azure_disk");
        assert!(matches!(
            provider.inspect_registration(
                Uuid::new_v4(),
                "/dev/sdb",
                MIN_BLOCK_VOLUME_BYTES,
                1,
                BlockDeviceIdentity::AzureLun(3),
            ),
            Err(VolumeError::Invalid(_))
        ));
        assert_eq!(provider.placement_constraint().zone.as_deref(), Some("1"));

        let aws = AttachedBlockProvider::new(
            AttachedBlockKind::AwsEbs,
            "host-a",
            Some("us-east-1".into()),
            Some("us-east-1a".into()),
        )
        .unwrap();
        assert!(matches!(
            aws.inspect_registration(
                Uuid::new_v4(),
                "/dev/null",
                MIN_BLOCK_VOLUME_BYTES,
                1,
                BlockDeviceIdentity::AwsEbsVolumeId("not-an-ebs-id".into()),
            ),
            Err(VolumeError::Invalid(_))
        ));
    }

    /// The c8i gate supplies a disposable loop device. Keeping setup outside
    /// the test makes ownership and cleanup explicit and prevents a unit test
    /// from ever selecting an arbitrary host block device.
    #[cfg(target_os = "linux")]
    #[test]
    fn disposable_loop_device_is_identity_and_generation_fenced() {
        let Ok(path) = std::env::var("TARIT_TEST_ATTACHED_BLOCK_DEVICE") else {
            return;
        };
        let size_bytes: u64 = std::env::var("TARIT_TEST_ATTACHED_BLOCK_SIZE")
            .expect("loop size")
            .parse()
            .expect("numeric loop size");
        let id = Uuid::new_v4();
        let provider =
            AttachedBlockProvider::new(AttachedBlockKind::Raw, "c8i-loop-test", None, None)
                .unwrap();
        let registration = provider
            .inspect_registration(id, &path, size_bytes, 9, BlockDeviceIdentity::LinuxDevice)
            .expect("inspect exact loop device");
        provider.register(registration.clone()).unwrap();
        provider
            .register(registration)
            .expect("idempotent registration");
        assert!(matches!(
            provider.prepare(id, size_bytes, AccessMode::ReadWriteOnce, 8),
            Err(VolumeError::IdentityMismatch)
        ));
        assert!(matches!(
            provider.prepare(id, size_bytes, AccessMode::ReadWriteMany, 9),
            Err(VolumeError::Unsupported(_))
        ));
        let attachment = provider
            .prepare(id, size_bytes, AccessMode::ReadWriteOnce, 9)
            .expect("prepare exact device");
        attachment
            .file
            .write_all_at(b"tarit-attached-block", 4096)
            .unwrap();
        attachment.file.sync_all().unwrap();
        let mut proof = [0_u8; 20];
        attachment.file.read_exact_at(&mut proof, 4096).unwrap();
        assert_eq!(&proof, b"tarit-attached-block");
        assert!(!format!("{attachment:?}").contains(&path));
        drop(attachment);
        assert!(matches!(
            provider.unregister(id, 8),
            Err(VolumeError::IdentityMismatch)
        ));
        provider.unregister(id, 9).unwrap();
        assert!(matches!(
            provider.prepare(id, size_bytes, AccessMode::ReadOnlyMany, 9),
            Err(VolumeError::NotFound)
        ));
    }
}
