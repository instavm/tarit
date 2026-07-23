//! Internal peer-facing routes authenticated with short-lived, target-bound
//! request HMACs. The shared cluster key is never transmitted.
//!
//! These are the "execute on THIS node" endpoints that a public handler on
//! another node forwards to when it does not own the target VM (or is placing a
//! new VM here). They call the same node-local `ops` as the public API, so
//! behavior is identical whether a request arrives from a client or a peer.

use axum::{
    body::{to_bytes, Body},
    extract::{DefaultBodyLimit, Extension, Path, State},
    http::{HeaderMap, Request, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{any, get, patch, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tarit_types::{CreateVmRequest, EgressUpdateRequest, RestoreRequest, VmRecord};
use uuid::Uuid;

use crate::{
    api::{
        enforce_api_traffic, enforce_create_path_policy, ensure_vm_access, ApiError,
        ApiTrafficLimits, AppState,
    },
    cluster::Owner,
    share_gateway::{self, resolve_share_owner, share_peer_identity_id, GatewayError},
};

const IDENTITY_SIGNATURE_VERSION: &str = "tarit-peer-identity-v1";
const REQUEST_SIGNATURE_VERSION: &str = "tarit-peer-request-v1";
const STREAMING_PAYLOAD: &str = "STREAMING-UNSIGNED-PAYLOAD";
// A 10-second acceptance window tolerates ordinary clock skew while keeping a
// captured signature's replay lifetime short. At the default 5,000 request/s
// limit, a single source can retain the full window without saturating its
// 65,536-entry bucket.
const MAX_PEER_IDENTITY_AGE_SECS: u64 = 10;
const MAX_PEER_SOURCE_LEN: usize = 128;
const MAX_TRACKED_NONCES_PER_SOURCE: usize = 65_536;
const MAX_TRACKED_NONCES_TOTAL: usize = 262_144;
const MAX_TRACKED_PEER_SOURCES: usize = 1_024;
static USED_PEER_IDENTITY_NONCES: OnceLock<Mutex<ReplayCache>> = OnceLock::new();
static USED_PEER_REQUEST_NONCES: OnceLock<Mutex<ReplayCache>> = OnceLock::new();

#[derive(Default)]
struct ReplayCache {
    sources: HashMap<String, HashSet<Uuid>>,
    expirations: VecDeque<(Instant, String, Uuid)>,
    tracked: usize,
}

#[derive(Clone, Copy)]
struct ReplayLimits {
    per_source: usize,
    total: usize,
    sources: usize,
}

#[derive(Clone)]
struct VerifiedPeerSource(String);
use crate::config::{ApiIdentity, ApiRole};
use crate::ops;

#[derive(serde::Serialize)]
struct InternalVmRecord {
    #[serde(flatten)]
    record: VmRecord,
    owner_key: Option<String>,
    api_key_id: Option<String>,
}

impl From<VmRecord> for InternalVmRecord {
    fn from(record: VmRecord) -> Self {
        let owner_key = record.owner_key.clone();
        let api_key_id = record.api_key_id.clone();
        Self {
            record,
            owner_key,
            api_key_id,
        }
    }
}

#[derive(Deserialize)]
pub struct InternalExecBody {
    pub command: String,
    pub timeout_ms: u64,
}

#[derive(Deserialize)]
pub struct InternalSnapshotBody {
    #[serde(default)]
    pub diff: bool,
}

pub fn internal_router(state: AppState) -> Router {
    let traffic_limits = ApiTrafficLimits::new(&state.config);
    let max_body_bytes = state.config.api_max_body_bytes;
    Router::new()
        .route("/internal/v1/vms", post(internal_create))
        .route("/internal/v1/restore", post(internal_restore))
        .route(
            "/internal/v1/vms/{id}",
            get(internal_get).delete(internal_stop),
        )
        .route("/internal/v1/vms/{id}/status", get(internal_status))
        .route("/internal/v1/vms/{id}/exec", post(internal_exec))
        .route("/internal/v1/vms/{id}/pause", post(internal_pause))
        .route("/internal/v1/vms/{id}/suspend", post(internal_suspend))
        .route("/internal/v1/vms/{id}/resume", post(internal_resume))
        .route("/internal/v1/vms/{id}/snapshot", post(internal_snapshot))
        .route("/internal/v1/vms/{id}/egress", patch(internal_egress))
        .route("/internal/v1/shares/{id}", any(internal_share_proxy_root))
        .route(
            "/internal/v1/shares/{id}/{*path}",
            any(internal_share_proxy),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_peer_signature,
        ))
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(axum::middleware::from_fn_with_state(
            traffic_limits,
            enforce_api_traffic,
        ))
        .with_state(state)
}

async fn require_peer_signature(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let share_request = request.uri().path().starts_with("/internal/v1/shares/");
    match verify_peer_request(request, &state).await {
        Some(mut request) => {
            let source = request
                .extensions()
                .get::<VerifiedPeerSource>()
                .map(|source| source.0.clone());
            if let Some(identity) = source.as_deref().and_then(|source| {
                peer_identity_from_headers(request.headers(), &state.config.peer_secret, source)
            }) {
                request.extensions_mut().insert(identity);
            }
            next.run(request).await
        }
        None if share_request => GatewayError::Unavailable.into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn verify_peer_request(
    mut request: Request<Body>,
    state: &AppState,
) -> Option<Request<Body>> {
    let headers = request.headers();
    if headers.contains_key("x-peer-secret")
        || single_header(headers, "X-Tarit-Peer-Version")? != REQUEST_SIGNATURE_VERSION
    {
        return None;
    }
    let source = single_header(headers, "X-Tarit-Peer-Source")
        .filter(|v| !v.is_empty() && v.len() <= MAX_PEER_SOURCE_LEN)?
        .to_string();
    let target = single_header(headers, "X-Tarit-Peer-Target")?.to_string();
    if target != state.config.host_id {
        return None;
    }
    let issued_at = single_header(headers, "X-Tarit-Peer-Timestamp")
        .and_then(|value| value.parse::<i64>().ok())?;
    if Utc::now().timestamp().abs_diff(issued_at) > MAX_PEER_IDENTITY_AGE_SECS {
        return None;
    }
    let nonce = single_header(headers, "X-Tarit-Peer-Nonce")
        .and_then(|value| Uuid::parse_str(value).ok())?;
    let claimed_payload_hash = single_header(headers, "X-Tarit-Peer-Body-SHA256")?.to_string();
    let signature = single_header(headers, "X-Tarit-Peer-Signature")
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())?;

    let is_share = request.uri().path().starts_with("/internal/v1/shares/");
    let actual_payload_hash = if claimed_payload_hash == STREAMING_PAYLOAD {
        if !is_share {
            return None;
        }
        STREAMING_PAYLOAD.to_string()
    } else {
        let (parts, body) = request.into_parts();
        let bytes = to_bytes(body, state.config.api_max_body_bytes).await.ok()?;
        let actual = URL_SAFE_NO_PAD.encode(Sha256::digest(&bytes));
        request = Request::from_parts(parts, Body::from(bytes));
        actual
    };
    if claimed_payload_hash != actual_payload_hash {
        return None;
    }

    let canonical_path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or(request.uri().path());
    let issued_at_string = issued_at.to_string();
    let nonce_string = nonce.to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(state.config.peer_secret.as_bytes()).ok()?;
    for component in [
        REQUEST_SIGNATURE_VERSION,
        request.method().as_str(),
        canonical_path,
        claimed_payload_hash.as_str(),
        issued_at_string.as_str(),
        nonce_string.as_str(),
        source.as_str(),
        target.as_str(),
    ] {
        mac.update(component.as_bytes());
        mac.update(b"\n");
    }
    mac.verify_slice(&signature).ok()?;
    consume_nonce(&USED_PEER_REQUEST_NONCES, &source, nonce)?;
    request.extensions_mut().insert(VerifiedPeerSource(source));
    Some(request)
}

async fn internal_share_proxy_root(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
    request: Request<Body>,
) -> Response {
    internal_share_proxy_impl(state, identity.map(|identity| identity.0), id, request).await
}

async fn internal_share_proxy(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path((id, _path)): Path<(Uuid, String)>,
    request: Request<Body>,
) -> Response {
    internal_share_proxy_impl(state, identity.map(|identity| identity.0), id, request).await
}

async fn internal_share_proxy_impl(
    state: AppState,
    identity: Option<ApiIdentity>,
    id: Uuid,
    request: Request<Body>,
) -> Response {
    let result = async {
        let identity = require_peer_identity(identity.as_ref())?;
        let share = state
            .shares
            .get(id)
            .await?
            .filter(|share| share.revoked_at.is_none())
            .ok_or_else(|| tarit_types::OrchError::NotFound("share not found".into()))?;
        if !identity.is_admin()
            && (identity.tenant != share.owner_key
                || identity.api_key_id != share_peer_identity_id(&share))
        {
            return Err(tarit_types::OrchError::Forbidden(
                "share does not belong to forwarded tenant".into(),
            ));
        }
        if !state.supervisor.is_running(share.vm_id)
            || !matches!(
                resolve_share_owner(&state, share.vm_id).await?,
                Owner::Local
            )
        {
            return Err(tarit_types::OrchError::Internal(
                "share VM is not owned locally".into(),
            ));
        }
        let request = rewrite_share_request_uri(request, id)?;
        share_gateway::proxy_authoritative_local_share(&state, &share, request)
            .await
            .map_err(|_| tarit_types::OrchError::Internal("share proxy unavailable".into()))
    }
    .await;

    match result {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(share_id = %id, %error, "internal share proxy rejected");
            GatewayError::Unavailable.into_response()
        }
    }
}

fn rewrite_share_request_uri(
    request: Request<Body>,
    id: Uuid,
) -> Result<Request<Body>, tarit_types::OrchError> {
    let (mut parts, body) = request.into_parts();
    let prefix = format!("/internal/v1/shares/{id}");
    let path = parts
        .uri
        .path()
        .strip_prefix(&prefix)
        .filter(|path| path.is_empty() || path.starts_with('/'))
        .ok_or_else(|| tarit_types::OrchError::BadRequest("invalid internal share path".into()))?;
    let path = if path.is_empty() { "/" } else { path };
    let target = match parts.uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    };
    parts.uri = target
        .parse::<Uri>()
        .map_err(|_| tarit_types::OrchError::BadRequest("invalid share request URI".into()))?;
    Ok(Request::from_parts(parts, body))
}

fn peer_identity_from_headers(
    headers: &HeaderMap,
    peer_secret: &str,
    source: &str,
) -> Option<ApiIdentity> {
    let tenant = single_header(headers, "X-Tarit-Tenant").filter(|value| !value.is_empty())?;
    let role =
        single_header(headers, "X-Tarit-Role").and_then(|value| value.parse::<ApiRole>().ok())?;
    let api_key_id = single_header(headers, "X-Tarit-Api-Key-Id")?;
    let issued_at = single_header(headers, "X-Tarit-Identity-Timestamp")
        .and_then(|value| value.parse::<i64>().ok())?;
    if Utc::now().timestamp().abs_diff(issued_at) > MAX_PEER_IDENTITY_AGE_SECS {
        return None;
    }
    let nonce = single_header(headers, "X-Tarit-Identity-Nonce")
        .and_then(|value| Uuid::parse_str(value).ok())?;
    let signature = single_header(headers, "X-Tarit-Identity-Signature")
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())?;
    let mut mac = Hmac::<Sha256>::new_from_slice(peer_secret.as_bytes()).ok()?;
    mac.update(IDENTITY_SIGNATURE_VERSION.as_bytes());
    mac.update(b"\n");
    mac.update(source.as_bytes());
    mac.update(b"\n");
    mac.update(issued_at.to_string().as_bytes());
    mac.update(b"\n");
    mac.update(nonce.to_string().as_bytes());
    mac.update(b"\n");
    mac.update(tenant.as_bytes());
    mac.update(b"\n");
    mac.update(role.as_str().as_bytes());
    mac.update(b"\n");
    mac.update(api_key_id.as_bytes());
    mac.verify_slice(&signature).ok()?;
    consume_nonce(&USED_PEER_IDENTITY_NONCES, source, nonce)?;
    Some(ApiIdentity {
        tenant: tenant.to_string(),
        role,
        max_vms: None,
        api_key_id: api_key_id.to_string(),
    })
}

fn consume_nonce(cache: &OnceLock<Mutex<ReplayCache>>, source: &str, nonce: Uuid) -> Option<()> {
    let now = Instant::now();
    let mut cache = cache
        .get_or_init(|| Mutex::new(ReplayCache::default()))
        .lock()
        .ok()?;
    cache.consume_at(
        source,
        nonce,
        now,
        Duration::from_secs(MAX_PEER_IDENTITY_AGE_SECS),
    )
}

impl ReplayCache {
    fn consume_at(&mut self, source: &str, nonce: Uuid, now: Instant, ttl: Duration) -> Option<()> {
        self.consume_at_with_limits(
            source,
            nonce,
            now,
            ttl,
            ReplayLimits {
                per_source: MAX_TRACKED_NONCES_PER_SOURCE,
                total: MAX_TRACKED_NONCES_TOTAL,
                sources: MAX_TRACKED_PEER_SOURCES,
            },
        )
    }

    fn consume_at_with_limits(
        &mut self,
        source: &str,
        nonce: Uuid,
        now: Instant,
        ttl: Duration,
        limits: ReplayLimits,
    ) -> Option<()> {
        while self
            .expirations
            .front()
            .is_some_and(|(expires_at, _, _)| *expires_at <= now)
        {
            let Some((_, expired_source, expired_nonce)) = self.expirations.pop_front() else {
                break;
            };
            if let Some(nonces) = self.sources.get_mut(&expired_source) {
                if nonces.remove(&expired_nonce) {
                    self.tracked = self.tracked.saturating_sub(1);
                }
                if nonces.is_empty() {
                    self.sources.remove(&expired_source);
                }
            }
        }

        if source.is_empty() || source.len() > MAX_PEER_SOURCE_LEN {
            return None;
        }
        if !self.sources.contains_key(source) && self.sources.len() >= limits.sources {
            return None;
        }
        let source_size = self.sources.get(source).map_or(0, HashSet::len);
        if source_size >= limits.per_source || self.tracked >= limits.total {
            return None;
        }
        let nonces = self.sources.entry(source.to_string()).or_default();
        if !nonces.insert(nonce) {
            return None;
        }
        self.tracked += 1;
        self.expirations
            .push_back((now + ttl, source.to_string(), nonce));
        Some(())
    }
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let values = headers.get_all(name);
    (values.iter().count() == 1)
        .then(|| values.iter().next())
        .flatten()
        .and_then(|value| value.to_str().ok())
}

/// Resolve the peer-forwarded caller identity, failing closed if the signed
/// identity headers were absent. Every internal route that acts on a tenant's
/// behalf must know who the caller is; a valid peer request HMAC alone is not
/// enough to skip tenant authorization.
fn require_peer_identity(
    identity: Option<&ApiIdentity>,
) -> Result<&ApiIdentity, tarit_types::OrchError> {
    identity.ok_or(tarit_types::OrchError::Unauthorized)
}

fn enforce_peer_vm_access(
    state: &AppState,
    id: Uuid,
    identity: Option<&ApiIdentity>,
) -> Result<(), tarit_types::OrchError> {
    let identity = require_peer_identity(identity)?;
    let vm = ops::get_local(state, id)?;
    ensure_vm_access(identity, &vm)?;
    Ok(())
}

async fn internal_create(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Json(mut req): Json<CreateVmRequest>,
) -> Result<(StatusCode, Json<InternalVmRecord>), ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|i| &i.0))?;
    // Bind the created VM to the authenticated caller. Admins may create on
    // behalf of another tenant (owner_key carried in the request); everyone
    // else can only create VMs owned by their own tenant.
    if identity.role == ApiRole::Admin {
        if req.owner_key.is_none() {
            req.owner_key = Some(identity.tenant.clone());
            req.api_key_id = Some(identity.api_key_id.clone());
        }
    } else {
        enforce_create_path_policy(identity, &req)?;
        if let Some(owner) = req.owner_key.as_deref() {
            if owner != identity.tenant {
                return Err(tarit_types::OrchError::Forbidden(
                    "cannot create a VM owned by another tenant".into(),
                )
                .into());
            }
        }
        req.owner_key = Some(identity.tenant.clone());
        req.api_key_id = Some(identity.api_key_id.clone());
    }
    let rec = ops::create_local(&state, &req).await?;
    Ok((StatusCode::CREATED, Json(rec.into())))
}

async fn internal_restore(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Json(req): Json<RestoreRequest>,
) -> Result<(StatusCode, Json<InternalVmRecord>), ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|i| &i.0))?;
    let rec = ops::restore_local(
        &state,
        &req.snapshot_path,
        req.id,
        req.owner_key,
        req.api_key_id,
        identity.is_admin(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(rec.into())))
}

async fn internal_exec(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
    Json(body): Json<InternalExecBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    enforce_peer_vm_access(&state, id, identity.as_ref().map(|i| &i.0))?;
    let (code, stdout, stderr, duration_ms) =
        ops::exec_local(&state, id, body.command, body.timeout_ms).await?;
    Ok(Json(serde_json::json!({
        "exit_code": code,
        "stdout": stdout,
        "stderr": stderr,
        "duration_ms": duration_ms,
    })))
}

async fn internal_stop(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    enforce_peer_vm_access(&state, id, identity.as_ref().map(|i| &i.0))?;
    ops::stop_local(&state, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn internal_get(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<Json<InternalVmRecord>, ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|i| &i.0))?;
    let vm = ops::get_local(&state, id)?;
    ensure_vm_access(identity, &vm)?;
    Ok(Json(vm.into()))
}

async fn internal_status(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    enforce_peer_vm_access(&state, id, identity.as_ref().map(|i| &i.0))?;
    Ok(Json(ops::status_local(&state, id).await?))
}

async fn internal_pause(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<Json<InternalVmRecord>, ApiError> {
    enforce_peer_vm_access(&state, id, identity.as_ref().map(|i| &i.0))?;
    Ok(Json(ops::pause_local(&state, id).await?.into()))
}

async fn internal_suspend(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<Json<InternalVmRecord>, ApiError> {
    enforce_peer_vm_access(&state, id, identity.as_ref().map(|i| &i.0))?;
    Ok(Json(ops::suspend_local(&state, id).await?.into()))
}

async fn internal_resume(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<Json<InternalVmRecord>, ApiError> {
    enforce_peer_vm_access(&state, id, identity.as_ref().map(|i| &i.0))?;
    Ok(Json(ops::resume_local(&state, id).await?.into()))
}

async fn internal_snapshot(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
    Json(body): Json<InternalSnapshotBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    enforce_peer_vm_access(&state, id, identity.as_ref().map(|i| &i.0))?;
    let path = ops::snapshot_local(&state, id, body.diff).await?;
    Ok(Json(
        serde_json::json!({ "path": path, "host_id": state.config.host_id }),
    ))
}

async fn internal_egress(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
    Json(body): Json<EgressUpdateRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    enforce_peer_vm_access(&state, id, identity.as_ref().map(|i| &i.0))?;
    let rules = ops::egress_local(&state, id, body.allowlist, body.allow_existing).await?;
    Ok(Json(serde_json::json!({ "rules_applied": rules })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tarit_types::VmStatus;

    fn sample_record() -> VmRecord {
        VmRecord {
            id: Uuid::new_v4(),
            host_id: "node-a".into(),
            owner_key: Some("tenant-a".into()),
            api_key_id: Some("key-1".into()),
            status: VmStatus::Running,
            revision: 1,
            startup_path: Some(tarit_types::VmStartupPath::Cold),
            memory_mib: 256,
            vcpus: 1,
            kernel_path: "/tmp/vmlinux".into(),
            rootfs_path: Some("/tmp/rootfs.ext4".into()),
            cmdline: "console=ttyS0".into(),
            socket_path: Some("/run/taritd/vm.sock".into()),
            pid: Some(42),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn internal_record_transmits_owner_key_to_peers() {
        let record = sample_record();
        let value = serde_json::to_value(InternalVmRecord::from(record.clone())).unwrap();
        assert_eq!(value["owner_key"], serde_json::json!("tenant-a"));
        assert_eq!(value["api_key_id"], serde_json::json!("key-1"));

        let decoded: VmRecord = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.owner_key.as_deref(), Some("tenant-a"));
        assert_eq!(decoded.api_key_id.as_deref(), Some("key-1"));
    }

    #[test]
    fn public_record_still_hides_owner_key() {
        let value = serde_json::to_value(sample_record()).unwrap();
        assert!(value.get("owner_key").is_none());
        assert!(value.get("api_key_id").is_none());
    }

    #[test]
    fn replay_cache_is_bounded_per_source_and_globally() {
        let now = Instant::now();
        let ttl = Duration::from_secs(10);
        let mut cache = ReplayCache::default();
        let a1 = Uuid::new_v4();
        let a2 = Uuid::new_v4();
        let b1 = Uuid::new_v4();
        let limits = ReplayLimits {
            per_source: 2,
            total: 3,
            sources: 2,
        };

        assert!(cache
            .consume_at_with_limits("node-a", a1, now, ttl, limits)
            .is_some());
        assert!(cache
            .consume_at_with_limits("node-a", a2, now, ttl, limits)
            .is_some());
        assert!(cache
            .consume_at_with_limits("node-a", Uuid::new_v4(), now, ttl, limits)
            .is_none());
        assert!(cache
            .consume_at_with_limits("node-b", b1, now, ttl, limits)
            .is_some());
        assert!(cache
            .consume_at_with_limits("node-c", Uuid::new_v4(), now, ttl, limits)
            .is_none());
        assert_eq!(cache.tracked, 3);
    }

    #[test]
    fn replay_cache_rejects_reuse_then_reclaims_expired_capacity() {
        let now = Instant::now();
        let ttl = Duration::from_millis(10);
        let nonce = Uuid::new_v4();
        let mut cache = ReplayCache::default();
        let limits = ReplayLimits {
            per_source: 1,
            total: 1,
            sources: 1,
        };

        assert!(cache
            .consume_at_with_limits("node-a", nonce, now, ttl, limits)
            .is_some());
        assert!(cache
            .consume_at_with_limits("node-a", nonce, now, ttl, limits)
            .is_none());
        assert!(cache
            .consume_at_with_limits("node-b", Uuid::new_v4(), now + ttl, ttl, limits)
            .is_some());
        assert_eq!(cache.tracked, 1);
        assert!(!cache.sources.contains_key("node-a"));
    }
}
