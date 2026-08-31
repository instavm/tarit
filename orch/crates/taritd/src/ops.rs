//! Node-local VM operations shared by the public API (this node owns/places the
//! VM) and the internal peer API (a peer forwarded a request to this owner).
//!
//! Everything that actually touches the local supervisor + store lives here, so
//! the public router and the internal router never duplicate "do the work"
//! logic (DRY). Placement/routing decisions live in `cluster`; the public
//! handlers combine the two.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use tarit_types::{
    ArtifactBootMetadata, ArtifactKind, ArtifactRecord, ArtifactReplicationState, ArtifactStatus,
    CreateVmRequest, EgressPolicyRecord, OrchError, VmRecord, VmStartupPath, VmStatus,
    VmVolumeAttachmentRecord, VolumeAttachmentMode,
};
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
    OwnedTaskControl, PublicationFailure, ShutdownSummary, SpawnPurpose, VmDataVolumeConfig,
    VmSpawnConfig, VmmSupervisor, WarmClaimOutcome,
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
    // Every caller reaches the terminal transition only after the supervisor
    // has contained the runtime. Do not durably advertise ownership of a dead
    // PID or paths that have already been removed; restart reconciliation must
    // never try to re-adopt terminal process metadata.
    record.runtime_layout = None;
    record.socket_path = None;
    record.pid = None;
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
                if let Some(owner_key) = record.owner_key.as_deref() {
                    let store = state
                        .store
                        .lock()
                        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?;
                    // A scale-to-zero VM has no supervisor runtime to clean,
                    // but its durable hibernation relation still owns the
                    // snapshot binding. Remove it as part of the retryable
                    // terminal phase so DELETE cannot leave a stale wake path.
                    match store.delete_hibernation(owner_key, id) {
                        Ok(_) | Err(tarit_store::StoreError::NotFound) => {}
                        Err(error) => return Err(crate::api::store_err(error)),
                    }
                    store
                        .unbind_vm_volumes(owner_key, id)
                        .map_err(crate::api::store_err)?;
                }
                cleanup_ephemeral_snapshots_for_vm(state, id).await?;
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

/// Retire only private lifecycle snapshots explicitly leased to this VM. Fleet metadata
/// is withdrawn before local bytes, while local metadata remains until all
/// exact private files are gone so an interrupted deletion is safely retryable.
pub async fn cleanup_ephemeral_snapshots_for_vm(
    state: &AppState,
    vm_id: Uuid,
) -> Result<u64, OrchError> {
    let snapshots = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .list_ephemeral_snapshots_for_vm(vm_id)
        .map_err(crate::api::store_err)?;
    cleanup_ephemeral_snapshots(state, snapshots).await
}

/// Retry cleanup left after a successful hibernation activation. A
/// hibernation lease names the same VM as both snapshot source and owner;
/// live-fork leases name distinct source and child VMs and are retained until
/// child deletion.
async fn cleanup_hibernation_snapshots_for_vm(
    state: &AppState,
    vm_id: Uuid,
) -> Result<u64, OrchError> {
    let snapshots = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .list_ephemeral_snapshots_for_vm(vm_id)
        .map_err(crate::api::store_err)?
        .into_iter()
        .filter(|snapshot| snapshot.vm_id == vm_id)
        .collect();
    cleanup_ephemeral_snapshots(state, snapshots).await
}

async fn cleanup_ephemeral_snapshots(
    state: &AppState,
    snapshots: Vec<tarit_store::SnapshotRecord>,
) -> Result<u64, OrchError> {
    let mut removed_files = 0u64;
    for snapshot in snapshots {
        let artifact = if let Some(owner_key) = snapshot.owner_key.as_deref() {
            let store = state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock poisoned".into()))?;
            match store.get_artifact(owner_key, snapshot.snapshot_id) {
                Ok(artifact) => Some(artifact),
                Err(tarit_store::StoreError::NotFound) => None,
                Err(error) => return Err(crate::api::store_err(error)),
            }
        } else {
            None
        };
        if let (Some(fleet), Some(owner_key)) =
            (state.fleet.as_ref(), snapshot.owner_key.as_deref())
        {
            if artifact.is_some() {
                match fleet
                    .delete_artifact_if_unreferenced(owner_key, snapshot.snapshot_id)
                    .await
                {
                    Ok(_) | Err(tarit_fleet::FleetError::NotFound) => {}
                    Err(error) => {
                        return Err(OrchError::Internal(format!(
                            "retire fork artifact from fleet: {error}"
                        )))
                    }
                }
            }
            fleet
                .delete_snapshot_by_id(owner_key, snapshot.snapshot_id)
                .await
                .map_err(|error| {
                    OrchError::Internal(format!("retire fork snapshot from fleet: {error}"))
                })?;
        }
        removed_files = removed_files.saturating_add(
            crate::disk::delete_owned_snapshot_components(&state.config.socket_dir, &snapshot)?,
        );
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?;
        if let (Some(artifact), Some(owner_key)) =
            (artifact.as_ref(), snapshot.owner_key.as_deref())
        {
            store
                .delete_local_replica_metadata_if_unreferenced(
                    owner_key,
                    artifact.artifact_id,
                    &snapshot.path,
                )
                .map_err(crate::api::store_err)?;
        } else {
            match store.delete_snapshot(&snapshot.path) {
                Ok(()) | Err(tarit_store::StoreError::NotFound) => {}
                Err(error) => return Err(crate::api::store_err(error)),
            }
        }
    }
    Ok(removed_files)
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
        rootfs_read_only: spawn_cfg.read_only,
        cmdline: spawn_cfg.cmdline.clone(),
        runtime_layout: Some(state.supervisor.runtime_layout_for_config(id, spawn_cfg)),
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
    let warm_enabled = state.config.warm_pool.enabled
        && req.id.is_none()
        && req.image.is_none()
        && req.volumes.is_empty();

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
        image::resolve_request_image(&store, req, &state.config.image_admission_policy)
    };
    let req = match resolved_request {
        Ok(req) => req,
        Err(error) => {
            state.supervisor.abort_unstarted_boot(&ticket).await;
            fail_create_or_restore(state, id, error).await?;
            unreachable!("failed lifecycle helper always returns an error")
        }
    };
    let mut spawn_cfg = VmSpawnConfig::from_defaults(&state.config, &req);
    spawn_cfg.data_volumes = match bind_requested_vm_volumes(state, &req, id).await {
        Ok(volumes) => volumes,
        Err(error) => {
            state.supervisor.abort_unstarted_boot(&ticket).await;
            fail_create_or_restore(state, id, error).await?;
            unreachable!("failed lifecycle helper always returns an error")
        }
    };
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

async fn bind_requested_vm_volumes(
    state: &AppState,
    request: &CreateVmRequest,
    vm_id: Uuid,
) -> Result<Vec<VmDataVolumeConfig>, OrchError> {
    if request.volumes.len() > 15 {
        return Err(OrchError::BadRequest(
            "a VM supports at most 15 persistent data volumes".into(),
        ));
    }
    let owner_key = request.owner_key.as_deref().ok_or_else(|| {
        OrchError::Internal(format!("VM {vm_id} volume request has no tenant owner"))
    })?;
    let mut seen = std::collections::HashSet::new();
    let mut bindings = Vec::with_capacity(request.volumes.len());
    let mut spawn = Vec::with_capacity(request.volumes.len());
    for (index, requested) in request.volumes.iter().enumerate() {
        if !seen.insert(requested.volume_id) {
            return Err(OrchError::BadRequest(format!(
                "volume {} is attached more than once",
                requested.volume_id
            )));
        }
        let volume = if let Some(fleet) = &state.fleet {
            fleet
                .get_volume(owner_key, requested.volume_id)
                .await
                .map_err(crate::api::volume_fleet_err)?
        } else {
            state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
                .get_volume(owner_key, requested.volume_id)
                .map_err(crate::api::store_err)?
        };
        let target = crate::volume_provider::placement_target(&state.config, &volume)?;
        if target
            .as_deref()
            .is_some_and(|host| host != state.config.host_id)
        {
            return Err(OrchError::Conflict(format!(
                "volume {} is pinned to another host",
                requested.volume_id
            )));
        }
        if state.fleet.is_some() {
            state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
                .insert_volume(&volume)
                .map_err(crate::api::store_err)?;
        }
        let read_only = requested.mode == VolumeAttachmentMode::ReadOnly;
        let record = VmVolumeAttachmentRecord {
            vm_id,
            volume_id: volume.id,
            device_index: u8::try_from(index).map_err(|_| {
                OrchError::BadRequest("persistent volume device index overflow".into())
            })?,
            owner_key: owner_key.to_string(),
            mode: requested.mode,
            volume_generation: volume.generation,
            created_at: Utc::now(),
        };
        bindings.push(record);
        spawn.push(VmDataVolumeConfig {
            id: volume.id,
            provider: volume.provider.clone(),
            size_bytes: volume.size_bytes,
            read_only,
            generation: volume.generation,
        });
    }
    if let Some(fleet) = &state.fleet {
        fleet
            .bind_vm_volumes(&bindings)
            .await
            .map_err(crate::api::volume_fleet_err)?;
    }
    let local_result = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .bind_vm_volumes(&bindings)
        .map_err(crate::api::store_err);
    if let Err(error) = local_result {
        if let Some(fleet) = &state.fleet {
            if let Err(rollback) = fleet.unbind_vm_volumes(owner_key, vm_id).await {
                return Err(OrchError::Internal(format!(
                    "{error}; failed to roll back fleet volume fence: {rollback}"
                )));
            }
        }
        return Err(error);
    }
    Ok(spawn)
}

async fn attached_volume_spawn_config(
    state: &AppState,
    owner_key: &str,
    vm_id: Uuid,
) -> Result<Vec<VmDataVolumeConfig>, OrchError> {
    let attachments = if let Some(fleet) = &state.fleet {
        fleet
            .list_vm_volume_attachments(owner_key, vm_id)
            .await
            .map_err(crate::api::volume_fleet_err)?
    } else {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .list_vm_volume_attachments(owner_key, vm_id)
            .map_err(crate::api::store_err)?
    };
    let mut spawn = Vec::with_capacity(attachments.len());
    for attachment in &attachments {
        let volume = if let Some(fleet) = &state.fleet {
            fleet
                .get_volume(owner_key, attachment.volume_id)
                .await
                .map_err(crate::api::volume_fleet_err)?
        } else {
            state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
                .get_volume(owner_key, attachment.volume_id)
                .map_err(crate::api::store_err)?
        };
        let target = crate::volume_provider::placement_target(&state.config, &volume)?;
        if volume.generation != attachment.volume_generation
            || target
                .as_deref()
                .is_some_and(|host| host != state.config.host_id)
        {
            return Err(OrchError::Conflict(format!(
                "volume {} cannot be reattached on this host",
                volume.id
            )));
        }
        if state.fleet.is_some() {
            state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
                .insert_volume(&volume)
                .map_err(crate::api::store_err)?;
        }
        spawn.push(VmDataVolumeConfig {
            id: volume.id,
            provider: volume.provider,
            size_bytes: volume.size_bytes,
            read_only: attachment.mode == VolumeAttachmentMode::ReadOnly,
            generation: volume.generation,
        });
    }
    if state.fleet.is_some() {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .bind_vm_volumes(&attachments)
            .map_err(crate::api::store_err)?;
    }
    Ok(spawn)
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
    restore_local_with_policy(
        state,
        snapshot_path,
        id,
        owner_key,
        api_key_id,
        caller_is_admin,
        false,
    )
    .await
}

pub async fn restore_local_from_surviving_artifact(
    state: &AppState,
    snapshot_path: &str,
    id: Option<Uuid>,
    owner_key: Option<String>,
    api_key_id: Option<String>,
    caller_is_admin: bool,
) -> Result<VmRecord, OrchError> {
    restore_local_with_policy(
        state,
        snapshot_path,
        id,
        owner_key,
        api_key_id,
        caller_is_admin,
        true,
    )
    .await
}

async fn restore_local_with_policy(
    state: &AppState,
    snapshot_path: &str,
    id: Option<Uuid>,
    owner_key: Option<String>,
    api_key_id: Option<String>,
    caller_is_admin: bool,
    allow_degraded_fleet: bool,
) -> Result<VmRecord, OrchError> {
    let id = id.unwrap_or_else(Uuid::new_v4);
    let state = state.clone();
    let snapshot_path = snapshot_path.to_string();
    let worker_state = state.clone();
    let access = RestoreAccess {
        owner_key,
        api_key_id,
        caller_is_admin,
        allow_degraded_fleet,
    };
    run_supervised_lifecycle(&state, id, move |task| async move {
        restore_local_owned(&worker_state, &snapshot_path, id, access, &task).await
    })
    .await
}

struct RestoreAccess {
    owner_key: Option<String>,
    api_key_id: Option<String>,
    caller_is_admin: bool,
    allow_degraded_fleet: bool,
}

async fn restore_local_owned(
    state: &AppState,
    snapshot_path: &str,
    id: Uuid,
    access: RestoreAccess,
    task: &OwnedTaskControl,
) -> Result<VmRecord, OrchError> {
    let snapshot = verify_snapshot_access(
        state,
        snapshot_path,
        access.owner_key.as_deref(),
        access.caller_is_admin,
    )?;
    let backing_artifact = verify_snapshot_artifact_ready(
        state,
        &snapshot,
        state.config.image_admission_policy.require_signature,
        access.allow_degraded_fleet,
    )
    .await?;
    let integrity = crate::supervisor::verify_snapshot_integrity(&snapshot)?;
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
        read_only: restored_rootfs_read_only(snapshot.rootfs_read_only),
        egress_allowlist: Vec::new(),
        egress_allow_existing: false,
        data_volumes: Vec::new(),
    };
    let now = Utc::now();
    let record = VmRecord {
        id,
        host_id: state.config.host_id.clone(),
        owner_key: access.owner_key.clone(),
        api_key_id: access.api_key_id.clone(),
        status: VmStatus::Creating,
        revision: 1,
        startup_path: Some(VmStartupPath::SnapshotRestore),
        memory_mib,
        vcpus,
        kernel_path,
        rootfs_path: snapshot.rootfs_path,
        rootfs_read_only: restore_config.read_only,
        cmdline,
        runtime_layout: Some(state.supervisor.runtime_layout_for_snapshot_restore(
            id,
            &restore_config,
            PathBuf::from(snapshot_path).as_path(),
        )),
        socket_path: None,
        pid: None,
        created_at: now,
        updated_at: now,
    };
    if let (Some(fleet), Some(artifact)) = (state.fleet.as_ref(), backing_artifact.as_ref()) {
        fleet
            .acquire_vm_artifact_ref(&record, artifact.artifact_id)
            .await
            .map_err(|error| {
                OrchError::Internal(format!("acquire VM artifact reference: {error}"))
            })?;
    }
    let binding_record = record.clone();
    let result = async {
        let creating_state = state.clone();
        let creating_record = record.clone();
        let ticket =
            state
                .supervisor
                .begin_boot_with_registration(
                    id,
                    crate::supervisor::SpawnPurpose::Live,
                    restore_config.resource_shape(),
                    move || async move {
                        register_creating_record(&creating_state, creating_record).await
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
            return cancel_unstarted_lifecycle(
                state,
                id,
                &ticket,
                task,
                lifecycle_cancelled_error(),
            )
            .await;
        }
        if task.is_cancelled() {
            return cancel_unstarted_lifecycle(
                state,
                id,
                &ticket,
                task,
                lifecycle_cancelled_error(),
            )
            .await;
        }
        let path = snapshot_path.to_string();
        let sup = Arc::clone(&state.supervisor);
        let restore_shape = restore_config.resource_shape();
        let publication_state = state.clone();
        let publication_record = record.clone();
        let booted = tokio::task::spawn_blocking(move || {
            sup.restore_vm(
                ticket,
                path,
                snapshot_overlay_path,
                restore_config,
                restore_shape,
                integrity,
            )
        })
        .await;
        let booted = match booted {
            Err(error) => {
                let error =
                    state
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
    .await;
    if result.is_err() && backing_artifact.is_some() {
        if let Some(fleet) = state.fleet.as_ref() {
            if let Err(cleanup) = fleet.release_vm_artifact_ref(&binding_record).await {
                return Err(OrchError::Internal(format!(
                    "restore failed and VM artifact reference cleanup failed: {cleanup}"
                )));
            }
        }
    }
    result
}

fn restored_rootfs_read_only(value: Option<bool>) -> bool {
    value.unwrap_or(true)
}

pub async fn exec_local(
    state: &AppState,
    vm_id: Uuid,
    command: String,
    timeout_ms: u64,
) -> Result<(i32, String, String, u64), OrchError> {
    ensure_active_local(state, vm_id).await?;
    let gate = state.supervisor.operation_gate(vm_id)?;
    let _operation = gate.lock_owned().await;
    ensure_vm_status(state, vm_id, "exec", &[VmStatus::Running])?;
    let sup = Arc::clone(&state.supervisor);
    tokio::task::spawn_blocking(move || sup.exec_vm(vm_id, &command, timeout_ms))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))?
}

pub async fn stop_local(state: &AppState, id: Uuid) -> Result<(), OrchError> {
    let policy_owner = vm_get(state, id).ok().and_then(|vm| vm.owner_key);
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
        if let Some(owner) = policy_owner.as_deref() {
            state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
                .delete_egress_policy(owner, id)
                .map_err(crate::api::store_err)?;
        }
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
            finish_terminal_transition(state, id).await?;
            if let Some(owner) = policy_owner.as_deref() {
                state
                    .store
                    .lock()
                    .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
                    .delete_egress_policy(owner, id)
                    .map_err(crate::api::store_err)?;
            }
            return Ok(());
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
        // A caller that arrives after another DELETE completed may already
        // have resolved the VM as local before waiting on its operation gate.
        // Revalidate after the gate and expose the same terminal 404 as a
        // request that arrived after completion; otherwise two racing deletes
        // both claim to have performed the transition.
        return Err(OrchError::NotFound(format!("vm {id} not found")));
    }
    // Bill the final runtime interval before teardown, while the VM record (and
    // its owning key) is still in the cache, then drop its watermark.
    crate::usage::meter_vm_final(state, id);
    let sup = Arc::clone(&state.supervisor);
    tokio::task::spawn_blocking(move || sup.stop_vm(id))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
    state.pty_registry.close_vm_sessions(id);
    start_terminal_transition(state, id, VmStatus::Stopped, true)?;
    finish_terminal_transition(state, id).await?;
    if let Some(owner) = policy_owner.as_deref() {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .delete_egress_policy(owner, id)
            .map_err(crate::api::store_err)?;
    }
    Ok(())
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
            state.pty_registry.close_vm_sessions(exit.id);
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

pub async fn hibernate_local(
    state: &AppState,
    id: Uuid,
    identity: &crate::config::ApiIdentity,
) -> Result<VmRecord, OrchError> {
    // A hibernated VM deliberately has no supervisor runtime/gate. Validate
    // the durable lifecycle record first so an invalid repeat is a state
    // conflict, not the misleading claim that the VM does not exist.
    ensure_vm_status(state, id, "hibernate", &[VmStatus::Running])?;
    let gate = state.supervisor.operation_gate(id)?;
    let _operation = gate.lock_owned().await;
    let current = ensure_vm_status(state, id, "hibernate", &[VmStatus::Running])?;
    let snapshot_path = snapshot_local_locked(state, id, false, Some(id), None)
        .await?
        .path;
    let snapshot =
        verify_snapshot_access(state, &snapshot_path, current.owner_key.as_deref(), false)?;
    let owner_key = current.owner_key.as_ref().ok_or_else(|| {
        OrchError::Internal(format!("vm {id} has no tenant owner for hibernation"))
    })?;
    let mut replication_identity = identity.clone();
    replication_identity.tenant = owner_key.clone();
    replication_identity.api_key_id = current
        .api_key_id
        .clone()
        .unwrap_or_else(|| identity.api_key_id.clone());
    crate::api::ensure_artifact_replication_ready(
        state,
        &replication_identity,
        snapshot.snapshot_id,
    )
    .await?;
    verify_snapshot_artifact_ready(
        state,
        &snapshot,
        state.config.image_admission_policy.require_signature,
        false,
    )
    .await?;
    let owner_key = owner_key.clone();
    let desired_egress = get_egress_policy_local(state, id, &owner_key)?;
    let now = Utc::now();
    let hibernation = tarit_store::HibernationRecord {
        vm_id: id,
        owner_key: owner_key.clone(),
        snapshot_path,
        created_at: now,
        updated_at: now,
    };
    state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .upsert_hibernation(&hibernation)
        .map_err(crate::api::store_err)?;
    if let Some(fleet) = state.fleet.as_ref() {
        let fleet_hibernation = tarit_fleet::FleetHibernationRecord {
            vm_id: id,
            owner_key: owner_key.clone(),
            artifact_id: snapshot.snapshot_id,
            egress_policy: desired_egress,
            created_at: now,
            updated_at: now,
        };
        if let Err(error) = fleet.upsert_hibernation(&fleet_hibernation).await {
            let _ = state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
                .delete_hibernation(&owner_key, id);
            return Err(OrchError::Internal(format!(
                "publish durable fleet hibernation: {error}"
            )));
        }
    }

    state.pty_registry.close_vm_sessions(id);
    let supervisor = Arc::clone(&state.supervisor);
    if let Err(error) = tokio::task::spawn_blocking(move || supervisor.stop_vm(id))
        .await
        .map_err(|join| OrchError::Internal(format!("hibernate teardown join: {join}")))?
    {
        let fleet_cleanup = if let Some(fleet) = state.fleet.as_ref() {
            fleet
                .delete_hibernation(&hibernation.owner_key, id)
                .await
                .err()
        } else {
            None
        };
        let cleanup = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .delete_hibernation(&hibernation.owner_key, id);
        return Err(match (cleanup, fleet_cleanup) {
            (Ok(_), None) => error,
            (local, fleet) => OrchError::Internal(format!(
                "{error}; failed to fully remove unusable hibernation record: local={local:?}, fleet={fleet:?}"
            )),
        });
    }

    let mut record = current;
    record.status = VmStatus::Hibernated;
    record.revision = record
        .revision
        .checked_add(1)
        .ok_or_else(|| OrchError::Internal(format!("VM {id} revision exhausted")))?;
    record.runtime_layout = None;
    record.socket_path = None;
    record.pid = None;
    record.updated_at = Utc::now();
    claim_lifecycle_ownership(state, &record).await?;
    state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .insert_vm(&record)
        .map_err(crate::api::store_err)?;
    commit_vm_record(state, record.clone())?;
    refresh_running_lifecycle(state, &record)?;
    state.supervisor.release_reservation_after_terminal(id)?;
    Ok(record)
}

pub async fn resume_local(state: &AppState, id: Uuid) -> Result<VmRecord, OrchError> {
    if vm_get(state, id)?.status == VmStatus::Hibernated {
        return resume_hibernated_local(state, id, false).await;
    }
    vm_op(state, id, |sup, id| sup.resume_vm(id), VmStatus::Running).await
}

/// Recover the same hibernated VM identity after its fleet owner becomes stale.
/// Artifact localization happens before the ownership CAS; only a verified
/// local survivor can become the new owner, and the CAS can never steal a live
/// or merely suspended runtime.
pub async fn recover_hibernated_on_stale_owner(
    state: &AppState,
    id: Uuid,
    identity: &crate::config::ApiIdentity,
) -> Result<VmRecord, OrchError> {
    let fleet = state
        .fleet
        .as_ref()
        .ok_or_else(|| OrchError::Unavailable("fleet recovery is not configured".into()))?;
    let fleet_vm = fleet.get_vm(id).await.map_err(|error| match error {
        tarit_fleet::FleetError::NotFound => OrchError::NotFound(format!("vm {id} not found")),
        other => OrchError::Internal(format!("read fleet VM for recovery: {other}")),
    })?;
    if fleet_vm.owner_key.as_deref() != Some(identity.tenant.as_str()) && !identity.is_admin() {
        return Err(OrchError::Forbidden("VM belongs to another tenant".into()));
    }
    if fleet_vm.status != VmStatus::Hibernated {
        return Err(OrchError::Unavailable(format!(
            "VM {id} cannot be recovered while {}",
            fleet_vm.status.as_str()
        )));
    }
    let owner_key = fleet_vm
        .owner_key
        .clone()
        .ok_or_else(|| OrchError::Internal(format!("hibernated VM {id} has no tenant owner")))?;
    let durable = fleet
        .get_hibernation(&owner_key, id)
        .await
        .map_err(|error| match error {
            tarit_fleet::FleetError::NotFound => {
                OrchError::Unavailable("durable hibernation binding is missing".into())
            }
            other => OrchError::Internal(format!("read durable hibernation: {other}")),
        })?;
    let artifact = fleet
        .get_artifact(&owner_key, durable.artifact_id)
        .await
        .map_err(|error| match error {
            tarit_fleet::FleetError::NotFound => {
                OrchError::Unavailable("hibernation artifact is missing".into())
            }
            other => OrchError::Internal(format!("read hibernation artifact: {other}")),
        })?;
    if artifact.source_vm_id != Some(id)
        || artifact.status != ArtifactStatus::Available
        || !matches!(
            artifact.replication_state,
            ArtifactReplicationState::Ready | ArtifactReplicationState::Degraded
        )
    {
        return Err(OrchError::Unavailable(
            "hibernation artifact lineage or availability is invalid".into(),
        ));
    }

    let mut localization_identity = identity.clone();
    localization_identity.tenant = owner_key.clone();
    localization_identity.api_key_id = fleet_vm
        .api_key_id
        .clone()
        .unwrap_or_else(|| identity.api_key_id.clone());
    localize_branch_artifact(state, &artifact, &localization_identity, false).await?;
    let snapshot = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .get_snapshot_by_id(artifact.artifact_id)
        .map_err(crate::api::store_err)?
        .ok_or_else(|| {
            OrchError::Unavailable("localized hibernation snapshot is missing".into())
        })?;
    if snapshot.owner_key.as_deref() != Some(owner_key.as_str()) || snapshot.vm_id != id {
        return Err(OrchError::Unavailable(
            "localized hibernation snapshot identity is invalid".into(),
        ));
    }
    verify_artifact_boot_metadata(state, &snapshot, &artifact)?;

    let mut claimed = fleet
        .claim_hibernated_vm(
            &owner_key,
            id,
            &state.config.host_id,
            state.config.host_session_id,
            Utc::now() - chrono::Duration::seconds(15),
        )
        .await
        .map_err(|error| match error {
            tarit_fleet::FleetError::NotFound => OrchError::NotFound(format!("vm {id} not found")),
            tarit_fleet::FleetError::Conflict(message) => OrchError::Conflict(message),
            other => OrchError::Internal(format!("claim hibernated VM: {other}")),
        })?;
    claimed.memory_mib = snapshot.memory_mib.ok_or_else(|| {
        OrchError::Unavailable("hibernation snapshot is missing memory metadata".into())
    })?;
    claimed.vcpus = snapshot.vcpus.ok_or_else(|| {
        OrchError::Unavailable("hibernation snapshot is missing vCPU metadata".into())
    })?;
    claimed.kernel_path = snapshot.kernel_path.clone().ok_or_else(|| {
        OrchError::Unavailable("hibernation snapshot is missing kernel metadata".into())
    })?;
    claimed.rootfs_path = snapshot.rootfs_path.clone();
    claimed.rootfs_read_only = restored_rootfs_read_only(snapshot.rootfs_read_only);
    claimed.cmdline = snapshot.cmdline.clone().ok_or_else(|| {
        OrchError::Unavailable("hibernation snapshot is missing command-line metadata".into())
    })?;
    claimed.runtime_layout = None;
    claimed.socket_path = None;
    claimed.pid = None;
    let local_hibernation = tarit_store::HibernationRecord {
        vm_id: id,
        owner_key: owner_key.clone(),
        snapshot_path: snapshot.path.clone(),
        created_at: durable.created_at,
        updated_at: Utc::now(),
    };
    {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?;
        store.insert_vm(&claimed).map_err(crate::api::store_err)?;
        store
            .upsert_hibernation(&local_hibernation)
            .map_err(crate::api::store_err)?;
        store
            .upsert_recovered_egress_policy(&durable.egress_policy)
            .map_err(crate::api::store_err)?;
    }
    commit_vm_record(state, claimed.clone())?;
    refresh_running_lifecycle(state, &claimed)?;
    resume_hibernated_local(state, id, true).await
}

/// Resolve an activation-capable ingress and transparently recover a durable
/// hibernated VM when its former owner is stale. Healthy remote owners remain
/// remote and continue through the ordinary fenced peer path.
pub async fn resolve_owner_for_activation(
    state: &AppState,
    id: Uuid,
    identity: &crate::config::ApiIdentity,
) -> Result<cluster::Owner, OrchError> {
    match cluster::resolve_owner(state, id).await {
        Ok(cluster::Owner::Local) => {
            if matches!(vm_get(state, id), Err(OrchError::NotFound(_))) {
                recover_hibernated_on_stale_owner(state, id, identity).await?;
            }
            Ok(cluster::Owner::Local)
        }
        Ok(owner) => Ok(owner),
        Err(OrchError::Unavailable(_)) => {
            recover_hibernated_on_stale_owner(state, id, identity).await?;
            Ok(cluster::Owner::Local)
        }
        Err(error) => Err(error),
    }
}

fn activation_gate(state: &AppState, id: Uuid) -> Result<Arc<tokio::sync::Mutex<()>>, OrchError> {
    let mut gates = state
        .activation_gates
        .lock()
        .map_err(|_| OrchError::Internal("activation gate map poisoned".into()))?;
    Ok(Arc::clone(
        gates
            .entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    ))
}

/// Make a logical VM runnable, coalescing every concurrent wake source into
/// one restore. This is deliberately callable before a supervisor operation
/// gate exists: hibernation removes the old runtime and its operation gate.
pub(crate) async fn ensure_active_local(state: &AppState, id: Uuid) -> Result<VmRecord, OrchError> {
    let current = vm_get(state, id)?;
    if current.status == VmStatus::Hibernated {
        resume_hibernated_local(state, id, false).await
    } else {
        if current.status == VmStatus::Running {
            cleanup_hibernation_snapshots_for_vm(state, id).await?;
        }
        Ok(current)
    }
}

async fn resume_hibernated_local(
    state: &AppState,
    id: Uuid,
    allow_degraded_artifact: bool,
) -> Result<VmRecord, OrchError> {
    let gate = activation_gate(state, id)?;
    let _activation = gate.lock().await;

    // A waiter in the same ingress burst observes the first caller's durable
    // publication and never starts a second VMM.
    let current = vm_get(state, id)?;
    if current.status != VmStatus::Hibernated {
        return if current.status == VmStatus::Running {
            Ok(current)
        } else {
            Err(OrchError::Conflict(format!(
                "cannot activate VM {id} from {}",
                current.status.as_str()
            )))
        };
    }
    let owner_key = current.owner_key.clone().ok_or_else(|| {
        OrchError::Internal(format!("vm {id} has no tenant owner for activation"))
    })?;
    let desired_egress = {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?;
        store
            .get_egress_policy(&owner_key, id)
            .map_err(crate::api::store_err)?
    };
    let hibernation = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .get_hibernation(&owner_key, id)
        .map_err(crate::api::store_err)?;
    let snapshot =
        verify_snapshot_access(state, &hibernation.snapshot_path, Some(&owner_key), false)?;
    verify_snapshot_artifact_ready(
        state,
        &snapshot,
        state.config.image_admission_policy.require_signature,
        allow_degraded_artifact,
    )
    .await?;
    let integrity = crate::supervisor::verify_snapshot_integrity(&snapshot)?;

    let memory_mib = snapshot.memory_mib.ok_or_else(|| {
        OrchError::BadRequest("hibernation snapshot is missing memory metadata".into())
    })?;
    let vcpus = snapshot.vcpus.ok_or_else(|| {
        OrchError::BadRequest("hibernation snapshot is missing vCPU metadata".into())
    })?;
    let kernel_path = snapshot.kernel_path.clone().ok_or_else(|| {
        OrchError::BadRequest("hibernation snapshot is missing kernel metadata".into())
    })?;
    let cmdline = snapshot.cmdline.clone().ok_or_else(|| {
        OrchError::BadRequest("hibernation snapshot is missing command-line metadata".into())
    })?;
    let snapshot_overlay_path = match (
        snapshot.rootfs_path.as_ref(),
        snapshot.overlay_path.as_ref(),
    ) {
        (Some(_), Some(path)) => Some(path.clone()),
        (None, None) => None,
        (Some(_), None) => {
            return Err(OrchError::BadRequest(
                "hibernation snapshot is missing its disk artifact".into(),
            ));
        }
        (None, Some(_)) => {
            return Err(OrchError::BadRequest(
                "rootfs-less hibernation has unexpected disk metadata".into(),
            ));
        }
    };
    let data_volumes = attached_volume_spawn_config(state, &owner_key, id).await?;
    let restore_config = VmSpawnConfig {
        memory_mib,
        vcpus,
        kernel_path: kernel_path.clone().into(),
        rootfs_path: snapshot.rootfs_path.clone().map(Into::into),
        cmdline: cmdline.clone(),
        read_only: restored_rootfs_read_only(snapshot.rootfs_read_only),
        egress_allowlist: desired_egress
            .as_ref()
            .map(|policy| policy.allowlist.clone())
            .unwrap_or_default(),
        egress_allow_existing: desired_egress
            .as_ref()
            .is_some_and(|policy| policy.allow_existing),
        data_volumes: data_volumes.clone(),
    };
    let ticket = match state
        .supervisor
        .begin_boot_with_registration(
            id,
            SpawnPurpose::Live,
            restore_config.resource_shape(),
            || async { Ok(()) },
        )
        .await
    {
        Ok(ticket) => ticket,
        Err(error @ OrchError::Conflict(_)) => {
            if !state
                .supervisor
                .wait_for_registered_boot_or_running(id)
                .await?
            {
                return Err(error);
            }
            let joined = vm_get(state, id)?;
            if joined.status == VmStatus::Running {
                return Ok(joined);
            }
            return Err(OrchError::Conflict(format!(
                "registered activation for VM {id} completed in {}",
                joined.status.as_str()
            )));
        }
        Err(error) => return Err(error),
    };
    let path = hibernation.snapshot_path.clone();
    let shape = restore_config.resource_shape();
    let supervisor = Arc::clone(&state.supervisor);
    let booted = tokio::task::spawn_blocking(move || {
        supervisor.restore_vm(
            ticket,
            path,
            snapshot_overlay_path,
            restore_config,
            shape,
            integrity,
        )
    })
    .await
    .map_err(|join| {
        state
            .supervisor
            .cleanup_boot_join_failure(id, "hibernation restore task", join)
    })??;

    let publication_state = state.clone();
    let mut publication_record = current;
    publication_record.startup_path = Some(VmStartupPath::SnapshotRestore);
    publication_record.memory_mib = memory_mib;
    publication_record.vcpus = vcpus;
    publication_record.kernel_path = kernel_path;
    publication_record.rootfs_path = snapshot.rootfs_path;
    publication_record.rootfs_read_only = restored_rootfs_read_only(snapshot.rootfs_read_only);
    publication_record.cmdline = cmdline;
    publication_record.runtime_layout = Some(
        state.supervisor.runtime_layout_for_snapshot_restore(
            id,
            &VmSpawnConfig {
                memory_mib,
                vcpus,
                kernel_path: publication_record.kernel_path.clone().into(),
                rootfs_path: publication_record.rootfs_path.clone().map(Into::into),
                cmdline: publication_record.cmdline.clone(),
                read_only: publication_record.rootfs_read_only,
                egress_allowlist: desired_egress
                    .as_ref()
                    .map(|policy| policy.allowlist.clone())
                    .unwrap_or_default(),
                egress_allow_existing: desired_egress
                    .as_ref()
                    .is_some_and(|policy| policy.allow_existing),
                data_volumes,
            },
            PathBuf::from(&hibernation.snapshot_path).as_path(),
        ),
    );
    let record = state
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
        .await?;
    mark_running(state, record.clone())?;

    if let Some(fleet) = state.fleet.as_ref() {
        if let Err(error) = fleet.delete_hibernation(&owner_key, id).await {
            tracing::warn!(%id, %error, "durable fleet hibernation cleanup deferred after successful resume");
        }
    }

    // The relation is removed only after the replacement runtime is ready and
    // its Running record is durable. Snapshot GC remains separately governed
    // by its snapshot ownership row.
    state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .delete_hibernation(&owner_key, id)
        .map_err(crate::api::store_err)?;
    cleanup_hibernation_snapshots_for_vm(state, id).await?;
    Ok(record)
}

pub async fn snapshot_local(state: &AppState, id: Uuid, diff: bool) -> Result<String, OrchError> {
    if diff {
        return Err(OrchError::Unprocessable(
            "incremental orchestrator snapshots are disabled until durable parent-chain relocation is available; request a full snapshot with diff=false"
                .into(),
        ));
    }
    // Scale-to-zero removes the supervisor gate, but not the VM. Check the
    // durable status before looking up a live-runtime gate so callers receive
    // a lifecycle conflict for hibernated VMs rather than a false 404.
    ensure_vm_status(
        state,
        id,
        "snapshot",
        &[VmStatus::Running, VmStatus::Paused],
    )?;
    let gate = state.supervisor.operation_gate(id)?;
    let _operation = gate.lock_owned().await;
    Ok(snapshot_local_locked(state, id, diff, None, None)
        .await?
        .path)
}

pub(crate) struct ForkSnapshotOutcome {
    pub(crate) path: String,
    pub(crate) live_stats: Option<tarit_proto::LiveSnapshotStats>,
}

/// Create the private snapshot that backs one local live-fork attempt. Its
/// child binding is committed with the snapshot row, so terminal cleanup never
/// has to infer ownership from filenames or timing.
pub async fn snapshot_local_for_fork(
    state: &AppState,
    source_id: Uuid,
    child_id: Uuid,
) -> Result<ForkSnapshotOutcome, OrchError> {
    if let Some(existing) = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .get_snapshot_by_id(child_id)
        .map_err(crate::api::store_err)?
    {
        if existing.vm_id != source_id || existing.ephemeral_owner_vm_id != Some(child_id) {
            return Err(OrchError::Conflict(format!(
                "fork artifact {child_id} is already bound to another operation"
            )));
        }
        return Ok(ForkSnapshotOutcome {
            path: existing.path,
            live_stats: None,
        });
    }
    ensure_vm_status(state, source_id, "fork", &[VmStatus::Running])?;
    let gate = state.supervisor.operation_gate(source_id)?;
    let _operation = gate.lock_owned().await;
    snapshot_local_locked(state, source_id, false, Some(child_id), Some(child_id)).await
}

pub fn bind_localized_snapshot_to_fork(
    state: &AppState,
    snapshot_path: &str,
    child_id: Uuid,
) -> Result<(), OrchError> {
    state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .bind_snapshot_ephemeral_owner(snapshot_path, child_id)
        .map_err(crate::api::store_err)
}

/// Snapshot implementation for callers that already hold the exact runtime's
/// operation gate (hibernate must keep it through snapshot and teardown).
async fn snapshot_local_locked(
    state: &AppState,
    id: Uuid,
    diff: bool,
    ephemeral_owner_vm_id: Option<Uuid>,
    snapshot_id: Option<Uuid>,
) -> Result<ForkSnapshotOutcome, OrchError> {
    let vm = ensure_vm_status(
        state,
        id,
        "snapshot",
        &[VmStatus::Running, VmStatus::Paused],
    )?;
    let running = vm.status == VmStatus::Running;
    let overlay_path = vm
        .runtime_layout
        .as_ref()
        .and_then(|layout| layout.overlay_path.as_ref())
        .map(PathBuf::from);
    let memory_mib = vm.memory_mib;
    let sup = Arc::clone(&state.supervisor);
    let bundle = tokio::task::spawn_blocking(move || {
        if running {
            sup.live_snapshot_bundle_vm(id, memory_mib, overlay_path.is_some())
        } else {
            sup.snapshot_bundle_vm(id, diff, false, overlay_path, memory_mib)
        }
    })
    .await;
    let mut bundle = match bundle {
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
    let integrity = bundle.integrity()?;
    let admitted_image = if let Some(rootfs_path) = vm.rootfs_path.as_deref() {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .get_image_by_rootfs_path(rootfs_path)
            .map_err(crate::api::store_err)?
    } else {
        None
    };
    let artifact_identity = admitted_image
        .map(|image_record| {
            image::verify_admitted_image(&image_record, &state.config.image_admission_policy)
                .map_err(|error| {
                    OrchError::Unprocessable(format!(
                        "snapshot image failed immutable admission: {error}"
                    ))
                })?;
            let immutable_image_digest = image_record.source_digest.ok_or_else(|| {
                OrchError::Unprocessable("snapshot image has no immutable source digest".into())
            })?;
            let rootfs_digest = image_record.rootfs_digest.ok_or_else(|| {
                OrchError::Unprocessable("snapshot image has no immutable rootfs digest".into())
            })?;
            let agent_digest = image_record.agent_digest.ok_or_else(|| {
                OrchError::Unprocessable("snapshot image has no injected-agent digest".into())
            })?;
            let kernel_digest = image::sha256_regular_file(std::path::Path::new(&vm.kernel_path))
                .map_err(|error| {
                OrchError::Unprocessable(format!(
                    "snapshot kernel failed immutable admission: {error}"
                ))
            })?;
            let boot_manifest_digest = ArtifactBootMetadata {
                version: ArtifactBootMetadata::VERSION,
                kernel_digest,
                immutable_image_digest: immutable_image_digest.clone(),
                rootfs_digest,
                agent_digest: agent_digest.clone(),
                memory_mib: vm.memory_mib,
                vcpus: vm.vcpus,
                cmdline: vm.cmdline.clone(),
                rootfs_read_only: vm.rootfs_read_only,
            }
            .digest()
            .map_err(|error| {
                OrchError::Internal(format!("encode artifact boot manifest: {error}"))
            })?;
            Ok::<_, OrchError>((immutable_image_digest, agent_digest, boot_manifest_digest))
        })
        .transpose()?;
    // R-006: record who owns this snapshot file so a later restore can verify
    // tenant access before the path is handed to the VMM. Fail closed if the
    // record cannot be written, so we never create a snapshot that only an
    // admin could restore.
    let record = tarit_store::SnapshotRecord {
        snapshot_id: snapshot_id.unwrap_or_else(Uuid::new_v4),
        path: bundle.snapshot_path().to_string(),
        overlay_path: bundle.overlay_path().map(str::to_string),
        host_id: state.config.host_id.clone(),
        owner_key: vm.owner_key.clone(),
        api_key_id: vm.api_key_id.clone(),
        vm_id: id,
        ephemeral_owner_vm_id,
        memory_mib: Some(vm.memory_mib),
        vcpus: Some(vm.vcpus),
        kernel_path: Some(vm.kernel_path.clone()),
        rootfs_path: vm.rootfs_path.clone(),
        rootfs_read_only: Some(vm.rootfs_read_only),
        cmdline: Some(vm.cmdline.clone()),
        content_digest: Some(integrity.content_digest.clone()),
        size_bytes: Some(integrity.size_bytes),
        created_at: Utc::now(),
    };
    let artifact = artifact_identity.zip(record.owner_key.clone()).map(
        |((immutable_image_digest, agent_digest, boot_manifest_digest), owner_key)| {
            ArtifactRecord {
                artifact_id: record.snapshot_id,
                owner_key,
                host_id: record.host_id.clone(),
                storage_locator: record.path.clone(),
                kind: ArtifactKind::VmSnapshot,
                status: ArtifactStatus::Available,
                content_digest: integrity.content_digest,
                size_bytes: integrity.size_bytes,
                immutable_image_digest,
                agent_digest,
                boot_manifest_digest,
                parent_artifact_id: None,
                source_vm_id: Some(id),
                creation_revision: vm.revision,
                integrity_manifest_digest: record
                    .content_digest
                    .clone()
                    .expect("snapshot digest was populated"),
                chunk_size_bytes: integrity.chunk_size_bytes,
                chunk_count: integrity.chunk_count,
                replication_state: ArtifactReplicationState::Ready,
                reference_count: 0,
                created_at: record.created_at,
                updated_at: record.created_at,
            }
        },
    );
    let insert = {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?;
        store.insert_snapshot(&record).and_then(|_| {
            artifact
                .as_ref()
                .map_or(Ok(()), |artifact| store.insert_artifact(artifact))
        })
    };
    if let Err(error) = insert {
        // SnapshotBundle's exact-inode cleanup runs before the operation gate
        // is released, so a failed ownership write cannot leave a restorable
        // but unowned RAM/disk pair.
        if let Ok(store) = state.store.lock() {
            if let Some(artifact) = artifact.as_ref() {
                let _ = store
                    .delete_artifact_if_unreferenced(&artifact.owner_key, artifact.artifact_id);
            }
            let _ = store.delete_snapshot(&record.path);
        }
        drop(bundle);
        return Err(crate::api::store_err(error));
    }
    if let Some(fleet) = state.fleet.as_ref() {
        if let Err(error) = fleet.upsert_snapshot(&record).await {
            if let Ok(store) = state.store.lock() {
                if let Some(artifact) = artifact.as_ref() {
                    let _ = store
                        .delete_artifact_if_unreferenced(&artifact.owner_key, artifact.artifact_id);
                }
                let _ = store.delete_snapshot(&record.path);
            }
            drop(bundle);
            return Err(OrchError::Internal(format!(
                "publish opaque snapshot locator: {error}"
            )));
        }
        if let Some(artifact) = artifact.as_ref() {
            if let Err(error) = fleet.insert_artifact(artifact).await {
                let _ = fleet.delete_snapshot(&record).await;
                if let Ok(store) = state.store.lock() {
                    let _ = store
                        .delete_artifact_if_unreferenced(&artifact.owner_key, artifact.artifact_id);
                    let _ = store.delete_snapshot(&record.path);
                }
                drop(bundle);
                return Err(OrchError::Internal(format!(
                    "publish immutable artifact: {error}"
                )));
            }
            let primary_replica = tarit_types::ArtifactReplicaRecord {
                artifact_id: artifact.artifact_id,
                owner_key: artifact.owner_key.clone(),
                host_id: state.config.host_id.clone(),
                failure_domain: state.config.zone.clone(),
                storage_locator: artifact.storage_locator.clone(),
                status: tarit_types::ArtifactReplicaStatus::Available,
                content_digest: artifact.content_digest.clone(),
                size_bytes: artifact.size_bytes,
                integrity_manifest_digest: artifact.integrity_manifest_digest.clone(),
                verified_at: Some(artifact.updated_at),
                created_at: artifact.created_at,
                updated_at: artifact.updated_at,
            };
            if let Err(error) = fleet.upsert_artifact_replica(&primary_replica).await {
                let _ = fleet
                    .delete_artifact_if_unreferenced(&artifact.owner_key, artifact.artifact_id)
                    .await;
                let _ = fleet.delete_snapshot(&record).await;
                if let Ok(store) = state.store.lock() {
                    let _ = store
                        .delete_artifact_if_unreferenced(&artifact.owner_key, artifact.artifact_id);
                    let _ = store.delete_snapshot(&record.path);
                }
                drop(bundle);
                return Err(OrchError::Internal(format!(
                    "publish primary artifact replica: {error}"
                )));
            }
        }
    }
    let path = record.path;
    let live_stats = bundle.live_stats().cloned();
    bundle.persist();
    Ok(ForkSnapshotOutcome { path, live_stats })
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

async fn verify_snapshot_artifact_ready(
    state: &AppState,
    snapshot: &tarit_store::SnapshotRecord,
    artifact_required: bool,
    allow_degraded_fleet: bool,
) -> Result<Option<ArtifactRecord>, OrchError> {
    let Some(owner_key) = snapshot.owner_key.as_deref() else {
        if artifact_required {
            return Err(OrchError::Unprocessable(
                "production restore requires a tenant-owned immutable artifact".into(),
            ));
        }
        return Ok(None);
    };
    let local = {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?;
        match store.get_artifact(owner_key, snapshot.snapshot_id) {
            Ok(artifact) => Some(artifact),
            Err(tarit_store::StoreError::NotFound) => None,
            Err(error) => return Err(crate::api::store_err(error)),
        }
    };
    let Some(local) = local else {
        if artifact_required {
            return Err(OrchError::Unprocessable(
                "production restore requires a replicated immutable artifact".into(),
            ));
        }
        return Ok(None);
    };
    let snapshot_digest = snapshot.content_digest.as_deref().ok_or_else(|| {
        OrchError::Unprocessable("artifact-backed snapshot has no authenticated digest".into())
    })?;
    if local.status != ArtifactStatus::Available
        || local.replication_state != ArtifactReplicationState::Ready
        || local.content_digest != snapshot_digest
        || local.integrity_manifest_digest != snapshot_digest
        || local.size_bytes != snapshot.size_bytes.unwrap_or_default()
        || local.host_id != snapshot.host_id
        || local.storage_locator != snapshot.path
    {
        return Err(OrchError::Unavailable(
            "snapshot artifact is not verified and replication-ready".into(),
        ));
    }
    verify_artifact_boot_metadata(state, snapshot, &local)?;
    if let Some(fleet) = state.fleet.as_ref() {
        let global = fleet
            .get_artifact(owner_key, snapshot.snapshot_id)
            .await
            .map_err(|error| match error {
                tarit_fleet::FleetError::NotFound => OrchError::Unavailable(
                    "snapshot artifact is absent from the fleet index".into(),
                ),
                error => OrchError::Internal(format!("read fleet artifact: {error}")),
            })?;
        if global.status != ArtifactStatus::Available
            || (!allow_degraded_fleet
                && global.replication_state != ArtifactReplicationState::Ready)
            || (allow_degraded_fleet
                && !matches!(
                    global.replication_state,
                    ArtifactReplicationState::Ready | ArtifactReplicationState::Degraded
                ))
            || global.content_digest != local.content_digest
            || global.integrity_manifest_digest != local.integrity_manifest_digest
            || global.boot_manifest_digest != local.boot_manifest_digest
            || global.size_bytes != local.size_bytes
        {
            return Err(OrchError::Unavailable(
                "fleet artifact is not verified and replication-ready".into(),
            ));
        }
    }
    Ok(Some(local))
}

fn verify_artifact_boot_metadata(
    state: &AppState,
    snapshot: &tarit_store::SnapshotRecord,
    artifact: &ArtifactRecord,
) -> Result<(), OrchError> {
    let kernel_path = snapshot.kernel_path.as_deref().ok_or_else(|| {
        OrchError::Unprocessable("artifact is missing authenticated kernel metadata".into())
    })?;
    let rootfs_path = snapshot.rootfs_path.as_deref().ok_or_else(|| {
        OrchError::Unprocessable("artifact is missing authenticated rootfs metadata".into())
    })?;
    let image_record = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .get_image_by_rootfs_path(rootfs_path)
        .map_err(crate::api::store_err)?
        .ok_or_else(|| OrchError::Unavailable("artifact image is not admitted locally".into()))?;
    image::verify_admitted_image(&image_record, &state.config.image_admission_policy)
        .map_err(|error| OrchError::Unavailable(format!("artifact image verification: {error}")))?;
    let immutable_image_digest = image_record.source_digest.ok_or_else(|| {
        OrchError::Unprocessable("artifact image has no immutable source digest".into())
    })?;
    let rootfs_digest = image_record.rootfs_digest.ok_or_else(|| {
        OrchError::Unprocessable("artifact image has no immutable rootfs digest".into())
    })?;
    let agent_digest = image_record.agent_digest.ok_or_else(|| {
        OrchError::Unprocessable("artifact image has no injected-agent digest".into())
    })?;
    let boot = ArtifactBootMetadata {
        version: ArtifactBootMetadata::VERSION,
        kernel_digest: image::sha256_regular_file(std::path::Path::new(kernel_path)).map_err(
            |error| OrchError::Unavailable(format!("artifact kernel verification: {error}")),
        )?,
        immutable_image_digest,
        rootfs_digest,
        agent_digest,
        memory_mib: snapshot.memory_mib.ok_or_else(|| {
            OrchError::Unprocessable("artifact is missing authenticated memory metadata".into())
        })?,
        vcpus: snapshot.vcpus.ok_or_else(|| {
            OrchError::Unprocessable("artifact is missing authenticated vCPU metadata".into())
        })?,
        cmdline: snapshot.cmdline.clone().ok_or_else(|| {
            OrchError::Unprocessable("artifact is missing authenticated command line".into())
        })?,
        rootfs_read_only: snapshot.rootfs_read_only.ok_or_else(|| {
            OrchError::Unprocessable("artifact is missing authenticated rootfs mode".into())
        })?,
    };
    let digest = boot
        .digest()
        .map_err(|error| OrchError::Internal(format!("encode artifact boot metadata: {error}")))?;
    if digest != artifact.boot_manifest_digest
        || boot.immutable_image_digest != artifact.immutable_image_digest
        || boot.agent_digest != artifact.agent_digest
    {
        return Err(OrchError::Unavailable(
            "artifact boot metadata authentication failed".into(),
        ));
    }
    Ok(())
}

pub async fn localize_branch_artifact(
    state: &AppState,
    artifact: &ArtifactRecord,
    identity: &crate::config::ApiIdentity,
    require_replication_ready: bool,
) -> Result<String, OrchError> {
    if state.supervisor.disk_pressure_snapshot().pressured {
        return Err(OrchError::Overloaded {
            message: "artifact localization is blocked by filesystem pressure".into(),
            retry_after_secs: 5,
        });
    }
    let gate = activation_gate(state, artifact.artifact_id)?;
    let _localization = gate.lock_owned().await;
    let existing_local = {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?;
        store
            .get_artifact(&identity.tenant, artifact.artifact_id)
            .ok()
            .and_then(|local| {
                store
                    .get_snapshot_by_id(artifact.artifact_id)
                    .ok()
                    .flatten()
                    .map(|snapshot| (local, snapshot))
            })
    };
    if let Some((local, snapshot)) = existing_local {
        if local.status == ArtifactStatus::Available
            && local.replication_state == ArtifactReplicationState::Ready
        {
            verify_artifact_boot_metadata(state, &snapshot, &local)?;
            if let Some(fleet) = state.fleet.as_ref() {
                let replica = tarit_types::ArtifactReplicaRecord {
                    artifact_id: artifact.artifact_id,
                    owner_key: identity.tenant.clone(),
                    host_id: state.config.host_id.clone(),
                    failure_domain: state.config.zone.clone(),
                    storage_locator: local.storage_locator.clone(),
                    status: tarit_types::ArtifactReplicaStatus::Available,
                    content_digest: local.content_digest.clone(),
                    size_bytes: local.size_bytes,
                    integrity_manifest_digest: local.integrity_manifest_digest.clone(),
                    verified_at: Some(Utc::now()),
                    created_at: snapshot.created_at,
                    updated_at: Utc::now(),
                };
                let replication_state =
                    fleet
                        .upsert_artifact_replica(&replica)
                        .await
                        .map_err(|error| {
                            OrchError::Internal(format!(
                                "repair fleet replica publication: {error}"
                            ))
                        })?;
                if require_replication_ready && replication_state != ArtifactReplicationState::Ready
                {
                    return Err(OrchError::Unavailable(
                        "localized artifact has not satisfied replication policy".into(),
                    ));
                }
            }
            return Ok(local.storage_locator);
        }
    }
    let fleet = state
        .fleet
        .as_ref()
        .ok_or_else(|| OrchError::NotFound("artifact is not available on this node".into()))?;
    let mut replicas = fleet
        .list_artifact_replicas(&identity.tenant, artifact.artifact_id)
        .await
        .map_err(|error| OrchError::Internal(format!("list fleet replicas: {error}")))?;
    replicas.retain(|replica| {
        replica.status == tarit_types::ArtifactReplicaStatus::Available
            && replica.verified_at.is_some()
            && replica.host_id != state.config.host_id
    });
    replicas.sort_by_key(|replica| (replica.host_id != artifact.host_id, replica.host_id.clone()));
    let mut source = None;
    for replica in replicas {
        match cluster::peer_rpc(state, &replica.host_id).await {
            Ok(Some(target)) => {
                source = Some(target);
                break;
            }
            Ok(None) | Err(OrchError::NotFound(_) | OrchError::Unavailable(_)) => continue,
            Err(error) => return Err(error),
        }
    }
    let source = source.ok_or_else(|| {
        OrchError::Unavailable("no healthy verified artifact replica is reachable".into())
    })?;
    let peer = Arc::clone(&state.peer);
    let descriptor_target = source.clone();
    let descriptor_identity = identity.clone();
    let artifact_id = artifact.artifact_id;
    let descriptor = tokio::task::spawn_blocking(move || {
        peer.artifact_descriptor(&descriptor_target, artifact_id, &descriptor_identity)
    })
    .await
    .map_err(|error| OrchError::Internal(format!("artifact descriptor join: {error}")))??;
    if descriptor.artifact_id != artifact.artifact_id
        || descriptor.content_digest != artifact.content_digest
        || descriptor.size_bytes != artifact.size_bytes
        || descriptor.immutable_image_digest != artifact.immutable_image_digest
        || descriptor.agent_digest != artifact.agent_digest
        || descriptor.boot_manifest_digest != artifact.boot_manifest_digest
        || descriptor.creation_revision != artifact.creation_revision
        || descriptor.integrity_manifest_digest != artifact.integrity_manifest_digest
        || descriptor.chunk_size_bytes != artifact.chunk_size_bytes
        || descriptor.chunk_count != artifact.chunk_count
        || descriptor.ram_bytes.checked_add(descriptor.overlay_bytes) != Some(descriptor.size_bytes)
        || descriptor.has_overlay != (descriptor.overlay_bytes > 0)
    {
        return Err(OrchError::Unavailable(
            "peer artifact descriptor does not match the fleet manifest".into(),
        ));
    }
    let boot_metadata = ArtifactBootMetadata {
        version: ArtifactBootMetadata::VERSION,
        kernel_digest: descriptor.kernel_digest.clone(),
        immutable_image_digest: descriptor.immutable_image_digest.clone(),
        rootfs_digest: descriptor.rootfs_digest.clone(),
        agent_digest: descriptor.agent_digest.clone(),
        memory_mib: descriptor.memory_mib,
        vcpus: descriptor.vcpus,
        cmdline: descriptor.cmdline.clone(),
        rootfs_read_only: descriptor.rootfs_read_only,
    };
    if boot_metadata.digest().map_err(|error| {
        OrchError::Internal(format!("encode peer artifact boot metadata: {error}"))
    })? != artifact.boot_manifest_digest
    {
        return Err(OrchError::Unavailable(
            "peer artifact boot metadata failed authentication".into(),
        ));
    }
    let local_kernel = image::sha256_regular_file(&state.config.kernel)
        .ok()
        .filter(|digest| digest == &descriptor.kernel_digest)
        .map(|_| state.config.kernel.clone());
    let local_image = {
        state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
            .get_image_by_source_digest(&artifact.immutable_image_digest)
            .map_err(crate::api::store_err)?
    }
    .filter(|image_record| {
        image_record.rootfs_digest.as_deref() == Some(descriptor.rootfs_digest.as_str())
            && image_record.size_bytes == descriptor.rootfs_bytes
            && image_record.agent_digest.as_deref() == Some(artifact.agent_digest.as_str())
            && image::verify_admitted_image(image_record, &state.config.image_admission_policy)
                .is_ok()
    });

    let boot_dir = state.config.images_dir.join("peer-boot-inputs");
    if local_kernel.is_none() || local_image.is_none() {
        create_private_directory(&boot_dir, "peer boot-input directory")?;
    }
    let boot_bytes = (local_kernel.is_none() as u64)
        .checked_mul(descriptor.kernel_bytes)
        .and_then(|bytes| {
            bytes.checked_add((local_image.is_none() as u64) * descriptor.rootfs_bytes)
        })
        .ok_or_else(|| OrchError::Unprocessable("boot-input localization size overflow".into()))?;
    let _boot_reservation = (boot_bytes > 0)
        .then(|| {
            state.supervisor.reserve_artifact_localization(
                boot_dir.clone(),
                boot_bytes,
                u64::from(local_kernel.is_none()) + u64::from(local_image.is_none()),
            )
        })
        .transpose()?;
    let boot_token = Uuid::new_v4();
    let staging_kernel = boot_dir.join(format!(".kernel-stage-{boot_token}"));
    let staging_rootfs = boot_dir.join(format!(".rootfs-stage-{boot_token}"));
    let final_kernel = boot_dir.join(format!("kernel-{}-{boot_token}", artifact.artifact_id));
    let final_rootfs = boot_dir.join(format!("rootfs-{}-{boot_token}.ext4", artifact.artifact_id));
    if local_kernel.is_none() || local_image.is_none() {
        let peer = Arc::clone(&state.peer);
        let target = source.clone();
        let transfer_identity = identity.clone();
        let transfer_descriptor = descriptor.clone();
        let need_kernel = local_kernel.is_none();
        let need_rootfs = local_image.is_none();
        let kernel_stage = staging_kernel.clone();
        let rootfs_stage = staging_rootfs.clone();
        let transfer = tokio::task::spawn_blocking(move || {
            if need_kernel {
                let mut kernel = create_private_replica_file(&kernel_stage)?;
                let (bytes, digest) = peer.download_artifact_component(
                    &target,
                    transfer_descriptor.artifact_id,
                    "kernel",
                    &transfer_identity,
                    &mut kernel,
                    transfer_descriptor.kernel_bytes,
                )?;
                if bytes != transfer_descriptor.kernel_bytes
                    || digest != transfer_descriptor.kernel_digest
                {
                    return Err(OrchError::Unprocessable(
                        "peer kernel digest or length mismatch".into(),
                    ));
                }
            }
            if need_rootfs {
                let mut rootfs = create_private_replica_file(&rootfs_stage)?;
                let (bytes, digest) = peer.download_artifact_component(
                    &target,
                    transfer_descriptor.artifact_id,
                    "rootfs",
                    &transfer_identity,
                    &mut rootfs,
                    transfer_descriptor.rootfs_bytes,
                )?;
                if bytes != transfer_descriptor.rootfs_bytes
                    || digest != transfer_descriptor.rootfs_digest
                {
                    return Err(OrchError::Unprocessable(
                        "peer rootfs digest or length mismatch".into(),
                    ));
                }
            }
            Ok::<(), OrchError>(())
        })
        .await
        .map_err(|error| OrchError::Internal(format!("boot-input transfer join: {error}")))?;
        if let Err(error) = transfer {
            cleanup_replica_paths([&staging_kernel, &staging_rootfs]);
            return Err(error);
        }
        use std::os::unix::fs::PermissionsExt as _;
        let publish = (|| -> std::io::Result<()> {
            if local_kernel.is_none() {
                std::fs::set_permissions(&staging_kernel, std::fs::Permissions::from_mode(0o444))?;
                std::fs::rename(&staging_kernel, &final_kernel)?;
            }
            if local_image.is_none() {
                std::fs::set_permissions(&staging_rootfs, std::fs::Permissions::from_mode(0o444))?;
                std::fs::rename(&staging_rootfs, &final_rootfs)?;
            }
            Ok(())
        })();
        if let Err(error) = publish {
            cleanup_replica_paths([
                &staging_kernel,
                &staging_rootfs,
                &final_kernel,
                &final_rootfs,
            ]);
            return Err(OrchError::Internal(format!(
                "publish localized boot inputs: {error}"
            )));
        }
    }
    let kernel_path = local_kernel.unwrap_or(final_kernel);
    let image_record = match local_image {
        Some(image_record) => image_record,
        None => {
            let image_record = tarit_store::ImageRecord {
                name: format!("peer-{}", artifact.artifact_id),
                tag: "immutable".into(),
                rootfs_path: final_rootfs.display().to_string(),
                created_at: Utc::now(),
                size_bytes: descriptor.rootfs_bytes,
                source_ref: descriptor.image_source_ref.clone(),
                source_digest: Some(descriptor.immutable_image_digest.clone()),
                rootfs_digest: Some(descriptor.rootfs_digest.clone()),
                agent_digest: Some(descriptor.agent_digest.clone()),
                provenance_key_digest: descriptor.provenance_key_digest.clone(),
                provenance_verified_at: descriptor.provenance_verified_at,
                golden_snapshot_path: None,
            };
            if let Err(error) =
                image::verify_admitted_image(&image_record, &state.config.image_admission_policy)
            {
                cleanup_replica_paths([&final_rootfs]);
                return Err(OrchError::Unavailable(format!(
                    "transferred image failed admission: {error}"
                )));
            }
            state
                .store
                .lock()
                .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
                .upsert_image(&image_record)
                .map_err(crate::api::store_err)?;
            image_record
        }
    };

    let snapshot_dir = state.config.socket_dir.join("snapshots");
    if !snapshot_dir.exists() {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(&snapshot_dir).map_err(|error| {
            OrchError::Internal(format!("create replica snapshot directory: {error}"))
        })?;
    }
    let snapshot_dir_metadata = std::fs::symlink_metadata(&snapshot_dir).map_err(|error| {
        OrchError::Internal(format!("inspect replica snapshot directory: {error}"))
    })?;
    use std::os::unix::fs::PermissionsExt as _;
    if !snapshot_dir_metadata.is_dir()
        || snapshot_dir_metadata.file_type().is_symlink()
        || snapshot_dir_metadata.permissions().mode() & 0o077 != 0
    {
        return Err(OrchError::Unavailable(
            "replica snapshot directory is not a private real directory".into(),
        ));
    }
    let localization_bytes = descriptor
        .ram_bytes
        .checked_add(descriptor.overlay_bytes)
        .and_then(|bytes| bytes.checked_add(descriptor.integrity_bytes))
        .ok_or_else(|| OrchError::Unprocessable("artifact localization size overflow".into()))?;
    // Hold the exact reservation through sync+rename+metadata publication so
    // concurrent snapshots/localizations cannot turn a clean preflight into a
    // partial ENOSPC transfer.
    let _disk_reservation = state.supervisor.reserve_artifact_localization(
        snapshot_dir.clone(),
        localization_bytes,
        if descriptor.has_overlay { 3 } else { 2 },
    )?;
    let token = Uuid::new_v4();
    let staging_ram = snapshot_dir.join(format!(".replica-stage-{token}.ram"));
    let staging_overlay = snapshot_dir.join(format!(".replica-stage-{token}.cow"));
    let staging_integrity = PathBuf::from(format!("{}.integrity", staging_ram.display()));
    let final_ram = snapshot_dir.join(format!("replica-{}-{token}.ram", artifact.artifact_id));
    let final_overlay = snapshot_dir.join(format!("replica-{}-{token}.cow", artifact.artifact_id));
    let final_integrity = PathBuf::from(format!("{}.integrity", final_ram.display()));
    let transfer_result = {
        let peer = Arc::clone(&state.peer);
        let target = source.clone();
        let identity = identity.clone();
        let ram_path = staging_ram.clone();
        let overlay_path = staging_overlay.clone();
        let integrity_path = staging_integrity.clone();
        let descriptor = descriptor.clone();
        tokio::task::spawn_blocking(move || {
            let mut ram = create_private_replica_file(&ram_path)?;
            let (ram_bytes, _) = peer.download_artifact_component(
                &target,
                descriptor.artifact_id,
                "ram",
                &identity,
                &mut ram,
                descriptor.ram_bytes,
            )?;
            if ram_bytes != descriptor.ram_bytes {
                return Err(OrchError::Unprocessable(
                    "peer RAM artifact length mismatch".into(),
                ));
            }
            if descriptor.has_overlay {
                let mut overlay = create_private_replica_file(&overlay_path)?;
                let (bytes, _) = peer.download_artifact_component(
                    &target,
                    descriptor.artifact_id,
                    "overlay",
                    &identity,
                    &mut overlay,
                    descriptor.overlay_bytes,
                )?;
                if bytes != descriptor.overlay_bytes {
                    return Err(OrchError::Unprocessable(
                        "peer overlay artifact length mismatch".into(),
                    ));
                }
            }
            let manifest_bound = descriptor
                .chunk_count
                .checked_mul(128)
                .and_then(|bytes| bytes.checked_add(1024 * 1024))
                .ok_or_else(|| OrchError::Unprocessable("manifest size bound overflow".into()))?;
            if descriptor.integrity_bytes > manifest_bound {
                return Err(OrchError::Unprocessable(
                    "peer integrity manifest exceeds its authenticated bound".into(),
                ));
            }
            let mut integrity = create_private_replica_file(&integrity_path)?;
            let (bytes, digest) = peer.download_artifact_component(
                &target,
                descriptor.artifact_id,
                "integrity",
                &identity,
                &mut integrity,
                manifest_bound,
            )?;
            if bytes != descriptor.integrity_bytes || digest != descriptor.integrity_manifest_digest
            {
                return Err(OrchError::Unprocessable(
                    "peer integrity manifest digest or length mismatch".into(),
                ));
            }
            Ok::<(), OrchError>(())
        })
        .await
        .map_err(|error| OrchError::Internal(format!("artifact transfer join: {error}")))?
    };
    if let Err(error) = transfer_result {
        cleanup_replica_paths([&staging_ram, &staging_overlay, &staging_integrity]);
        return Err(error);
    }
    let staging_snapshot = tarit_store::SnapshotRecord {
        snapshot_id: artifact.artifact_id,
        path: staging_ram.display().to_string(),
        overlay_path: descriptor
            .has_overlay
            .then(|| staging_overlay.display().to_string()),
        host_id: state.config.host_id.clone(),
        owner_key: Some(identity.tenant.clone()),
        api_key_id: Some(identity.api_key_id.clone()),
        vm_id: descriptor.source_vm_id,
        ephemeral_owner_vm_id: None,
        memory_mib: Some(descriptor.memory_mib),
        vcpus: Some(descriptor.vcpus),
        kernel_path: Some(kernel_path.display().to_string()),
        rootfs_path: Some(image_record.rootfs_path.clone()),
        rootfs_read_only: Some(descriptor.rootfs_read_only),
        cmdline: Some(descriptor.cmdline.clone()),
        content_digest: Some(descriptor.content_digest.clone()),
        size_bytes: Some(descriptor.size_bytes),
        created_at: Utc::now(),
    };
    if let Err(error) = crate::supervisor::verify_snapshot_integrity(&staging_snapshot) {
        cleanup_replica_paths([&staging_ram, &staging_overlay, &staging_integrity]);
        return Err(error);
    }
    std::fs::rename(&staging_ram, &final_ram)
        .and_then(|_| {
            if descriptor.has_overlay {
                std::fs::rename(&staging_overlay, &final_overlay)
            } else {
                Ok(())
            }
        })
        .and_then(|_| std::fs::rename(&staging_integrity, &final_integrity))
        .map_err(|error| {
            cleanup_replica_paths([
                &staging_ram,
                &staging_overlay,
                &staging_integrity,
                &final_ram,
                &final_overlay,
                &final_integrity,
            ]);
            OrchError::Internal(format!("publish localized artifact files: {error}"))
        })?;
    let final_snapshot = tarit_store::SnapshotRecord {
        path: final_ram.display().to_string(),
        overlay_path: descriptor
            .has_overlay
            .then(|| final_overlay.display().to_string()),
        ..staging_snapshot
    };
    let local_artifact = ArtifactRecord {
        host_id: state.config.host_id.clone(),
        storage_locator: final_snapshot.path.clone(),
        replication_state: ArtifactReplicationState::Ready,
        reference_count: 0,
        created_at: final_snapshot.created_at,
        updated_at: final_snapshot.created_at,
        ..artifact.clone()
    };
    let local_insert = {
        let store = state
            .store
            .lock()
            .map_err(|_| OrchError::Internal("store lock poisoned".into()))?;
        store
            .insert_snapshot(&final_snapshot)
            .and_then(|_| store.insert_artifact(&local_artifact))
    };
    if let Err(error) = local_insert {
        if let Ok(store) = state.store.lock() {
            let _ = store.delete_snapshot(&final_snapshot.path);
            let _ = store
                .delete_artifact_if_unreferenced(&local_artifact.owner_key, artifact.artifact_id);
        }
        cleanup_replica_paths([&final_ram, &final_overlay, &final_integrity]);
        return Err(crate::api::store_err(error));
    }
    let replica = tarit_types::ArtifactReplicaRecord {
        artifact_id: artifact.artifact_id,
        owner_key: identity.tenant.clone(),
        host_id: state.config.host_id.clone(),
        failure_domain: state.config.zone.clone(),
        storage_locator: final_snapshot.path.clone(),
        status: tarit_types::ArtifactReplicaStatus::Available,
        content_digest: artifact.content_digest.clone(),
        size_bytes: artifact.size_bytes,
        integrity_manifest_digest: artifact.integrity_manifest_digest.clone(),
        verified_at: Some(Utc::now()),
        created_at: final_snapshot.created_at,
        updated_at: Utc::now(),
    };
    let state_value = fleet
        .upsert_artifact_replica(&replica)
        .await
        .map_err(|error| OrchError::Internal(format!("publish fleet replica: {error}")))?;
    if require_replication_ready && state_value != ArtifactReplicationState::Ready {
        return Err(OrchError::Unavailable(
            "artifact replica was verified but the failure-domain policy is not yet satisfied"
                .into(),
        ));
    }
    Ok(final_snapshot.path)
}

fn create_private_replica_file(path: &std::path::Path) -> Result<std::fs::File, OrchError> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| OrchError::Internal(format!("create replica staging file: {error}")))
}

fn create_private_directory(path: &std::path::Path, purpose: &str) -> Result<(), OrchError> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    if !path.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(path)
            .map_err(|error| OrchError::Internal(format!("create {purpose}: {error}")))?;
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| OrchError::Internal(format!("inspect {purpose}: {error}")))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(OrchError::Unavailable(format!(
            "{purpose} is not a private real directory"
        )));
    }
    Ok(())
}

fn cleanup_replica_paths<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

pub async fn egress_local(
    state: &AppState,
    id: Uuid,
    allowlist: Vec<String>,
    allow_existing: bool,
) -> Result<usize, OrchError> {
    let owner_key = vm_get(state, id)?.owner_key.ok_or_else(|| {
        OrchError::BadRequest("admin-owned VM has no durable tenant policy".into())
    })?;
    let current = get_egress_policy_local(state, id, &owner_key)?;
    let policy = put_egress_policy_local(
        state,
        id,
        &owner_key,
        current.revision,
        allowlist,
        allow_existing,
    )
    .await?;
    Ok(policy.allowlist.len())
}

pub fn get_egress_policy_local(
    state: &AppState,
    id: Uuid,
    owner_key: &str,
) -> Result<EgressPolicyRecord, OrchError> {
    let vm = vm_get(state, id)?;
    if vm.owner_key.as_deref() != Some(owner_key) {
        return Err(OrchError::Forbidden("VM belongs to another tenant".into()));
    }
    let policy = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .get_egress_policy(owner_key, id)
        .map_err(crate::api::store_err)?;
    Ok(policy.unwrap_or(EgressPolicyRecord {
        vm_id: id,
        owner_key: owner_key.to_string(),
        revision: 1,
        allowlist: Vec::new(),
        allow_existing: false,
        created_at: vm.created_at,
        updated_at: vm.updated_at,
    }))
}

pub async fn put_egress_policy_local(
    state: &AppState,
    id: Uuid,
    owner_key: &str,
    expected_revision: u64,
    allowlist: Vec<String>,
    allow_existing: bool,
) -> Result<EgressPolicyRecord, OrchError> {
    // Serialize with HTTP/PTY/SSH activation so a policy update cannot race a
    // hibernated VM's transition back to a newly provisioned TAP.
    let activation = activation_gate(state, id)?;
    let _activation = activation.lock().await;
    let vm = vm_get(state, id)?;
    if vm.owner_key.as_deref() != Some(owner_key) {
        return Err(OrchError::Forbidden("VM belongs to another tenant".into()));
    }
    if !LIVE_CONTROL_STATUSES.contains(&vm.status) && vm.status != VmStatus::Hibernated {
        return Err(OrchError::Conflict(format!(
            "cannot update egress for VM {id} while {}",
            vm.status.as_str()
        )));
    }
    let _operation = if vm.status == VmStatus::Hibernated {
        None
    } else {
        Some(state.supervisor.operation_gate(id)?.lock_owned().await)
    };
    let allowlist = crate::net::normalize_egress_allowlist(&allowlist)?;
    let now = Utc::now();
    if vm.status == VmStatus::Hibernated {
        if let Some(fleet) = state.fleet.as_ref() {
            let current = get_egress_policy_local(state, id, owner_key)?;
            if current.revision != expected_revision {
                return Err(OrchError::Conflict(format!(
                    "egress policy revision is {}, expected {expected_revision}",
                    current.revision
                )));
            }
            let replacement = EgressPolicyRecord {
                vm_id: id,
                owner_key: owner_key.to_string(),
                revision: expected_revision
                    .checked_add(1)
                    .ok_or_else(|| OrchError::Conflict("egress policy revision overflow".into()))?,
                allowlist: allowlist.clone(),
                allow_existing,
                created_at: current.created_at,
                updated_at: now,
            };
            if let Err(error) = fleet
                .update_hibernation_egress(owner_key, id, expected_revision, &replacement)
                .await
            {
                tracing::warn!(%id, tenant = owner_key, %error,
                    "durable hibernation egress update failed");
                return Err(OrchError::Conflict(format!(
                    "durable hibernation egress update: {error}"
                )));
            }
        }
    }
    let policy = state
        .store
        .lock()
        .map_err(|_| OrchError::Internal("store lock poisoned".into()))?
        .update_egress_policy(
            owner_key,
            id,
            expected_revision,
            &allowlist,
            allow_existing,
            now,
        )
        .map_err(|error| {
            tracing::error!(%id, tenant = owner_key, %error,
                "local hibernation egress persistence failed after fleet CAS");
            crate::api::store_err(error)
        })?;
    if vm.status != VmStatus::Hibernated {
        let sup = Arc::clone(&state.supervisor);
        let applied = allowlist.clone();
        tokio::task::spawn_blocking(move || sup.update_egress(id, applied, allow_existing))
            .await
            .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
    }
    Ok(policy)
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
    ensure_vm_status(state, id, "query live status for", LIVE_CONTROL_STATUSES)?;
    let gate = state.supervisor.operation_gate(id)?;
    let _operation = gate.lock_owned().await;
    ensure_vm_status(state, id, "query live status for", LIVE_CONTROL_STATUSES)?;
    let sup = Arc::clone(&state.supervisor);
    let status = tokio::task::spawn_blocking(move || sup.status_vm(id))
        .await
        .map_err(|e| OrchError::Internal(format!("join: {e}")))??;
    serde_json::to_value(status).map_err(|e| OrchError::Internal(format!("status encode: {e}")))
}

pub async fn set_balloon_local(
    state: &AppState,
    id: Uuid,
    target_mib: u64,
) -> Result<(u64, u64, u32, u32), OrchError> {
    let vm = ensure_vm_status(
        state,
        id,
        "set balloon for",
        &[VmStatus::Running, VmStatus::Paused],
    )?;
    if target_mib > vm.memory_mib {
        return Err(OrchError::BadRequest(format!(
            "balloon target {target_mib} MiB exceeds VM memory {} MiB",
            vm.memory_mib
        )));
    }
    let gate = state.supervisor.operation_gate(id)?;
    let _operation = gate.lock_owned().await;
    let vm = ensure_vm_status(
        state,
        id,
        "set balloon for",
        &[VmStatus::Running, VmStatus::Paused],
    )?;
    if target_mib > vm.memory_mib {
        return Err(OrchError::BadRequest(format!(
            "balloon target {target_mib} MiB exceeds VM memory {} MiB",
            vm.memory_mib
        )));
    }
    let supervisor = Arc::clone(&state.supervisor);
    tokio::task::spawn_blocking(move || supervisor.set_balloon_vm(id, target_mib))
        .await
        .map_err(|error| OrchError::Internal(format!("balloon join: {error}")))?
}

pub async fn balloon_local(state: &AppState, id: Uuid) -> Result<(u64, u64, u32, u32), OrchError> {
    ensure_vm_status(
        state,
        id,
        "get balloon for",
        &[VmStatus::Running, VmStatus::Paused],
    )?;
    let gate = state.supervisor.operation_gate(id)?;
    let _operation = gate.lock_owned().await;
    ensure_vm_status(
        state,
        id,
        "get balloon for",
        &[VmStatus::Running, VmStatus::Paused],
    )?;
    let supervisor = Arc::clone(&state.supervisor);
    tokio::task::spawn_blocking(move || supervisor.balloon_vm(id))
        .await
        .map_err(|error| OrchError::Internal(format!("balloon join: {error}")))?
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
    // Durable VM state outlives a scale-to-zero runtime. Preflight the state
    // before supervisor lookup, then validate again under the operation gate
    // to preserve serialization with concurrent live transitions.
    let observed = get_local(state, id)?;
    validate_live_transition(id, observed.status, new_status)?;
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
        .map_err(|e| OrchError::Internal(format!("join: {e}")))?
        .map_err(|error| {
            tracing::warn!(
                vm = %id,
                from = current.status.as_str(),
                to = new_status.as_str(),
                %error,
                "VMM lifecycle operation failed"
            );
            error
        })?;
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
    #[cfg(target_os = "linux")]
    use sha2::Digest as _;
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
        for invalid in [
            VmStatus::Creating,
            VmStatus::Hibernated,
            VmStatus::Stopped,
            VmStatus::Error,
        ] {
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
    fn artifact_backed_restore_requires_exact_ready_metadata() {
        let (state, _) = test_state_with_durable_writer();
        let now = Utc::now();
        let boot_dir = PathBuf::from(format!("target/artifact-boot-metadata-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&boot_dir).unwrap();
        let kernel_path = boot_dir.join("vmlinux");
        let rootfs_path = boot_dir.join("rootfs.ext4");
        std::fs::write(&kernel_path, b"exact test kernel").unwrap();
        std::fs::write(&rootfs_path, b"exact test rootfs").unwrap();
        let immutable_image_digest = format!("sha256:{}", "2".repeat(64));
        let agent_digest = format!("sha256:{}", "3".repeat(64));
        let kernel_digest = image::sha256_regular_file(&kernel_path).unwrap();
        let rootfs_digest = image::sha256_regular_file(&rootfs_path).unwrap();
        state
            .store
            .lock()
            .unwrap()
            .upsert_image(&tarit_store::ImageRecord {
                name: "artifact-test".into(),
                tag: "latest".into(),
                rootfs_path: rootfs_path.display().to_string(),
                created_at: now,
                size_bytes: std::fs::metadata(&rootfs_path).unwrap().len(),
                source_ref: format!("docker://example.invalid/test@{immutable_image_digest}"),
                source_digest: Some(immutable_image_digest.clone()),
                rootfs_digest: Some(rootfs_digest.clone()),
                agent_digest: Some(agent_digest.clone()),
                provenance_key_digest: None,
                provenance_verified_at: None,
                golden_snapshot_path: None,
            })
            .unwrap();
        let snapshot = tarit_store::SnapshotRecord {
            snapshot_id: Uuid::new_v4(),
            path: "/private/snapshot.ram".into(),
            overlay_path: None,
            host_id: state.config.host_id.clone(),
            owner_key: Some("tenant-a".into()),
            api_key_id: Some("key-a".into()),
            vm_id: Uuid::new_v4(),
            ephemeral_owner_vm_id: None,
            memory_mib: Some(256),
            vcpus: Some(1),
            kernel_path: Some(kernel_path.display().to_string()),
            rootfs_path: Some(rootfs_path.display().to_string()),
            rootfs_read_only: Some(true),
            cmdline: Some("console=ttyS0".into()),
            content_digest: Some("sha256:manifest".into()),
            size_bytes: Some(8192),
            created_at: now,
        };
        state
            .store
            .lock()
            .unwrap()
            .insert_snapshot(&snapshot)
            .unwrap();
        let runtime = test_runtime();
        assert!(runtime
            .block_on(verify_snapshot_artifact_ready(
                &state, &snapshot, false, false,
            ))
            .unwrap()
            .is_none());
        assert!(matches!(
            runtime.block_on(verify_snapshot_artifact_ready(
                &state, &snapshot, true, false,
            )),
            Err(OrchError::Unprocessable(_))
        ));

        let artifact = ArtifactRecord {
            artifact_id: snapshot.snapshot_id,
            owner_key: "tenant-a".into(),
            host_id: snapshot.host_id.clone(),
            storage_locator: snapshot.path.clone(),
            kind: ArtifactKind::VmSnapshot,
            status: ArtifactStatus::Available,
            content_digest: "sha256:manifest".into(),
            size_bytes: 8192,
            immutable_image_digest: immutable_image_digest.clone(),
            agent_digest: agent_digest.clone(),
            boot_manifest_digest: ArtifactBootMetadata {
                version: ArtifactBootMetadata::VERSION,
                kernel_digest,
                immutable_image_digest,
                rootfs_digest,
                agent_digest,
                memory_mib: snapshot.memory_mib.unwrap(),
                vcpus: snapshot.vcpus.unwrap(),
                cmdline: snapshot.cmdline.clone().unwrap(),
                rootfs_read_only: snapshot.rootfs_read_only.unwrap(),
            }
            .digest()
            .unwrap(),
            parent_artifact_id: None,
            source_vm_id: Some(snapshot.vm_id),
            creation_revision: 1,
            integrity_manifest_digest: "sha256:manifest".into(),
            chunk_size_bytes: 4096,
            chunk_count: 2,
            replication_state: ArtifactReplicationState::Ready,
            reference_count: 0,
            created_at: now,
            updated_at: now,
        };
        state
            .store
            .lock()
            .unwrap()
            .insert_artifact(&artifact)
            .unwrap();
        assert_eq!(
            runtime
                .block_on(verify_snapshot_artifact_ready(
                    &state, &snapshot, true, false,
                ))
                .unwrap(),
            Some(artifact.clone())
        );
        let mut tampered_boot = snapshot.clone();
        tampered_boot.cmdline = Some("console=ttyS0 init=/bin/sh".into());
        assert!(matches!(
            verify_artifact_boot_metadata(&state, &tampered_boot, &artifact),
            Err(OrchError::Unavailable(_))
        ));

        let replica = tarit_types::ArtifactReplicaRecord {
            artifact_id: artifact.artifact_id,
            owner_key: artifact.owner_key.clone(),
            host_id: "host-b".into(),
            failure_domain: "zone-b".into(),
            storage_locator: "/private/replica-b".into(),
            status: tarit_types::ArtifactReplicaStatus::Staging,
            content_digest: artifact.content_digest.clone(),
            size_bytes: artifact.size_bytes,
            integrity_manifest_digest: artifact.integrity_manifest_digest.clone(),
            verified_at: None,
            created_at: now,
            updated_at: Utc::now(),
        };
        state
            .store
            .lock()
            .unwrap()
            .upsert_artifact_replica(&replica, 2, 2)
            .unwrap();
        assert!(matches!(
            runtime.block_on(verify_snapshot_artifact_ready(
                &state, &snapshot, true, false,
            )),
            Err(OrchError::Unavailable(_))
        ));
        std::fs::remove_dir_all(boot_dir).unwrap();
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
    fn exec_against_suspended_vm_is_a_conflict() {
        let (state, _rx) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        state.vm_cache.write().unwrap().get_mut(&id).unwrap().status = VmStatus::Suspended;
        state
            .supervisor
            .install_test_control_runtime(id, PathBuf::from("unused.sock"));

        let error = test_runtime()
            .block_on(exec_local(&state, id, "true".into(), 100))
            .expect_err("exec against a suspended VM must be rejected");

        assert!(matches!(error, OrchError::Conflict(_)));
        assert!(error.to_string().contains("suspended"));
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
        assert!(
            matches!(
                requests.as_slice(),
                [
                    tarit_vmm_client::ApiRequest::Pause,
                    tarit_vmm_client::ApiRequest::Status,
                    tarit_vmm_client::ApiRequest::Resume,
                    tarit_vmm_client::ApiRequest::Exec { .. },
                    tarit_vmm_client::ApiRequest::Stop
                ]
            ),
            "unexpected VMM request sequence: {requests:?}"
        );
        assert!(!socket.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_live_snapshot_is_observed_and_fenced_paused() {
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
            .expect_err("a failed live snapshot must not leave an assumed-running record");
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
        assert!(
            matches!(
                requests.as_slice(),
                [
                    tarit_vmm_client::ApiRequest::Snapshot { live: true, .. },
                    tarit_vmm_client::ApiRequest::Status,
                    tarit_vmm_client::ApiRequest::Stop
                ]
            ),
            "unexpected VMM request sequence: {requests:?}"
        );
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

        let initial_requests = (0..2)
            .map(|index| {
                requests_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap_or_else(|error| panic!("missing initial VMM request {index}: {error}"))
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            initial_requests.as_slice(),
            [
                tarit_vmm_client::ApiRequest::Snapshot { live: true, .. },
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
        assert!(
            matches!(
                requests.as_slice(),
                [
                    tarit_vmm_client::ApiRequest::Status,
                    tarit_vmm_client::ApiRequest::Stop
                ]
            ),
            "unexpected VMM request sequence: {requests:?}"
        );
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
        let memory = b"immutable RAM image";
        let mut snapshot_bytes = Vec::new();
        snapshot_bytes.extend_from_slice(b"VMSN");
        snapshot_bytes.extend_from_slice(&1u16.to_le_bytes());
        snapshot_bytes.extend_from_slice(&0u16.to_le_bytes());
        snapshot_bytes.extend_from_slice(&0u64.to_le_bytes());
        snapshot_bytes.extend_from_slice(&0u32.to_le_bytes());
        snapshot_bytes.extend_from_slice(&(memory.len() as u64).to_le_bytes());
        snapshot_bytes.extend_from_slice(&0u32.to_le_bytes());
        snapshot_bytes.extend_from_slice(memory);
        scratch_file.write_all(&snapshot_bytes).unwrap();
        scratch_file.sync_all().unwrap();
        drop(scratch_file);
        let integrity_scratch = PathBuf::from(format!("{}.precomputed", scratch.display()));
        let manifest = tarit_proto::IntegrityManifest {
            chunk_size: tarit_proto::INTEGRITY_CHUNK_SIZE,
            artifacts: vec![
                tarit_proto::ArtifactIntegrity {
                    kind: tarit_proto::ArtifactKind::SnapshotMetadata,
                    len: 32,
                    chunk_hashes: vec![sha2::Sha256::digest(&snapshot_bytes[..32]).into()],
                },
                tarit_proto::ArtifactIntegrity {
                    kind: tarit_proto::ArtifactKind::Ram,
                    len: memory.len() as u64,
                    chunk_hashes: vec![sha2::Sha256::digest(memory).into()],
                },
            ],
        };
        let mut integrity_options = std::fs::OpenOptions::new();
        integrity_options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let mut integrity_file = integrity_options.open(&integrity_scratch).unwrap();
        integrity_file
            .write_all(&manifest.encode().unwrap())
            .unwrap();
        integrity_file.sync_all().unwrap();
        drop(integrity_file);

        let socket = PathBuf::from(format!(
            "/tmp/taritd-snapshot-order-{}-{id}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let (requests_tx, requests_rx) = std::sync::mpsc::channel();
        let server_scratch = scratch.display().to_string();
        let server_integrity_scratch = integrity_scratch.display().to_string();
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
                        overlay_path: None,
                        integrity_path: Some(server_integrity_scratch.clone()),
                        live_stats: Some(tarit_vmm_client::LiveSnapshotStats {
                            rounds: 2,
                            pages_copied: 1,
                            final_dirty_pages: 0,
                            elapsed_us: 1,
                            downtime_us: 1,
                            termination: tarit_vmm_client::LiveSnapshotTermination::Converged,
                        }),
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
        assert_eq!(std::fs::read(&durable).unwrap(), snapshot_bytes);
        assert!(!scratch.exists(), "released VMM scratch must be removed");
        assert!(
            !integrity_scratch.exists(),
            "consumed VMM integrity scratch must be removed"
        );

        state.supervisor.stop_vm(id).unwrap();
        server.join().unwrap();
        let requests = requests_rx.into_iter().collect::<Vec<_>>();
        assert!(
            matches!(
                requests.as_slice(),
                [
                    // A successful live Snapshot response means the VMM has
                    // already resumed. Only then may taritd hand off scratch.
                    tarit_vmm_client::ApiRequest::Snapshot { live: true, .. },
                    tarit_vmm_client::ApiRequest::ReleaseScratch { .. },
                    tarit_vmm_client::ApiRequest::ReleaseScratch { .. },
                    tarit_vmm_client::ApiRequest::Stop
                ]
            ),
            "unexpected VMM request sequence: {requests:?}"
        );
        std::fs::remove_file(&durable).unwrap();
        std::fs::remove_file(format!("{durable}.integrity")).unwrap();
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
            egress_allowlist: Vec::new(),
            egress_allow_existing: false,
            data_volumes: Vec::new(),
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
                assert_eq!(state.supervisor.warm_count(&warm_cfg), 0);
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
                volumes: Vec::new(),
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
                        volumes: Vec::new(),
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
            egress_allowlist: Vec::new(),
            egress_allow_existing: false,
            data_volumes: Vec::new(),
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
    fn terminal_record_drops_dead_runtime_ownership() {
        let (state, _) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        {
            let mut cache = state.vm_cache.write().unwrap();
            let record = cache.get_mut(&id).unwrap();
            record.runtime_layout = Some(tarit_types::VmRuntimeLayout {
                overlay_path: Some("/runtime/rootfs.cow".into()),
                jail_path: Some("/runtime/jail".into()),
                artifact_paths: vec!["/runtime/vmm.sock".into()],
            });
            record.socket_path = Some("/runtime/vmm.sock".into());
            record.pid = Some(4242);
        }

        let record = terminal_record(&state, id, VmStatus::Error).unwrap();

        assert_eq!(record.status, VmStatus::Error);
        assert!(record.runtime_layout.is_none());
        assert!(record.socket_path.is_none());
        assert!(record.pid.is_none());
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
            host_session_id: Uuid::nil(),
            vmm_bin: PathBuf::from("true"),
            kernel: PathBuf::from("kernel"),
            rootfs: PathBuf::from("rootfs"),
            socket_dir: PathBuf::from("target/taritd-ops-test/sockets"),
            db_path: PathBuf::from("target/taritd-ops-test/fleet.db"),
            net_state_path: PathBuf::from("target/taritd-ops-test/net-state.json"),
            images_dir: PathBuf::from("target/taritd-ops-test/images"),
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
                activation_gates: Arc::new(Mutex::new(HashMap::new())),
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
                rootfs_read_only: false,
                cmdline: "console=ttyS0".into(),
                runtime_layout: None,
                socket_path: None,
                pid: None,
                created_at: now,
                updated_at: now,
            },
        );
        id
    }

    #[test]
    fn stop_revalidates_a_terminal_record_after_waiting_for_another_delete() {
        let (state, _writes) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        state.vm_cache.write().unwrap().get_mut(&id).unwrap().status = VmStatus::Stopped;

        let error = test_runtime()
            .block_on(stop_local(&state, id))
            .expect_err("a completed concurrent delete must be terminal");
        assert!(matches!(error, OrchError::NotFound(_)));
    }

    #[test]
    fn hibernated_egress_policy_updates_without_a_live_network_and_uses_cas() {
        let (state, _) = test_state_with_durable_writer();
        let id = insert_running_vm(&state);
        {
            let mut cache = state.vm_cache.write().unwrap();
            cache.get_mut(&id).unwrap().status = VmStatus::Hibernated;
        }
        let runtime = test_runtime();
        let initial = get_egress_policy_local(&state, id, "test").unwrap();
        assert_eq!(initial.revision, 1);
        assert!(initial.allowlist.is_empty());

        let updated = runtime
            .block_on(put_egress_policy_local(
                &state,
                id,
                "test",
                1,
                vec!["10.1.2.3/8".into(), "1.1.1.1:443".into()],
                true,
            ))
            .unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.allowlist, vec!["1.1.1.1:443/tcp", "10.0.0.0/8"]);
        assert!(updated.allow_existing);
        assert!(matches!(
            runtime.block_on(put_egress_policy_local(
                &state,
                id,
                "test",
                1,
                vec![],
                false,
            )),
            Err(OrchError::Conflict(_))
        ));
        assert!(matches!(
            get_egress_policy_local(&state, id, "other-tenant"),
            Err(OrchError::Forbidden(_))
        ));
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
}
#[test]
fn legacy_snapshot_null_rootfs_mode_restores_read_only() {
    assert!(restored_rootfs_read_only(None));
    assert!(restored_rootfs_read_only(Some(true)));
    assert!(!restored_rootfs_read_only(Some(false)));
}
