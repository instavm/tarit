//! Per-API-key audit trail. Each audited action (create, delete, pause, resume,
//! snapshot, restore, exec, attach_pty, ssh_attempt, update_egress) is recorded
//! as an `AuditEvent` attributed to the acting API key, buffered in the local
//! outbox, and flushed to the primary store (Postgres) by the usage flusher so
//! the primary DB is the one source of truth. Most recording is fire-and-forget;
//! lifecycle actions that require an audit trail use the synchronous durable
//! outbox path below.

use std::sync::{Arc, Mutex};
use tarit_store::Store;
use tarit_types::AuditEvent;
use uuid::Uuid;

use crate::api::{AppState, StoreWrite};
use crate::config::ApiIdentity;

/// Synchronous persistence boundary for audit events that must survive before a
/// caller can mutate share state or receive a token.
pub(crate) trait DurableAuditOutbox: Send + Sync {
    fn enqueue(&self, event: &AuditEvent) -> Result<(), ()>;
}

pub(crate) struct LocalAuditOutbox {
    store: Arc<Mutex<Store>>,
}

impl LocalAuditOutbox {
    pub(crate) fn new(store: Arc<Mutex<Store>>) -> Self {
        Self { store }
    }
}

impl DurableAuditOutbox for LocalAuditOutbox {
    fn enqueue(&self, event: &AuditEvent) -> Result<(), ()> {
        let store = self.store.lock().map_err(|_| ())?;
        store.enqueue_audit(event).map_err(|_| ())
    }
}

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
    if let Err(error) = state.store_tx.try_send(StoreWrite::Audit(event.clone())) {
        state.metrics.inc_store_enqueue_failure();
        if state.audit_outbox.enqueue(&event).is_err() {
            tracing::error!(%error, "audit outbox queue and synchronous fallback unavailable");
        }
    }
}

/// Record an audit event synchronously in the durable local outbox. The fleet
/// exporter delivers that outbox asynchronously, but callers that require an
/// audit trail must not rely on a channel enqueue as evidence of persistence.
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
    state.audit_outbox.enqueue(&event)
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
    if let Err(error) = state.store_tx.try_send(StoreWrite::Audit(event.clone())) {
        state.metrics.inc_store_enqueue_failure();
        if state.audit_outbox.enqueue(&event).is_err() {
            tracing::error!(%error, "audit outbox queue and synchronous fallback unavailable");
        }
    }
}
