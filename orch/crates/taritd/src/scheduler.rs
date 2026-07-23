//! VM placement: local-first on this host; spill to peers when cluster store has capacity.

use crate::config::Config;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tarit_types::OrchError;
use uuid::Uuid;

/// Resources reserved for one VM for its entire local lifecycle. Keeping the
/// shape next to the VM id makes release O(1), prevents callers from releasing
/// a different shape, and leaves one place to add disk/network/IO accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceShape {
    pub vcpus: u64,
    pub memory_mib: u64,
}

impl ResourceShape {
    pub fn new(vcpus: u8, memory_mib: u64) -> Self {
        Self {
            vcpus: u64::from(vcpus),
            memory_mib,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceUsage {
    pub vm_count: usize,
    pub vcpus: u64,
    pub memory_mib: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationError {
    AlreadyReserved,
    VmLimit,
    VcpuLimit,
    MemoryLimit,
    AccountingOverflow,
}

#[derive(Default)]
struct ResourceLedger {
    by_vm: HashMap<Uuid, ResourceShape>,
    used: ResourceUsage,
}

impl ResourceLedger {
    fn reserve(
        &mut self,
        id: Uuid,
        shape: ResourceShape,
        config: &Config,
        enforce_limits: bool,
    ) -> Result<(), ReservationError> {
        if self.by_vm.contains_key(&id) {
            return Err(ReservationError::AlreadyReserved);
        }
        let vm_count = self
            .used
            .vm_count
            .checked_add(1)
            .ok_or(ReservationError::AccountingOverflow)?;
        let vcpus = self
            .used
            .vcpus
            .checked_add(shape.vcpus)
            .ok_or(ReservationError::AccountingOverflow)?;
        let memory_mib = self
            .used
            .memory_mib
            .checked_add(shape.memory_mib)
            .ok_or(ReservationError::AccountingOverflow)?;
        if enforce_limits {
            if vm_count > config.max_vms {
                return Err(ReservationError::VmLimit);
            }
            if vcpus > config.max_vcpus {
                return Err(ReservationError::VcpuLimit);
            }
            if memory_mib > config.max_memory_mib {
                return Err(ReservationError::MemoryLimit);
            }
        }
        self.by_vm.insert(id, shape);
        self.used = ResourceUsage {
            vm_count,
            vcpus,
            memory_mib,
        };
        Ok(())
    }

    fn release(&mut self, id: Uuid) -> bool {
        let Some(shape) = self.by_vm.remove(&id) else {
            return false;
        };
        self.used.vm_count = self.used.vm_count.saturating_sub(1);
        self.used.vcpus = self.used.vcpus.saturating_sub(shape.vcpus);
        self.used.memory_mib = self.used.memory_mib.saturating_sub(shape.memory_mib);
        true
    }
}

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
    ledger: Mutex<ResourceLedger>,
}

impl Scheduler {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            ledger: Mutex::new(ResourceLedger::default()),
        }
    }

    /// Atomically reserve the full resource shape for a new local VM.
    pub fn try_reserve(&self, id: Uuid, shape: ResourceShape) -> Result<(), ReservationError> {
        self.ledger
            .lock()
            .map_err(|_| ReservationError::AccountingOverflow)?
            .reserve(id, shape, &self.config, true)
    }

    /// Account a VM that survived an orchestrator restart. Existing work is
    /// adopted even if a new, lower configured limit is already exceeded; its
    /// usage then correctly blocks all further admission.
    pub fn reserve_existing(&self, id: Uuid, shape: ResourceShape) -> Result<(), ReservationError> {
        self.ledger
            .lock()
            .map_err(|_| ReservationError::AccountingOverflow)?
            .reserve(id, shape, &self.config, false)
    }

    /// Release exactly the shape associated with `id`. Duplicate releases are
    /// harmless and return false, which makes terminal compensation idempotent.
    pub fn release(&self, id: Uuid) -> bool {
        self.ledger
            .lock()
            .map(|mut ledger| ledger.release(id))
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub fn is_reserved(&self, id: Uuid) -> bool {
        self.ledger
            .lock()
            .map(|ledger| ledger.by_vm.contains_key(&id))
            .unwrap_or(true)
    }

    pub fn usage(&self) -> ResourceUsage {
        self.ledger
            .lock()
            .map(|ledger| ledger.used)
            .unwrap_or(ResourceUsage {
                vm_count: self.config.max_vms,
                vcpus: self.config.max_vcpus,
                memory_mib: self.config.max_memory_mib,
            })
    }

    /// Return actual remaining host capacity. Request-shape arguments remain in
    /// the interface for placement call-site compatibility; unlike the old
    /// implementation, they do not distort usage accounting.
    pub fn local_capacity(&self, _req_vcpus: u8, _req_mem_mib: u64) -> HostCapacity {
        let used = self.usage();
        let at_vm_limit = used.vm_count >= self.config.max_vms;
        HostCapacity {
            host_id: self.config.host_id.clone(),
            sandbox_count: used.vm_count,
            free_vcpus: if at_vm_limit {
                0
            } else {
                self.config.max_vcpus.saturating_sub(used.vcpus)
            },
            free_memory_mib: if at_vm_limit {
                0
            } else {
                self.config.max_memory_mib.saturating_sub(used.memory_mib)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiKeyRegistry, ApiRole, AutoscaleConfig, WarmPoolConfig};
    use std::path::PathBuf;

    fn config(max_vms: usize, max_vcpus: u64, max_memory_mib: u64) -> Config {
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            api_keys: ApiKeyRegistry::from_plaintext_entries(vec![(
                "test-key".into(),
                "test".into(),
                ApiRole::Admin,
                0,
            )])
            .unwrap(),
            host_id: "host-a".into(),
            vmm_bin: PathBuf::from("vmm"),
            kernel: PathBuf::from("vmlinux"),
            rootfs: PathBuf::from("rootfs.ext4"),
            socket_dir: PathBuf::from("/tmp/tarit-scheduler-test"),
            db_path: PathBuf::from("test.db"),
            net_state_path: PathBuf::from("net-state.json"),
            images_dir: PathBuf::from("images"),
            max_vms,
            max_vcpus,
            max_memory_mib,
            peer_secret: "peer-secret".into(),
            database_url: None,
            rpc_addr: "http://127.0.0.1:0".into(),
            allow_insecure_peer_http: true,
            enable_net: false,
            rootfs_read_only: true,
            metrics_expose_tenant_labels: false,
            api_max_in_flight: 128,
            api_requests_per_second: 10_000,
            api_request_timeout_ms: 5_000,
            api_max_body_bytes: 1024 * 1024,
            vm_cgroup_parent: None,
            vm_cgroup_pids_max: 128,
            warm_pool: WarmPoolConfig::default(),
            admission_timeout_ms: 1,
            reap_on_shutdown: true,
            region: "local".into(),
            zone: "local".into(),
            cloud: "onprem".into(),
            autoscale: AutoscaleConfig::default(),
            ssh_gateway_enabled: false,
            ssh_gateway_addr: "127.0.0.1:0".parse().unwrap(),
            ssh_gateway_host_key_path: PathBuf::from("ssh-host"),
            share_listen: None,
            share_domain: None,
            share_token_key: None,
            share_token_ttl_secs: 300,
            share_connect_timeout_ms: 1_000,
            share_idle_timeout_secs: 1,
        }
    }

    #[test]
    fn reserves_actual_shapes_and_reports_shape_independent_capacity() {
        let scheduler = Scheduler::new(config(4, 8, 4096));
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        scheduler
            .try_reserve(a, ResourceShape::new(2, 768))
            .unwrap();
        scheduler
            .try_reserve(b, ResourceShape::new(3, 1024))
            .unwrap();

        let small = scheduler.local_capacity(1, 1);
        let large = scheduler.local_capacity(8, 8192);
        assert_eq!(small.sandbox_count, 2);
        assert_eq!(small.free_vcpus, 3);
        assert_eq!(small.free_memory_mib, 2304);
        assert_eq!(small.free_vcpus, large.free_vcpus);
        assert_eq!(small.free_memory_mib, large.free_memory_mib);
    }

    #[test]
    fn enforces_each_limit_without_mutating_ledger() {
        let scheduler = Scheduler::new(config(2, 3, 1024));
        let first = Uuid::new_v4();
        scheduler
            .try_reserve(first, ResourceShape::new(2, 512))
            .unwrap();
        assert_eq!(
            scheduler.try_reserve(Uuid::new_v4(), ResourceShape::new(2, 1)),
            Err(ReservationError::VcpuLimit)
        );
        assert_eq!(
            scheduler.try_reserve(Uuid::new_v4(), ResourceShape::new(1, 513)),
            Err(ReservationError::MemoryLimit)
        );
        scheduler
            .try_reserve(Uuid::new_v4(), ResourceShape::new(1, 512))
            .unwrap();
        assert_eq!(
            scheduler.try_reserve(Uuid::new_v4(), ResourceShape::new(0, 0)),
            Err(ReservationError::VmLimit)
        );
        assert_eq!(scheduler.usage().vm_count, 2);
    }

    #[test]
    fn duplicate_and_repeated_release_cannot_corrupt_totals() {
        let scheduler = Scheduler::new(config(2, 4, 1024));
        let id = Uuid::new_v4();
        let shape = ResourceShape::new(2, 512);
        scheduler.try_reserve(id, shape).unwrap();
        assert_eq!(
            scheduler.try_reserve(id, shape),
            Err(ReservationError::AlreadyReserved)
        );
        assert!(scheduler.release(id));
        assert!(!scheduler.release(id));
        assert_eq!(scheduler.usage(), ResourceUsage::default());
    }

    #[test]
    fn adopted_usage_blocks_new_admission_even_above_new_limits() {
        let scheduler = Scheduler::new(config(1, 1, 256));
        scheduler
            .reserve_existing(Uuid::new_v4(), ResourceShape::new(2, 512))
            .unwrap();
        let capacity = scheduler.local_capacity(1, 256);
        assert_eq!(capacity.free_vcpus, 0);
        assert_eq!(capacity.free_memory_mib, 0);
        assert_eq!(
            scheduler.try_reserve(Uuid::new_v4(), ResourceShape::new(1, 1)),
            Err(ReservationError::VmLimit)
        );
    }
}
