//! Per-API-key audit trail. Each audited action (create, delete, pause, resume,
//! snapshot, restore, exec, attach_pty, ssh_attempt, update_egress) is recorded
//! as an `AuditEvent` attributed to the acting API key, buffered in the local
//! outbox, and flushed to the primary store (Postgres) by the usage flusher so
//! the primary DB is the one source of truth. Recording is fire-and-forget and
//! never blocks or fails a request.

use uuid::Uuid;

use tarit_types::AuditEvent;

use crate::api::{AppState, StoreWrite};
use crate::config::ApiIdentity;

/// Record an audited action for the acting key. `detail` is a small,
/// secret-free string (e.g. a fingerprint, a rule count, or an error summary).
pub fn record(
    state: &AppState,
    identity: &ApiIdentity,
    action: &str,
    vm_id: Option<Uuid>,
    outcome: &str,
    detail: Option<String>,
) {
    let event = AuditEvent {
        id: Uuid::new_v4(),
        api_key_id: identity.api_key_id.clone(),
        owner_key: identity.tenant.clone(),
        host_id: state.config.host_id.clone(),
        vm_id,
        action: action.to_string(),
        outcome: outcome.to_string(),
        detail,
        created_at: chrono::Utc::now(),
    };
    let _ = state.store_tx.send(StoreWrite::Audit(event));
}

/// Record an audit event when the caller must not report success until the
/// write-behind pipeline has accepted it.
pub(crate) fn record_required(
    state: &AppState,
    identity: &ApiIdentity,
    action: &str,
    vm_id: Option<Uuid>,
    outcome: &str,
    detail: Option<String>,
) -> Result<(), ()> {
    let event = AuditEvent {
        id: Uuid::new_v4(),
        api_key_id: identity.api_key_id.clone(),
        owner_key: identity.tenant.clone(),
        host_id: state.config.host_id.clone(),
        vm_id,
        action: action.to_string(),
        outcome: outcome.to_string(),
        detail,
        created_at: chrono::Utc::now(),
    };
    state
        .store_tx
        .send(StoreWrite::Audit(event))
        .map_err(|_| ())
}

/// Record an audit event from raw identity parts (used by the SSH gateway,
/// which authenticates a key before it has a full `ApiIdentity`).
pub fn record_parts(
    state: &AppState,
    api_key_id: &str,
    owner_key: &str,
    action: &str,
    vm_id: Option<Uuid>,
    outcome: &str,
    detail: Option<String>,
) {
    let event = AuditEvent {
        id: Uuid::new_v4(),
        api_key_id: api_key_id.to_string(),
        owner_key: owner_key.to_string(),
        host_id: state.config.host_id.clone(),
        vm_id,
        action: action.to_string(),
        outcome: outcome.to_string(),
        detail,
        created_at: chrono::Utc::now(),
    };
    let _ = state.store_tx.send(StoreWrite::Audit(event));
}
