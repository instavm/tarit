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

use crate::api::{AppState, LifecycleState, StoreWrite};

const FLUSH_BATCH: usize = 500;
const PRUNE_INTERVAL: Duration = Duration::from_secs(3600);
/// How often deleted VMs are dropped from the in-memory VM cache.
const CACHE_EVICT_INTERVAL: Duration = Duration::from_secs(60);
/// How long a deleted VM stays in the in-memory cache. The API already
/// answers 404 for it; the grace only lets in-flight work that looked it up
/// finish against the same record.
pub(crate) const STOPPED_VM_CACHE_GRACE: Duration = Duration::from_secs(300);

/// A local alive VM the meter needs: id, key id, and tenant.
type AliveVm = (Uuid, Option<String>, Option<String>);

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
            .map(|v| (v.id, v.api_key_id.clone(), v.owner_key.clone()))
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
    for (vm_id, api_key_id, owner_key) in alive {
        let (Some(api_key_id), Some(owner_key)) = (api_key_id, owner_key) else {
            continue;
        };
        // Process death ends usage even if teardown retains runtime resources
        // for retry (nftables, journal or conntrack failure). The supervisor
        // records this boundary before beginning any resource cleanup.
        match state.supervisor.runtime_stopped_at(vm_id) {
            Ok(Some(end)) => {
                let Ok(Some(start)) = store.get_billing_watermark(vm_id) else {
                    continue;
                };
                let Ok(Some(end)) = store.freeze_billing_end(vm_id, end) else {
                    continue;
                };
                if end > start {
                    let event = runtime_event(state, vm_id, api_key_id, owner_key, start, end);
                    let _ = store.commit_runtime_usage(&event, true);
                } else {
                    let _ = store.clear_billing_watermark(vm_id);
                }
                continue;
            }
            Err(_) => continue, // Unreadable boundary must never extend billing.
            Ok(None) => {}
        }
        // Check under the store mutex, immediately before metering, so a
        // stale cache scan cannot bill a stopped lifecycle if persisting its
        // frozen end failed. Lifecycle writers release their mutex before I/O.
        let closed = state
            .lifecycle
            .lock()
            .map(|lifecycle| {
                matches!(
                    lifecycle.get(&vm_id),
                    Some(
                        LifecycleState::Terminal { .. }
                            | LifecycleState::Hibernating {
                                stopped_at: Some(_),
                                ..
                            }
                    )
                )
            })
            .unwrap_or(true);
        if closed || !matches!(store.get_billing_end(vm_id), Ok(None)) {
            continue;
        }
        let Ok(Some(start)) = store.get_billing_watermark(vm_id) else {
            continue;
        };
        let seconds = (now - start).num_milliseconds() as f64 / 1000.0;
        if seconds <= 0.0 {
            continue;
        }
        let event = runtime_event(state, vm_id, api_key_id, owner_key, start, now);
        let _ = store.commit_runtime_usage(&event, false);
    }
}

/// Emit the final runtime interval when a VM stops, then drop its watermark.
/// Called after successful teardown; failures keep the interval open for retry.
#[cfg(test)]
pub fn meter_vm_final(state: &AppState, vm_id: Uuid) -> Result<(), tarit_types::OrchError> {
    meter_vm_final_at(state, vm_id, Utc::now())
}

pub(crate) fn meter_vm_final_at(
    state: &AppState,
    vm_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(), tarit_types::OrchError> {
    use tarit_types::OrchError;
    let now = state.supervisor.runtime_stopped_at(vm_id)?.unwrap_or(now);
    let vm = state
        .vm_cache
        .read()
        .map_err(|_| OrchError::Internal("vm cache lock poisoned".into()))?
        .get(&vm_id)
        .cloned();
    let Some(vm) = vm else {
        return Ok(());
    };
    if vm.host_id != state.config.host_id {
        return Ok(());
    }
    let (Some(api_key_id), Some(owner_key)) = (vm.api_key_id.clone(), vm.owner_key.clone()) else {
        return Ok(());
    };
    let store = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?;
    // Missing means never published live, or already finalized. In particular,
    // deleting an Error VM must not reconstruct its entire lifetime.
    let Some(start) = store
        .get_billing_watermark(vm_id)
        .map_err(crate::api::store_err)?
    else {
        return Ok(());
    };
    let Some(end) = store
        .freeze_billing_end(vm_id, now)
        .map_err(crate::api::store_err)?
    else {
        return Ok(());
    };
    if end > start {
        let event = runtime_event(state, vm_id, api_key_id, owner_key, start, end);
        store
            .commit_runtime_usage(&event, true)
            .map_err(crate::api::store_err)?;
    } else {
        store
            .clear_billing_watermark(vm_id)
            .map_err(crate::api::store_err)?;
    }
    Ok(())
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
    execution_retention_hours: u64,
    stopped_vm_retention_hours: u64,
    mut shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let fleet = state.fleet.clone();
    let interval = Duration::from_secs(interval_secs.max(1));
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        let mut last_prune: Option<std::time::Instant> = None;
        let mut last_evict = std::time::Instant::now();
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
                prune_executions(&state, execution_retention_hours).await;
                prune_stopped_vms(&state, stopped_vm_retention_hours).await;
            }
            if last_evict.elapsed() >= CACHE_EVICT_INTERVAL {
                last_evict = std::time::Instant::now();
                evict_stopped_vms_from_cache(&state, STOPPED_VM_CACHE_GRACE);
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

/// Rows deleted per store lock while pruning executions.
const EXECUTION_PRUNE_BATCH: usize = 2_000;

/// Drop finished executions (and their captured output) older than
/// `retention_hours`; 0 keeps them forever. Runs in short batches so creates
/// and commands are not held off the store behind one large delete.
pub(crate) fn execution_retention_cutoff(
    retention_hours: u64,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if retention_hours == 0 {
        return None;
    }
    let hours = i64::try_from(retention_hours).unwrap_or(i64::MAX);
    Some(
        chrono::Duration::try_hours(hours)
            .and_then(|age| now.checked_sub_signed(age))
            .unwrap_or(DateTime::<Utc>::MIN_UTC),
    )
}

pub(crate) async fn prune_executions(state: &AppState, retention_hours: u64) {
    let Some(cutoff) = execution_retention_cutoff(retention_hours, Utc::now()) else {
        return;
    };
    // Independently prune both durable stores. A failed fleet pass must not
    // prevent local eviction; polling also enforces expiry before rehydration.
    if let Some(fleet) = &state.fleet {
        loop {
            match fleet
                .prune_finished_executions(cutoff, EXECUTION_PRUNE_BATCH)
                .await
            {
                Ok(deleted) if deleted == EXECUTION_PRUNE_BATCH => tokio::task::yield_now().await,
                Ok(_) => break,
                Err(error) => {
                    tracing::warn!(%error, "fleet execution record prune failed");
                    break;
                }
            }
        }
    }
    let mut total = 0usize;
    loop {
        let deleted = match state.store.lock() {
            Ok(store) => store.prune_finished_executions(cutoff, EXECUTION_PRUNE_BATCH),
            Err(_) => return,
        };
        match deleted {
            Ok(deleted) => {
                total += deleted;
                if deleted < EXECUTION_PRUNE_BATCH {
                    break;
                }
            }
            Err(error) => {
                tracing::warn!(%error, "execution record prune failed");
                return;
            }
        }
        tokio::task::yield_now().await;
    }
    // SQLite and the polling cache have the same retention boundary. Prune
    // even when SQLite deleted zero rows (e.g. an earlier cleanup already ran).
    if let Ok(mut cache) = state.exec_cache.write() {
        cache.retain(|_, record| !record.is_expired(cutoff));
    }
    if total > 0 {
        tracing::info!(
            deleted = total,
            retention_hours,
            "pruned finished executions"
        );
    }
}

/// Whether a cached record is a deleted VM old enough to leave the cache.
pub(crate) fn stopped_vm_is_evictable(
    vm: &tarit_types::VmRecord,
    now: DateTime<Utc>,
    grace: Duration,
) -> bool {
    vm.status == VmStatus::Stopped
        && chrono::Duration::from_std(grace)
            .ok()
            .and_then(|grace| now.checked_sub_signed(grace))
            .is_some_and(|cutoff| vm.updated_at < cutoff)
}

/// Drop deleted VMs from the in-memory cache once `grace` has passed and no
/// lifecycle work still holds them. The API stops serving a VM as soon as it
/// is stopped, so without this the cache (and every scan of it) grew with
/// every VM ever deleted. Durable rows are pruned separately.
pub(crate) fn evict_stopped_vms_from_cache(state: &AppState, grace: Duration) -> usize {
    let now = Utc::now();
    let candidates = match state.vm_cache.read() {
        Ok(cache) => cache
            .values()
            .filter(|vm| stopped_vm_is_evictable(vm, now, grace))
            .map(|vm| vm.id)
            .collect::<Vec<_>>(),
        Err(_) => return 0,
    };
    if candidates.is_empty() {
        return 0;
    }
    let candidates = match state.lifecycle.lock() {
        Ok(lifecycle) => candidates
            .into_iter()
            .filter(|id| !lifecycle.contains_key(id))
            .collect::<Vec<_>>(),
        Err(_) => return 0,
    };
    let candidates = candidates
        .into_iter()
        .filter(|id| !state.supervisor.is_running(*id))
        .collect::<Vec<_>>();
    let Ok(mut cache) = state.vm_cache.write() else {
        return 0;
    };
    let mut evicted = 0;
    for id in candidates {
        // The id may have been reused by a create since the scan.
        if cache
            .get(&id)
            .is_some_and(|vm| stopped_vm_is_evictable(vm, now, grace))
        {
            cache.remove(&id);
            evicted += 1;
        }
    }
    if evicted > 0 {
        tracing::debug!(evicted, "dropped deleted VMs from the VM cache");
    }
    evicted
}

/// Rows deleted per store lock while pruning deleted VMs.
const STOPPED_VM_PRUNE_BATCH: usize = 2_000;

/// Drop durable rows of VMs deleted more than `retention_hours` ago (0 keeps
/// them forever), then the committed fork records of children that are gone.
/// While a row exists its id cannot be reused, so this is also how long a
/// client-chosen VM or fork id stays reserved.
async fn prune_stopped_vms(state: &AppState, retention_hours: u64) {
    if retention_hours == 0 {
        return;
    }
    let hours = i64::try_from(retention_hours).unwrap_or(i64::MAX);
    let cutoff = chrono::Duration::try_hours(hours)
        .and_then(|age| Utc::now().checked_sub_signed(age))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let mut vms = 0usize;
    let mut forks = 0usize;
    for prune_forks in [false, true] {
        loop {
            let deleted = match state.store.lock() {
                Ok(store) if prune_forks => {
                    store.prune_committed_fork_operations(cutoff, STOPPED_VM_PRUNE_BATCH)
                }
                Ok(store) => store.prune_stopped_vms(cutoff, STOPPED_VM_PRUNE_BATCH),
                Err(_) => return,
            };
            match deleted {
                Ok(deleted) => {
                    if prune_forks {
                        forks += deleted;
                    } else {
                        vms += deleted;
                    }
                    if deleted < STOPPED_VM_PRUNE_BATCH {
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "deleted VM record prune failed");
                    return;
                }
            }
            tokio::task::yield_now().await;
        }
    }
    if vms > 0 || forks > 0 {
        tracing::info!(vms, forks, retention_hours, "pruned records of deleted VMs");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests::{insert_vm, test_state};

    fn owned_vm(state: &AppState, status: VmStatus) -> Uuid {
        let id = Uuid::new_v4();
        insert_vm(state, id, "tenant", status);
        let mut cache = state.vm_cache.write().unwrap();
        cache.get_mut(&id).unwrap().api_key_id = Some("key".into());
        id
    }

    #[test]
    fn runtime_finalization_is_consuming_and_failed_boot_is_not_billed() {
        let state = test_state();
        let failed = owned_vm(&state, VmStatus::Creating);
        state
            .vm_cache
            .write()
            .unwrap()
            .get_mut(&failed)
            .unwrap()
            .status = VmStatus::Error;
        meter_vm_final(&state, failed).unwrap();
        assert!(state
            .store
            .lock()
            .unwrap()
            .list_unsent_usage(100)
            .unwrap()
            .is_empty());

        let running = owned_vm(&state, VmStatus::Running);
        let start = Utc::now() - chrono::Duration::seconds(10);
        state
            .store
            .lock()
            .unwrap()
            .set_billing_watermark(running, start)
            .unwrap();
        meter_vm_final(&state, running).unwrap();
        state
            .vm_cache
            .write()
            .unwrap()
            .get_mut(&running)
            .unwrap()
            .status = VmStatus::Error;
        meter_vm_final(&state, running).unwrap();
        // Also exercise a stale meter snapshot after teardown: it must never
        // reopen a consumed interval while terminal publication is pending.
        state
            .vm_cache
            .write()
            .unwrap()
            .get_mut(&running)
            .unwrap()
            .status = VmStatus::Running;
        meter_runtime_once(&state);
        let events = state.store.lock().unwrap().list_unsent_usage(100).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].window_start, start);
        assert!(events[0].seconds.unwrap() >= 10.0);
    }

    #[test]
    fn confirmed_death_closes_usage_while_cleanup_leaves_status_running() {
        let state = test_state();
        let id = owned_vm(&state, VmStatus::Running);
        let start = Utc::now() - chrono::Duration::hours(1);
        state
            .store
            .lock()
            .unwrap()
            .set_billing_watermark(id, start)
            .unwrap();
        state.supervisor.record_runtime_stopped(id).unwrap();
        let end = state.supervisor.runtime_stopped_at(id).unwrap().unwrap();
        meter_runtime_once(&state);
        meter_runtime_once(&state);
        meter_vm_final_at(&state, id, end + chrono::Duration::hours(8)).unwrap();
        let store = state.store.lock().unwrap();
        let events = store.list_unsent_usage(10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].window_start, start);
        assert_eq!(events[0].window_end, end);
        assert_eq!(store.get_billing_watermark(id).unwrap(), None);
        assert_eq!(
            state.vm_cache.read().unwrap()[&id].status,
            VmStatus::Running
        );
    }

    #[test]
    fn runtime_final_retry_keeps_teardown_end_and_meter_cannot_extend_it() {
        let state = test_state();
        let id = owned_vm(&state, VmStatus::Running);
        let start = Utc::now() - chrono::Duration::seconds(20);
        let stopped_at = start + chrono::Duration::seconds(10);
        {
            let store = state.store.lock().unwrap();
            store.set_billing_watermark(id, start).unwrap();
            // This marker survives an unsuccessful final outbox transaction.
            store.freeze_billing_end(id, stopped_at).unwrap();
        }
        meter_runtime_once(&state);
        assert!(state
            .store
            .lock()
            .unwrap()
            .list_unsent_usage(100)
            .unwrap()
            .is_empty());
        meter_vm_final_at(&state, id, stopped_at + chrono::Duration::hours(8)).unwrap();
        let events = state.store.lock().unwrap().list_unsent_usage(100).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].window_end, stopped_at);
        assert_eq!(events[0].seconds, Some(10.0));
    }

    #[test]
    fn runtime_hibernate_wake_bills_only_live_intervals() {
        let state = test_state();
        let id = owned_vm(&state, VmStatus::Running);
        let first_start = Utc::now() - chrono::Duration::seconds(20);
        state
            .store
            .lock()
            .unwrap()
            .set_billing_watermark(id, first_start)
            .unwrap();
        meter_vm_final_at(&state, id, Utc::now() - chrono::Duration::seconds(10)).unwrap();
        let mut vm = state.store.lock().unwrap().get_vm(id).unwrap();
        vm.api_key_id = Some("key".into());
        vm.status = VmStatus::Hibernated;
        vm.revision += 1;
        state.store.lock().unwrap().insert_vm(&vm).unwrap();
        state.vm_cache.write().unwrap().insert(id, vm.clone());
        meter_runtime_once(&state);
        // Failed wake leaves Hibernated and does not reopen billing.
        meter_vm_final(&state, id).unwrap();
        assert!(state
            .store
            .lock()
            .unwrap()
            .get_billing_watermark(id)
            .unwrap()
            .is_none());
        vm.status = VmStatus::Running;
        vm.revision += 1;
        vm.updated_at = Utc::now() - chrono::Duration::seconds(2);
        let wake_start = vm.updated_at;
        state.store.lock().unwrap().insert_vm(&vm).unwrap();
        // Retry publication must not reset an already advanced watermark.
        state.store.lock().unwrap().insert_vm(&vm).unwrap();
        state.vm_cache.write().unwrap().insert(id, vm);
        meter_vm_final(&state, id).unwrap();
        let events = state.store.lock().unwrap().list_unsent_usage(100).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].window_start, first_start);
        assert_eq!(events[1].window_start, wake_start);
        assert!(events[0].window_end <= events[1].window_start);
    }
}
