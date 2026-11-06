//! Usage stats metering. This records raw usage only (which key, which VM, how
//! many wall-clock seconds a VM was alive, and per-exec durations). A user or
//! billing layer sits above the orchestrator and interprets these stats; the
//! orchestrator never computes prices.
//!
//! Flow: a meter tick emits non-overlapping VM-runtime intervals per alive local
//! VM, advancing a persisted per-VM watermark so nothing is double counted and a
//! crash loses at most one tick. Events are buffered in the local SQLite outbox
//! and a flusher writes them to the primary store (Postgres) as the one source
//! of truth. `(vm_id, kind, window_end)` is unique in Postgres, so re-sending a
//! not-yet-acked batch is idempotent.

use std::time::Duration;
use tokio::sync::watch;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use tarit_fleet::PostgresFleet;
use tarit_types::{UsageEvent, UsageKind, VmStatus};

use crate::api::{AppState, StoreWrite};

const FLUSH_BATCH: usize = 500;
const PRUNE_INTERVAL: Duration = Duration::from_secs(3600);

/// A local alive VM the meter needs: id, key id, tenant, and creation time.
type AliveVm = (Uuid, Option<String>, Option<String>, DateTime<Utc>);

/// Spawn the VM-runtime meter. Every `interval_secs` it bills each alive local
/// VM for the wall-clock seconds since its last billed watermark.
pub fn spawn_usage_meter(
    state: AppState,
    interval_secs: u64,
    mut shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
        loop {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut shutdown_rx) => break,
                _ = tick.tick() => {}
            }
            meter_runtime_once(&state);
        }
    })
}

/// Emit VM-runtime usage for every alive local VM in one pass.
fn meter_runtime_once(state: &AppState) {
    let alive: Vec<AliveVm> = match state.vm_cache.read() {
        Ok(cache) => cache
            .values()
            .filter(|v| {
                v.host_id == state.config.host_id
                    && matches!(
                        v.status,
                        VmStatus::Running | VmStatus::Paused | VmStatus::Suspended
                    )
            })
            .map(|v| {
                (
                    v.id,
                    v.api_key_id.clone(),
                    v.owner_key.clone(),
                    v.created_at,
                )
            })
            .collect(),
        Err(_) => return,
    };
    if alive.is_empty() {
        return;
    }
    let now = Utc::now();
    let Ok(store) = state.store.lock() else {
        return;
    };
    for (vm_id, api_key_id, owner_key, created_at) in alive {
        let (Some(api_key_id), Some(owner_key)) = (api_key_id, owner_key) else {
            continue;
        };
        let start = store
            .get_billing_watermark(vm_id)
            .ok()
            .flatten()
            .unwrap_or(created_at);
        let seconds = (now - start).num_milliseconds() as f64 / 1000.0;
        if seconds <= 0.0 {
            continue;
        }
        let event = runtime_event(state, vm_id, api_key_id, owner_key, start, now);
        if store.enqueue_usage(&event).is_ok() {
            let _ = store.set_billing_watermark(vm_id, now);
        }
    }
}

/// Emit the final runtime interval when a VM stops, then drop its watermark.
/// Called off the delete/teardown path; best effort.
pub fn meter_vm_final(state: &AppState, vm_id: Uuid) {
    let vm = state
        .vm_cache
        .read()
        .ok()
        .and_then(|c| c.get(&vm_id).cloned());
    let Some(vm) = vm else {
        return;
    };
    if vm.host_id != state.config.host_id {
        return;
    }
    let (Some(api_key_id), Some(owner_key)) = (vm.api_key_id.clone(), vm.owner_key.clone()) else {
        return;
    };
    let now = Utc::now();
    let Ok(store) = state.store.lock() else {
        return;
    };
    let start = store
        .get_billing_watermark(vm_id)
        .ok()
        .flatten()
        .unwrap_or(vm.created_at);
    let seconds = (now - start).num_milliseconds() as f64 / 1000.0;
    if seconds > 0.0 {
        let event = runtime_event(state, vm_id, api_key_id, owner_key, start, now);
        let _ = store.enqueue_usage(&event);
    }
    let _ = store.clear_billing_watermark(vm_id);
}

/// Enqueue an exec usage stat (secondary metering). Fire-and-forget.
pub fn meter_exec(
    state: &AppState,
    api_key_id: &str,
    owner_key: &str,
    vm_id: Uuid,
    duration_ms: u64,
) {
    let now = Utc::now();
    let event = UsageEvent {
        id: Uuid::new_v4(),
        api_key_id: api_key_id.to_string(),
        owner_key: owner_key.to_string(),
        host_id: state.config.host_id.clone(),
        vm_id,
        kind: UsageKind::Exec,
        seconds: None,
        duration_ms: Some(duration_ms as i64),
        window_start: now,
        window_end: now,
        created_at: now,
    };
    if let Err(error) = state.store_tx.try_send(StoreWrite::Usage(event.clone())) {
        state.metrics.inc_store_enqueue_failure();
        let persisted = state
            .store
            .lock()
            .ok()
            .and_then(|store| store.enqueue_usage(&event).ok());
        if persisted.is_none() {
            tracing::error!(%error, "usage outbox queue and synchronous fallback unavailable");
        }
    }
}

fn runtime_event(
    state: &AppState,
    vm_id: Uuid,
    api_key_id: String,
    owner_key: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> UsageEvent {
    UsageEvent {
        id: Uuid::new_v4(),
        api_key_id,
        owner_key,
        host_id: state.config.host_id.clone(),
        vm_id,
        kind: UsageKind::VmRuntime,
        seconds: Some((end - start).num_milliseconds() as f64 / 1000.0),
        duration_ms: None,
        window_start: start,
        window_end: end,
        created_at: Utc::now(),
    }
}

/// Spawn the outbox flusher: push locally buffered usage and audit events to the
/// primary store (Postgres). No-op in single-host mode (no fleet), where events
/// stay in the local outbox until a fleet is configured.
pub fn spawn_outbox_flusher(
    state: AppState,
    interval_secs: u64,
    local_retention_days: u64,
    mut shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let fleet = state.fleet.clone();
    let interval = Duration::from_secs(interval_secs.max(1));
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        let mut last_prune: Option<std::time::Instant> = None;
        loop {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut shutdown_rx) => break,
                _ = tick.tick() => {}
            }
            if let Some(fleet) = fleet.as_ref() {
                flush_once(&state, fleet).await;
            }
            if last_prune.is_none_or(|at| at.elapsed() >= PRUNE_INTERVAL) {
                last_prune = Some(std::time::Instant::now());
                prune_outbox(&state, fleet.is_some(), local_retention_days);
            }
        }
    }))
}

/// Keep the local outbox bounded. With a fleet store, rows already flushed
/// there are only kept for a day. Without one the outbox is the source of
/// truth, so rows are kept for `local_retention_days` (0 keeps them forever).
fn prune_outbox(state: &AppState, has_fleet: bool, local_retention_days: u64) {
    let Ok(store) = state.store.lock() else {
        return;
    };
    let result = if has_fleet {
        store.prune_sent_outbox(Utc::now() - chrono::Duration::days(1))
    } else if local_retention_days > 0 {
        let days = i64::try_from(local_retention_days).unwrap_or(i64::MAX);
        let cutoff = chrono::Duration::try_days(days)
            .and_then(|age| Utc::now().checked_sub_signed(age))
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        store.prune_local_outbox(cutoff)
    } else {
        Ok(())
    };
    if let Err(error) = result {
        tracing::warn!(%error, "usage/audit outbox prune failed");
    }
}

async fn wait_for_shutdown(shutdown_rx: &mut watch::Receiver<Option<&'static str>>) {
    loop {
        if shutdown_rx.borrow().is_some() {
            return;
        }
        if shutdown_rx.changed().await.is_err() {
            return;
        }
    }
}

async fn flush_once(state: &AppState, fleet: &PostgresFleet) {
    // Usage. Lock only to read/mark; never hold the store mutex across await.
    let usage = {
        state
            .store
            .lock()
            .ok()
            .and_then(|s| s.list_unsent_usage(FLUSH_BATCH).ok())
            .unwrap_or_default()
    };
    if !usage.is_empty() && fleet.insert_usage_events(&usage).await.is_ok() {
        let ids: Vec<Uuid> = usage.iter().map(|e| e.id).collect();
        if let Ok(s) = state.store.lock() {
            let _ = s.mark_usage_sent(&ids);
        }
    }

    // Audit.
    let audit = {
        state
            .store
            .lock()
            .ok()
            .and_then(|s| s.list_unsent_audit(FLUSH_BATCH).ok())
            .unwrap_or_default()
    };
    if !audit.is_empty() && fleet.insert_audit_events(&audit).await.is_ok() {
        let ids: Vec<Uuid> = audit.iter().map(|e| e.id).collect();
        if let Ok(s) = state.store.lock() {
            let _ = s.mark_audit_sent(&ids);
        }
    }
}
