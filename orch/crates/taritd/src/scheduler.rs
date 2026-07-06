//! VM placement: local-first on this host; spill to peers when cluster store has capacity.

use crate::config::Config;
use std::sync::{Arc, Mutex};
use tarit_types::OrchError;

/// Advertised capacity for one orchestrator host.
#[derive(Debug, Clone)]
#[allow(dead_code)] // host_id/healthy are read by the multi-host placement path
pub struct HostCapacity {
    pub host_id: String,
    pub sandbox_count: usize,
    pub free_vcpus: u64,
    pub free_memory_mib: u64,
    pub healthy: bool,
}

/// Placement decision for a new sandbox.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Placement {
    pub host_id: String,
    pub local: bool,
}

pub struct Scheduler {
    config: Config,
    /// Local sandbox count (mirrors supervisor; updated on create/delete).
    local_count: Mutex<usize>,
}

impl Scheduler {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            local_count: Mutex::new(0),
        }
    }

    /// Register a successfully started local VM.
    #[allow(dead_code)] // superseded by try_reserve; kept for external callers/tests
    pub fn on_local_vm_started(&self) {
        if let Ok(mut n) = self.local_count.lock() {
            *n += 1;
        }
    }

    /// Atomically reserve a local VM slot if under the concurrency cap
    /// (`max_vms`). Returns false when full, so callers can wait for a slot to
    /// free (graceful degradation) instead of overshooting the cap. Pairs with
    /// `on_local_vm_stopped` to release. Prevents the check-then-spawn race
    /// where many concurrent creates all see room and blow past the limit.
    pub fn try_reserve(&self) -> bool {
        if let Ok(mut n) = self.local_count.lock() {
            if *n < self.config.max_vms {
                *n += 1;
                return true;
            }
        }
        false
    }

    /// Release a slot reserved via `try_reserve` that never became a live VM
    /// (spawn failed). Same effect as a stop.
    pub fn release(&self) {
        self.on_local_vm_stopped();
    }

    /// Register a stopped local VM.
    pub fn on_local_vm_stopped(&self) {
        if let Ok(mut n) = self.local_count.lock() {
            *n = n.saturating_sub(1);
        }
    }

    pub fn local_capacity(&self, req_vcpus: u8, req_mem_mib: u64) -> HostCapacity {
        let count = self.local_count.lock().map(|n| *n).unwrap_or(0);
        let at_vm_limit = count >= self.config.max_vms;
        HostCapacity {
            host_id: self.config.host_id.clone(),
            sandbox_count: count,
            free_vcpus: if at_vm_limit {
                0
            } else {
                self.config
                    .max_vcpus
                    .saturating_sub(count as u64 * req_vcpus as u64)
            },
            free_memory_mib: if at_vm_limit {
                0
            } else {
                self.config
                    .max_memory_mib
                    .saturating_sub(count as u64 * req_mem_mib)
            },
            healthy: true,
        }
    }

    /// Pick host for a new VM. Single-host: always local if capacity permits.
    /// Multi-host: prefer local when under fleet-average density; else least-loaded peer.
    #[allow(dead_code)] // multi-host placement; single-host admission uses try_reserve
    pub fn place(
        &self,
        req_vcpus: u8,
        req_mem_mib: u64,
        peers: &[HostCapacity],
    ) -> Result<Placement, OrchError> {
        let local = self.local_capacity(req_vcpus, req_mem_mib);

        if local.healthy
            && local.free_vcpus >= req_vcpus as u64
            && local.free_memory_mib >= req_mem_mib
        {
            let fleet_avg = fleet_average_density(peers, &local);
            if local.sandbox_count as f64 <= fleet_avg + 1.0 {
                return Ok(Placement {
                    host_id: local.host_id.clone(),
                    local: true,
                });
            }
        }

        // Try peers (multi-host); single-host list is empty → no capacity.
        let mut candidates: Vec<&HostCapacity> = peers
            .iter()
            .filter(|p| {
                p.healthy
                    && p.host_id != self.config.host_id
                    && p.free_vcpus >= req_vcpus as u64
                    && p.free_memory_mib >= req_mem_mib
            })
            .collect();
        candidates.sort_by_key(|p| p.sandbox_count);

        if let Some(peer) = candidates.first() {
            return Ok(Placement {
                host_id: peer.host_id.clone(),
                local: false,
            });
        }

        // Last resort: local if we still have raw capacity.
        if local.healthy
            && local.free_vcpus >= req_vcpus as u64
            && local.free_memory_mib >= req_mem_mib
        {
            return Ok(Placement {
                host_id: local.host_id.clone(),
                local: true,
            });
        }

        Err(OrchError::BadRequest(
            "no capacity on local host or peers".into(),
        ))
    }
}

#[allow(dead_code)] // used by the multi-host placement path (place)
fn fleet_average_density(peers: &[HostCapacity], local: &HostCapacity) -> f64 {
    let mut total = local.sandbox_count;
    let mut hosts = 1usize;
    for p in peers {
        if p.healthy {
            total += p.sandbox_count;
            hosts += 1;
        }
    }
    total as f64 / hosts as f64
}

/// Shared scheduler handle for API handlers.
#[allow(dead_code)]
pub type SharedScheduler = Arc<Scheduler>;
