use crate::{ImmutableObject, ObjectDigest, VolumeError};
use futures_util::StreamExt;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, WriteMultipart};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{Read, Seek};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path as FilePath;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Content-addressed immutable blobs backed by a remote object store.
///
/// The provider intentionally exposes no filesystem, random-write, append,
/// rename, or locking operation. Callers must retain their own durable
/// references and invoke deletion only after reference-counted GC permits it.
#[derive(Clone)]
pub struct RemoteImmutableObjectProvider {
    provider_name: &'static str,
    store: Arc<dyn ObjectStore>,
    prefix: String,
    max_object_bytes: u64,
}

impl fmt::Debug for RemoteImmutableObjectProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteImmutableObjectProvider")
            .field("provider_name", &self.provider_name)
            .field("store", &"[REDACTED]")
            .field("prefix", &"[REDACTED]")
            .field("max_object_bytes", &self.max_object_bytes)
            .finish()
    }
}

impl RemoteImmutableObjectProvider {
    pub fn validate_namespace(prefix: &str, max_object_bytes: u64) -> Result<(), VolumeError> {
        if max_object_bytes == 0 {
            return Err(VolumeError::Invalid(
                "maximum immutable object size must be positive".into(),
            ));
        }
        validate_prefix(prefix)
    }

    pub fn new(
        provider_name: &'static str,
        store: Arc<dyn ObjectStore>,
        prefix: impl Into<String>,
        max_object_bytes: u64,
    ) -> Result<Self, VolumeError> {
        let prefix = prefix.into();
        if provider_name.is_empty()
            || !provider_name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(VolumeError::Invalid(
                "remote object provider name is invalid".into(),
            ));
        }
        Self::validate_namespace(&prefix, max_object_bytes)?;
        Ok(Self {
            provider_name,
            store,
            prefix,
            max_object_bytes,
        })
    }

    pub fn provider_name(&self) -> &'static str {
        self.provider_name
    }

    pub fn scoped(&self, suffix: &str) -> Result<Self, VolumeError> {
        validate_prefix(suffix)?;
        Self::new(
            self.provider_name,
            Arc::clone(&self.store),
            format!("{}/{suffix}", self.prefix),
            self.max_object_bytes,
        )
    }

    fn location(&self, digest: ObjectDigest) -> Path {
        Path::from(format!("{}/{}.blob", self.prefix, digest.hex()))
    }

    fn staging_location(&self) -> Path {
        Path::from(format!("{}/.staging/{}", self.prefix, uuid::Uuid::new_v4()))
    }

    pub async fn put_if_absent(&self, bytes: &[u8]) -> Result<ImmutableObject, VolumeError> {
        let size_bytes = u64::try_from(bytes.len())
            .map_err(|_| VolumeError::Invalid("object length overflows u64".into()))?;
        self.validate_size(size_bytes)?;
        let object = ImmutableObject {
            digest: ObjectDigest::from_bytes(bytes),
            size_bytes,
        };
        let location = self.location(object.digest);
        match self
            .store
            .put_opts(&location, bytes.to_vec().into(), PutMode::Create.into())
            .await
        {
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => {
                let verified = self.get_verified(&object).await?;
                if verified.as_slice() != bytes {
                    return Err(VolumeError::IdentityMismatch);
                }
                Ok(object)
            }
            Err(error) => Err(map_store_error(error)),
        }
    }

    pub async fn get_verified(&self, object: &ImmutableObject) -> Result<Vec<u8>, VolumeError> {
        self.validate_size(object.size_bytes)?;
        let result = self
            .store
            .get(&self.location(object.digest))
            .await
            .map_err(map_store_error)?;
        if result.meta.size != object.size_bytes {
            return Err(VolumeError::IdentityMismatch);
        }
        let bytes = result.bytes().await.map_err(map_store_error)?;
        let expected_len = usize::try_from(object.size_bytes)
            .map_err(|_| VolumeError::Invalid("object size overflows usize".into()))?;
        if bytes.len() != expected_len || ObjectDigest::from_bytes(&bytes) != object.digest {
            return Err(VolumeError::IdentityMismatch);
        }
        Ok(bytes.to_vec())
    }

    /// Stream a regular file into a temporary remote object and publish it to
    /// its content-addressed location only after the complete upload succeeds.
    pub async fn put_file_if_absent(
        &self,
        source: &FilePath,
    ) -> Result<ImmutableObject, VolumeError> {
        let mut source = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(source)
            .map_err(VolumeError::Io)?;
        let metadata = source.metadata().map_err(VolumeError::Io)?;
        if !metadata.file_type().is_file() {
            return Err(VolumeError::UnsafeObject);
        }
        self.validate_size(metadata.len())?;
        if metadata.len() == 0 {
            let mut probe = [0_u8; 1];
            match source.read(&mut probe).map_err(VolumeError::Io)? {
                0 => return self.put_if_absent(&[]).await,
                _ => source.rewind().map_err(VolumeError::Io)?,
            }
        }

        let staging = self.staging_location();
        let upload = self
            .store
            .put_multipart(&staging)
            .await
            .map_err(map_store_error)?;
        let mut writer = WriteMultipart::new(upload);
        let mut source = tokio::fs::File::from_std(source);
        let mut buffer = vec![0_u8; 1024 * 1024];
        let mut digest = Sha256::new();
        let mut size_bytes = 0_u64;

        loop {
            let read = match source.read(&mut buffer).await {
                Ok(read) => read,
                Err(error) => {
                    let _ = writer.abort().await;
                    return Err(VolumeError::Io(error));
                }
            };
            if read == 0 {
                break;
            }
            size_bytes = match size_bytes.checked_add(read as u64) {
                Some(size_bytes) => size_bytes,
                None => {
                    let _ = writer.abort().await;
                    return Err(VolumeError::Invalid("object size overflows u64".into()));
                }
            };
            if let Err(error) = self.validate_size(size_bytes) {
                let _ = writer.abort().await;
                return Err(error);
            }
            digest.update(&buffer[..read]);
            writer.write(&buffer[..read]);
            if let Err(error) = writer.wait_for_capacity(4).await {
                let _ = writer.abort().await;
                return Err(map_store_error(error));
            }
        }
        writer.finish().await.map_err(map_store_error)?;

        let object = ImmutableObject {
            digest: ObjectDigest::from_sha256(digest.finalize().into()),
            size_bytes,
        };
        let location = self.location(object.digest);
        let publication = self.store.copy_if_not_exists(&staging, &location).await;
        let cleanup = self.store.delete(&staging).await;
        match publication {
            Ok(()) | Err(object_store::Error::AlreadyExists { .. }) => {
                cleanup.map_err(map_store_error)?;
                self.verify_remote_object(&object).await?;
                Ok(object)
            }
            Err(error) => {
                let _ = cleanup;
                Err(map_store_error(error))
            }
        }
    }

    /// Download an immutable object to a newly-created regular file while
    /// verifying its declared size and SHA-256 digest before success.
    pub async fn get_to_new_file_verified(
        &self,
        object: &ImmutableObject,
        destination: &FilePath,
    ) -> Result<(), VolumeError> {
        self.validate_size(object.size_bytes)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(destination)
            .map_err(VolumeError::Io)?;
        let result = self
            .download_to_file(object, tokio::fs::File::from_std(file))
            .await;
        if result.is_err() {
            let _ = std::fs::remove_file(destination);
        }
        result
    }

    async fn verify_remote_object(&self, object: &ImmutableObject) -> Result<(), VolumeError> {
        let result = self
            .store
            .get(&self.location(object.digest))
            .await
            .map_err(map_store_error)?;
        if result.meta.size != object.size_bytes {
            return Err(VolumeError::IdentityMismatch);
        }
        let mut stream = result.into_stream();
        let mut digest = Sha256::new();
        let mut size_bytes = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_store_error)?;
            size_bytes = size_bytes
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| VolumeError::Invalid("object size overflows u64".into()))?;
            self.validate_size(size_bytes)?;
            digest.update(&chunk);
        }
        if size_bytes != object.size_bytes
            || ObjectDigest::from_sha256(digest.finalize().into()) != object.digest
        {
            return Err(VolumeError::IdentityMismatch);
        }
        Ok(())
    }

    async fn download_to_file(
        &self,
        object: &ImmutableObject,
        mut destination: tokio::fs::File,
    ) -> Result<(), VolumeError> {
        let result = self
            .store
            .get(&self.location(object.digest))
            .await
            .map_err(map_store_error)?;
        if result.meta.size != object.size_bytes {
            return Err(VolumeError::IdentityMismatch);
        }
        let mut stream = result.into_stream();
        let mut digest = Sha256::new();
        let mut size_bytes = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_store_error)?;
            size_bytes = size_bytes
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| VolumeError::Invalid("object size overflows u64".into()))?;
            self.validate_size(size_bytes)?;
            digest.update(&chunk);
            destination
                .write_all(&chunk)
                .await
                .map_err(VolumeError::Io)?;
        }
        destination.sync_all().await.map_err(VolumeError::Io)?;
        if size_bytes != object.size_bytes
            || ObjectDigest::from_sha256(digest.finalize().into()) != object.digest
        {
            return Err(VolumeError::IdentityMismatch);
        }
        Ok(())
    }

    pub async fn delete_verified(&self, object: &ImmutableObject) -> Result<(), VolumeError> {
        self.get_verified(object).await?;
        self.store
            .delete(&self.location(object.digest))
            .await
            .map_err(map_store_error)
    }

    fn validate_size(&self, size_bytes: u64) -> Result<(), VolumeError> {
        if size_bytes > self.max_object_bytes {
            return Err(VolumeError::Invalid(
                "immutable object exceeds configured maximum".into(),
            ));
        }
        Ok(())
    }

    #[cfg(feature = "cloud-object-store-aws")]
    pub fn s3_from_env(
        bucket: &str,
        prefix: impl Into<String>,
        max_object_bytes: u64,
        allow_insecure_http: bool,
    ) -> Result<Self, VolumeError> {
        if bucket.is_empty() {
            return Err(VolumeError::Invalid("S3 bucket is empty".into()));
        }
        let store = object_store::aws::AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .with_allow_http(allow_insecure_http)
            .build()
            .map_err(map_store_error)?;
        Self::new(
            "aws_s3_immutable_object",
            Arc::new(store),
            prefix,
            max_object_bytes,
        )
    }

    #[cfg(feature = "cloud-object-store-azure")]
    pub fn azure_from_env(
        container: &str,
        prefix: impl Into<String>,
        max_object_bytes: u64,
        allow_insecure_http: bool,
    ) -> Result<Self, VolumeError> {
        if container.is_empty() {
            return Err(VolumeError::Invalid("Azure container is empty".into()));
        }
        let store = object_store::azure::MicrosoftAzureBuilder::from_env()
            .with_container_name(container)
            .with_allow_http(allow_insecure_http)
            .build()
            .map_err(map_store_error)?;
        Self::new(
            "azure_blob_immutable_object",
            Arc::new(store),
            prefix,
            max_object_bytes,
        )
    }
}

fn validate_prefix(prefix: &str) -> Result<(), VolumeError> {
    if prefix.is_empty()
        || prefix.len() > 512
        || prefix.starts_with('/')
        || prefix.ends_with('/')
        || prefix
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
    {
        return Err(VolumeError::Invalid(
            "remote immutable object prefix is invalid".into(),
        ));
    }
    Ok(())
}

fn map_store_error(error: object_store::Error) -> VolumeError {
    match error {
        object_store::Error::NotFound { .. } => VolumeError::NotFound,
        object_store::Error::AlreadyExists { .. } => VolumeError::Conflict,
        object_store::Error::Precondition { .. } => VolumeError::IdentityMismatch,
        _ => VolumeError::Io(std::io::Error::other(
            "remote immutable object request failed",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    fn provider(store: Arc<dyn ObjectStore>) -> RemoteImmutableObjectProvider {
        RemoteImmutableObjectProvider::new("test_object", store, "tenant/artifacts", 1024).unwrap()
    }

    async fn remote_round_trip(provider: RemoteImmutableObjectProvider) {
        let payload = format!("tarit-object-transport-{}", uuid::Uuid::new_v4());
        let provider = Arc::new(provider);
        let mut writes = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let provider = Arc::clone(&provider);
            let payload = payload.clone();
            writes.spawn(async move { provider.put_if_absent(payload.as_bytes()).await });
        }
        let mut object = None;
        while let Some(result) = writes.join_next().await {
            let written = result.unwrap().unwrap();
            assert_eq!(*object.get_or_insert(written.clone()), written);
        }
        let object = object.expect("at least one conditional write completed");
        assert_eq!(
            provider.get_verified(&object).await.unwrap(),
            payload.as_bytes()
        );
        provider.delete_verified(&object).await.unwrap();
        assert!(matches!(
            provider.get_verified(&object).await,
            Err(VolumeError::NotFound)
        ));

        let token = uuid::Uuid::new_v4();
        let source = std::env::temp_dir().join(format!("tarit-object-source-{token}"));
        let destination = std::env::temp_dir().join(format!("tarit-object-destination-{token}"));
        let mut payload = vec![0_u8; 13 * 1024 * 1024 + 137];
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        std::fs::write(&source, &payload).unwrap();
        let streamed = provider.put_file_if_absent(&source).await.unwrap();
        assert_eq!(streamed.digest, ObjectDigest::from_bytes(&payload));
        assert_eq!(streamed.size_bytes, payload.len() as u64);
        provider
            .get_to_new_file_verified(&streamed, &destination)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), payload);
        provider.delete_verified(&streamed).await.unwrap();
        std::fs::remove_file(source).unwrap();
        std::fs::remove_file(destination).unwrap();

        let empty_source = std::env::temp_dir().join(format!("tarit-empty-source-{token}"));
        let empty_destination =
            std::env::temp_dir().join(format!("tarit-empty-destination-{token}"));
        std::fs::write(&empty_source, []).unwrap();
        let empty = provider.put_file_if_absent(&empty_source).await.unwrap();
        assert_eq!(empty.digest, ObjectDigest::from_bytes(&[]));
        assert_eq!(empty.size_bytes, 0);
        provider
            .get_to_new_file_verified(&empty, &empty_destination)
            .await
            .unwrap();
        assert!(std::fs::read(&empty_destination).unwrap().is_empty());
        provider.delete_verified(&empty).await.unwrap();
        std::fs::remove_file(empty_source).unwrap();
        std::fs::remove_file(empty_destination).unwrap();
    }

    #[tokio::test]
    async fn streaming_round_trip_is_bounded_verified_and_deletable() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let provider = RemoteImmutableObjectProvider::new(
            "test_object",
            store,
            "tenant/artifacts",
            20 * 1024 * 1024,
        )
        .unwrap();
        remote_round_trip(provider).await;
    }

    #[tokio::test]
    async fn immutable_put_is_idempotent_verified_and_deletable() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let provider = provider(store);
        let first = provider
            .put_if_absent(b"immutable-checkpoint")
            .await
            .unwrap();
        let replay = provider
            .put_if_absent(b"immutable-checkpoint")
            .await
            .unwrap();
        assert_eq!(first, replay);
        assert_eq!(
            provider.get_verified(&first).await.unwrap(),
            b"immutable-checkpoint"
        );
        provider.delete_verified(&first).await.unwrap();
        assert!(matches!(
            provider.get_verified(&first).await,
            Err(VolumeError::NotFound)
        ));
    }

    #[tokio::test]
    async fn collision_corruption_and_limits_fail_closed() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let provider = provider(store.clone());
        let object = provider.put_if_absent(b"original").await.unwrap();
        store
            .put(
                &provider.location(object.digest),
                b"tampered".as_slice().into(),
            )
            .await
            .unwrap();
        assert!(matches!(
            provider.get_verified(&object).await,
            Err(VolumeError::IdentityMismatch)
        ));
        assert!(matches!(
            provider.put_if_absent(b"original").await,
            Err(VolumeError::IdentityMismatch)
        ));
        assert!(matches!(
            provider.put_if_absent(&vec![0; 1025]).await,
            Err(VolumeError::Invalid(_))
        ));
    }

    #[test]
    fn names_and_prefixes_are_strict() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        for prefix in ["", "/root", "root/", "root//child", "root/../child"] {
            assert!(
                RemoteImmutableObjectProvider::new("test_object", store.clone(), prefix, 1,)
                    .is_err()
            );
        }
        assert!(
            RemoteImmutableObjectProvider::new("UPPERCASE", store, "tenant/artifacts", 1,).is_err()
        );
    }

    #[test]
    fn scoped_provider_is_strict_and_debug_redacted() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let provider = RemoteImmutableObjectProvider::new(
            "test_object",
            store,
            "private-fleet/artifacts",
            1024,
        )
        .unwrap();
        let scoped = provider.scoped("tenant-digest/snapshots").unwrap();
        let debug = format!("{scoped:?}");
        assert!(!debug.contains("private-fleet"));
        assert!(!debug.contains("tenant-digest"));
        assert!(scoped.scoped("../foreign").is_err());
    }

    #[cfg(feature = "cloud-object-store-aws")]
    #[tokio::test]
    #[ignore = "requires an isolated S3 bucket configured through AWS environment variables"]
    async fn s3_transport_round_trip() {
        let bucket = std::env::var("TARIT_TEST_S3_BUCKET").expect("TARIT_TEST_S3_BUCKET");
        let allow_http = std::env::var("TARIT_TEST_OBJECT_ALLOW_HTTP").as_deref() == Ok("1");
        let prefix = format!("tarit-e2e/{}", uuid::Uuid::new_v4());
        let provider =
            RemoteImmutableObjectProvider::s3_from_env(&bucket, prefix, 1024 * 1024, allow_http)
                .unwrap();
        remote_round_trip(provider).await;
    }

    #[cfg(feature = "cloud-object-store-azure")]
    #[tokio::test]
    #[ignore = "requires an isolated Azure Blob container configured through Azure environment variables"]
    async fn azure_transport_round_trip() {
        let container =
            std::env::var("TARIT_TEST_AZURE_CONTAINER").expect("TARIT_TEST_AZURE_CONTAINER");
        let allow_http = std::env::var("TARIT_TEST_OBJECT_ALLOW_HTTP").as_deref() == Ok("1");
        let prefix = format!("tarit-e2e/{}", uuid::Uuid::new_v4());
        let provider = RemoteImmutableObjectProvider::azure_from_env(
            &container,
            prefix,
            1024 * 1024,
            allow_http,
        )
        .unwrap();
        remote_round_trip(provider).await;
    }
}
