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
    http::{header::CONTENT_LENGTH, HeaderMap, HeaderValue, Request, StatusCode, Uri},
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
    fs::OpenOptions,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tarit_store::HostRecord;
use tarit_types::{
    ArtifactReplicaStatus, ArtifactStatus, CreateVmRequest, EgressPolicyRecord,
    EgressUpdateRequest, PeerSnapshotResponse, PutEgressPolicyRequest, VmRecord,
};
use tokio::io::AsyncReadExt;
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
const REQUEST_SIGNATURE_VERSION: &str = "tarit-peer-request-v2";
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
const MAX_PEER_SOURCE_HEARTBEAT_AGE_SECS: i64 = 15;
const MAX_PEER_SOURCE_FUTURE_SKEW_SECS: i64 = 5;
static USED_PEER_IDENTITY_NONCES: OnceLock<Mutex<ReplayCache>> = OnceLock::new();
static USED_PEER_REQUEST_NONCES: OnceLock<Mutex<ReplayCache>> = OnceLock::new();

/// SHA-256 identity of the leaf certificate authenticated by rustls for this
/// concrete connection. The peer server inserts it after the TLS handshake.
#[derive(Clone, Debug)]
pub(crate) struct VerifiedPeerCertificate(pub String);

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
    #[serde(default)]
    pub fork_child_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct InternalRestoreRequest {
    snapshot_path: String,
    id: Uuid,
    owner_key: String,
    api_key_id: String,
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
        .route(
            "/internal/v1/volumes/{id}",
            axum::routing::delete(internal_delete_volume),
        )
        .route("/internal/v1/vms/{id}/status", get(internal_status))
        .route(
            "/internal/v1/vms/{id}/balloon",
            get(internal_get_balloon).post(internal_set_balloon),
        )
        .route("/internal/v1/vms/{id}/exec", post(internal_exec))
        .route("/internal/v1/vms/{id}/pause", post(internal_pause))
        .route("/internal/v1/vms/{id}/suspend", post(internal_suspend))
        .route("/internal/v1/vms/{id}/hibernate", post(internal_hibernate))
        .route("/internal/v1/vms/{id}/resume", post(internal_resume))
        .route("/internal/v1/vms/{id}/snapshot", post(internal_snapshot))
        .route(
            "/internal/v1/artifacts/{id}",
            get(internal_artifact_descriptor).post(internal_localize_artifact),
        )
        .route(
            "/internal/v1/artifacts/{id}/{component}",
            get(internal_artifact_component),
        )
        .route("/internal/v1/vms/{id}/egress", patch(internal_egress))
        .route(
            "/internal/v1/vms/{id}/egress-policy",
            get(internal_get_egress_policy).put(internal_put_egress_policy),
        )
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

async fn internal_artifact_component(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path((artifact_id, component)): Path<(Uuid, String)>,
) -> Result<Response, ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|identity| &identity.0))?;
    let (_artifact, snapshot) = local_artifact_export(&state, identity, artifact_id)?;
    let path = artifact_component_path(&snapshot, &component)?;
    let (file, length) = if matches!(component.as_str(), "kernel" | "rootfs") {
        open_immutable_boot_component(&path)?
    } else {
        open_private_artifact_component(&path)?
    };
    let file = tokio::fs::File::from_std(file);
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut chunk = vec![0u8; 64 * 1024];
        let read = file.read(&mut chunk).await?;
        if read == 0 {
            Ok::<_, std::io::Error>(None)
        } else {
            chunk.truncate(read);
            Ok::<_, std::io::Error>(Some((chunk, file)))
        }
    });
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).map_err(|_| {
            tarit_types::OrchError::Internal("artifact length header overflow".into())
        })?,
    );
    Ok(response)
}

async fn internal_artifact_descriptor(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(artifact_id): Path<Uuid>,
) -> Result<Json<crate::peer::ArtifactTransferDescriptor>, ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|identity| &identity.0))?;
    let (artifact, snapshot) = local_artifact_export(&state, identity, artifact_id)?;
    Ok(Json(artifact_transfer_descriptor(
        &state, &artifact, &snapshot,
    )?))
}

pub(crate) fn artifact_transfer_descriptor(
    state: &AppState,
    artifact: &tarit_types::ArtifactRecord,
    snapshot: &tarit_store::SnapshotRecord,
) -> Result<crate::peer::ArtifactTransferDescriptor, ApiError> {
    let (_, ram_bytes) = open_private_artifact_component(&PathBuf::from(&snapshot.path))?;
    let overlay_bytes = match snapshot.overlay_path.as_deref() {
        Some(path) => open_private_artifact_component(&PathBuf::from(path))?.1,
        None => 0,
    };
    let (_, integrity_bytes) =
        open_private_artifact_component(&PathBuf::from(format!("{}.integrity", snapshot.path)))?;
    let memory_mib = snapshot.memory_mib.ok_or_else(|| {
        tarit_types::OrchError::Unprocessable("artifact is missing memory metadata".into())
    })?;
    let vcpus = snapshot.vcpus.ok_or_else(|| {
        tarit_types::OrchError::Unprocessable("artifact is missing vCPU metadata".into())
    })?;
    let cmdline = snapshot.cmdline.clone().ok_or_else(|| {
        tarit_types::OrchError::Unprocessable("artifact is missing command-line metadata".into())
    })?;
    let kernel_path = snapshot.kernel_path.as_deref().ok_or_else(|| {
        tarit_types::OrchError::Unprocessable("artifact is missing kernel metadata".into())
    })?;
    let rootfs_path = snapshot.rootfs_path.as_deref().ok_or_else(|| {
        tarit_types::OrchError::Unprocessable("artifact is missing rootfs metadata".into())
    })?;
    let (_, kernel_bytes) = open_immutable_boot_component(&PathBuf::from(kernel_path))?;
    let (_, rootfs_bytes) = open_immutable_boot_component(&PathBuf::from(rootfs_path))?;
    let kernel_digest = crate::image::sha256_regular_file(std::path::Path::new(kernel_path))
        .map_err(|error| {
            tarit_types::OrchError::Unavailable(format!(
                "artifact kernel failed immutable verification: {error}"
            ))
        })?;
    let image = state
        .store
        .lock()
        .map_err(|_| tarit_types::OrchError::Internal("store lock poisoned".into()))?
        .get_image_by_source_digest(&artifact.immutable_image_digest)
        .map_err(crate::api::store_err)?
        .filter(|image| image.rootfs_path == rootfs_path)
        .ok_or_else(|| {
            tarit_types::OrchError::Unavailable(
                "artifact immutable image admission record is missing".into(),
            )
        })?;
    crate::image::verify_admitted_image(&image, &state.config.image_admission_policy).map_err(
        |error| {
            tarit_types::OrchError::Unavailable(format!(
                "artifact immutable image failed admission: {error}"
            ))
        },
    )?;
    let rootfs_digest = image.rootfs_digest.clone().ok_or_else(|| {
        tarit_types::OrchError::Unavailable("artifact image rootfs digest is missing".into())
    })?;
    if image.size_bytes != rootfs_bytes
        || image.agent_digest.as_deref() != Some(artifact.agent_digest.as_str())
    {
        return Err(tarit_types::OrchError::Unavailable(
            "artifact image lineage does not match immutable metadata".into(),
        )
        .into());
    }
    let source_vm_id = artifact.source_vm_id.ok_or_else(|| {
        tarit_types::OrchError::Unprocessable("artifact is missing source VM lineage".into())
    })?;
    let boot_metadata = tarit_types::ArtifactBootMetadata {
        version: tarit_types::ArtifactBootMetadata::VERSION,
        kernel_digest: kernel_digest.clone(),
        immutable_image_digest: artifact.immutable_image_digest.clone(),
        rootfs_digest: rootfs_digest.clone(),
        agent_digest: artifact.agent_digest.clone(),
        memory_mib,
        vcpus,
        cmdline: cmdline.clone(),
        rootfs_read_only: snapshot.rootfs_read_only.unwrap_or(true),
    };
    if boot_metadata.digest().map_err(|error| {
        tarit_types::OrchError::Internal(format!("encode artifact boot metadata: {error}"))
    })? != artifact.boot_manifest_digest
    {
        return Err(tarit_types::OrchError::Unavailable(
            "artifact boot metadata failed immutable verification".into(),
        )
        .into());
    }
    if ram_bytes.checked_add(overlay_bytes) != Some(artifact.size_bytes) {
        return Err(tarit_types::OrchError::Unavailable(
            "artifact component sizes do not match immutable metadata".into(),
        )
        .into());
    }
    Ok(crate::peer::ArtifactTransferDescriptor {
        artifact_id: artifact.artifact_id,
        content_digest: artifact.content_digest.clone(),
        size_bytes: artifact.size_bytes,
        immutable_image_digest: artifact.immutable_image_digest.clone(),
        agent_digest: artifact.agent_digest.clone(),
        boot_manifest_digest: artifact.boot_manifest_digest.clone(),
        kernel_digest,
        kernel_bytes,
        rootfs_digest,
        rootfs_bytes,
        image_source_ref: image.source_ref,
        provenance_key_digest: image.provenance_key_digest,
        provenance_verified_at: image.provenance_verified_at,
        creation_revision: artifact.creation_revision,
        integrity_manifest_digest: artifact.integrity_manifest_digest.clone(),
        chunk_size_bytes: artifact.chunk_size_bytes,
        chunk_count: artifact.chunk_count,
        source_vm_id,
        memory_mib,
        vcpus,
        cmdline,
        rootfs_read_only: snapshot.rootfs_read_only.unwrap_or(true),
        has_overlay: snapshot.overlay_path.is_some(),
        ram_bytes,
        overlay_bytes,
        integrity_bytes,
    })
}

async fn internal_localize_artifact(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(artifact_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|identity| &identity.0))?;
    let fleet = state
        .fleet
        .as_ref()
        .ok_or_else(|| tarit_types::OrchError::Unavailable("fleet storage is disabled".into()))?;
    let artifact = fleet
        .get_artifact(&identity.tenant, artifact_id)
        .await
        .map_err(|error| match error {
            tarit_fleet::FleetError::NotFound => {
                tarit_types::OrchError::NotFound("artifact not found".into())
            }
            error => tarit_types::OrchError::Internal(format!("fleet artifact: {error}")),
        })?;
    ops::localize_branch_artifact(&state, &artifact, identity, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn local_artifact_export(
    state: &AppState,
    identity: &ApiIdentity,
    artifact_id: Uuid,
) -> Result<(tarit_types::ArtifactRecord, tarit_store::SnapshotRecord), ApiError> {
    let (artifact, snapshot, replicas) = {
        let store = state
            .store
            .lock()
            .map_err(|_| tarit_types::OrchError::Internal("store lock poisoned".into()))?;
        let artifact = store
            .get_artifact(&identity.tenant, artifact_id)
            .map_err(crate::api::store_err)?;
        let snapshot = store
            .get_snapshot_by_id(artifact_id)
            .map_err(crate::api::store_err)?
            .ok_or_else(|| tarit_types::OrchError::NotFound("artifact not found".into()))?;
        let replicas = store
            .list_artifact_replicas(&identity.tenant, artifact_id)
            .map_err(crate::api::store_err)?;
        (artifact, snapshot, replicas)
    };
    let local_replica_ready = replicas.iter().any(|replica| {
        replica.host_id == state.config.host_id
            && replica.storage_locator == artifact.storage_locator
            && replica.status == ArtifactReplicaStatus::Available
            && replica.verified_at.is_some()
    });
    if artifact.status != ArtifactStatus::Available
        || artifact.host_id != state.config.host_id
        || artifact.storage_locator != snapshot.path
        || snapshot.owner_key.as_deref() != Some(identity.tenant.as_str())
        || !local_replica_ready
    {
        return Err(tarit_types::OrchError::NotFound("artifact not found".into()).into());
    }
    Ok((artifact, snapshot))
}

fn artifact_component_path(
    snapshot: &tarit_store::SnapshotRecord,
    component: &str,
) -> Result<PathBuf, ApiError> {
    let path = match component {
        "ram" => PathBuf::from(&snapshot.path),
        "overlay" => snapshot
            .overlay_path
            .as_deref()
            .map(PathBuf::from)
            .ok_or_else(|| {
                tarit_types::OrchError::NotFound("artifact component not found".into())
            })?,
        "integrity" => PathBuf::from(format!("{}.integrity", snapshot.path)),
        "kernel" => snapshot
            .kernel_path
            .as_deref()
            .map(PathBuf::from)
            .ok_or_else(|| {
                tarit_types::OrchError::NotFound("artifact component not found".into())
            })?,
        "rootfs" => snapshot
            .rootfs_path
            .as_deref()
            .map(PathBuf::from)
            .ok_or_else(|| {
                tarit_types::OrchError::NotFound("artifact component not found".into())
            })?,
        _ => {
            return Err(
                tarit_types::OrchError::NotFound("artifact component not found".into()).into(),
            )
        }
    };
    Ok(path)
}

fn open_private_artifact_component(path: &PathBuf) -> Result<(std::fs::File, u64), ApiError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| tarit_types::OrchError::NotFound("artifact component not found".into()))?;
    let metadata = file
        .metadata()
        .map_err(|error| tarit_types::OrchError::Internal(format!("stat artifact: {error}")))?;
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(tarit_types::OrchError::Unavailable(
            "artifact component failed private-file validation".into(),
        )
        .into());
    }
    let length = metadata.len();
    Ok((file, length))
}

fn open_immutable_boot_component(path: &PathBuf) -> Result<(std::fs::File, u64), ApiError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| tarit_types::OrchError::NotFound("artifact component not found".into()))?;
    let metadata = file
        .metadata()
        .map_err(|error| tarit_types::OrchError::Internal(format!("stat boot input: {error}")))?;
    if !metadata.file_type().is_file() {
        return Err(tarit_types::OrchError::Unavailable(
            "artifact boot input is not a regular file".into(),
        )
        .into());
    }
    Ok((file, metadata.len()))
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
    let source_session = single_header(headers, "X-Tarit-Peer-Source-Session")
        .and_then(|value| Uuid::parse_str(value).ok())?;
    let target_session = single_header(headers, "X-Tarit-Peer-Target-Session")
        .and_then(|value| Uuid::parse_str(value).ok())?;
    if target_session != state.config.host_session_id {
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
    // The request signature binds the forwarded identity envelope (empty when
    // absent) so a signed request cannot be spliced together with an identity
    // captured from a different request by the same source host.
    let identity_binding = if headers
        .get_all("X-Tarit-Identity-Signature")
        .iter()
        .next()
        .is_none()
    {
        String::new()
    } else {
        single_header(headers, "X-Tarit-Identity-Signature")?.to_string()
    };

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
    let source_session_string = source_session.to_string();
    let target_session_string = target_session.to_string();
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
        source_session_string.as_str(),
        target_session_string.as_str(),
        identity_binding.as_str(),
    ] {
        mac.update(component.as_bytes());
        mac.update(b"\n");
    }
    mac.verify_slice(&signature).ok()?;
    if let Some(fleet) = state.fleet.as_ref() {
        let current_source = fleet.get_host(&source).await.ok().flatten()?;
        let connection_certificate = request.extensions().get::<VerifiedPeerCertificate>()?;
        if !current_peer_source(
            &current_source,
            &source,
            source_session,
            &connection_certificate.0,
            Utc::now(),
        ) {
            return None;
        }
    }
    consume_nonce(&USED_PEER_REQUEST_NONCES, &source, nonce)?;
    request.extensions_mut().insert(VerifiedPeerSource(source));
    Some(request)
}

fn current_peer_source(
    host: &HostRecord,
    source: &str,
    session: Uuid,
    peer_certificate_sha256: &str,
    now: chrono::DateTime<Utc>,
) -> bool {
    let age = now - host.last_heartbeat;
    host.host_id == source
        && host.boot_session_id == Some(session)
        && host.peer_certificate_sha256.as_deref() == Some(peer_certificate_sha256)
        && host.healthy
        && age <= chrono::Duration::seconds(MAX_PEER_SOURCE_HEARTBEAT_AGE_SECS)
        && age >= -chrono::Duration::seconds(MAX_PEER_SOURCE_FUTURE_SKEW_SECS)
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

async fn internal_delete_volume(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|value| &value.0))?;
    crate::api::delete_volume_local(&state, identity, id).await
}

async fn internal_get_balloon(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<Json<crate::api::PublicBalloonState>, ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|value| &value.0))?;
    enforce_peer_vm_access(&state, id, Some(identity))?;
    Ok(Json(crate::api::public_balloon(
        ops::balloon_local(&state, id).await?,
    )))
}

async fn internal_set_balloon(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
    Json(request): Json<crate::api::BalloonTargetRequest>,
) -> Result<Json<crate::api::PublicBalloonState>, ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|value| &value.0))?;
    enforce_peer_vm_access(&state, id, Some(identity))?;
    Ok(Json(crate::api::public_balloon(
        ops::set_balloon_local(&state, id, request.target_mib).await?,
    )))
}

async fn internal_restore(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Json(req): Json<InternalRestoreRequest>,
) -> Result<(StatusCode, Json<InternalVmRecord>), ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|i| &i.0))?;
    let rec = ops::restore_local(
        &state,
        &req.snapshot_path,
        Some(req.id),
        Some(req.owner_key),
        Some(req.api_key_id),
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

async fn internal_hibernate(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<Json<InternalVmRecord>, ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|identity| &identity.0))?;
    enforce_peer_vm_access(&state, id, Some(identity))?;
    Ok(Json(
        ops::hibernate_local(&state, id, identity).await?.into(),
    ))
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
) -> Result<Json<PeerSnapshotResponse>, ApiError> {
    enforce_peer_vm_access(&state, id, identity.as_ref().map(|i| &i.0))?;
    let (path, live_snapshot) = if let Some(child_id) = body.fork_child_id {
        if body.diff {
            return Err(tarit_types::OrchError::Unprocessable(
                "fork snapshots must be full snapshots".into(),
            )
            .into());
        }
        let snapshot = ops::snapshot_local_for_fork(&state, id, child_id).await?;
        (
            snapshot.path,
            snapshot
                .live_stats
                .map(crate::api::public_fork_snapshot_metrics),
        )
    } else {
        (ops::snapshot_local(&state, id, body.diff).await?, None)
    };
    let snapshot = state
        .store
        .lock()
        .map_err(|_| tarit_types::OrchError::Internal("store lock poisoned".into()))?
        .get_snapshot(&path)
        .map_err(crate::api::store_err)?
        .ok_or_else(|| tarit_types::OrchError::Internal("snapshot publication missing".into()))?;
    Ok(Json(PeerSnapshotResponse {
        snapshot_id: snapshot.snapshot_id,
        live_snapshot,
    }))
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

async fn internal_get_egress_policy(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<Json<EgressPolicyRecord>, ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|i| &i.0))?;
    let vm = ops::get_local(&state, id)?;
    ensure_vm_access(identity, &vm)?;
    let owner = vm.owner_key.ok_or_else(|| {
        tarit_types::OrchError::BadRequest("admin-owned VM has no durable tenant policy".into())
    })?;
    Ok(Json(ops::get_egress_policy_local(&state, id, &owner)?))
}

async fn internal_put_egress_policy(
    State(state): State<AppState>,
    identity: Option<Extension<ApiIdentity>>,
    Path(id): Path<Uuid>,
    Json(body): Json<PutEgressPolicyRequest>,
) -> Result<Json<EgressPolicyRecord>, ApiError> {
    let identity = require_peer_identity(identity.as_ref().map(|i| &i.0))?;
    let vm = ops::get_local(&state, id)?;
    ensure_vm_access(identity, &vm)?;
    let owner = vm.owner_key.ok_or_else(|| {
        tarit_types::OrchError::BadRequest("admin-owned VM has no durable tenant policy".into())
    })?;
    Ok(Json(
        ops::put_egress_policy_local(
            &state,
            id,
            &owner,
            body.expected_revision,
            body.allowlist,
            body.allow_existing,
        )
        .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tarit_types::VmStatus;

    #[test]
    fn artifact_export_opens_only_private_regular_nonsymlink_files() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("tarit-artifact-export-{}", Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("artifact.ram");
        std::fs::write(&file, b"verified bytes").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let Ok((_, size)) = open_private_artifact_component(&file) else {
            panic!("private regular artifact must open");
        };
        assert_eq!(size, 14);

        let link = dir.join("artifact-link.ram");
        symlink(&file, &link).unwrap();
        assert!(open_private_artifact_component(&link).is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_private_artifact_component(&file).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

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
            rootfs_read_only: true,
            cmdline: "console=ttyS0".into(),
            runtime_layout: None,
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
    fn peer_source_session_must_match_the_current_healthy_heartbeat() {
        let now = Utc::now();
        let session = Uuid::new_v4();
        let mut host = HostRecord {
            host_id: "node-a".into(),
            boot_session_id: Some(session),
            peer_certificate_sha256: Some("certificate-a".into()),
            rpc_addr: Some("https://node-a.internal:8443".into()),
            sandbox_count: 0,
            free_vcpus: 4,
            free_memory_mib: 4096,
            healthy: true,
            last_heartbeat: now,
        };
        assert!(current_peer_source(
            &host,
            "node-a",
            session,
            "certificate-a",
            now
        ));
        assert!(!current_peer_source(
            &host,
            "node-a",
            Uuid::new_v4(),
            "certificate-a",
            now
        ));
        assert!(!current_peer_source(
            &host,
            "node-a",
            session,
            "certificate-b",
            now
        ));
        host.healthy = false;
        assert!(!current_peer_source(
            &host,
            "node-a",
            session,
            "certificate-a",
            now
        ));
        host.healthy = true;
        host.last_heartbeat = now - chrono::Duration::seconds(16);
        assert!(!current_peer_source(
            &host,
            "node-a",
            session,
            "certificate-a",
            now
        ));
        host.last_heartbeat = now + chrono::Duration::seconds(6);
        assert!(!current_peer_source(
            &host,
            "node-a",
            session,
            "certificate-a",
            now
        ));
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
