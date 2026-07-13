//! Node-local VM operations shared by the public API (this node owns/places the
//! VM) and the internal peer API (a peer forwarded a request to this owner).
//!
//! Everything that actually touches the local supervisor + store lives here, so
//! the public router and the internal router never duplicate "do the work"
//! logic (DRY). Placement/routing decisions live in `cluster`; the public
//! handlers combine the two.

use std::sync::Arc;

use chrono::Utc;
use tarit_types::{CreateVmRequest, OrchError, VmRecord, VmStatus};
use uuid::Uuid;

use crate::api::{running_record, AppState, PendingStop, StoreWrite};
use crate::cluster;
use crate::image;
use crate::supervisor::{ShutdownSummary, VmSpawnConfig, VmmSupervisor};

/// Write a VM record: update the in-memory cache (read source of truth) and queue
/// the SQLite persist on the background writer (write-behind), so the create/update
/// hot path never blocks on the store mutex.
fn vm_put(state: &AppState, rec: &VmRecord) {
    if let Ok(mut c) = state.vm_cache.write() {
        c.insert(rec.id, rec.clone());
    }
    let _ = state.store_tx.send(StoreWrite::Vm(rec.clone()));
}

fn vm_get(state: &AppState, id: Uuid) -> Result<VmRecord, OrchError> {
    state
        .vm_cache
        .read()
        .ok()
        .and_then(|c| c.get(&id).cloned())
        .ok_or_else(|| OrchError::NotFound(format!("vm {id} not found")))
}

fn vm_set_status(state: &AppState, id: Uuid, status: VmStatus) -> Result<VmRecord, OrchError> {
    let rec = {
        let mut c = state
            .vm_cache
            .write()
            .map_err(|_| OrchError::Internal("vm cache".into()))?;
        let r = c
            .get_mut(&id)
            .ok_or_else(|| OrchError::NotFound(format!("vm {id} not found")))?;
        r.status = status;
        r.updated_at = Utc::now();
        r.clone()
    };
    let _ = state.store_tx.send(StoreWrite::Vm(rec.clone()));
    Ok(rec)
}

fn stopped_record(state: &AppState, id: Uuid) -> Result<VmRecord, OrchError> {
    let mut record = state
        .vm_cache
        .read()
        .map_err(|_| OrchError::Internal("vm cache".into()))?
        .get(&id)
        .cloned()
        .ok_or_else(|| OrchError::NotFound(format!("vm {id} not found")))?;
    record.status = VmStatus::Stopped;
    record.updated_at = Utc::now();
    Ok(record)
}

fn commit_vm_record(state: &AppState, record: VmRecord) -> Result<(), OrchError> {
    state
        .vm_cache
        .write()
        .map_err(|_| OrchError::Internal("vm cache".into()))?
        .insert(record.id, record);
    Ok(())
}

async fn persist_stopped_record(state: &AppState, record: VmRecord) -> Result<(), OrchError> {
    let (completion, persisted) = tokio::sync::oneshot::channel();
    state
        .store_tx
        .send(StoreWrite::VmDurable(record, completion))
        .map_err(|_| OrchError::Internal("store writer unavailable during shutdown".into()))?;
    persisted.await.map_err(|_| {
        OrchError::Internal("store writer dropped shutdown persistence confirmation".into())
    })?
}

fn persist_running_record_blocking(state: &AppState, record: VmRecord) -> Result<(), OrchError> {
    let (completion, persisted) = tokio::sync::oneshot::channel();
    state
        .store_tx
        .send(StoreWrite::VmDurable(record, completion))
        .map_err(|_| {
            OrchError::Internal("store writer unavailable during boot publication".into())
        })?;
    persisted.blocking_recv().map_err(|_| {
        OrchError::Internal("store writer dropped boot publication confirmation".into())
    })?
}

fn publish_running_record_blocking(state: &AppState, record: VmRecord) -> Result<(), OrchError> {
    let runtime = tokio::runtime::Handle::current();
    runtime.block_on(cluster::record_ownership_required(state, &record))?;
    if let Err(error) = persist_running_record_blocking(state, record.clone()) {
        let cleanup = runtime.block_on(cluster::clear_ownership(state, record.id));
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(OrchError::Internal(format!(
                "{error}; fleet ownership rollback after failed boot publication: {cleanup}"
            ))),
        };
    }
    commit_vm_record(state, record)
}

fn retain_pending_stop(state: &AppState, record: VmRecord) -> Result<(), OrchError> {
    state
        .pending_stops
        .lock()
        .map_err(|_| OrchError::Internal("pending stop transitions".into()))?
        .entry(record.id)
        .or_insert(PendingStop {
            record,
            sqlite_persisted: false,
        });
    Ok(())
}

async fn finish_pending_stop(state: &AppState, id: Uuid) -> Result<(), OrchError> {
    let pending = state
        .pending_stops
        .lock()
        .map_err(|_| OrchError::Internal("pending stop transitions".into()))?
        .get(&id)
        .cloned()
        .ok_or_else(|| {
            OrchError::Internal(format!("missing pending stop transition for VM {id}"))
        })?;

    if !pending.sqlite_persisted {
        persist_stopped_record(state, pending.record.clone()).await?;
        state
            .pending_stops
            .lock()
            .map_err(|_| OrchError::Internal("pending stop transitions".into()))?
            .get_mut(&id)
            .ok_or_else(|| {
                OrchError::Internal(format!("pending stop transition disappeared for VM {id}"))
            })?
            .sqlite_persisted = true;
    }
    commit_vm_record(state, pending.record)?;
    cluster::clear_ownership(state, id).await?;
    state.scheduler.on_local_vm_stopped();
    state
        .pending_stops
        .lock()
        .map_err(|_| OrchError::Internal("pending stop transitions".into()))?
        .remove(&id);
    Ok(())
}

async fn retry_pending_stops(state: &AppState) -> Vec<String> {
    let ids = match state.pending_stops.lock() {
        Ok(pending) => pending.keys().copied().collect::<Vec<_>>(),
        Err(_) => return vec!["pending stop transitions lock poisoned".into()],
    };
    let mut failures = Vec::new();
    for id in ids {
        if let Err(error) = finish_pending_stop(state, id).await {
            failures.push(format!(
                "VM {id} retained stopped transition, ownership, and scheduler reservation: {error}"
            ));
        }
    }
    failures
}

fn store_insert(state: &AppState, rec: &VmRecord) -> Result<(), OrchError> {
    vm_put(state, rec);
    Ok(())
}

fn store_update(state: &AppState, rec: &VmRecord) -> Result<(), OrchError> {
    vm_put(state, rec);
    Ok(())
}

fn retry_after_secs(admission_timeout_ms: u64) -> u64 {
    admission_timeout_ms.div_ceil(1000).max(1)
}

fn overloaded(message: impl Into<String>, retry_after_secs: u64) -> OrchError {
    OrchError::Overloaded {
        message: message.into(),
        retry_after_secs,
    }
}

/// Create a VM on THIS node exactly once: a warm-pool hand-out if available,
/// else reserve a concurrency slot and cold-boot. Returns `Conflict` when the
/// local host is at capacity — the caller (public create) orchestrates cluster
/// spill; the internal create just reports back so the placer tries another
/// peer. Writes the local store and the fleet ownership map on success.
pub async fn create_local(state: &AppState, req: &CreateVmRequest) -> Result<VmRecord, OrchError> {
    let req = {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock".into()))?;
        image::resolve_request_image(&store, req)?
    };
    let now = Utc::now();
    let spawn_cfg = VmSpawnConfig::from_defaults(&state.config, &req);
    let warm_enabled = state.config.warm_pool.enabled && req.id.is_none();

    if warm_enabled {
        let sup = Arc::clone(&state.supervisor);
        let want = spawn_cfg.clone();
        let taken = tokio::task::spawn_blocking(move || sup.take_warm(&want))
            .await
            .map_err(|e| OrchError::Internal(format!("join: {e}")))?;
        if let Some((id, pid, socket_path)) = taken {
            let record = running_record(
                state,
                &spawn_cfg,
                id,
                pid,
                &socket_path,
                req.owner_key.clone(),
                req.api_key_id.clone(),
                now,
            );
            store_insert(state, &record)?;
            cluster::record_ownership(state, &record).await;
            tracing::info!(id = %id, host = %state.config.host_id, "create: warm pool");
            return Ok(record);
        }
    }

    if state.scheduler.try_reserve() {
        let id = req.id.unwrap_or_else(Uuid::new_v4);
        let mut record = VmRecord {
            id,
            host_id: state.config.host_id.clone(),
            owner_key: req.owner_key.clone(),
            api_key_id: req.api_key_id.clone(),
            status: VmStatus::Creating,
            memory_mib: spawn_cfg.memory_mib,
            vcpus: spawn_cfg.vcpus,
            kernel_path: spawn_cfg.kernel_path.display().to_string(),
            rootfs_path: spawn_cfg
                .rootfs_path
                .as_ref()
                .map(|p| p.display().to_string()),
            cmdline: spawn_cfg.cmdline.clone(),
            socket_path: None,
            pid: None,
            created_at: now,
            updated_at: now,
        };
        store_insert(state, &record)?;

        let sup = Arc::clone(&state.supervisor);
        let cfg = spawn_cfg.clone();
        let publication_state = state.clone();
        let publication_record = record.clone();
        let spawned = tokio::task::spawn_blocking(move || {
            sup.spawn_vm_with_publication(id, cfg, move |pid, socket_path| {
                let mut record = publication_record;
                record.status = VmStatus::Running;
                record.pid = Some(pid);
                record.socket_path = Some(socket_path.display().to_string());
                record.updated_at = Utc::now();
                publish_running_record_blocking(&publication_state, record.clone())?;
                Ok(record)
            })
        })
        .await;
        return match spawned {
            Ok(Ok(record)) => {
                tracing::info!(id = %id, host = %state.config.host_id, "create: cold start");
                Ok(record)
            }
            Ok(Err(e)) => {
                if !state.supervisor.is_shutting_down() {
                    record.status = VmStatus::Error;
                    record.updated_at = Utc::now();
                    let _ = store_update(state, &record);
                    state.scheduler.release();
                }
                Err(e)
            }
            Err(e) => {
                if !state.supervisor.is_shutting_down() {
                    record.status = VmStatus::Error;
                    record.updated_at = Utc::now();
                    let _ = store_update(state, &record);
                    state.scheduler.release();
                }
                Err(OrchError::Internal(format!("join: {e}")))
            }
        };
    }

    Err(overloaded(
        "host at capacity",
        retry_after_secs(state.config.admission_timeout_ms),
    ))
}

/// Restore a VM from a node-local snapshot file on THIS node. Reserves a slot,
/// spawns `vmm serve`, and resumes. `Conflict` if the host is at capacity.
pub async fn restore_local(
    state: &AppState,
    snapshot_path: &str,
    id: Option<Uuid>,
    owner_key: Option<String>,
    api_key_id: Option<String>,
    caller_is_admin: bool,
) -> Result<VmRecord, OrchError> {
    // R-006: authorize the snapshot before reserving a slot or touching the VMM.
    verify_snapshot_access(state, snapshot_path, owner_key.as_deref(), caller_is_admin)?;
    if !state.scheduler.try_reserve() {
        return Err(overloaded(
            "host at capacity",
            retry_after_secs(state.config.admission_timeout_ms),
        ));
    }
    let id = id.unwrap_or_else(Uuid::new_v4);
    let now = Utc::now();
    let mut record = VmRecord {
        id,
        host_id: state.config.host_id.clone(),
        owner_key,
        api_key_id,
        status: VmStatus::Creating,
        memory_mib: 0,
        vcpus: 0,
        kernel_path: "(restored)".into(),
        rootfs_path: None,
        cmdline: "(restored)".into(),
        socket_path: None,
        pid: None,
        created_at: now,
        updated_at: now,
    };
    store_insert(state, &record)?;
    let path = snapshot_path.to_string();
    let sup = Arc::clone(&state.supervisor);
    let publication_state = state.clone();
    let publication_record = record.clone();
    let spawned = tokio::task::spawn_blocking(move || {
        sup.restore_vm_with_publication(id, &path, move |pid, socket_path| {
            let mut record = publication_record;
            record.status = VmStatus::Running;
            record.pid = Some(pid);
            record.socket_path = Some(socket_path.display().to_string());
            record.updated_at = Utc::now();
            publish_running_record_blocking(&publication_state, record.clone())?;
            Ok(record)
        })
    })
    .await;
    match spawned {
        Ok(Ok(record)) => {
            tracing::info!(id = %id, host = %state.config.host_id, "restore: from snapshot");
            Ok(record)
        }
        Ok(Err(e)) => {
            if !state.supervisor.is_shutting_down() {
                record.status = VmStatus::Error;
                record.updated_at = Utc::now();
                let _ = store_update(state, &record);
                state.scheduler.release();
            }
            Err(e)
        }
        Err(e) => {
            if !state.supervisor.is_shutting_down() {
                record.status = VmStatus::Error;
                record.updated_at = Utc::now();
                let _ = store_update(state, &record);
                state.scheduler.release();
            }
            Err(OrchError::Internal(format!("join: {e}")))
        }
    }
}

pub async fn exec_local(
    state: &AppState,
    vm_id: Uuid,
    command: String,
    timeout_ms: u64,
) -> Result<(i32, String, String, u64), OrchError> {
    ensure_vm_can_receive_live_op(state, vm_id)?;
    let sup = Arc::clone(&state.supervisor);
    tokio::task::spawn_blocking(move || sup.exec_vm(vm_id, &command, timeout_ms))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))?
}

pub async fn stop_local(state: &AppState, id: Uuid) -> Result<(), OrchError> {
    let _terminal_gate = state.terminal_transition_gate.lock().await;
    if state
        .pending_stops
        .lock()
        .map_err(|_| OrchError::Internal("pending stop transitions".into()))?
        .contains_key(&id)
    {
        return finish_pending_stop(state, id).await;
    }
    // Bill the final runtime interval before teardown, while the VM record (and
    // its owning key) is still in the cache, then drop its watermark.
    crate::usage::meter_vm_final(state, id);
    let sup = Arc::clone(&state.supervisor);
    tokio::task::spawn_blocking(move || sup.stop_vm(id))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
    retain_pending_stop(state, stopped_record(state, id)?)?;
    finish_pending_stop(state, id).await
}

pub async fn stop_all_local(state: &AppState) -> Result<ShutdownSummary, OrchError> {
    let _terminal_gate = state.terminal_transition_gate.lock().await;
    let mut failures = retry_pending_stops(state).await;
    let sup = Arc::clone(&state.supervisor);
    let outcome = tokio::task::spawn_blocking(move || sup.stop_all())
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))?;
    let (summary, failure) = match outcome {
        Ok(summary) => (summary, None),
        Err(failure) => (failure.summary, Some(failure.error)),
    };

    failures.extend(failure.into_iter().map(|error| error.to_string()));
    let stopped_ids = summary
        .running_ids
        .iter()
        .chain(summary.booting_ids.iter())
        .copied()
        .collect::<Vec<_>>();

    for id in stopped_ids {
        let record = match stopped_record(state, id) {
            Ok(record) => record,
            Err(error) => {
                failures.push(format!(
                    "VM {id} shutdown transition retained scheduler and ownership reservation: {error}"
                ));
                continue;
            }
        };
        if let Err(error) = retain_pending_stop(state, record) {
            failures.push(format!(
                "VM {id} shutdown transition retained scheduler and ownership reservation: {error}"
            ));
            continue;
        }
        if let Err(error) = finish_pending_stop(state, id).await {
            failures.push(format!(
                "VM {id} shutdown transition retained scheduler and ownership reservation: {error}"
            ));
        }
    }
    for _ in 0..summary.warm + summary.internal_booting {
        state.scheduler.on_local_vm_stopped();
    }

    if failures.is_empty() {
        Ok(summary)
    } else {
        Err(OrchError::Internal(failures.join("; ")))
    }
}

pub async fn pause_local(state: &AppState, id: Uuid) -> Result<VmRecord, OrchError> {
    vm_op(state, id, |sup, id| sup.pause_vm(id), VmStatus::Paused).await
}

pub async fn resume_local(state: &AppState, id: Uuid) -> Result<VmRecord, OrchError> {
    vm_op(state, id, |sup, id| sup.resume_vm(id), VmStatus::Running).await
}

pub async fn snapshot_local(state: &AppState, id: Uuid, diff: bool) -> Result<String, OrchError> {
    ensure_vm_can_receive_live_op(state, id)?;
    let sup = Arc::clone(&state.supervisor);
    let path = tokio::task::spawn_blocking(move || sup.snapshot_vm(id, diff))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
    // R-006: record who owns this snapshot file so a later restore can verify
    // tenant access before the path is handed to the VMM. Fail closed if the
    // record cannot be written, so we never create a snapshot that only an
    // admin could restore.
    let vm = vm_get(state, id)?;
    let record = tarit_store::SnapshotRecord {
        path: path.clone(),
        host_id: state.config.host_id.clone(),
        owner_key: vm.owner_key.clone(),
        api_key_id: vm.api_key_id.clone(),
        vm_id: id,
        created_at: Utc::now(),
    };
    state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .insert_snapshot(&record)
        .map_err(crate::api::store_err)?;
    Ok(path)
}

/// R-006: confirm the caller may restore the snapshot at `snapshot_path`.
///
/// A snapshot is a first-class owned record. A non-admin caller may only
/// restore a snapshot their own tenant created; an unknown path (no ownership
/// record) is refused so a tenant cannot point restore at an arbitrary host
/// file or another tenant's snapshot. Admins may restore any path, including
/// raw operator-supplied paths that have no record.
fn verify_snapshot_access(
    state: &AppState,
    snapshot_path: &str,
    caller_owner: Option<&str>,
    caller_is_admin: bool,
) -> Result<(), OrchError> {
    let snapshot = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .get_snapshot(snapshot_path)
        .map_err(crate::api::store_err)?;
    if caller_is_admin {
        return Ok(());
    }
    match snapshot {
        Some(rec) if caller_owner.is_some() && rec.owner_key.as_deref() == caller_owner => Ok(()),
        Some(_) => Err(OrchError::Forbidden(
            "snapshot belongs to another tenant".into(),
        )),
        None => Err(OrchError::Forbidden(
            "unknown snapshot; restore requires a snapshot created by your tenant".into(),
        )),
    }
}

pub async fn egress_local(
    state: &AppState,
    id: Uuid,
    allowlist: Vec<String>,
    allow_existing: bool,
) -> Result<usize, OrchError> {
    ensure_vm_can_receive_live_op(state, id)?;
    let sup = Arc::clone(&state.supervisor);
    tokio::task::spawn_blocking(move || sup.update_egress(id, allowlist, allow_existing))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))?
}

pub fn get_local(state: &AppState, id: Uuid) -> Result<VmRecord, OrchError> {
    vm_get(state, id)
}

/// Live VMM status for a locally-owned VM (state/uptime/vcpus/mem/vcpu_alive),
/// queried from the `vmm serve` process over its UDS.
pub async fn status_local(state: &AppState, id: Uuid) -> Result<serde_json::Value, OrchError> {
    ensure_vm_can_receive_live_op(state, id)?;
    let sup = Arc::clone(&state.supervisor);
    let status = tokio::task::spawn_blocking(move || sup.status_vm(id))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
    serde_json::to_value(status).map_err(|e| OrchError::Internal(format!("status encode: {e}")))
}

async fn vm_op<F>(
    state: &AppState,
    id: Uuid,
    op: F,
    new_status: VmStatus,
) -> Result<VmRecord, OrchError>
where
    F: FnOnce(&VmmSupervisor, Uuid) -> Result<(), OrchError> + Send + 'static,
{
    ensure_vm_can_receive_live_op(state, id)?;
    let sup = Arc::clone(&state.supervisor);
    tokio::task::spawn_blocking(move || op(&sup, id))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
    vm_set_status(state, id, new_status)
}

fn ensure_vm_can_receive_live_op(state: &AppState, id: Uuid) -> Result<(), OrchError> {
    let record = get_local(state, id)?;
    if record.status == VmStatus::Stopped {
        return Err(OrchError::Conflict(format!("vm {id} is stopped")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiKeyRegistry, ApiRole, AutoscaleConfig, Config, WarmPoolConfig};
    use crate::metrics::Metrics;
    use crate::peer::PeerClient;
    use crate::pty::PtyRegistry;
    use crate::scheduler::Scheduler;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::{Mutex, RwLock};
    use tarit_store::Store;

    #[test]
    fn ordinary_delete_writer_failure_keeps_a_retryable_transition_and_reservation() {
        let (state, mut writes) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        assert!(state.scheduler.try_reserve());
        let runtime = test_runtime();
        runtime.block_on(async {
            let writer = tokio::spawn(async move {
                let StoreWrite::VmDurable(_, completion) = writes.recv().await.unwrap() else {
                    panic!("ordinary stop must use the durable lifecycle writer");
                };
                completion
                    .send(Err(OrchError::Internal("injected SQLite failure".into())))
                    .unwrap();
            });

            let error = stop_local(&state, id)
                .await
                .expect_err("ordinary stop must fail when SQLite rejects its stopped record");
            writer.await.unwrap();

            assert!(error.to_string().contains("injected SQLite failure"));
            assert!(state.pending_stops.lock().unwrap().contains_key(&id));
            assert_eq!(vm_get(&state, id).unwrap().status, VmStatus::Running);
            assert_eq!(state.scheduler.local_capacity(1, 1).sandbox_count, 1);
        });
        drop(runtime);
        drop(state);
    }

    #[test]
    fn later_stop_retries_pending_persistence_without_releasing_early() {
        let (state, mut writes) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        assert!(state.scheduler.try_reserve());
        let durable_attempts = Arc::new(AtomicUsize::new(0));
        let writer_attempts = Arc::clone(&durable_attempts);
        let runtime = test_runtime();
        runtime.block_on(async {
            let writer = tokio::spawn(async move {
                for result in [
                    Err(OrchError::Internal("injected SQLite failure".into())),
                    Ok(()),
                ] {
                    let StoreWrite::VmDurable(_, completion) = writes.recv().await.unwrap() else {
                        panic!("terminal transitions must stay on the durable writer path");
                    };
                    writer_attempts.fetch_add(1, Ordering::SeqCst);
                    completion.send(result).unwrap();
                }
            });

            assert!(stop_local(&state, id).await.is_err());
            assert!(state.pending_stops.lock().unwrap().contains_key(&id));
            assert_eq!(state.scheduler.local_capacity(1, 1).sandbox_count, 1);

            stop_local(&state, id)
                .await
                .expect("a later stop must retry only the retained stopped transition");
            writer.await.unwrap();

            assert_eq!(durable_attempts.load(Ordering::SeqCst), 2);
            assert!(!state.pending_stops.lock().unwrap().contains_key(&id));
            assert_eq!(vm_get(&state, id).unwrap().status, VmStatus::Stopped);
            assert_eq!(state.scheduler.local_capacity(1, 1).sandbox_count, 0);
        });
        drop(runtime);
        drop(state);
    }

    fn test_state_with_durable_writer(
    ) -> (AppState, tokio::sync::mpsc::UnboundedReceiver<StoreWrite>) {
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            api_keys: ApiKeyRegistry::from_plaintext_entries(vec![(
                "test-key".into(),
                "test".into(),
                ApiRole::Admin,
                0,
            )])
            .unwrap(),
            host_id: "test-host".into(),
            vmm_bin: PathBuf::from("true"),
            kernel: PathBuf::from("kernel"),
            rootfs: PathBuf::from("rootfs"),
            socket_dir: PathBuf::from("target/taritd-ops-test/sockets"),
            db_path: PathBuf::from("target/taritd-ops-test/fleet.db"),
            net_state_path: PathBuf::from("target/taritd-ops-test/net-state.json"),
            images_dir: PathBuf::from("target/taritd-ops-test/images"),
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
            ssh_gateway_host_key_path: PathBuf::from("target/taritd-ops-test/ssh_host"),
        };
        let (store_tx, store_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            AppState {
                config: config.clone(),
                store: Arc::new(Mutex::new(Store::open(":memory:").unwrap())),
                exec_cache: Arc::new(RwLock::new(HashMap::new())),
                vm_cache: Arc::new(RwLock::new(HashMap::new())),
                store_tx,
                pending_stops: Arc::new(Mutex::new(HashMap::new())),
                terminal_transition_gate: Arc::new(tokio::sync::Mutex::new(())),
                pty_registry: Arc::new(PtyRegistry::default()),
                supervisor: Arc::new(VmmSupervisor::new(config.clone())),
                scheduler: Arc::new(Scheduler::new(config)),
                peer: Arc::new(PeerClient::new("peer-secret".into())),
                fleet: None,
                metrics: Arc::new(Metrics::default()),
            },
            store_rx,
        )
    }

    fn insert_running_vm(state: &AppState) -> Uuid {
        let id = Uuid::new_v4();
        let now = Utc::now();
        state.vm_cache.write().unwrap().insert(
            id,
            VmRecord {
                id,
                host_id: state.config.host_id.clone(),
                owner_key: Some("test".into()),
                api_key_id: None,
                status: VmStatus::Running,
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
        id
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
}
