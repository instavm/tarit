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
use tarit_store::HostRecord;
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
const HOST_FUTURE_SKEW: chrono::Duration = chrono::Duration::seconds(5);

/// Where a VM lives relative to this node.
pub enum Owner {
    Local,
    /// The owning peer's identity and advertised RPC base URL (from the fleet
    /// registry). Both are retained so request authentication is bound to the
    /// intended host rather than only to an attacker-controlled URL.
    Remote(PeerTarget),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerTarget {
    pub host_id: String,
    pub rpc_addr: String,
}

impl std::fmt::Display for PeerTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}@{}", self.host_id, self.rpc_addr)
    }
}

#[cfg(test)]
static TEST_AUTHORITATIVE_OWNERS: OnceLock<Mutex<HashMap<(String, Uuid), PeerTarget>>> =
    OnceLock::new();

#[cfg(test)]
pub(crate) fn set_test_authoritative_owner(
    host_id: &str,
    id: Uuid,
    target_host_id: &str,
    rpc_addr: String,
) {
    TEST_AUTHORITATIVE_OWNERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("test authoritative owner lock")
        .insert(
            (host_id.to_string(), id),
            PeerTarget {
                host_id: target_host_id.to_string(),
                rpc_addr,
            },
        );
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
                OrchError::Unavailable(format!("owner host {host_id} is not registered"))
            })?;
        ensure_routable_host(&host, "VM owner")?;
        return host
            .rpc_addr
            .map(|rpc_addr| {
                Owner::Remote(PeerTarget {
                    host_id: host_id.clone(),
                    rpc_addr,
                })
            })
            .ok_or_else(|| {
                OrchError::Unavailable(format!("owner host {host_id} has no RPC address"))
            });
    }
    #[cfg(test)]
    if let Some(target) = TEST_AUTHORITATIVE_OWNERS.get().and_then(|owners| {
        owners
            .lock()
            .ok()?
            .get(&(state.config.host_id.clone(), id))
            .cloned()
    }) {
        return Ok(Owner::Remote(target));
    }

    // Single-node fast path: if the VM is running (or paused) on THIS node it
    // is ours, so skip the mutex-guarded SQLite read.
    if state.supervisor.is_running(id) {
        return Ok(Owner::Local);
    }
    let exists = state
        .vm_cache
        .read()
        .map(|cache| {
            cache
                .get(&id)
                .is_some_and(|vm| vm.status != tarit_types::VmStatus::Stopped)
        })
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
pub async fn place_candidates(state: &AppState, vcpus: u8, mem_mib: u64) -> Vec<PeerTarget> {
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
    candidates
        .into_iter()
        .filter_map(|host| {
            host.rpc_addr.map(|rpc_addr| PeerTarget {
                host_id: host.host_id,
                rpc_addr,
            })
        })
        .collect()
}

/// Resolve a host_id to its advertised peer RPC address (for routing a restore
/// to the node holding the snapshot). `Ok(None)` when running single-host.
pub async fn peer_rpc(state: &AppState, host_id: &str) -> Result<Option<PeerTarget>, OrchError> {
    let Some(fleet) = state.fleet.as_ref() else {
        return Ok(None);
    };
    match fleet.get_host(host_id).await {
        Ok(Some(h)) => {
            ensure_routable_host(&h, "snapshot owner")?;
            h.rpc_addr
                .map(|rpc_addr| {
                    Some(PeerTarget {
                        host_id: h.host_id,
                        rpc_addr,
                    })
                })
                .ok_or_else(|| {
                    OrchError::Unavailable(format!(
                        "snapshot owner host {host_id} has no RPC address"
                    ))
                })
        }
        Ok(None) => Err(OrchError::NotFound(format!(
            "snapshot owner host {host_id} not registered in fleet"
        ))),
        Err(e) => Err(OrchError::Internal(format!("fleet get_host: {e}"))),
    }
}

fn ensure_routable_host(host: &HostRecord, purpose: &str) -> Result<(), OrchError> {
    let age = chrono::Utc::now() - host.last_heartbeat;
    // Compare signed durations: converting a slightly-future (negative) age
    // through to_std() fails and would reject healthy owners on any skew.
    let stale_after = chrono::Duration::from_std(HOST_STALE_AFTER)
        .expect("stale-after constant fits chrono range");
    let fresh = age >= -HOST_FUTURE_SKEW && age < stale_after;
    if !host.healthy || !fresh {
        return Err(OrchError::Unavailable(format!(
            "{purpose} host {} is unhealthy or its heartbeat is stale",
            host.host_id
        )));
    }
    Ok(())
}

/// Commit user boot ownership while the boot/shutdown gate is held.
pub async fn record_ownership_required(state: &AppState, vm: &VmRecord) -> Result<(), OrchError> {
    if let Some(fleet) = &state.fleet {
        fleet
            .upsert_vm(vm)
            .await
            .map_err(|e| OrchError::Internal(format!("fleet upsert_vm: {e}")))?;
    }
    Ok(())
}

/// Remove VM ownership from the fleet map on stop/delete so the cluster stops
/// routing to a dead sandbox.
pub async fn clear_ownership(state: &AppState, id: Uuid) -> Result<(), OrchError> {
    if let Some(fleet) = &state.fleet {
        // A bare UUID is not a safe fencing token: the same UUID may have been
        // recreated after this lifecycle began. Use the resource incarnation
        // already persisted in the local record so a stale owner cannot delete a
        // newer fleet claim, even when both incarnations landed on this host.
        let record = state
            .vm_cache
            .read()
            .ok()
            .and_then(|cache| cache.get(&id).cloned())
            .or_else(|| state.store.lock().ok()?.get_vm(id).ok())
            .ok_or_else(|| {
                OrchError::Internal(format!(
                    "refusing unfenced fleet ownership delete for VM {id}: local record missing"
                ))
            })?;
        fleet
            .delete_vm(&record)
            .await
            .map_err(|e| OrchError::Internal(format!("fleet delete_vm: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(last_heartbeat: chrono::DateTime<chrono::Utc>, healthy: bool) -> HostRecord {
        HostRecord {
            host_id: "node-a".into(),
            rpc_addr: Some("https://node-a.internal".into()),
            sandbox_count: 0,
            free_vcpus: 8,
            free_memory_mib: 8_192,
            healthy,
            last_heartbeat,
        }
    }

    #[test]
    fn routing_rejects_stale_unhealthy_and_far_future_hosts() {
        assert!(ensure_routable_host(&host(chrono::Utc::now(), true), "owner").is_ok());
        assert!(matches!(
            ensure_routable_host(
                &host(chrono::Utc::now() - chrono::Duration::seconds(16), true),
                "owner",
            ),
            Err(OrchError::Unavailable(_))
        ));
        assert!(matches!(
            ensure_routable_host(&host(chrono::Utc::now(), false), "owner"),
            Err(OrchError::Unavailable(_))
        ));
        assert!(matches!(
            ensure_routable_host(
                &host(chrono::Utc::now() + chrono::Duration::seconds(6), true),
                "owner",
            ),
            Err(OrchError::Unavailable(_))
        ));
    }
}
