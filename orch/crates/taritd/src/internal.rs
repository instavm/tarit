//! Internal peer-facing routes (authenticated with `X-Peer-Secret`).
//!
//! These are the "execute on THIS node" endpoints that a public handler on
//! another node forwards to when it does not own the target VM (or is placing a
//! new VM here). They call the same node-local `ops` as the public API, so
//! behavior is identical whether a request arrives from a client or a peer.

use axum::{
    body::Body,
    extract::{Extension, Path, State},
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
    collections::{HashSet, VecDeque},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tarit_types::{CreateVmRequest, EgressUpdateRequest, RestoreRequest, VmRecord};
use uuid::Uuid;

use crate::{
    api::{enforce_create_path_policy, ensure_vm_access, ApiError, AppState},
    cluster::Owner,
    share_gateway::{self, resolve_share_owner, share_peer_identity_id, GatewayError},
};

const IDENTITY_SIGNATURE_VERSION: &str = "tarit-peer-identity-v1";
const MAX_PEER_IDENTITY_AGE_SECS: u64 = 60;
const MAX_TRACKED_PEER_IDENTITY_NONCES: usize = 65_536;
static USED_PEER_IDENTITY_NONCES: OnceLock<Mutex<ReplayCache>> = OnceLock::new();

#[derive(Default)]
struct ReplayCache {
    nonces: HashSet<Uuid>,
    expirations: VecDeque<(Instant, Uuid)>,
}
use crate::config::{ApiIdentity, ApiRole};
use crate::ops;

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
            require_peer_secret,
        ))
        .with_state(state)
}

async fn require_peer_secret(
    State(state): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let ok = request
        .headers()
        .get("X-Peer-Secret")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| peer_secret_matches(s, &state.config.peer_secret));
    if ok {
        if let Some(identity) =
            peer_identity_from_headers(request.headers(), &state.config.peer_secret)
        {
            request.extensions_mut().insert(identity);
        }
        next.run(request).await
    } else {
        if request.uri().path().starts_with("/internal/v1/shares/") {
            GatewayError::Unavailable.into_response()
        } else {
            StatusCode::UNAUTHORIZED.into_response()
        }
    }
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

/// Constant-time comparison of the presented peer secret against the configured
/// one. Both sides are hashed to a fixed 32-byte digest first, so neither the
/// secret length nor an early byte mismatch is observable through timing.
fn peer_secret_matches(provided: &str, expected: &str) -> bool {
    let a = Sha256::digest(provided.as_bytes());
    let b = Sha256::digest(expected.as_bytes());
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn peer_identity_from_headers(headers: &HeaderMap, peer_secret: &str) -> Option<ApiIdentity> {
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
    consume_peer_identity_nonce(nonce)?;
    Some(ApiIdentity {
        tenant: tenant.to_string(),
        role,
        max_vms: None,
        api_key_id: api_key_id.to_string(),
    })
}

fn consume_peer_identity_nonce(nonce: Uuid) -> Option<()> {
    let now = Instant::now();
    let expires_at = now + Duration::from_secs(MAX_PEER_IDENTITY_AGE_SECS);
    let mut cache = USED_PEER_IDENTITY_NONCES
        .get_or_init(|| Mutex::new(ReplayCache::default()))
        .lock()
        .ok()?;
    while cache
        .expirations
        .front()
        .is_some_and(|(expires_at, _)| *expires_at <= now)
    {
        if let Some((_, expired_nonce)) = cache.expirations.pop_front() {
            cache.nonces.remove(&expired_nonce);
        }
    }
    if cache.nonces.len() >= MAX_TRACKED_PEER_IDENTITY_NONCES || !cache.nonces.insert(nonce) {
        return None;
    }
    cache.expirations.push_back((expires_at, nonce));
    Some(())
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
/// behalf must know who the caller is; a valid `X-Peer-Secret` alone is not
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
) -> Result<(StatusCode, Json<VmRecord>), ApiError> {
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
    Ok((StatusCode::CREATED, Json(rec)))
}

async fn internal_restore(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Json(req): Json<RestoreRequest>,
) -> Result<(StatusCode, Json<VmRecord>), ApiError> {
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
    Ok((StatusCode::CREATED, Json(rec)))
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
) -> Result<Json<VmRecord>, ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|i| &i.0))?;
    let vm = ops::get_local(&state, id)?;
    ensure_vm_access(identity, &vm)?;
    Ok(Json(vm))
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
) -> Result<Json<VmRecord>, ApiError> {
    enforce_peer_vm_access(&state, id, identity.as_ref().map(|i| &i.0))?;
    Ok(Json(ops::pause_local(&state, id).await?))
}

async fn internal_resume(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<Json<VmRecord>, ApiError> {
    enforce_peer_vm_access(&state, id, identity.as_ref().map(|i| &i.0))?;
    Ok(Json(ops::resume_local(&state, id).await?))
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
