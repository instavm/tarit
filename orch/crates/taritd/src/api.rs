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
    extract::{Extension, Path, Query, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use chrono::Utc;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use tarit_store::Store;
use tarit_types::{
    AuditEvent, CreateShareRequest, CreateVmRequest, EgressUpdateRequest, ErrorBody,
    ExecuteRequest, ExecutionRecord, ExecutionStatus, HealthResponse, OrchError, ShareRecord,
    ShareTokenResponse, ShareVisibility, SnapshotRequest, UpdateShareRequest, UsageEvent,
    UsageSummary, VmRecord, VmStatus,
};
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

/// A durability write applied asynchronously by the background store writer, so
/// no request ever blocks on the single SQLite connection. The in-memory caches
/// (vm_cache/exec_cache) are the source of truth for reads; SQLite lags them.
pub enum StoreWrite {
    Vm(VmRecord),
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
    /// In-memory VM records (read source of truth). Writes go here synchronously
    /// and to SQLite asynchronously via store_tx, keeping create/delete off the
    /// store mutex on the hot path.
    pub vm_cache: Arc<RwLock<HashMap<Uuid, VmRecord>>>,
    /// Channel to the background store writer (durability, write-behind).
    pub store_tx: tokio::sync::mpsc::UnboundedSender<StoreWrite>,
    /// Registered user lifecycle state. The supervisor boot gate establishes
    /// Creating records before VMM work; this map then owns publication and
    /// terminal retry progress until reservations can be released.
    pub(crate) lifecycle: Arc<Mutex<HashMap<Uuid, LifecycleState>>>,
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
            _ => err.to_string(),
        };
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
            OrchError::BadRequest(error) => {
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
            error @ (OrchError::Internal(_) | OrchError::Vmm(_) | OrchError::Overloaded { .. }) => {
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
    let protected = Router::new()
        .route("/v1/vms", post(create_vm).get(list_vms))
        .route("/v1/vms/{id}", get(get_vm).delete(delete_vm))
        .route("/v1/vms/{id}/status", get(vm_status))
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
        .route("/v1/vms/{id}/resume", post(resume_vm))
        .route("/v1/vms/{id}/snapshot", post(snapshot_vm))
        .route("/v1/restore", post(restore_vm))
        .route("/v1/execute_async", post(execute_async))
        .route("/v1/execute", post(execute))
        .route("/v1/executions/{id}", get(get_execution))
        .route("/v1/egress/vm/{id}", patch(update_egress))
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
        .route("/v1/usage", get(usage_stats))
        .route("/v1/audit", get(audit_log))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ))
        .with_state(state.clone());

    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics_handler))
        .route("/openapi.yaml", get(openapi::spec))
        .route("/docs", get(openapi::docs))
        .route(
            "/v1/vms/{id}/pty/{pty_id}/connect",
            get(crate::pty::connect_ws),
        )
        .merge(protected)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse::default())
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

async fn metrics_handler(State(state): State<AppState>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        crate::metrics::render_metrics(&state),
    )
        .into_response()
}

async fn create_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Json(req): Json<CreateVmRequest>,
) -> Result<(StatusCode, Json<VmRecord>), ApiError> {
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
) -> Result<(StatusCode, Json<VmRecord>), ApiError> {
    ensure_create_admission_open(state)?;
    req.owner_key = Some(identity.tenant.clone());
    req.api_key_id = Some(identity.api_key_id.clone());
    enforce_create_path_policy(identity, &req)?;
    if let Some(id) = req.id {
        match cluster::resolve_owner(state, id).await {
            Ok(_) => {
                return Err(OrchError::Conflict(format!("vm {id} already exists")).into());
            }
            Err(OrchError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    enforce_vm_quota(state, identity).await?;

    // Cluster admission: place locally (warm/cold) if this node has room; else
    // spill to ANY peer that has capacity (exhaustive). Only if the WHOLE
    // cluster is full do we wait for a slot to free, and only after the
    // admission timeout do we return 429 + Retry-After. As long as one node in
    // the fleet can take the VM, the request succeeds.
    let deadline = Instant::now() + Duration::from_millis(state.config.admission_timeout_ms);
    loop {
        let last_overloaded = match ops::create_local(state, &req).await {
            Ok(record) => return Ok((StatusCode::CREATED, Json(record))),
            Err(OrchError::Overloaded { message, .. }) => message, // local full — try the rest of the fleet
            Err(e) => return Err(e.into()),
        };

        if state.fleet.is_some() {
            if let Some(record) = place_on_peer(state, &req, identity).await? {
                return Ok((StatusCode::CREATED, Json(record)));
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
    Json(mut req): Json<RestoreRequest>,
) -> Result<(StatusCode, Json<VmRecord>), ApiError> {
    req.owner_key = Some(identity.tenant.clone());
    req.api_key_id = Some(identity.api_key_id.clone());
    enforce_vm_quota(&state, &identity).await?;
    let on_peer = match &req.host_id {
        Some(h) if *h != state.config.host_id => cluster::peer_rpc(&state, h).await?,
        _ => None,
    };
    let record = match on_peer {
        Some(rpc) => {
            let peer = Arc::clone(&state.peer);
            let req = req.clone();
            let identity = identity.clone();
            tokio::task::spawn_blocking(move || peer.restore_remote(&rpc, &req, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??
        }
        None => {
            ops::restore_local(
                &state,
                &req.snapshot_path,
                req.id,
                req.owner_key,
                req.api_key_id,
                identity.is_admin(),
            )
            .await?
        }
    };
    audit::record(
        &state,
        &identity,
        audit_action::RESTORE,
        Some(record.id),
        audit_outcome::OK,
        None,
    );
    Ok((StatusCode::CREATED, Json(record)))
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
        memory_mib: spawn_cfg.memory_mib,
        vcpus: spawn_cfg.vcpus,
        kernel_path: spawn_cfg.kernel_path.display().to_string(),
        rootfs_path: spawn_cfg
            .rootfs_path
            .as_ref()
            .map(|p| p.display().to_string()),
        cmdline: spawn_cfg.cmdline.clone(),
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

async fn tenant_active_vm_count(state: &AppState, tenant: &str) -> Result<usize, OrchError> {
    if let Some(fleet) = &state.fleet {
        return fleet
            .count_active_vms_for_owner(tenant)
            .await
            .map_err(|e| OrchError::Internal(format!("fleet tenant quota count: {e}")));
    }
    Ok(tenant_active_vm_count_local(state, tenant))
}

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

fn is_active_vm_status(status: VmStatus) -> bool {
    matches!(
        status,
        VmStatus::Creating | VmStatus::Running | VmStatus::Paused
    )
}

async fn list_vms(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
) -> Result<Json<Vec<VmRecord>>, ApiError> {
    let vms = state
        .vm_cache
        .read()
        .map(|c| {
            c.values()
                .filter(|vm| identity_can_access_vm(&identity, vm))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Json(vms))
}

async fn get_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<VmRecord>, ApiError> {
    match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            Ok(Json(vm))
        }
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let vm = tokio::task::spawn_blocking(move || peer.get_remote(&rpc, id, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
            Ok(Json(vm))
        }
    }
}

/// Live VMM status (state/uptime/vcpus/mem/config/vcpu_alive), routed to the
/// owning node. Distinct from `GET /v1/vms/{id}`, which returns the stored record.
async fn vm_status(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            Ok(Json(ops::status_local(&state, id).await?))
        }
        Owner::Remote(rpc) => {
            let peer = Arc::clone(&state.peer);
            let v = tokio::task::spawn_blocking(move || peer.status_remote(&rpc, id, &identity))
                .await
                .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
            Ok(Json(v))
        }
    }
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
) -> Result<Json<VmRecord>, ApiError> {
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
    Ok(Json(vm))
}

async fn resume_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<VmRecord>, ApiError> {
    let vm = match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            ops::resume_local(&state, id).await?
        }
        Owner::Remote(rpc) => {
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
    Ok(Json(vm))
}

async fn snapshot_vm(
    State(state): State<AppState>,
    Extension(identity): Extension<ApiIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<SnapshotRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let out = match cluster::resolve_owner(&state, id).await? {
        Owner::Local => {
            let vm = ops::get_local(&state, id)?;
            ensure_vm_access(&identity, &vm)?;
            let path = ops::snapshot_local(&state, id, body.diff).await?;
            // Return the owning node so a later restore routes to the file.
            serde_json::json!({ "path": path, "host_id": state.config.host_id })
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
    let owner = cluster::resolve_owner(&state, req.vm_id).await?;
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

    // Serve status polls from the in-memory cache; persist to SQLite off the
    // response path (in the exec task below), so a 200-wide burst's create+exec
    // does not serialize on the single store connection.
    if let Ok(mut c) = state.exec_cache.write() {
        c.insert(exec_id, record.clone());
    }
    let initial_record = record.clone();

    let state2 = state.clone();
    let command = req.command;
    let timeout_ms = req.timeout_ms;
    let vm_id = req.vm_id;

    tokio::spawn(async move {
        let _ = state2.store_tx.send(StoreWrite::Exec(initial_record));
        let _ = update_exec_status(&state2, exec_id, ExecutionStatus::Running, None);

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
                persist_exec(&state2, &rec);
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
                audit::record(
                    &state2,
                    &identity,
                    audit_action::EXEC,
                    Some(vm_id),
                    audit_outcome::ERROR,
                    Some(e.to_string()),
                );
                let _ = update_exec_status(
                    &state2,
                    exec_id,
                    ExecutionStatus::Failed,
                    Some(e.to_string()),
                );
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
    let owner = cluster::resolve_owner(state, req.vm_id).await?;
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
        Err(e) => ExecutionRecord {
            id: exec_id,
            vm_id,
            command,
            timeout_ms,
            status: ExecutionStatus::Failed,
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: None,
            error: Some(e.to_string()),
            created_at: now,
            updated_at: Utc::now(),
        },
    };
    if let Ok(mut c) = state.exec_cache.write() {
        c.insert(exec_id, rec.clone());
    }
    let _ = state.store_tx.send(StoreWrite::Exec(rec.clone()));
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
        "nodes": nodes,
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

/// Write an execution record through to both the in-memory cache (the read path
/// for status polls) and the SQLite store (durability).
fn persist_exec(state: &AppState, rec: &ExecutionRecord) {
    if let Ok(mut c) = state.exec_cache.write() {
        c.insert(rec.id, rec.clone());
    }
    let _ = state.store_tx.send(StoreWrite::Exec(rec.clone()));
}

fn update_exec_status(
    state: &AppState,
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
    persist_exec(state, &rec);
    Ok(())
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
        let user_vms: Vec<VmRecord> = serde_json::from_slice(&body).unwrap();
        assert_eq!(user_vms.len(), 1);
        assert_eq!(user_vms[0].id, tenant_a_id);

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
        let admin_vms: Vec<VmRecord> = serde_json::from_slice(&body).unwrap();
        assert_eq!(admin_vms.len(), 2);
        drop(rt);
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
        while let Ok(write) = writes.try_recv() {
            assert!(
                !matches!(write, StoreWrite::Vm(_)),
                "shutdown rejection must not enqueue a provisional VM record"
            );
        }
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

    fn test_state_with_audit() -> (AppState, tokio::sync::mpsc::UnboundedReceiver<StoreWrite>) {
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            api_keys: ApiKeyRegistry::from_plaintext_entries(vec![
                ("tenant-a-key".into(), "tenant-a".into(), ApiRole::User, 1),
                ("tenant-b-key".into(), "tenant-b".into(), ApiRole::User, 2),
                ("admin-key".into(), "admin".into(), ApiRole::Admin, 0),
            ])
            .unwrap(),
            host_id: "test-host".into(),
            vmm_bin: PathBuf::from("target/taritd-api-test/vmm"),
            kernel: PathBuf::from("target/taritd-api-test/kernel"),
            rootfs: PathBuf::from("target/taritd-api-test/rootfs"),
            socket_dir: PathBuf::from("target/taritd-api-test/sockets"),
            db_path: PathBuf::from("target/taritd-api-test/fleet.db"),
            net_state_path: PathBuf::from("target/taritd-api-test/net-state.json"),
            images_dir: PathBuf::from("target/taritd-api-test/images"),
            max_vms: 4,
            max_vcpus: 4,
            max_memory_mib: 1024,
            peer_secret: "peer-secret".into(),
            database_url: None,
            rpc_addr: "http://127.0.0.1:0".into(),
            enable_net: false,
            rootfs_read_only: false,
            metrics_expose_tenant_labels: false,
            vm_cgroup_parent: None,
            vm_cgroup_pids_max: 1024,
            warm_pool: WarmPoolConfig::default(),
            admission_timeout_ms: 1,
            reap_on_shutdown: true,
            region: "local".into(),
            zone: "local".into(),
            cloud: "onprem".into(),
            autoscale: AutoscaleConfig::default(),
            ssh_gateway_enabled: false,
            ssh_gateway_addr: "127.0.0.1:0".parse().unwrap(),
            ssh_gateway_host_key_path: PathBuf::from("target/taritd-api-test/ssh_host"),
            share_listen: None,
            share_domain: None,
            share_token_key: None,
            share_token_ttl_secs: 300,
            share_connect_timeout_ms: 10_000,
            share_idle_timeout_secs: 300,
            acme_enabled: false,
            acme_directory_url: "https://acme-v02.api.letsencrypt.org/directory".into(),
            acme_contact_email: None,
            acme_dns_provider: None,
            acme_cloudflare_api_token: None,
            acme_cloudflare_zone_id: None,
            acme_route53_zone_id: None,
            acme_kek: None,
            share_tls_listen: None,
        };
        let store = Arc::new(Mutex::new(Store::open(":memory:").unwrap()));
        let shares = ShareRepository::new(Arc::clone(&store), None);
        let (store_tx, store_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            AppState {
                config: config.clone(),
                audit_outbox: Arc::new(audit::LocalAuditOutbox::new(Arc::clone(&store))),
                store,
                exec_cache: Arc::new(RwLock::new(HashMap::new())),
                vm_cache: Arc::new(RwLock::new(HashMap::new())),
                store_tx,
                lifecycle: Arc::new(Mutex::new(HashMap::new())),
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
        state.vm_cache.write().unwrap().insert(
            id,
            VmRecord {
                id,
                host_id: state.config.host_id.clone(),
                owner_key: Some(tenant.into()),
                api_key_id: None,
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
            },
        );
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
