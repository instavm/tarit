//! Node-local VM operations shared by the public API (this node owns/places the
//! VM) and the internal peer API (a peer forwarded a request to this owner).
//!
//! Everything that actually touches the local supervisor + store lives here, so
//! the public router and the internal router never duplicate "do the work"
//! logic (DRY). Placement/routing decisions live in `cluster`; the public
//! handlers combine the two.

use std::sync::Arc;

use chrono::Utc;
use tarit_types::{CreateVmRequest, OrchError, VmRecord, VmStartupPath, VmStatus};
use uuid::Uuid;

use crate::api::{
    running_record, AppState, CreatingPhase, LifecycleState, PublicationPhase, StoreWrite,
    TerminalPhase,
};
#[cfg(test)]
use crate::api::{LifecycleFault, LifecyclePause, LifecyclePauseControl};
use crate::cluster;
use crate::image;
use crate::supervisor::{
    OwnedTaskControl, PublicationFailure, ShutdownSummary, SpawnPurpose, VmSpawnConfig,
    VmmSupervisor, WarmClaimOutcome,
};

const LIVE_CONTROL_STATUSES: &[VmStatus] =
    &[VmStatus::Running, VmStatus::Paused, VmStatus::Suspended];

fn vm_get(state: &AppState, id: Uuid) -> Result<VmRecord, OrchError> {
    state
        .vm_cache
        .read()
        .ok()
        .and_then(|c| c.get(&id).cloned())
        .ok_or_else(|| OrchError::NotFound(format!("vm {id} not found")))
}

async fn vm_set_status(
    state: &AppState,
    id: Uuid,
    status: VmStatus,
) -> Result<VmRecord, OrchError> {
    let rec = {
        let c = state
            .vm_cache
            .read()
            .map_err(|_| OrchError::Internal("vm cache".into()))?;
        let mut r = c
            .get(&id)
            .cloned()
            .ok_or_else(|| OrchError::NotFound(format!("vm {id} not found")))?;
        r.status = status;
        r.revision = r
            .revision
            .checked_add(1)
            .ok_or_else(|| OrchError::Internal(format!("VM {id} revision exhausted")))?;
        r.updated_at = Utc::now();
        r
    };
    // Match boot publication ordering: global ownership first, then durable
    // local state, and only then the read cache. A retry at the same revision
    // is idempotent in both stores; stale queued records cannot overwrite it.
    claim_lifecycle_ownership(state, &rec).await?;
    #[cfg(test)]
    if take_lifecycle_fault(state, LifecycleFault::SQLite) {
        return Err(OrchError::Internal("injected SQLite failure".into()));
    }
    state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .insert_vm(&rec)
        .map_err(crate::api::store_err)?;
    commit_vm_record(state, rec.clone())?;
    refresh_running_lifecycle(state, &rec)?;
    Ok(rec)
}

/// Keep the `Running` lifecycle record aligned with the newest committed
/// revision so terminal transitions never derive from a stale record. Other
/// lifecycle phases own their records and are left untouched.
fn refresh_running_lifecycle(state: &AppState, record: &VmRecord) -> Result<(), OrchError> {
    let mut lifecycle = state
        .lifecycle
        .lock()
        .map_err(|_| OrchError::Internal("lifecycle state lock poisoned".into()))?;
    if let Some(current @ LifecycleState::Running { .. }) = lifecycle.get_mut(&record.id) {
        *current = LifecycleState::Running {
            record: record.clone(),
        };
    }
    Ok(())
}

/// Fence a failed live transition with a newer record for the state restored in
/// the VMM. Revision N+2 supersedes both the prior N record and any partially
/// published target at N+1, so a later retry cannot be rejected by fleet
/// fencing after the VMM rollback succeeded.
async fn compensate_vm_status(
    state: &AppState,
    prior: &VmRecord,
    observed_status: VmStatus,
) -> Result<VmRecord, OrchError> {
    let mut compensation = prior.clone();
    compensation.status = observed_status;
    compensation.revision = prior
        .revision
        .checked_add(2)
        .ok_or_else(|| OrchError::Internal(format!("VM {} revision exhausted", prior.id)))?;
    compensation.updated_at = Utc::now();
    claim_lifecycle_ownership(state, &compensation).await?;
    state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .insert_vm(&compensation)
        .map_err(crate::api::store_err)?;
    commit_vm_record(state, compensation.clone())?;
    refresh_running_lifecycle(state, &compensation)?;
    Ok(compensation)
}

fn control_status(status: tarit_vmm_client::VmState) -> Result<VmStatus, OrchError> {
    match status {
        tarit_vmm_client::VmState::Running => Ok(VmStatus::Running),
        tarit_vmm_client::VmState::Paused => Ok(VmStatus::Paused),
        tarit_vmm_client::VmState::Suspended => Ok(VmStatus::Suspended),
        tarit_vmm_client::VmState::Created | tarit_vmm_client::VmState::Stopped => {
            Err(OrchError::Internal(format!(
                "VMM reported non-live state {status:?} during lifecycle reconciliation"
            )))
        }
    }
}

async fn observe_and_compensate_vm_status(
    state: &AppState,
    prior: &VmRecord,
) -> Result<VmRecord, OrchError> {
    let supervisor = Arc::clone(&state.supervisor);
    let id = prior.id;
    let observed = tokio::task::spawn_blocking(move || supervisor.status_vm(id))
        .await
        .map_err(|error| OrchError::Internal(format!("status reconciliation join: {error}")))??;
    compensate_vm_status(state, prior, control_status(observed.state)?).await
}

async fn reconcile_snapshot_pause_failure(
    state: &AppState,
    prior: &VmRecord,
    primary: OrchError,
) -> OrchError {
    if prior.status != VmStatus::Running {
        return primary;
    }
    let supervisor = Arc::clone(&state.supervisor);
    let id = prior.id;
    let observed = tokio::task::spawn_blocking(move || supervisor.status_vm(id)).await;
    let observed = match observed {
        Ok(Ok(status)) => match control_status(status.state) {
            Ok(status) => status,
            Err(error) => {
                return retain_snapshot_reconciliation(
                    state,
                    prior,
                    primary,
                    format!("snapshot pause reconciliation rejected VMM state: {error}"),
                );
            }
        },
        Ok(Err(error)) => {
            return retain_snapshot_reconciliation(
                state,
                prior,
                primary,
                format!("VMM state could not be observed: {error}"),
            );
        }
        Err(error) => {
            return retain_snapshot_reconciliation(
                state,
                prior,
                primary,
                format!("VMM status task failed: {error}"),
            );
        }
    };
    if observed == VmStatus::Running {
        return primary;
    }
    match compensate_vm_status(state, prior, observed).await {
        Ok(record) => {
            if let Err(error) = set_lifecycle_state(
                state,
                prior.id,
                LifecycleState::Running {
                    record: record.clone(),
                },
            ) {
                return OrchError::Internal(format!(
                    "{primary}; VM was fenced {} at revision {} but stable lifecycle publication failed: {error}",
                    observed.as_str(),
                    record.revision
                ));
            }
            OrchError::Internal(format!(
                "{primary}; VM was fenced {} after snapshot compensation",
                observed.as_str()
            ))
        }
        Err(compensation) => retain_snapshot_reconciliation(
            state,
            prior,
            primary,
            format!(
                "observed VM state {} but durable fencing failed: {compensation}",
                observed.as_str()
            ),
        ),
    }
}

fn retain_snapshot_reconciliation(
    state: &AppState,
    prior: &VmRecord,
    primary: OrchError,
    detail: String,
) -> OrchError {
    let retained = set_lifecycle_state(
        state,
        prior.id,
        LifecycleState::Reconciling {
            record: prior.clone(),
        },
    )
    .err()
    .map(|error| format!("; retaining reconciliation failed: {error}"))
    .unwrap_or_default();
    OrchError::Internal(format!(
        "{primary}; {detail}; VMM state remains unknown and retryable{retained}"
    ))
}

fn commit_vm_record(state: &AppState, record: VmRecord) -> Result<(), OrchError> {
    #[cfg(test)]
    if take_lifecycle_fault(state, LifecycleFault::CacheCommit) {
        return Err(OrchError::Internal("injected cache commit failure".into()));
    }
    let mut cache = state
        .vm_cache
        .write()
        .map_err(|_| OrchError::Internal("vm cache".into()))?;
    if cache
        .get(&record.id)
        .is_some_and(|current| current.revision > record.revision)
    {
        return Ok(());
    }
    cache.insert(record.id, record);
    Ok(())
}

/// The writer stops at the shutdown signal, but terminal transitions in the
/// drain/sweep window must still land durably.
async fn persist_stopped_record(state: &AppState, record: VmRecord) -> Result<(), OrchError> {
    #[cfg(test)]
    if take_lifecycle_fault(state, LifecycleFault::SQLite) {
        return Err(OrchError::Internal("injected SQLite failure".into()));
    }
    let (completion, persisted) = tokio::sync::oneshot::channel();
    let fallback = record.clone();
    let direct_insert = |record: &VmRecord| {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .insert_vm(record)
            .map_err(crate::api::store_err)
    };
    match state
        .store_tx
        .send(StoreWrite::VmDurable(record, completion))
        .await
    {
        // The writer can accept the send and still exit on the shutdown signal
        // before draining it. A dropped confirmation therefore falls back to
        // the direct durable write, which is idempotent if the writer already
        // committed the record.
        Ok(()) => match persisted.await {
            Ok(result) => result,
            Err(_) => direct_insert(&fallback),
        },
        Err(error) => {
            let StoreWrite::VmDurable(record, _) = error.0 else {
                unreachable!("only durable VM writes are sent here");
            };
            direct_insert(&record)
        }
    }
}

async fn persist_running_record(state: &AppState, record: VmRecord) -> Result<(), OrchError> {
    #[cfg(test)]
    if take_lifecycle_fault(state, LifecycleFault::SQLite) {
        return Err(OrchError::Internal("injected SQLite failure".into()));
    }
    let (completion, persisted) = tokio::sync::oneshot::channel();
    state
        .store_tx
        .send(StoreWrite::VmDurable(record, completion))
        .await
        .map_err(|_| {
            OrchError::Internal("store writer unavailable during boot publication".into())
        })?;
    persisted.await.map_err(|_| {
        OrchError::Internal("store writer dropped boot publication confirmation".into())
    })?
}

async fn claim_lifecycle_ownership(state: &AppState, record: &VmRecord) -> Result<(), OrchError> {
    #[cfg(test)]
    if take_lifecycle_fault(state, LifecycleFault::FleetClaim) {
        return Err(OrchError::Internal("injected fleet claim failure".into()));
    }
    cluster::record_ownership_required(state, record).await
}

async fn clear_lifecycle_ownership(state: &AppState, id: Uuid) -> Result<(), OrchError> {
    #[cfg(test)]
    if take_lifecycle_fault(state, LifecycleFault::FleetClear) {
        return Err(OrchError::Internal("injected fleet clear failure".into()));
    }
    cluster::clear_ownership(state, id).await
}

fn is_shutdown_rejection(cause: &OrchError) -> bool {
    matches!(
        cause,
        OrchError::Overloaded { message, .. } if message == "taritd is shutting down"
    )
}

/// An in-flight create/restore rejected because taritd is shutting down was
/// never acknowledged to the client, so it must leave no trace and surface the
/// original 429 shutdown cause. Tear down any VMM the boot started, then drive
/// the phased terminal transition (which releases the boot reservation, clears
/// fleet ownership, and is retried by the shutdown sweep on failure), and
/// finally erase the terminal tombstone the transition would otherwise leave.
async fn rollback_shutdown_rejected_lifecycle(
    state: &AppState,
    id: Uuid,
    task: Option<&OwnedTaskControl>,
    cause: OrchError,
) -> Result<(), OrchError> {
    let sup = Arc::clone(&state.supervisor);
    if let Err(teardown) = tokio::task::spawn_blocking(move || sup.stop_vm(id))
        .await
        .map_err(|error| OrchError::Internal(format!("shutdown rollback teardown join: {error}")))?
    {
        return Err(OrchError::Internal(format!(
            "{cause}; shutdown rollback retained VMM resources: {teardown}"
        )));
    }

    if let Err(cleanup) = finish_failed_boot(state, id).await {
        return Err(OrchError::Internal(format!(
            "{cause}; shutdown rollback retained lifecycle for terminal retry: {cleanup}"
        )));
    }

    state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .delete_vm(id)
        .map_err(crate::api::store_err)?;
    state
        .vm_cache
        .write()
        .map_err(|_| OrchError::Internal("vm cache".into()))?
        .remove(&id);
    if let Some(task) = task {
        task.mark_terminal_converged();
    }
    Err(cause)
}

fn lifecycle_state(state: &AppState, id: Uuid) -> Result<Option<LifecycleState>, OrchError> {
    state
        .lifecycle
        .lock()
        .map_err(|_| OrchError::Internal("lifecycle state lock poisoned".into()))
        .map(|lifecycle| lifecycle.get(&id).cloned())
}

fn set_lifecycle_state(
    state: &AppState,
    id: Uuid,
    lifecycle_state: LifecycleState,
) -> Result<(), OrchError> {
    state
        .lifecycle
        .lock()
        .map_err(|_| OrchError::Internal("lifecycle state lock poisoned".into()))?
        .insert(id, lifecycle_state);
    Ok(())
}

fn terminal_record(state: &AppState, id: Uuid, status: VmStatus) -> Result<VmRecord, OrchError> {
    // Base the terminal write on the newest committed record. The lifecycle
    // record can trail the cache when live transitions or partially failed
    // creations advanced the durable revision, and reusing that stale revision
    // would collide with the already-committed record in SQLite.
    let lifecycle = lifecycle_state(state, id)?.map(|lifecycle| lifecycle.record().clone());
    let cached = state.vm_cache.read().ok().and_then(|c| c.get(&id).cloned());
    let mut record = match (lifecycle, cached) {
        (Some(lifecycle), Some(cached)) if cached.revision > lifecycle.revision => cached,
        (Some(lifecycle), _) => lifecycle,
        (None, Some(cached)) => cached,
        (None, None) => return Err(OrchError::NotFound(format!("vm {id} not found"))),
    };
    record.status = status;
    record.revision = record
        .revision
        .checked_add(1)
        .ok_or_else(|| OrchError::Internal(format!("VM {id} revision exhausted")))?;
    record.updated_at = Utc::now();
    Ok(record)
}

async fn register_creating_record(state: &AppState, record: VmRecord) -> Result<(), OrchError> {
    let id = record.id;
    set_lifecycle_state(
        state,
        id,
        LifecycleState::Creating {
            record: record.clone(),
            phase: CreatingPhase::CacheVisible,
        },
    )?;
    commit_vm_record(state, record.clone())?;
    persist_running_record(state, record.clone()).await?;
    set_lifecycle_state(
        state,
        id,
        LifecycleState::Creating {
            record: record.clone(),
            phase: CreatingPhase::SQLitePersisted,
        },
    )?;
    claim_lifecycle_ownership(state, &record).await?;
    set_lifecycle_state(
        state,
        id,
        LifecycleState::Creating {
            record,
            phase: CreatingPhase::FleetClaimed,
        },
    )
}

async fn register_warm_creating_record(
    state: &AppState,
    record: VmRecord,
) -> Result<(), OrchError> {
    let id = record.id;
    let Err(error) = register_creating_record(state, record).await else {
        return Ok(());
    };

    // A warm VM remains parked until all Creating ownership is durable and
    // routable. Undo every partial user-visible registration on failure without
    // releasing the warm reservation, so the exact warm VM remains reusable.
    let rollback = async {
        clear_lifecycle_ownership(state, id).await?;
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .delete_vm(id)
            .map_err(crate::api::store_err)?;
        state
            .vm_cache
            .write()
            .map_err(|_| OrchError::Internal("vm cache".into()))?
            .remove(&id);
        state
            .lifecycle
            .lock()
            .map_err(|_| OrchError::Internal("lifecycle state lock poisoned".into()))?
            .remove(&id);
        Ok::<(), OrchError>(())
    }
    .await;
    match rollback {
        Ok(()) => Err(error),
        Err(rollback_error) => {
            // The registry must retain the warm VM for terminal cleanup when an
            // externally visible partial claim cannot be withdrawn.
            if let Ok(mut lifecycle) = state.lifecycle.lock() {
                if let Some(current) = lifecycle.get(&id).cloned() {
                    lifecycle.insert(
                        id,
                        LifecycleState::Abandoned {
                            record: current.record().clone(),
                        },
                    );
                }
            }
            state.supervisor.abandon_lifecycle(id);
            Err(OrchError::Internal(format!(
                "{error}; warm Creating registration rollback retained lifecycle: {rollback_error}"
            )))
        }
    }
}

async fn update_creating_record(state: &AppState, mut record: VmRecord) -> Result<(), OrchError> {
    let id = record.id;
    if let Some(current) = state
        .vm_cache
        .read()
        .map_err(|_| OrchError::Internal("vm cache".into()))?
        .get(&id)
    {
        record.revision = current
            .revision
            .checked_add(1)
            .ok_or_else(|| OrchError::Internal(format!("VM {id} revision exhausted")))?;
    }
    commit_vm_record(state, record.clone())?;
    persist_running_record(state, record.clone()).await?;
    claim_lifecycle_ownership(state, &record).await?;
    set_lifecycle_state(
        state,
        id,
        LifecycleState::Creating {
            record,
            phase: CreatingPhase::FleetClaimed,
        },
    )
}

async fn publish_running_record(
    state: &AppState,
    mut record: VmRecord,
) -> Result<(), PublicationFailure> {
    let id = record.id;
    if let Some(current) = state
        .vm_cache
        .read()
        .map_err(|_| PublicationFailure(OrchError::Internal("vm cache".into())))?
        .get(&id)
    {
        record.revision = current.revision.checked_add(1).ok_or_else(|| {
            PublicationFailure(OrchError::Internal(format!("VM {id} revision exhausted")))
        })?;
    }
    set_lifecycle_state(
        state,
        id,
        LifecycleState::Publishing {
            record: record.clone(),
            phase: PublicationPhase::NeedFleetUpdate,
        },
    )
    .map_err(PublicationFailure)?;
    #[cfg(test)]
    wait_lifecycle_pause(state, LifecyclePause::Fleet).await;
    claim_lifecycle_ownership(state, &record)
        .await
        .map_err(PublicationFailure)?;
    set_lifecycle_state(
        state,
        id,
        LifecycleState::Publishing {
            record: record.clone(),
            phase: PublicationPhase::FleetUpdated,
        },
    )
    .map_err(PublicationFailure)?;
    #[cfg(test)]
    wait_lifecycle_pause(state, LifecyclePause::SQLite).await;
    persist_running_record(state, record.clone())
        .await
        .map_err(PublicationFailure)?;
    set_lifecycle_state(
        state,
        id,
        LifecycleState::Publishing {
            record: record.clone(),
            phase: PublicationPhase::SQLitePersisted,
        },
    )
    .map_err(PublicationFailure)?;
    #[cfg(test)]
    wait_lifecycle_pause(state, LifecyclePause::Cache).await;
    commit_vm_record(state, record.clone()).map_err(PublicationFailure)?;
    set_lifecycle_state(
        state,
        id,
        LifecycleState::Publishing {
            record,
            phase: PublicationPhase::CacheVisible,
        },
    )
    .map_err(PublicationFailure)
}

async fn finish_publication(state: &AppState, id: Uuid) -> Result<(), OrchError> {
    loop {
        let LifecycleState::Publishing { record, phase } = lifecycle_state(state, id)?
            .ok_or_else(|| OrchError::NotFound(format!("vm {id} has no lifecycle state")))?
        else {
            return Ok(());
        };
        match phase {
            PublicationPhase::NeedFleetUpdate => {
                claim_lifecycle_ownership(state, &record).await?;
                set_lifecycle_state(
                    state,
                    id,
                    LifecycleState::Publishing {
                        record,
                        phase: PublicationPhase::FleetUpdated,
                    },
                )?;
            }
            PublicationPhase::FleetUpdated => {
                persist_running_record(state, record.clone()).await?;
                set_lifecycle_state(
                    state,
                    id,
                    LifecycleState::Publishing {
                        record,
                        phase: PublicationPhase::SQLitePersisted,
                    },
                )?;
            }
            PublicationPhase::SQLitePersisted => {
                commit_vm_record(state, record.clone())?;
                set_lifecycle_state(
                    state,
                    id,
                    LifecycleState::Publishing {
                        record,
                        phase: PublicationPhase::CacheVisible,
                    },
                )?;
            }
            PublicationPhase::CacheVisible => {
                return set_lifecycle_state(state, id, LifecycleState::Running { record });
            }
        }
    }
}

fn mark_running(state: &AppState, record: VmRecord) -> Result<(), OrchError> {
    set_lifecycle_state(state, record.id, LifecycleState::Running { record })
}

fn start_terminal_transition(
    state: &AppState,
    id: Uuid,
    status: VmStatus,
    release_reservation: bool,
) -> Result<(), OrchError> {
    let record = terminal_record(state, id, status)?;
    set_lifecycle_state(
        state,
        id,
        LifecycleState::Terminal {
            record,
            phase: if release_reservation {
                TerminalPhase::PersistRecordAndRelease
            } else {
                TerminalPhase::PersistRecordOnly
            },
        },
    )
}

async fn finish_terminal_transition(state: &AppState, id: Uuid) -> Result<(), OrchError> {
    loop {
        let LifecycleState::Terminal { record, phase } = lifecycle_state(state, id)?
            .ok_or_else(|| OrchError::NotFound(format!("vm {id} has no terminal lifecycle")))?
        else {
            return Ok(());
        };
        match phase {
            TerminalPhase::PersistRecordAndRelease | TerminalPhase::PersistRecordOnly => {
                persist_stopped_record(state, record.clone()).await?;
                set_lifecycle_state(
                    state,
                    id,
                    LifecycleState::Terminal {
                        record,
                        phase: if phase == TerminalPhase::PersistRecordAndRelease {
                            TerminalPhase::ClearFleetOwnershipAndRelease
                        } else {
                            TerminalPhase::ClearFleetOwnershipOnly
                        },
                    },
                )?;
            }
            TerminalPhase::ClearFleetOwnershipAndRelease
            | TerminalPhase::ClearFleetOwnershipOnly => {
                clear_lifecycle_ownership(state, id).await?;
                set_lifecycle_state(
                    state,
                    id,
                    LifecycleState::Terminal {
                        record,
                        phase: if phase == TerminalPhase::ClearFleetOwnershipAndRelease {
                            TerminalPhase::CommitCacheAndRelease
                        } else {
                            TerminalPhase::CommitCacheOnly
                        },
                    },
                )?;
            }
            TerminalPhase::CommitCacheAndRelease | TerminalPhase::CommitCacheOnly => {
                commit_vm_record(state, record.clone())?;
                set_lifecycle_state(
                    state,
                    id,
                    LifecycleState::Terminal {
                        record,
                        phase: if phase == TerminalPhase::CommitCacheAndRelease {
                            TerminalPhase::ReleaseReservation
                        } else {
                            TerminalPhase::Complete
                        },
                    },
                )?;
            }
            TerminalPhase::ReleaseReservation => {
                state.supervisor.release_reservation_after_terminal(id)?;
                set_lifecycle_state(
                    state,
                    id,
                    LifecycleState::Terminal {
                        record,
                        phase: TerminalPhase::Complete,
                    },
                )?;
            }
            TerminalPhase::Complete => {
                state
                    .lifecycle
                    .lock()
                    .map_err(|_| OrchError::Internal("lifecycle state lock poisoned".into()))?
                    .remove(&id);
                return Ok(());
            }
        }
    }
}

async fn finish_failed_boot(state: &AppState, id: Uuid) -> Result<(), OrchError> {
    let _terminal_gate = state.terminal_transition_gate.lock().await;
    if let Some(LifecycleState::Creating { phase, .. }) = lifecycle_state(state, id)? {
        tracing::debug!(?phase, %id, "finishing failed Creating lifecycle");
    }
    start_terminal_transition(state, id, VmStatus::Error, true)?;
    finish_terminal_transition(state, id).await
}

async fn retry_pending_lifecycle(state: &AppState) -> Vec<String> {
    let states = match state.lifecycle.lock() {
        Ok(lifecycle) => lifecycle
            .iter()
            .map(|(id, lifecycle)| (*id, lifecycle.clone()))
            .collect::<Vec<_>>(),
        Err(_) => return vec!["lifecycle state lock poisoned".into()],
    };
    let mut failures = Vec::new();
    for (id, lifecycle) in states {
        let result = match lifecycle {
            LifecycleState::Publishing { .. } => finish_publication(state, id).await,
            LifecycleState::Terminal { .. } => finish_terminal_transition(state, id).await,
            LifecycleState::Reconciling { .. } => {
                let gate = match state.supervisor.operation_gate(id) {
                    Ok(gate) => gate,
                    Err(OrchError::NotFound(_))
                        if !matches!(
                            lifecycle_state(state, id),
                            Ok(Some(LifecycleState::Reconciling { .. }))
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => {
                        failures.push(format!(
                            "VM {id} retained lifecycle state for retry: {error}"
                        ));
                        continue;
                    }
                };
                let _operation = gate.lock_owned().await;
                let record = match lifecycle_state(state, id) {
                    Ok(Some(LifecycleState::Reconciling { record })) => record,
                    Ok(_) => continue,
                    Err(error) => {
                        failures.push(format!(
                            "VM {id} retained lifecycle state for retry: {error}"
                        ));
                        continue;
                    }
                };
                observe_and_compensate_vm_status(state, &record)
                    .await
                    .and_then(|record| {
                        set_lifecycle_state(state, id, LifecycleState::Running { record })
                    })
            }
            LifecycleState::Creating { .. }
            | LifecycleState::Running { .. }
            | LifecycleState::Abandoned { .. } => continue,
        };
        if let Err(error) = result {
            failures.push(format!(
                "VM {id} retained lifecycle state for retry: {error}"
            ));
        }
    }
    failures
}

fn creating_record(
    state: &AppState,
    spawn_cfg: &VmSpawnConfig,
    id: Uuid,
    owner_key: Option<String>,
    api_key_id: Option<String>,
    now: chrono::DateTime<Utc>,
) -> VmRecord {
    VmRecord {
        id,
        host_id: state.config.host_id.clone(),
        owner_key,
        api_key_id,
        status: VmStatus::Creating,
        revision: 1,
        startup_path: None,
        memory_mib: spawn_cfg.memory_mib,
        vcpus: spawn_cfg.vcpus,
        kernel_path: spawn_cfg.kernel_path.display().to_string(),
        rootfs_path: spawn_cfg
            .rootfs_path
            .as_ref()
            .map(|path| path.display().to_string()),
        cmdline: spawn_cfg.cmdline.clone(),
        socket_path: None,
        pid: None,
        created_at: now,
        updated_at: now,
    }
}

async fn fail_create_or_restore(
    state: &AppState,
    id: Uuid,
    cause: OrchError,
) -> Result<(), OrchError> {
    if lifecycle_state(state, id)?.is_none() {
        return Err(cause);
    }
    if is_shutdown_rejection(&cause) {
        return rollback_shutdown_rejected_lifecycle(state, id, None, cause).await;
    }
    match finish_failed_boot(state, id).await {
        Ok(()) => Err(cause),
        Err(cleanup) => Err(OrchError::Internal(format!(
            "{cause}; retained Creating lifecycle for terminal retry: {cleanup}"
        ))),
    }
}

/// A DELETE/stop-all has marked a supervisor-owned lifecycle for cancellation.
/// Publication is never cancelled mid-await: the owner reaches this point only
/// after the current fleet/SQLite/cache operation has returned, then tears down
/// and durably clears ownership in terminal order.
async fn finish_cancelled_lifecycle<T>(
    state: &AppState,
    id: Uuid,
    task: &OwnedTaskControl,
    cause: OrchError,
) -> Result<T, OrchError>
where
    T: Send,
{
    let sup = Arc::clone(&state.supervisor);
    if let Err(error) = tokio::task::spawn_blocking(move || sup.stop_vm(id))
        .await
        .map_err(|error| {
            OrchError::Internal(format!("cancelled lifecycle teardown join: {error}"))
        })?
    {
        return Err(OrchError::Internal(format!(
            "{cause}; cancelled lifecycle teardown retained resources: {error}"
        )));
    }

    let terminal_result = match lifecycle_state(state, id)? {
        None => Ok(()),
        Some(LifecycleState::Terminal { .. }) => finish_terminal_transition(state, id)
            .await
            .map_err(|error| {
                OrchError::Internal(format!(
                    "{cause}; cancelled lifecycle terminal retry retained ownership: {error}"
                ))
            }),
        Some(_) => {
            start_terminal_transition(state, id, VmStatus::Stopped, true)?;
            finish_terminal_transition(state, id)
                .await
                .map_err(|error| {
                    OrchError::Internal(format!(
                        "{cause}; cancelled lifecycle terminal transition retained ownership: {error}"
                    ))
                })
        }
    };
    terminal_result?;
    task.mark_terminal_converged();
    Err(cause)
}

fn lifecycle_cancelled_error() -> OrchError {
    OrchError::Overloaded {
        message: "VM lifecycle cancelled by delete or shutdown".into(),
        retry_after_secs: 1,
    }
}

async fn cancel_unstarted_lifecycle<T>(
    state: &AppState,
    id: Uuid,
    ticket: &crate::supervisor::BootTicket,
    task: &OwnedTaskControl,
    cause: OrchError,
) -> Result<T, OrchError>
where
    T: Send,
{
    state.supervisor.abort_unstarted_boot(ticket).await;
    finish_cancelled_lifecycle(state, id, task, cause).await
}

/// The caller awaits only this result channel. The worker is registered with
/// the supervisor before spawning, so dropping an API or peer-RPC future cannot
/// cancel an in-flight fleet, SQLite, cache, or VMM operation.
async fn run_supervised_lifecycle<T, F, Fut>(
    state: &AppState,
    id: Uuid,
    operation: F,
) -> Result<T, OrchError>
where
    T: Send + 'static,
    F: FnOnce(Arc<OwnedTaskControl>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, OrchError>> + Send + 'static,
{
    state
        .supervisor
        .run_owned_task(id, SpawnPurpose::Live, operation)
        .await
}

/// Create a VM on THIS node exactly once: a warm-pool hand-out if available,
/// else reserve a concurrency slot and cold-boot. Returns `Conflict` when the
/// local host is at capacity — the caller (public create) orchestrates cluster
/// spill; the internal create just reports back so the placer tries another
/// peer. Writes the local store and the fleet ownership map on success.
pub async fn create_local(state: &AppState, req: &CreateVmRequest) -> Result<VmRecord, OrchError> {
    let id = req.id.unwrap_or_else(Uuid::new_v4);
    let state = state.clone();
    let req = req.clone();
    let worker_state = state.clone();
    run_supervised_lifecycle(&state, id, move |task| async move {
        create_local_owned(&worker_state, &req, id, &task).await
    })
    .await
}

async fn create_local_owned(
    state: &AppState,
    req: &CreateVmRequest,
    id: Uuid,
    task: &OwnedTaskControl,
) -> Result<VmRecord, OrchError> {
    let now = Utc::now();
    let unverified_cfg = VmSpawnConfig::from_defaults(&state.config, req);
    let warm_enabled = state.config.warm_pool.enabled && req.id.is_none() && req.image.is_none();

    if warm_enabled {
        let lifecycle_id = Arc::new(std::sync::Mutex::new(id));
        let publication_state = state.clone();
        let publication_cfg = unverified_cfg.clone();
        let owner_key = req.owner_key.clone();
        let api_key_id = req.api_key_id.clone();
        let registration_state = state.clone();
        let registration_cfg = unverified_cfg.clone();
        let registration_owner = req.owner_key.clone();
        let registration_api_key = req.api_key_id.clone();
        let registration_lifecycle_id = Arc::clone(&lifecycle_id);
        let taken = state
            .supervisor
            .take_warm_with_publication(
                &unverified_cfg,
                task,
                move |warm_id| {
                    let registration_state = registration_state.clone();
                    let mut record = creating_record(
                        &registration_state,
                        &registration_cfg,
                        warm_id,
                        registration_owner,
                        registration_api_key,
                        now,
                    );
                    record.startup_path = Some(VmStartupPath::Warm);
                    async move {
                        *registration_lifecycle_id.lock().map_err(|_| {
                            OrchError::Internal("warm lifecycle id lock poisoned".into())
                        })? = warm_id;
                        register_warm_creating_record(&registration_state, record).await
                    }
                },
                move |id, pid, socket_path| {
                    let mut record = running_record(
                        &publication_state,
                        &publication_cfg,
                        id,
                        pid,
                        &socket_path,
                        owner_key,
                        api_key_id,
                        now,
                    );
                    record.startup_path = Some(VmStartupPath::Warm);
                    async move {
                        publish_running_record(&publication_state, record.clone()).await?;
                        Ok(record)
                    }
                },
            )
            .await;
        match taken? {
            WarmClaimOutcome::Published(record) => {
                if task.is_cancelled() {
                    return finish_cancelled_lifecycle(
                        state,
                        record.id,
                        task,
                        lifecycle_cancelled_error(),
                    )
                    .await;
                }
                mark_running(state, record.clone())?;
                let id = record.id;
                tracing::info!(id = %id, host = %state.config.host_id, "create: warm pool");
                return Ok(record);
            }
            WarmClaimOutcome::NoMatch => {}
            WarmClaimOutcome::PreRuntimeFailure(error) => {
                if task.is_cancelled() {
                    let lifecycle_id = *lifecycle_id.lock().map_err(|_| {
                        OrchError::Internal("warm lifecycle id lock poisoned".into())
                    })?;
                    return finish_cancelled_lifecycle(state, lifecycle_id, task, error).await;
                }
                return Err(error);
            }
            WarmClaimOutcome::RetainedPublicationFailure(error) => {
                if task.is_cancelled() {
                    let lifecycle_id = *lifecycle_id.lock().map_err(|_| {
                        OrchError::Internal("warm lifecycle id lock poisoned".into())
                    })?;
                    return finish_cancelled_lifecycle(state, lifecycle_id, task, error).await;
                }
                return Err(error);
            }
        }
    }

    let mut initial_record = creating_record(
        state,
        &unverified_cfg,
        id,
        req.owner_key.clone(),
        req.api_key_id.clone(),
        now,
    );
    initial_record.startup_path = Some(VmStartupPath::Cold);
    let creating_state = state.clone();
    let registration_record = initial_record.clone();
    let ticket =
        state
            .supervisor
            .begin_boot_with_registration(
                id,
                crate::supervisor::SpawnPurpose::Live,
                unverified_cfg.resource_shape(),
                move || async move {
                    register_creating_record(&creating_state, registration_record).await
                },
            )
            .await;
    let ticket = match ticket {
        Ok(ticket) => ticket,
        Err(error) => {
            fail_create_or_restore(state, id, error).await?;
            unreachable!("failed lifecycle helper always returns an error")
        }
    };
    if task.is_cancelled() {
        return cancel_unstarted_lifecycle(state, id, &ticket, task, lifecycle_cancelled_error())
            .await;
    }
    let resolved_request = {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock".into()))?;
        image::resolve_request_image(&store, req)
    };
    let req = match resolved_request {
        Ok(req) => req,
        Err(error) => {
            state.supervisor.abort_unstarted_boot(&ticket).await;
            fail_create_or_restore(state, id, error).await?;
            unreachable!("failed lifecycle helper always returns an error")
        }
    };
    let spawn_cfg = VmSpawnConfig::from_defaults(&state.config, &req);
    let mut record = creating_record(
        state,
        &spawn_cfg,
        id,
        req.owner_key.clone(),
        req.api_key_id.clone(),
        now,
    );
    record.startup_path = Some(VmStartupPath::Cold);
    if let Err(error) = update_creating_record(state, record.clone()).await {
        state.supervisor.abort_unstarted_boot(&ticket).await;
        fail_create_or_restore(state, id, error).await?;
        unreachable!("failed lifecycle helper always returns an error")
    }
    if task.is_cancelled() {
        return cancel_unstarted_lifecycle(state, id, &ticket, task, lifecycle_cancelled_error())
            .await;
    }
    let sup = Arc::clone(&state.supervisor);
    let cfg = spawn_cfg.clone();
    let booted = tokio::task::spawn_blocking(move || sup.spawn_vm(ticket, cfg)).await;
    let booted = match booted {
        Err(error) => {
            let error = state
                .supervisor
                .cleanup_boot_join_failure(id, "create boot task", error);
            if task.is_cancelled() {
                return finish_cancelled_lifecycle(state, id, task, error).await;
            }
            if state.supervisor.has_retained_boot(id) {
                return Err(error);
            }
            fail_create_or_restore(state, id, error).await?;
            unreachable!("failed lifecycle helper always returns an error")
        }
        Ok(Ok(booted)) => booted,
        Ok(Err(error)) => {
            if task.is_cancelled() {
                return finish_cancelled_lifecycle(state, id, task, error).await;
            }
            if state.supervisor.has_retained_boot(id) {
                return Err(error);
            }
            fail_create_or_restore(state, id, error).await?;
            unreachable!("failed lifecycle helper always returns an error")
        }
    };
    if task.is_cancelled() {
        let cause = state.supervisor.discard_booted_vm(booted);
        return finish_cancelled_lifecycle(state, id, task, cause).await;
    }
    let publication_state = state.clone();
    let publication_record = record.clone();
    let record = match state
        .supervisor
        .publish_running_with(booted, move |pid, socket_path| {
            let mut record = publication_record;
            record.status = VmStatus::Running;
            record.startup_path = Some(VmStartupPath::Cold);
            record.pid = Some(pid);
            record.socket_path = Some(socket_path.display().to_string());
            record.updated_at = Utc::now();
            async move {
                publish_running_record(&publication_state, record.clone()).await?;
                Ok(record)
            }
        })
        .await
    {
        Ok(record) => record,
        Err(error) => {
            if is_shutdown_rejection(&error) {
                rollback_shutdown_rejected_lifecycle(state, id, Some(task), error).await?;
                unreachable!("shutdown lifecycle rollback always returns an error")
            }
            if task.is_cancelled() {
                return finish_cancelled_lifecycle(state, id, task, error).await;
            }
            return Err(error);
        }
    };
    if task.is_cancelled() {
        return finish_cancelled_lifecycle(state, id, task, lifecycle_cancelled_error()).await;
    }
    mark_running(state, record.clone())?;
    tracing::info!(id = %id, host = %state.config.host_id, "create: cold start");
    Ok(record)
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
    let id = id.unwrap_or_else(Uuid::new_v4);
    let state = state.clone();
    let snapshot_path = snapshot_path.to_string();
    let worker_state = state.clone();
    run_supervised_lifecycle(&state, id, move |task| async move {
        restore_local_owned(
            &worker_state,
            &snapshot_path,
            id,
            owner_key,
            api_key_id,
            caller_is_admin,
            &task,
        )
        .await
    })
    .await
}

async fn restore_local_owned(
    state: &AppState,
    snapshot_path: &str,
    id: Uuid,
    owner_key: Option<String>,
    api_key_id: Option<String>,
    caller_is_admin: bool,
    task: &OwnedTaskControl,
) -> Result<VmRecord, OrchError> {
    let snapshot =
        verify_snapshot_access(state, snapshot_path, owner_key.as_deref(), caller_is_admin)?;
    let memory_mib = snapshot.memory_mib.ok_or_else(|| {
        OrchError::BadRequest("snapshot is missing resource metadata; create a new snapshot".into())
    })?;
    let vcpus = snapshot.vcpus.ok_or_else(|| {
        OrchError::BadRequest("snapshot is missing resource metadata; create a new snapshot".into())
    })?;
    let kernel_path = snapshot.kernel_path.ok_or_else(|| {
        OrchError::BadRequest("snapshot is missing boot metadata; create a new snapshot".into())
    })?;
    let cmdline = snapshot.cmdline.ok_or_else(|| {
        OrchError::BadRequest("snapshot is missing boot metadata; create a new snapshot".into())
    })?;
    let snapshot_overlay_path = match (
        snapshot.rootfs_path.as_ref(),
        snapshot.overlay_path.as_ref(),
    ) {
        (Some(_), Some(path)) => Some(path.clone()),
        (Some(_), None) => {
            return Err(OrchError::BadRequest(
                "snapshot is missing its disk artifact; create a new snapshot".into(),
            ));
        }
        (None, None) => None,
        (None, Some(_)) => {
            return Err(OrchError::BadRequest(
                "rootfs-less snapshot has unexpected disk metadata".into(),
            ));
        }
    };
    let restore_config = VmSpawnConfig {
        memory_mib,
        vcpus,
        kernel_path: kernel_path.clone().into(),
        rootfs_path: snapshot.rootfs_path.clone().map(Into::into),
        cmdline: cmdline.clone(),
        read_only: true,
    };
    let now = Utc::now();
    let record = VmRecord {
        id,
        host_id: state.config.host_id.clone(),
        owner_key: owner_key.clone(),
        api_key_id: api_key_id.clone(),
        status: VmStatus::Creating,
        revision: 1,
        startup_path: Some(VmStartupPath::SnapshotRestore),
        memory_mib,
        vcpus,
        kernel_path,
        rootfs_path: snapshot.rootfs_path,
        cmdline,
        socket_path: None,
        pid: None,
        created_at: now,
        updated_at: now,
    };
    let creating_state = state.clone();
    let creating_record = record.clone();
    let ticket = state
        .supervisor
        .begin_boot_with_registration(
            id,
            crate::supervisor::SpawnPurpose::Live,
            restore_config.resource_shape(),
            move || async move { register_creating_record(&creating_state, creating_record).await },
        )
        .await;
    let ticket = match ticket {
        Ok(ticket) => ticket,
        Err(error) => {
            fail_create_or_restore(state, id, error).await?;
            unreachable!("failed lifecycle helper always returns an error")
        }
    };
    if task.is_cancelled() {
        return cancel_unstarted_lifecycle(state, id, &ticket, task, lifecycle_cancelled_error())
            .await;
    }
    if task.is_cancelled() {
        return cancel_unstarted_lifecycle(state, id, &ticket, task, lifecycle_cancelled_error())
            .await;
    }
    let path = snapshot_path.to_string();
    let sup = Arc::clone(&state.supervisor);
    let restore_shape = restore_config.resource_shape();
    let publication_state = state.clone();
    let publication_record = record.clone();
    let booted = tokio::task::spawn_blocking(move || {
        sup.restore_vm(ticket, path, snapshot_overlay_path, restore_shape)
    })
    .await;
    let booted = match booted {
        Err(error) => {
            let error = state
                .supervisor
                .cleanup_boot_join_failure(id, "restore boot task", error);
            if task.is_cancelled() {
                return finish_cancelled_lifecycle(state, id, task, error).await;
            }
            if state.supervisor.has_retained_boot(id) {
                return Err(error);
            }
            fail_create_or_restore(state, id, error).await?;
            unreachable!("failed lifecycle helper always returns an error")
        }
        Ok(Ok(booted)) => booted,
        Ok(Err(error)) => {
            if task.is_cancelled() {
                return finish_cancelled_lifecycle(state, id, task, error).await;
            }
            if state.supervisor.has_retained_boot(id) {
                return Err(error);
            }
            fail_create_or_restore(state, id, error).await?;
            unreachable!("failed lifecycle helper always returns an error")
        }
    };
    if task.is_cancelled() {
        let cause = state.supervisor.discard_booted_vm(booted);
        return finish_cancelled_lifecycle(state, id, task, cause).await;
    }
    let record = match state
        .supervisor
        .publish_running_with(booted, move |pid, socket_path| {
            let mut record = publication_record;
            record.status = VmStatus::Running;
            record.pid = Some(pid);
            record.socket_path = Some(socket_path.display().to_string());
            record.updated_at = Utc::now();
            async move {
                publish_running_record(&publication_state, record.clone()).await?;
                Ok(record)
            }
        })
        .await
    {
        Ok(record) => record,
        Err(error) => {
            if is_shutdown_rejection(&error) {
                rollback_shutdown_rejected_lifecycle(state, id, Some(task), error).await?;
                unreachable!("shutdown lifecycle rollback always returns an error")
            }
            if task.is_cancelled() {
                return finish_cancelled_lifecycle(state, id, task, error).await;
            }
            return Err(error);
        }
    };
    if task.is_cancelled() {
        return finish_cancelled_lifecycle(state, id, task, lifecycle_cancelled_error()).await;
    }
    mark_running(state, record.clone())?;
    tracing::info!(id = %id, host = %state.config.host_id, "restore: from snapshot");
    Ok(record)
}

pub async fn exec_local(
    state: &AppState,
    vm_id: Uuid,
    command: String,
    timeout_ms: u64,
) -> Result<(i32, String, String, u64), OrchError> {
    let gate = state.supervisor.operation_gate(vm_id)?;
    let _operation = gate.lock_owned().await;
    ensure_vm_status(state, vm_id, "exec", &[VmStatus::Running])?;
    let sup = Arc::clone(&state.supervisor);
    tokio::task::spawn_blocking(move || sup.exec_vm(vm_id, &command, timeout_ms))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))?
}

pub async fn stop_local(state: &AppState, id: Uuid) -> Result<(), OrchError> {
    // Mark and await the supervisor-owned worker before taking the terminal
    // gate. That worker finishes its current publication operation and either
    // converges terminal state itself or hands a fully published VM to the
    // ordinary delete path below.
    let sup = Arc::clone(&state.supervisor);
    let worker_converged = tokio::task::spawn_blocking(move || sup.cancel_and_wait_owned_task(id))
        .await
        .map_err(|error| {
            OrchError::Internal(format!("cancelled lifecycle wait join: {error}"))
        })??;
    if worker_converged {
        return Ok(());
    }
    let operation_gate = match state.supervisor.operation_gate(id) {
        Ok(gate) => Some(gate),
        Err(OrchError::NotFound(_)) => None,
        Err(error) => return Err(error),
    };
    let _operation = match operation_gate {
        Some(gate) => Some(gate.lock_owned().await),
        None => None,
    };
    let _terminal_gate = state.terminal_transition_gate.lock().await;
    match lifecycle_state(state, id)? {
        Some(LifecycleState::Terminal { .. }) => {
            return finish_terminal_transition(state, id).await
        }
        Some(LifecycleState::Publishing { .. }) => finish_publication(state, id).await?,
        Some(
            LifecycleState::Creating { .. }
            | LifecycleState::Running { .. }
            | LifecycleState::Reconciling { .. }
            | LifecycleState::Abandoned { .. },
        )
        | None => {}
    }
    if vm_get(state, id).is_ok_and(|record| record.status == VmStatus::Stopped) {
        return Ok(());
    }
    // Bill the final runtime interval before teardown, while the VM record (and
    // its owning key) is still in the cache, then drop its watermark.
    crate::usage::meter_vm_final(state, id);
    let sup = Arc::clone(&state.supervisor);
    tokio::task::spawn_blocking(move || sup.stop_vm(id))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
    start_terminal_transition(state, id, VmStatus::Stopped, true)?;
    finish_terminal_transition(state, id).await
}

/// Detect and persist unexpected VMM exits using one shared bounded scan.
/// Runtime cleanup and scheduler release have already happened synchronously;
/// this converges durable/cache/fleet state without touching the dead process.
pub(crate) async fn reconcile_unexpected_vmm_exits(state: &AppState) -> Vec<String> {
    let mut failures = retry_pending_lifecycle(state).await;
    let supervisor = Arc::clone(&state.supervisor);
    if let Err(error) = tokio::task::spawn_blocking(move || {
        supervisor.scan_for_exited_processes();
    })
    .await
    {
        failures.push(format!("VMM exit scan task failed: {error}"));
        return failures;
    }
    let exits = state.supervisor.take_unexpected_exits();
    if exits.is_empty() {
        return failures;
    }
    let _terminal_gate = state.terminal_transition_gate.lock().await;
    for exit in exits {
        if let Some(cleanup_error) = &exit.cleanup_error {
            tracing::error!(vm = %exit.id, pid = exit.pid, %cleanup_error, "dead VMM left resources requiring operator reconciliation");
        }
        let result = async {
            if matches!(
                lifecycle_state(state, exit.id)?,
                Some(LifecycleState::Publishing { .. })
            ) {
                finish_publication(state, exit.id).await?;
            }
            crate::usage::meter_vm_final(state, exit.id);
            start_terminal_transition(state, exit.id, VmStatus::Error, false)?;
            finish_terminal_transition(state, exit.id).await
        }
        .await;
        if let Err(error) = result {
            failures.push(format!(
                "VM {} exited ({}) but durable reconciliation failed: {error}",
                exit.id, exit.status
            ));
        }
    }
    failures
}

pub async fn stop_all_local(state: &AppState) -> Result<ShutdownSummary, OrchError> {
    let sup = Arc::clone(&state.supervisor);
    let owned_task_failure =
        match tokio::task::spawn_blocking(move || sup.cancel_and_wait_all_owned_tasks()).await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(error) => Some(format!("cancelled lifecycle wait join: {error}")),
        };
    let _terminal_gate = state.terminal_transition_gate.lock().await;
    let mut failures = retry_pending_lifecycle(state).await;
    failures.extend(owned_task_failure);
    let sup = Arc::clone(&state.supervisor);
    let outcome = tokio::task::spawn_blocking(move || sup.stop_all())
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))?;
    let (summary, failure) = match outcome {
        Ok(summary) => (summary, None),
        Err(failure) => {
            let failure = *failure;
            (failure.summary, Some(failure.error))
        }
    };

    failures.extend(failure.into_iter().map(|error| error.to_string()));
    let stopped_ids = summary
        .running_ids
        .iter()
        .chain(summary.booting_ids.iter())
        .copied()
        .collect::<Vec<_>>();

    for id in stopped_ids {
        if let Err(error) = start_terminal_transition(state, id, VmStatus::Stopped, true) {
            failures.push(format!(
                "VM {id} shutdown transition retained scheduler and ownership reservation: {error}"
            ));
            continue;
        }
        if let Err(error) = finish_terminal_transition(state, id).await {
            failures.push(format!(
                "VM {id} shutdown transition retained scheduler and ownership reservation: {error}"
            ));
        }
    }
    for id in summary
        .warm_ids
        .iter()
        .chain(summary.internal_booting_ids.iter())
    {
        if let Err(error) = state.supervisor.release_reservation_after_terminal(*id) {
            failures.push(format!(
                "VM {id} shutdown cleanup retained scheduler reservation: {error}"
            ));
        }
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

pub async fn suspend_local(state: &AppState, id: Uuid) -> Result<VmRecord, OrchError> {
    vm_op(
        state,
        id,
        |supervisor, id| supervisor.suspend_vm(id),
        VmStatus::Suspended,
    )
    .await
}

pub async fn resume_local(state: &AppState, id: Uuid) -> Result<VmRecord, OrchError> {
    vm_op(state, id, |sup, id| sup.resume_vm(id), VmStatus::Running).await
}

pub async fn snapshot_local(state: &AppState, id: Uuid, diff: bool) -> Result<String, OrchError> {
    if diff {
        return Err(OrchError::Unprocessable(
            "incremental orchestrator snapshots are disabled until durable parent-chain relocation is available; request a full snapshot with diff=false"
                .into(),
        ));
    }
    let gate = state.supervisor.operation_gate(id)?;
    let _operation = gate.lock_owned().await;
    let vm = ensure_vm_status(
        state,
        id,
        "snapshot",
        &[VmStatus::Running, VmStatus::Paused],
    )?;
    let resume_after = vm.status == VmStatus::Running;
    let has_overlay = vm.rootfs_path.is_some();
    let sup = Arc::clone(&state.supervisor);
    let bundle = tokio::task::spawn_blocking(move || {
        sup.snapshot_bundle_vm(id, diff, resume_after, has_overlay)
    })
    .await;
    let bundle = match bundle {
        Ok(Ok(bundle)) => bundle,
        Ok(Err(error)) => {
            return Err(reconcile_snapshot_pause_failure(state, &vm, error).await);
        }
        Err(error) => {
            return Err(reconcile_snapshot_pause_failure(
                state,
                &vm,
                OrchError::Internal(format!("snapshot task failed: {error}")),
            )
            .await);
        }
    };
    // R-006: record who owns this snapshot file so a later restore can verify
    // tenant access before the path is handed to the VMM. Fail closed if the
    // record cannot be written, so we never create a snapshot that only an
    // admin could restore.
    let record = tarit_store::SnapshotRecord {
        path: bundle.snapshot_path().to_string(),
        overlay_path: bundle.overlay_path().map(str::to_string),
        host_id: state.config.host_id.clone(),
        owner_key: vm.owner_key.clone(),
        api_key_id: vm.api_key_id.clone(),
        vm_id: id,
        memory_mib: Some(vm.memory_mib),
        vcpus: Some(vm.vcpus),
        kernel_path: Some(vm.kernel_path.clone()),
        rootfs_path: vm.rootfs_path.clone(),
        cmdline: Some(vm.cmdline.clone()),
        created_at: Utc::now(),
    };
    let insert = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .insert_snapshot(&record);
    if let Err(error) = insert {
        // SnapshotBundle's exact-inode cleanup runs before the operation gate
        // is released, so a failed ownership write cannot leave a restorable
        // but unowned RAM/disk pair.
        drop(bundle);
        return Err(crate::api::store_err(error));
    }
    let path = record.path;
    bundle.persist();
    Ok(path)
}

/// R-006: confirm the caller may restore the snapshot at `snapshot_path`.
///
/// A snapshot is a first-class owned record. A non-admin caller may only
/// restore a snapshot their own tenant created; an unknown path (no ownership
/// record) is refused so a tenant cannot point restore at an arbitrary host
/// file or another tenant's snapshot. Admins bypass tenant ownership but still
/// require a registered manifest: resource admission and cgroups must be sized
/// before any untrusted snapshot state is restored.
fn verify_snapshot_access(
    state: &AppState,
    snapshot_path: &str,
    caller_owner: Option<&str>,
    caller_is_admin: bool,
) -> Result<tarit_store::SnapshotRecord, OrchError> {
    let snapshot = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .get_snapshot(snapshot_path)
        .map_err(crate::api::store_err)?;
    match snapshot {
        Some(rec) if caller_is_admin => Ok(rec),
        Some(rec) if caller_owner.is_some() && rec.owner_key.as_deref() == caller_owner => Ok(rec),
        Some(_) => Err(OrchError::Forbidden(
            "snapshot belongs to another tenant".into(),
        )),
        None => Err(OrchError::BadRequest(
            "unknown snapshot or missing manifest; restore requires a registered snapshot".into(),
        )),
    }
}

pub async fn egress_local(
    state: &AppState,
    id: Uuid,
    allowlist: Vec<String>,
    allow_existing: bool,
) -> Result<usize, OrchError> {
    let gate = state.supervisor.operation_gate(id)?;
    let _operation = gate.lock_owned().await;
    ensure_vm_status(state, id, "update egress for", LIVE_CONTROL_STATUSES)?;
    let sup = Arc::clone(&state.supervisor);
    tokio::task::spawn_blocking(move || sup.update_egress(id, allowlist, allow_existing))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))?
}

pub fn get_local(state: &AppState, id: Uuid) -> Result<VmRecord, OrchError> {
    if matches!(
        lifecycle_state(state, id)?,
        Some(LifecycleState::Reconciling { .. })
    ) {
        return Err(OrchError::Unavailable(format!(
            "vm {id} lifecycle reconciliation is pending"
        )));
    }
    vm_get(state, id)
}

/// Live VMM status for a locally-owned VM (state/uptime/vcpus/mem/vcpu_alive),
/// queried from the `vmm serve` process over its UDS.
pub async fn status_local(state: &AppState, id: Uuid) -> Result<serde_json::Value, OrchError> {
    let gate = state.supervisor.operation_gate(id)?;
    let _operation = gate.lock_owned().await;
    ensure_vm_status(state, id, "query live status for", LIVE_CONTROL_STATUSES)?;
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
    let gate = state.supervisor.operation_gate(id)?;
    let _operation = gate.lock_owned().await;
    let current = get_local(state, id)?;
    match validate_live_transition(id, current.status, new_status)? {
        TransitionDecision::Noop => return Ok(current),
        TransitionDecision::Apply => {}
    }
    let operation_supervisor = Arc::clone(&state.supervisor);
    tokio::task::spawn_blocking(move || op(&operation_supervisor, id))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
    match vm_set_status(state, id, new_status).await {
        Ok(record) => Ok(record),
        Err(persist_error) => {
            let rollback_supervisor = Arc::clone(&state.supervisor);
            let prior_status = current.status;
            let rollback = tokio::task::spawn_blocking(move || {
                rollback_vmm_transition(&rollback_supervisor, id, prior_status, new_status)
            })
            .await
            .map_err(|error| OrchError::Internal(format!("rollback join: {error}")))?;
            match rollback {
                Ok(()) => {
                    match compensate_vm_status(state, &current, prior_status).await {
                        Ok(compensation) => {
                            tracing::warn!(
                                vm = %id,
                                from = prior_status.as_str(),
                                to = new_status.as_str(),
                                revision = compensation.revision,
                                %persist_error,
                                "rolled back VMM and fenced the failed lifecycle transition"
                            );
                            Err(persist_error)
                        }
                        Err(compensation_error) => Err(OrchError::Internal(format!(
                            "persist VM {id} transition {} -> {} failed: {persist_error}; VMM rollback to {} succeeded but revision-N+2 control-plane compensation failed: {compensation_error}",
                            prior_status.as_str(),
                            new_status.as_str(),
                            prior_status.as_str()
                        ))),
                    }
                }
                Err(rollback_error) => {
                    match observe_and_compensate_vm_status(state, &current).await {
                        Ok(observed) => {
                            let _ = set_lifecycle_state(
                                state,
                                id,
                                LifecycleState::Running {
                                    record: observed.clone(),
                                },
                            );
                            Err(OrchError::Internal(format!(
                                "persist VM {id} transition {} -> {} failed: {persist_error}; rollback to {} failed: {rollback_error}; fenced observed VMM state {} at revision {}",
                                prior_status.as_str(),
                                new_status.as_str(),
                                prior_status.as_str(),
                                observed.status.as_str(),
                                observed.revision
                            )))
                        }
                        Err(reconcile_error) => {
                            let retain_error = set_lifecycle_state(
                                state,
                                id,
                                LifecycleState::Reconciling {
                                    record: current.clone(),
                                },
                            )
                            .err()
                            .map(|error| format!("; retaining reconciliation failed: {error}"))
                            .unwrap_or_default();
                            Err(OrchError::Internal(format!(
                                "persist VM {id} transition {} -> {} failed: {persist_error}; rollback to {} failed: {rollback_error}; observing/fencing the VMM failed and remains retryable: {reconcile_error}{retain_error}",
                                prior_status.as_str(),
                                new_status.as_str(),
                                prior_status.as_str()
                            )))
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionDecision {
    Noop,
    Apply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RollbackPlan {
    Resume,
    Pause,
    Suspend,
    ResumeThenPause,
}

fn rollback_plan(prior: VmStatus, target: VmStatus) -> Option<RollbackPlan> {
    match (prior, target) {
        (VmStatus::Running, VmStatus::Paused | VmStatus::Suspended) => Some(RollbackPlan::Resume),
        (VmStatus::Paused, VmStatus::Running) => Some(RollbackPlan::Pause),
        (VmStatus::Paused, VmStatus::Suspended) => Some(RollbackPlan::ResumeThenPause),
        (VmStatus::Suspended, VmStatus::Running) => Some(RollbackPlan::Suspend),
        _ => None,
    }
}

fn rollback_vmm_transition(
    supervisor: &VmmSupervisor,
    id: Uuid,
    prior: VmStatus,
    target: VmStatus,
) -> Result<(), OrchError> {
    match rollback_plan(prior, target).ok_or_else(|| {
        OrchError::Internal(format!(
            "no rollback plan for VM {id} transition {} -> {}",
            prior.as_str(),
            target.as_str()
        ))
    })? {
        RollbackPlan::Resume => supervisor.resume_vm(id),
        RollbackPlan::Pause => supervisor.pause_vm(id),
        RollbackPlan::Suspend => supervisor.suspend_vm(id),
        RollbackPlan::ResumeThenPause => {
            supervisor.resume_vm(id)?;
            supervisor.pause_vm(id)
        }
    }
}

fn validate_live_transition(
    id: Uuid,
    current: VmStatus,
    target: VmStatus,
) -> Result<TransitionDecision, OrchError> {
    if current == target {
        return Ok(TransitionDecision::Noop);
    }
    let allowed = match target {
        VmStatus::Paused => matches!(current, VmStatus::Running),
        VmStatus::Suspended => matches!(current, VmStatus::Running | VmStatus::Paused),
        VmStatus::Running => matches!(current, VmStatus::Paused | VmStatus::Suspended),
        _ => false,
    };
    if allowed {
        Ok(TransitionDecision::Apply)
    } else {
        Err(OrchError::Conflict(format!(
            "cannot transition vm {id} from {} to {}",
            current.as_str(),
            target.as_str()
        )))
    }
}

fn ensure_vm_status(
    state: &AppState,
    id: Uuid,
    operation: &str,
    allowed: &[VmStatus],
) -> Result<VmRecord, OrchError> {
    let record = get_local(state, id)?;
    if allowed.contains(&record.status) {
        Ok(record)
    } else {
        Err(OrchError::Conflict(format!(
            "cannot {operation} vm {id} while it is {}",
            record.status.as_str()
        )))
    }
}

#[cfg(test)]
fn take_lifecycle_fault(state: &AppState, fault: LifecycleFault) -> bool {
    let Ok(mut faults) = state.lifecycle_faults.lock() else {
        return false;
    };
    let Some(index) = faults.iter().position(|candidate| *candidate == fault) else {
        return false;
    };
    faults.remove(index);
    true
}

#[cfg(test)]
fn inject_lifecycle_fault(state: &AppState, fault: LifecycleFault) {
    state.lifecycle_faults.lock().unwrap().push(fault);
}

#[cfg(test)]
async fn wait_lifecycle_pause(state: &AppState, pause: LifecyclePause) {
    let control = state
        .lifecycle_pauses
        .lock()
        .ok()
        .and_then(|pauses| pauses.get(&pause).cloned());
    if let Some(control) = control {
        control.entered.notify_one();
        control.release.notified().await;
    }
}

#[cfg(test)]
fn pause_lifecycle(state: &AppState, pause: LifecyclePause) -> LifecyclePauseControl {
    let control = LifecyclePauseControl::default();
    state
        .lifecycle_pauses
        .lock()
        .unwrap()
        .insert(pause, control.clone());
    control
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
    #[cfg(target_os = "linux")]
    use std::io::{Read, Write};
    #[cfg(target_os = "linux")]
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::{Mutex, RwLock};
    #[cfg(target_os = "linux")]
    use std::time::Duration;
    use tarit_store::Store;

    #[test]
    fn live_transition_validation_is_idempotent_and_rejects_wrong_states() {
        let id = Uuid::new_v4();
        for status in [VmStatus::Running, VmStatus::Paused, VmStatus::Suspended] {
            assert_eq!(
                validate_live_transition(id, status, status).unwrap(),
                TransitionDecision::Noop
            );
        }
        for (from, to) in [
            (VmStatus::Running, VmStatus::Paused),
            (VmStatus::Running, VmStatus::Suspended),
            (VmStatus::Paused, VmStatus::Suspended),
            (VmStatus::Paused, VmStatus::Running),
            (VmStatus::Suspended, VmStatus::Running),
        ] {
            assert_eq!(
                validate_live_transition(id, from, to).unwrap(),
                TransitionDecision::Apply
            );
        }
        for invalid in [VmStatus::Creating, VmStatus::Stopped, VmStatus::Error] {
            assert!(validate_live_transition(id, invalid, VmStatus::Paused).is_err());
            assert!(validate_live_transition(id, invalid, VmStatus::Suspended).is_err());
            assert!(validate_live_transition(id, invalid, VmStatus::Running).is_err());
        }
        assert!(
            validate_live_transition(id, VmStatus::Suspended, VmStatus::Paused).is_err(),
            "suspended RAM must be rehydrated via resume before pause"
        );
    }

    #[test]
    fn incremental_orchestrator_snapshot_is_rejected_before_vmm_lookup() {
        let (state, _) = test_state_with_durable_writer();
        let error = test_runtime()
            .block_on(snapshot_local(&state, Uuid::new_v4(), true))
            .expect_err("diff snapshots must fail before runtime lookup");
        assert!(matches!(error, OrchError::Unprocessable(_)));
        assert_eq!(error.http_status(), 422);
        assert!(error.to_string().contains("diff=false"));
    }

    #[test]
    fn live_status_and_egress_state_gate_excludes_terminal_or_unpublished_vms() {
        assert_eq!(
            LIVE_CONTROL_STATUSES,
            &[VmStatus::Running, VmStatus::Paused, VmStatus::Suspended]
        );
        for status in [VmStatus::Creating, VmStatus::Stopped, VmStatus::Error] {
            assert!(!LIVE_CONTROL_STATUSES.contains(&status));
        }
    }

    #[test]
    fn vmm_observation_maps_only_controllable_live_states() {
        assert_eq!(
            control_status(tarit_vmm_client::VmState::Running).unwrap(),
            VmStatus::Running
        );
        assert_eq!(
            control_status(tarit_vmm_client::VmState::Paused).unwrap(),
            VmStatus::Paused
        );
        assert_eq!(
            control_status(tarit_vmm_client::VmState::Suspended).unwrap(),
            VmStatus::Suspended
        );
        assert!(control_status(tarit_vmm_client::VmState::Created).is_err());
        assert!(control_status(tarit_vmm_client::VmState::Stopped).is_err());
    }

    #[test]
    fn every_live_transition_has_an_exact_vmm_rollback_plan() {
        assert_eq!(
            rollback_plan(VmStatus::Running, VmStatus::Paused),
            Some(RollbackPlan::Resume)
        );
        assert_eq!(
            rollback_plan(VmStatus::Running, VmStatus::Suspended),
            Some(RollbackPlan::Resume)
        );
        assert_eq!(
            rollback_plan(VmStatus::Paused, VmStatus::Running),
            Some(RollbackPlan::Pause)
        );
        assert_eq!(
            rollback_plan(VmStatus::Paused, VmStatus::Suspended),
            Some(RollbackPlan::ResumeThenPause)
        );
        assert_eq!(
            rollback_plan(VmStatus::Suspended, VmStatus::Running),
            Some(RollbackPlan::Suspend)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_live_transition_is_rolled_back_and_fenced_at_revision_n_plus_two() {
        let (state, _) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        let initial = vm_get(&state, id).unwrap();
        state.store.lock().unwrap().insert_vm(&initial).unwrap();

        let socket = PathBuf::from(format!("/tmp/taritd-{}-{id}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let (requests_tx, requests_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || loop {
            let (mut stream, _) = listener.accept().unwrap();
            let mut length = [0_u8; 4];
            stream.read_exact(&mut length).unwrap();
            let mut body = vec![0; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut body).unwrap();
            let request: tarit_vmm_client::ApiRequest = serde_json::from_slice(&body).unwrap();
            let response = match &request {
                tarit_vmm_client::ApiRequest::Status => {
                    tarit_vmm_client::ApiResponse::Status(tarit_vmm_client::VmStatus {
                        state: tarit_vmm_client::VmState::Paused,
                        uptime_ms: 1,
                        vcpus: 1,
                        mem_mib: 256,
                        volumes: 0,
                        nets: 0,
                        kernel: "kernel".into(),
                        vcpu_alive: true,
                    })
                }
                tarit_vmm_client::ApiRequest::Exec { .. } => tarit_vmm_client::ApiResponse::Exec {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms: 1,
                },
                _ => tarit_vmm_client::ApiResponse::Ok,
            };
            let encoded = serde_json::to_vec(&response).unwrap();
            stream
                .write_all(&(encoded.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&encoded).unwrap();
            stream.flush().unwrap();
            let stopped = matches!(request, tarit_vmm_client::ApiRequest::Stop);
            requests_tx.send(request).unwrap();
            if stopped {
                break;
            }
        });
        state
            .supervisor
            .install_test_control_runtime(id, socket.clone());
        inject_lifecycle_fault(&state, LifecycleFault::SQLite);

        let error = test_runtime()
            .block_on(pause_local(&state, id))
            .expect_err("injected local persistence failure must fail the request");
        assert!(error.to_string().contains("injected SQLite failure"));

        let cached = vm_get(&state, id).unwrap();
        let durable = state.store.lock().unwrap().get_vm(id).unwrap();
        assert_eq!(cached.status, VmStatus::Running);
        assert_eq!(durable.status, VmStatus::Running);
        assert_eq!(cached.revision, initial.revision + 2);
        assert_eq!(durable.revision, initial.revision + 2);

        state.supervisor.stop_vm(id).unwrap();
        server.join().unwrap();
        let requests = requests_rx.into_iter().collect::<Vec<_>>();
        assert!(matches!(
            requests.as_slice(),
            [
                tarit_vmm_client::ApiRequest::Pause,
                tarit_vmm_client::ApiRequest::Status,
                tarit_vmm_client::ApiRequest::Resume,
                tarit_vmm_client::ApiRequest::Exec { .. },
                tarit_vmm_client::ApiRequest::Stop
            ]
        ));
        assert!(!socket.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_snapshot_resume_is_observed_and_fenced_paused() {
        let (state, _) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        let initial = vm_get(&state, id).unwrap();
        state.store.lock().unwrap().insert_vm(&initial).unwrap();

        let socket = PathBuf::from(format!(
            "/tmp/taritd-snapshot-{}-{id}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let (requests_tx, requests_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || loop {
            let (mut stream, _) = listener.accept().unwrap();
            let mut length = [0_u8; 4];
            stream.read_exact(&mut length).unwrap();
            let mut body = vec![0; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut body).unwrap();
            let request: tarit_vmm_client::ApiRequest = serde_json::from_slice(&body).unwrap();
            let response = match &request {
                tarit_vmm_client::ApiRequest::Snapshot { .. } => {
                    tarit_vmm_client::ApiResponse::Err {
                        msg: "injected snapshot failure".into(),
                    }
                }
                tarit_vmm_client::ApiRequest::Resume => tarit_vmm_client::ApiResponse::Err {
                    msg: "injected resume failure".into(),
                },
                tarit_vmm_client::ApiRequest::Status => {
                    tarit_vmm_client::ApiResponse::Status(tarit_vmm_client::VmStatus {
                        state: tarit_vmm_client::VmState::Paused,
                        uptime_ms: 1,
                        vcpus: 1,
                        mem_mib: 256,
                        volumes: 0,
                        nets: 0,
                        kernel: "kernel".into(),
                        vcpu_alive: true,
                    })
                }
                _ => tarit_vmm_client::ApiResponse::Ok,
            };
            let encoded = serde_json::to_vec(&response).unwrap();
            stream
                .write_all(&(encoded.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&encoded).unwrap();
            stream.flush().unwrap();
            let stopped = matches!(request, tarit_vmm_client::ApiRequest::Stop);
            requests_tx.send(request).unwrap();
            if stopped {
                break;
            }
        });
        state
            .supervisor
            .install_test_control_runtime(id, socket.clone());

        let error = test_runtime()
            .block_on(snapshot_local(&state, id, false))
            .expect_err("snapshot and resume failures must not leave a running record");
        assert!(error.to_string().contains("fenced paused"));

        let cached = vm_get(&state, id).unwrap();
        let durable = state.store.lock().unwrap().get_vm(id).unwrap();
        assert_eq!(cached.status, VmStatus::Paused);
        assert_eq!(durable.status, VmStatus::Paused);
        assert_eq!(cached.revision, initial.revision + 2);
        assert_eq!(durable.revision, initial.revision + 2);

        state.supervisor.stop_vm(id).unwrap();
        server.join().unwrap();
        let requests = requests_rx.into_iter().collect::<Vec<_>>();
        assert!(matches!(
            requests.as_slice(),
            [
                tarit_vmm_client::ApiRequest::Pause,
                tarit_vmm_client::ApiRequest::Snapshot { .. },
                tarit_vmm_client::ApiRequest::Resume,
                tarit_vmm_client::ApiRequest::Status,
                tarit_vmm_client::ApiRequest::Stop
            ]
        ));
        assert!(!socket.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unobservable_snapshot_state_stays_reconciling_until_retry_converges() {
        let (state, _) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        let initial = vm_get(&state, id).unwrap();
        state.store.lock().unwrap().insert_vm(&initial).unwrap();

        let socket = PathBuf::from(format!(
            "/tmp/taritd-snapshot-reconcile-{}-{id}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let (requests_tx, requests_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut status_requests = 0;
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let mut length = [0_u8; 4];
                stream.read_exact(&mut length).unwrap();
                let mut body = vec![0; u32::from_be_bytes(length) as usize];
                stream.read_exact(&mut body).unwrap();
                let request: tarit_vmm_client::ApiRequest = serde_json::from_slice(&body).unwrap();
                let response = match &request {
                    tarit_vmm_client::ApiRequest::Snapshot { .. } => {
                        tarit_vmm_client::ApiResponse::Err {
                            msg: "injected snapshot failure".into(),
                        }
                    }
                    tarit_vmm_client::ApiRequest::Resume => tarit_vmm_client::ApiResponse::Err {
                        msg: "injected resume failure".into(),
                    },
                    tarit_vmm_client::ApiRequest::Status => {
                        status_requests += 1;
                        if status_requests == 1 {
                            tarit_vmm_client::ApiResponse::Err {
                                msg: "injected status outage".into(),
                            }
                        } else {
                            tarit_vmm_client::ApiResponse::Status(tarit_vmm_client::VmStatus {
                                state: tarit_vmm_client::VmState::Paused,
                                uptime_ms: 1,
                                vcpus: 1,
                                mem_mib: 256,
                                volumes: 0,
                                nets: 0,
                                kernel: "kernel".into(),
                                vcpu_alive: true,
                            })
                        }
                    }
                    _ => tarit_vmm_client::ApiResponse::Ok,
                };
                let encoded = serde_json::to_vec(&response).unwrap();
                stream
                    .write_all(&(encoded.len() as u32).to_be_bytes())
                    .unwrap();
                stream.write_all(&encoded).unwrap();
                stream.flush().unwrap();
                let stopped = matches!(request, tarit_vmm_client::ApiRequest::Stop);
                requests_tx.send(request).unwrap();
                if stopped {
                    break;
                }
            }
        });
        state
            .supervisor
            .install_test_control_runtime(id, socket.clone());

        let runtime = test_runtime();
        let error = runtime
            .block_on(snapshot_local(&state, id, false))
            .expect_err("unobservable compensation must retain reconciliation");
        assert!(error.to_string().contains("remains unknown and retryable"));
        assert!(matches!(
            lifecycle_state(&state, id).unwrap(),
            Some(LifecycleState::Reconciling { .. })
        ));
        assert!(matches!(
            get_local(&state, id),
            Err(OrchError::Unavailable(_))
        ));
        assert!(matches!(
            runtime.block_on(exec_local(&state, id, "true".into(), 100)),
            Err(OrchError::Unavailable(_))
        ));
        assert_eq!(vm_get(&state, id).unwrap().status, VmStatus::Running);
        assert_eq!(
            state.store.lock().unwrap().get_vm(id).unwrap().status,
            VmStatus::Running
        );

        let initial_requests = (0..4)
            .map(|_| requests_rx.recv_timeout(Duration::from_secs(1)).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            initial_requests.as_slice(),
            [
                tarit_vmm_client::ApiRequest::Pause,
                tarit_vmm_client::ApiRequest::Snapshot { .. },
                tarit_vmm_client::ApiRequest::Resume,
                tarit_vmm_client::ApiRequest::Status
            ]
        ));
        let gate = state.supervisor.operation_gate(id).unwrap();
        runtime.block_on(async {
            let held_operation = gate.lock_owned().await;
            let retry_state = state.clone();
            let retry =
                tokio::spawn(async move { reconcile_unexpected_vmm_exits(&retry_state).await });
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(
                requests_rx.try_recv().is_err(),
                "periodic status/fencing must wait for the runtime operation gate"
            );
            drop(held_operation);
            assert!(
                retry.await.unwrap().is_empty(),
                "periodic reconciliation must observe and durably fence the VMM"
            );
        });
        let cached = vm_get(&state, id).unwrap();
        let durable = state.store.lock().unwrap().get_vm(id).unwrap();
        assert_eq!(cached.status, VmStatus::Paused);
        assert_eq!(durable.status, VmStatus::Paused);
        assert_eq!(cached.revision, initial.revision + 2);
        assert_eq!(durable.revision, initial.revision + 2);
        assert!(matches!(
            lifecycle_state(&state, id).unwrap(),
            Some(LifecycleState::Running { record }) if record.status == VmStatus::Paused
        ));

        state.supervisor.stop_vm(id).unwrap();
        server.join().unwrap();
        let requests = requests_rx.into_iter().collect::<Vec<_>>();
        assert!(matches!(
            requests.as_slice(),
            [
                tarit_vmm_client::ApiRequest::Status,
                tarit_vmm_client::ApiRequest::Stop
            ]
        ));
        assert!(!socket.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn running_snapshot_resumes_before_ram_scratch_handoff() {
        use std::os::unix::fs::OpenOptionsExt;

        let (state, _) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        let initial = vm_get(&state, id).unwrap();
        state.store.lock().unwrap().insert_vm(&initial).unwrap();
        let scratch = PathBuf::from(format!(
            "/tmp/vmm-snap-{}-{}.snap",
            std::process::id(),
            Uuid::new_v4()
        ));
        let mut options = std::fs::OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let mut scratch_file = options.open(&scratch).unwrap();
        scratch_file.write_all(b"immutable RAM image").unwrap();
        scratch_file.sync_all().unwrap();
        drop(scratch_file);

        let socket = PathBuf::from(format!(
            "/tmp/taritd-snapshot-order-{}-{id}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let (requests_tx, requests_rx) = std::sync::mpsc::channel();
        let server_scratch = scratch.display().to_string();
        let server = std::thread::spawn(move || loop {
            let (mut stream, _) = listener.accept().unwrap();
            let mut length = [0_u8; 4];
            stream.read_exact(&mut length).unwrap();
            let mut body = vec![0; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut body).unwrap();
            let request: tarit_vmm_client::ApiRequest = serde_json::from_slice(&body).unwrap();
            let response = match &request {
                tarit_vmm_client::ApiRequest::Snapshot { .. } => {
                    tarit_vmm_client::ApiResponse::Snapshot {
                        path: server_scratch.clone(),
                    }
                }
                _ => tarit_vmm_client::ApiResponse::Ok,
            };
            let encoded = serde_json::to_vec(&response).unwrap();
            stream
                .write_all(&(encoded.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&encoded).unwrap();
            stream.flush().unwrap();
            let stopped = matches!(request, tarit_vmm_client::ApiRequest::Stop);
            requests_tx.send(request).unwrap();
            if stopped {
                break;
            }
        });
        state
            .supervisor
            .install_test_control_runtime(id, socket.clone());

        let durable = test_runtime()
            .block_on(snapshot_local(&state, id, false))
            .expect("full snapshot succeeds");
        assert_ne!(durable, scratch.display().to_string());
        assert!(durable.contains("/snapshots/bundle-"));
        assert_eq!(std::fs::read(&durable).unwrap(), b"immutable RAM image");
        assert!(!scratch.exists(), "released VMM scratch must be removed");

        state.supervisor.stop_vm(id).unwrap();
        server.join().unwrap();
        let requests = requests_rx.into_iter().collect::<Vec<_>>();
        assert!(matches!(
            requests.as_slice(),
            [
                tarit_vmm_client::ApiRequest::Pause,
                tarit_vmm_client::ApiRequest::Snapshot { .. },
                tarit_vmm_client::ApiRequest::Resume,
                tarit_vmm_client::ApiRequest::ReleaseScratch { .. },
                tarit_vmm_client::ApiRequest::Stop
            ]
        ));
        std::fs::remove_file(durable).unwrap();
        assert!(!socket.exists());
    }

    #[test]
    fn shutdown_rejection_is_identified_precisely() {
        assert!(is_shutdown_rejection(&OrchError::Overloaded {
            message: "taritd is shutting down".into(),
            retry_after_secs: 1,
        }));
        assert!(!is_shutdown_rejection(&OrchError::Overloaded {
            message: "cluster at capacity".into(),
            retry_after_secs: 1,
        }));
        assert!(!is_shutdown_rejection(&OrchError::Internal(
            "store unavailable".into()
        )));
    }

    #[test]
    fn stopped_record_persists_directly_after_store_writer_stops() {
        let (state, writes) = test_state_with_durable_writer();
        drop(writes);
        let id = insert_running_vm(&state);
        let mut record = vm_get(&state, id).unwrap();
        record.status = VmStatus::Stopped;

        test_runtime()
            .block_on(persist_stopped_record(&state, record))
            .expect("a stopped record must persist after the store writer stops");

        let persisted = state.store.lock().unwrap().get_vm(id).unwrap();
        assert_eq!(persisted.status, VmStatus::Stopped);
    }

    #[test]
    fn shutdown_rejection_releases_its_boot_reservation() {
        let (state, writes) = test_state_with_durable_writer();
        drop(writes);
        let id = insert_running_vm(&state);
        let mut record = vm_get(&state, id).unwrap();
        record.status = VmStatus::Creating;
        state.store.lock().unwrap().insert_vm(&record).unwrap();
        commit_vm_record(&state, record.clone()).unwrap();
        set_lifecycle_state(
            &state,
            id,
            LifecycleState::Creating {
                record,
                phase: CreatingPhase::FleetClaimed,
            },
        )
        .unwrap();
        state.supervisor.reserve_existing_for_test(id);

        let error = test_runtime()
            .block_on(fail_create_or_restore(
                &state,
                id,
                OrchError::Overloaded {
                    message: "taritd is shutting down".into(),
                    retry_after_secs: 1,
                },
            ))
            .expect_err("shutdown rejection must be returned to the unacknowledged request");

        assert!(matches!(
            error,
            OrchError::Overloaded { message, .. } if message == "taritd is shutting down"
        ));
        assert!(state.store.lock().unwrap().get_vm(id).is_err());
        assert!(state.vm_cache.read().unwrap().get(&id).is_none());
        assert!(lifecycle_state(&state, id).unwrap().is_none());
        assert_eq!(state.scheduler.local_capacity(1, 1).sandbox_count, 0);
    }

    #[test]
    fn ordinary_delete_writer_failure_keeps_a_retryable_transition_and_reservation() {
        let (state, mut writes) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        state.supervisor.reserve_existing_for_test(id);
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
            assert!(matches!(
                lifecycle_state(&state, id).unwrap(),
                Some(LifecycleState::Terminal { .. })
            ));
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
        state.supervisor.reserve_existing_for_test(id);
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
            assert!(matches!(
                lifecycle_state(&state, id).unwrap(),
                Some(LifecycleState::Terminal { .. })
            ));
            assert_eq!(state.scheduler.local_capacity(1, 1).sandbox_count, 1);

            stop_local(&state, id)
                .await
                .expect("a later stop must retry only the retained stopped transition");
            writer.await.unwrap();

            assert_eq!(durable_attempts.load(Ordering::SeqCst), 2);
            assert!(lifecycle_state(&state, id).unwrap().is_none());
            assert_eq!(vm_get(&state, id).unwrap().status, VmStatus::Stopped);
            assert_eq!(state.scheduler.local_capacity(1, 1).sandbox_count, 0);
        });
        drop(runtime);
        drop(state);
    }

    #[test]
    fn publication_boundary_failures_retain_running_ownership_and_reservation() {
        let (state, mut writes) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        state.supervisor.reserve_existing_for_test(id);
        let running = vm_get(&state, id).unwrap();
        set_lifecycle_state(
            &state,
            id,
            LifecycleState::Creating {
                record: running.clone(),
                phase: CreatingPhase::FleetClaimed,
            },
        )
        .unwrap();
        let runtime = test_runtime();
        runtime.block_on(async {
            inject_lifecycle_fault(&state, LifecycleFault::SQLite);
            assert!(publish_running_record(&state, running.clone())
                .await
                .is_err());
            assert!(matches!(
                lifecycle_state(&state, id).unwrap(),
                Some(LifecycleState::Publishing {
                    phase: PublicationPhase::FleetUpdated,
                    ..
                })
            ));
            assert_eq!(state.scheduler.local_capacity(1, 1).sandbox_count, 1);

            inject_lifecycle_fault(&state, LifecycleFault::FleetClaim);
            assert!(publish_running_record(&state, running.clone())
                .await
                .is_err());
            assert!(matches!(
                lifecycle_state(&state, id).unwrap(),
                Some(LifecycleState::Publishing {
                    phase: PublicationPhase::NeedFleetUpdate,
                    ..
                })
            ));

            let writer = tokio::spawn(async move {
                let StoreWrite::VmDurable(_, completion) = writes.recv().await.unwrap() else {
                    panic!("publication must use the durable SQLite writer");
                };
                completion.send(Ok(())).unwrap();
            });
            inject_lifecycle_fault(&state, LifecycleFault::CacheCommit);
            assert!(publish_running_record(&state, running).await.is_err());
            writer.await.unwrap();
            assert!(matches!(
                lifecycle_state(&state, id).unwrap(),
                Some(LifecycleState::Publishing {
                    phase: PublicationPhase::SQLitePersisted,
                    ..
                })
            ));
            assert_eq!(state.scheduler.local_capacity(1, 1).sandbox_count, 1);
        });
        drop(runtime);
        drop(state);
    }

    #[test]
    fn warm_publication_failures_retain_the_live_vm_and_retry_ownership() {
        let (state, writes) = test_state_with_durable_writer();
        let writes = Arc::new(tokio::sync::Mutex::new(writes));
        let warm_cfg = VmSpawnConfig {
            memory_mib: 256,
            vcpus: 1,
            kernel_path: PathBuf::from("kernel"),
            rootfs_path: Some(PathBuf::from("rootfs")),
            cmdline: "console=ttyS0".into(),
            read_only: false,
        };
        let runtime = test_runtime();

        runtime.block_on(async {
            for (index, (fault, expected_phase)) in [
                (
                    LifecycleFault::FleetClaim,
                    PublicationPhase::NeedFleetUpdate,
                ),
                (LifecycleFault::SQLite, PublicationPhase::FleetUpdated),
                (
                    LifecycleFault::CacheCommit,
                    PublicationPhase::SQLitePersisted,
                ),
            ]
            .into_iter()
            .enumerate()
            {
                let id = Uuid::new_v4();
                state
                    .supervisor
                    .seed_warm_for_test(id, warm_cfg.clone())
                    .unwrap();
                let record = running_record(
                    &state,
                    &warm_cfg,
                    id,
                    1,
                    &PathBuf::from(format!("warm-publication-{id}.sock")),
                    None,
                    None,
                    Utc::now(),
                );
                set_lifecycle_state(
                    &state,
                    id,
                    LifecycleState::Creating {
                        record: record.clone(),
                        phase: CreatingPhase::FleetClaimed,
                    },
                )
                .unwrap();
                inject_lifecycle_fault(&state, fault);
                let writer = if fault == LifecycleFault::CacheCommit {
                    let writes = Arc::clone(&writes);
                    Some(tokio::spawn(async move {
                        let StoreWrite::VmDurable(_, completion) =
                            writes.lock().await.recv().await.unwrap()
                        else {
                            panic!("warm publication must use durable SQLite");
                        };
                        completion.send(Ok(())).unwrap();
                    }))
                } else {
                    None
                };
                let publication_state = state.clone();
                let publication_record = record.clone();
                let task = Arc::new(OwnedTaskControl::new());
                let outcome = state
                    .supervisor
                    .take_warm_with_publication(
                        &warm_cfg,
                        &task,
                        |_| async { Ok(()) },
                        move |_, _, _| async move {
                            publish_running_record(&publication_state, publication_record).await?;
                            Ok(())
                        },
                    )
                    .await
                    .unwrap();
                if let Some(writer) = writer {
                    writer.await.unwrap();
                }

                assert!(matches!(
                    outcome,
                    WarmClaimOutcome::RetainedPublicationFailure(_)
                ));
                assert!(state.supervisor.is_running(id));
                assert_eq!(state.supervisor.warm_count(1, 256), 0);
                assert_eq!(
                    state.scheduler.local_capacity(1, 1).sandbox_count,
                    index + 1
                );
                assert!(matches!(
                    lifecycle_state(&state, id).unwrap(),
                    Some(LifecycleState::Publishing { phase, .. }) if phase == expected_phase
                ));
            }
        });
        drop(runtime);
        drop(state);
    }

    #[test]
    fn aborted_request_with_delayed_fleet_publication_stays_owned_until_delete_converges() {
        let (mut state, mut writes) = test_state_with_durable_writer();
        state.config.warm_pool.enabled = true;
        let warm_cfg = VmSpawnConfig::from_defaults(
            &state.config,
            &CreateVmRequest {
                id: None,
                owner_key: Some("test".into()),
                api_key_id: None,
                memory_mib: 256,
                vcpus: 1,
                kernel_path: None,
                image: None,
                rootfs_path: None,
                cmdline: None,
            },
        );
        let id = Uuid::new_v4();
        state
            .supervisor
            .seed_warm_for_test(id, warm_cfg)
            .expect("a warm VM must be available for the lifecycle request");
        let fleet_pause = pause_lifecycle(&state, LifecyclePause::Fleet);
        let runtime = test_runtime();

        runtime.block_on(async {
            let writer = tokio::spawn(async move {
                while let Some(write) = writes.recv().await {
                    if let StoreWrite::VmDurable(_, completion) = write {
                        let _ = completion.send(Ok(()));
                    }
                }
            });
            let request_state = state.clone();
            let request = tokio::spawn(async move {
                create_local(
                    &request_state,
                    &CreateVmRequest {
                        id: None,
                        owner_key: Some("test".into()),
                        api_key_id: None,
                        memory_mib: 256,
                        vcpus: 1,
                        kernel_path: None,
                        image: None,
                        rootfs_path: None,
                        cmdline: None,
                    },
                )
                .await
            });

            fleet_pause.entered.notified().await;
            request.abort();
            assert!(matches!(request.await, Err(error) if error.is_cancelled()));
            assert!(
                state.supervisor.has_owned_task(id),
                "dropping the API future must detach from the supervisor-owned publication"
            );

            let delete_state = state.clone();
            let delete = tokio::spawn(async move { stop_local(&delete_state, id).await });
            tokio::task::yield_now().await;
            assert!(
                !delete.is_finished(),
                "DELETE must wait for the delayed fleet operation before terminal clear"
            );
            fleet_pause.release.notify_one();
            delete
                .await
                .expect("DELETE task must finish")
                .expect("DELETE must converge the owned lifecycle");

            assert!(!state.supervisor.has_owned_task(id));
            assert!(lifecycle_state(&state, id).unwrap().is_none());
            assert_eq!(vm_get(&state, id).unwrap().status, VmStatus::Stopped);
            assert!(!state.supervisor.is_running(id));
            assert_eq!(state.scheduler.local_capacity(1, 1).sandbox_count, 0);
            writer.abort();
        });
        drop(runtime);
        drop(state);
    }

    #[test]
    fn stop_all_converges_an_abandoned_warm_publication_without_releasing_early() {
        let (state, mut writes) = test_state_with_durable_writer();
        let warm_cfg = VmSpawnConfig {
            memory_mib: 256,
            vcpus: 1,
            kernel_path: PathBuf::from("kernel"),
            rootfs_path: Some(PathBuf::from("rootfs")),
            cmdline: "console=ttyS0".into(),
            read_only: false,
        };
        let id = Uuid::new_v4();
        state
            .supervisor
            .seed_warm_for_test(id, warm_cfg.clone())
            .unwrap();
        let record = running_record(
            &state,
            &warm_cfg,
            id,
            1,
            &PathBuf::from(format!("warm-stop-all-{id}.sock")),
            None,
            None,
            Utc::now(),
        );
        set_lifecycle_state(&state, id, LifecycleState::Abandoned { record }).unwrap();
        state.supervisor.abandon_lifecycle(id);
        assert!(state.supervisor.is_running(id));
        assert_eq!(state.scheduler.local_capacity(1, 1).sandbox_count, 1);

        let runtime = test_runtime();
        runtime.block_on(async {
            let writer = tokio::spawn(async move {
                let StoreWrite::VmDurable(_, completion) = writes.recv().await.unwrap() else {
                    panic!("stop-all must durably persist the abandoned VM terminal record");
                };
                completion.send(Ok(())).unwrap();
            });
            stop_all_local(&state)
                .await
                .expect("stop-all must converge an abandoned warm VM");
            writer.await.unwrap();
        });

        assert!(lifecycle_state(&state, id).unwrap().is_none());
        assert_eq!(vm_get(&state, id).unwrap().status, VmStatus::Stopped);
        assert!(!state.supervisor.is_running(id));
        assert_eq!(state.scheduler.local_capacity(1, 1).sandbox_count, 0);
        drop(runtime);
        drop(state);
    }

    #[test]
    fn terminal_fleet_clear_failure_retains_the_creating_reservation_for_retry() {
        let (state, _) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        state.supervisor.reserve_existing_for_test(id);
        let record = terminal_record(&state, id, VmStatus::Error).unwrap();
        set_lifecycle_state(
            &state,
            id,
            LifecycleState::Terminal {
                record,
                phase: TerminalPhase::ClearFleetOwnershipAndRelease,
            },
        )
        .unwrap();
        inject_lifecycle_fault(&state, LifecycleFault::FleetClear);

        let error = test_runtime()
            .block_on(finish_terminal_transition(&state, id))
            .expect_err("a failed fleet clear must retain the terminal lifecycle");

        assert!(error.to_string().contains("injected fleet clear failure"));
        assert!(matches!(
            lifecycle_state(&state, id).unwrap(),
            Some(LifecycleState::Terminal {
                phase: TerminalPhase::ClearFleetOwnershipAndRelease,
                ..
            })
        ));
        assert_eq!(state.scheduler.local_capacity(1, 1).sandbox_count, 1);
    }

    #[test]
    fn registered_creating_record_routes_delete_to_the_local_cluster_owner() {
        let (state, _) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        let mut creating = vm_get(&state, id).unwrap();
        creating.status = VmStatus::Creating;
        commit_vm_record(&state, creating.clone()).unwrap();
        set_lifecycle_state(
            &state,
            id,
            LifecycleState::Creating {
                record: creating,
                phase: CreatingPhase::FleetClaimed,
            },
        )
        .unwrap();

        let owner = test_runtime()
            .block_on(cluster::resolve_owner(&state, id))
            .expect("a registered Creating record must be routable for DELETE");
        assert!(matches!(owner, cluster::Owner::Local));
    }

    fn test_state_with_durable_writer() -> (AppState, tokio::sync::mpsc::Receiver<StoreWrite>) {
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
            allow_insecure_peer_http: true,
            enable_net: false,
            rootfs_read_only: false,
            metrics_expose_tenant_labels: false,
            api_max_in_flight: 128,
            api_requests_per_second: 10_000,
            api_request_timeout_ms: 5_000,
            api_max_body_bytes: 1024 * 1024,
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
            share_listen: None,
            share_domain: None,
            share_token_key: None,
            share_token_ttl_secs: 300,
            share_connect_timeout_ms: 1_000,
            share_idle_timeout_secs: 1,
        };
        let (store_tx, store_rx) = tokio::sync::mpsc::channel(128);
        let scheduler = Arc::new(Scheduler::new(config.clone()));
        let store = Arc::new(Mutex::new(Store::open(":memory:").unwrap()));
        let shares = crate::shares::ShareRepository::new(Arc::clone(&store), None);
        let supervisor = Arc::new(
            VmmSupervisor::new_with_live_vms(
                config.clone(),
                std::iter::empty(),
                &[],
                Arc::clone(&scheduler),
            )
            .unwrap(),
        );
        (
            AppState {
                config: config.clone(),
                audit_outbox: Arc::new(crate::audit::LocalAuditOutbox::new(Arc::clone(&store))),
                store,
                exec_cache: Arc::new(RwLock::new(HashMap::new())),
                vm_cache: Arc::new(RwLock::new(HashMap::new())),
                store_tx,
                lifecycle: Arc::new(Mutex::new(HashMap::new())),
                lifecycle_faults: Arc::new(Mutex::new(Vec::new())),
                lifecycle_pauses: Arc::new(Mutex::new(HashMap::new())),
                terminal_transition_gate: Arc::new(tokio::sync::Mutex::new(())),
                pty_registry: Arc::new(PtyRegistry::default()),
                supervisor,
                scheduler,
                peer: Arc::new(PeerClient::new("peer-secret".into())),
                shares,
                fleet: None,
                metrics: Arc::new(Metrics::default()),
                share_runtime: Arc::new(crate::share_gateway::ShareRuntime::default()),
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
                revision: 1,
                startup_path: None,
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
