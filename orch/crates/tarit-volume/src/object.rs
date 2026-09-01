use crate::{ensure_private_root, sync_directory, VolumeError};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectDigest([u8; 32]);

impl ObjectDigest {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[cfg(feature = "cloud-object-store")]
    pub(crate) fn from_sha256(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    #[cfg(feature = "cloud-object-store")]
    pub(crate) fn hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

impl fmt::Display for ObjectDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("sha256:")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ObjectDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl FromStr for ObjectDigest {
    type Err = VolumeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let hex = value.strip_prefix("sha256:").ok_or_else(|| {
            VolumeError::Invalid("immutable object digest must use sha256".into())
        })?;
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(VolumeError::Invalid(
                "immutable object digest must be canonical lowercase SHA-256".into(),
            ));
        }
        let mut digest = [0_u8; 32];
        for (index, output) in digest.iter_mut().enumerate() {
            *output = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).map_err(|_| {
                VolumeError::Invalid(
                    "immutable object digest must be canonical lowercase SHA-256".into(),
                )
            })?;
        }
        Ok(Self(digest))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableObject {
    pub digest: ObjectDigest,
    pub size_bytes: u64,
}

/// Immutable artifact semantics only. This interface intentionally has no
/// mount, rename, append, locking, or random-write operation.
pub trait ImmutableObjectProvider: Send + Sync {
    fn provider_name(&self) -> &'static str;
    fn put_if_absent(&self, bytes: &[u8]) -> Result<ImmutableObject, VolumeError>;
    fn get_verified(&self, object: &ImmutableObject) -> Result<Vec<u8>, VolumeError>;
    fn delete(&self, object: &ImmutableObject) -> Result<(), VolumeError>;
}

#[derive(Debug, Clone)]
pub struct LocalImmutableObjectProvider {
    root: PathBuf,
    max_object_bytes: u64,
}

impl LocalImmutableObjectProvider {
    pub fn open(root: impl Into<PathBuf>, max_object_bytes: u64) -> Result<Self, VolumeError> {
        if max_object_bytes == 0 {
            return Err(VolumeError::Invalid(
                "maximum immutable object size must be positive".into(),
            ));
        }
        let root = root.into();
        ensure_private_root(&root)?;
        Ok(Self {
            root,
            max_object_bytes,
        })
    }

    fn path(&self, digest: ObjectDigest) -> PathBuf {
        self.root.join(format!(
            "{}.blob",
            digest.to_string().trim_start_matches("sha256:")
        ))
    }

    fn open_existing(&self, object: &ImmutableObject) -> Result<File, VolumeError> {
        let path = self.path(object.digest);
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    VolumeError::NotFound
                } else if error.raw_os_error() == Some(libc::ELOOP) {
                    VolumeError::UnsafeObject
                } else {
                    VolumeError::Io(error)
                }
            })?;
        validate_object_file(&file, object.size_bytes)?;
        Ok(file)
    }
}

impl ImmutableObjectProvider for LocalImmutableObjectProvider {
    fn provider_name(&self) -> &'static str {
        "local_immutable_object"
    }

    fn put_if_absent(&self, bytes: &[u8]) -> Result<ImmutableObject, VolumeError> {
        let size_bytes = u64::try_from(bytes.len())
            .map_err(|_| VolumeError::Invalid("object length overflows u64".into()))?;
        if size_bytes > self.max_object_bytes {
            return Err(VolumeError::Invalid(
                "immutable object exceeds configured maximum".into(),
            ));
        }
        let object = ImmutableObject {
            digest: ObjectDigest::from_bytes(bytes),
            size_bytes,
        };
        let path = self.path(object.digest);
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.get_verified(&object)?;
                return Ok(object);
            }
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = (|| -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o400))?;
            sync_directory(&self.root)
        })() {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error.into());
        }
        Ok(object)
    }

    fn get_verified(&self, object: &ImmutableObject) -> Result<Vec<u8>, VolumeError> {
        if object.size_bytes > self.max_object_bytes {
            return Err(VolumeError::Invalid(
                "immutable object exceeds configured maximum".into(),
            ));
        }
        let file = self.open_existing(object)?;
        let size = usize::try_from(object.size_bytes)
            .map_err(|_| VolumeError::Invalid("object size overflows usize".into()))?;
        let mut bytes = Vec::with_capacity(size);
        file.take(object.size_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() != size || ObjectDigest::from_bytes(&bytes) != object.digest {
            return Err(VolumeError::IdentityMismatch);
        }
        Ok(bytes)
    }

    fn delete(&self, object: &ImmutableObject) -> Result<(), VolumeError> {
        let file = self.open_existing(object)?;
        let opened = file.metadata()?;
        let path = self.path(object.digest);
        let current = fs::symlink_metadata(&path)?;
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

fn validate_object_file(file: &File, expected_size: u64) -> Result<(), VolumeError> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o377 != 0
        || metadata.len() != expected_size
    {
        return Err(VolumeError::UnsafeObject);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use uuid::Uuid;

    fn root() -> PathBuf {
        let path = std::env::temp_dir().join(format!("tarit-object-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn immutable_put_is_idempotent_verified_and_private() {
        let root = root();
        let provider = LocalImmutableObjectProvider::open(&root, 1024).unwrap();
        let object = provider.put_if_absent(b"immutable-checkpoint").unwrap();
        assert_eq!(
            provider.put_if_absent(b"immutable-checkpoint").unwrap(),
            object
        );
        assert_eq!(
            provider.get_verified(&object).unwrap(),
            b"immutable-checkpoint"
        );
        assert_eq!(
            fs::metadata(provider.path(object.digest)).unwrap().mode() & 0o777,
            0o400
        );
        provider.delete(&object).unwrap();
        assert!(matches!(
            provider.get_verified(&object),
            Err(VolumeError::NotFound)
        ));
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn corruption_and_path_substitution_fail_closed() {
        let root = root();
        let provider = LocalImmutableObjectProvider::open(&root, 1024).unwrap();
        let object = provider.put_if_absent(b"original").unwrap();
        let path = provider.path(object.digest);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, b"tampered").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        assert!(matches!(
            provider.get_verified(&object),
            Err(VolumeError::IdentityMismatch)
        ));
        fs::remove_file(&path).unwrap();
        symlink("/etc/passwd", &path).unwrap();
        assert!(matches!(
            provider.get_verified(&object),
            Err(VolumeError::UnsafeObject)
        ));
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn object_digest_parser_requires_canonical_sha256() {
        let digest = ObjectDigest::from_bytes(b"canonical");
        assert_eq!(digest.to_string().parse::<ObjectDigest>().unwrap(), digest);
        for value in [
            "",
            "md5:00000000000000000000000000000000",
            "sha256:00",
            "sha256:GG00000000000000000000000000000000000000000000000000000000000000",
            "sha256:AA00000000000000000000000000000000000000000000000000000000000000",
        ] {
            assert!(value.parse::<ObjectDigest>().is_err(), "{value}");
        }
    }
}
