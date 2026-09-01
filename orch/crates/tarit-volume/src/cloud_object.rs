use crate::{ImmutableObject, ObjectDigest, VolumeError};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode};
use std::sync::Arc;

/// Content-addressed immutable blobs backed by a remote object store.
///
/// The provider intentionally exposes no filesystem, random-write, append,
/// rename, or locking operation. Callers must retain their own durable
/// references and invoke deletion only after reference-counted GC permits it.
#[derive(Debug)]
pub struct RemoteImmutableObjectProvider {
    provider_name: &'static str,
    store: Arc<dyn ObjectStore>,
    prefix: String,
    max_object_bytes: u64,
}

impl RemoteImmutableObjectProvider {
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
        if max_object_bytes == 0 {
            return Err(VolumeError::Invalid(
                "maximum immutable object size must be positive".into(),
            ));
        }
        validate_prefix(&prefix)?;
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

    fn location(&self, digest: ObjectDigest) -> Path {
        Path::from(format!("{}/{}.blob", self.prefix, digest.hex()))
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
