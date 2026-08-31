use crate::config::{Config, SharedBlockProviderKind};
use std::time::Duration;
use tarit_types::{OrchError, VolumeRecord, VolumeStatus, VolumeStorageClass};
use tarit_volume::{
    BlockVolumeProvider, LocalBlockProvider, NfsBackedBlockProvider, NfsDialect, NfsProvider,
    PlacementConstraint,
};

pub(crate) const MAX_LOCAL_BLOCK_VOLUME_BYTES: u64 = 1024 * 1024 * 1024 * 1024;

pub(crate) fn max_size_bytes(config: &Config, provider_name: &str) -> Option<u64> {
    if provider_name == "local_block" {
        return Some(MAX_LOCAL_BLOCK_VOLUME_BYTES);
    }
    config
        .shared_block
        .as_ref()
        .filter(|shared| shared.kind.provider_name() == provider_name)
        .map(|shared| shared.max_size_bytes)
}

pub(crate) fn placement(
    config: &Config,
    provider_name: &str,
) -> Result<PlacementConstraint, OrchError> {
    if provider_name == "local_block" {
        return Ok(PlacementConstraint {
            host_id: Some(config.host_id.clone()),
            region: Some(config.region.clone()),
            zone: Some(config.zone.clone()),
        });
    }
    let shared = config
        .shared_block
        .as_ref()
        .filter(|shared| shared.kind.provider_name() == provider_name)
        .ok_or_else(|| {
            OrchError::Unprocessable(format!(
                "volume provider {provider_name:?} is not configured on this host"
            ))
        })?;
    Ok(PlacementConstraint {
        host_id: None,
        region: Some(config.region.clone()),
        zone: match shared.kind {
            SharedBlockProviderKind::NfsV4_1 => None,
        },
    })
}

pub(crate) fn placement_target(
    config: &Config,
    volume: &VolumeRecord,
) -> Result<Option<String>, OrchError> {
    if volume.status != VolumeStatus::Available || volume.storage_class != VolumeStorageClass::Block
    {
        return Err(OrchError::Conflict(format!(
            "volume {} is not an available block volume",
            volume.id
        )));
    }
    if volume.provider == "local_block" {
        return volume.host_id.clone().map(Some).ok_or_else(|| {
            OrchError::Conflict(format!("volume {} has no exact-host placement", volume.id))
        });
    }
    config
        .shared_block
        .as_ref()
        .filter(|shared| shared.kind.provider_name() == volume.provider)
        .ok_or_else(|| {
            OrchError::Conflict(format!(
                "volume {} provider is not configured on this host",
                volume.id
            ))
        })?;
    if volume.host_id.is_some()
        || volume.region.as_deref() != Some(config.region.as_str())
        || volume.zone.is_some()
    {
        return Err(OrchError::Conflict(format!(
            "volume {} shared placement does not match this region",
            volume.id
        )));
    }
    Ok(None)
}

pub(crate) fn open(
    config: &Config,
    provider_name: &str,
) -> Result<Box<dyn BlockVolumeProvider>, OrchError> {
    if provider_name == "local_block" {
        return LocalBlockProvider::open(
            config.images_dir.join("volumes"),
            config.host_id.clone(),
            MAX_LOCAL_BLOCK_VOLUME_BYTES,
        )
        .map(|provider| Box::new(provider) as Box<dyn BlockVolumeProvider>)
        .map_err(|error| {
            tracing::error!(%error, "initialize local volume provider");
            OrchError::Internal("initialize volume provider".into())
        });
    }
    let shared = config
        .shared_block
        .as_ref()
        .filter(|shared| shared.kind.provider_name() == provider_name)
        .ok_or_else(|| {
            OrchError::Unprocessable(format!(
                "volume provider {provider_name:?} is not configured on this host"
            ))
        })?;
    let dialect = match shared.kind {
        SharedBlockProviderKind::NfsV4_1 => NfsDialect::GenericV4_1,
    };
    let nfs = NfsProvider::new(
        dialect,
        shared.endpoint.clone(),
        shared.export.clone(),
        Some(config.region.clone()),
        None,
    )
    .and_then(|provider| provider.with_security(shared.security))
    .map_err(|error| {
        tracing::error!(%error, provider = provider_name, "initialize shared NFS profile");
        OrchError::Internal("initialize shared volume provider".into())
    })?;
    NfsBackedBlockProvider::open(
        nfs,
        &shared.mount_root,
        shared.max_size_bytes,
        Duration::from_millis(shared.operation_timeout_ms),
    )
    .map(|provider| Box::new(provider) as Box<dyn BlockVolumeProvider>)
    .map_err(|error| {
        tracing::error!(%error, provider = provider_name, "initialize shared block provider");
        OrchError::Internal("initialize shared volume provider".into())
    })
}
