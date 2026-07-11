//! Cluster routing + placement.
//!
//! In cluster mode (a Postgres fleet is configured) the fleet map is the
//! authoritative source of VM ownership. This module resolves which node owns a
//! VM (so any node can forward a request to the owner), picks a peer for
//! cross-node placement when the local host is at capacity, and keeps the
//! ownership map in sync. Single-host mode degrades to the local store.

use std::time::Duration;
use uuid::Uuid;

use crate::api::AppState;
use tarit_types::{OrchError, VmRecord};

#[cfg(test)]
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

/// A host is a placement/routing candidate only if its heartbeat is fresher
/// than this. 3x the 5s heartbeat interval tolerates a missed beat without
/// flapping a node out of the cluster.
const HOST_STALE_AFTER: Duration = Duration::from_secs(15);

/// Where a VM lives relative to this node.
pub enum Owner {
    Local,
    /// The owning peer's advertised RPC base URL (from the fleet registry).
    Remote(String),
}

#[cfg(test)]
static TEST_AUTHORITATIVE_OWNERS: OnceLock<Mutex<HashMap<(String, Uuid), String>>> =
    OnceLock::new();

#[cfg(test)]
pub(crate) fn set_test_authoritative_owner(host_id: &str, id: Uuid, rpc_addr: String) {
    TEST_AUTHORITATIVE_OWNERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("test authoritative owner lock")
        .insert((host_id.to_string(), id), rpc_addr);
}

/// Resolve the owner of `id`. In cluster mode the fleet ownership map is
/// authoritative and unavailable authority fails closed. Single-host mode
/// falls back to the local store so the node still answers for VMs it owns.
pub async fn resolve_owner(state: &AppState, id: Uuid) -> Result<Owner, OrchError> {
    if let Some(fleet) = &state.fleet {
        let host_id = fleet
            .get_vm_host(id)
            .await
            .map_err(|error| OrchError::Internal(format!("fleet get_vm_host: {error}")))?
            .ok_or_else(|| OrchError::NotFound(format!("vm {id} not found in fleet")))?;
        if host_id == state.config.host_id {
            return Ok(Owner::Local);
        }
        let host = fleet
            .get_host(&host_id)
            .await
            .map_err(|error| OrchError::Internal(format!("fleet get_host: {error}")))?
            .ok_or_else(|| {
                OrchError::Internal(format!("owner host {host_id} not registered in fleet"))
            })?;
        return host
            .rpc_addr
            .map(Owner::Remote)
            .ok_or_else(|| OrchError::Internal(format!("owner host {host_id} has no rpc_addr")));
    }
    #[cfg(test)]
    if let Some(rpc_addr) = TEST_AUTHORITATIVE_OWNERS.get().and_then(|owners| {
        owners
            .lock()
            .ok()?
            .get(&(state.config.host_id.clone(), id))
            .cloned()
    }) {
        return Ok(Owner::Remote(rpc_addr));
    }

    // Single-node fast path: if the VM is running (or paused) on THIS node it
    // is ours, so skip the mutex-guarded SQLite read.
    if state.supervisor.is_running(id) {
        return Ok(Owner::Local);
    }
    let exists = state
        .vm_cache
        .read()
        .map(|c| c.contains_key(&id))
        .unwrap_or(false);
    if exists {
        Ok(Owner::Local)
    } else {
        Err(OrchError::NotFound(format!("vm {id} not found in cluster")))
    }
}

/// All peers (best-first) that could place a VM of the given shape right now:
/// healthy, fresh heartbeat, advertising free capacity. Least-loaded first
/// (spread), ties broken by most free memory. The caller tries them in order,
/// so placement only fails when NO node in the cluster can take the VM.
pub async fn place_candidates(state: &AppState, vcpus: u8, mem_mib: u64) -> Vec<String> {
    let Some(fleet) = state.fleet.as_ref() else {
        return Vec::new();
    };
    let Ok(hosts) = fleet.list_hosts().await else {
        return Vec::new();
    };
    let now = chrono::Utc::now();

    let mut candidates: Vec<_> = hosts
        .into_iter()
        .filter(|h| {
            h.host_id != state.config.host_id
                && h.healthy
                && h.rpc_addr.is_some()
                && (now - h.last_heartbeat)
                    .to_std()
                    .map(|d| d < HOST_STALE_AFTER)
                    .unwrap_or(false)
                && h.free_vcpus >= vcpus as u64
                && h.free_memory_mib >= mem_mib
        })
        .collect();

    candidates.sort_by(|a, b| {
        a.sandbox_count
            .cmp(&b.sandbox_count)
            .then(b.free_memory_mib.cmp(&a.free_memory_mib))
    });
    candidates.into_iter().filter_map(|h| h.rpc_addr).collect()
}

/// Resolve a host_id to its advertised peer RPC address (for routing a restore
/// to the node holding the snapshot). `Ok(None)` when running single-host.
pub async fn peer_rpc(state: &AppState, host_id: &str) -> Result<Option<String>, OrchError> {
    let Some(fleet) = state.fleet.as_ref() else {
        return Ok(None);
    };
    match fleet.get_host(host_id).await {
        Ok(Some(h)) => Ok(h.rpc_addr),
        Ok(None) => Err(OrchError::NotFound(format!(
            "snapshot owner host {host_id} not registered in fleet"
        ))),
        Err(e) => Err(OrchError::Internal(format!("fleet get_host: {e}"))),
    }
}

/// Record (or update) VM ownership in the fleet map. Best-effort: a fleet write
/// failure logs but does not fail the operation — the owner's local store stays
/// authoritative and the next heartbeat/reconcile can repair the map.
pub async fn record_ownership(state: &AppState, vm: &VmRecord) {
    if let Some(fleet) = &state.fleet {
        if let Err(e) = fleet.upsert_vm(vm).await {
            tracing::warn!(id = %vm.id, "fleet upsert_vm failed: {e}");
        }
    }
}

/// Remove VM ownership from the fleet map on stop/delete so the cluster stops
/// routing to a dead sandbox.
pub async fn clear_ownership(state: &AppState, id: Uuid) {
    if let Some(fleet) = &state.fleet {
        if let Err(e) = fleet.delete_vm(id).await {
            tracing::warn!(%id, "fleet delete_vm failed: {e}");
        }
    }
}
