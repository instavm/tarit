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
    extract::{Extension, Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use tarit_store::Store;
use tarit_types::{
    AuditEvent, CreateVmRequest, EgressUpdateRequest, ErrorBody, ExecuteRequest, ExecutionRecord,
    ExecutionStatus, HealthResponse, OrchError, SnapshotRequest, UsageEvent, UsageSummary,
    VmRecord, VmStatus,
};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::cluster::Owner;
use crate::config::{ApiIdentity, ApiRole, Config};
use crate::openapi;
use crate::peer::PeerClient;
use crate::scheduler::Scheduler;
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
    Exec(ExecutionRecord),
    Usage(UsageEvent),
    Audit(AuditEvent),
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
    pub(crate) pty_registry: Arc<crate::pty::PtyRegistry>,
    pub supervisor: Arc<VmmSupervisor>,
    pub scheduler: Arc<Scheduler>,
    pub peer: Arc<PeerClient>,
    /// Global fleet registry (Postgres). `None` in single-host mode; when set,
    /// enables cross-node placement, VM->owner routing, and membership.
    pub fleet: Option<Arc<tarit_fleet::PostgresFleet>>,
    pub metrics: Arc<crate::metrics::Metrics>,
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
        let status =
            StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (
            status,
            Json(ErrorBody {
                error: err.to_string(),
            }),
        )
            .into_response();
        if let Some(retry_after_secs) = err.retry_after_secs() {
            if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
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
        let res = tokio::task::spawn_blocking(move || peer.create_remote(&rpc, &req, &identity))
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
    use std::sync::{Arc, Mutex, RwLock};
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

    fn test_state() -> AppState {
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
        };
        let store = Store::open(":memory:").unwrap();
        let (store_tx, _store_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState {
            config: config.clone(),
            store: Arc::new(Mutex::new(store)),
            exec_cache: Arc::new(RwLock::new(HashMap::new())),
            vm_cache: Arc::new(RwLock::new(HashMap::new())),
            store_tx,
            pty_registry: Arc::new(PtyRegistry::default()),
            supervisor: Arc::new(VmmSupervisor::new(config.clone())),
            scheduler: Arc::new(Scheduler::new(config)),
            peer: Arc::new(PeerClient::new("peer-secret".into())),
            fleet: None,
            metrics: Arc::new(Metrics::default()),
        }
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
}
