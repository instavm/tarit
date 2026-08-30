mod auth {
    use axum::{
        body::Body,
        extract::State,
        http::{HeaderMap, Request, StatusCode},
        middleware::Next,
        response::{IntoResponse, Response},
        Json,
    };
    use tarit_types::ErrorBody;

    use super::AppState;
    use crate::config::{ApiIdentity, Config};

    pub async fn require_api_key(
        State(state): State<AppState>,
        mut request: Request<Body>,
        next: Next,
    ) -> Response {
        match resolve_identity_from_headers(&state.config, request.headers()) {
            Ok(identity) => {
                request.extensions_mut().insert(identity);
                next.run(request).await
            }
            Err(_) => unauthorized_response(),
        }
    }

    pub(crate) fn resolve_identity_from_headers(
        config: &Config,
        headers: &HeaderMap,
    ) -> Result<ApiIdentity, tarit_types::OrchError> {
        headers
            .get("X-API-Key")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .and_then(|key| config.api_keys.resolve(key))
            .ok_or(tarit_types::OrchError::Unauthorized)
    }

    pub(crate) fn unauthorized_response() -> Response {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody {
                error: "unauthorized".into(),
            }),
        )
            .into_response()
    }
}

use auth::require_api_key;
use axum::{
    body::to_bytes,
    extract::{DefaultBodyLimit, Extension, Path, Query, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use chrono::Utc;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use tarit_store::{Store, VolumeTransition};
use tarit_types::{
    AuditEvent, BranchRecord, CreateBranchRequest, CreateShareRequest, CreateVmRequest,
    CreateVolumeRequest, EgressPolicyRecord, EgressUpdateRequest, ErrorBody, ExecuteRequest,
    ExecutionRecord, ExecutionStatus, ForkVmRequest, ForkVmResponse, OrchError, PublicVmRecord,
    PublicVolumeRecord, PutEgressPolicyRequest, RestoreBranchRequest, ShareRecord,
    ShareTokenResponse, ShareVisibility, SnapshotRequest, SnapshotResponse,
    UpdateBranchHeadRequest, UpdateShareRequest, UsageEvent, UsageSummary, VmRecord, VmStatus,
    VolumeCapabilities, VolumeRecord, VolumeStatus, VolumeStorageClass,
};
use tarit_volume::{VolumeError, MIN_BLOCK_VOLUME_BYTES};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::cluster::Owner;
use crate::config::{ApiIdentity, ApiRole, Config};
use crate::openapi;
use crate::peer::PeerClient;
use crate::scheduler::Scheduler;
use crate::shares::ShareRepository;
use crate::supervisor::{VmSpawnConfig, VmmSupervisor};
use crate::{audit, cluster, ops, usage};
use std::time::{Duration, Instant};
use tarit_types::RestoreRequest;
use tarit_types::{audit_action, audit_outcome};

/// A durability write applied by the bounded background store writer. VM
/// lifecycle transitions use the acknowledged variant because reservations and
/// fleet ownership must not advance ahead of SQLite durability. Execution and
/// outbox writes retain their bounded write-behind behavior.
pub enum StoreWrite {
    /// A lifecycle transition that must reach SQLite before its resource
    /// reservation or fleet ownership may be released.
    VmDurable(
        VmRecord,
        tokio::sync::oneshot::Sender<Result<(), OrchError>>,
    ),
    Exec(ExecutionRecord),
    Usage(UsageEvent),
    Audit(AuditEvent),
}

/// The only mutable lifecycle coordination record for a user VM. A record stays
/// here until every durable/externally-visible step has acknowledged; this makes
/// retry ownership explicit instead of inferring it from cache and supervisor
/// side effects.
#[derive(Clone, Debug)]
pub(crate) enum LifecycleState {
    Creating {
        record: VmRecord,
        phase: CreatingPhase,
    },
    Publishing {
        record: VmRecord,
        phase: PublicationPhase,
    },
    Running {
        record: VmRecord,
    },
    /// A live VMM transition could not be rolled back or observed. The record
    /// stays registered so the lifecycle sweeper retries VMM observation and
    /// fences the observed state through fleet, SQLite and cache.
    Reconciling {
        record: VmRecord,
    },
    /// A legacy partial warm-registration rollback retained resources. Resources
    /// stay registered until DELETE/stop-all performs the normal terminal
    /// transition; request futures never own asynchronous lifecycle cleanup.
    Abandoned {
        record: VmRecord,
    },
    Terminal {
        record: VmRecord,
        phase: TerminalPhase,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CreatingPhase {
    CacheVisible,
    SQLitePersisted,
    FleetClaimed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationPhase {
    NeedFleetUpdate,
    FleetUpdated,
    SQLitePersisted,
    CacheVisible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalPhase {
    PersistRecordAndRelease,
    PersistRecordOnly,
    ClearFleetOwnershipAndRelease,
    ClearFleetOwnershipOnly,
    CommitCacheAndRelease,
    CommitCacheOnly,
    ReleaseReservation,
    Complete,
}

impl LifecycleState {
    pub(crate) fn record(&self) -> &VmRecord {
        match self {
            Self::Creating { record, .. }
            | Self::Publishing { record, .. }
            | Self::Running { record }
            | Self::Reconciling { record }
            | Self::Abandoned { record }
            | Self::Terminal { record, .. } => record,
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LifecycleFault {
    SQLite,
    FleetClaim,
    FleetClear,
    CacheCommit,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum LifecyclePause {
    Fleet,
    SQLite,
    Cache,
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct LifecyclePauseControl {
    pub(crate) entered: Arc<tokio::sync::Notify>,
    pub(crate) release: Arc<tokio::sync::Notify>,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub store: Arc<Mutex<Store>>,
    /// In-memory execution status, updated write-through with the store. Lets the
    /// client's 15ms status polling scale to a 200-wide burst without serializing
    /// every poll on the single SQLite connection mutex.
    pub exec_cache: Arc<RwLock<HashMap<Uuid, ExecutionRecord>>>,
    /// In-memory VM records used for fast reads after lifecycle publication.
    /// Lifecycle writes reach SQLite through an acknowledged store operation
    /// before this cache becomes externally visible.
    pub vm_cache: Arc<RwLock<HashMap<Uuid, VmRecord>>>,
    /// Channel to the background store writer (durability, write-behind).
    pub store_tx: tokio::sync::mpsc::Sender<StoreWrite>,
    /// Registered user lifecycle state. The supervisor boot gate establishes
    /// Creating records before VMM work; this map then owns publication and
    /// terminal retry progress until reservations can be released.
    pub(crate) lifecycle: Arc<Mutex<HashMap<Uuid, LifecycleState>>>,
    /// One async mutex per logical VM serializes hibernation activation. All
    /// ingress paths share this gate so an HTTP/PTY/SSH burst starts exactly
    /// one replacement VMM and the remaining callers await its publication.
    pub(crate) activation_gates: Arc<Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>>,
    #[cfg(test)]
    pub(crate) lifecycle_faults: Arc<Mutex<Vec<LifecycleFault>>>,
    #[cfg(test)]
    pub(crate) lifecycle_pauses: Arc<Mutex<HashMap<LifecyclePause, LifecyclePauseControl>>>,
    /// Serializes terminal transition retries so a second stop cannot repeat
    /// destructive teardown while the first stop awaits durable persistence.
    /// When both are needed, this gate is acquired before the supervisor boot
    /// gate; boot publication never acquires this gate.
    pub(crate) terminal_transition_gate: Arc<tokio::sync::Mutex<()>>,
    /// Durable audit outbox used by lifecycle operations that cannot rely on
    /// the best-effort background writer.
    pub(crate) audit_outbox: Arc<dyn audit::DurableAuditOutbox>,
    pub(crate) pty_registry: Arc<crate::pty::PtyRegistry>,
    pub supervisor: Arc<VmmSupervisor>,
    pub scheduler: Arc<Scheduler>,
    pub peer: Arc<PeerClient>,
    pub shares: ShareRepository,
    /// Global fleet registry (Postgres). `None` in single-host mode; when set,
    /// enables cross-node placement, VM->owner routing, and membership.
    pub fleet: Option<Arc<tarit_fleet::PostgresFleet>>,
    pub metrics: Arc<crate::metrics::Metrics>,
    pub(crate) share_runtime: Arc<crate::share_gateway::ShareRuntime>,
}

#[derive(Clone)]
pub(crate) struct ApiTrafficLimits {
    concurrency: Arc<tokio::sync::Semaphore>,
    rate: Arc<Mutex<TokenBucket>>,
    requests_per_second: u64,
    timeout: Duration,
}

struct TokenBucket {
    tokens: f64,
    updated_at: Instant,
}

impl ApiTrafficLimits {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            concurrency: Arc::new(tokio::sync::Semaphore::new(config.api_max_in_flight)),
            rate: Arc::new(Mutex::new(TokenBucket {
                tokens: config.api_requests_per_second as f64,
                updated_at: Instant::now(),
            })),
            requests_per_second: config.api_requests_per_second,
            timeout: Duration::from_millis(config.api_request_timeout_ms),
        }
    }

    fn take_rate_token(&self) -> bool {
        let Ok(mut bucket) = self.rate.lock() else {
            return false;
        };
        let now = Instant::now();
        let replenished = bucket.tokens
            + now.duration_since(bucket.updated_at).as_secs_f64() * self.requests_per_second as f64;
        bucket.tokens = replenished.min(self.requests_per_second as f64);
        bucket.updated_at = now;
        if bucket.tokens < 1.0 {
            false
        } else {
            bucket.tokens -= 1.0;
            true
        }
    }
}

pub(crate) async fn enforce_api_traffic(
    State(limits): State<ApiTrafficLimits>,
    request: Request,
    next: middleware::Next,
) -> Response {
    if !limits.take_rate_token() {
        return overloaded_response("API rate limit exceeded");
    }
    let Ok(permit) = Arc::clone(&limits.concurrency).try_acquire_owned() else {
        return overloaded_response("API concurrency limit exceeded");
    };
    // Own the handler in a spawned task so the deadline cannot cancel a
    // mutating operation mid-flight: `spawn_blocking` VMM transitions would
    // otherwise keep running while their persistence and rollback logic is
    // dropped. On timeout the task runs to completion (holding its
    // concurrency permit) and only the response is abandoned.
    let mut handler = tokio::spawn(async move {
        let _permit = permit;
        next.run(request).await
    });
    match tokio::time::timeout(limits.timeout, &mut handler).await {
        Ok(Ok(response)) => response,
        Ok(Err(join_error)) => {
            tracing::error!(%join_error, "API handler task failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    error: "API handler failed".into(),
                }),
            )
                .into_response()
        }
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(ErrorBody {
                error: "API request deadline exceeded".into(),
            }),
        )
            .into_response(),
    }
}

fn overloaded_response(message: &str) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(ErrorBody {
            error: message.to_string(),
        }),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

pub struct ApiError(pub OrchError);

impl From<OrchError> for ApiError {
    fn from(value: OrchError) -> Self {
        Self(value)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let err = self.0;
        let error = match &err {
            OrchError::Overloaded { message, .. } if message == "taritd is shutting down" => {
                message.clone()
            }
            OrchError::Unavailable(_) => "service unavailable".into(),
            OrchError::Internal(_) => "internal error".into(),
            OrchError::Vmm(_) => "VM operation failed".into(),
            _ => err.to_string(),
        };
        // The response body hides internal detail; keep the full cause
        // observable server-side or failures become undiagnosable.
        if matches!(
            &err,
            OrchError::Unavailable(_) | OrchError::Internal(_) | OrchError::Vmm(_)
        ) {
            tracing::error!(error = %err, "request failed");
        }
        let status =
            StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, Json(ErrorBody { error })).into_response();
        if let Some(retry_after_secs) = err.retry_after_secs() {
            if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct PublicVmRuntimeStatus {
    state: String,
    uptime_ms: u64,
    vcpus: u8,
    mem_mib: u64,
    vcpu_alive: bool,
}

fn public_vm_runtime_status(status: serde_json::Value) -> Result<PublicVmRuntimeStatus, OrchError> {
    serde_json::from_value(status)
        .map_err(|_| OrchError::Internal("invalid VMM status response".into()))
}

fn public_operation_error(error: &OrchError) -> String {
    match error {
        OrchError::Unavailable(_) => "service unavailable".into(),
        OrchError::Internal(_) => "internal error".into(),
        OrchError::Vmm(_) => "VM operation failed".into(),
        _ => error.to_string(),
    }
}

enum ShareApiError {
    InvalidRequest,
    BadRequest,
    NotFound,
    Conflict,
    Forbidden,
    OwnerUnavailable,
    ServiceUnavailable,
    AuditUnavailable,
}

impl ShareApiError {
    fn from_service(error: OrchError) -> Self {
        match error {
            OrchError::BadRequest(error) | OrchError::Unprocessable(error) => {
                tracing::debug!(%error, "share request rejected");
                Self::BadRequest
            }
            OrchError::NotFound(error) => {
                tracing::debug!(%error, "share resource not found");
                Self::NotFound
            }
            OrchError::Conflict(error) => {
                tracing::debug!(%error, "share request conflicted");
                Self::Conflict
            }
            // The public API key was already authenticated by middleware. A
            // share-service 401 can only be an internal peer authentication
            // failure, which must not be disclosed as a caller credential error.
            OrchError::Unauthorized => Self::OwnerUnavailable,
            OrchError::Forbidden(error) => {
                tracing::debug!(%error, "share request forbidden");
                Self::Forbidden
            }
            error @ (OrchError::Unavailable(_)
            | OrchError::Internal(_)
            | OrchError::Vmm(_)
            | OrchError::Overloaded { .. }) => {
                tracing::warn!(error = %error, "share service unavailable");
                Self::ServiceUnavailable
            }
        }
    }

    fn audit_unavailable(action: &str) -> Self {
        tracing::error!(action, "share audit pipeline unavailable");
        Self::AuditUnavailable
    }
}

impl IntoResponse for ShareApiError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::InvalidRequest | Self::BadRequest => {
                (StatusCode::BAD_REQUEST, "invalid share request")
            }
            Self::NotFound => (StatusCode::NOT_FOUND, "share not found"),
            Self::Conflict => (StatusCode::CONFLICT, "share conflict"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            Self::OwnerUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"),
            Self::ServiceUnavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, "share service unavailable")
            }
            Self::AuditUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "share audit unavailable"),
        };
        (
            status,
            Json(ErrorBody {
                error: error.into(),
            }),
        )
            .into_response()
    }
}

pub fn router(state: AppState) -> Router {
    let traffic_limits = ApiTrafficLimits::new(&state.config);
    let max_body_bytes = state.config.api_max_body_bytes;
    let protected = Router::new()
        .route("/v1/vms", post(create_vm).get(list_vms))
        .route("/v1/vms/{id}", get(get_vm).delete(delete_vm))
        .route("/v1/vms/{id}/status", get(vm_status))
        .route("/v1/vms/{id}/balloon", get(get_balloon).put(set_balloon))
        .route(
            "/v1/vms/{id}/pty/sessions",
            post(crate::pty::create_session).get(crate::pty::list_sessions),
        )
        .route(
            "/v1/vms/{id}/pty/sessions/{pty_id}",
            get(crate::pty::get_session).delete(crate::pty::delete_session),
        )
        .route(
            "/v1/vms/{id}/pty/sessions/{pty_id}/resize",
            post(crate::pty::resize_session),
        )
        .route("/v1/vms/{id}/pause", post(pause_vm))
        .route("/v1/vms/{id}/suspend", post(suspend_vm))
        .route("/v1/vms/{id}/hibernate", post(hibernate_vm))
        .route("/v1/vms/{id}/resume", post(resume_vm))
        .route("/v1/vms/{id}/snapshot", post(snapshot_vm))
        .route("/v1/vms/{id}/fork", post(fork_vm))
        .route("/v1/restore", post(restore_vm))
        .route("/v1/execute_async", post(execute_async))
        .route("/v1/execute", post(execute))
        .route("/v1/executions/{id}", get(get_execution))
        .route("/v1/volumes", post(create_volume).get(list_volumes))
        .route("/v1/volumes/{id}", get(get_volume).delete(delete_volume))
        .route("/v1/egress/vm/{id}", patch(update_egress))
        .route("/v1/branches", post(create_branch).get(list_branches))
        .route(
            "/v1/branches/{id}",
            get(get_branch)
                .put(update_branch_head)
                .delete(delete_branch),
        )
        .route("/v1/branches/{id}/restore", post(restore_branch))
        .route("/v1/branches/{id}/fork", post(restore_branch))
        .route(
            "/v1/vms/{id}/egress-policy",
            get(get_egress_policy).put(put_egress_policy),
        )
        .route("/v1/shares", post(create_share).get(list_shares))
        .route(
            "/v1/shares/{id}",
            get(get_share).patch(update_share).delete(revoke_share),
        )
        .route("/v1/shares/{id}/tokens", post(issue_share_token))
        .route(
            "/v1/ssh-keys",
            post(crate::ssh_keys::create_ssh_key).get(crate::ssh_keys::list_ssh_keys),
        )
        .route(
            "/v1/ssh-keys/{key_id}",
            delete(crate::ssh_keys::delete_ssh_key),
        )
        .route("/v1/cluster", get(cluster_status))
        .route("/v1/warm-pool", get(warm_pool_status))
        .route("/v1/usage", get(usage_stats))
        .route("/v1/audit", get(audit_log))
        .route("/metrics", get(metrics_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ))
        .layer(DefaultBodyLimit::max(max_body_bytes));
    let admitted = protected
        .route(
            "/v1/vms/{id}/pty/{pty_id}/connect",
            get(crate::pty::connect_ws),
        )
        // Keep the global admission guard outside authentication. Invalid
        // API credentials and invalid PTY connect tokens still consume bounded
        // concurrency and rate capacity, preventing authentication floods from
        // bypassing backpressure.
        .layer(middleware::from_fn_with_state(
            traffic_limits,
            enforce_api_traffic,
        ));

    Router::new()
        .route("/health", get(health))
        .route("/livez", get(livez))
        .route("/startupz", get(startupz))
        .route("/readyz", get(readyz))
        .route("/openapi.yaml", get(openapi::spec))
        .route("/docs", get(openapi::docs))
        .merge(admitted)
        .fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: "not found".into(),
        }),
    )
        .into_response()
}

async fn health(State(state): State<AppState>) -> Response {
    live_health(&state)
}

async fn livez(State(state): State<AppState>) -> Response {
    live_health(&state)
}

async fn startupz(State(state): State<AppState>) -> Response {
    let mut checks = BTreeMap::new();
    checks.insert("configuration", "ok");
    checks.insert("local_store", local_store_health(&state));
    checks.insert(
        "persistence_queue",
        if state.store_tx.is_closed() {
            "closed"
        } else {
            "ok"
        },
    );
    health_response(checks)
}

async fn readyz(State(state): State<AppState>) -> Response {
    let mut checks = BTreeMap::new();
    checks.insert("local_store", local_store_health(&state));
    checks.insert(
        "persistence_queue",
        if state.store_tx.is_closed() {
            "closed"
        } else if state.store_tx.capacity() == 0 {
            "saturated"
        } else {
            "ok"
        },
    );
    let admission = state.supervisor.admission_gate();
    checks.insert(
        "admission",
        if admission.enter().is_ok() {
            "ok"
        } else {
            "closed"
        },
    );
    checks.insert(
        "disk_pressure",
        if state.supervisor.disk_pressure_snapshot().pressured {
            "pressure"
        } else {
            "ok"
        },
    );
    if let Some(fleet) = &state.fleet {
        let fleet_status =
            match tokio::time::timeout(Duration::from_secs(2), fleet.healthcheck()).await {
                Ok(Ok(())) => "ok",
                Ok(Err(_)) => "error",
                Err(_) => "timeout",
            };
        checks.insert("fleet_store", fleet_status);
    }
    health_response(checks)
}

fn live_health(state: &AppState) -> Response {
    let mut checks = BTreeMap::new();
    checks.insert(
        "persistence_worker",
        if state.store_tx.is_closed() {
            "closed"
        } else {
            "ok"
        },
    );
    health_response(checks)
}

fn local_store_health(state: &AppState) -> &'static str {
    match state.store.try_lock() {
        Ok(store) if store.healthcheck().is_ok() => "ok",
        Ok(_) => "error",
        // A held mutex means the writer is actively using the single SQLite
        // connection, not that it is unhealthy. Avoid readiness flapping under
        // normal write load.
        Err(std::sync::TryLockError::WouldBlock) => "ok",
        Err(std::sync::TryLockError::Poisoned(_)) => "poisoned",
    }
}

fn health_response(checks: BTreeMap<&'static str, &'static str>) -> Response {
    let healthy = checks.values().all(|status| *status == "ok");
    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(serde_json::json!({
            "status": if healthy { "ok" } else { "unhealthy" },
            "checks": checks,
        })),
    )
        .into_response()
}

async fn create_share(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    request: Request,
) -> Result<(StatusCode, Json<ShareRecord>), ShareApiError> {
    let request = parse_share_json::<CreateShareRequest>(request).await;
    let attempted = match &request {
        Ok(request) => ShareAuditFields::from_create_request(request),
        Err(fields) => *fields,
    };
    record_share_audit(
        &state,
        &identity,
        audit_action::CREATE_SHARE,
        attempted,
        audit_outcome::ATTEMPT,
    )?;
    let request = match request {
        Ok(request) => request,
        Err(_) => {
            record_share_audit(
                &state,
                &identity,
                audit_action::CREATE_SHARE,
                attempted,
                audit_outcome::ERROR,
            )?;
            return Err(ShareApiError::InvalidRequest);
        }
    };
    match crate::shares::create(&state, &identity, request).await {
        Ok(share) => {
            record_share_audit(
                &state,
                &identity,
                audit_action::CREATE_SHARE,
                ShareAuditFields::from(&share),
                audit_outcome::OK,
            )?;
            Ok((StatusCode::CREATED, Json(share)))
        }
        Err(error) => {
            record_share_audit(
                &state,
                &identity,
                audit_action::CREATE_SHARE,
                attempted,
                audit_outcome_for(&error),
            )?;
            Err(ShareApiError::from_service(error))
        }
    }
}

async fn list_shares(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
) -> Result<Json<Vec<ShareRecord>>, ShareApiError> {
    Ok(Json(
        crate::shares::list(&state, &identity)
            .await
            .map_err(ShareApiError::from_service)?,
    ))
}

async fn get_share(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<String>,
) -> Result<Json<ShareRecord>, ShareApiError> {
    let id = parse_share_id(id)?;
    Ok(Json(
        crate::shares::get(&state, &identity, id)
            .await
            .map_err(ShareApiError::from_service)?,
    ))
}

async fn update_share(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<ShareRecord>, ShareApiError> {
    let id = parse_lifecycle_share_id(&state, &identity, audit_action::UPDATE_SHARE, id)?;
    let pre_mutation = share_audit_fields(&state, id).await;
    let request = parse_share_json::<UpdateShareRequest>(request).await;
    let attempted = match &request {
        Ok(request) => pre_mutation.merge(ShareAuditFields::from_update_request(request)),
        Err(fields) => pre_mutation.merge(*fields),
    };
    record_share_audit(
        &state,
        &identity,
        audit_action::UPDATE_SHARE,
        attempted,
        audit_outcome::ATTEMPT,
    )?;
    let request = match request {
        Ok(request) => request,
        Err(_) => {
            record_share_audit(
                &state,
                &identity,
                audit_action::UPDATE_SHARE,
                attempted,
                audit_outcome::ERROR,
            )?;
            return Err(ShareApiError::InvalidRequest);
        }
    };
    match crate::shares::update(&state, &identity, id, request).await {
        Ok(share) => {
            record_share_audit(
                &state,
                &identity,
                audit_action::UPDATE_SHARE,
                ShareAuditFields::from(&share),
                audit_outcome::OK,
            )?;
            Ok(Json(share))
        }
        Err(error) => {
            record_share_audit(
                &state,
                &identity,
                audit_action::UPDATE_SHARE,
                attempted,
                audit_outcome_for(&error),
            )?;
            Err(ShareApiError::from_service(error))
        }
    }
}

async fn revoke_share(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<String>,
) -> Result<StatusCode, ShareApiError> {
    let id = parse_lifecycle_share_id(&state, &identity, audit_action::REVOKE_SHARE, id)?;
    let pre_mutation = share_audit_fields(&state, id).await;
    record_share_audit(
        &state,
        &identity,
        audit_action::REVOKE_SHARE,
        pre_mutation,
        audit_outcome::ATTEMPT,
    )?;
    match crate::shares::revoke(&state, &identity, id).await {
        Ok(share) => {
            record_share_audit(
                &state,
                &identity,
                audit_action::REVOKE_SHARE,
                ShareAuditFields::from(&share),
                audit_outcome::OK,
            )?;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(error) => {
            record_share_audit(
                &state,
                &identity,
                audit_action::REVOKE_SHARE,
                pre_mutation,
                audit_outcome_for(&error),
            )?;
            Err(ShareApiError::from_service(error))
        }
    }
}

async fn issue_share_token(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<String>,
) -> Result<Json<ShareTokenResponse>, ShareApiError> {
    let id = parse_lifecycle_share_id(&state, &identity, audit_action::ISSUE_SHARE_TOKEN, id)?;
    let pre_mutation = share_audit_fields(&state, id).await;
    record_share_audit(
        &state,
        &identity,
        audit_action::ISSUE_SHARE_TOKEN,
        pre_mutation,
        audit_outcome::ATTEMPT,
    )?;
    match crate::shares::issue_token(&state, &identity, id, Utc::now()).await {
        Ok(token) => {
            record_share_audit(
                &state,
                &identity,
                audit_action::ISSUE_SHARE_TOKEN,
                pre_mutation,
                audit_outcome::OK,
            )?;
            Ok(Json(token))
        }
        Err(error) => {
            record_share_audit(
                &state,
                &identity,
                audit_action::ISSUE_SHARE_TOKEN,
                pre_mutation,
                audit_outcome_for(&error),
            )?;
            Err(ShareApiError::from_service(error))
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ShareAuditFields {
    share_id: Option<Uuid>,
    vm_id: Option<Uuid>,
    guest_port: Option<u16>,
    attempted_guest_port: Option<i64>,
    visibility: Option<ShareVisibility>,
}

impl From<&ShareRecord> for ShareAuditFields {
    fn from(share: &ShareRecord) -> Self {
        Self {
            share_id: Some(share.id),
            vm_id: Some(share.vm_id),
            guest_port: Some(share.guest_port),
            attempted_guest_port: None,
            visibility: Some(share.visibility),
        }
    }
}

impl ShareAuditFields {
    fn from_create_request(request: &CreateShareRequest) -> Self {
        Self {
            vm_id: Some(request.vm_id),
            guest_port: Some(request.guest_port),
            visibility: Some(request.visibility),
            ..Default::default()
        }
    }

    fn from_update_request(request: &UpdateShareRequest) -> Self {
        Self {
            vm_id: request.vm_id,
            guest_port: request.guest_port,
            visibility: request.visibility,
            ..Default::default()
        }
    }

    fn from_malformed_json(body: &[u8]) -> Self {
        let Ok(serde_json::Value::Object(object)) = serde_json::from_slice(body) else {
            return Self::default();
        };
        let (guest_port, attempted_guest_port) = match object.get("guest_port") {
            Some(serde_json::Value::Number(port)) => match port.as_i64() {
                Some(port) => match u16::try_from(port).ok() {
                    Some(port) => (Some(port), None),
                    None => (None, Some(port)),
                },
                None => (None, None),
            },
            _ => (None, None),
        };
        Self {
            vm_id: object
                .get("vm_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|id| Uuid::parse_str(id).ok()),
            guest_port,
            attempted_guest_port,
            visibility: object
                .get("visibility")
                .and_then(serde_json::Value::as_str)
                .and_then(|visibility| match visibility {
                    "public" => Some(ShareVisibility::Public),
                    "private" => Some(ShareVisibility::Private),
                    _ => None,
                }),
            ..Default::default()
        }
    }

    fn merge(self, attempted: Self) -> Self {
        Self {
            share_id: attempted.share_id.or(self.share_id),
            vm_id: attempted.vm_id.or(self.vm_id),
            guest_port: attempted.guest_port.or(self.guest_port),
            attempted_guest_port: attempted.attempted_guest_port.or(self.attempted_guest_port),
            visibility: attempted.visibility.or(self.visibility),
        }
    }
}

const MAX_SHARE_REQUEST_BYTES: usize = 1024 * 1024;

async fn parse_share_json<T: DeserializeOwned>(request: Request) -> Result<T, ShareAuditFields> {
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_SHARE_REQUEST_BYTES).await {
        Ok(body) => body,
        Err(error) => {
            tracing::debug!(%error, "invalid share request body");
            return Err(ShareAuditFields::default());
        }
    };
    let fields = ShareAuditFields::from_malformed_json(&body);
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    if !content_type.is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
    }) {
        tracing::debug!("invalid share content type");
        return Err(fields);
    }
    serde_json::from_slice(&body).map_err(|error| {
        tracing::debug!(%error, "invalid share request");
        fields
    })
}

fn parse_share_id(id: String) -> Result<Uuid, ShareApiError> {
    Uuid::parse_str(&id).map_err(|error| {
        tracing::debug!(%error, "invalid share path");
        ShareApiError::InvalidRequest
    })
}

fn parse_lifecycle_share_id(
    state: &AppState,
    identity: &ApiIdentity,
    action: &str,
    id: String,
) -> Result<Uuid, ShareApiError> {
    match parse_share_id(id) {
        Ok(id) => Ok(id),
        Err(error) => {
            record_share_audit(
                state,
                identity,
                action,
                ShareAuditFields::default(),
                audit_outcome::ATTEMPT,
            )?;
            record_share_audit(
                state,
                identity,
                action,
                ShareAuditFields::default(),
                audit_outcome::ERROR,
            )?;
            Err(error)
        }
    }
}

async fn share_audit_fields(state: &AppState, id: Uuid) -> ShareAuditFields {
    match state.shares.get(id).await {
        Ok(Some(share)) => ShareAuditFields::from(&share),
        Ok(None) | Err(_) => ShareAuditFields {
            share_id: Some(id),
            ..Default::default()
        },
    }
}

fn audit_outcome_for(error: &OrchError) -> &'static str {
    match error {
        OrchError::Forbidden(_) => audit_outcome::DENIED,
        _ => audit_outcome::ERROR,
    }
}

fn record_share_audit(
    state: &AppState,
    identity: &ApiIdentity,
    action: &str,
    fields: ShareAuditFields,
    outcome: &str,
) -> Result<(), ShareApiError> {
    let vm_id = fields.vm_id;
    let mut detail_fields = Vec::new();
    if let Some(share_id) = fields.share_id {
        detail_fields.push(format!("share_id={share_id}"));
    }
    if let Some(vm_id) = fields.vm_id {
        detail_fields.push(format!("vm_id={vm_id}"));
    }
    if let Some(guest_port) = fields.guest_port {
        detail_fields.push(format!("guest_port={guest_port}"));
    }
    if let Some(attempted_guest_port) = fields.attempted_guest_port {
        detail_fields.push(format!("attempted_guest_port={attempted_guest_port}"));
    }
    if let Some(visibility) = fields.visibility {
        let visibility = match visibility {
            ShareVisibility::Public => "public",
            ShareVisibility::Private => "private",
        };
        detail_fields.push(format!("visibility={visibility}"));
    }
    let detail = (!detail_fields.is_empty()).then(|| detail_fields.join("; "));
    audit::record_required(state, identity, action, vm_id, outcome, detail)
        .map_err(|_| ShareApiError::audit_unavailable(action))
}

fn validate_volume_name(name: &str) -> Result<(), OrchError> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(OrchError::BadRequest(
            "volume name must be 1..=128 ASCII [A-Za-z0-9._-] bytes".into(),
        ));
    }
    Ok(())
}

fn volume_provider_error(id: Uuid, error: VolumeError) -> OrchError {
    match error {
        VolumeError::Invalid(message) => OrchError::BadRequest(message),
        VolumeError::NotFound => OrchError::NotFound(format!("volume {id}")),
        VolumeError::Conflict => OrchError::Conflict(format!(
            "volume {id} has different immutable provider properties"
        )),
        VolumeError::Unsupported(message) => OrchError::Unprocessable(message),
        VolumeError::IdentityMismatch => {
            tracing::error!(volume_id = %id, "volume provider identity/generation mismatch");
            OrchError::Conflict(format!("volume {id} attachment identity changed"))
        }
        VolumeError::Busy => OrchError::Conflict(format!("volume {id} is busy")),
        VolumeError::Timeout => OrchError::Unavailable(format!("volume {id} provider timed out")),
        VolumeError::UnsafeObject | VolumeError::Io(_) => {
            tracing::error!(volume_id = %id, %error, "volume provider failed closed");
            OrchError::Internal("volume provider operation failed".into())
        }
    }
}

fn public_volume(record: VolumeRecord) -> PublicVolumeRecord {
    PublicVolumeRecord::from(record)
}

pub(crate) fn volume_fleet_err(error: tarit_fleet::FleetError) -> OrchError {
    match error {
        tarit_fleet::FleetError::NotFound => OrchError::NotFound("volume not found".into()),
        tarit_fleet::FleetError::Conflict(message) => OrchError::Conflict(message),
        other => OrchError::Internal(format!("fleet volume operation: {other}")),
    }
}

async fn create_volume(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Json(request): Json<CreateVolumeRequest>,
) -> Result<(StatusCode, Json<PublicVolumeRecord>), ApiError> {
    validate_volume_name(&request.name)?;
    let max_size_bytes = crate::volume_provider::max_size_bytes(&state.config, &request.provider)
        .ok_or_else(|| {
        OrchError::Unprocessable(format!(
            "volume provider {:?} is not configured on this host",
            request.provider
        ))
    })?;
    if !(MIN_BLOCK_VOLUME_BYTES..=max_size_bytes).contains(&request.size_bytes) {
        return Err(OrchError::BadRequest(format!(
            "block volume size must be between {MIN_BLOCK_VOLUME_BYTES} and {max_size_bytes} bytes"
        ))
        .into());
    }
    let id = request.id.unwrap_or_else(Uuid::new_v4);
    let provider = crate::volume_provider::open(&state.config, &request.provider)?;
    let placement = crate::volume_provider::placement(&state.config, &request.provider)?;
    let capabilities = provider.capabilities();
    let now = Utc::now();
    let desired = VolumeRecord {
        id,
        owner_key: identity.tenant.clone(),
        name: request.name,
        provider: provider.provider_name().into(),
        storage_class: VolumeStorageClass::Block,
        size_bytes: request.size_bytes,
        status: VolumeStatus::Creating,
        capabilities: VolumeCapabilities {
            read_only_many: capabilities.read_only_many,
            read_write_once: capabilities.read_write_once,
            read_write_many: capabilities.read_write_many,
            snapshots: capabilities.snapshots,
            clones: capabilities.clones,
        },
        host_id: placement.host_id,
        region: placement.region,
        zone: placement.zone,
        generation: 1,
        revision: 1,
        last_error: None,
        created_at: now,
        updated_at: now,
    };
    let (fleet_persisted, existed) = if let Some(fleet) = &state.fleet {
        let existed = fleet.get_volume(&identity.tenant, id).await.is_ok();
        let persisted = fleet
            .insert_volume(&desired)
            .await
            .map_err(volume_fleet_err)?;
        (Some(persisted), existed)
    } else {
        (None, false)
    };
    let (local_persisted, local_existed) = {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock".into()))?;
        let existed = store.get_volume(&identity.tenant, id).is_ok();
        let persisted = store.insert_volume(&desired).map_err(store_err)?;
        (persisted, existed)
    };
    let persisted = fleet_persisted.unwrap_or_else(|| local_persisted.clone());
    let existed = if state.fleet.is_some() {
        existed
    } else {
        local_existed
    };
    if persisted.status == VolumeStatus::Deleting {
        return Err(OrchError::Conflict(format!("volume {id} is deleting")).into());
    }

    let size_bytes = persisted.size_bytes;
    let provider_result = tokio::task::spawn_blocking(move || provider.create(id, size_bytes))
        .await
        .map_err(|error| OrchError::Internal(format!("volume provider join: {error}")))?;
    let record = match provider_result {
        Ok(_) => {
            let authoritative = if persisted.status == VolumeStatus::Available {
                persisted
            } else if let Some(fleet) = &state.fleet {
                fleet
                    .transition_volume(
                        &identity.tenant,
                        id,
                        VolumeTransition {
                            expected_status: persisted.status,
                            expected_revision: persisted.revision,
                            status: VolumeStatus::Available,
                            last_error: None,
                            updated_at: Utc::now(),
                        },
                    )
                    .await
                    .map_err(volume_fleet_err)?
            } else {
                state
                    .store
                    .lock()
                    .map_err(|_| OrchError::Internal("store lock".into()))?
                    .transition_volume(
                        &identity.tenant,
                        id,
                        VolumeTransition {
                            expected_status: persisted.status,
                            expected_revision: persisted.revision,
                            status: VolumeStatus::Available,
                            last_error: None,
                            updated_at: Utc::now(),
                        },
                    )
                    .map_err(store_err)?
            };
            if state.fleet.is_some() && local_persisted.status != VolumeStatus::Available {
                state
                    .store
                    .lock()
                    .map_err(|_| OrchError::Internal("store lock".into()))?
                    .transition_volume(
                        &identity.tenant,
                        id,
                        VolumeTransition {
                            expected_status: local_persisted.status,
                            expected_revision: local_persisted.revision,
                            status: VolumeStatus::Available,
                            last_error: None,
                            updated_at: Utc::now(),
                        },
                    )
                    .map_err(store_err)?;
            }
            authoritative
        }
        Err(error) => {
            let safe_error = error.to_string();
            if persisted.status != VolumeStatus::Error {
                if let Some(fleet) = &state.fleet {
                    if let Err(transition_error) = fleet
                        .transition_volume(
                            &identity.tenant,
                            id,
                            VolumeTransition {
                                expected_status: persisted.status,
                                expected_revision: persisted.revision,
                                status: VolumeStatus::Error,
                                last_error: Some(&safe_error),
                                updated_at: Utc::now(),
                            },
                        )
                        .await
                    {
                        tracing::error!(volume_id = %id, %transition_error,
                            "failed to persist fleet volume provider error");
                    }
                }
                let transition_result = state
                    .store
                    .lock()
                    .map_err(|_| OrchError::Internal("store lock".into()))?
                    .transition_volume(
                        &identity.tenant,
                        id,
                        VolumeTransition {
                            expected_status: persisted.status,
                            expected_revision: persisted.revision,
                            status: VolumeStatus::Error,
                            last_error: Some(&safe_error),
                            updated_at: Utc::now(),
                        },
                    );
                if let Err(transition_error) = transition_result {
                    tracing::error!(volume_id = %id, %transition_error,
                        "failed to persist volume provider error");
                }
            }
            return Err(volume_provider_error(id, error).into());
        }
    };
    audit::record(
        &state,
        &identity,
        audit_action::CREATE_VOLUME,
        None,
        audit_outcome::OK,
        None,
    );
    let status = if existed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(public_volume(record))))
}

async fn list_volumes(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
) -> Result<Json<Vec<PublicVolumeRecord>>, ApiError> {
    let records = if let Some(fleet) = &state.fleet {
        fleet
            .list_volumes(&identity.tenant)
            .await
            .map_err(volume_fleet_err)?
    } else {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock".into()))?
            .list_volumes(&identity.tenant)
            .map_err(store_err)?
    };
    Ok(Json(records.into_iter().map(public_volume).collect()))
}

async fn get_volume(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<PublicVolumeRecord>, ApiError> {
    let record = if let Some(fleet) = &state.fleet {
        fleet
            .get_volume(&identity.tenant, id)
            .await
            .map_err(volume_fleet_err)?
    } else {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock".into()))?
            .get_volume(&identity.tenant, id)
            .map_err(store_err)?
    };
    Ok(Json(public_volume(record)))
}

async fn delete_volume(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if let Some(fleet) = &state.fleet {
        let current = fleet
            .get_volume(&identity.tenant, id)
            .await
            .map_err(volume_fleet_err)?;
        if current
            .host_id
            .as_deref()
            .is_some_and(|host_id| host_id != state.config.host_id)
        {
            let host_id = current
                .host_id
                .ok_or_else(|| OrchError::Conflict(format!("volume {id} has no owning host")))?;
            let target = cluster::peer_rpc(&state, &host_id)
                .await?
                .ok_or_else(|| OrchError::Unavailable(format!("volume host {host_id} is down")))?;
            let peer = Arc::clone(&state.peer);
            tokio::task::spawn_blocking(move || peer.delete_volume_remote(&target, id, &identity))
                .await
                .map_err(|error| OrchError::Internal(format!("volume delete join: {error}")))??;
            return Ok(StatusCode::NO_CONTENT);
        }
    }
    delete_volume_local(&state, &identity, id).await
}

pub(crate) async fn delete_volume_local(
    state: &AppState,
    identity: &ApiIdentity,
    id: Uuid,
) -> Result<StatusCode, ApiError> {
    let current = if let Some(fleet) = &state.fleet {
        fleet
            .get_volume(&identity.tenant, id)
            .await
            .map_err(volume_fleet_err)?
    } else {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock".into()))?
            .get_volume(&identity.tenant, id)
            .map_err(store_err)?
    };
    let attachment_count = if let Some(fleet) = &state.fleet {
        fleet
            .volume_attachment_count(&identity.tenant, id)
            .await
            .map_err(volume_fleet_err)?
    } else {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock".into()))?
            .volume_attachment_count(&identity.tenant, id)
            .map_err(store_err)?
    };
    if attachment_count != 0 {
        return Err(OrchError::Conflict(format!(
            "volume {id} is attached to {attachment_count} VM(s)"
        ))
        .into());
    }
    let deleting = match current.status {
        VolumeStatus::Deleting => current,
        VolumeStatus::Available | VolumeStatus::Error => {
            if let Some(fleet) = &state.fleet {
                fleet
                    .begin_volume_delete(
                        &identity.tenant,
                        id,
                        current.status,
                        current.revision,
                        Utc::now(),
                    )
                    .await
                    .map_err(volume_fleet_err)?
            } else {
                state
                    .store
                    .lock()
                    .map_err(|_| OrchError::Internal("store lock".into()))?
                    .begin_volume_delete(
                        &identity.tenant,
                        id,
                        current.status,
                        current.revision,
                        Utc::now(),
                    )
                    .map_err(store_err)?
            }
        }
        VolumeStatus::Creating => {
            return Err(OrchError::Conflict(format!("volume {id} is creating")).into())
        }
    };
    if state.fleet.is_some() {
        let local = {
            let store = state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock".into()))?;
            match store.get_volume(&identity.tenant, id) {
                Ok(local) => local,
                Err(tarit_store::StoreError::NotFound) => {
                    store.insert_volume(&deleting).map_err(store_err)?
                }
                Err(error) => return Err(store_err(error).into()),
            }
        };
        if local.status != VolumeStatus::Deleting {
            state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock".into()))?
                .begin_volume_delete(
                    &identity.tenant,
                    id,
                    local.status,
                    local.revision,
                    Utc::now(),
                )
                .map_err(store_err)?;
        }
    }
    let provider = crate::volume_provider::open(&state.config, &deleting.provider)?;
    let provider_result = tokio::task::spawn_blocking(move || provider.delete(id))
        .await
        .map_err(|error| OrchError::Internal(format!("volume provider join: {error}")))?;
    if let Err(error) = provider_result {
        if !matches!(error, VolumeError::NotFound) {
            let safe_error = error.to_string();
            if let Some(fleet) = &state.fleet {
                if let Err(transition_error) = fleet
                    .transition_volume(
                        &identity.tenant,
                        id,
                        VolumeTransition {
                            expected_status: VolumeStatus::Deleting,
                            expected_revision: deleting.revision,
                            status: VolumeStatus::Error,
                            last_error: Some(&safe_error),
                            updated_at: Utc::now(),
                        },
                    )
                    .await
                {
                    tracing::error!(volume_id = %id, %transition_error,
                        "failed to persist fleet volume deletion error");
                }
            }
            let transition_result = state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock".into()))?
                .transition_volume(
                    &identity.tenant,
                    id,
                    VolumeTransition {
                        expected_status: VolumeStatus::Deleting,
                        expected_revision: deleting.revision,
                        status: VolumeStatus::Error,
                        last_error: Some(&safe_error),
                        updated_at: Utc::now(),
                    },
                );
            if let Err(transition_error) = transition_result {
                tracing::error!(volume_id = %id, %transition_error,
                    "failed to persist volume deletion error");
            }
            return Err(volume_provider_error(id, error).into());
        }
    }
    let local_delete = {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock".into()))?;
        store
            .get_volume(&identity.tenant, id)
            .and_then(|local| store.delete_volume_metadata(&identity.tenant, id, local.revision))
    };
    if let Err(error) = local_delete {
        if !matches!(error, tarit_store::StoreError::NotFound) {
            return Err(store_err(error).into());
        }
    }
    if let Some(fleet) = &state.fleet {
        fleet
            .delete_volume_metadata(&identity.tenant, id, deleting.revision)
            .await
            .map_err(volume_fleet_err)?;
    }
    audit::record(
        state,
        identity,
        audit_action::DELETE_VOLUME,
        None,
        audit_outcome::OK,
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn metrics_handler(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
) -> Result<Response, ApiError> {
    require_admin(&identity)?;
    Ok((
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        crate::metrics::render_metrics(&state),
    )
        .into_response())
}

async fn create_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Json(req): Json<CreateVmRequest>,
) -> Result<(StatusCode, Json<PublicVmRecord>), ApiError> {
    let result = create_vm_impl(&state, &identity, req).await;
    match &result {
        Ok((_, Json(rec))) => {
            state.metrics.inc_vm_create_total();
            audit::record(
                &state,
                &identity,
                audit_action::CREATE,
                Some(rec.id),
                audit_outcome::OK,
                None,
            );
        }
        Err(e) => {
            state.metrics.inc_vm_create_errors_total();
            audit::record(
                &state,
                &identity,
                audit_action::CREATE,
                None,
                audit_outcome::ERROR,
                Some(e.0.to_string()),
            );
        }
    }
    result
}

async fn create_vm_impl(
    state: &AppState,
    identity: &ApiIdentity,
    mut req: CreateVmRequest,
) -> Result<(StatusCode, Json<PublicVmRecord>), ApiError> {
    ensure_create_admission_open(state)?;
    req.owner_key = Some(identity.tenant.clone());
    req.api_key_id = Some(identity.api_key_id.clone());
    let id = *req.id.get_or_insert_with(Uuid::new_v4);
    enforce_create_path_policy(identity, &req)?;
    enforce_image_admission_policy(identity, &req, &state.config.image_admission_policy)?;
    match cluster::resolve_owner(state, id).await {
        Ok(_) => {
            return Err(OrchError::Conflict(format!("vm {id} already exists")).into());
        }
        Err(OrchError::NotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }
    let reserved = reserve_vm_quota(state, identity, id).await?;
    let result = create_vm_after_quota(state, identity, req).await;
    if reserved {
        if let Err(error) = release_vm_quota(state, identity, id).await {
            tracing::warn!(vm = %id, tenant = %identity.tenant, %error,
                "failed to release create quota reservation; TTL cleanup will retry implicitly");
        }
    }
    result
}

fn enforce_image_admission_policy(
    identity: &ApiIdentity,
    request: &CreateVmRequest,
    policy: &crate::image::ImageAdmissionPolicy,
) -> Result<(), OrchError> {
    if policy.require_signature && !identity.is_admin() && request.image.is_none() {
        return Err(OrchError::Unprocessable(
            "production admission requires an OCI image verified by the configured provenance key"
                .into(),
        ));
    }
    Ok(())
}

async fn create_vm_after_quota(
    state: &AppState,
    identity: &ApiIdentity,
    req: CreateVmRequest,
) -> Result<(StatusCode, Json<PublicVmRecord>), ApiError> {
    let exact_volume_host = requested_volume_host(state, identity, &req).await?;
    if let Some(target_host) = exact_volume_host.as_deref() {
        if target_host != state.config.host_id {
            let target = cluster::peer_rpc(state, target_host)
                .await?
                .ok_or_else(|| {
                    OrchError::Unavailable(format!(
                        "persistent-volume host {target_host} is not healthy"
                    ))
                })?;
            let peer = Arc::clone(&state.peer);
            let forwarded = req.clone();
            let identity = identity.clone();
            let record = tokio::task::spawn_blocking(move || {
                peer.create_remote(&target, &forwarded, &identity)
            })
            .await
            .map_err(|error| OrchError::Internal(format!("volume placement join: {error}")))??;
            return Ok((StatusCode::CREATED, Json(PublicVmRecord::from(record))));
        }
    }
    // Cluster admission: place locally (warm/cold) if this node has room; else
    // spill to ANY peer that has capacity (exhaustive). Only if the WHOLE
    // cluster is full do we wait for a slot to free, and only after the
    // admission timeout do we return 429 + Retry-After. As long as one node in
    // the fleet can take the VM, the request succeeds.
    let deadline = Instant::now() + Duration::from_millis(state.config.admission_timeout_ms);
    loop {
        let last_overloaded = match ops::create_local(state, &req).await {
            Ok(record) => return Ok((StatusCode::CREATED, Json(PublicVmRecord::from(record)))),
            Err(OrchError::Overloaded { message, .. }) => message, // local full — try the rest of the fleet
            Err(e) => return Err(e.into()),
        };

        if exact_volume_host.is_some() {
            return Err(OrchError::Overloaded {
                message: format!(
                    "the exact host for local persistent volumes is at capacity: {last_overloaded}"
                ),
                retry_after_secs: 1,
            }
            .into());
        }

        if state.fleet.is_some() {
            if let Some(record) = place_on_peer(state, &req, identity).await? {
                return Ok((StatusCode::CREATED, Json(PublicVmRecord::from(record))));
            }
        }

        if is_network_pool_exhausted(&last_overloaded) {
            return Err(OrchError::Overloaded {
                message: last_overloaded,
                retry_after_secs: 1,
            }
            .into());
        }

        if Instant::now() >= deadline {
            let detail = format!(" (last local capacity error: {last_overloaded})");
            return Err(OrchError::Overloaded {
                message: format!(
                    "cluster at capacity — no VM slot became available within {}ms{}",
                    state.config.admission_timeout_ms, detail
                ),
                retry_after_secs: state.config.admission_timeout_ms.div_ceil(1000).max(1),
            }
            .into());
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

/// Resolve the exact host required by any node-local block volumes. Shared
/// regional block volumes do not constrain cluster placement; mixed local and
/// shared requests follow the local volume's host.
async fn requested_volume_host(
    state: &AppState,
    identity: &ApiIdentity,
    request: &CreateVmRequest,
) -> Result<Option<String>, OrchError> {
    if request.volumes.is_empty() {
        return Ok(None);
    }
    let Some(fleet) = &state.fleet else {
        return Ok(Some(state.config.host_id.clone()));
    };
    let mut target = None;
    let mut seen = std::collections::HashSet::new();
    for requested in &request.volumes {
        if !seen.insert(requested.volume_id) {
            return Err(OrchError::BadRequest(format!(
                "volume {} is attached more than once",
                requested.volume_id
            )));
        }
        let volume = fleet
            .get_volume(&identity.tenant, requested.volume_id)
            .await
            .map_err(volume_fleet_err)?;
        if let Some(host) = crate::volume_provider::placement_target(&state.config, &volume)? {
            if target.as_ref().is_some_and(|current| current != &host) {
                return Err(OrchError::Conflict(
                    "requested local block volumes are pinned to different hosts".into(),
                ));
            }
            target = Some(host);
        }
    }
    Ok(target)
}

pub(crate) fn enforce_create_path_policy(
    identity: &ApiIdentity,
    req: &CreateVmRequest,
) -> Result<(), OrchError> {
    if identity.is_admin() {
        return Ok(());
    }
    if req.kernel_path.is_some() || req.rootfs_path.is_some() {
        return Err(OrchError::Forbidden(
            "non-admin create requests must use node defaults or a registered image".into(),
        ));
    }
    Ok(())
}

/// Reject a create before owner lookup, local scheduling, or peer placement once
/// shutdown has closed admission. The short-lived permit serializes this check
/// with `VmAdmissionGate::close`; peer placement takes its own permit at the
/// task-spawn boundary below.
fn ensure_create_admission_open(state: &AppState) -> Result<(), ApiError> {
    let admission = state.supervisor.admission_gate();
    let _permit = admission.enter()?;
    Ok(())
}

/// Exhaustively try to place `req` on peers: iterate every healthy peer that
/// currently advertises capacity (best-first) and forward the create until one
/// accepts. Returns `Ok(None)` only if no peer could take it right now.
async fn place_on_peer(
    state: &AppState,
    req: &CreateVmRequest,
    identity: &ApiIdentity,
) -> Result<Option<VmRecord>, ApiError> {
    let candidates = cluster::place_candidates(state, req.vcpus, req.memory_mib).await;
    for rpc in candidates {
        let peer = Arc::clone(&state.peer);
        let req = req.clone();
        let identity = identity.clone();
        let rpc_for_log = rpc.clone();
        // Serialize admission with shutdown at the side-effect boundary. This
        // mirrors the autoscaler provider: a request admitted before shutdown
        // may finish, but a draining node cannot launch a new peer-create task.
        let task = {
            let admission = state.supervisor.admission_gate();
            let _permit = admission.enter()?;
            tokio::task::spawn_blocking(move || peer.create_remote(&rpc, &req, &identity))
        };
        let res = task
            .await
            .map_err(|e| OrchError::Internal(format!("join: {e}")))?;
        match res {
            Ok(record) => {
                tracing::info!(peer = %rpc_for_log, id = %record.id, "create: placed on peer");
                return Ok(Some(record));
            }
            // Peer filled up or the VM vanished between the capacity read and the
            // call — just try the next candidate. A peer 409 is a real conflict
            // (for example a duplicate requested id), not capacity backpressure.
            Err(OrchError::Overloaded { .. }) | Err(OrchError::NotFound(_)) => continue,
            Err(e @ OrchError::Conflict(_)) => return Err(e.into()),
            Err(e) => {
                tracing::warn!(peer = %rpc_for_log, "peer create failed: {e}; trying next");
                continue;
            }
        }
    }
    Ok(None)
}

fn is_network_pool_exhausted(message: &str) -> bool {
    message.contains("network address pool exhausted")
}

/// Restore a snapshot into a running VM. Routes to the node that holds the
/// snapshot file (`host_id`, as returned by the snapshot call) so no cross-node
/// file transfer is needed; `None`/self restores locally.
async fn restore_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Json(req): Json<RestoreRequest>,
) -> Result<(StatusCode, Json<PublicVmRecord>), ApiError> {
    let id = req.id.unwrap_or_else(Uuid::new_v4);
    match cluster::resolve_owner(&state, id).await {
        Ok(_) => return Err(OrchError::Conflict(format!("vm {id} already exists")).into()),
        Err(OrchError::NotFound(_)) => {}
        Err(error) => return Err(error.into()),
    }
    let reserved = reserve_vm_quota(&state, &identity, id).await?;
    let result = restore_vm_after_quota(&state, &identity, req.snapshot_id, id).await;
    if reserved {
        if let Err(error) = release_vm_quota(&state, &identity, id).await {
            tracing::warn!(vm = %id, tenant = %identity.tenant, %error,
                "failed to release restore quota reservation; TTL cleanup will retry implicitly");
        }
    }
    result
}

async fn restore_vm_after_quota(
    state: &AppState,
    identity: &ApiIdentity,
    snapshot_id: Uuid,
    id: Uuid,
) -> Result<(StatusCode, Json<PublicVmRecord>), ApiError> {
    let (host_id, snapshot_path) = resolve_snapshot_locator(state, identity, snapshot_id).await?;
    let on_peer = if host_id != state.config.host_id {
        cluster::peer_rpc(state, &host_id).await?
    } else {
        None
    };
    let record = match on_peer {
        Some(rpc) => {
            let peer = Arc::clone(&state.peer);
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || {
                peer.restore_remote(&rpc, &snapshot_path, id, &identity)
            })
            .await
            .map_err(|e| OrchError::Internal(format!("join: {e}")))??
        }
        None => {
            ops::restore_local(
                state,
                &snapshot_path,
                Some(id),
                Some(identity.tenant.clone()),
                Some(identity.api_key_id.clone()),
                identity.is_admin(),
            )
            .await?
        }
    };
    audit::record(
        state,
        identity,
        audit_action::RESTORE,
        Some(record.id),
        audit_outcome::OK,
        None,
    );
    Ok((StatusCode::CREATED, Json(PublicVmRecord::from(record))))
}

async fn resolve_snapshot_locator(
    state: &AppState,
    identity: &ApiIdentity,
    snapshot_id: Uuid,
) -> Result<(String, String), ApiError> {
    let local = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .get_snapshot_by_id(snapshot_id)
        .map_err(store_err)?;
    if let Some(snapshot) = local {
        if !identity.is_admin() && snapshot.owner_key.as_deref() != Some(&identity.tenant) {
            return Err(OrchError::NotFound("snapshot not found".into()).into());
        }
        return Ok((snapshot.host_id, snapshot.path));
    }
    if let Some(fleet) = state.fleet.as_ref() {
        if let Some(snapshot) = fleet
            .get_snapshot_location(snapshot_id)
            .await
            .map_err(|error| OrchError::Internal(format!("fleet snapshot lookup: {error}")))?
        {
            if !identity.is_admin() && snapshot.owner_key != identity.tenant {
                return Err(OrchError::NotFound("snapshot not found".into()).into());
            }
            return Ok((snapshot.host_id, snapshot.snapshot_path));
        }
    }
    Err(OrchError::NotFound("snapshot not found".into()).into())
}

/// Build a `VmRecord` for an already-running VM (warm-pool hand-out).
#[allow(clippy::too_many_arguments)]
pub(crate) fn running_record(
    state: &AppState,
    spawn_cfg: &VmSpawnConfig,
    id: Uuid,
    pid: u32,
    socket_path: &std::path::Path,
    owner_key: Option<String>,
    api_key_id: Option<String>,
    now: chrono::DateTime<Utc>,
) -> VmRecord {
    VmRecord {
        id,
        host_id: state.config.host_id.clone(),
        owner_key,
        api_key_id,
        status: VmStatus::Running,
        revision: 1,
        startup_path: None,
        memory_mib: spawn_cfg.memory_mib,
        vcpus: spawn_cfg.vcpus,
        kernel_path: spawn_cfg.kernel_path.display().to_string(),
        rootfs_path: spawn_cfg
            .rootfs_path
            .as_ref()
            .map(|p| p.display().to_string()),
        rootfs_read_only: spawn_cfg.read_only,
        cmdline: spawn_cfg.cmdline.clone(),
        runtime_layout: Some(state.supervisor.runtime_layout_for_config(id, spawn_cfg)),
        socket_path: Some(socket_path.display().to_string()),
        pid: Some(pid),
        created_at: now,
        updated_at: Utc::now(),
    }
}

pub(crate) fn ensure_vm_access(identity: &ApiIdentity, vm: &VmRecord) -> Result<(), OrchError> {
    if identity_can_access_vm(identity, vm) {
        Ok(())
    } else {
        Err(OrchError::Forbidden(
            "VM does not belong to this tenant".into(),
        ))
    }
}

pub(crate) fn identity_can_access_vm(identity: &ApiIdentity, vm: &VmRecord) -> bool {
    identity.is_admin() || vm.owner_key.as_deref() == Some(identity.tenant.as_str())
}

fn require_admin(identity: &ApiIdentity) -> Result<(), OrchError> {
    if identity.role == ApiRole::Admin {
        Ok(())
    } else {
        Err(OrchError::Forbidden("admin role required".into()))
    }
}

#[cfg(test)]
async fn enforce_vm_quota(state: &AppState, identity: &ApiIdentity) -> Result<(), OrchError> {
    let Some(max_vms) = identity.max_vms else {
        return Ok(());
    };
    let active = tenant_active_vm_count(state, &identity.tenant).await?;
    if active >= max_vms {
        return Err(OrchError::Forbidden(format!(
            "tenant {} VM quota exceeded: active VMs {active} >= max_vms {max_vms}",
            identity.tenant
        )));
    }
    Ok(())
}

/// Reserve quota before placement, rather than merely counting active rows.
/// The reservation id is the VM id carried through local and peer placement, so
/// exactly one slot is consumed for the whole admission loop.
async fn reserve_vm_quota(
    state: &AppState,
    identity: &ApiIdentity,
    id: Uuid,
) -> Result<bool, OrchError> {
    let Some(max_vms) = identity.max_vms else {
        return Ok(false);
    };
    let ttl_ms = state
        .config
        .admission_timeout_ms
        .saturating_add(5 * 60 * 1_000)
        .min(i64::MAX as u64) as i64;
    let expires_at = Utc::now() + chrono::Duration::milliseconds(ttl_ms);
    if let Some(fleet) = &state.fleet {
        return match fleet
            .reserve_vm_quota(&identity.tenant, id, max_vms, expires_at)
            .await
        {
            Ok(()) => Ok(true),
            Err(tarit_fleet::FleetError::QuotaExceeded { .. }) => {
                Err(OrchError::Forbidden(format!(
                    "tenant {} VM quota exceeded: max_vms {max_vms}",
                    identity.tenant
                )))
            }
            Err(tarit_fleet::FleetError::Conflict(message)) => Err(OrchError::Conflict(message)),
            Err(error) => Err(OrchError::Internal(format!(
                "reserve fleet tenant quota: {error}"
            ))),
        };
    }
    let outcome = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .reserve_vm_quota(&identity.tenant, id, max_vms, expires_at)
        .map_err(store_err)?;
    match outcome {
        tarit_store::VmQuotaReservationOutcome::Reserved => Ok(true),
        tarit_store::VmQuotaReservationOutcome::QuotaExceeded => {
            Err(OrchError::Forbidden(format!(
                "tenant {} VM quota exceeded: max_vms {max_vms}",
                identity.tenant
            )))
        }
        tarit_store::VmQuotaReservationOutcome::IdConflict => Err(OrchError::Conflict(format!(
            "VM {id} already exists or is being created"
        ))),
    }
}

async fn release_vm_quota(
    state: &AppState,
    identity: &ApiIdentity,
    id: Uuid,
) -> Result<(), OrchError> {
    if let Some(fleet) = &state.fleet {
        fleet
            .release_vm_quota(&identity.tenant, id)
            .await
            .map_err(|error| OrchError::Internal(format!("release fleet tenant quota: {error}")))
    } else {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .release_vm_quota(&identity.tenant, id)
            .map_err(store_err)
    }
}

#[cfg(test)]
async fn tenant_active_vm_count(state: &AppState, tenant: &str) -> Result<usize, OrchError> {
    if let Some(fleet) = &state.fleet {
        return fleet
            .count_active_vms_for_owner(tenant)
            .await
            .map_err(|e| OrchError::Internal(format!("fleet tenant quota count: {e}")));
    }
    Ok(tenant_active_vm_count_local(state, tenant))
}

#[cfg(test)]
fn tenant_active_vm_count_local(state: &AppState, tenant: &str) -> usize {
    state
        .vm_cache
        .read()
        .map(|cache| {
            cache
                .values()
                .filter(|vm| vm.owner_key.as_deref() == Some(tenant))
                .filter(|vm| is_active_vm_status(vm.status))
                .count()
        })
        .unwrap_or_default()
}

#[cfg(test)]
fn is_active_vm_status(status: VmStatus) -> bool {
    matches!(
        status,
        VmStatus::Creating
            | VmStatus::Running
            | VmStatus::Paused
            | VmStatus::Suspended
            | VmStatus::Hibernated
    )
}

async fn list_vms(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
) -> Result<Json<Vec<PublicVmRecord>>, ApiError> {
    if let Some(fleet) = &state.fleet {
        let owner_filter = (!identity.is_admin()).then_some(identity.tenant.as_str());
        let vms = fleet
            .list_vms(owner_filter)
            .await
            .map_err(|error| OrchError::Internal(format!("fleet VM list: {error}")))?;
        return Ok(Json(
            vms.into_iter()
                .filter(|vm| vm.status != VmStatus::Stopped)
                .map(PublicVmRecord::from)
                .collect(),
        ));
    }
    let vms = state
        .vm_cache
        .read()
        .map(|c| {
            c.values()
                .filter(|vm| {
                    vm.status != VmStatus::Stopped && identity_can_access_vm(&identity, vm)
                })
                .map(PublicVmRecord::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Json(vms))
}

async fn get_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<PublicVmRecord>, ApiError> {
    match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            Ok(Json(PublicVmRecord::from(vm)))
        }
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let vm = tokio::task::spawn_blocking(move || peer.get_remote(&rpc, id, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
            Ok(Json(PublicVmRecord::from(vm)))
        }
    }
}

/// Sanitized live status routed to the owning node. Host paths and device
/// configuration remain private to the VMM/control-plane boundary.
async fn vm_status(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<PublicVmRuntimeStatus>, ApiError> {
    let status = match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            ops::status_local(&state, id).await?
        }
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            tokio::task::spawn_blocking(move || peer.status_remote(&rpc, id, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??
        }
    };
    Ok(Json(public_vm_runtime_status(status)?))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BalloonTargetRequest {
    pub target_mib: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct PublicBalloonState {
    pub target_mib: u64,
    pub actual_mib: u64,
    pub target_pages: u32,
    pub actual_pages: u32,
}

pub(crate) fn public_balloon(values: (u64, u64, u32, u32)) -> PublicBalloonState {
    PublicBalloonState {
        target_mib: values.0,
        actual_mib: values.1,
        target_pages: values.2,
        actual_pages: values.3,
    }
}

async fn get_balloon(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<PublicBalloonState>, ApiError> {
    let balloon = match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            public_balloon(ops::balloon_local(&state, id).await?)
        }
        Owner::Remote(target) => {
            let peer = Arc::clone(&state.peer);
            tokio::task::spawn_blocking(move || peer.balloon_remote(&target, id, &identity))
                .await
                .map_err(|error| OrchError::Internal(format!("balloon join: {error}")))??
        }
    };
    Ok(Json(balloon))
}

async fn set_balloon(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
    Json(request): Json<BalloonTargetRequest>,
) -> Result<Json<PublicBalloonState>, ApiError> {
    let balloon = match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            public_balloon(ops::set_balloon_local(&state, id, request.target_mib).await?)
        }
        Owner::Remote(target) => {
            let peer = Arc::clone(&state.peer);
            let peer_identity = identity.clone();
            tokio::task::spawn_blocking(move || {
                peer.set_balloon_remote(&target, id, request.target_mib, &peer_identity)
            })
            .await
            .map_err(|error| OrchError::Internal(format!("balloon join: {error}")))??
        }
    };
    audit::record(
        &state,
        &identity,
        audit_action::SET_BALLOON,
        Some(id),
        audit_outcome::OK,
        Some(format!("target_mib={}", request.target_mib)),
    );
    Ok(Json(balloon))
}

async fn delete_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            ops::stop_local(&state, id).await?
        }
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || peer.stop_remote(&rpc, id, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
        }
    }
    audit::record(
        &state,
        &identity,
        audit_action::DELETE,
        Some(id),
        audit_outcome::OK,
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn pause_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<PublicVmRecord>, ApiError> {
    let vm = match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            ops::pause_local(&state, id).await?
        }
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || peer.pause_remote(&rpc, id, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??
        }
    };
    audit::record(
        &state,
        &identity,
        audit_action::PAUSE,
        Some(id),
        audit_outcome::OK,
        None,
    );
    Ok(Json(PublicVmRecord::from(vm)))
}

async fn suspend_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<PublicVmRecord>, ApiError> {
    let vm = match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            ops::suspend_local(&state, id).await?
        }
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || peer.suspend_remote(&rpc, id, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??
        }
    };
    audit::record(
        &state,
        &identity,
        audit_action::SUSPEND,
        Some(id),
        audit_outcome::OK,
        None,
    );
    Ok(Json(PublicVmRecord::from(vm)))
}

async fn hibernate_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<PublicVmRecord>, ApiError> {
    let vm = match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            ops::hibernate_local(&state, id, &identity).await?
        }
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || peer.hibernate_remote(&rpc, id, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??
        }
    };
    audit::record(
        &state,
        &identity,
        audit_action::HIBERNATE,
        Some(id),
        audit_outcome::OK,
        None,
    );
    Ok(Json(PublicVmRecord::from(vm)))
}

async fn resume_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<PublicVmRecord>, ApiError> {
    let owner = cluster::resolve_owner(&state, id).await;
    let vm = match owner {
        Err(OrchError::Unavailable(_)) => {
            ops::recover_hibernated_on_stale_owner(&state, id, &identity).await?
        }
        Err(error) => return Err(error.into()),
        Ok(Owner::Local) => match ops::get_local(&state, id) {
            Ok(vm) => {
                ensure_vm_access(&identity, &vm)?;
                ops::resume_local(&state, id).await?
            }
            Err(OrchError::NotFound(_)) => {
                ops::recover_hibernated_on_stale_owner(&state, id, &identity).await?
            }
            Err(error) => return Err(error.into()),
        },
        Ok(Owner::Remote(rpc)) => {
            let peer = Arc::clone(&state.peer);
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || peer.resume_remote(&rpc, id, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??
        }
    };
    audit::record(
        &state,
        &identity,
        audit_action::RESUME,
        Some(id),
        audit_outcome::OK,
        None,
    );
    Ok(Json(PublicVmRecord::from(vm)))
}

async fn snapshot_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<SnapshotRequest>,
) -> Result<Json<SnapshotResponse>, ApiError> {
    let out = match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            let path = ops::snapshot_local(&state, id, body.diff).await?;
            let snapshot = state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
                .get_snapshot(&path)
                .map_err(store_err)?
                .ok_or_else(|| OrchError::Internal("snapshot publication missing".into()))?;
            SnapshotResponse {
                snapshot_id: snapshot.snapshot_id,
            }
        }
        Owner::Remote(rpc) => {
            let diff = body.diff;
            let peer = Arc::clone(&state.peer);
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || peer.snapshot_remote(&rpc, id, diff, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??
        }
    };
    audit::record(
        &state,
        &identity,
        audit_action::SNAPSHOT,
        Some(id),
        audit_outcome::OK,
        Some(format!("diff={}", body.diff)),
    );
    Ok(Json(out))
}

async fn fork_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(source_id): Path<Uuid>,
    Json(mut body): Json<ForkVmRequest>,
) -> Result<(StatusCode, Json<ForkVmResponse>), ApiError> {
    let owner = cluster::resolve_owner(&state, source_id).await?;
    let (source, remote_source) = match &owner {
        Owner::Local => {
            let source = ops::get_local(&state, source_id)?;
            ensure_vm_access(&identity, &source)?;
            (source, None)
        }
        Owner::Remote(rpc) => {
            if state.fleet.is_none() {
                return Err(OrchError::Unavailable(
                    "cross-node fork requires durable fleet storage".into(),
                )
                .into());
            }
            let peer = Arc::clone(&state.peer);
            let target = rpc.clone();
            let request_identity = identity.clone();
            let source = tokio::task::spawn_blocking(move || {
                peer.get_remote(&target, source_id, &request_identity)
            })
            .await
            .map_err(|error| {
                OrchError::Internal(format!("cross-node fork lookup join: {error}"))
            })??;
            (source, Some(rpc.clone()))
        }
    };
    if source.status != VmStatus::Running {
        return Err(OrchError::Conflict(format!(
            "vm {source_id} must be running to use the atomic live-fork path"
        ))
        .into());
    }
    let child_id = *body.id.get_or_insert_with(Uuid::new_v4);
    match cluster::resolve_owner(&state, child_id).await {
        Ok(_) => {
            return Err(OrchError::Conflict(format!("vm {child_id} already exists")).into());
        }
        Err(OrchError::NotFound(_)) => {}
        Err(error) => return Err(error.into()),
    }

    let reserved = reserve_vm_quota(&state, &identity, child_id).await?;
    let result = async {
        let (child, source_host) = if let Some(remote_source) = remote_source {
            // The target contains the source host's current boot session. Every
            // request below is signed for that exact source/target session pair,
            // so an owner restart or stale fleet route fails closed.
            let peer = Arc::clone(&state.peer);
            let target = remote_source.clone();
            let request_identity = identity.clone();
            let snapshot = tokio::task::spawn_blocking(move || {
                peer.snapshot_remote(&target, source_id, false, &request_identity)
            })
            .await
            .map_err(|error| {
                OrchError::Internal(format!("cross-node fork snapshot join: {error}"))
            })??;
            let fleet = state.fleet.as_ref().ok_or_else(|| {
                OrchError::Unavailable("cross-node fork requires durable fleet storage".into())
            })?;
            let artifact = fleet
                .get_artifact(&identity.tenant, snapshot.snapshot_id)
                .await
                .map_err(branch_fleet_err)?;
            let snapshot_path =
                ops::localize_branch_artifact(&state, &artifact, &identity, false).await?;
            let replicated = fleet
                .get_artifact(&identity.tenant, snapshot.snapshot_id)
                .await
                .map_err(branch_fleet_err)?;
            if replicated.status != tarit_types::ArtifactStatus::Available
                || replicated.replication_state != tarit_types::ArtifactReplicationState::Ready
            {
                return Err(OrchError::Unavailable(
                    "cross-node fork snapshot has not satisfied the configured replication policy"
                        .into(),
                ));
            }
            let child = ops::restore_local_from_surviving_artifact(
                &state,
                &snapshot_path,
                Some(child_id),
                Some(identity.tenant.clone()),
                Some(identity.api_key_id.clone()),
                identity.is_admin(),
            )
            .await?;
            (child, Some(remote_source.host_id))
        } else {
            let snapshot_path = ops::snapshot_local(&state, source_id, false).await?;
            let child = ops::restore_local(
                &state,
                &snapshot_path,
                Some(child_id),
                Some(identity.tenant.clone()),
                Some(identity.api_key_id.clone()),
                identity.is_admin(),
            )
            .await?;
            (child, None)
        };
        audit::record(
            &state,
            &identity,
            audit_action::FORK,
            Some(source_id),
            audit_outcome::OK,
            Some(match source_host {
                Some(source_host) => format!(
                    "child_vm_id={child_id};source_host={source_host};target_host={}",
                    state.config.host_id
                ),
                None => format!("child_vm_id={child_id}"),
            }),
        );
        Ok::<_, OrchError>((
            StatusCode::CREATED,
            Json(ForkVmResponse {
                source_vm_id: source_id,
                vm: PublicVmRecord::from(child),
            }),
        ))
    }
    .await;
    if reserved {
        if let Err(error) = release_vm_quota(&state, &identity, child_id).await {
            tracing::warn!(vm = %child_id, tenant = %identity.tenant, %error,
                "failed to release fork quota reservation; TTL cleanup will retry implicitly");
        }
    }
    result.map_err(ApiError::from)
}

async fn authorize_vm_action(
    state: &AppState,
    owner: &Owner,
    vm_id: Uuid,
    identity: &ApiIdentity,
) -> Result<(), ApiError> {
    match owner {
        Owner::Local => {
            let vm = ops::get_local(state, vm_id)?;
            ensure_vm_access(identity, &vm)?;
        }
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let rpc = rpc.clone();
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || peer.get_remote(&rpc, vm_id, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
        }
    }
    Ok(())
}

async fn execute_async(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Json(req): Json<ExecuteRequest>,
) -> Result<(StatusCode, Json<ExecutionRecord>), ApiError> {
    let result = execute_async_impl(state.clone(), identity, req).await;
    if result.is_ok() {
        state.metrics.inc_exec_total();
    }
    result
}

async fn execute_async_impl(
    state: AppState,
    identity: ApiIdentity,
    req: ExecuteRequest,
) -> Result<(StatusCode, Json<ExecutionRecord>), ApiError> {
    // Locate the VM anywhere in the cluster; the exec runs on its owning node.
    // (Resolving here also validates existence for a clean 404.)
    let owner = ops::resolve_owner_for_activation(&state, req.vm_id, &identity).await?;
    authorize_vm_action(&state, &owner, req.vm_id, &identity).await?;

    let now = Utc::now();
    let exec_id = Uuid::new_v4();
    let record = ExecutionRecord {
        id: exec_id,
        vm_id: req.vm_id,
        command: req.command.clone(),
        timeout_ms: req.timeout_ms,
        status: ExecutionStatus::Pending,
        exit_code: None,
        stdout: None,
        stderr: None,
        duration_ms: None,
        error: None,
        created_at: now,
        updated_at: now,
    };

    // Persist Pending before returning 202. In fleet mode this makes operation
    // polling stateless across API nodes; local SQLite remains a recovery cache.
    persist_exec(&state, &record, &identity).await?;

    let state2 = state.clone();
    let command = req.command;
    let timeout_ms = req.timeout_ms;
    let vm_id = req.vm_id;

    tokio::spawn(async move {
        if let Err(error) =
            update_exec_status(&state2, &identity, exec_id, ExecutionStatus::Running, None).await
        {
            tracing::error!(execution = %exec_id, %error, "persist running execution state");
        }

        let result = match owner {
            Owner::Local => ops::exec_local(&state2, vm_id, command.clone(), timeout_ms).await,
            Owner::Remote(rpc) => {
                let peer = Arc::clone(&state2.peer);
                let cmd = command.clone();
                let identity = identity.clone();
                match tokio::task::spawn_blocking(move || {
                    peer.exec_remote(&rpc, vm_id, &cmd, timeout_ms, &identity)
                })
                .await
                {
                    Ok(r) => r,
                    Err(e) => Err(OrchError::Internal(format!("join: {e}"))),
                }
            }
        };

        match result {
            Ok((code, stdout, stderr, duration_ms)) => {
                let rec = ExecutionRecord {
                    id: exec_id,
                    vm_id,
                    command,
                    timeout_ms,
                    status: ExecutionStatus::Completed,
                    exit_code: Some(code),
                    stdout: Some(stdout),
                    stderr: Some(stderr),
                    duration_ms: Some(duration_ms),
                    error: None,
                    created_at: now,
                    updated_at: Utc::now(),
                };
                if let Err(error) = persist_exec(&state2, &rec, &identity).await {
                    tracing::error!(execution = %exec_id, %error, "persist completed execution state");
                }
                usage::meter_exec(
                    &state2,
                    &identity.api_key_id,
                    &identity.tenant,
                    vm_id,
                    duration_ms,
                );
                audit::record(
                    &state2,
                    &identity,
                    audit_action::EXEC,
                    Some(vm_id),
                    audit_outcome::OK,
                    None,
                );
            }
            Err(e) => {
                tracing::warn!(execution = %exec_id, vm = %vm_id, error = %e, "async exec failed");
                audit::record(
                    &state2,
                    &identity,
                    audit_action::EXEC,
                    Some(vm_id),
                    audit_outcome::ERROR,
                    Some(e.to_string()),
                );
                if let Err(persist_error) = update_exec_status(
                    &state2,
                    &identity,
                    exec_id,
                    ExecutionStatus::Failed,
                    Some(public_operation_error(&e)),
                )
                .await
                {
                    tracing::error!(execution = %exec_id, %persist_error,
                        "persist failed execution state");
                }
            }
        }
    });

    Ok((StatusCode::ACCEPTED, Json(record)))
}

/// Synchronous exec: run the command and return the completed record in one
/// request. The ComputeSDK-style path -- no client polling (the 15ms poll of
/// execute_async/get_execution dominates a concurrent burst's tail).
async fn execute(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Json(req): Json<ExecuteRequest>,
) -> Result<Json<ExecutionRecord>, ApiError> {
    let result = execute_impl(&state, &identity, req).await;
    if result.is_ok() {
        state.metrics.inc_exec_total();
    }
    result
}

async fn execute_impl(
    state: &AppState,
    identity: &ApiIdentity,
    req: ExecuteRequest,
) -> Result<Json<ExecutionRecord>, ApiError> {
    let owner = ops::resolve_owner_for_activation(state, req.vm_id, identity).await?;
    authorize_vm_action(state, &owner, req.vm_id, identity).await?;
    let now = Utc::now();
    let exec_id = Uuid::new_v4();
    let vm_id = req.vm_id;
    let command = req.command.clone();
    let timeout_ms = req.timeout_ms;

    let result = match owner {
        Owner::Local => ops::exec_local(state, vm_id, command.clone(), timeout_ms).await,
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let cmd = command.clone();
            let identity = identity.clone();
            match tokio::task::spawn_blocking(move || {
                peer.exec_remote(&rpc, vm_id, &cmd, timeout_ms, &identity)
            })
            .await
            {
                Ok(r) => r,
                Err(e) => Err(OrchError::Internal(format!("join: {e}"))),
            }
        }
    };

    let rec = match result {
        Ok((code, stdout, stderr, duration_ms)) => ExecutionRecord {
            id: exec_id,
            vm_id,
            command,
            timeout_ms,
            status: ExecutionStatus::Completed,
            exit_code: Some(code),
            stdout: Some(stdout),
            stderr: Some(stderr),
            duration_ms: Some(duration_ms),
            error: None,
            created_at: now,
            updated_at: Utc::now(),
        },
        // Precondition rejections (e.g. exec against a suspended VM) are API
        // errors, not execution outcomes: no command ran, so no record.
        Err(e @ OrchError::Conflict(_)) => return Err(e.into()),
        Err(e) => {
            // The record carries only the sanitized message; keep the cause
            // diagnosable server-side.
            tracing::warn!(execution = %exec_id, vm = %vm_id, error = %e, "exec failed");
            ExecutionRecord {
                id: exec_id,
                vm_id,
                command,
                timeout_ms,
                status: ExecutionStatus::Failed,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: Some(public_operation_error(&e)),
                created_at: now,
                updated_at: Utc::now(),
            }
        }
    };
    persist_exec(state, &rec, identity).await?;
    if matches!(rec.status, ExecutionStatus::Completed) {
        usage::meter_exec(
            state,
            &identity.api_key_id,
            &identity.tenant,
            vm_id,
            rec.duration_ms.unwrap_or(0),
        );
        audit::record(
            state,
            identity,
            audit_action::EXEC,
            Some(vm_id),
            audit_outcome::OK,
            None,
        );
    } else {
        audit::record(
            state,
            identity,
            audit_action::EXEC,
            Some(vm_id),
            audit_outcome::ERROR,
            rec.error.clone(),
        );
    }
    Ok(Json(rec))
}

async fn get_execution(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<ExecutionRecord>, ApiError> {
    if let Some(fleet) = &state.fleet {
        let global = fleet
            .get_execution(id)
            .await
            .map_err(|error| OrchError::Internal(format!("fleet execution lookup: {error}")))?
            .ok_or_else(|| OrchError::NotFound(format!("execution {id} not found")))?;
        if !identity.is_admin() && global.owner_key != identity.tenant {
            return Err(OrchError::Forbidden("execution belongs to another tenant".into()).into());
        }
        if let Ok(mut cache) = state.exec_cache.write() {
            cache.insert(id, global.record.clone());
        }
        return Ok(Json(global.record));
    }
    let rec = if let Some(rec) = state
        .exec_cache
        .read()
        .ok()
        .and_then(|c| c.get(&id).cloned())
    {
        rec
    } else {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock".into()))?;
        store.get_execution(id).map_err(store_err)?
    };
    let owner = cluster::resolve_owner(&state, rec.vm_id).await?;
    authorize_vm_action(&state, &owner, rec.vm_id, &identity).await?;
    Ok(Json(rec))
}

async fn update_egress(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<EgressUpdateRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let rule_count = body.allowlist.len();
    let out = match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            let rules = ops::egress_local(&state, id, body.allowlist, body.allow_existing).await?;
            serde_json::json!({ "rules_applied": rules })
        }
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || peer.egress_remote(&rpc, id, &body, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??
        }
    };
    audit::record(
        &state,
        &identity,
        audit_action::UPDATE_EGRESS,
        Some(id),
        audit_outcome::OK,
        Some(format!("rules={rule_count}")),
    );
    Ok(Json(out))
}

async fn get_egress_policy(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<EgressPolicyRecord>, ApiError> {
    let policy = match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            let owner = vm.owner_key.ok_or_else(|| {
                OrchError::BadRequest("admin-owned VM has no durable tenant policy".into())
            })?;
            ops::get_egress_policy_local(&state, id, &owner)?
        }
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || peer.get_egress_policy_remote(&rpc, id, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??
        }
    };
    Ok(Json(policy))
}

async fn put_egress_policy(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<PutEgressPolicyRequest>,
) -> Result<Json<EgressPolicyRecord>, ApiError> {
    let rule_count = body.allowlist.len();
    let policy = match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            let owner = vm.owner_key.ok_or_else(|| {
                OrchError::BadRequest("admin-owned VM has no durable tenant policy".into())
            })?;
            ops::put_egress_policy_local(
                &state,
                id,
                &owner,
                body.expected_revision,
                body.allowlist,
                body.allow_existing,
            )
            .await?
        }
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || {
                peer.put_egress_policy_remote(&rpc, id, &body, &identity)
            })
            .await
            .map_err(|e| OrchError::Internal(format!("join: {e}")))??
        }
    };
    audit::record(
        &state,
        &identity,
        audit_action::UPDATE_EGRESS,
        Some(id),
        audit_outcome::OK,
        Some(format!("rules={rule_count};revision={}", policy.revision)),
    );
    Ok(Json(policy))
}

async fn create_branch(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Json(request): Json<CreateBranchRequest>,
) -> Result<(StatusCode, Json<BranchRecord>), ApiError> {
    request.validate()?;
    let branch_id = request.branch_id.unwrap_or_else(Uuid::new_v4);
    let now = Utc::now();
    let proposed = BranchRecord {
        branch_id,
        owner_key: identity.tenant.clone(),
        name: request.name,
        head_artifact_id: request.head_artifact_id,
        source_vm_id: request.source_vm_id,
        source_branch_id: request.source_branch_id,
        revision: 1,
        created_at: now,
        updated_at: now,
    };
    let (status, branch) = if let Some(fleet) = state.fleet.as_ref() {
        ensure_artifact_replication_ready(&state, &identity, proposed.head_artifact_id).await?;
        let existed = match fleet.get_branch(&identity.tenant, branch_id).await {
            Ok(_) => true,
            Err(tarit_fleet::FleetError::NotFound) => false,
            Err(error) => return Err(branch_fleet_err(error).into()),
        };
        let branch = fleet
            .insert_branch(&proposed)
            .await
            .map_err(branch_fleet_err)?;
        (
            if existed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            },
            branch,
        )
    } else {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?;
        let existing = store.get_branch(&identity.tenant, branch_id).ok();
        store.insert_branch(&proposed).map_err(store_err)?;
        let branch = store
            .get_branch(&identity.tenant, branch_id)
            .map_err(store_err)?;
        (
            if existing.is_some() {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            },
            branch,
        )
    };
    audit::record(
        &state,
        &identity,
        audit_action::CREATE_BRANCH,
        None,
        audit_outcome::OK,
        Some(format!("branch_id={branch_id}")),
    );
    Ok((status, Json(branch)))
}

pub(crate) async fn ensure_artifact_replication_ready(
    state: &AppState,
    identity: &ApiIdentity,
    artifact_id: Uuid,
) -> Result<(), OrchError> {
    let Some(fleet) = state.fleet.as_ref() else {
        return Ok(());
    };
    let artifact = fleet
        .get_artifact(&identity.tenant, artifact_id)
        .await
        .map_err(branch_fleet_err)?;
    if artifact.status == tarit_types::ArtifactStatus::Available
        && artifact.replication_state == tarit_types::ArtifactReplicationState::Ready
    {
        return Ok(());
    }
    let candidates = cluster::place_candidates(state, 1, 1).await;
    let mut failures = Vec::new();
    let tenant = identity.tenant.clone();
    for candidate in candidates {
        let peer = Arc::clone(&state.peer);
        let request_identity = identity.clone();
        let attempt = candidate.clone();
        let result = tokio::task::spawn_blocking(move || {
            peer.localize_artifact_remote(&attempt, artifact_id, &request_identity)
        })
        .await
        .map_err(|error| OrchError::Internal(format!("artifact localization join: {error}")))?;
        match result {
            Ok(()) => {
                let refreshed = fleet
                    .get_artifact(&tenant, artifact_id)
                    .await
                    .map_err(branch_fleet_err)?;
                if refreshed.status == tarit_types::ArtifactStatus::Available
                    && refreshed.replication_state == tarit_types::ArtifactReplicationState::Ready
                {
                    return Ok(());
                }
            }
            Err(error) => failures.push(format!("{}: {error}", candidate.host_id)),
        }
    }
    tracing::warn!(
        artifact = %artifact_id,
        attempted = failures.len(),
        "artifact did not satisfy replication policy: {}",
        failures.join("; ")
    );
    Err(OrchError::Unavailable(
        "artifact has not satisfied the configured replication policy".into(),
    ))
}

async fn list_branches(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
) -> Result<Json<Vec<BranchRecord>>, ApiError> {
    let branches = if let Some(fleet) = state.fleet.as_ref() {
        fleet
            .list_branches(&identity.tenant)
            .await
            .map_err(branch_fleet_err)?
    } else {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .list_branches(&identity.tenant)
            .map_err(store_err)?
    };
    Ok(Json(branches))
}

async fn get_branch(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<BranchRecord>, ApiError> {
    let branch = if let Some(fleet) = state.fleet.as_ref() {
        fleet
            .get_branch(&identity.tenant, id)
            .await
            .map_err(branch_fleet_err)?
    } else {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .get_branch(&identity.tenant, id)
            .map_err(store_err)?
    };
    Ok(Json(branch))
}

async fn update_branch_head(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
    Json(request): Json<UpdateBranchHeadRequest>,
) -> Result<Json<BranchRecord>, ApiError> {
    if request.expected_revision == 0 {
        return Err(OrchError::BadRequest("expected_revision must be positive".into()).into());
    }
    let branch = if let Some(fleet) = state.fleet.as_ref() {
        fleet
            .update_branch_head(
                &identity.tenant,
                id,
                request.expected_revision,
                request.head_artifact_id,
                Utc::now(),
            )
            .await
            .map_err(branch_fleet_err)?
    } else {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .update_branch_head(
                &identity.tenant,
                id,
                request.expected_revision,
                request.head_artifact_id,
                Utc::now(),
            )
            .map_err(store_err)?
    };
    audit::record(
        &state,
        &identity,
        audit_action::UPDATE_BRANCH,
        None,
        audit_outcome::OK,
        Some(format!("branch_id={id};revision={}", branch.revision)),
    );
    Ok(Json(branch))
}

async fn delete_branch(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if let Some(fleet) = state.fleet.as_ref() {
        fleet
            .delete_branch(&identity.tenant, id)
            .await
            .map_err(branch_fleet_err)?;
    } else {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .delete_branch(&identity.tenant, id)
            .map_err(store_err)?;
    }
    audit::record(
        &state,
        &identity,
        audit_action::DELETE_BRANCH,
        None,
        audit_outcome::OK,
        Some(format!("branch_id={id}")),
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn restore_branch(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(branch_id): Path<Uuid>,
    Json(request): Json<RestoreBranchRequest>,
) -> Result<(StatusCode, Json<PublicVmRecord>), ApiError> {
    let (branch, artifact) = if let Some(fleet) = state.fleet.as_ref() {
        let branch = fleet
            .get_branch(&identity.tenant, branch_id)
            .await
            .map_err(branch_fleet_err)?;
        let artifact = fleet
            .get_artifact(&identity.tenant, branch.head_artifact_id)
            .await
            .map_err(branch_fleet_err)?;
        (branch, artifact)
    } else {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?;
        let branch = store
            .get_branch(&identity.tenant, branch_id)
            .map_err(store_err)?;
        let artifact = store
            .get_artifact(&identity.tenant, branch.head_artifact_id)
            .map_err(store_err)?;
        (branch, artifact)
    };
    if artifact.status != tarit_types::ArtifactStatus::Available {
        return Err(OrchError::Unavailable("branch head artifact is unavailable".into()).into());
    }
    let child_id = request.id.unwrap_or_else(Uuid::new_v4);
    match cluster::resolve_owner(&state, child_id).await {
        Ok(_) => return Err(OrchError::Conflict(format!("vm {child_id} already exists")).into()),
        Err(OrchError::NotFound(_)) => {}
        Err(error) => return Err(error.into()),
    }
    let reserved = reserve_vm_quota(&state, &identity, child_id).await?;
    let result = async {
        let snapshot_path =
            ops::localize_branch_artifact(&state, &artifact, &identity, false).await?;
        let child = ops::restore_local_from_surviving_artifact(
            &state,
            &snapshot_path,
            Some(child_id),
            Some(identity.tenant.clone()),
            Some(identity.api_key_id.clone()),
            identity.is_admin(),
        )
        .await?;
        audit::record(
            &state,
            &identity,
            audit_action::RESTORE,
            Some(child_id),
            audit_outcome::OK,
            Some(format!(
                "branch_id={branch_id};head_artifact_id={}",
                branch.head_artifact_id
            )),
        );
        Ok::<_, OrchError>((StatusCode::CREATED, Json(PublicVmRecord::from(child))))
    }
    .await;
    if reserved {
        if let Err(error) = release_vm_quota(&state, &identity, child_id).await {
            tracing::warn!(vm = %child_id, tenant = %identity.tenant, %error,
                "failed to release branch-restore quota reservation; TTL cleanup will retry");
        }
    }
    result.map_err(ApiError::from)
}

fn branch_fleet_err(error: tarit_fleet::FleetError) -> OrchError {
    match error {
        tarit_fleet::FleetError::NotFound => {
            OrchError::NotFound("branch or artifact not found".into())
        }
        tarit_fleet::FleetError::Conflict(message) => OrchError::Conflict(message),
        error => OrchError::Internal(format!("branch store: {error}")),
    }
}

/// Cluster-wide capacity + health view. Serves as both an observability
/// endpoint and the signal an autoscaler consumes to decide scale-out/in.
async fn cluster_status(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&identity)?;
    let hosts: Vec<tarit_store::HostRecord> = if let Some(fleet) = &state.fleet {
        fleet
            .list_hosts()
            .await
            .map_err(|e| OrchError::Internal(format!("fleet list_hosts: {e}")))?
    } else {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock".into()))?;
        store.list_hosts().map_err(store_err)?
    };

    let now = chrono::Utc::now();
    let mut healthy_nodes = 0usize;
    let mut free_vcpus = 0u64;
    let mut free_mem = 0u64;
    let nodes: Vec<_> = hosts
        .iter()
        .map(|h| {
            let fresh = (now - h.last_heartbeat)
                .to_std()
                .map(|d| d < std::time::Duration::from_secs(15))
                .unwrap_or(false);
            let up = h.healthy && fresh;
            if up {
                healthy_nodes += 1;
                free_vcpus += h.free_vcpus;
                free_mem += h.free_memory_mib;
            }

            serde_json::json!({
                "host_id": h.host_id,
                "rpc_addr": h.rpc_addr,
                "sandbox_count": h.sandbox_count,
                "free_vcpus": h.free_vcpus,
                "free_memory_mib": h.free_memory_mib,
                "up": up,
                "last_heartbeat": h.last_heartbeat,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "this_host": state.config.host_id,
        "clustered": state.fleet.is_some(),
        "total_nodes": hosts.len(),
        "healthy_nodes": healthy_nodes,
        "cluster_free_vcpus": free_vcpus,
        "cluster_free_memory_mib": free_mem,
        "disk_pressure": state.supervisor.disk_pressure_snapshot(),
        "nodes": nodes,
    })))
}

async fn warm_pool_status(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&identity)?;
    let classes = state
        .config
        .warm_pool
        .classes
        .iter()
        .map(|class| {
            let spawn = VmSpawnConfig::from_warm_class(&state.config, class);
            let depth = state.supervisor.warm_count(&spawn);
            serde_json::json!({
                "vcpus": class.vcpus,
                "memory_mib": class.memory_mib,
                "hard_floor": class.hard_floor,
                "low_watermark": class.low_watermark,
                "target": class.target,
                "high_watermark": class.high_watermark,
                "restore": class.restore,
                "rootfs": class.rootfs.as_ref().map(|path| path.display().to_string()),
                "image": class.image,
                "kernel": spawn.kernel_path,
                "cmdline": spawn.cmdline,
                "read_only": spawn.read_only,
                "depth": depth,
                "refill_needed": class.refill_needed(depth),
            })
        })
        .collect::<Vec<_>>();
    let disk_pressure = state.supervisor.disk_pressure_snapshot();
    Ok(Json(serde_json::json!({
        "enabled": state.config.warm_pool.enabled,
        "cpu_overcommit": state.config.warm_pool.cpu_overcommit,
        "replenish_concurrency": state.config.warm_pool.replenish_concurrency,
        "total_target": state.config.warm_pool.total_target(),
        "disk_pressure": {
            "active": disk_pressure.pressured,
            "used_bytes": disk_pressure.used_bytes,
            "used_inodes": disk_pressure.used_inodes,
            "reserved_bytes": disk_pressure.reserved_bytes,
            "reserved_inodes": disk_pressure.reserved_inodes,
            "roots": disk_pressure.roots,
            "last_removed_files": disk_pressure.last_removed_files,
            "last_removed_jails": disk_pressure.last_removed_jails,
        },
        "classes": classes,
    })))
}

#[derive(serde::Deserialize)]
struct UsageQuery {
    from: Option<chrono::DateTime<Utc>>,
    to: Option<chrono::DateTime<Utc>>,
    api_key_id: Option<String>,
}

/// Aggregated usage stats per API key, read from the primary store. Admins see
/// every key (optionally filtered by `api_key_id`); a non-admin key sees only
/// its own. This is raw metering, not billing; a layer above interprets it.
async fn usage_stats(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<Vec<UsageSummary>>, ApiError> {
    let fleet = state.fleet.as_ref().ok_or_else(|| {
        OrchError::Internal("usage stats require a fleet store (TARIT_DATABASE_URL)".into())
    })?;
    let to = q.to.unwrap_or_else(Utc::now);
    let from = q.from.unwrap_or_else(|| to - chrono::Duration::days(30));
    let key_filter = if identity.is_admin() {
        q.api_key_id.as_deref()
    } else {
        Some(identity.api_key_id.as_str())
    };
    let out = fleet
        .usage_summary(key_filter, from, to)
        .await
        .map_err(|e| OrchError::Internal(format!("usage summary: {e}")))?;
    Ok(Json(out))
}

#[derive(serde::Deserialize)]
struct AuditQuery {
    api_key_id: Option<String>,
    vm_id: Option<Uuid>,
    limit: Option<i64>,
}

/// Recent audit trail from the primary store, newest first. Admins see every
/// key; a non-admin key sees only its own actions.
async fn audit_log(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Vec<AuditEvent>>, ApiError> {
    let fleet = state.fleet.as_ref().ok_or_else(|| {
        OrchError::Internal("audit log requires a fleet store (TARIT_DATABASE_URL)".into())
    })?;
    let key_filter = if identity.is_admin() {
        q.api_key_id.as_deref()
    } else {
        Some(identity.api_key_id.as_str())
    };
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let out = fleet
        .list_audit(key_filter, q.vm_id, limit)
        .await
        .map_err(|e| OrchError::Internal(format!("audit list: {e}")))?;
    Ok(Json(out))
}

pub(crate) fn store_err(e: tarit_store::StoreError) -> OrchError {
    match e {
        tarit_store::StoreError::NotFound => OrchError::NotFound("record not found".into()),
        tarit_store::StoreError::Conflict(message) => OrchError::Conflict(message),
        tarit_store::StoreError::Sqlite(e) => OrchError::Internal(e.to_string()),
    }
}

/// Write an execution record through to the global operation store, local
/// in-memory cache, and bounded SQLite write-behind queue. Queue saturation uses
/// a synchronous SQLite fallback: overload may add latency, but never silently
/// loses terminal operation state.
async fn persist_exec(
    state: &AppState,
    rec: &ExecutionRecord,
    identity: &ApiIdentity,
) -> Result<(), OrchError> {
    if let Some(fleet) = &state.fleet {
        fleet
            .upsert_execution(
                rec,
                &identity.tenant,
                &identity.api_key_id,
                &state.config.host_id,
            )
            .await
            .map_err(|error| {
                OrchError::Internal(format!("persist global execution state: {error}"))
            })?;
    }
    if let Ok(mut c) = state.exec_cache.write() {
        c.insert(rec.id, rec.clone());
    }
    if let Err(error) = state.store_tx.try_send(StoreWrite::Exec(rec.clone())) {
        state.metrics.inc_store_enqueue_failure();
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .insert_execution(rec)
            .map_err(store_err)?;
        tracing::warn!(execution = %rec.id, %error,
            "bounded persistence queue unavailable; wrote execution synchronously");
    }
    Ok(())
}

async fn update_exec_status(
    state: &AppState,
    identity: &ApiIdentity,
    id: Uuid,
    status: ExecutionStatus,
    error: Option<String>,
) -> Result<(), OrchError> {
    let mut rec = match state
        .exec_cache
        .read()
        .ok()
        .and_then(|c| c.get(&id).cloned())
    {
        Some(r) => r,
        None => {
            let store = state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock".into()))?;
            store.get_execution(id).map_err(store_err)?
        }
    };
    rec.status = status;
    rec.error = error;
    rec.updated_at = Utc::now();
    persist_exec(state, &rec, identity).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiIdentity, ApiKeyRegistry, ApiRole, AutoscaleConfig, WarmPoolConfig};
    use crate::metrics::Metrics;
    use crate::peer::PeerClient;
    use crate::pty::PtyRegistry;
    use crate::scheduler::Scheduler;
    use crate::supervisor::VmmSupervisor;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Condvar, Mutex, RwLock,
    };
    use std::time::Duration;
    use tarit_store::Store;
    use tower::ServiceExt;

    #[test]
    fn overloaded_response_includes_retry_after() {
        let response = ApiError(OrchError::Overloaded {
            message: "cluster at capacity".into(),
            retry_after_secs: 7,
        })
        .into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("7")
        );
    }

    #[test]
    fn store_conflict_maps_to_http_conflict() {
        let response = ApiError(store_err(tarit_store::StoreError::Conflict(
            "share slug already exists".into(),
        )))
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn branch_api_is_idempotent_tenant_scoped_cas_and_reference_safe() {
        fn artifact(
            owner: &str,
            state: tarit_types::ArtifactReplicationState,
        ) -> tarit_types::ArtifactRecord {
            let now = Utc::now();
            tarit_types::ArtifactRecord {
                artifact_id: Uuid::new_v4(),
                owner_key: owner.into(),
                host_id: "test-host".into(),
                storage_locator: format!("/private/{}", Uuid::new_v4()),
                kind: tarit_types::ArtifactKind::VmSnapshot,
                status: tarit_types::ArtifactStatus::Available,
                content_digest: format!("sha256:{}", "1".repeat(64)),
                size_bytes: 4096,
                immutable_image_digest: format!("sha256:{}", "2".repeat(64)),
                agent_digest: format!("sha256:{}", "3".repeat(64)),
                boot_manifest_digest: format!("sha256:{}", "5".repeat(64)),
                parent_artifact_id: None,
                source_vm_id: None,
                creation_revision: 1,
                integrity_manifest_digest: format!("sha256:{}", "4".repeat(64)),
                chunk_size_bytes: 65536,
                chunk_count: 1,
                replication_state: state,
                reference_count: 0,
                created_at: now,
                updated_at: now,
            }
        }

        let state = test_state();
        let first = artifact("tenant-a", tarit_types::ArtifactReplicationState::Ready);
        let second = artifact("tenant-a", tarit_types::ArtifactReplicationState::Ready);
        let pending = artifact("tenant-a", tarit_types::ArtifactReplicationState::Pending);
        let foreign = artifact("tenant-b", tarit_types::ArtifactReplicationState::Ready);
        {
            let store = state.store.lock().unwrap();
            for artifact in [&first, &second, &pending, &foreign] {
                store.insert_artifact(artifact).unwrap();
            }
        }
        let branch_id = Uuid::new_v4();
        let body = serde_json::json!({
            "branch_id": branch_id,
            "name": "main",
            "head_artifact_id": first.artifact_id,
        });
        let runtime = test_runtime();
        let app = router(state.clone());
        let created = runtime.block_on(request_json(
            app.clone(),
            "POST",
            "/v1/branches",
            "tenant-a-key",
            body.clone(),
        ));
        assert_eq!(created.status(), StatusCode::CREATED);
        let created = runtime.block_on(response_json(created));
        assert_eq!(created["branch_id"], branch_id.to_string());
        assert!(created.get("owner_key").is_none());

        let replay = runtime.block_on(request_json(
            app.clone(),
            "POST",
            "/v1/branches",
            "tenant-a-key",
            body,
        ));
        assert_eq!(replay.status(), StatusCode::OK);
        assert_eq!(
            state
                .store
                .lock()
                .unwrap()
                .get_artifact("tenant-a", first.artifact_id)
                .unwrap()
                .reference_count,
            1
        );

        let foreign_get = runtime.block_on(request_json(
            app.clone(),
            "GET",
            &format!("/v1/branches/{branch_id}"),
            "tenant-b-key",
            serde_json::json!({}),
        ));
        assert_eq!(foreign_get.status(), StatusCode::NOT_FOUND);

        for (artifact_id, revision, expected) in [
            (foreign.artifact_id, 1, StatusCode::NOT_FOUND),
            (pending.artifact_id, 1, StatusCode::NOT_FOUND),
            (second.artifact_id, 0, StatusCode::BAD_REQUEST),
            (second.artifact_id, 1, StatusCode::OK),
            (first.artifact_id, 1, StatusCode::CONFLICT),
        ] {
            let response = runtime.block_on(request_json(
                app.clone(),
                "PUT",
                &format!("/v1/branches/{branch_id}"),
                "tenant-a-key",
                serde_json::json!({
                    "expected_revision": revision,
                    "head_artifact_id": artifact_id,
                }),
            ));
            assert_eq!(response.status(), expected);
        }
        assert_eq!(
            state
                .store
                .lock()
                .unwrap()
                .get_artifact("tenant-a", first.artifact_id)
                .unwrap()
                .reference_count,
            0
        );
        assert_eq!(
            state
                .store
                .lock()
                .unwrap()
                .get_artifact("tenant-a", second.artifact_id)
                .unwrap()
                .reference_count,
            1
        );

        let deleted = runtime.block_on(request_json(
            app,
            "DELETE",
            &format!("/v1/branches/{branch_id}"),
            "tenant-a-key",
            serde_json::json!({}),
        ));
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            state
                .store
                .lock()
                .unwrap()
                .get_artifact("tenant-a", second.artifact_id)
                .unwrap()
                .reference_count,
            0
        );
    }

    #[test]
    fn share_request_json_content_type_accepts_case_insensitive_media_type() {
        let request = Request::builder()
            .header(header::CONTENT_TYPE, "Application/JSON; charset=utf-8")
            .body(Body::from(r#"{"guest_port":8080}"#))
            .unwrap();
        let rt = test_runtime();

        let parsed = rt.block_on(parse_share_json::<serde_json::Value>(request));

        assert_eq!(parsed.ok(), Some(serde_json::json!({"guest_port": 8080})));
        drop(rt);
    }

    #[test]
    fn share_request_json_content_type_rejects_near_prefix_media_type() {
        let request = Request::builder()
            .header(header::CONTENT_TYPE, "application/jsonp; charset=utf-8")
            .body(Body::from(r#"{"guest_port":8080}"#))
            .unwrap();
        let rt = test_runtime();

        let parsed = rt.block_on(parse_share_json::<serde_json::Value>(request));

        assert!(parsed.is_err());
        drop(rt);
    }

    #[test]
    fn volume_api_is_idempotent_tenant_scoped_private_and_physically_deletes() {
        let state = test_state();
        let app = router(state.clone());
        let runtime = test_runtime();
        let id = Uuid::new_v4();
        let body = serde_json::json!({
            "id": id,
            "name": format!("workspace-{}", id.simple()),
            "size_bytes": 4 * 1024 * 1024,
            "provider": "local_block",
        });

        let created = runtime.block_on(request_json(
            app.clone(),
            "POST",
            "/v1/volumes",
            "tenant-a-key",
            body.clone(),
        ));
        let created_status = created.status();
        let created = runtime.block_on(response_json(created));
        assert_eq!(created_status, StatusCode::CREATED, "{created}");
        assert_eq!(created["id"], id.to_string());
        assert_eq!(created["status"], "available");
        assert_eq!(created["storage_class"], "block");
        for private in ["owner_key", "host_id", "last_error", "private_path"] {
            assert!(
                created.get(private).is_none(),
                "public volume leaked {private}"
            );
        }

        let replay = runtime.block_on(request_json(
            app.clone(),
            "POST",
            "/v1/volumes",
            "tenant-a-key",
            body,
        ));
        assert_eq!(replay.status(), StatusCode::OK);

        let foreign = runtime.block_on(request_json(
            app.clone(),
            "GET",
            &format!("/v1/volumes/{id}"),
            "tenant-b-key",
            serde_json::json!({}),
        ));
        assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
        let foreign_delete = runtime.block_on(request_json(
            app.clone(),
            "DELETE",
            &format!("/v1/volumes/{id}"),
            "tenant-b-key",
            serde_json::json!({}),
        ));
        assert_eq!(foreign_delete.status(), StatusCode::NOT_FOUND);

        let private_path = state
            .config
            .images_dir
            .join("volumes")
            .join(format!("{id}.block"));
        assert!(private_path.is_file());
        let deleted = runtime.block_on(request_json(
            app.clone(),
            "DELETE",
            &format!("/v1/volumes/{id}"),
            "tenant-a-key",
            serde_json::json!({}),
        ));
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        assert!(!private_path.exists());
        assert!(matches!(
            state.store.lock().unwrap().get_volume("tenant-a", id),
            Err(tarit_store::StoreError::NotFound)
        ));

        let invalid_id = Uuid::new_v4();
        let invalid = runtime.block_on(request_json(
            app,
            "POST",
            "/v1/volumes",
            "tenant-a-key",
            serde_json::json!({
                "id": invalid_id,
                "name": "too-small",
                "size_bytes": 4096,
            }),
        ));
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert!(matches!(
            state
                .store
                .lock()
                .unwrap()
                .get_volume("tenant-a", invalid_id),
            Err(tarit_store::StoreError::NotFound)
        ));
    }

    #[test]
    fn unknown_api_key_returns_401() {
        let app = router(test_state());
        let rt = test_runtime();
        let response = rt
            .block_on(
                app.clone().oneshot(
                    Request::builder()
                        .uri("/v1/vms")
                        .header("X-API-Key", "unknown")
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
            .unwrap();
        drop(rt);

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn live_fork_rejects_foreign_and_nonrunning_sources_before_vmm_access() {
        let state = test_state();
        let foreign = Uuid::new_v4();
        let paused = Uuid::new_v4();
        insert_vm(&state, foreign, "tenant-b", VmStatus::Running);
        insert_vm(&state, paused, "tenant-a", VmStatus::Paused);
        let app = router(state);
        let rt = test_runtime();

        let foreign_response = rt.block_on(request_json(
            app.clone(),
            "POST",
            &format!("/v1/vms/{foreign}/fork"),
            "tenant-a-key",
            serde_json::json!({}),
        ));
        assert_eq!(foreign_response.status(), StatusCode::FORBIDDEN);

        let paused_response = rt.block_on(request_json(
            app,
            "POST",
            &format!("/v1/vms/{paused}/fork"),
            "tenant-a-key",
            serde_json::json!({}),
        ));
        assert_eq!(paused_response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn unknown_route_returns_404_without_auth() {
        let app = router(test_state());
        let rt = test_runtime();

        // An unauthenticated request to an unknown path (e.g. a share host
        // pointed at the control listener) must not be dispatched and must not
        // require a credential to receive a not-found answer.
        let unknown = rt
            .block_on(
                app.clone().oneshot(
                    Request::builder()
                        .uri("/")
                        .header(header::HOST, "some-share.shares.example.test")
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

        // A known protected route without a credential still requires auth.
        let protected = rt
            .block_on(
                app.clone().oneshot(
                    Request::builder()
                        .uri("/v1/vms")
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
            .unwrap();
        assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);
        drop(rt);
    }

    #[test]
    fn user_lists_only_own_tenant_vms_admin_lists_all() {
        let state = test_state();
        let tenant_a_id = Uuid::new_v4();
        insert_vm(&state, tenant_a_id, "tenant-a", VmStatus::Running);
        insert_vm(&state, Uuid::new_v4(), "tenant-b", VmStatus::Running);
        let app = router(state);
        let rt = test_runtime();

        let user_response = rt
            .block_on(
                app.clone().oneshot(
                    Request::builder()
                        .uri("/v1/vms")
                        .header("X-API-Key", "tenant-a-key")
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
            .unwrap();
        assert_eq!(user_response.status(), StatusCode::OK);
        let body = rt
            .block_on(to_bytes(user_response.into_body(), usize::MAX))
            .unwrap();
        let user_vms: Vec<PublicVmRecord> = serde_json::from_slice(&body).unwrap();
        assert_eq!(user_vms.len(), 1);
        assert_eq!(user_vms[0].id, tenant_a_id);
        let user_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        for internal_field in [
            "host_id",
            "owner_key",
            "api_key_id",
            "kernel_path",
            "rootfs_path",
            "cmdline",
            "socket_path",
            "pid",
        ] {
            assert!(
                user_json[0].get(internal_field).is_none(),
                "public VM response leaked {internal_field}"
            );
        }

        let admin_response = rt
            .block_on(
                app.clone().oneshot(
                    Request::builder()
                        .uri("/v1/vms")
                        .header("X-API-Key", "admin-key")
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
            .unwrap();
        assert_eq!(admin_response.status(), StatusCode::OK);
        let body = rt
            .block_on(to_bytes(admin_response.into_body(), usize::MAX))
            .unwrap();
        let admin_vms: Vec<PublicVmRecord> = serde_json::from_slice(&body).unwrap();
        assert_eq!(admin_vms.len(), 2);
        drop(rt);
    }

    #[test]
    fn unauthorized_requests_are_subject_to_global_rate_limit() {
        let mut state = test_state();
        state.config.api_requests_per_second = 1;
        let app = router(state);
        let rt = test_runtime();

        let first = rt
            .block_on(
                app.clone().oneshot(
                    Request::builder()
                        .uri("/v1/vms")
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
            .unwrap();
        assert_eq!(first.status(), StatusCode::UNAUTHORIZED);

        let second = rt
            .block_on(
                app.oneshot(
                    Request::builder()
                        .uri("/v1/vms")
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        drop(rt);
    }

    #[test]
    fn invalid_pty_connect_tokens_are_subject_to_global_rate_limit() {
        let mut state = test_state();
        state.config.api_requests_per_second = 1;
        let app = router(state);
        let rt = test_runtime();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await });
            let uri = format!(
                "ws://{address}/v1/vms/{}/pty/{}/connect?token=invalid",
                Uuid::new_v4(),
                Uuid::new_v4()
            );
            let first = tokio_tungstenite::connect_async(&uri)
                .await
                .expect_err("invalid PTY token must reject the WebSocket handshake");
            let tokio_tungstenite::tungstenite::Error::Http(first) = first else {
                panic!("expected an HTTP handshake rejection");
            };
            assert_eq!(first.status().as_u16(), StatusCode::UNAUTHORIZED.as_u16());

            let second = tokio_tungstenite::connect_async(&uri)
                .await
                .expect_err("second invalid PTY token must be rate limited");
            let tokio_tungstenite::tungstenite::Error::Http(second) = second else {
                panic!("expected an HTTP rate-limit response");
            };
            assert_eq!(
                second.status().as_u16(),
                StatusCode::TOO_MANY_REQUESTS.as_u16()
            );
            server.abort();
        });
        drop(rt);
    }

    #[test]
    fn public_runtime_status_drops_vmm_configuration() {
        let status = public_vm_runtime_status(serde_json::json!({
            "state": "running",
            "uptime_ms": 10,
            "vcpus": 1,
            "mem_mib": 256,
            "volumes": 1,
            "nets": 1,
            "kernel": "/srv/tarit/private/vmlinux",
            "vcpu_alive": true
        }))
        .unwrap();
        let value = serde_json::to_value(status).unwrap();
        assert!(value.get("kernel").is_none());
        assert!(value.get("volumes").is_none());
        assert!(value.get("nets").is_none());
    }

    #[test]
    fn public_errors_hide_internal_paths_and_peer_addresses() {
        let rt = test_runtime();
        let internal = ApiError(OrchError::Internal(
            "peer https://node.internal /srv/tarit/vm.sock".into(),
        ))
        .into_response();
        let value = rt.block_on(response_json(internal));
        assert_eq!(value["error"], "internal error");

        let unavailable = ApiError(OrchError::Unavailable(
            "owner host node-private is stale".into(),
        ))
        .into_response();
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        let value = rt.block_on(response_json(unavailable));
        assert_eq!(value["error"], "service unavailable");
    }

    #[test]
    fn stopped_vms_are_deleted_from_public_list_semantics() {
        let state = test_state();
        insert_vm(&state, Uuid::new_v4(), "tenant-a", VmStatus::Stopped);
        let app = router(state);
        let rt = test_runtime();
        let response = rt
            .block_on(
                app.oneshot(
                    Request::builder()
                        .uri("/v1/vms")
                        .header("X-API-Key", "tenant-a-key")
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = rt
            .block_on(to_bytes(response.into_body(), usize::MAX))
            .unwrap();
        let vms: Vec<PublicVmRecord> = serde_json::from_slice(&body).unwrap();
        assert!(vms.is_empty());
    }

    #[test]
    fn tenant_quota_blocks_create_before_spawn() {
        let state = test_state();
        insert_vm(&state, Uuid::new_v4(), "tenant-a", VmStatus::Running);
        let app = router(state);
        let rt = test_runtime();

        let response = rt
            .block_on(
                app.clone().oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/vms")
                        .header("X-API-Key", "tenant-a-key")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"memory_mib":256,"vcpus":1}"#))
                        .unwrap(),
                ),
            )
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = rt
            .block_on(to_bytes(response.into_body(), usize::MAX))
            .unwrap();
        let error: ErrorBody = serde_json::from_slice(&body).unwrap();
        assert!(error.error.contains("quota"));
        drop(rt);
    }

    #[test]
    fn shutdown_rejects_vm_create_before_cluster_placement() {
        let (mut state, mut writes) = test_state_with_audit();
        state.config.admission_timeout_ms = 60_000;
        state.supervisor.begin_shutdown();
        let app = router(state);
        let rt = test_runtime();

        let response = rt
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_millis(100),
                    request_json(
                        app,
                        "POST",
                        "/v1/vms",
                        "admin-key",
                        serde_json::json!({"memory_mib": 256, "vcpus": 1}),
                    ),
                )
                .await
            })
            .expect("shutdown must reject create without waiting for placement");

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            rt.block_on(response_json(response)),
            serde_json::json!({"error": "taritd is shutting down"})
        );
        let StoreWrite::Audit(event) = writes
            .try_recv()
            .expect("the rejected create must emit one audit event")
        else {
            panic!("shutdown rejection emitted a non-audit store write");
        };
        assert_eq!(event.action, audit_action::CREATE);
        assert_eq!(event.outcome, audit_outcome::ERROR);
        assert!(writes.try_recv().is_err());
        drop(rt);
    }

    #[test]
    fn user_cannot_call_admin_cluster_route() {
        let app = router(test_state());
        let rt = test_runtime();
        let response = rt
            .block_on(
                app.clone().oneshot(
                    Request::builder()
                        .uri("/v1/cluster")
                        .header("X-API-Key", "tenant-a-key")
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
            .unwrap();
        drop(rt);

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn admin_can_query_warm_pool_status() {
        let app = router(test_state());
        let rt = test_runtime();
        let response = rt
            .block_on(
                app.clone().oneshot(
                    Request::builder()
                        .uri("/v1/warm-pool")
                        .header("X-API-Key", "admin-key")
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
            .unwrap();
        let body = rt
            .block_on(async { axum::body::to_bytes(response.into_body(), usize::MAX).await })
            .unwrap();
        drop(rt);

        assert!(std::str::from_utf8(&body)
            .unwrap()
            .contains("\"replenish_concurrency\""));
    }

    #[test]
    fn access_and_quota_helpers_enforce_tenant_policy() {
        let state = test_state();
        let vm_id = Uuid::new_v4();
        insert_vm(&state, vm_id, "tenant-a", VmStatus::Running);
        let user_a = state.config.api_keys.resolve("tenant-a-key").unwrap();
        let user_b = ApiIdentity {
            tenant: "tenant-b".into(),
            role: ApiRole::User,
            max_vms: Some(1),
            api_key_id: "test-key-b".into(),
        };
        let admin = state.config.api_keys.resolve("admin-key").unwrap();
        let vm = ops::get_local(&state, vm_id).unwrap();

        assert!(identity_can_access_vm(&user_a, &vm));
        assert!(!identity_can_access_vm(&user_b, &vm));
        assert!(identity_can_access_vm(&admin, &vm));
        let rt = test_runtime();
        assert!(matches!(
            rt.block_on(enforce_vm_quota(&state, &user_a)),
            Err(OrchError::Forbidden(_))
        ));
        drop(rt);
        assert!(require_admin(&user_a).is_err());
        assert!(require_admin(&admin).is_ok());
    }

    #[test]
    fn non_admin_create_requests_cannot_override_host_paths() {
        let user = ApiIdentity {
            tenant: "tenant-a".into(),
            role: ApiRole::User,
            max_vms: None,
            api_key_id: "key-a".into(),
        };
        let admin = ApiIdentity {
            role: ApiRole::Admin,
            ..user.clone()
        };

        let mut req = CreateVmRequest {
            id: None,
            owner_key: None,
            api_key_id: None,
            memory_mib: 256,
            vcpus: 1,
            kernel_path: Some("/dev/mem".into()),
            image: None,
            rootfs_path: None,
            cmdline: None,
            volumes: Vec::new(),
        };
        assert!(matches!(
            enforce_create_path_policy(&user, &req),
            Err(OrchError::Forbidden(_))
        ));
        assert!(enforce_create_path_policy(&admin, &req).is_ok());

        req.kernel_path = None;
        req.rootfs_path = Some("/etc/shadow".into());
        assert!(matches!(
            enforce_create_path_policy(&user, &req),
            Err(OrchError::Forbidden(_))
        ));

        req.rootfs_path = None;
        req.image = Some("node20".into());
        assert!(enforce_create_path_policy(&user, &req).is_ok());

        let signed_policy = crate::image::ImageAdmissionPolicy {
            require_signature: true,
            cosign_key: Some("trusted.pub".into()),
        };
        req.image = None;
        assert!(matches!(
            enforce_image_admission_policy(&user, &req, &signed_policy),
            Err(OrchError::Unprocessable(_))
        ));
        assert!(enforce_image_admission_policy(&admin, &req, &signed_policy).is_ok());
        req.image = Some("ubuntu:24.04".into());
        assert!(enforce_image_admission_policy(&user, &req, &signed_policy).is_ok());
    }

    #[test]
    fn tenant_cannot_create_share_for_foreign_vm() {
        let (state, _audits) = test_state_with_audit();
        let vm_id = Uuid::new_v4();
        insert_vm(&state, vm_id, "tenant-b", VmStatus::Running);
        let rt = test_runtime();

        let response = rt.block_on(request_json(
            router(state.clone()),
            "POST",
            "/v1/shares",
            "tenant-a-key",
            serde_json::json!({
                "vm_id": vm_id,
                "guest_port": 8080,
                "visibility": "public",
            }),
        ));

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let audits = durable_audits(&state);
        assert_eq!(audits.len(), 2);
        assert_eq!(audits[0].action, audit_action::CREATE_SHARE);
        assert_eq!(audits[0].outcome, audit_outcome::ATTEMPT);
        assert_eq!(audits[0].vm_id, Some(vm_id));
        assert!(audits[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("guest_port=8080"));
        assert_eq!(audits[1].action, audit_action::CREATE_SHARE);
        assert_eq!(audits[1].outcome, audit_outcome::DENIED);
        assert_eq!(audits[1].vm_id, Some(vm_id));
        assert!(audits[1]
            .detail
            .as_deref()
            .unwrap()
            .contains("guest_port=8080"));
        drop(rt);
    }

    #[test]
    fn share_routes_enforce_lifecycle_statuses_and_keep_tokens_out_of_audits() {
        let (mut state, _audits) = test_state_with_audit();
        state.config.share_token_key = Some([7; 32]);
        let vm_id = Uuid::new_v4();
        insert_vm(&state, vm_id, "tenant-a", VmStatus::Running);
        let rt = test_runtime();

        let create = rt.block_on(request_json(
            router(state.clone()),
            "POST",
            "/v1/shares",
            "tenant-a-key",
            serde_json::json!({"vm_id": vm_id, "guest_port": 8080}),
        ));
        assert_eq!(create.status(), StatusCode::CREATED);
        let created = rt.block_on(response_json(create));
        let share_id = created["id"].as_str().unwrap().to_owned();
        assert_eq!(created["vm_id"], vm_id.to_string());
        assert_eq!(created["guest_port"], 8080);
        assert_eq!(created["visibility"], "private");
        let audits = durable_audits(&state);
        assert_eq!(audits.len(), 2);
        let create_attempt = &audits[0];
        assert_eq!(create_attempt.action, audit_action::CREATE_SHARE);
        assert_eq!(create_attempt.outcome, audit_outcome::ATTEMPT);
        assert_eq!(create_attempt.vm_id, Some(vm_id));
        let create_attempt_detail = create_attempt.detail.as_deref().unwrap();
        assert!(create_attempt_detail.contains("guest_port=8080"));
        assert!(create_attempt_detail.contains("visibility=private"));
        assert!(!create_attempt_detail.contains("share_id="));
        let create_audit = &audits[1];
        assert_eq!(create_audit.action, audit_action::CREATE_SHARE);
        assert_eq!(create_audit.vm_id, Some(vm_id));
        assert_eq!(create_audit.outcome, audit_outcome::OK);
        assert_share_audit_detail(create_audit, &share_id, 8080, "private");

        let list = rt.block_on(request_json(
            router(state.clone()),
            "GET",
            "/v1/shares",
            "tenant-a-key",
            serde_json::json!({}),
        ));
        assert_eq!(list.status(), StatusCode::OK);
        let listed = rt.block_on(response_json(list));
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["id"], share_id);

        let get = rt.block_on(request_json(
            router(state.clone()),
            "GET",
            &format!("/v1/shares/{share_id}"),
            "tenant-a-key",
            serde_json::json!({}),
        ));
        assert_eq!(get.status(), StatusCode::OK);

        let foreign_get = rt.block_on(request_json(
            router(state.clone()),
            "GET",
            &format!("/v1/shares/{share_id}"),
            "tenant-b-key",
            serde_json::json!({}),
        ));
        assert_eq!(foreign_get.status(), StatusCode::FORBIDDEN);

        let missing_get = rt.block_on(request_json(
            router(state.clone()),
            "GET",
            &format!("/v1/shares/{}", Uuid::new_v4()),
            "tenant-a-key",
            serde_json::json!({}),
        ));
        assert_eq!(missing_get.status(), StatusCode::NOT_FOUND);

        let invalid_update = rt.block_on(request_json(
            router(state.clone()),
            "PATCH",
            &format!("/v1/shares/{share_id}"),
            "tenant-a-key",
            serde_json::json!({"guest_port": 0}),
        ));
        assert_eq!(invalid_update.status(), StatusCode::BAD_REQUEST);
        let audits = durable_audits(&state);
        assert_eq!(audits.len(), 4);
        assert_eq!(audits[2].action, audit_action::UPDATE_SHARE);
        assert_eq!(audits[2].outcome, audit_outcome::ATTEMPT);
        let invalid_update_audit = &audits[3];
        assert_eq!(invalid_update_audit.action, audit_action::UPDATE_SHARE);
        assert_eq!(invalid_update_audit.outcome, audit_outcome::ERROR);
        let invalid_update_detail = invalid_update_audit.detail.as_deref().unwrap();
        assert!(invalid_update_detail.contains(&format!("share_id={share_id}")));
        assert!(invalid_update_detail.contains("guest_port=0"));

        let token = rt.block_on(request_json(
            router(state.clone()),
            "POST",
            &format!("/v1/shares/{share_id}/tokens"),
            "tenant-a-key",
            serde_json::json!({}),
        ));
        assert_eq!(token.status(), StatusCode::OK);
        let token_response = rt.block_on(response_json(token));
        let token = token_response["token"].as_str().unwrap();
        let token_fields = token_response.as_object().unwrap();
        assert_eq!(token_fields.len(), 2);
        assert!(token_fields.contains_key("token"));
        assert!(token_fields.contains_key("expires_at"));
        let audits = durable_audits(&state);
        assert_eq!(audits.len(), 6);
        assert_eq!(audits[4].action, audit_action::ISSUE_SHARE_TOKEN);
        assert_eq!(audits[4].outcome, audit_outcome::ATTEMPT);
        let token_audit = &audits[5];
        assert_eq!(token_audit.action, audit_action::ISSUE_SHARE_TOKEN);
        assert_eq!(token_audit.outcome, audit_outcome::OK);
        assert_share_audit_detail(token_audit, &share_id, 8080, "private");
        assert!(!token_audit.detail.as_deref().unwrap().contains(token));

        let update = rt.block_on(request_json(
            router(state.clone()),
            "PATCH",
            &format!("/v1/shares/{share_id}"),
            "tenant-a-key",
            serde_json::json!({"guest_port": 9090, "visibility": "public"}),
        ));
        assert_eq!(update.status(), StatusCode::OK);
        let updated = rt.block_on(response_json(update));
        assert_eq!(updated["guest_port"], 9090);
        assert_eq!(updated["visibility"], "public");
        let audits = durable_audits(&state);
        assert_eq!(audits.len(), 8);
        assert_eq!(audits[6].action, audit_action::UPDATE_SHARE);
        assert_eq!(audits[6].outcome, audit_outcome::ATTEMPT);
        let update_audit = &audits[7];
        assert_eq!(update_audit.action, audit_action::UPDATE_SHARE);
        assert_eq!(update_audit.outcome, audit_outcome::OK);
        assert_share_audit_detail(update_audit, &share_id, 9090, "public");

        let public_token = rt.block_on(request_json(
            router(state.clone()),
            "POST",
            &format!("/v1/shares/{share_id}/tokens"),
            "tenant-a-key",
            serde_json::json!({}),
        ));
        assert_eq!(public_token.status(), StatusCode::BAD_REQUEST);
        let audits = durable_audits(&state);
        assert_eq!(audits.len(), 10);
        assert_eq!(audits[8].action, audit_action::ISSUE_SHARE_TOKEN);
        assert_eq!(audits[8].outcome, audit_outcome::ATTEMPT);
        let public_token_audit = &audits[9];
        assert_eq!(public_token_audit.action, audit_action::ISSUE_SHARE_TOKEN);
        assert_eq!(public_token_audit.outcome, audit_outcome::ERROR);
        assert_share_audit_detail(public_token_audit, &share_id, 9090, "public");
        assert!(!public_token_audit
            .detail
            .as_deref()
            .unwrap()
            .contains(token));

        let revoke = rt.block_on(request_json(
            router(state.clone()),
            "DELETE",
            &format!("/v1/shares/{share_id}"),
            "tenant-a-key",
            serde_json::json!({}),
        ));
        assert_eq!(revoke.status(), StatusCode::NO_CONTENT);
        let audits = durable_audits(&state);
        assert_eq!(audits.len(), 12);
        assert_eq!(audits[10].action, audit_action::REVOKE_SHARE);
        assert_eq!(audits[10].outcome, audit_outcome::ATTEMPT);
        let revoke_audit = &audits[11];
        assert_eq!(revoke_audit.action, audit_action::REVOKE_SHARE);
        assert_eq!(revoke_audit.outcome, audit_outcome::OK);
        assert_share_audit_detail(revoke_audit, &share_id, 9090, "public");
        assert!(audits
            .iter()
            .all(|audit| { !audit.detail.as_deref().unwrap_or_default().contains(token) }));
        drop(rt);
    }

    #[test]
    fn share_routes_reject_owner_override_and_admin_uses_service_ownership() {
        let (state, _audits) = test_state_with_audit();
        let tenant_a_vm = Uuid::new_v4();
        let tenant_b_vm = Uuid::new_v4();
        insert_vm(&state, tenant_a_vm, "tenant-a", VmStatus::Running);
        insert_vm(&state, tenant_b_vm, "tenant-b", VmStatus::Running);
        let rt = test_runtime();

        let owner_override = rt.block_on(request_json(
            router(state.clone()),
            "POST",
            "/v1/shares",
            "tenant-a-key",
            serde_json::json!({
                "vm_id": tenant_a_vm,
                "guest_port": 8080,
                "owner_key": "tenant-b",
            }),
        ));
        assert_eq!(owner_override.status(), StatusCode::BAD_REQUEST);

        let created = rt.block_on(request_json(
            router(state.clone()),
            "POST",
            "/v1/shares",
            "admin-key",
            serde_json::json!({
                "vm_id": tenant_b_vm,
                "guest_port": 8080,
                "visibility": "private",
            }),
        ));
        assert_eq!(created.status(), StatusCode::CREATED);
        let created = rt.block_on(response_json(created));
        let share_id = created["id"].as_str().unwrap();

        let admin_get = rt.block_on(request_json(
            router(state.clone()),
            "GET",
            &format!("/v1/shares/{share_id}"),
            "admin-key",
            serde_json::json!({}),
        ));
        assert_eq!(admin_get.status(), StatusCode::OK);

        let tenant_a_get = rt.block_on(request_json(
            router(state.clone()),
            "GET",
            &format!("/v1/shares/{share_id}"),
            "tenant-a-key",
            serde_json::json!({}),
        ));
        assert_eq!(tenant_a_get.status(), StatusCode::FORBIDDEN);

        let admin_list = rt.block_on(request_json(
            router(state.clone()),
            "GET",
            "/v1/shares",
            "admin-key",
            serde_json::json!({}),
        ));
        assert_eq!(admin_list.status(), StatusCode::OK);
        assert!(rt
            .block_on(response_json(admin_list))
            .as_array()
            .unwrap()
            .is_empty());

        let tenant_b_list = rt.block_on(request_json(
            router(state.clone()),
            "GET",
            "/v1/shares",
            "tenant-b-key",
            serde_json::json!({}),
        ));
        assert_eq!(tenant_b_list.status(), StatusCode::OK);
        assert_eq!(
            rt.block_on(response_json(tenant_b_list))
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let admin_update = rt.block_on(request_json(
            router(state.clone()),
            "PATCH",
            &format!("/v1/shares/{share_id}"),
            "admin-key",
            serde_json::json!({"guest_port": 9090}),
        ));
        assert_eq!(admin_update.status(), StatusCode::OK);

        let admin_revoke = rt.block_on(request_json(
            router(state.clone()),
            "DELETE",
            &format!("/v1/shares/{share_id}"),
            "admin-key",
            serde_json::json!({}),
        ));
        assert_eq!(admin_revoke.status(), StatusCode::NO_CONTENT);
        drop(rt);
    }

    #[test]
    fn share_create_rejects_invalid_guest_port_json_as_bad_request() {
        let (state, _audits) = test_state_with_audit();
        let vm_id = Uuid::new_v4();
        insert_vm(&state, vm_id, "tenant-a", VmStatus::Running);
        let rt = test_runtime();

        for body in [
            format!(r#"{{"vm_id":"{vm_id}","guest_port":65536}}"#),
            format!(r#"{{"vm_id":"{vm_id}","guest_port":-1}}"#),
            format!(r#"{{"vm_id":"{vm_id}","guest_port":"8080"}}"#),
            r#"{"vm_id":"not-a-uuid","guest_port":8080}"#.into(),
            r#"{"vm_id":"not-a-uuid","guest_port":8080"#.into(),
        ] {
            let response = rt.block_on(request_raw(
                router(state.clone()),
                "POST",
                "/v1/shares",
                Some("tenant-a-key"),
                &body,
            ));
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        }

        drop(rt);
    }

    #[test]
    fn share_update_rejects_invalid_guest_port_json_as_bad_request() {
        let (state, _audits) = test_state_with_audit();
        let share_id = Uuid::new_v4();
        let rt = test_runtime();
        rt.block_on(insert_share(&state, share_id, "tenant-a"));

        for body in [
            r#"{"guest_port":0}"#,
            r#"{"guest_port":65536}"#,
            r#"{"guest_port":-1}"#,
            r#"{"guest_port":false}"#,
            r#"{"guest_port":8080"#,
        ] {
            let response = rt.block_on(request_raw(
                router(state.clone()),
                "PATCH",
                &format!("/v1/shares/{share_id}"),
                Some("tenant-a-key"),
                body,
            ));
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        }

        drop(rt);
    }

    #[test]
    fn share_routes_require_an_api_key() {
        let response = test_runtime().block_on(request_raw(
            router(test_state()),
            "GET",
            "/v1/shares",
            None,
            "",
        ));

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn foreign_tenant_cannot_update_share() {
        let (state, _audits) = test_state_with_audit();
        let share_id = Uuid::new_v4();
        let rt = test_runtime();
        rt.block_on(insert_share(&state, share_id, "tenant-b"));

        let response = rt.block_on(request_json(
            router(state.clone()),
            "PATCH",
            &format!("/v1/shares/{share_id}"),
            "tenant-a-key",
            serde_json::json!({"guest_port": 9090}),
        ));

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let audits = durable_audits(&state);
        assert_eq!(audits.len(), 2);
        assert_eq!(audits[0].outcome, audit_outcome::ATTEMPT);
        assert_eq!(audits[1].action, audit_action::UPDATE_SHARE);
        assert_eq!(audits[1].outcome, audit_outcome::DENIED);
        assert_share_audit_detail(&audits[1], &share_id.to_string(), 9090, "private");
        assert_eq!(audits[0].vm_id, audits[1].vm_id);
        drop(rt);
    }

    #[test]
    fn foreign_tenant_cannot_revoke_share() {
        let (state, _audits) = test_state_with_audit();
        let share_id = Uuid::new_v4();
        let rt = test_runtime();
        rt.block_on(insert_share(&state, share_id, "tenant-b"));

        let response = rt.block_on(request_json(
            router(state.clone()),
            "DELETE",
            &format!("/v1/shares/{share_id}"),
            "tenant-a-key",
            serde_json::json!({}),
        ));

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let audits = durable_audits(&state);
        assert_eq!(audits.len(), 2);
        assert_eq!(audits[0].outcome, audit_outcome::ATTEMPT);
        assert_eq!(audits[1].action, audit_action::REVOKE_SHARE);
        assert_eq!(audits[1].outcome, audit_outcome::DENIED);
        assert_share_audit_detail(&audits[1], &share_id.to_string(), 8080, "private");
        assert_eq!(audits[0].vm_id, audits[1].vm_id);
        drop(rt);
    }

    #[test]
    fn foreign_tenant_cannot_issue_share_token() {
        let (mut state, _audits) = test_state_with_audit();
        state.config.share_token_key = Some([7; 32]);
        let share_id = Uuid::new_v4();
        let rt = test_runtime();
        rt.block_on(insert_share(&state, share_id, "tenant-b"));

        let response = rt.block_on(request_json(
            router(state.clone()),
            "POST",
            &format!("/v1/shares/{share_id}/tokens"),
            "tenant-a-key",
            serde_json::json!({}),
        ));

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let audits = durable_audits(&state);
        assert_eq!(audits.len(), 2);
        assert_eq!(audits[0].outcome, audit_outcome::ATTEMPT);
        assert_eq!(audits[1].action, audit_action::ISSUE_SHARE_TOKEN);
        assert_eq!(audits[1].outcome, audit_outcome::DENIED);
        assert_share_audit_detail(&audits[1], &share_id.to_string(), 8080, "private");
        assert_eq!(audits[0].vm_id, audits[1].vm_id);
        drop(rt);
    }

    #[test]
    fn share_route_rejects_malformed_uuid_paths() {
        let rt = test_runtime();
        for (method, path) in [
            ("GET", "/v1/shares/not-a-uuid"),
            ("PATCH", "/v1/shares/not-a-uuid"),
            ("DELETE", "/v1/shares/not-a-uuid"),
            ("POST", "/v1/shares/not-a-uuid/tokens"),
        ] {
            let response = rt.block_on(request_json(
                router(test_state()),
                method,
                path,
                "tenant-a-key",
                serde_json::json!({}),
            ));
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{method} {path}"
            );
            assert_eq!(
                rt.block_on(response_json(response))["error"],
                "invalid share request"
            );
        }
        drop(rt);
    }

    #[test]
    fn stale_share_update_returns_conflict() {
        let (state, _audits) = test_state_with_audit();
        let share_id = Uuid::new_v4();
        let rt = test_runtime();
        rt.block_on(insert_share(&state, share_id, "tenant-a"));

        let revoke = rt.block_on(request_json(
            router(state.clone()),
            "DELETE",
            &format!("/v1/shares/{share_id}"),
            "tenant-a-key",
            serde_json::json!({}),
        ));
        assert_eq!(revoke.status(), StatusCode::NO_CONTENT);

        let update = rt.block_on(request_json(
            router(state),
            "PATCH",
            &format!("/v1/shares/{share_id}"),
            "tenant-a-key",
            serde_json::json!({"guest_port": 9090}),
        ));
        assert_eq!(update.status(), StatusCode::CONFLICT);
        drop(rt);
    }

    #[test]
    fn share_create_does_not_rely_on_background_audit_channel() {
        let (state, audits) = test_state_with_audit();
        drop(audits);
        let vm_id = Uuid::new_v4();
        insert_vm(&state, vm_id, "tenant-a", VmStatus::Running);
        let rt = test_runtime();

        let response = rt.block_on(request_json(
            router(state.clone()),
            "POST",
            "/v1/shares",
            "tenant-a-key",
            serde_json::json!({"vm_id": vm_id, "guest_port": 8080}),
        ));
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(durable_audits(&state).len(), 2);
        drop(rt);
    }

    #[test]
    fn share_create_persists_durable_intent_and_outcome() {
        let (state, _audits) = test_state_with_audit();
        let vm_id = Uuid::new_v4();
        insert_vm(&state, vm_id, "tenant-a", VmStatus::Running);
        let rt = test_runtime();

        let response = rt.block_on(request_json(
            router(state.clone()),
            "POST",
            "/v1/shares",
            "tenant-a-key",
            serde_json::json!({"vm_id": vm_id, "guest_port": 8080, "visibility": "public"}),
        ));

        assert_eq!(response.status(), StatusCode::CREATED);
        let share_id = rt.block_on(response_json(response))["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let audits = state.store.lock().unwrap().list_unsent_audit(10).unwrap();
        assert_eq!(audits.len(), 2);
        assert_eq!(audits[0].action, audit_action::CREATE_SHARE);
        assert_eq!(audits[0].outcome, "attempt");
        assert_eq!(audits[0].vm_id, Some(vm_id));
        assert!(audits[0]
            .detail
            .as_deref()
            .unwrap()
            .contains(&format!("vm_id={vm_id}")));
        assert!(!audits[0].detail.as_deref().unwrap().contains("unknown"));
        assert_eq!(audits[1].action, audit_action::CREATE_SHARE);
        assert_eq!(audits[1].outcome, audit_outcome::OK);
        assert_eq!(audits[1].vm_id, Some(vm_id));
        assert_share_audit_detail(&audits[1], &share_id, 8080, "public");
        drop(rt);
    }

    #[test]
    fn durable_share_intent_exists_before_create_mutates() {
        let (mut state, _audits) = test_state_with_audit();
        let vm_id = Uuid::new_v4();
        insert_vm(&state, vm_id, "tenant-a", VmStatus::Running);
        let (intent_tx, intent_rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        state.audit_outbox = Arc::new(BlockingFirstAuditOutbox {
            store: Arc::clone(&state.store),
            intent_tx,
            gate: Arc::clone(&gate),
            calls: AtomicUsize::default(),
        });
        let identity = state.config.api_keys.resolve("tenant-a-key").unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let response = rt.block_on(async {
            let request_state = state.clone();
            let request = tokio::spawn(async move {
                request_json(
                    router(request_state),
                    "POST",
                    "/v1/shares",
                    "tenant-a-key",
                    serde_json::json!({"vm_id": vm_id, "guest_port": 8080}),
                )
                .await
            });

            intent_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("durable intent should be written before the mutation");
            let audits = durable_audits(&state);
            assert_eq!(audits.len(), 1);
            assert_eq!(audits[0].action, audit_action::CREATE_SHARE);
            assert_eq!(audits[0].outcome, audit_outcome::ATTEMPT);
            assert!(crate::shares::list(&state, &identity)
                .await
                .unwrap()
                .is_empty());

            let (released, wake) = &*gate;
            *released.lock().unwrap() = true;
            wake.notify_one();
            request.await.unwrap()
        });

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(durable_audits(&state).len(), 2);
        drop(rt);
    }

    #[test]
    fn share_create_does_not_mutate_when_durable_intent_persistence_fails() {
        let (mut state, _audits) = test_state_with_audit();
        state.audit_outbox = Arc::new(AlwaysFailAuditOutbox);
        let vm_id = Uuid::new_v4();
        insert_vm(&state, vm_id, "tenant-a", VmStatus::Running);
        let rt = test_runtime();

        let response = rt.block_on(request_json(
            router(state.clone()),
            "POST",
            "/v1/shares",
            "tenant-a-key",
            serde_json::json!({"vm_id": vm_id, "guest_port": 8080}),
        ));

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            rt.block_on(response_json(response))["error"],
            "share audit unavailable"
        );
        assert!(rt
            .block_on(crate::shares::list(
                &state,
                &state.config.api_keys.resolve("tenant-a-key").unwrap()
            ))
            .unwrap()
            .is_empty());
        drop(rt);
    }

    #[test]
    fn share_create_returns_503_when_durable_outcome_persistence_fails() {
        let (mut state, _audits) = test_state_with_audit();
        state.audit_outbox = Arc::new(PersistFirstThenFailAuditOutbox {
            store: Arc::clone(&state.store),
            calls: AtomicUsize::default(),
        });
        let vm_id = Uuid::new_v4();
        insert_vm(&state, vm_id, "tenant-a", VmStatus::Running);
        let rt = test_runtime();

        let response = rt.block_on(request_json(
            router(state.clone()),
            "POST",
            "/v1/shares",
            "tenant-a-key",
            serde_json::json!({"vm_id": vm_id, "guest_port": 8080}),
        ));

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            rt.block_on(response_json(response))["error"],
            "share audit unavailable"
        );
        let audits = durable_audits(&state);
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, audit_action::CREATE_SHARE);
        assert_eq!(audits[0].outcome, audit_outcome::ATTEMPT);
        assert_eq!(audits[0].vm_id, Some(vm_id));
        assert!(audits[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("guest_port=8080"));
        let shares = rt
            .block_on(crate::shares::list(
                &state,
                &state.config.api_keys.resolve("tenant-a-key").unwrap(),
            ))
            .unwrap();
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].vm_id, vm_id);
        assert_eq!(shares[0].guest_port, 8080);
        drop(rt);
    }

    #[test]
    fn malformed_share_requests_are_durably_audited_without_unknown_fields() {
        let (state, _audits) = test_state_with_audit();
        let share_id = Uuid::new_v4();
        let vm_id = Uuid::new_v4();
        let rt = test_runtime();
        rt.block_on(insert_share(&state, share_id, "tenant-a"));
        insert_vm(&state, vm_id, "tenant-a", VmStatus::Running);

        let malformed_create = rt.block_on(request_raw(
            router(state.clone()),
            "POST",
            "/v1/shares",
            Some("tenant-a-key"),
            &format!(r#"{{"vm_id":"{vm_id}","guest_port":65536}}"#),
        ));
        assert_eq!(malformed_create.status(), StatusCode::BAD_REQUEST);

        let malformed_body = rt.block_on(request_raw(
            router(state.clone()),
            "PATCH",
            &format!("/v1/shares/{share_id}"),
            Some("tenant-a-key"),
            r#"{"guest_port":8080"#,
        ));
        assert_eq!(malformed_body.status(), StatusCode::BAD_REQUEST);

        let malformed_id = rt.block_on(request_raw(
            router(state.clone()),
            "DELETE",
            "/v1/shares/not-a-uuid",
            Some("tenant-a-key"),
            "",
        ));
        assert_eq!(malformed_id.status(), StatusCode::BAD_REQUEST);

        let audits = state.store.lock().unwrap().list_unsent_audit(10).unwrap();
        assert_eq!(audits.len(), 6);
        assert_eq!(audits[0].action, audit_action::CREATE_SHARE);
        assert_eq!(audits[0].outcome, audit_outcome::ATTEMPT);
        assert_eq!(audits[0].vm_id, Some(vm_id));
        assert!(audits[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("attempted_guest_port=65536"));
        assert_eq!(audits[1].outcome, audit_outcome::ERROR);
        assert_eq!(audits[2].action, audit_action::UPDATE_SHARE);
        assert_eq!(audits[2].outcome, audit_outcome::ATTEMPT);
        assert_share_audit_detail(&audits[2], &share_id.to_string(), 8080, "private");
        assert_eq!(audits[3].outcome, audit_outcome::ERROR);
        assert_eq!(audits[4].action, audit_action::REVOKE_SHARE);
        assert_eq!(audits[4].outcome, audit_outcome::ATTEMPT);
        assert_eq!(audits[4].vm_id, None);
        assert_eq!(audits[4].detail, None);
        assert_eq!(audits[5].outcome, audit_outcome::ERROR);
        assert_eq!(audits[5].detail, None);
        assert!(audits.iter().all(|audit| !audit
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("unknown")));
        drop(rt);
    }

    #[test]
    fn share_peer_unauthorized_is_owner_unavailable_and_audited_as_error() {
        let (state, _audits) = test_state_with_audit();
        let response = ShareApiError::from_service(OrchError::Unauthorized).into_response();
        let rt = test_runtime();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            rt.block_on(response_json(response))["error"],
            "owner_unavailable"
        );
        assert!(record_share_audit(
            &state,
            &state.config.api_keys.resolve("tenant-a-key").unwrap(),
            audit_action::CREATE_SHARE,
            ShareAuditFields::default(),
            audit_outcome_for(&OrchError::Unauthorized),
        )
        .is_ok());
        let audits = durable_audits(&state);
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].outcome, audit_outcome::ERROR);
        drop(rt);
    }

    #[test]
    fn share_internal_errors_are_unavailable_without_source_details() {
        let (mut state, _audits) = test_state_with_audit();
        state.config.share_token_key = Some([7; 32]);
        state.config.share_token_ttl_secs = 0;
        let share_id = Uuid::new_v4();
        let rt = test_runtime();
        rt.block_on(insert_share(&state, share_id, "tenant-a"));

        let response = rt.block_on(request_json(
            router(state),
            "POST",
            &format!("/v1/shares/{share_id}/tokens"),
            "tenant-a-key",
            serde_json::json!({}),
        ));

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let error = rt.block_on(response_json(response));
        assert_eq!(error["error"], "share service unavailable");
        assert!(!error["error"]
            .as_str()
            .unwrap()
            .contains("share token TTL must be positive"));

        let peer_response = ShareApiError::from_service(OrchError::Internal(
            "peer http://10.0.0.2:8443/internal/v1/vms upstream body: connection refused".into(),
        ))
        .into_response();
        assert_eq!(peer_response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let peer_error = rt.block_on(response_json(peer_response));
        assert_eq!(peer_error["error"], "share service unavailable");
        assert!(!peer_error["error"].as_str().unwrap().contains("10.0.0.2"));
        assert!(!peer_error["error"]
            .as_str()
            .unwrap()
            .contains("connection refused"));
        drop(rt);
    }

    struct AlwaysFailAuditOutbox;

    impl audit::DurableAuditOutbox for AlwaysFailAuditOutbox {
        fn enqueue(&self, _: &AuditEvent) -> Result<(), ()> {
            Err(())
        }
    }

    struct PersistFirstThenFailAuditOutbox {
        store: Arc<Mutex<Store>>,
        calls: AtomicUsize,
    }

    impl audit::DurableAuditOutbox for PersistFirstThenFailAuditOutbox {
        fn enqueue(&self, event: &AuditEvent) -> Result<(), ()> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let store = self.store.lock().map_err(|_| ())?;
                store.enqueue_audit(event).map_err(|_| ())
            } else {
                Err(())
            }
        }
    }

    struct BlockingFirstAuditOutbox {
        store: Arc<Mutex<Store>>,
        intent_tx: mpsc::Sender<()>,
        gate: Arc<(Mutex<bool>, Condvar)>,
        calls: AtomicUsize,
    }

    impl audit::DurableAuditOutbox for BlockingFirstAuditOutbox {
        fn enqueue(&self, event: &AuditEvent) -> Result<(), ()> {
            {
                let store = self.store.lock().map_err(|_| ())?;
                store.enqueue_audit(event).map_err(|_| ())?;
            }
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.intent_tx.send(()).map_err(|_| ())?;
                let (released, wake) = &*self.gate;
                let mut released = released.lock().map_err(|_| ())?;
                while !*released {
                    released = wake.wait(released).map_err(|_| ())?;
                }
            }
            Ok(())
        }
    }

    fn test_state() -> AppState {
        test_state_with_audit().0
    }

    fn test_state_with_audit() -> (AppState, tokio::sync::mpsc::Receiver<StoreWrite>) {
        let test_root = PathBuf::from(format!("target/taritd-api-test-{}", unsafe {
            libc::geteuid()
        }));
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            api_keys: ApiKeyRegistry::from_plaintext_entries(vec![
                ("tenant-a-key".into(), "tenant-a".into(), ApiRole::User, 1),
                ("tenant-b-key".into(), "tenant-b".into(), ApiRole::User, 2),
                ("admin-key".into(), "admin".into(), ApiRole::Admin, 0),
            ])
            .unwrap(),
            host_id: "test-host".into(),
            host_session_id: Uuid::nil(),
            vmm_bin: test_root.join("vmm"),
            kernel: test_root.join("kernel"),
            rootfs: test_root.join("rootfs"),
            socket_dir: test_root.join("sockets"),
            db_path: test_root.join("fleet.db"),
            net_state_path: test_root.join("net-state.json"),
            images_dir: test_root.join("images"),
            shared_block: None,
            image_admission_policy: crate::image::ImageAdmissionPolicy::default(),
            max_vms: 4,
            max_vcpus: 4,
            max_memory_mib: 1024,
            peer_secret: "peer-secret".into(),
            peer_listen: None,
            peer_tls: None,
            database_url: None,
            rpc_addr: "http://127.0.0.1:0".into(),
            allow_insecure_peer_http: true,
            enable_net: false,
            rootfs_read_only: false,
            metrics_expose_tenant_labels: false,
            api_max_in_flight: 128,
            api_requests_per_second: 10_000,
            api_request_timeout_ms: 5_000,
            api_max_body_bytes: 1024 * 1024,
            vm_cgroup_parent: None,
            vm_jail: None,
            vm_cgroup_pids_max: 1024,
            vm_io_quota: crate::config::VmIoQuotaConfig::default(),
            vm_net_quota: crate::config::VmNetQuotaConfig::default(),
            disk_pressure: crate::config::DiskPressureConfig::default(),
            warm_pool: WarmPoolConfig::default(),
            admission_timeout_ms: 1,
            reap_on_shutdown: true,
            region: "local".into(),
            zone: "local".into(),
            cloud: "onprem".into(),
            autoscale: AutoscaleConfig::default(),
            ssh_gateway_enabled: false,
            ssh_gateway_addr: "127.0.0.1:0".parse().unwrap(),
            ssh_gateway_host_key_path: test_root.join("ssh_host"),
            share_listen: None,
            share_domain: None,
            share_token_key: None,
            share_token_ttl_secs: 300,
            share_connect_timeout_ms: 10_000,
            share_idle_timeout_secs: 300,
        };
        let store = Arc::new(Mutex::new(Store::open(":memory:").unwrap()));
        let shares = ShareRepository::new(Arc::clone(&store), None);
        let (store_tx, store_rx) = tokio::sync::mpsc::channel(128);
        (
            AppState {
                config: config.clone(),
                audit_outbox: Arc::new(audit::LocalAuditOutbox::new(Arc::clone(&store))),
                store,
                exec_cache: Arc::new(RwLock::new(HashMap::new())),
                vm_cache: Arc::new(RwLock::new(HashMap::new())),
                store_tx,
                lifecycle: Arc::new(Mutex::new(HashMap::new())),
                activation_gates: Arc::new(Mutex::new(HashMap::new())),
                lifecycle_faults: Arc::new(Mutex::new(Vec::new())),
                lifecycle_pauses: Arc::new(Mutex::new(HashMap::new())),
                terminal_transition_gate: Arc::new(tokio::sync::Mutex::new(())),
                pty_registry: Arc::new(PtyRegistry::default()),
                supervisor: Arc::new(VmmSupervisor::new(config.clone())),
                scheduler: Arc::new(Scheduler::new(config)),
                peer: Arc::new(PeerClient::new("peer-secret".into())),
                shares,
                fleet: None,
                metrics: Arc::new(Metrics::default()),
                share_runtime: Arc::new(crate::share_gateway::ShareRuntime::default()),
            },
            store_rx,
        )
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn insert_vm(state: &AppState, id: Uuid, tenant: &str, status: VmStatus) {
        let now = Utc::now();
        let record = VmRecord {
            id,
            host_id: state.config.host_id.clone(),
            owner_key: Some(tenant.into()),
            api_key_id: None,
            status,
            revision: 1,
            startup_path: None,
            memory_mib: 256,
            vcpus: 1,
            kernel_path: "kernel".into(),
            rootfs_path: None,
            rootfs_read_only: false,
            cmdline: "console=ttyS0".into(),
            runtime_layout: None,
            socket_path: None,
            pid: None,
            created_at: now,
            updated_at: now,
        };
        state.store.lock().unwrap().insert_vm(&record).unwrap();
        state.vm_cache.write().unwrap().insert(id, record);
    }

    async fn request_json(
        app: Router,
        method: &str,
        uri: &str,
        api_key: &str,
        body: serde_json::Value,
    ) -> Response {
        app.oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("X-API-Key", api_key)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    async fn request_raw(
        app: Router,
        method: &str,
        uri: &str,
        api_key: Option<&str>,
        body: &str,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(api_key) = api_key {
            request = request.header("X-API-Key", api_key);
        }
        app.oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap()
    }

    async fn insert_share(state: &AppState, id: Uuid, owner_key: &str) {
        let now = Utc::now();
        state
            .shares
            .insert(&ShareRecord {
                id,
                slug: id.simple().to_string(),
                owner_key: owner_key.into(),
                vm_id: Uuid::new_v4(),
                guest_port: 8080,
                visibility: ShareVisibility::Private,
                token_version: 0,
                revoked_at: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn durable_audits(state: &AppState) -> Vec<AuditEvent> {
        state.store.lock().unwrap().list_unsent_audit(100).unwrap()
    }

    fn assert_share_audit_detail(
        audit: &AuditEvent,
        share_id: &str,
        guest_port: u16,
        visibility: &str,
    ) {
        let detail = audit.detail.as_deref().unwrap();
        assert!(detail.contains(&format!("share_id={share_id}")));
        assert!(detail.contains(&format!("guest_port={guest_port}")));
        assert!(detail.contains(&format!("visibility={visibility}")));
    }
}
