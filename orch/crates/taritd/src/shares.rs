#![allow(dead_code)]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tarit_fleet::{FleetError, PostgresFleet};
use tarit_store::{Store, StoreError};
use tarit_types::{
    CreateShareRequest, OrchError, ShareRecord, ShareTokenResponse, ShareVisibility,
    UpdateShareRequest, VmRecord, VmStatus,
};
use uuid::Uuid;

use crate::{
    api::{self, AppState},
    cluster::{self, Owner},
    config::ApiIdentity,
    ops,
};

const SHARE_TOKEN_AUDIENCE: &str = "tarit-share";
const MAX_SHARE_TOKEN_LEN: usize = 4096;
const MAX_SLUG_ATTEMPTS: usize = 8;

#[derive(Clone)]
pub struct ShareRepository {
    local: Arc<Mutex<Store>>,
    fleet: Option<Arc<PostgresFleet>>,
    mutation_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ShareRepository {
    pub fn new(local: Arc<Mutex<Store>>, fleet: Option<Arc<PostgresFleet>>) -> Self {
        Self {
            local,
            fleet,
            mutation_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub async fn get(&self, id: Uuid) -> Result<Option<ShareRecord>, OrchError> {
        if let Some(fleet) = &self.fleet {
            return fleet.get_share(id).await.map_err(map_fleet_error);
        }
        let store = self
            .local
            .lock()
            .map_err(|_| OrchError::Internal("share store lock".into()))?;
        match store.get_share(id) {
            Ok(share) => Ok(Some(share)),
            Err(StoreError::NotFound) => Ok(None),
            Err(error) => Err(map_store_error(error)),
        }
    }

    pub async fn get_by_slug(&self, slug: &str) -> Result<Option<ShareRecord>, OrchError> {
        if let Some(fleet) = &self.fleet {
            return fleet.get_share_by_slug(slug).await.map_err(map_fleet_error);
        }
        let store = self
            .local
            .lock()
            .map_err(|_| OrchError::Internal("share store lock".into()))?;
        store.get_share_by_slug(slug).map_err(map_store_error)
    }

    pub async fn insert(&self, share: &ShareRecord) -> Result<(), OrchError> {
        if let Some(fleet) = &self.fleet {
            return fleet.insert_share(share).await.map_err(map_fleet_error);
        }
        let store = self
            .local
            .lock()
            .map_err(|_| OrchError::Internal("share store lock".into()))?;
        store.insert_share(share).map_err(map_store_error)
    }

    pub async fn update(&self, share: &ShareRecord) -> Result<(), OrchError> {
        if let Some(fleet) = &self.fleet {
            return fleet.update_share(share).await.map_err(map_fleet_error);
        }
        let store = self
            .local
            .lock()
            .map_err(|_| OrchError::Internal("share store lock".into()))?;
        store.update_share(share).map_err(map_store_error)
    }

    pub async fn update_if_current(
        &self,
        share: &ShareRecord,
        expected_token_version: u64,
    ) -> Result<(), OrchError> {
        if let Some(fleet) = &self.fleet {
            return fleet
                .update_share_if_current(share, expected_token_version)
                .await
                .map_err(map_fleet_error);
        }
        let store = self
            .local
            .lock()
            .map_err(|_| OrchError::Internal("share store lock".into()))?;
        store
            .update_share_if_current(share, expected_token_version)
            .map_err(map_store_error)
    }

    pub async fn list(&self, owner_key: &str) -> Result<Vec<ShareRecord>, OrchError> {
        if let Some(fleet) = &self.fleet {
            return fleet.list_shares(owner_key).await.map_err(map_fleet_error);
        }
        let store = self
            .local
            .lock()
            .map_err(|_| OrchError::Internal("share store lock".into()))?;
        store.list_shares(owner_key).map_err(map_store_error)
    }
}

fn map_store_error(error: StoreError) -> OrchError {
    match error {
        StoreError::NotFound => OrchError::NotFound("share not found".into()),
        StoreError::Conflict(message) => OrchError::Conflict(message),
        StoreError::Sqlite(error) => OrchError::Internal(format!("share store: {error}")),
    }
}

fn map_fleet_error(error: FleetError) -> OrchError {
    match error {
        FleetError::NotFound => OrchError::NotFound("share not found".into()),
        FleetError::Conflict(message) => OrchError::Conflict(message),
        error => OrchError::Internal(format!("share fleet: {error}")),
    }
}

async fn insert_with_generated_slug<F>(
    repository: &ShareRepository,
    owner_key: &str,
    vm_id: Uuid,
    guest_port: u16,
    visibility: ShareVisibility,
    now: DateTime<Utc>,
    mut next_slug: F,
) -> Result<ShareRecord, OrchError>
where
    F: FnMut() -> String,
{
    for _ in 0..MAX_SLUG_ATTEMPTS {
        let share = ShareRecord {
            id: Uuid::new_v4(),
            slug: next_slug(),
            owner_key: owner_key.into(),
            vm_id,
            guest_port,
            visibility,
            token_version: 0,
            revoked_at: None,
            created_at: now,
            updated_at: now,
        };
        match repository.insert(&share).await {
            Ok(()) => return Ok(share),
            Err(OrchError::Conflict(_)) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(OrchError::Conflict(
        "unable to allocate a unique share slug".into(),
    ))
}

fn new_slug() -> String {
    Uuid::new_v4().simple().to_string()
}

fn apply_update(
    share: &ShareRecord,
    request: &UpdateShareRequest,
    now: DateTime<Utc>,
) -> Result<ShareRecord, OrchError> {
    request.validate()?;
    if share.revoked_at.is_some() {
        return Err(OrchError::Conflict("share is revoked".into()));
    }

    let vm_id = request.vm_id.unwrap_or(share.vm_id);
    let guest_port = request.guest_port.unwrap_or(share.guest_port);
    let visibility = request.visibility.unwrap_or(share.visibility);
    let rotated =
        vm_id != share.vm_id || guest_port != share.guest_port || visibility != share.visibility;
    let token_version = if rotated {
        share
            .token_version
            .checked_add(1)
            .ok_or_else(|| OrchError::Internal("share token version overflow".into()))?
    } else {
        share.token_version
    };

    Ok(ShareRecord {
        vm_id,
        guest_port,
        visibility,
        token_version,
        updated_at: if rotated { now } else { share.updated_at },
        ..share.clone()
    })
}

pub async fn create(
    state: &AppState,
    identity: &ApiIdentity,
    request: CreateShareRequest,
) -> Result<ShareRecord, OrchError> {
    let vm = resolve_authorized_running_vm(state, identity, request.vm_id).await?;
    create_for_vm(&state.shares, identity, &vm, request).await
}

pub async fn update(
    state: &AppState,
    identity: &ApiIdentity,
    share_id: Uuid,
    request: UpdateShareRequest,
) -> Result<ShareRecord, OrchError> {
    request.validate()?;
    let replacement_vm = match request.vm_id {
        Some(vm_id) => Some(resolve_authorized_running_vm(state, identity, vm_id).await?),
        None => None,
    };
    let _mutation = state.shares.mutation_lock.lock().await;
    update_for_share(
        &state.shares,
        identity,
        share_id,
        replacement_vm.as_ref(),
        request,
        Utc::now(),
    )
    .await
}

pub async fn revoke(
    state: &AppState,
    identity: &ApiIdentity,
    share_id: Uuid,
) -> Result<ShareRecord, OrchError> {
    let _mutation = state.shares.mutation_lock.lock().await;
    revoke_for_share(&state.shares, identity, share_id, Utc::now()).await
}

pub async fn list(state: &AppState, identity: &ApiIdentity) -> Result<Vec<ShareRecord>, OrchError> {
    state.shares.list(&identity.tenant).await
}

pub async fn get(
    state: &AppState,
    identity: &ApiIdentity,
    share_id: Uuid,
) -> Result<ShareRecord, OrchError> {
    get_owned_share(&state.shares, identity, share_id).await
}

pub async fn get_visible(state: &AppState, slug: &str) -> Result<ShareRecord, OrchError> {
    get_visible_from_repository(&state.shares, slug).await
}

pub async fn issue_token(
    state: &AppState,
    identity: &ApiIdentity,
    share_id: Uuid,
    now: DateTime<Utc>,
) -> Result<ShareTokenResponse, OrchError> {
    let share = get_owned_share(&state.shares, identity, share_id).await?;
    let signer = signer_from_state(state)?;
    Ok(ShareTokenResponse {
        token: signer.issue(&share, now)?,
        expires_at: token_expiry_at(now, state.config.share_token_ttl_secs)?,
    })
}

pub async fn authorize_gateway(
    state: &AppState,
    slug: &str,
    token: Option<&str>,
) -> Result<ShareRecord, OrchError> {
    let share = get_visible(state, slug).await?;
    let signer = match share.visibility {
        ShareVisibility::Public => None,
        ShareVisibility::Private => Some(signer_from_state(state)?),
    };
    authorize_share(&share, token, signer.as_ref())
}

async fn create_for_vm(
    repository: &ShareRepository,
    identity: &ApiIdentity,
    vm: &VmRecord,
    request: CreateShareRequest,
) -> Result<ShareRecord, OrchError> {
    request.validate()?;
    ensure_share_vm_access(identity, vm)?;
    let owner_key = share_vm_owner(vm)?;
    insert_with_generated_slug(
        repository,
        owner_key,
        vm.id,
        request.guest_port,
        request.visibility,
        Utc::now(),
        new_slug,
    )
    .await
}

async fn update_for_share(
    repository: &ShareRepository,
    identity: &ApiIdentity,
    share_id: Uuid,
    replacement_vm: Option<&VmRecord>,
    request: UpdateShareRequest,
    now: DateTime<Utc>,
) -> Result<ShareRecord, OrchError> {
    let share = get_owned_share(repository, identity, share_id).await?;
    if let Some(vm_id) = request.vm_id {
        let vm = replacement_vm.ok_or_else(|| {
            OrchError::BadRequest("replacement VM must be resolved before update".into())
        })?;
        if vm.id != vm_id {
            return Err(OrchError::BadRequest(
                "replacement VM does not match update request".into(),
            ));
        }
        ensure_share_vm_access(identity, vm)?;
        if vm.owner_key.as_deref() != Some(share.owner_key.as_str()) {
            return Err(OrchError::Forbidden(
                "replacement VM does not belong to the share owner".into(),
            ));
        }
    }
    let updated = apply_update(&share, &request, now)?;
    if updated.token_version != share.token_version {
        repository
            .update_if_current(&updated, share.token_version)
            .await?;
    }
    Ok(updated)
}

async fn revoke_for_share(
    repository: &ShareRepository,
    identity: &ApiIdentity,
    share_id: Uuid,
    now: DateTime<Utc>,
) -> Result<ShareRecord, OrchError> {
    let share = get_owned_share(repository, identity, share_id).await?;
    if share.revoked_at.is_some() {
        return Ok(share);
    }
    let revoked = ShareRecord {
        token_version: share
            .token_version
            .checked_add(1)
            .ok_or_else(|| OrchError::Internal("share token version overflow".into()))?,
        revoked_at: Some(now),
        updated_at: now,
        ..share
    };
    repository
        .update_if_current(&revoked, share.token_version)
        .await?;
    Ok(revoked)
}

async fn get_visible_from_repository(
    repository: &ShareRepository,
    slug: &str,
) -> Result<ShareRecord, OrchError> {
    let share = repository
        .get_by_slug(slug)
        .await?
        .ok_or_else(|| OrchError::NotFound("share not found".into()))?;
    if share.revoked_at.is_some() {
        return Err(OrchError::NotFound("share not found".into()));
    }
    Ok(share)
}

async fn authorize_with_repository(
    repository: &ShareRepository,
    slug: &str,
    token: Option<&str>,
    signer: Option<&ShareTokenSigner>,
) -> Result<ShareRecord, OrchError> {
    let share = get_visible_from_repository(repository, slug).await?;
    authorize_share(&share, token, signer)
}

fn authorize_share(
    share: &ShareRecord,
    token: Option<&str>,
    signer: Option<&ShareTokenSigner>,
) -> Result<ShareRecord, OrchError> {
    if share.visibility == ShareVisibility::Public {
        return Ok(share.clone());
    }
    let token = token.ok_or(OrchError::Unauthorized)?;
    let signer =
        signer.ok_or_else(|| OrchError::Internal("share token signer is unavailable".into()))?;
    signer.verify(token, share, Utc::now())?;
    Ok(share.clone())
}

async fn get_owned_share(
    repository: &ShareRepository,
    identity: &ApiIdentity,
    share_id: Uuid,
) -> Result<ShareRecord, OrchError> {
    let share = repository
        .get(share_id)
        .await?
        .ok_or_else(|| OrchError::NotFound("share not found".into()))?;
    if identity.is_admin() || share.owner_key == identity.tenant {
        Ok(share)
    } else {
        Err(OrchError::Forbidden(
            "share does not belong to this tenant".into(),
        ))
    }
}

fn ensure_share_vm_access(identity: &ApiIdentity, vm: &VmRecord) -> Result<(), OrchError> {
    api::ensure_vm_access(identity, vm)?;
    if vm.status != VmStatus::Running {
        return Err(OrchError::Conflict(
            "VM must be running before it can be shared".into(),
        ));
    }
    Ok(())
}

fn share_vm_owner(vm: &VmRecord) -> Result<&str, OrchError> {
    vm.owner_key.as_deref().ok_or_else(|| {
        OrchError::BadRequest("VM must have a tenant owner before it can be shared".into())
    })
}

async fn resolve_authorized_running_vm(
    state: &AppState,
    identity: &ApiIdentity,
    vm_id: Uuid,
) -> Result<VmRecord, OrchError> {
    let vm = match cluster::resolve_owner(state, vm_id).await? {
        Owner::Local => ops::get_local(state, vm_id)?,
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || peer.get_remote(&rpc, vm_id, &identity))
                .await
                .map_err(|error| OrchError::Internal(format!("share VM lookup join: {error}")))??
        }
    };
    ensure_share_vm_access(identity, &vm)?;
    Ok(vm)
}

fn signer_from_state(state: &AppState) -> Result<ShareTokenSigner, OrchError> {
    let key = state
        .config
        .share_token_key
        .ok_or_else(|| OrchError::Internal("share token signer is unavailable".into()))?;
    Ok(ShareTokenSigner::new(
        key,
        Duration::from_secs(state.config.share_token_ttl_secs),
    ))
}

fn token_expiry_at(now: DateTime<Utc>, ttl_secs: u64) -> Result<DateTime<Utc>, OrchError> {
    let ttl = i64::try_from(ttl_secs)
        .ok()
        .filter(|ttl| *ttl > 0)
        .ok_or_else(|| OrchError::Internal("share token TTL must be positive".into()))?;
    let exp = now
        .timestamp()
        .checked_add(ttl)
        .ok_or_else(|| OrchError::Internal("share token expiry overflow".into()))?;
    DateTime::from_timestamp(exp, 0)
        .ok_or_else(|| OrchError::Internal("share token expiry is invalid".into()))
}

#[derive(Clone)]
pub struct ShareTokenSigner {
    key: [u8; 32],
    ttl: Duration,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareTokenClaims {
    share_id: Uuid,
    owner_key: String,
    token_version: u64,
    aud: String,
    iat: i64,
    exp: i64,
}

impl ShareTokenSigner {
    pub fn new(key: [u8; 32], ttl: Duration) -> Self {
        Self { key, ttl }
    }

    pub fn issue(&self, share: &ShareRecord, now: DateTime<Utc>) -> Result<String, OrchError> {
        if share.visibility != ShareVisibility::Private || share.revoked_at.is_some() {
            return Err(OrchError::BadRequest(
                "tokens can only be issued for active private shares".into(),
            ));
        }
        let iat = now.timestamp();
        let exp = iat
            .checked_add(self.ttl_seconds()?)
            .ok_or_else(|| OrchError::Internal("share token expiry overflow".into()))?;
        let claims = ShareTokenClaims {
            share_id: share.id,
            owner_key: share.owner_key.clone(),
            token_version: share.token_version,
            aud: SHARE_TOKEN_AUDIENCE.into(),
            iat,
            exp,
        };
        let payload = serde_json::to_vec(&claims)
            .map_err(|_| OrchError::Internal("encode share token".into()))?;
        let payload = URL_SAFE_NO_PAD.encode(payload);
        let signature = self.sign(payload.as_bytes())?;
        Ok(format!("{payload}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }

    pub fn verify(
        &self,
        token: &str,
        share: &ShareRecord,
        now: DateTime<Utc>,
    ) -> Result<(), OrchError> {
        if token.len() > MAX_SHARE_TOKEN_LEN
            || share.visibility != ShareVisibility::Private
            || share.revoked_at.is_some()
        {
            return Err(OrchError::Unauthorized);
        }

        let mut parts = token.split('.');
        let (Some(payload), Some(signature), None) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(OrchError::Unauthorized);
        };
        if payload.is_empty() || signature.is_empty() {
            return Err(OrchError::Unauthorized);
        }

        let signature = decode_base64url(signature).ok_or(OrchError::Unauthorized)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key)
            .map_err(|_| OrchError::Internal("initialize share token signer".into()))?;
        mac.update(payload.as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| OrchError::Unauthorized)?;

        let payload = decode_base64url(payload).ok_or(OrchError::Unauthorized)?;
        let claims: ShareTokenClaims =
            serde_json::from_slice(&payload).map_err(|_| OrchError::Unauthorized)?;
        let now = now.timestamp();
        let expected_exp = claims
            .iat
            .checked_add(self.ttl_seconds().map_err(|_| OrchError::Unauthorized)?)
            .ok_or(OrchError::Unauthorized)?;
        if claims.aud != SHARE_TOKEN_AUDIENCE
            || claims.share_id != share.id
            || claims.owner_key != share.owner_key
            || claims.token_version != share.token_version
            || claims.iat > now
            || claims.exp != expected_exp
            || claims.exp <= now
        {
            return Err(OrchError::Unauthorized);
        }
        Ok(())
    }

    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>, OrchError> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key)
            .map_err(|_| OrchError::Internal("initialize share token signer".into()))?;
        mac.update(payload);
        Ok(mac.finalize().into_bytes().to_vec())
    }

    fn ttl_seconds(&self) -> Result<i64, OrchError> {
        i64::try_from(self.ttl.as_secs())
            .ok()
            .filter(|ttl| *ttl > 0)
            .ok_or_else(|| OrchError::Internal("share token TTL must be positive".into()))
    }
}

fn decode_base64url(encoded: &str) -> Option<Vec<u8>> {
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    (URL_SAFE_NO_PAD.encode(&decoded) == encoded).then_some(decoded)
}

#[cfg(test)]
mod tests {
    use super::{
        apply_update, authorize_with_repository, create_for_vm, insert_with_generated_slug,
        revoke_for_share, token_expiry_at, update_for_share, ShareRepository, ShareTokenSigner,
        MAX_SLUG_ATTEMPTS,
    };
    use crate::config::{ApiIdentity, ApiRole};
    use base64::Engine as _;
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tarit_store::Store;
    use tarit_types::{
        CreateShareRequest, OrchError, ShareRecord, ShareVisibility, UpdateShareRequest, VmRecord,
        VmStatus,
    };
    use uuid::Uuid;

    fn test_private_share(token_version: u64) -> ShareRecord {
        let now = Utc::now();
        ShareRecord {
            id: Uuid::new_v4(),
            slug: "test-share".into(),
            owner_key: "tenant-a".into(),
            vm_id: Uuid::new_v4(),
            guest_port: 8080,
            visibility: ShareVisibility::Private,
            token_version,
            revoked_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn token_rejects_wrong_version_and_expiry() {
        let signer = ShareTokenSigner::new([7u8; 32], Duration::from_secs(300));
        let share = test_private_share(4);
        let token = signer.issue(&share, Utc::now()).unwrap();
        assert!(signer.verify(&token, &share, Utc::now()).is_ok());

        let rotated = ShareRecord {
            token_version: 5,
            ..share.clone()
        };
        assert!(signer.verify(&token, &rotated, Utc::now()).is_err());
        assert!(signer
            .verify(&token, &share, Utc::now() + ChronoDuration::minutes(6))
            .is_err());
    }

    #[test]
    fn token_rejects_malformed_noncanonical_and_wrong_owner_input_without_panicking() {
        let signer = ShareTokenSigner::new([7u8; 32], Duration::from_secs(300));
        let share = test_private_share(4);
        let token = signer.issue(&share, Utc::now()).unwrap();
        let another_owner = ShareRecord {
            owner_key: "tenant-b".into(),
            ..share.clone()
        };

        for malformed in [
            "",
            "no-separator",
            ".",
            "..",
            "a.b.c",
            "%%%%.%%%%",
            &format!("{token}="),
        ] {
            let result = std::panic::catch_unwind(|| signer.verify(malformed, &share, Utc::now()));
            assert!(result.is_ok(), "malformed token must not panic");
            assert!(result.unwrap().is_err());
        }
        assert!(signer.verify(&token, &another_owner, Utc::now()).is_err());
        assert!(signer
            .verify(&token, &share, Utc::now() + ChronoDuration::seconds(300))
            .is_err());
    }

    #[test]
    fn token_requires_exact_audience_and_nonfuture_issued_at() {
        let signer = ShareTokenSigner::new([7u8; 32], Duration::from_secs(300));
        let share = test_private_share(4);
        let now = Utc::now();
        let iat = now.timestamp();

        let wrong_audience = signed_token(&signer, &share, "other-audience", iat, iat + 300);
        assert!(signer.verify(&wrong_audience, &share, now).is_err());

        let future_issued_at = signed_token(
            &signer,
            &share,
            super::SHARE_TOKEN_AUDIENCE,
            iat + 1,
            iat + 301,
        );
        assert!(signer.verify(&future_issued_at, &share, now).is_err());
    }

    #[test]
    fn token_response_expiry_matches_the_integer_second_in_its_claim() {
        let issued_at = Utc.timestamp_opt(1_000, 900_000_000).single().unwrap();
        assert_eq!(
            token_expiry_at(issued_at, 300).unwrap(),
            Utc.timestamp_opt(1_300, 0).single().unwrap()
        );
    }

    #[tokio::test]
    async fn repository_uses_sqlite_and_preserves_conflicts() {
        let repository =
            ShareRepository::new(Arc::new(Mutex::new(Store::open(":memory:").unwrap())), None);
        let share = test_private_share(0);
        repository.insert(&share).await.unwrap();
        assert_eq!(
            repository
                .get_by_slug(&share.slug)
                .await
                .unwrap()
                .unwrap()
                .id,
            share.id
        );
        let duplicate = ShareRecord {
            id: Uuid::new_v4(),
            ..share.clone()
        };
        assert!(matches!(
            repository.insert(&duplicate).await,
            Err(tarit_types::OrchError::Conflict(_))
        ));
        assert_eq!(repository.list(&share.owner_key).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn repository_rejects_a_stale_share_mutation() {
        let repository =
            ShareRepository::new(Arc::new(Mutex::new(Store::open(":memory:").unwrap())), None);
        let share = test_private_share(0);
        repository.insert(&share).await.unwrap();

        let first_update = ShareRecord {
            guest_port: 9090,
            token_version: 1,
            ..share.clone()
        };
        repository
            .update_if_current(&first_update, share.token_version)
            .await
            .unwrap();
        let stale_update = ShareRecord {
            visibility: ShareVisibility::Public,
            token_version: 1,
            ..share.clone()
        };
        assert!(matches!(
            repository
                .update_if_current(&stale_update, share.token_version)
                .await,
            Err(OrchError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn create_retries_slug_collisions_within_the_eight_attempt_budget() {
        let repository =
            ShareRepository::new(Arc::new(Mutex::new(Store::open(":memory:").unwrap())), None);
        repository.insert(&test_private_share(0)).await.unwrap();
        let mut attempts = 0;
        let created = insert_with_generated_slug(
            &repository,
            "tenant-a",
            Uuid::new_v4(),
            8081,
            ShareVisibility::Private,
            Utc::now(),
            || {
                attempts += 1;
                if attempts < MAX_SLUG_ATTEMPTS {
                    "test-share".into()
                } else {
                    "available-share".into()
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(attempts, MAX_SLUG_ATTEMPTS);
        assert_eq!(created.slug, "available-share");

        let mut exhausted_attempts = 0;
        let exhausted = insert_with_generated_slug(
            &repository,
            "tenant-a",
            Uuid::new_v4(),
            8082,
            ShareVisibility::Private,
            Utc::now(),
            || {
                exhausted_attempts += 1;
                "test-share".into()
            },
        )
        .await;
        assert!(matches!(
            exhausted,
            Err(tarit_types::OrchError::Conflict(_))
        ));
        assert_eq!(exhausted_attempts, MAX_SLUG_ATTEMPTS);
    }

    #[tokio::test]
    async fn admin_create_uses_the_target_vm_tenant_as_share_owner() {
        let repository =
            ShareRepository::new(Arc::new(Mutex::new(Store::open(":memory:").unwrap())), None);
        let admin = ApiIdentity {
            tenant: "admin-tenant".into(),
            role: ApiRole::Admin,
            max_vms: None,
            api_key_id: "admin-key".into(),
        };
        let vm = test_vm("tenant-a", VmStatus::Running);

        let share = create_for_vm(
            &repository,
            &admin,
            &vm,
            CreateShareRequest {
                vm_id: vm.id,
                guest_port: 8080,
                visibility: ShareVisibility::Private,
            },
        )
        .await
        .unwrap();

        assert_eq!(share.owner_key, "tenant-a");
    }

    #[tokio::test]
    async fn admin_cannot_retarget_a_share_to_another_tenant_vm() {
        let repository =
            ShareRepository::new(Arc::new(Mutex::new(Store::open(":memory:").unwrap())), None);
        let admin = ApiIdentity {
            tenant: "admin-tenant".into(),
            role: ApiRole::Admin,
            max_vms: None,
            api_key_id: "admin-key".into(),
        };
        let share = test_private_share(0);
        repository.insert(&share).await.unwrap();
        let replacement = test_vm("tenant-b", VmStatus::Running);

        assert!(matches!(
            update_for_share(
                &repository,
                &admin,
                share.id,
                Some(&replacement),
                UpdateShareRequest {
                    vm_id: Some(replacement.id),
                    ..Default::default()
                },
                Utc::now(),
            )
            .await,
            Err(OrchError::Forbidden(_))
        ));
    }

    #[test]
    fn update_rotates_token_version_only_for_authorization_relevant_changes() {
        let share = test_private_share(4);
        let unchanged = apply_update(&share, &UpdateShareRequest::default(), Utc::now()).unwrap();
        assert_eq!(unchanged.token_version, 4);

        let updated = apply_update(
            &share,
            &UpdateShareRequest {
                guest_port: Some(9090),
                ..Default::default()
            },
            Utc::now(),
        )
        .unwrap();
        assert_eq!(updated.guest_port, 9090);
        assert_eq!(updated.token_version, 5);
    }

    #[tokio::test]
    async fn lifecycle_checks_vm_access_running_state_rotates_and_revokes_terminally() {
        let repository =
            ShareRepository::new(Arc::new(Mutex::new(Store::open(":memory:").unwrap())), None);
        let identity = ApiIdentity {
            tenant: "tenant-a".into(),
            role: ApiRole::User,
            max_vms: None,
            api_key_id: "test-key".into(),
        };
        let other_identity = ApiIdentity {
            tenant: "tenant-b".into(),
            ..identity.clone()
        };
        let vm = test_vm("tenant-a", VmStatus::Running);
        let request = CreateShareRequest {
            vm_id: vm.id,
            guest_port: 8080,
            visibility: ShareVisibility::Private,
        };

        assert!(matches!(
            create_for_vm(&repository, &other_identity, &vm, request.clone()).await,
            Err(OrchError::Forbidden(_))
        ));
        let stopped = VmRecord {
            status: VmStatus::Stopped,
            ..vm.clone()
        };
        assert!(matches!(
            create_for_vm(&repository, &identity, &stopped, request.clone()).await,
            Err(OrchError::Conflict(_))
        ));

        let created = create_for_vm(&repository, &identity, &vm, request)
            .await
            .unwrap();
        let updated = update_for_share(
            &repository,
            &identity,
            created.id,
            None,
            UpdateShareRequest {
                guest_port: Some(9090),
                ..Default::default()
            },
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(updated.token_version, created.token_version + 1);

        let revoked = revoke_for_share(&repository, &identity, updated.id, Utc::now())
            .await
            .unwrap();
        assert!(revoked.revoked_at.is_some());
        assert_eq!(revoked.token_version, updated.token_version + 1);
        assert!(matches!(
            update_for_share(
                &repository,
                &identity,
                revoked.id,
                None,
                UpdateShareRequest {
                    guest_port: Some(10000),
                    ..Default::default()
                },
                Utc::now(),
            )
            .await,
            Err(OrchError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn gateway_authorization_requires_active_private_token() {
        let repository =
            ShareRepository::new(Arc::new(Mutex::new(Store::open(":memory:").unwrap())), None);
        let private = test_private_share(0);
        repository.insert(&private).await.unwrap();
        let signer = ShareTokenSigner::new([3u8; 32], Duration::from_secs(300));
        let token = signer.issue(&private, Utc::now()).unwrap();
        assert_eq!(
            authorize_with_repository(
                &repository,
                private.slug.as_str(),
                Some(&token),
                Some(&signer)
            )
            .await
            .unwrap()
            .id,
            private.id
        );
        assert!(matches!(
            authorize_with_repository(&repository, private.slug.as_str(), None, Some(&signer))
                .await,
            Err(OrchError::Unauthorized)
        ));

        let public = ShareRecord {
            id: Uuid::new_v4(),
            slug: "public-share".into(),
            visibility: ShareVisibility::Public,
            ..private.clone()
        };
        repository.insert(&public).await.unwrap();
        assert_eq!(
            authorize_with_repository(&repository, public.slug.as_str(), None, None)
                .await
                .unwrap()
                .id,
            public.id
        );
    }

    fn test_vm(owner_key: &str, status: VmStatus) -> VmRecord {
        let now = Utc::now();
        VmRecord {
            id: Uuid::new_v4(),
            host_id: "test-host".into(),
            owner_key: Some(owner_key.into()),
            api_key_id: Some("test-key".into()),
            status,
            memory_mib: 256,
            vcpus: 1,
            kernel_path: "kernel".into(),
            rootfs_path: None,
            cmdline: "console=ttyS0".into(),
            socket_path: None,
            pid: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn signed_token(
        signer: &ShareTokenSigner,
        share: &ShareRecord,
        audience: &str,
        iat: i64,
        exp: i64,
    ) -> String {
        let claims = super::ShareTokenClaims {
            share_id: share.id,
            owner_key: share.owner_key.clone(),
            token_version: share.token_version,
            aud: audience.into(),
            iat,
            exp,
        };
        let payload = super::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let signature = signer.sign(payload.as_bytes()).unwrap();
        format!("{payload}.{}", super::URL_SAFE_NO_PAD.encode(signature))
    }
}
