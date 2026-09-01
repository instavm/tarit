//! Global control-plane store backed by PostgreSQL.
//!
//! Uses `tokio-postgres` + `deadpool-postgres` (both MIT OR Apache-2.0).

use chrono::{DateTime, Utc};
use deadpool_postgres::{Config as PoolConfig, Pool, Runtime};
use rustls::{ClientConfig, RootCertStore};
use tarit_store::{HostRecord, SnapshotRecord, VolumeTransition};
use tarit_types::{
    ArtifactKind, ArtifactObjectReplicaRecord, ArtifactRecord, ArtifactReplicaRecord,
    ArtifactReplicaStatus, ArtifactReplicationState, ArtifactStatus, AuditEvent, BranchRecord,
    EgressPolicyRecord, ExecutionRecord, ExecutionStatus, ForkOperationRecord, ForkOperationStatus,
    ShareRecord, ShareVisibility, UsageEvent, UsageSummary, VmRecord, VmStartupPath, VmStatus,
    VmVolumeAttachmentRecord, VolumeAttachmentMode, VolumeCapabilities, VolumeRecord, VolumeStatus,
    VolumeStorageClass,
};
use tokio_postgres_rustls::MakeRustlsConnect;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum FleetError {
    #[error("postgres: {0}")]
    Postgres(#[from] tokio_postgres::Error),
    #[error("pool: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),
    #[error("config: {0}")]
    Config(String),
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("tenant {owner_key} VM quota exceeded (max {max_vms})")]
    QuotaExceeded { owner_key: String, max_vms: usize },
    #[error("invalid fleet share row: {0}")]
    InvalidShareRow(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotLocation {
    pub snapshot_id: Uuid,
    pub host_id: String,
    pub owner_key: String,
    pub snapshot_path: String,
}

/// Durable control-plane binding used to recover the same logical hibernated
/// VM on a surviving host. Physical paths remain in the replica table and are
/// never stored here or exposed publicly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetHibernationRecord {
    pub vm_id: Uuid,
    pub owner_key: String,
    pub artifact_id: Uuid,
    pub egress_policy: EgressPolicyRecord,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct PostgresFleet {
    pool: Pool,
    min_artifact_replicas: u64,
    min_artifact_failure_domains: u64,
}

#[derive(Debug, Clone)]
pub struct FleetExecutionRecord {
    pub record: ExecutionRecord,
    pub owner_key: String,
    pub api_key_id: String,
    pub host_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkOperationClaimOutcome {
    New,
    Resumed,
    InProgress,
    Committed,
}

impl PostgresFleet {
    pub async fn connect(database_url: &str) -> Result<Self, FleetError> {
        let production = std::env::var("TARIT_PRODUCTION").ok().is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        });
        let default_minimum = if production { 2 } else { 1 };
        let min_replicas = env_positive_u64("TARIT_ARTIFACT_MIN_REPLICAS", default_minimum)?;
        let min_failure_domains =
            env_positive_u64("TARIT_ARTIFACT_MIN_FAILURE_DOMAINS", default_minimum)?;
        Self::connect_with_replication_policy(database_url, min_replicas, min_failure_domains).await
    }

    pub async fn connect_with_replication_policy(
        database_url: &str,
        min_artifact_replicas: u64,
        min_artifact_failure_domains: u64,
    ) -> Result<Self, FleetError> {
        if min_artifact_replicas == 0
            || min_artifact_failure_domains == 0
            || min_artifact_failure_domains > min_artifact_replicas
        {
            return Err(FleetError::Config(
                "artifact replication minima must be positive and failure domains cannot exceed replicas"
                    .into(),
            ));
        }
        let mut cfg = PoolConfig::new();
        cfg.url = Some(database_url.to_string());
        let tls = make_rustls_connector()?;
        let pool = cfg
            .create_pool(Some(Runtime::Tokio1), tls)
            .map_err(|e| FleetError::Config(e.to_string()))?;
        let mut client = pool.get().await?;
        let tx = client.transaction().await?;
        // Every node may start concurrently. The transaction-scoped advisory
        // lock serializes schema migration without leaving a session lock behind
        // when startup fails.
        tx.query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended('tarit-fleet-schema', 0))",
            &[],
        )
        .await?;
        tx.batch_execute(FLEET_SCHEMA).await?;
        tx.batch_execute(
            "ALTER TABLE fleet_vms ADD COLUMN IF NOT EXISTS owner_key TEXT;
             ALTER TABLE fleet_vms ADD COLUMN IF NOT EXISTS api_key_id TEXT;
             ALTER TABLE fleet_vms ADD COLUMN IF NOT EXISTS revision BIGINT NOT NULL DEFAULT 1;
             ALTER TABLE fleet_vms ADD COLUMN IF NOT EXISTS startup_path TEXT;
             ALTER TABLE fleet_vms ADD COLUMN IF NOT EXISTS rootfs_read_only BOOLEAN NOT NULL DEFAULT FALSE;
             ALTER TABLE fleet_vms ADD COLUMN IF NOT EXISTS generation BIGINT NOT NULL DEFAULT 1;
             ALTER TABLE fleet_vm_fork_operations ADD COLUMN IF NOT EXISTS target_boot_session_id UUID;
             CREATE INDEX IF NOT EXISTS fleet_vms_owner_status ON fleet_vms (owner_key, status);
             CREATE TABLE IF NOT EXISTS fleet_schema_migrations (
               version BIGINT PRIMARY KEY,
               applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
             );
             INSERT INTO fleet_schema_migrations (version) VALUES (1)
               ON CONFLICT (version) DO NOTHING;",
        )
        .await?;
        tx.execute(
            "WITH health AS (
               SELECT artifact.artifact_id, artifact.owner_key, artifact.status,
                      COUNT(replica.artifact_id) FILTER (
                        WHERE replica.status = 'available'
                          AND replica.verified_at IS NOT NULL
                          AND host.healthy = TRUE
                          AND host.last_heartbeat >= NOW() - INTERVAL '15 seconds'
                      ) AS available,
                      COUNT(DISTINCT replica.failure_domain) FILTER (
                        WHERE replica.status = 'available'
                          AND replica.verified_at IS NOT NULL
                          AND host.healthy = TRUE
                          AND host.last_heartbeat >= NOW() - INTERVAL '15 seconds'
                      ) AS failure_domains
                 FROM fleet_artifacts artifact
                 LEFT JOIN fleet_artifact_replicas replica
                   ON replica.artifact_id = artifact.artifact_id
                  AND replica.owner_key = artifact.owner_key
                 LEFT JOIN fleet_hosts host ON host.host_id = replica.host_id
                GROUP BY artifact.artifact_id, artifact.owner_key, artifact.status
             )
             UPDATE fleet_artifacts artifact
                SET replication_state = CASE
                  WHEN health.status <> 'available' OR health.available = 0 THEN 'pending'
                  WHEN health.available >= $1 AND health.failure_domains >= $2 THEN 'ready'
                  ELSE 'degraded'
                END
               FROM health
              WHERE artifact.artifact_id = health.artifact_id
                AND artifact.owner_key = health.owner_key",
            &[
                &u64_to_sql_i64(min_artifact_replicas)?,
                &u64_to_sql_i64(min_artifact_failure_domains)?,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(Self {
            pool,
            min_artifact_replicas,
            min_artifact_failure_domains,
        })
    }

    pub async fn upsert_host(&self, host: &HostRecord) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO fleet_hosts (host_id, boot_session_id, peer_certificate_sha256, rpc_addr, sandbox_count, free_vcpus, free_memory_mib, healthy, last_heartbeat)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                 ON CONFLICT (host_id) DO UPDATE SET
                   boot_session_id = EXCLUDED.boot_session_id,
                   peer_certificate_sha256 = EXCLUDED.peer_certificate_sha256,
                   rpc_addr = EXCLUDED.rpc_addr,
                   sandbox_count = EXCLUDED.sandbox_count,
                   free_vcpus = EXCLUDED.free_vcpus,
                   free_memory_mib = EXCLUDED.free_memory_mib,
                   healthy = EXCLUDED.healthy,
                   last_heartbeat = EXCLUDED.last_heartbeat",
                &[
                    &host.host_id,
                    &host.boot_session_id,
                    &host.peer_certificate_sha256,
                    &host.rpc_addr,
                    &(host.sandbox_count as i64),
                    &(host.free_vcpus as i64),
                    &(host.free_memory_mib as i64),
                    &host.healthy,
                    &host.last_heartbeat,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn list_hosts(&self) -> Result<Vec<HostRecord>, FleetError> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT host_id, boot_session_id, peer_certificate_sha256, rpc_addr, sandbox_count, free_vcpus, free_memory_mib, healthy, last_heartbeat
                 FROM fleet_hosts ORDER BY host_id",
                &[],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| HostRecord {
                host_id: row.get(0),
                boot_session_id: row.get(1),
                peer_certificate_sha256: row.get(2),
                rpc_addr: row.get(3),
                sandbox_count: row.get::<_, i64>(4) as usize,
                free_vcpus: row.get::<_, i64>(5) as u64,
                free_memory_mib: row.get::<_, i64>(6) as u64,
                healthy: row.get(7),
                last_heartbeat: row.get(8),
            })
            .collect())
    }

    /// Atomically claim a VM id or update the same resource incarnation.
    ///
    /// `(host_id, created_at)` is the fencing identity already carried by every
    /// `VmRecord`; `generation` is advanced for each accepted transition. A
    /// different host or a reused UUID with a different creation timestamp can
    /// never steal the row. Older lifecycle writes also cannot regress state.
    pub async fn upsert_vm(&self, vm: &VmRecord) -> Result<u64, FleetError> {
        // PostgreSQL TIMESTAMPTZ and the postgres wire protocol preserve
        // microseconds, while chrono can carry nanoseconds. Compare retries
        // against the same representation that PostgreSQL persists so an
        // identical retry cannot become a false same-revision conflict.
        let vm = normalize_vm_timestamps_for_postgres(vm);
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let row = tx
            .query_opt(
                "INSERT INTO fleet_vms (
                   id, host_id, owner_key, api_key_id, status, revision, startup_path, memory_mib, vcpus,
                   kernel_path, rootfs_path, rootfs_read_only, cmdline, created_at, updated_at,
                   generation
                 ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,1)
                 ON CONFLICT (id) DO UPDATE SET
                   owner_key = EXCLUDED.owner_key,
                   api_key_id = EXCLUDED.api_key_id,
                   status = EXCLUDED.status,
                   revision = EXCLUDED.revision,
                   startup_path = EXCLUDED.startup_path,
                   memory_mib = EXCLUDED.memory_mib,
                   vcpus = EXCLUDED.vcpus,
                   kernel_path = EXCLUDED.kernel_path,
                   rootfs_path = EXCLUDED.rootfs_path,
                   rootfs_read_only = EXCLUDED.rootfs_read_only,
                   cmdline = EXCLUDED.cmdline,
                   updated_at = EXCLUDED.updated_at,
                   generation = fleet_vms.generation + 1
                 WHERE fleet_vms.host_id = EXCLUDED.host_id
                   AND fleet_vms.created_at = EXCLUDED.created_at
                   AND fleet_vms.revision < EXCLUDED.revision
                 RETURNING generation",
                &[
                    &vm.id,
                    &vm.host_id,
                    &vm.owner_key,
                    &vm.api_key_id,
                    &vm.status.as_str(),
                    &i64::try_from(vm.revision)
                        .map_err(|_| FleetError::Config("VM revision exceeds PostgreSQL BIGINT".into()))?,
                    &vm.startup_path.map(VmStartupPath::as_str),
                    &(vm.memory_mib as i64),
                    &(vm.vcpus as i16),
                    &vm.kernel_path,
                    &vm.rootfs_path,
                    &vm.rootfs_read_only,
                    &vm.cmdline,
                    &vm.created_at,
                    &vm.updated_at,
                ],
            )
            .await?;
        let row = match row {
            Some(row) => row,
            None => {
                let existing = tx
                    .query_opt(
                        "SELECT id, host_id, owner_key, api_key_id, status, revision, startup_path,
                                memory_mib, vcpus, kernel_path, rootfs_path, rootfs_read_only,
                                cmdline, created_at, updated_at, generation
                           FROM fleet_vms WHERE id = $1",
                        &[&vm.id],
                    )
                    .await?
                    .ok_or(FleetError::NotFound)?;
                let existing_vm = row_to_vm(&existing)?;
                if existing_vm.host_id != vm.host_id || existing_vm.created_at != vm.created_at {
                    return Err(FleetError::Conflict(format!(
                        "VM {} is owned by another resource incarnation",
                        vm.id
                    )));
                }
                let mut expected = vm.clone();
                expected.runtime_layout = None;
                expected.socket_path = None;
                expected.pid = None;
                if existing_vm.revision == vm.revision && existing_vm != expected {
                    return Err(FleetError::Conflict(format!(
                        "VM {} has two different records at revision {}",
                        vm.id, vm.revision
                    )));
                }
                tx.commit().await?;
                return u64::try_from(existing.get::<_, i64>(15))
                    .map_err(|_| FleetError::Config("negative VM generation".into()));
            }
        };
        if let Some(owner_key) = vm.owner_key.as_deref() {
            tx.execute(
                "DELETE FROM tenant_vm_reservations WHERE id = $1 AND owner_key = $2",
                &[&vm.id, &owner_key],
            )
            .await?;
        }
        let generation = u64::try_from(row.get::<_, i64>(0))
            .map_err(|_| FleetError::Config("negative VM generation".into()))?;
        tx.commit().await?;
        Ok(generation)
    }

    pub async fn list_vms(&self, owner_key: Option<&str>) -> Result<Vec<VmRecord>, FleetError> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT id, host_id, owner_key, api_key_id, status, revision, startup_path,
                        memory_mib, vcpus, kernel_path, rootfs_path, rootfs_read_only, cmdline,
                        created_at, updated_at
                   FROM fleet_vms
                 WHERE ($1::TEXT IS NULL OR owner_key = $1)
                   AND status <> 'stopped'
                  ORDER BY created_at DESC",
                &[&owner_key],
            )
            .await?;
        rows.iter().map(row_to_vm).collect()
    }

    pub async fn get_vm(&self, id: Uuid) -> Result<VmRecord, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT id, host_id, owner_key, api_key_id, status, revision, startup_path,
                        memory_mib, vcpus, kernel_path, rootfs_path, rootfs_read_only, cmdline,
                        created_at, updated_at
                   FROM fleet_vms WHERE id = $1",
                &[&id],
            )
            .await?
            .ok_or(FleetError::NotFound)?;
        row_to_vm(&row)
    }

    /// Move ownership of a hibernated VM only after its prior owner has become
    /// stale. The target's current boot session is checked in the same locked
    /// transaction, fencing both a recovered old process and a restarted
    /// claimant. Running, paused, and suspended VMs can never enter this path.
    pub async fn claim_hibernated_vm(
        &self,
        owner_key: &str,
        id: Uuid,
        target_host_id: &str,
        target_boot_session_id: Uuid,
        stale_before: DateTime<Utc>,
    ) -> Result<VmRecord, FleetError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let row = tx
            .query_opt(
                "SELECT id, host_id, owner_key, api_key_id, status, revision, startup_path,
                        memory_mib, vcpus, kernel_path, rootfs_path, rootfs_read_only, cmdline,
                        created_at, updated_at
                   FROM fleet_vms WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?
            .ok_or(FleetError::NotFound)?;
        let mut vm = row_to_vm(&row)?;
        if vm.owner_key.as_deref() != Some(owner_key) {
            return Err(FleetError::NotFound);
        }
        if vm.status != VmStatus::Hibernated {
            return Err(FleetError::Conflict(format!("VM {id} is not hibernated")));
        }
        let target_is_current = tx
            .query_opt(
                "SELECT 1 FROM fleet_hosts
                  WHERE host_id = $1 AND boot_session_id = $2
                    AND healthy = TRUE AND last_heartbeat >= $3",
                &[&target_host_id, &target_boot_session_id, &stale_before],
            )
            .await?
            .is_some();
        if !target_is_current {
            return Err(FleetError::Conflict(
                "target host boot session is not healthy and current".into(),
            ));
        }
        if vm.host_id != target_host_id {
            let prior_still_live = tx
                .query_opt(
                    "SELECT 1 FROM fleet_hosts
                      WHERE host_id = $1 AND healthy = TRUE AND last_heartbeat >= $2",
                    &[&vm.host_id, &stale_before],
                )
                .await?
                .is_some();
            if prior_still_live {
                return Err(FleetError::Conflict(
                    "hibernated VM owner is still healthy".into(),
                ));
            }
            vm.host_id = target_host_id.to_string();
            vm.revision = vm
                .revision
                .checked_add(1)
                .ok_or_else(|| FleetError::Conflict("VM revision overflow".into()))?;
            vm.updated_at = Utc::now();
            let changed = tx
                .execute(
                    "UPDATE fleet_vms
                        SET host_id = $2, revision = $3, updated_at = $4,
                            generation = generation + 1
                      WHERE id = $1 AND status = 'hibernated'",
                    &[
                        &id,
                        &target_host_id,
                        &u64_to_sql_i64(vm.revision)?,
                        &vm.updated_at,
                    ],
                )
                .await?;
            if changed != 1 {
                return Err(FleetError::Conflict(
                    "hibernated VM ownership changed".into(),
                ));
            }
        }
        tx.commit().await?;
        Ok(vm)
    }

    pub async fn count_active_vms_for_owner(&self, owner_key: &str) -> Result<usize, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_one(
                "SELECT COUNT(*) FROM fleet_vms
                 WHERE owner_key = $1 AND status IN ('creating', 'running', 'paused', 'suspended')",
                &[&owner_key],
            )
            .await?;
        Ok(row.get::<_, i64>(0) as usize)
    }

    pub async fn get_vm_host(&self, id: Uuid) -> Result<Option<String>, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt("SELECT host_id FROM fleet_vms WHERE id = $1", &[&id])
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    /// Fetch a single host record (used to resolve an owner's peer RPC address).
    pub async fn get_host(&self, host_id: &str) -> Result<Option<HostRecord>, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT host_id, boot_session_id, peer_certificate_sha256, rpc_addr, sandbox_count, free_vcpus, free_memory_mib, healthy, last_heartbeat
                 FROM fleet_hosts WHERE host_id = $1",
                &[&host_id],
            )
            .await?;
        Ok(row.map(|row| HostRecord {
            host_id: row.get(0),
            boot_session_id: row.get(1),
            peer_certificate_sha256: row.get(2),
            rpc_addr: row.get(3),
            sandbox_count: row.get::<_, i64>(4) as usize,
            free_vcpus: row.get::<_, i64>(5) as u64,
            free_memory_mib: row.get::<_, i64>(6) as u64,
            healthy: row.get(7),
            last_heartbeat: row.get(8),
        }))
    }

    /// Publish the private locator for an opaque snapshot handle. This table is
    /// control-plane-only and must never be serialized through the public API.
    pub async fn upsert_snapshot(&self, snapshot: &SnapshotRecord) -> Result<(), FleetError> {
        let owner_key = snapshot
            .owner_key
            .as_deref()
            .ok_or_else(|| FleetError::Conflict("fleet snapshot requires a tenant owner".into()))?;
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO fleet_snapshots (snapshot_id, host_id, owner_key, snapshot_path, created_at)
                 VALUES ($1,$2,$3,$4,$5)
                 ON CONFLICT (snapshot_id) DO UPDATE SET
                   host_id = EXCLUDED.host_id,
                   owner_key = EXCLUDED.owner_key,
                   snapshot_path = EXCLUDED.snapshot_path,
                   created_at = EXCLUDED.created_at",
                &[
                    &snapshot.snapshot_id,
                    &snapshot.host_id,
                    &owner_key,
                    &snapshot.path,
                    &snapshot.created_at,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn get_snapshot_location(
        &self,
        snapshot_id: Uuid,
    ) -> Result<Option<SnapshotLocation>, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT snapshot_id, host_id, owner_key, snapshot_path
                 FROM fleet_snapshots WHERE snapshot_id = $1",
                &[&snapshot_id],
            )
            .await?;
        Ok(row.map(|row| SnapshotLocation {
            snapshot_id: row.get(0),
            host_id: row.get(1),
            owner_key: row.get(2),
            snapshot_path: row.get(3),
        }))
    }

    pub async fn delete_snapshot(&self, snapshot: &SnapshotRecord) -> Result<(), FleetError> {
        let owner_key = snapshot
            .owner_key
            .as_deref()
            .ok_or_else(|| FleetError::Conflict("fleet snapshot requires a tenant owner".into()))?;
        let client = self.pool.get().await?;
        client
            .execute(
                "DELETE FROM fleet_snapshots
                 WHERE snapshot_id = $1 AND owner_key = $2 AND host_id = $3 AND snapshot_path = $4",
                &[
                    &snapshot.snapshot_id,
                    &owner_key,
                    &snapshot.host_id,
                    &snapshot.path,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn delete_snapshot_by_id(
        &self,
        owner_key: &str,
        snapshot_id: Uuid,
    ) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        client
            .execute(
                "DELETE FROM fleet_snapshots WHERE snapshot_id = $1 AND owner_key = $2",
                &[&snapshot_id, &owner_key],
            )
            .await?;
        Ok(())
    }

    /// Bind a hibernated logical VM to the immutable artifact that can recover
    /// it. The artifact reference and binding move in one transaction so GC
    /// can never remove the only recovery image between those writes.
    pub async fn upsert_hibernation(
        &self,
        record: &FleetHibernationRecord,
    ) -> Result<(), FleetError> {
        if record.egress_policy.vm_id != record.vm_id
            || record.egress_policy.owner_key != record.owner_key
            || record.egress_policy.revision == 0
        {
            return Err(FleetError::Conflict(
                "hibernation egress policy identity or revision is invalid".into(),
            ));
        }
        let allowlist_json = serde_json::to_string(&record.egress_policy.allowlist)
            .map_err(|error| FleetError::Config(format!("encode hibernation egress: {error}")))?;
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let existing = tx
            .query_opt(
                "SELECT artifact_id FROM fleet_hibernations
                 WHERE vm_id = $1 AND owner_key = $2 FOR UPDATE",
                &[&record.vm_id, &record.owner_key],
            )
            .await?;
        let previous_artifact = existing.as_ref().map(|row| row.get::<_, Uuid>(0));
        if previous_artifact != Some(record.artifact_id) {
            let acquired = tx
                .execute(
                    "UPDATE fleet_artifacts
                        SET reference_count = reference_count + 1, updated_at = $3
                      WHERE artifact_id = $1 AND owner_key = $2
                        AND source_vm_id = $4
                        AND status = 'available' AND replication_state = 'ready'",
                    &[
                        &record.artifact_id,
                        &record.owner_key,
                        &record.updated_at,
                        &record.vm_id,
                    ],
                )
                .await?;
            if acquired != 1 {
                return Err(FleetError::NotFound);
            }
        }
        let changed = tx
            .execute(
                "INSERT INTO fleet_hibernations (
                   vm_id, owner_key, artifact_id, policy_revision, allowlist_json,
                   allow_existing, policy_created_at, policy_updated_at, created_at, updated_at
                 ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
                 ON CONFLICT (vm_id) DO UPDATE SET
                   artifact_id = EXCLUDED.artifact_id,
                   policy_revision = EXCLUDED.policy_revision,
                   allowlist_json = EXCLUDED.allowlist_json,
                   allow_existing = EXCLUDED.allow_existing,
                   policy_created_at = EXCLUDED.policy_created_at,
                   policy_updated_at = EXCLUDED.policy_updated_at,
                   updated_at = EXCLUDED.updated_at
                 WHERE fleet_hibernations.owner_key = EXCLUDED.owner_key",
                &[
                    &record.vm_id,
                    &record.owner_key,
                    &record.artifact_id,
                    &u64_to_sql_i64(record.egress_policy.revision)?,
                    &allowlist_json,
                    &record.egress_policy.allow_existing,
                    &record.egress_policy.created_at,
                    &record.egress_policy.updated_at,
                    &record.created_at,
                    &record.updated_at,
                ],
            )
            .await
            .map_err(fleet_error_from_postgres)?;
        if changed != 1 {
            return Err(FleetError::Conflict(
                "hibernation belongs to another tenant".into(),
            ));
        }
        if let Some(previous) = previous_artifact.filter(|id| *id != record.artifact_id) {
            let released = tx
                .execute(
                    "UPDATE fleet_artifacts
                        SET reference_count = reference_count - 1, updated_at = $3
                      WHERE artifact_id = $1 AND owner_key = $2 AND reference_count > 0",
                    &[&previous, &record.owner_key, &record.updated_at],
                )
                .await?;
            if released != 1 {
                return Err(FleetError::Conflict(
                    "prior hibernation artifact reference is missing".into(),
                ));
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_hibernation(
        &self,
        owner_key: &str,
        vm_id: Uuid,
    ) -> Result<FleetHibernationRecord, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT vm_id, owner_key, artifact_id, policy_revision, allowlist_json,
                        allow_existing, policy_created_at, policy_updated_at, created_at, updated_at
                   FROM fleet_hibernations WHERE vm_id = $1 AND owner_key = $2",
                &[&vm_id, &owner_key],
            )
            .await?
            .ok_or(FleetError::NotFound)?;
        row_to_hibernation(&row)
    }

    pub async fn update_hibernation_egress(
        &self,
        owner_key: &str,
        vm_id: Uuid,
        expected_revision: u64,
        policy: &EgressPolicyRecord,
    ) -> Result<(), FleetError> {
        if policy.vm_id != vm_id
            || policy.owner_key != owner_key
            || policy.revision
                != expected_revision
                    .checked_add(1)
                    .ok_or_else(|| FleetError::Conflict("egress policy revision overflow".into()))?
        {
            return Err(FleetError::Conflict(
                "replacement egress policy identity or revision is invalid".into(),
            ));
        }
        let allowlist_json = serde_json::to_string(&policy.allowlist)
            .map_err(|error| FleetError::Config(format!("encode hibernation egress: {error}")))?;
        let client = self.pool.get().await?;
        let changed = client
            .execute(
                "UPDATE fleet_hibernations
                    SET policy_revision = $4, allowlist_json = $5, allow_existing = $6,
                        policy_updated_at = $7, updated_at = $7
                  WHERE vm_id = $1 AND owner_key = $2 AND policy_revision = $3",
                &[
                    &vm_id,
                    &owner_key,
                    &u64_to_sql_i64(expected_revision)?,
                    &u64_to_sql_i64(policy.revision)?,
                    &allowlist_json,
                    &policy.allow_existing,
                    &policy.updated_at,
                ],
            )
            .await?;
        if changed != 1 {
            return Err(FleetError::Conflict(
                "hibernation egress revision changed".into(),
            ));
        }
        Ok(())
    }

    pub async fn delete_hibernation(&self, owner_key: &str, vm_id: Uuid) -> Result<(), FleetError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let row = tx
            .query_opt(
                "DELETE FROM fleet_hibernations
                  WHERE vm_id = $1 AND owner_key = $2
                  RETURNING artifact_id",
                &[&vm_id, &owner_key],
            )
            .await?
            .ok_or(FleetError::NotFound)?;
        let artifact_id: Uuid = row.get(0);
        let released = tx
            .execute(
                "UPDATE fleet_artifacts
                    SET reference_count = reference_count - 1, updated_at = NOW()
                  WHERE artifact_id = $1 AND owner_key = $2 AND reference_count > 0",
                &[&artifact_id, &owner_key],
            )
            .await?;
        if released != 1 {
            return Err(FleetError::Conflict(
                "hibernation artifact reference is missing".into(),
            ));
        }
        delete_unreferenced_artifact_chain_tx(&tx, owner_key, artifact_id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Acquire the artifact backing a running lazy restore before the VMM can
    /// fault from it. The VM creation timestamp fences UUID reuse while still
    /// allowing the same logical VM to be re-placed on another host.
    pub async fn acquire_vm_artifact_ref(
        &self,
        vm: &VmRecord,
        artifact_id: Uuid,
    ) -> Result<(), FleetError> {
        let vm = normalize_vm_timestamps_for_postgres(vm);
        let owner_key = vm.owner_key.as_deref().ok_or_else(|| {
            FleetError::Conflict("artifact-backed VM requires a tenant owner".into())
        })?;
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        if let Some(row) = tx
            .query_opt(
                "SELECT owner_key, artifact_id, vm_created_at
                   FROM fleet_vm_artifact_refs WHERE vm_id = $1 FOR UPDATE",
                &[&vm.id],
            )
            .await?
        {
            if row.get::<_, String>(0) == owner_key
                && row.get::<_, Uuid>(1) == artifact_id
                && row.get::<_, DateTime<Utc>>(2) == vm.created_at
            {
                tx.commit().await?;
                return Ok(());
            }
            return Err(FleetError::Conflict(
                "VM artifact reference belongs to another incarnation".into(),
            ));
        }
        let acquired = tx
            .execute(
                "UPDATE fleet_artifacts
                    SET reference_count = reference_count + 1, updated_at = NOW()
                  WHERE artifact_id = $1 AND owner_key = $2 AND status = 'available'",
                &[&artifact_id, &owner_key],
            )
            .await?;
        if acquired != 1 {
            return Err(FleetError::NotFound);
        }
        tx.execute(
            "INSERT INTO fleet_vm_artifact_refs
               (vm_id, owner_key, artifact_id, vm_created_at, created_at, updated_at)
             VALUES ($1,$2,$3,$4,NOW(),NOW())",
            &[&vm.id, &owner_key, &artifact_id, &vm.created_at],
        )
        .await
        .map_err(fleet_error_from_postgres)?;
        tx.commit().await?;
        Ok(())
    }

    /// Release an artifact-backed VM reference. Missing bindings are
    /// idempotent; a binding for a reused UUID is never released.
    pub async fn release_vm_artifact_ref(&self, vm: &VmRecord) -> Result<(), FleetError> {
        let vm = normalize_vm_timestamps_for_postgres(vm);
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        release_vm_artifact_ref_tx(&tx, &vm).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn insert_artifact(&self, artifact: &ArtifactRecord) -> Result<(), FleetError> {
        if artifact.reference_count != 0 {
            return Err(FleetError::Conflict(
                "new artifact reference_count must be zero".into(),
            ));
        }
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        if let Some(existing) = tx
            .query_opt(
                "SELECT artifact_id, owner_key, host_id, storage_locator, kind, status,
                        content_digest, size_bytes, immutable_image_digest, agent_digest,
                        boot_manifest_digest, parent_artifact_id, source_vm_id, creation_revision,
                        integrity_manifest_digest, chunk_size_bytes, chunk_count,
                        replication_state, reference_count, created_at, updated_at
                 FROM fleet_artifacts WHERE artifact_id = $1 AND owner_key = $2",
                &[&artifact.artifact_id, &artifact.owner_key],
            )
            .await?
        {
            if same_immutable_artifact(&row_to_artifact(&existing)?, artifact) {
                return Ok(());
            }
            return Err(FleetError::Conflict(
                "artifact id already exists with different content".into(),
            ));
        }
        if let Some(parent) = artifact.parent_artifact_id {
            let changed = tx
                .execute(
                    "UPDATE fleet_artifacts SET reference_count = reference_count + 1, updated_at = $3
                     WHERE artifact_id = $1 AND owner_key = $2 AND status = 'available'
                       AND replication_state = 'ready'",
                    &[&parent, &artifact.owner_key, &artifact.updated_at],
                )
                .await?;
            if changed != 1 {
                return Err(FleetError::NotFound);
            }
        }
        tx.execute(
            "INSERT INTO fleet_artifacts (
               artifact_id, owner_key, host_id, storage_locator, kind, status, content_digest,
               size_bytes, immutable_image_digest, agent_digest, boot_manifest_digest,
               parent_artifact_id, source_vm_id, creation_revision, integrity_manifest_digest,
               chunk_size_bytes, chunk_count,
               replication_state, reference_count, created_at, updated_at
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,0,$19,$20)",
            &[
                &artifact.artifact_id,
                &artifact.owner_key,
                &artifact.host_id,
                &artifact.storage_locator,
                &artifact.kind.as_str(),
                &artifact.status.as_str(),
                &artifact.content_digest,
                &u64_to_sql_i64(artifact.size_bytes)?,
                &artifact.immutable_image_digest,
                &artifact.agent_digest,
                &artifact.boot_manifest_digest,
                &artifact.parent_artifact_id,
                &artifact.source_vm_id,
                &u64_to_sql_i64(artifact.creation_revision)?,
                &artifact.integrity_manifest_digest,
                &u64_to_sql_i64(artifact.chunk_size_bytes)?,
                &u64_to_sql_i64(artifact.chunk_count)?,
                &if artifact.status == ArtifactStatus::Available
                    && self.min_artifact_replicas == 1
                    && self.min_artifact_failure_domains == 1
                    && tx
                        .query_opt(
                            "SELECT 1 FROM fleet_hosts
                             WHERE host_id = $1 AND healthy = TRUE
                               AND last_heartbeat >= NOW() - INTERVAL '15 seconds'",
                            &[&artifact.host_id],
                        )
                        .await?
                        .is_some()
                {
                    ArtifactReplicationState::Ready.as_str()
                } else if artifact.status == ArtifactStatus::Available {
                    ArtifactReplicationState::Degraded.as_str()
                } else {
                    ArtifactReplicationState::Pending.as_str()
                },
                &artifact.created_at,
                &artifact.updated_at,
            ],
        )
        .await
        .map_err(fleet_error_from_postgres)?;
        let replica_status = match artifact.status {
            ArtifactStatus::Available => ArtifactReplicaStatus::Available,
            ArtifactStatus::Corrupt => ArtifactReplicaStatus::Corrupt,
            ArtifactStatus::Deleting => ArtifactReplicaStatus::Deleting,
            ArtifactStatus::Staging => ArtifactReplicaStatus::Staging,
        };
        tx.execute(
            "INSERT INTO fleet_artifact_replicas (
               artifact_id, owner_key, host_id, failure_domain, storage_locator, status,
               content_digest, size_bytes, integrity_manifest_digest, verified_at,
               created_at, updated_at
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
            &[
                &artifact.artifact_id,
                &artifact.owner_key,
                &artifact.host_id,
                &artifact.host_id,
                &artifact.storage_locator,
                &replica_status.as_str(),
                &artifact.content_digest,
                &u64_to_sql_i64(artifact.size_bytes)?,
                &artifact.integrity_manifest_digest,
                &if replica_status == ArtifactReplicaStatus::Available {
                    Some(artifact.updated_at)
                } else {
                    None
                },
                &artifact.created_at,
                &artifact.updated_at,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_artifact(
        &self,
        owner_key: &str,
        artifact_id: Uuid,
    ) -> Result<ArtifactRecord, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT artifact_id, owner_key, host_id, storage_locator, kind, status,
                        content_digest, size_bytes, immutable_image_digest, agent_digest,
                        boot_manifest_digest, parent_artifact_id, source_vm_id, creation_revision,
                        integrity_manifest_digest, chunk_size_bytes, chunk_count,
                        replication_state, reference_count, created_at, updated_at
                 FROM fleet_artifacts WHERE artifact_id = $1 AND owner_key = $2",
                &[&artifact_id, &owner_key],
            )
            .await?
            .ok_or(FleetError::NotFound)?;
        row_to_artifact(&row)
    }

    /// Internal GC guard for globally unique artifact ids. No tenant or
    /// storage-locator metadata is returned.
    pub async fn artifact_exists_by_id(&self, artifact_id: Uuid) -> Result<bool, FleetError> {
        let client = self.pool.get().await?;
        Ok(client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM fleet_artifacts WHERE artifact_id = $1)",
                &[&artifact_id],
            )
            .await?
            .get(0))
    }

    /// Publish one physical replica after its bytes and manifest were verified.
    /// The logical state is derived transactionally from verified copies and
    /// distinct failure domains, never accepted from a peer as a claim.
    pub async fn upsert_artifact_replica(
        &self,
        replica: &ArtifactReplicaRecord,
    ) -> Result<ArtifactReplicationState, FleetError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let artifact = tx
            .query_opt(
                "SELECT artifact_id, owner_key, host_id, storage_locator, kind, status,
                        content_digest, size_bytes, immutable_image_digest, agent_digest,
                        boot_manifest_digest, parent_artifact_id, source_vm_id, creation_revision,
                        integrity_manifest_digest, chunk_size_bytes, chunk_count,
                        replication_state, reference_count, created_at, updated_at
                 FROM fleet_artifacts WHERE artifact_id = $1 AND owner_key = $2 FOR UPDATE",
                &[&replica.artifact_id, &replica.owner_key],
            )
            .await?
            .ok_or(FleetError::NotFound)
            .and_then(|row| row_to_artifact(&row))?;
        if replica.content_digest != artifact.content_digest
            || replica.size_bytes != artifact.size_bytes
            || replica.integrity_manifest_digest != artifact.integrity_manifest_digest
        {
            return Err(FleetError::Conflict(
                "replica metadata does not match immutable artifact".into(),
            ));
        }
        if replica.status == ArtifactReplicaStatus::Available && replica.verified_at.is_none() {
            return Err(FleetError::Conflict(
                "available replica requires verified_at".into(),
            ));
        }
        tx.execute(
            "INSERT INTO fleet_artifact_replicas (
               artifact_id, owner_key, host_id, failure_domain, storage_locator, status,
               content_digest, size_bytes, integrity_manifest_digest, verified_at,
               created_at, updated_at
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
             ON CONFLICT (artifact_id, host_id) DO UPDATE SET
               failure_domain=EXCLUDED.failure_domain,
               storage_locator=EXCLUDED.storage_locator,
               status=EXCLUDED.status,
               content_digest=EXCLUDED.content_digest,
               size_bytes=EXCLUDED.size_bytes,
               integrity_manifest_digest=EXCLUDED.integrity_manifest_digest,
               verified_at=EXCLUDED.verified_at,
               updated_at=EXCLUDED.updated_at
             WHERE fleet_artifact_replicas.owner_key=EXCLUDED.owner_key",
            &[
                &replica.artifact_id,
                &replica.owner_key,
                &replica.host_id,
                &replica.failure_domain,
                &replica.storage_locator,
                &replica.status.as_str(),
                &replica.content_digest,
                &u64_to_sql_i64(replica.size_bytes)?,
                &replica.integrity_manifest_digest,
                &replica.verified_at,
                &replica.created_at,
                &replica.updated_at,
            ],
        )
        .await
        .map_err(fleet_error_from_postgres)?;
        let row = tx
            .query_one(
                "SELECT COUNT(*), COUNT(DISTINCT replica.failure_domain)
                 FROM fleet_artifact_replicas replica
                 JOIN fleet_hosts host ON host.host_id = replica.host_id
                 WHERE replica.artifact_id = $1 AND replica.owner_key = $2
                   AND replica.status = 'available'
                   AND replica.verified_at IS NOT NULL
                   AND host.healthy = TRUE
                   AND host.last_heartbeat >= NOW() - INTERVAL '15 seconds'",
                &[&replica.artifact_id, &replica.owner_key],
            )
            .await?;
        let available = u64::try_from(row.get::<_, i64>(0)).unwrap_or(0);
        let failure_domains = u64::try_from(row.get::<_, i64>(1)).unwrap_or(0);
        let state = if available == 0 {
            ArtifactReplicationState::Pending
        } else if available >= self.min_artifact_replicas
            && failure_domains >= self.min_artifact_failure_domains
        {
            ArtifactReplicationState::Ready
        } else {
            ArtifactReplicationState::Degraded
        };
        tx.execute(
            "UPDATE fleet_artifacts SET replication_state = $3, updated_at = $4
             WHERE artifact_id = $1 AND owner_key = $2",
            &[
                &replica.artifact_id,
                &replica.owner_key,
                &state.as_str(),
                &replica.updated_at,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(state)
    }

    pub async fn list_artifact_replicas(
        &self,
        owner_key: &str,
        artifact_id: Uuid,
    ) -> Result<Vec<ArtifactReplicaRecord>, FleetError> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT artifact_id, owner_key, host_id, failure_domain, storage_locator, status,
                        content_digest, size_bytes, integrity_manifest_digest, verified_at,
                        created_at, updated_at
                 FROM fleet_artifact_replicas WHERE artifact_id = $1 AND owner_key = $2
                 ORDER BY host_id",
                &[&artifact_id, &owner_key],
            )
            .await?;
        rows.iter().map(row_to_artifact_replica).collect()
    }

    pub async fn upsert_artifact_object_replica(
        &self,
        replica: &ArtifactObjectReplicaRecord,
    ) -> Result<(), FleetError> {
        if replica.provider.is_empty()
            || replica.manifest_digest.is_empty()
            || replica.manifest_size_bytes == 0
        {
            return Err(FleetError::Conflict(
                "object replica metadata is incomplete".into(),
            ));
        }
        if replica.status == ArtifactReplicaStatus::Available && replica.verified_at.is_none() {
            return Err(FleetError::Conflict(
                "available object replica requires verified_at".into(),
            ));
        }
        let client = self.pool.get().await?;
        let artifact_exists = client
            .query_opt(
                "SELECT 1 FROM fleet_artifacts WHERE artifact_id = $1 AND owner_key = $2",
                &[&replica.artifact_id, &replica.owner_key],
            )
            .await?
            .is_some();
        if !artifact_exists {
            return Err(FleetError::NotFound);
        }
        let changed = client
            .execute(
                "INSERT INTO fleet_artifact_object_replicas (
                   artifact_id, owner_key, provider, manifest_digest, manifest_size_bytes,
                   status, verified_at, created_at, updated_at
                 ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                 ON CONFLICT (artifact_id, provider) DO UPDATE SET
                   status=EXCLUDED.status,
                   verified_at=EXCLUDED.verified_at,
                   updated_at=EXCLUDED.updated_at
                 WHERE fleet_artifact_object_replicas.owner_key=EXCLUDED.owner_key
                   AND fleet_artifact_object_replicas.manifest_digest=EXCLUDED.manifest_digest
                   AND fleet_artifact_object_replicas.manifest_size_bytes=EXCLUDED.manifest_size_bytes",
                &[
                    &replica.artifact_id,
                    &replica.owner_key,
                    &replica.provider,
                    &replica.manifest_digest,
                    &u64_to_sql_i64(replica.manifest_size_bytes)?,
                    &replica.status.as_str(),
                    &replica.verified_at,
                    &replica.created_at,
                    &replica.updated_at,
                ],
            )
            .await?;
        if changed != 1 {
            return Err(FleetError::Conflict(
                "object replica identity is immutable or belongs to another tenant".into(),
            ));
        }
        Ok(())
    }

    pub async fn list_artifact_object_replicas(
        &self,
        owner_key: &str,
        artifact_id: Uuid,
    ) -> Result<Vec<ArtifactObjectReplicaRecord>, FleetError> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT artifact_id, owner_key, provider, manifest_digest, manifest_size_bytes,
                        status, verified_at, created_at, updated_at
                 FROM fleet_artifact_object_replicas
                 WHERE artifact_id = $1 AND owner_key = $2 ORDER BY provider",
                &[&artifact_id, &owner_key],
            )
            .await?;
        rows.iter().map(row_to_artifact_object_replica).collect()
    }

    /// Recompute logical readiness from replicas whose host heartbeat is both
    /// healthy and fresh. Durable rows on a dead node remain useful for repair
    /// diagnostics but cannot satisfy the active failure-domain policy.
    pub async fn refresh_artifact_replication_health(
        &self,
        stale_before: DateTime<Utc>,
    ) -> Result<u64, FleetError> {
        let client = self.pool.get().await?;
        let changed = client
            .execute(
                "WITH health AS (
                   SELECT artifact.artifact_id, artifact.owner_key, artifact.status,
                          COUNT(replica.artifact_id) FILTER (
                            WHERE replica.status = 'available'
                              AND replica.verified_at IS NOT NULL
                              AND host.healthy = TRUE
                              AND host.last_heartbeat >= $3
                          ) AS available,
                          COUNT(DISTINCT replica.failure_domain) FILTER (
                            WHERE replica.status = 'available'
                              AND replica.verified_at IS NOT NULL
                              AND host.healthy = TRUE
                              AND host.last_heartbeat >= $3
                          ) AS failure_domains
                     FROM fleet_artifacts artifact
                     LEFT JOIN fleet_artifact_replicas replica
                       ON replica.artifact_id = artifact.artifact_id
                      AND replica.owner_key = artifact.owner_key
                     LEFT JOIN fleet_hosts host ON host.host_id = replica.host_id
                    GROUP BY artifact.artifact_id, artifact.owner_key, artifact.status
                 ), desired AS (
                   SELECT artifact_id, owner_key,
                          CASE
                            WHEN status <> 'available' OR available = 0 THEN 'pending'
                            WHEN available >= $1 AND failure_domains >= $2 THEN 'ready'
                            ELSE 'degraded'
                          END AS replication_state
                     FROM health
                 )
                 UPDATE fleet_artifacts artifact
                    SET replication_state = desired.replication_state,
                        updated_at = NOW()
                   FROM desired
                  WHERE artifact.artifact_id = desired.artifact_id
                    AND artifact.owner_key = desired.owner_key
                    AND artifact.replication_state <> desired.replication_state",
                &[
                    &u64_to_sql_i64(self.min_artifact_replicas)?,
                    &u64_to_sql_i64(self.min_artifact_failure_domains)?,
                    &stale_before,
                ],
            )
            .await?;
        Ok(changed)
    }

    pub async fn list_degraded_artifacts(
        &self,
        limit: usize,
    ) -> Result<Vec<ArtifactRecord>, FleetError> {
        let limit = i64::try_from(limit.clamp(1, 128))
            .map_err(|_| FleetError::Config("artifact repair limit overflow".into()))?;
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT artifact_id, owner_key, host_id, storage_locator, kind, status,
                        content_digest, size_bytes, immutable_image_digest, agent_digest,
                        boot_manifest_digest, parent_artifact_id, source_vm_id, creation_revision,
                        integrity_manifest_digest, chunk_size_bytes, chunk_count,
                        replication_state, reference_count, created_at, updated_at
                   FROM fleet_artifacts
                  WHERE status = 'available' AND replication_state = 'degraded'
                  ORDER BY updated_at, artifact_id
                  LIMIT $1",
                &[&limit],
            )
            .await?;
        rows.iter().map(row_to_artifact).collect()
    }

    /// Acquire one repair target lease. The current host boot session and zone
    /// are checked transactionally, and a same-zone copy cannot win while the
    /// policy still lacks distinct failure domains.
    pub async fn try_acquire_artifact_repair_lease(
        &self,
        artifact_id: Uuid,
        host_id: &str,
        boot_session_id: Uuid,
        failure_domain: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<Option<Uuid>, FleetError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        if tx
            .query_opt(
                "SELECT 1 FROM fleet_hosts
                  WHERE host_id = $1 AND boot_session_id = $2 AND healthy = TRUE
                    AND last_heartbeat >= NOW() - INTERVAL '15 seconds'",
                &[&host_id, &boot_session_id],
            )
            .await?
            .is_none()
        {
            return Ok(None);
        }
        if tx
            .query_opt(
                "SELECT 1 FROM fleet_artifacts
                  WHERE artifact_id = $1 AND status = 'available'
                    AND replication_state = 'degraded' FOR UPDATE",
                &[&artifact_id],
            )
            .await?
            .is_none()
        {
            return Ok(None);
        }
        if tx
            .query_opt(
                "SELECT 1 FROM fleet_artifact_replicas
                  WHERE artifact_id = $1 AND host_id = $2
                    AND status = 'available' AND verified_at IS NOT NULL",
                &[&artifact_id, &host_id],
            )
            .await?
            .is_some()
        {
            return Ok(None);
        }
        let health = tx
            .query_one(
                "SELECT COUNT(replica.artifact_id),
                        COUNT(DISTINCT replica.failure_domain),
                        COUNT(replica.artifact_id) FILTER (WHERE replica.failure_domain = $2)
                   FROM fleet_artifact_replicas replica
                   JOIN fleet_hosts host ON host.host_id = replica.host_id
                  WHERE replica.artifact_id = $1
                    AND replica.status = 'available' AND replica.verified_at IS NOT NULL
                    AND host.healthy = TRUE
                    AND host.last_heartbeat >= NOW() - INTERVAL '15 seconds'",
                &[&artifact_id, &failure_domain],
            )
            .await?;
        let available = u64::try_from(health.get::<_, i64>(0)).unwrap_or(0);
        let domains = u64::try_from(health.get::<_, i64>(1)).unwrap_or(0);
        let same_domain = u64::try_from(health.get::<_, i64>(2)).unwrap_or(0);
        if same_domain > 0 && domains < self.min_artifact_failure_domains {
            return Ok(None);
        }
        if available >= self.min_artifact_replicas && domains >= self.min_artifact_failure_domains {
            return Ok(None);
        }
        let token = Uuid::new_v4();
        let acquired = tx
            .query_opt(
                "INSERT INTO fleet_artifact_repair_leases
                   (artifact_id, holder_host_id, holder_boot_session_id, lease_token, expires_at)
                 VALUES ($1,$2,$3,$4,$5)
                 ON CONFLICT (artifact_id) DO UPDATE SET
                   holder_host_id = EXCLUDED.holder_host_id,
                   holder_boot_session_id = EXCLUDED.holder_boot_session_id,
                   lease_token = EXCLUDED.lease_token,
                   expires_at = EXCLUDED.expires_at
                 WHERE fleet_artifact_repair_leases.expires_at < NOW()
                 RETURNING lease_token",
                &[
                    &artifact_id,
                    &host_id,
                    &boot_session_id,
                    &token,
                    &expires_at,
                ],
            )
            .await?
            .is_some();
        tx.commit().await?;
        Ok(acquired.then_some(token))
    }

    pub async fn renew_artifact_repair_lease(
        &self,
        artifact_id: Uuid,
        host_id: &str,
        boot_session_id: Uuid,
        lease_token: Uuid,
        expires_at: DateTime<Utc>,
    ) -> Result<bool, FleetError> {
        let client = self.pool.get().await?;
        Ok(client
            .execute(
                "UPDATE fleet_artifact_repair_leases SET expires_at = $5
                  WHERE artifact_id = $1 AND holder_host_id = $2
                    AND holder_boot_session_id = $3 AND lease_token = $4",
                &[
                    &artifact_id,
                    &host_id,
                    &boot_session_id,
                    &lease_token,
                    &expires_at,
                ],
            )
            .await?
            == 1)
    }

    pub async fn release_artifact_repair_lease(
        &self,
        artifact_id: Uuid,
        host_id: &str,
        boot_session_id: Uuid,
        lease_token: Uuid,
    ) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        client
            .execute(
                "DELETE FROM fleet_artifact_repair_leases
                  WHERE artifact_id = $1 AND holder_host_id = $2
                    AND holder_boot_session_id = $3 AND lease_token = $4",
                &[&artifact_id, &host_id, &boot_session_id, &lease_token],
            )
            .await?;
        Ok(())
    }

    pub async fn insert_branch(&self, branch: &BranchRecord) -> Result<BranchRecord, FleetError> {
        let branch = normalize_branch_timestamps_for_postgres(branch);
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        if let Some(row) = tx
            .query_opt(
                "SELECT branch_id, owner_key, name, head_artifact_id, source_vm_id,
                        source_branch_id, revision, created_at, updated_at
                 FROM fleet_branches WHERE branch_id = $1 AND owner_key = $2",
                &[&branch.branch_id, &branch.owner_key],
            )
            .await?
        {
            let existing = row_to_branch(&row)?;
            if existing.name == branch.name
                && existing.head_artifact_id == branch.head_artifact_id
                && existing.source_vm_id == branch.source_vm_id
                && existing.source_branch_id == branch.source_branch_id
            {
                return Ok(existing);
            }
            return Err(FleetError::Conflict(
                "branch id already exists with different content".into(),
            ));
        }
        if let Some(source_branch) = branch.source_branch_id {
            if tx
                .query_opt(
                    "SELECT 1 FROM fleet_branches WHERE branch_id = $1 AND owner_key = $2",
                    &[&source_branch, &branch.owner_key],
                )
                .await?
                .is_none()
            {
                return Err(FleetError::NotFound);
            }
        }
        if let Some(source_vm) = branch.source_vm_id {
            if tx
                .query_opt(
                    "SELECT 1 FROM fleet_vms WHERE id = $1 AND owner_key = $2",
                    &[&source_vm, &branch.owner_key],
                )
                .await?
                .is_none()
            {
                return Err(FleetError::NotFound);
            }
        }
        let acquired = tx
            .execute(
                "UPDATE fleet_artifacts SET reference_count = reference_count + 1, updated_at = $3
                 WHERE artifact_id = $1 AND owner_key = $2 AND status = 'available'
                   AND replication_state = 'ready'",
                &[
                    &branch.head_artifact_id,
                    &branch.owner_key,
                    &branch.updated_at,
                ],
            )
            .await?;
        if acquired != 1 {
            return Err(FleetError::NotFound);
        }
        tx.execute(
            "INSERT INTO fleet_branches (
               branch_id, owner_key, name, head_artifact_id, source_vm_id, source_branch_id,
               revision, created_at, updated_at
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
            &[
                &branch.branch_id,
                &branch.owner_key,
                &branch.name,
                &branch.head_artifact_id,
                &branch.source_vm_id,
                &branch.source_branch_id,
                &u64_to_sql_i64(branch.revision)?,
                &branch.created_at,
                &branch.updated_at,
            ],
        )
        .await
        .map_err(fleet_error_from_postgres)?;
        tx.commit().await?;
        Ok(branch)
    }

    pub async fn get_branch(
        &self,
        owner_key: &str,
        branch_id: Uuid,
    ) -> Result<BranchRecord, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT branch_id, owner_key, name, head_artifact_id, source_vm_id,
                        source_branch_id, revision, created_at, updated_at
                 FROM fleet_branches WHERE branch_id = $1 AND owner_key = $2",
                &[&branch_id, &owner_key],
            )
            .await?
            .ok_or(FleetError::NotFound)?;
        row_to_branch(&row)
    }

    pub async fn list_branches(&self, owner_key: &str) -> Result<Vec<BranchRecord>, FleetError> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT branch_id, owner_key, name, head_artifact_id, source_vm_id,
                        source_branch_id, revision, created_at, updated_at
                 FROM fleet_branches WHERE owner_key = $1 ORDER BY created_at DESC",
                &[&owner_key],
            )
            .await?;
        rows.iter().map(row_to_branch).collect()
    }

    pub async fn update_branch_head(
        &self,
        owner_key: &str,
        branch_id: Uuid,
        expected_revision: u64,
        new_head: Uuid,
        updated_at: DateTime<Utc>,
    ) -> Result<BranchRecord, FleetError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let row = tx
            .query_opt(
                "SELECT branch_id, owner_key, name, head_artifact_id, source_vm_id,
                        source_branch_id, revision, created_at, updated_at
                 FROM fleet_branches WHERE branch_id = $1 AND owner_key = $2 FOR UPDATE",
                &[&branch_id, &owner_key],
            )
            .await?
            .ok_or(FleetError::NotFound)?;
        let current = row_to_branch(&row)?;
        if current.revision != expected_revision {
            return Err(FleetError::Conflict(format!(
                "branch revision is {}, expected {expected_revision}",
                current.revision
            )));
        }
        if current.head_artifact_id != new_head {
            let acquired = tx
                .execute(
                    "UPDATE fleet_artifacts SET reference_count = reference_count + 1, updated_at = $3
                     WHERE artifact_id = $1 AND owner_key = $2 AND status = 'available'
                       AND replication_state = 'ready'",
                    &[&new_head, &owner_key, &updated_at],
                )
                .await?;
            if acquired != 1 {
                return Err(FleetError::NotFound);
            }
        }
        let next_revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| FleetError::Conflict("branch revision overflow".into()))?;
        tx.execute(
            "UPDATE fleet_branches SET head_artifact_id = $4, revision = $5, updated_at = $6
             WHERE branch_id = $1 AND owner_key = $2 AND revision = $3",
            &[
                &branch_id,
                &owner_key,
                &u64_to_sql_i64(expected_revision)?,
                &new_head,
                &u64_to_sql_i64(next_revision)?,
                &updated_at,
            ],
        )
        .await?;
        if current.head_artifact_id != new_head {
            let released = tx
                .execute(
                    "UPDATE fleet_artifacts SET reference_count = reference_count - 1, updated_at = $3
                     WHERE artifact_id = $1 AND owner_key = $2 AND reference_count > 0",
                    &[&current.head_artifact_id, &owner_key, &updated_at],
                )
                .await?;
            if released != 1 {
                return Err(FleetError::Conflict(
                    "old branch head has no reference to release".into(),
                ));
            }
        }
        let row = tx
            .query_one(
                "SELECT branch_id, owner_key, name, head_artifact_id, source_vm_id,
                        source_branch_id, revision, created_at, updated_at
                 FROM fleet_branches WHERE branch_id = $1",
                &[&branch_id],
            )
            .await?;
        let branch = row_to_branch(&row)?;
        tx.commit().await?;
        Ok(branch)
    }

    pub async fn delete_branch(
        &self,
        owner_key: &str,
        branch_id: Uuid,
    ) -> Result<BranchRecord, FleetError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let row = tx
            .query_opt(
                "SELECT branch_id, owner_key, name, head_artifact_id, source_vm_id,
                        source_branch_id, revision, created_at, updated_at
                 FROM fleet_branches WHERE branch_id = $1 AND owner_key = $2 FOR UPDATE",
                &[&branch_id, &owner_key],
            )
            .await?
            .ok_or(FleetError::NotFound)?;
        let branch = row_to_branch(&row)?;
        tx.execute(
            "DELETE FROM fleet_branches WHERE branch_id = $1 AND owner_key = $2",
            &[&branch_id, &owner_key],
        )
        .await?;
        let released = tx
            .execute(
                "UPDATE fleet_artifacts SET reference_count = reference_count - 1, updated_at = $3
                 WHERE artifact_id = $1 AND owner_key = $2 AND reference_count > 0",
                &[&branch.head_artifact_id, &owner_key, &Utc::now()],
            )
            .await?;
        if released != 1 {
            return Err(FleetError::Conflict(
                "branch head has no reference to release".into(),
            ));
        }
        delete_unreferenced_artifact_chain_tx(&tx, owner_key, branch.head_artifact_id).await?;
        tx.commit().await?;
        Ok(branch)
    }

    pub async fn delete_artifact_if_unreferenced(
        &self,
        owner_key: &str,
        artifact_id: Uuid,
    ) -> Result<ArtifactRecord, FleetError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let row = tx
            .query_opt(
                "SELECT artifact_id, owner_key, host_id, storage_locator, kind, status,
                        content_digest, size_bytes, immutable_image_digest, agent_digest,
                        boot_manifest_digest, parent_artifact_id, source_vm_id, creation_revision,
                        integrity_manifest_digest, chunk_size_bytes, chunk_count,
                        replication_state, reference_count, created_at, updated_at
                 FROM fleet_artifacts WHERE artifact_id = $1 AND owner_key = $2 FOR UPDATE",
                &[&artifact_id, &owner_key],
            )
            .await?
            .ok_or(FleetError::NotFound)?;
        let artifact = row_to_artifact(&row)?;
        if artifact.reference_count != 0 {
            return Err(FleetError::Conflict(format!(
                "artifact has {} references",
                artifact.reference_count
            )));
        }
        tx.execute(
            "DELETE FROM fleet_artifacts
             WHERE artifact_id = $1 AND owner_key = $2 AND reference_count = 0",
            &[&artifact_id, &owner_key],
        )
        .await
        .map_err(fleet_error_from_postgres)?;
        if let Some(parent_id) = artifact.parent_artifact_id {
            let released = tx
                .execute(
                    "UPDATE fleet_artifacts SET reference_count = reference_count - 1, updated_at = $3
                     WHERE artifact_id = $1 AND owner_key = $2 AND reference_count > 0",
                    &[&parent_id, &owner_key, &Utc::now()],
                )
                .await?;
            if released != 1 {
                return Err(FleetError::Conflict(
                    "parent artifact has no reference to release".into(),
                ));
            }
        }
        tx.commit().await?;
        Ok(artifact)
    }

    /// Remove a VM's ownership row (called when a VM is stopped/deleted) so the
    /// cluster no longer routes to a dead sandbox.
    pub async fn delete_vm(&self, vm: &VmRecord) -> Result<(), FleetError> {
        // `upsert_vm` persists PostgreSQL's microsecond representation. Apply
        // the same normalization to the incarnation fence here; otherwise a
        // valid cleanup carrying chrono nanoseconds is rejected as stale even
        // though it owns the row that PostgreSQL rounded on insertion.
        let vm = normalize_vm_timestamps_for_postgres(vm);
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let current = tx
            .query_opt(
                "SELECT host_id, created_at FROM fleet_vms WHERE id = $1 FOR UPDATE",
                &[&vm.id],
            )
            .await?;
        if let Some(current) = current {
            if current.get::<_, String>(0) != vm.host_id
                || current.get::<_, DateTime<Utc>>(1) != vm.created_at
            {
                return Err(FleetError::Conflict(format!(
                    "refusing stale ownership delete for VM {}",
                    vm.id
                )));
            }
        }
        // The lazy-restore binding is acquired before fleet VM publication, so
        // failed boots must release it even when no fleet_vms row was reached.
        release_vm_artifact_ref_tx(&tx, &vm).await?;
        if let Some(row) = tx
            .query_opt(
                "DELETE FROM fleet_hibernations WHERE vm_id = $1 RETURNING artifact_id, owner_key",
                &[&vm.id],
            )
            .await?
        {
            let artifact_id: Uuid = row.get(0);
            let owner_key: String = row.get(1);
            let released = tx
                .execute(
                    "UPDATE fleet_artifacts
                        SET reference_count = reference_count - 1, updated_at = NOW()
                      WHERE artifact_id = $1 AND owner_key = $2 AND reference_count > 0",
                    &[&artifact_id, &owner_key],
                )
                .await?;
            if released != 1 {
                return Err(FleetError::Conflict(
                    "hibernation artifact reference is missing".into(),
                ));
            }
            delete_unreferenced_artifact_chain_tx(&tx, &owner_key, artifact_id).await?;
        }
        tx.execute(
            "DELETE FROM fleet_vms WHERE id = $1 AND created_at = $2",
            &[&vm.id, &vm.created_at],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Publish the provider-neutral desired volume record. Exact replay is
    /// idempotent; neither a UUID nor a tenant-local name can be rebound to a
    /// different provider resource.
    pub async fn insert_volume(&self, volume: &VolumeRecord) -> Result<VolumeRecord, FleetError> {
        if volume.owner_key.is_empty()
            || volume.name.is_empty()
            || volume.provider.is_empty()
            || volume.generation == 0
            || volume.revision == 0
        {
            return Err(FleetError::Conflict(
                "volume identity, provider, generation, and revision are required".into(),
            ));
        }
        let client = self.pool.get().await?;
        let inserted = client
            .execute(
                "INSERT INTO fleet_volumes (
                   id, owner_key, name, provider, storage_class, size_bytes, status,
                   read_only_many, read_write_once, read_write_many, snapshots, clones,
                   host_id, region, zone, generation, revision, last_error, created_at, updated_at
                 ) VALUES (
                   $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20
                 ) ON CONFLICT DO NOTHING",
                &[
                    &volume.id,
                    &volume.owner_key,
                    &volume.name,
                    &volume.provider,
                    &volume.storage_class.as_str(),
                    &u64_to_sql_i64(volume.size_bytes)?,
                    &volume.status.as_str(),
                    &volume.capabilities.read_only_many,
                    &volume.capabilities.read_write_once,
                    &volume.capabilities.read_write_many,
                    &volume.capabilities.snapshots,
                    &volume.capabilities.clones,
                    &volume.host_id,
                    &volume.region,
                    &volume.zone,
                    &u64_to_sql_i64(volume.generation)?,
                    &u64_to_sql_i64(volume.revision)?,
                    &volume.last_error,
                    &volume.created_at,
                    &volume.updated_at,
                ],
            )
            .await?;
        if inserted == 1 {
            return Ok(volume.clone());
        }
        let row = client
            .query_opt(
                &format!(
                    "SELECT {VOLUME_COLUMNS} FROM fleet_volumes
                     WHERE id = $1 OR (owner_key = $2 AND name = $3)"
                ),
                &[&volume.id, &volume.owner_key, &volume.name],
            )
            .await?
            .ok_or(FleetError::NotFound)?;
        let existing = row_to_volume(&row)?;
        if same_immutable_volume(&existing, volume) {
            Ok(existing)
        } else {
            Err(FleetError::Conflict(
                "volume id or name already exists with different immutable properties".into(),
            ))
        }
    }

    pub async fn get_volume(&self, owner_key: &str, id: Uuid) -> Result<VolumeRecord, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                &format!(
                    "SELECT {VOLUME_COLUMNS} FROM fleet_volumes WHERE id = $1 AND owner_key = $2"
                ),
                &[&id, &owner_key],
            )
            .await?
            .ok_or(FleetError::NotFound)?;
        row_to_volume(&row)
    }

    pub async fn list_volumes(&self, owner_key: &str) -> Result<Vec<VolumeRecord>, FleetError> {
        let client = self.pool.get().await?;
        client
            .query(
                &format!(
                    "SELECT {VOLUME_COLUMNS} FROM fleet_volumes
                     WHERE owner_key = $1 ORDER BY created_at, id"
                ),
                &[&owner_key],
            )
            .await?
            .iter()
            .map(row_to_volume)
            .collect()
    }

    pub async fn transition_volume(
        &self,
        owner_key: &str,
        id: Uuid,
        transition: VolumeTransition<'_>,
    ) -> Result<VolumeRecord, FleetError> {
        let client = self.pool.get().await?;
        let next_revision = transition
            .expected_revision
            .checked_add(1)
            .ok_or_else(|| FleetError::Conflict("volume revision exhausted".into()))?;
        let row = client
            .query_opt(
                &format!(
                    "UPDATE fleet_volumes
                     SET status = $5, revision = $6, last_error = $7, updated_at = $8
                     WHERE id = $1 AND owner_key = $2 AND status = $3 AND revision = $4
                     RETURNING {VOLUME_COLUMNS}"
                ),
                &[
                    &id,
                    &owner_key,
                    &transition.expected_status.as_str(),
                    &u64_to_sql_i64(transition.expected_revision)?,
                    &transition.status.as_str(),
                    &u64_to_sql_i64(next_revision)?,
                    &transition.last_error,
                    &transition.updated_at,
                ],
            )
            .await?;
        match row {
            Some(row) => row_to_volume(&row),
            None if self.get_volume(owner_key, id).await.is_ok() => {
                Err(FleetError::Conflict("stale volume transition".into()))
            }
            None => Err(FleetError::NotFound),
        }
    }

    /// Atomically claim deletion only when no VM anywhere in the fleet holds
    /// an attachment. The status/revision CAS makes provider retries safe.
    pub async fn begin_volume_delete(
        &self,
        owner_key: &str,
        id: Uuid,
        expected_status: VolumeStatus,
        expected_revision: u64,
        updated_at: DateTime<Utc>,
    ) -> Result<VolumeRecord, FleetError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let row = tx
            .query_opt(
                "SELECT status, revision FROM fleet_volumes
                 WHERE id = $1 AND owner_key = $2 FOR UPDATE",
                &[&id, &owner_key],
            )
            .await?
            .ok_or(FleetError::NotFound)?;
        if row.get::<_, String>(0) != expected_status.as_str()
            || row.get::<_, i64>(1) != u64_to_sql_i64(expected_revision)?
        {
            return Err(FleetError::Conflict("stale volume deletion claim".into()));
        }
        if tx
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM fleet_vm_volume_attachments WHERE volume_id = $1)",
                &[&id],
            )
            .await?
            .get::<_, bool>(0)
        {
            return Err(FleetError::Conflict("volume is attached to a VM".into()));
        }
        let next_revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| FleetError::Conflict("volume revision exhausted".into()))?;
        let row = tx
            .query_one(
                &format!(
                    "UPDATE fleet_volumes
                     SET status = 'deleting', revision = $3, last_error = NULL, updated_at = $4
                     WHERE id = $1 AND owner_key = $2 RETURNING {VOLUME_COLUMNS}"
                ),
                &[
                    &id,
                    &owner_key,
                    &u64_to_sql_i64(next_revision)?,
                    &updated_at,
                ],
            )
            .await?;
        let volume = row_to_volume(&row)?;
        tx.commit().await?;
        Ok(volume)
    }

    pub async fn delete_volume_metadata(
        &self,
        owner_key: &str,
        id: Uuid,
        expected_revision: u64,
    ) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        let deleted = client
            .execute(
                "DELETE FROM fleet_volumes v
                 WHERE v.id = $1 AND v.owner_key = $2 AND v.status = 'deleting'
                   AND v.revision = $3
                   AND NOT EXISTS (
                     SELECT 1 FROM fleet_vm_volume_attachments a WHERE a.volume_id = v.id
                   )",
                &[&id, &owner_key, &u64_to_sql_i64(expected_revision)?],
            )
            .await?;
        if deleted == 1 {
            Ok(())
        } else if self.get_volume(owner_key, id).await.is_ok() {
            Err(FleetError::Conflict(
                "stale or attached volume deletion".into(),
            ))
        } else {
            Err(FleetError::NotFound)
        }
    }

    /// Acquire all requested bindings under one PostgreSQL transaction. Row
    /// locks and the partial unique index provide a fleet-wide RWO fence.
    pub async fn bind_vm_volumes(
        &self,
        attachments: &[VmVolumeAttachmentRecord],
    ) -> Result<(), FleetError> {
        if attachments.len() > 15 {
            return Err(FleetError::Conflict(
                "a VM supports at most 15 persistent data volumes".into(),
            ));
        }
        let Some(first) = attachments.first() else {
            return Ok(());
        };
        if attachments.iter().any(|attachment| {
            attachment.vm_id != first.vm_id || attachment.owner_key != first.owner_key
        }) {
            return Err(FleetError::Conflict(
                "one bind transaction must target one VM and tenant".into(),
            ));
        }
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let vm_owner = tx
            .query_opt(
                "SELECT owner_key FROM fleet_vms WHERE id = $1 FOR UPDATE",
                &[&first.vm_id],
            )
            .await?
            .ok_or(FleetError::NotFound)?
            .get::<_, Option<String>>(0);
        if vm_owner.as_deref() != Some(first.owner_key.as_str()) {
            return Err(FleetError::Conflict("VM belongs to another tenant".into()));
        }
        let mut ordered_attachments = attachments.iter().collect::<Vec<_>>();
        ordered_attachments.sort_unstable_by_key(|attachment| attachment.volume_id);
        for attachment in ordered_attachments {
            if attachment.owner_key.is_empty()
                || attachment.volume_generation == 0
                || usize::from(attachment.device_index) >= attachments.len()
            {
                return Err(FleetError::Conflict(
                    "invalid VM volume attachment identity".into(),
                ));
            }
            let volume = tx
                .query_opt(
                    "SELECT owner_key, status, generation, read_only_many, read_write_once,
                            read_write_many
                     FROM fleet_volumes WHERE id = $1 FOR UPDATE",
                    &[&attachment.volume_id],
                )
                .await?
                .ok_or(FleetError::NotFound)?;
            if volume.get::<_, String>(0) != attachment.owner_key
                || volume.get::<_, String>(1) != VolumeStatus::Available.as_str()
                || volume.get::<_, i64>(2) != u64_to_sql_i64(attachment.volume_generation)?
                || (attachment.mode == VolumeAttachmentMode::ReadOnly && !volume.get::<_, bool>(3))
                || (attachment.mode == VolumeAttachmentMode::ReadWrite
                    && !volume.get::<_, bool>(4)
                    && !volume.get::<_, bool>(5))
            {
                return Err(FleetError::Conflict(
                    "volume is unavailable, belongs to another tenant, or lacks the requested access mode"
                        .into(),
                ));
            }
            if let Some(existing) = tx
                .query_opt(
                    "SELECT device_index, owner_key, mode, volume_generation
                     FROM fleet_vm_volume_attachments WHERE vm_id = $1 AND volume_id = $2",
                    &[&attachment.vm_id, &attachment.volume_id],
                )
                .await?
            {
                if existing.get::<_, i16>(0) != i16::from(attachment.device_index)
                    || existing.get::<_, String>(1) != attachment.owner_key
                    || existing.get::<_, String>(2) != attachment.mode.as_str()
                    || existing.get::<_, i64>(3) != u64_to_sql_i64(attachment.volume_generation)?
                {
                    return Err(FleetError::Conflict(
                        "VM volume attachment replay changed immutable properties".into(),
                    ));
                }
                continue;
            }
            let read_write_many = volume.get::<_, bool>(5);
            let conflicting: bool = !read_write_many
                && tx
                    .query_one(
                        "SELECT EXISTS(
                           SELECT 1 FROM fleet_vm_volume_attachments
                           WHERE volume_id = $1 AND ($2 = 'read_write' OR mode = 'read_write')
                         )",
                        &[&attachment.volume_id, &attachment.mode.as_str()],
                    )
                    .await?
                    .get::<_, bool>(0);
            if conflicting {
                return Err(FleetError::Conflict(
                    "read-write volume attachment is exclusive".into(),
                ));
            }
            tx.execute(
                "INSERT INTO fleet_vm_volume_attachments
                 (vm_id, volume_id, device_index, owner_key, mode, volume_generation, created_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7)",
                &[
                    &attachment.vm_id,
                    &attachment.volume_id,
                    &i16::from(attachment.device_index),
                    &attachment.owner_key,
                    &attachment.mode.as_str(),
                    &u64_to_sql_i64(attachment.volume_generation)?,
                    &attachment.created_at,
                ],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn volume_attachment_count(
        &self,
        owner_key: &str,
        volume_id: Uuid,
    ) -> Result<u64, FleetError> {
        let client = self.pool.get().await?;
        let count = client
            .query_one(
                "SELECT COUNT(*) FROM fleet_vm_volume_attachments
                 WHERE owner_key = $1 AND volume_id = $2",
                &[&owner_key, &volume_id],
            )
            .await?
            .get::<_, i64>(0);
        u64::try_from(count)
            .map_err(|_| FleetError::Config("negative volume attachment count".into()))
    }

    pub async fn list_vm_volume_attachments(
        &self,
        owner_key: &str,
        vm_id: Uuid,
    ) -> Result<Vec<VmVolumeAttachmentRecord>, FleetError> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT vm_id, volume_id, device_index, owner_key, mode,
                        volume_generation, created_at
                 FROM fleet_vm_volume_attachments
                 WHERE vm_id = $1 AND owner_key = $2
                 ORDER BY device_index",
                &[&vm_id, &owner_key],
            )
            .await?;
        rows.into_iter()
            .map(|row| {
                let device_index = u8::try_from(row.get::<_, i16>(2))
                    .map_err(|_| FleetError::Config("invalid fleet volume device index".into()))?;
                let mode = row.get::<_, String>(4);
                let mode = VolumeAttachmentMode::parse(&mode).ok_or_else(|| {
                    FleetError::Config(format!("invalid fleet volume attachment mode: {mode}"))
                })?;
                let generation = u64::try_from(row.get::<_, i64>(5)).map_err(|_| {
                    FleetError::Config("invalid fleet volume attachment generation".into())
                })?;
                Ok(VmVolumeAttachmentRecord {
                    vm_id: row.get(0),
                    volume_id: row.get(1),
                    device_index,
                    owner_key: row.get(3),
                    mode,
                    volume_generation: generation,
                    created_at: row.get(6),
                })
            })
            .collect()
    }

    /// Release only one tenant's VM bindings. VM terminal deletion also
    /// cascades these rows, but this explicit path lets a node roll back a
    /// failed local-cache bind without weakening the global fence.
    pub async fn unbind_vm_volumes(&self, owner_key: &str, vm_id: Uuid) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        client
            .execute(
                "DELETE FROM fleet_vm_volume_attachments
                 WHERE vm_id = $1 AND owner_key = $2",
                &[&vm_id, &owner_key],
            )
            .await?;
        Ok(())
    }

    /// Reserve a tenant quota slot before placement. The tenant-scoped advisory
    /// lock makes count+reserve atomic across all API nodes. Reservations expire
    /// automatically if a request is cancelled before it can release the slot.
    pub async fn reserve_vm_quota(
        &self,
        owner_key: &str,
        id: Uuid,
        max_vms: usize,
        expires_at: DateTime<Utc>,
    ) -> Result<(), FleetError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            &[&owner_key],
        )
        .await?;
        tx.execute(
            "DELETE FROM tenant_vm_reservations WHERE expires_at <= NOW()",
            &[],
        )
        .await?;
        if tx
            .query_opt(
                "SELECT 1 FROM fleet_vms WHERE id = $1
                 UNION ALL
                 SELECT 1 FROM fleet_vm_fork_operations WHERE child_vm_id = $1
                 LIMIT 1",
                &[&id],
            )
            .await?
            .is_some()
        {
            return Err(FleetError::Conflict(format!("VM {id} already exists")));
        }
        if let Some(row) = tx
            .query_opt(
                "SELECT owner_key FROM tenant_vm_reservations WHERE id = $1",
                &[&id],
            )
            .await?
        {
            let existing_owner: String = row.get(0);
            if existing_owner != owner_key {
                return Err(FleetError::Conflict(format!(
                    "VM {id} quota reservation belongs to another tenant"
                )));
            }
            // Match the single-host store: an existing reservation is a
            // conflict even for the same tenant. Treating a duplicate request
            // as success would let two admission paths share one reservation
            // and release the survivor's quota protection early.
            return Err(FleetError::Conflict(format!(
                "VM {id} already exists or is being created"
            )));
        }
        let active = tx
            .query_one(
                "SELECT
                   (SELECT COUNT(*) FROM fleet_vms
                     WHERE owner_key = $1 AND status IN ('creating','running','paused','suspended'))
                   +
                   (SELECT COUNT(*) FROM tenant_vm_reservations
                     WHERE owner_key = $1 AND expires_at > NOW())",
                &[&owner_key],
            )
            .await?
            .get::<_, i64>(0);
        if active >= i64::try_from(max_vms).unwrap_or(i64::MAX) {
            return Err(FleetError::QuotaExceeded {
                owner_key: owner_key.to_string(),
                max_vms,
            });
        }
        tx.execute(
            "INSERT INTO tenant_vm_reservations (id, owner_key, expires_at)
             VALUES ($1,$2,$3)",
            &[&id, &owner_key, &expires_at],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Atomically create or resume a source-bound fork operation and hold its
    /// tenant admission slot. The child id cannot be claimed by an unrelated
    /// create while the operation is preparing.
    pub async fn claim_fork_operation(
        &self,
        operation: &ForkOperationRecord,
        max_vms: usize,
        expires_at: DateTime<Utc>,
    ) -> Result<ForkOperationClaimOutcome, FleetError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            &[&operation.owner_key],
        )
        .await?;
        tx.execute(
            "DELETE FROM tenant_vm_reservations WHERE expires_at <= NOW()",
            &[],
        )
        .await?;

        let existing = tx
            .query_opt(
                "SELECT source_vm_id, owner_key, source_host_id, target_host_id,
                        target_boot_session_id, status, child_created_at, created_at, updated_at
                 FROM fleet_vm_fork_operations WHERE child_vm_id = $1",
                &[&operation.child_vm_id],
            )
            .await?;
        let outcome = if let Some(row) = existing.as_ref() {
            let source_vm_id: Uuid = row.get(0);
            let owner_key: String = row.get(1);
            let source_host_id: String = row.get(2);
            let target_host_id: String = row.get(3);
            let target_boot_session_id: Option<Uuid> = row.get(4);
            let status: String = row.get(5);
            if source_vm_id != operation.source_vm_id
                || owner_key != operation.owner_key
                || source_host_id != operation.source_host_id
                || target_host_id != operation.target_host_id
            {
                return Err(FleetError::Conflict(format!(
                    "fork child {} is already bound to another operation",
                    operation.child_vm_id
                )));
            }
            match ForkOperationStatus::parse(&status) {
                Some(ForkOperationStatus::Committed) => {
                    tx.commit().await?;
                    return Ok(ForkOperationClaimOutcome::Committed);
                }
                Some(ForkOperationStatus::Preparing) => {
                    if target_boot_session_id != operation.target_boot_session_id {
                        tx.execute(
                            "UPDATE fleet_vm_fork_operations
                             SET target_boot_session_id = $2, updated_at = $3
                             WHERE child_vm_id = $1 AND status = 'preparing'",
                            &[
                                &operation.child_vm_id,
                                &operation.target_boot_session_id,
                                &operation.updated_at,
                            ],
                        )
                        .await?;
                    }
                    ForkOperationClaimOutcome::Resumed
                }
                None => {
                    return Err(FleetError::Conflict(format!(
                        "fork child {} has invalid durable status {status}",
                        operation.child_vm_id
                    )));
                }
            }
        } else {
            if tx
                .query_opt(
                    "SELECT 1 FROM fleet_vms WHERE id = $1",
                    &[&operation.child_vm_id],
                )
                .await?
                .is_some()
            {
                return Err(FleetError::Conflict(format!(
                    "VM {} already exists",
                    operation.child_vm_id
                )));
            }
            tx.execute(
                "INSERT INTO fleet_vm_fork_operations (
                   child_vm_id, source_vm_id, owner_key, source_host_id, target_host_id,
                   target_boot_session_id, status, child_created_at, created_at, updated_at
                 ) VALUES ($1,$2,$3,$4,$5,$6,'preparing',NULL,$7,$8)",
                &[
                    &operation.child_vm_id,
                    &operation.source_vm_id,
                    &operation.owner_key,
                    &operation.source_host_id,
                    &operation.target_host_id,
                    &operation.target_boot_session_id,
                    &operation.created_at,
                    &operation.updated_at,
                ],
            )
            .await?;
            ForkOperationClaimOutcome::New
        };

        let resumed_after_restart = existing
            .as_ref()
            .map(|row| row.get::<_, Option<Uuid>>(4) != operation.target_boot_session_id)
            .unwrap_or(false);
        if let Some(row) = tx
            .query_opt(
                "SELECT owner_key FROM tenant_vm_reservations WHERE id = $1",
                &[&operation.child_vm_id],
            )
            .await?
        {
            let existing_owner: String = row.get(0);
            if existing_owner != operation.owner_key {
                return Err(FleetError::Conflict(format!(
                    "VM {} reservation belongs to another tenant",
                    operation.child_vm_id
                )));
            }
            if outcome == ForkOperationClaimOutcome::Resumed && !resumed_after_restart {
                tx.commit().await?;
                return Ok(ForkOperationClaimOutcome::InProgress);
            }
            tx.execute(
                "UPDATE tenant_vm_reservations SET expires_at = $2 WHERE id = $1",
                &[&operation.child_vm_id, &expires_at],
            )
            .await?;
        } else if tx
            .query_opt(
                "SELECT 1 FROM fleet_vms WHERE id = $1",
                &[&operation.child_vm_id],
            )
            .await?
            .is_none()
        {
            let active = tx
                .query_one(
                    "SELECT
                       (SELECT COUNT(*) FROM fleet_vms
                         WHERE owner_key = $1 AND status IN ('creating','running','paused','suspended'))
                       +
                       (SELECT COUNT(*) FROM tenant_vm_reservations
                         WHERE owner_key = $1 AND expires_at > NOW())",
                    &[&operation.owner_key],
                )
                .await?
                .get::<_, i64>(0);
            if active >= i64::try_from(max_vms).unwrap_or(i64::MAX) {
                return Err(FleetError::QuotaExceeded {
                    owner_key: operation.owner_key.clone(),
                    max_vms,
                });
            }
            tx.execute(
                "INSERT INTO tenant_vm_reservations (id, owner_key, expires_at)
                 VALUES ($1,$2,$3)",
                &[&operation.child_vm_id, &operation.owner_key, &expires_at],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(outcome)
    }

    pub async fn get_fork_operation(
        &self,
        child_vm_id: Uuid,
    ) -> Result<Option<ForkOperationRecord>, FleetError> {
        let client = self.pool.get().await?;
        let Some(row) = client
            .query_opt(
                "SELECT source_vm_id, owner_key, source_host_id, target_host_id,
                        target_boot_session_id, status, child_created_at, created_at, updated_at
                 FROM fleet_vm_fork_operations WHERE child_vm_id = $1",
                &[&child_vm_id],
            )
            .await?
        else {
            return Ok(None);
        };
        let status: String = row.get(5);
        let status = ForkOperationStatus::parse(&status).ok_or_else(|| {
            FleetError::Conflict(format!(
                "fork child {child_vm_id} has invalid durable status {status}"
            ))
        })?;
        Ok(Some(ForkOperationRecord {
            child_vm_id,
            source_vm_id: row.get(0),
            owner_key: row.get(1),
            source_host_id: row.get(2),
            target_host_id: row.get(3),
            target_boot_session_id: row.get(4),
            status,
            child_created_at: row.get(6),
            created_at: row.get(7),
            updated_at: row.get(8),
        }))
    }

    pub async fn commit_fork_operation(
        &self,
        child_vm_id: Uuid,
        source_vm_id: Uuid,
        owner_key: &str,
        child_created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<(), FleetError> {
        let child_created_at = normalize_timestamp_for_postgres(child_created_at);
        let updated_at = normalize_timestamp_for_postgres(updated_at);
        let client = self.pool.get().await?;
        let changed = client
            .execute(
                "UPDATE fleet_vm_fork_operations
                 SET status = 'committed', child_created_at = $4, updated_at = $5
                 WHERE child_vm_id = $1 AND source_vm_id = $2 AND owner_key = $3
                   AND (status = 'preparing'
                        OR (status = 'committed' AND child_created_at = $4))",
                &[
                    &child_vm_id,
                    &source_vm_id,
                    &owner_key,
                    &child_created_at,
                    &updated_at,
                ],
            )
            .await?;
        if changed != 1 {
            return Err(FleetError::Conflict(format!(
                "fork child {child_vm_id} operation changed concurrently"
            )));
        }
        Ok(())
    }

    pub async fn release_vm_quota(&self, owner_key: &str, id: Uuid) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        client
            .execute(
                "DELETE FROM tenant_vm_reservations WHERE id = $1 AND owner_key = $2",
                &[&id, &owner_key],
            )
            .await?;
        Ok(())
    }

    pub async fn upsert_execution(
        &self,
        exec: &ExecutionRecord,
        owner_key: &str,
        api_key_id: &str,
        host_id: &str,
    ) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        let updated = client
            .execute(
                "INSERT INTO fleet_executions (
                   id, vm_id, owner_key, api_key_id, host_id, command, timeout_ms, status,
                   exit_code, stdout, stderr, duration_ms, error, created_at, updated_at
                 ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
                 ON CONFLICT (id) DO UPDATE SET
                   status = EXCLUDED.status,
                   exit_code = EXCLUDED.exit_code,
                   stdout = EXCLUDED.stdout,
                   stderr = EXCLUDED.stderr,
                   duration_ms = EXCLUDED.duration_ms,
                   error = EXCLUDED.error,
                   updated_at = EXCLUDED.updated_at
                 WHERE fleet_executions.owner_key = EXCLUDED.owner_key
                   AND fleet_executions.vm_id = EXCLUDED.vm_id
                   AND fleet_executions.updated_at <= EXCLUDED.updated_at",
                &[
                    &exec.id,
                    &exec.vm_id,
                    &owner_key,
                    &api_key_id,
                    &host_id,
                    &exec.command,
                    &u64_to_sql_i64(exec.timeout_ms)?,
                    &exec.status.as_str(),
                    &exec.exit_code,
                    &exec.stdout,
                    &exec.stderr,
                    &exec
                        .duration_ms
                        .map(i64::try_from)
                        .transpose()
                        .map_err(|e| {
                            FleetError::Config(format!(
                                "execution duration exceeds PostgreSQL BIGINT: {e}"
                            ))
                        })?,
                    &exec.error,
                    &exec.created_at,
                    &exec.updated_at,
                ],
            )
            .await?;
        if updated == 0 {
            return Err(FleetError::Conflict(format!(
                "execution {} was modified by another owner or a newer transition",
                exec.id
            )));
        }
        Ok(())
    }

    pub async fn get_execution(
        &self,
        id: Uuid,
    ) -> Result<Option<FleetExecutionRecord>, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT id, vm_id, owner_key, api_key_id, host_id, command, timeout_ms,
                        status, exit_code, stdout, stderr, duration_ms, error, created_at, updated_at
                   FROM fleet_executions WHERE id = $1",
                &[&id],
            )
            .await?;
        row.as_ref().map(row_to_execution).transpose()
    }

    pub async fn fail_incomplete_executions_for_host(
        &self,
        host_id: &str,
        reason: &str,
    ) -> Result<u64, FleetError> {
        let client = self.pool.get().await?;
        Ok(client
            .execute(
                "UPDATE fleet_executions
                    SET status = 'failed', error = $2, updated_at = NOW()
                  WHERE host_id = $1 AND status IN ('pending','running')",
                &[&host_id, &reason],
            )
            .await?)
    }

    pub async fn healthcheck(&self) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        client.query_one("SELECT 1", &[]).await?;
        Ok(())
    }

    pub async fn insert_share(&self, share: &ShareRecord) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        let revoked_at = share.revoked_at.as_ref().map(DateTime::to_rfc3339);
        let created_at = share.created_at.to_rfc3339();
        let updated_at = share.updated_at.to_rfc3339();
        client
            .execute(
                "INSERT INTO fleet_shares (
                   id, slug, owner_key, vm_id, guest_port, visibility, token_version, revoked_at,
                   created_at, updated_at
                 ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
                &[
                    &share.id,
                    &share.slug,
                    &share.owner_key,
                    &share.vm_id,
                    &(i32::from(share.guest_port)),
                    &share_visibility_as_str(share.visibility),
                    &u64_to_sql_i64(share.token_version)?,
                    &revoked_at,
                    &created_at,
                    &updated_at,
                ],
            )
            .await
            .map_err(fleet_error_from_postgres)?;
        Ok(())
    }

    pub async fn get_share(&self, id: Uuid) -> Result<Option<ShareRecord>, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT id, slug, owner_key, vm_id, guest_port, visibility, token_version,
                        revoked_at, created_at, updated_at
                 FROM fleet_shares WHERE id = $1",
                &[&id],
            )
            .await?;
        row.map(|row| row_to_share(&row)).transpose()
    }

    pub async fn get_share_by_slug(&self, slug: &str) -> Result<Option<ShareRecord>, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT id, slug, owner_key, vm_id, guest_port, visibility, token_version,
                        revoked_at, created_at, updated_at
                 FROM fleet_shares WHERE slug = $1",
                &[&slug],
            )
            .await?;
        row.map(|row| row_to_share(&row)).transpose()
    }

    pub async fn list_shares(&self, owner_key: &str) -> Result<Vec<ShareRecord>, FleetError> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT id, slug, owner_key, vm_id, guest_port, visibility, token_version,
                        revoked_at, created_at, updated_at
                 FROM fleet_shares WHERE owner_key = $1 ORDER BY created_at DESC",
                &[&owner_key],
            )
            .await?;
        rows.iter().map(row_to_share).collect()
    }

    pub async fn update_share(&self, share: &ShareRecord) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        let revoked_at = share.revoked_at.as_ref().map(DateTime::to_rfc3339);
        let updated_at = share.updated_at.to_rfc3339();
        let updated = client
            .execute(
                "UPDATE fleet_shares SET
                   slug = $2, vm_id = $3, guest_port = $4, visibility = $5, token_version = $6,
                   revoked_at = $7, updated_at = $8
                 WHERE id = $1",
                &[
                    &share.id,
                    &share.slug,
                    &share.vm_id,
                    &(i32::from(share.guest_port)),
                    &share_visibility_as_str(share.visibility),
                    &u64_to_sql_i64(share.token_version)?,
                    &revoked_at,
                    &updated_at,
                ],
            )
            .await
            .map_err(fleet_error_from_postgres)?;
        if updated == 0 {
            return Err(FleetError::NotFound);
        }
        Ok(())
    }

    /// Update an active share only when it still has the version read by the
    /// caller. This protects token rotation and terminal revocation from
    /// concurrent writers across taritd nodes.
    pub async fn update_share_if_current(
        &self,
        share: &ShareRecord,
        expected_token_version: u64,
    ) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        let revoked_at = share.revoked_at.as_ref().map(DateTime::to_rfc3339);
        let updated_at = share.updated_at.to_rfc3339();
        let updated = client
            .execute(
                "UPDATE fleet_shares SET
                   slug = $2, vm_id = $3, guest_port = $4, visibility = $5, token_version = $6,
                   revoked_at = $7, updated_at = $8
                  WHERE id = $1 AND token_version = $9 AND revoked_at IS NULL",
                &[
                    &share.id,
                    &share.slug,
                    &share.vm_id,
                    &(i32::from(share.guest_port)),
                    &share_visibility_as_str(share.visibility),
                    &u64_to_sql_i64(share.token_version)?,
                    &revoked_at,
                    &updated_at,
                    &u64_to_sql_i64(expected_token_version)?,
                ],
            )
            .await
            .map_err(fleet_error_from_postgres)?;
        if updated == 0 {
            return Err(FleetError::Conflict(
                "share was modified or revoked concurrently".into(),
            ));
        }
        Ok(())
    }

    /// Try to become (or renew being) the single autoscaler leader via a lease
    /// row. Succeeds if we already hold the lease or the current lease expired.
    /// Lease-based election tolerates a connection pool (unlike session advisory
    /// locks) and self-heals on leader death after `ttl_secs`.
    pub async fn try_acquire_leader(
        &self,
        node_id: &str,
        ttl_secs: i64,
    ) -> Result<bool, FleetError> {
        let client = self.pool.get().await?;
        let rows = client
            .execute(
                "INSERT INTO fleet_leader (id, leader_id, expires_at)
                 VALUES (1, $1, now() + ($2 || ' seconds')::interval)
                 ON CONFLICT (id) DO UPDATE
                   SET leader_id = EXCLUDED.leader_id, expires_at = EXCLUDED.expires_at
                   WHERE fleet_leader.leader_id = $1 OR fleet_leader.expires_at < now()",
                &[&node_id, &ttl_secs.to_string()],
            )
            .await?;
        Ok(rows > 0)
    }

    /// Append usage stats to the primary store. Idempotent: a re-sent batch is
    /// ignored via the `(vm_id, kind, window_end)` unique constraint, so the
    /// write-behind flusher can retry safely.
    pub async fn insert_usage_events(&self, events: &[UsageEvent]) -> Result<(), FleetError> {
        if events.is_empty() {
            return Ok(());
        }
        let client = self.pool.get().await?;
        for e in events {
            let kind = e.kind.as_str();
            client
                .execute(
                    "INSERT INTO usage_events
                       (id, api_key_id, owner_key, host_id, vm_id, kind, seconds, duration_ms, window_start, window_end, created_at)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
                     ON CONFLICT (vm_id, kind, window_end) DO NOTHING",
                    &[
                        &e.id,
                        &e.api_key_id,
                        &e.owner_key,
                        &e.host_id,
                        &e.vm_id,
                        &kind,
                        &e.seconds,
                        &e.duration_ms,
                        &e.window_start,
                        &e.window_end,
                        &e.created_at,
                    ],
                )
                .await?;
        }
        Ok(())
    }

    /// Append audit events to the primary store. Idempotent on the event id.
    pub async fn insert_audit_events(&self, events: &[AuditEvent]) -> Result<(), FleetError> {
        if events.is_empty() {
            return Ok(());
        }
        let client = self.pool.get().await?;
        for e in events {
            client
                .execute(
                    "INSERT INTO audit_events
                       (id, api_key_id, owner_key, host_id, vm_id, action, outcome, detail, created_at)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                     ON CONFLICT (id) DO NOTHING",
                    &[
                        &e.id,
                        &e.api_key_id,
                        &e.owner_key,
                        &e.host_id,
                        &e.vm_id,
                        &e.action,
                        &e.outcome,
                        &e.detail,
                        &e.created_at,
                    ],
                )
                .await?;
        }
        Ok(())
    }

    /// Aggregate usage stats per API key over `[from, to)`. Pass `api_key_id` to
    /// scope to one key, or `None` for every key.
    pub async fn usage_summary(
        &self,
        api_key_id: Option<&str>,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<UsageSummary>, FleetError> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT api_key_id, owner_key,
                   COALESCE(SUM(seconds) FILTER (WHERE kind='vm_runtime'), 0)::double precision AS vm_runtime_seconds,
                   COUNT(*) FILTER (WHERE kind='exec') AS exec_count,
                   COALESCE(SUM(duration_ms) FILTER (WHERE kind='exec'), 0)::bigint AS exec_duration_ms
                 FROM usage_events
                 WHERE window_end >= $2 AND window_end < $3
                   AND ($1::text IS NULL OR api_key_id = $1)
                 GROUP BY api_key_id, owner_key
                 ORDER BY api_key_id",
                &[&api_key_id, &from, &to],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| UsageSummary {
                api_key_id: r.get(0),
                owner_key: r.get(1),
                vm_runtime_seconds: r.get(2),
                exec_count: r.get(3),
                exec_duration_ms: r.get(4),
            })
            .collect())
    }

    /// List recent audit events, newest first. Optionally scope to one API key
    /// and/or one VM.
    pub async fn list_audit(
        &self,
        api_key_id: Option<&str>,
        vm_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<AuditEvent>, FleetError> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT id, api_key_id, owner_key, host_id, vm_id, action, outcome, detail, created_at
                 FROM audit_events
                 WHERE ($1::text IS NULL OR api_key_id = $1)
                   AND ($2::uuid IS NULL OR vm_id = $2)
                 ORDER BY created_at DESC
                 LIMIT $3",
                &[&api_key_id, &vm_id, &limit],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| AuditEvent {
                id: r.get(0),
                api_key_id: r.get(1),
                owner_key: r.get(2),
                host_id: r.get(3),
                vm_id: r.get(4),
                action: r.get(5),
                outcome: r.get(6),
                detail: r.get(7),
                created_at: r.get(8),
            })
            .collect())
    }
}

const FLEET_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS fleet_hosts (
  host_id TEXT PRIMARY KEY,
  boot_session_id UUID,
  peer_certificate_sha256 TEXT,
  rpc_addr TEXT,
  sandbox_count BIGINT NOT NULL DEFAULT 0,
  free_vcpus BIGINT NOT NULL DEFAULT 0,
  free_memory_mib BIGINT NOT NULL DEFAULT 0,
  healthy BOOLEAN NOT NULL DEFAULT TRUE,
  last_heartbeat TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
ALTER TABLE fleet_hosts ADD COLUMN IF NOT EXISTS boot_session_id UUID;
ALTER TABLE fleet_hosts ADD COLUMN IF NOT EXISTS peer_certificate_sha256 TEXT;
CREATE TABLE IF NOT EXISTS fleet_snapshots (
  snapshot_id UUID PRIMARY KEY,
  host_id TEXT NOT NULL,
  owner_key TEXT NOT NULL,
  snapshot_path TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS fleet_snapshots_owner ON fleet_snapshots(owner_key, created_at DESC);
CREATE TABLE IF NOT EXISTS fleet_artifacts (
  artifact_id UUID PRIMARY KEY,
  owner_key TEXT NOT NULL,
  host_id TEXT NOT NULL,
  storage_locator TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL CHECK (kind IN ('vm_snapshot','memory','disk','kernel','rootfs','agent')),
  status TEXT NOT NULL CHECK (status IN ('staging','available','deleting','corrupt')),
  content_digest TEXT NOT NULL,
  size_bytes BIGINT NOT NULL CHECK (size_bytes >= 0),
  immutable_image_digest TEXT NOT NULL,
  agent_digest TEXT NOT NULL,
  boot_manifest_digest TEXT NOT NULL,
  parent_artifact_id UUID REFERENCES fleet_artifacts(artifact_id) ON DELETE RESTRICT,
  source_vm_id UUID,
  creation_revision BIGINT NOT NULL CHECK (creation_revision > 0),
  integrity_manifest_digest TEXT NOT NULL,
  chunk_size_bytes BIGINT NOT NULL CHECK (chunk_size_bytes > 0),
  chunk_count BIGINT NOT NULL CHECK (chunk_count > 0),
  replication_state TEXT NOT NULL CHECK (replication_state IN ('pending','ready','degraded')),
  reference_count BIGINT NOT NULL DEFAULT 0 CHECK (reference_count >= 0),
  created_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL
);
ALTER TABLE fleet_artifacts ADD COLUMN IF NOT EXISTS boot_manifest_digest TEXT NOT NULL DEFAULT '';
CREATE INDEX IF NOT EXISTS fleet_artifacts_owner_created
  ON fleet_artifacts(owner_key, created_at DESC);
CREATE INDEX IF NOT EXISTS fleet_artifacts_parent
  ON fleet_artifacts(parent_artifact_id);
CREATE TABLE IF NOT EXISTS fleet_artifact_replicas (
  artifact_id UUID NOT NULL REFERENCES fleet_artifacts(artifact_id) ON DELETE CASCADE,
  owner_key TEXT NOT NULL,
  host_id TEXT NOT NULL,
  failure_domain TEXT NOT NULL,
  storage_locator TEXT NOT NULL UNIQUE,
  status TEXT NOT NULL CHECK (status IN ('staging','available','corrupt','deleting')),
  content_digest TEXT NOT NULL,
  size_bytes BIGINT NOT NULL CHECK (size_bytes >= 0),
  integrity_manifest_digest TEXT NOT NULL,
  verified_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (artifact_id, host_id)
);
CREATE INDEX IF NOT EXISTS fleet_artifact_replicas_artifact_status
  ON fleet_artifact_replicas(artifact_id, status);
CREATE INDEX IF NOT EXISTS fleet_artifact_replicas_failure_domain
  ON fleet_artifact_replicas(failure_domain, status);
CREATE TABLE IF NOT EXISTS fleet_artifact_object_replicas (
  artifact_id UUID NOT NULL REFERENCES fleet_artifacts(artifact_id) ON DELETE CASCADE,
  owner_key TEXT NOT NULL,
  provider TEXT NOT NULL,
  manifest_digest TEXT NOT NULL UNIQUE,
  manifest_size_bytes BIGINT NOT NULL CHECK (manifest_size_bytes > 0),
  status TEXT NOT NULL CHECK (status IN ('staging','available','corrupt','deleting')),
  verified_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (artifact_id, provider)
);
CREATE INDEX IF NOT EXISTS fleet_artifact_object_replicas_artifact_status
  ON fleet_artifact_object_replicas(artifact_id, status);
CREATE TABLE IF NOT EXISTS fleet_artifact_repair_leases (
  artifact_id UUID PRIMARY KEY REFERENCES fleet_artifacts(artifact_id) ON DELETE CASCADE,
  holder_host_id TEXT NOT NULL,
  holder_boot_session_id UUID NOT NULL,
  lease_token UUID NOT NULL,
  expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS fleet_artifact_repair_leases_expiry
  ON fleet_artifact_repair_leases(expires_at);
INSERT INTO fleet_artifact_replicas (
  artifact_id, owner_key, host_id, failure_domain, storage_locator, status,
  content_digest, size_bytes, integrity_manifest_digest, verified_at, created_at, updated_at
)
SELECT artifact_id, owner_key, host_id, host_id, storage_locator,
       CASE status WHEN 'available' THEN 'available' WHEN 'corrupt' THEN 'corrupt'
                   WHEN 'deleting' THEN 'deleting' ELSE 'staging' END,
       content_digest, size_bytes, integrity_manifest_digest,
       CASE WHEN status = 'available' THEN updated_at ELSE NULL END, created_at, updated_at
FROM fleet_artifacts
ON CONFLICT (artifact_id, host_id) DO NOTHING;
CREATE TABLE IF NOT EXISTS fleet_branches (
  branch_id UUID PRIMARY KEY,
  owner_key TEXT NOT NULL,
  name TEXT NOT NULL,
  head_artifact_id UUID NOT NULL REFERENCES fleet_artifacts(artifact_id) ON DELETE RESTRICT,
  source_vm_id UUID,
  source_branch_id UUID REFERENCES fleet_branches(branch_id) ON DELETE SET NULL,
  revision BIGINT NOT NULL CHECK (revision > 0),
  created_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL,
  UNIQUE(owner_key, name)
);
CREATE INDEX IF NOT EXISTS fleet_branches_owner_created
  ON fleet_branches(owner_key, created_at DESC);
CREATE INDEX IF NOT EXISTS fleet_branches_head
  ON fleet_branches(head_artifact_id);
CREATE TABLE IF NOT EXISTS fleet_vms (
  id UUID PRIMARY KEY,
  host_id TEXT NOT NULL,
  owner_key TEXT,
  api_key_id TEXT,
  status TEXT NOT NULL,
  revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
  startup_path TEXT,
  memory_mib BIGINT NOT NULL,
  vcpus SMALLINT NOT NULL,
  kernel_path TEXT NOT NULL,
  rootfs_path TEXT,
  rootfs_read_only BOOLEAN NOT NULL DEFAULT FALSE,
  cmdline TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL,
  generation BIGINT NOT NULL DEFAULT 1 CHECK (generation > 0)
);
CREATE TABLE IF NOT EXISTS fleet_vm_fork_operations (
  child_vm_id UUID PRIMARY KEY,
  source_vm_id UUID NOT NULL,
  owner_key TEXT NOT NULL,
  source_host_id TEXT NOT NULL,
  target_host_id TEXT NOT NULL,
  target_boot_session_id UUID,
  status TEXT NOT NULL CHECK (status IN ('preparing','committed')),
  child_created_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS fleet_vm_fork_operations_source
  ON fleet_vm_fork_operations(owner_key, source_vm_id);
CREATE TABLE IF NOT EXISTS fleet_hibernations (
  vm_id UUID PRIMARY KEY REFERENCES fleet_vms(id) ON DELETE CASCADE,
  owner_key TEXT NOT NULL,
  artifact_id UUID NOT NULL REFERENCES fleet_artifacts(artifact_id) ON DELETE RESTRICT,
  policy_revision BIGINT NOT NULL CHECK (policy_revision > 0),
  allowlist_json TEXT NOT NULL,
  allow_existing BOOLEAN NOT NULL,
  policy_created_at TIMESTAMPTZ NOT NULL,
  policy_updated_at TIMESTAMPTZ NOT NULL,
  created_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS fleet_hibernations_owner
  ON fleet_hibernations(owner_key, updated_at DESC);
CREATE INDEX IF NOT EXISTS fleet_hibernations_artifact
  ON fleet_hibernations(artifact_id);
CREATE TABLE IF NOT EXISTS fleet_volumes (
  id UUID PRIMARY KEY,
  owner_key TEXT NOT NULL,
  name TEXT NOT NULL,
  provider TEXT NOT NULL,
  storage_class TEXT NOT NULL CHECK (storage_class IN ('block','filesystem','object')),
  size_bytes BIGINT NOT NULL CHECK (size_bytes > 0),
  status TEXT NOT NULL CHECK (status IN ('creating','available','deleting','error')),
  read_only_many BOOLEAN NOT NULL,
  read_write_once BOOLEAN NOT NULL,
  read_write_many BOOLEAN NOT NULL,
  snapshots BOOLEAN NOT NULL,
  clones BOOLEAN NOT NULL,
  host_id TEXT,
  region TEXT,
  zone TEXT,
  generation BIGINT NOT NULL CHECK (generation > 0),
  revision BIGINT NOT NULL CHECK (revision > 0),
  last_error TEXT,
  created_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL,
  UNIQUE(owner_key, name)
);
CREATE INDEX IF NOT EXISTS fleet_volumes_owner_created
  ON fleet_volumes(owner_key, created_at, id);
CREATE TABLE IF NOT EXISTS fleet_vm_volume_attachments (
  vm_id UUID NOT NULL REFERENCES fleet_vms(id) ON DELETE CASCADE,
  volume_id UUID NOT NULL REFERENCES fleet_volumes(id) ON DELETE RESTRICT,
  device_index SMALLINT NOT NULL CHECK (device_index BETWEEN 0 AND 14),
  owner_key TEXT NOT NULL,
  mode TEXT NOT NULL CHECK (mode IN ('read_only','read_write')),
  volume_generation BIGINT NOT NULL CHECK (volume_generation > 0),
  created_at TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (vm_id, volume_id),
  UNIQUE (vm_id, device_index)
);
DROP INDEX IF EXISTS fleet_volume_single_writer;
CREATE INDEX IF NOT EXISTS fleet_volume_writer_lookup
  ON fleet_vm_volume_attachments(volume_id) WHERE mode = 'read_write';
CREATE INDEX IF NOT EXISTS fleet_volume_attachments_owner_volume
  ON fleet_vm_volume_attachments(owner_key, volume_id);
CREATE TABLE IF NOT EXISTS fleet_vm_artifact_refs (
  vm_id UUID PRIMARY KEY,
  owner_key TEXT NOT NULL,
  artifact_id UUID NOT NULL REFERENCES fleet_artifacts(artifact_id) ON DELETE RESTRICT,
  vm_created_at TIMESTAMPTZ NOT NULL,
  created_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS fleet_vm_artifact_refs_artifact
  ON fleet_vm_artifact_refs(artifact_id);
CREATE TABLE IF NOT EXISTS fleet_leader (
  id INT PRIMARY KEY,
  leader_id TEXT NOT NULL,
  expires_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE IF NOT EXISTS fleet_shares (
  id UUID PRIMARY KEY,
  slug TEXT NOT NULL UNIQUE,
  owner_key TEXT NOT NULL,
  vm_id UUID NOT NULL,
  guest_port INTEGER NOT NULL CHECK (guest_port BETWEEN 1 AND 65535),
  visibility TEXT NOT NULL CHECK (visibility IN ('public', 'private')),
  token_version BIGINT NOT NULL DEFAULT 0,
  revoked_at TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS fleet_shares_owner ON fleet_shares (owner_key, created_at DESC);
CREATE INDEX IF NOT EXISTS fleet_shares_vm ON fleet_shares (vm_id);
CREATE TABLE IF NOT EXISTS usage_events (
  id UUID PRIMARY KEY,
  api_key_id TEXT NOT NULL,
  owner_key TEXT NOT NULL,
  host_id TEXT NOT NULL,
  vm_id UUID NOT NULL,
  kind TEXT NOT NULL,
  seconds DOUBLE PRECISION,
  duration_ms BIGINT,
  window_start TIMESTAMPTZ NOT NULL,
  window_end TIMESTAMPTZ NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  CONSTRAINT usage_events_dedupe UNIQUE (vm_id, kind, window_end)
);
CREATE INDEX IF NOT EXISTS usage_events_key_time ON usage_events (api_key_id, window_end);
CREATE TABLE IF NOT EXISTS audit_events (
  id UUID PRIMARY KEY,
  api_key_id TEXT NOT NULL,
  owner_key TEXT NOT NULL,
  host_id TEXT NOT NULL,
  vm_id UUID,
  action TEXT NOT NULL,
  outcome TEXT NOT NULL,
  detail TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS audit_events_key_time ON audit_events (api_key_id, created_at DESC);
CREATE INDEX IF NOT EXISTS audit_events_vm ON audit_events (vm_id);
CREATE TABLE IF NOT EXISTS tenant_vm_reservations (
  id UUID PRIMARY KEY,
  owner_key TEXT NOT NULL,
  expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS tenant_vm_reservations_owner_expiry
  ON tenant_vm_reservations (owner_key, expires_at);
CREATE TABLE IF NOT EXISTS fleet_executions (
  id UUID PRIMARY KEY,
  vm_id UUID NOT NULL,
  owner_key TEXT NOT NULL,
  api_key_id TEXT NOT NULL,
  host_id TEXT NOT NULL,
  command TEXT NOT NULL,
  timeout_ms BIGINT NOT NULL CHECK (timeout_ms >= 0),
  status TEXT NOT NULL CHECK (status IN ('pending','running','completed','failed')),
  exit_code INTEGER,
  stdout TEXT,
  stderr TEXT,
  duration_ms BIGINT,
  error TEXT,
  created_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS fleet_executions_owner_time
  ON fleet_executions (owner_key, created_at DESC);
CREATE INDEX IF NOT EXISTS fleet_executions_host_status
  ON fleet_executions (host_id, status);
";

const VOLUME_COLUMNS: &str = "id, owner_key, name, provider, storage_class, size_bytes, status,
  read_only_many, read_write_once, read_write_many, snapshots, clones,
  host_id, region, zone, generation, revision, last_error, created_at, updated_at";

fn row_to_volume(row: &tokio_postgres::Row) -> Result<VolumeRecord, FleetError> {
    let storage_class: String = row.get(4);
    let status: String = row.get(6);
    Ok(VolumeRecord {
        id: row.get(0),
        owner_key: row.get(1),
        name: row.get(2),
        provider: row.get(3),
        storage_class: VolumeStorageClass::parse(&storage_class).ok_or_else(|| {
            FleetError::Config(format!("invalid volume storage class: {storage_class}"))
        })?,
        size_bytes: u64::try_from(row.get::<_, i64>(5))
            .map_err(|_| FleetError::Config("negative volume size".into()))?,
        status: VolumeStatus::parse(&status)
            .ok_or_else(|| FleetError::Config(format!("invalid volume status: {status}")))?,
        capabilities: VolumeCapabilities {
            read_only_many: row.get(7),
            read_write_once: row.get(8),
            read_write_many: row.get(9),
            snapshots: row.get(10),
            clones: row.get(11),
        },
        host_id: row.get(12),
        region: row.get(13),
        zone: row.get(14),
        generation: u64::try_from(row.get::<_, i64>(15))
            .map_err(|_| FleetError::Config("invalid volume generation".into()))?,
        revision: u64::try_from(row.get::<_, i64>(16))
            .map_err(|_| FleetError::Config("invalid volume revision".into()))?,
        last_error: row.get(17),
        created_at: row.get(18),
        updated_at: row.get(19),
    })
}

fn same_immutable_volume(left: &VolumeRecord, right: &VolumeRecord) -> bool {
    left.id == right.id
        && left.owner_key == right.owner_key
        && left.name == right.name
        && left.provider == right.provider
        && left.storage_class == right.storage_class
        && left.size_bytes == right.size_bytes
        && left.capabilities == right.capabilities
        && left.host_id == right.host_id
        && left.region == right.region
        && left.zone == right.zone
        && left.generation == right.generation
}

fn row_to_artifact(row: &tokio_postgres::Row) -> Result<ArtifactRecord, FleetError> {
    let kind: String = row.get(4);
    let status: String = row.get(5);
    let replication: String = row.get(17);
    Ok(ArtifactRecord {
        artifact_id: row.get(0),
        owner_key: row.get(1),
        host_id: row.get(2),
        storage_locator: row.get(3),
        kind: ArtifactKind::parse(&kind)
            .ok_or_else(|| FleetError::Config(format!("invalid artifact kind: {kind}")))?,
        status: ArtifactStatus::parse(&status)
            .ok_or_else(|| FleetError::Config(format!("invalid artifact status: {status}")))?,
        content_digest: row.get(6),
        size_bytes: u64::try_from(row.get::<_, i64>(7))
            .map_err(|_| FleetError::Config("negative artifact size".into()))?,
        immutable_image_digest: row.get(8),
        agent_digest: row.get(9),
        boot_manifest_digest: row.get(10),
        parent_artifact_id: row.get(11),
        source_vm_id: row.get(12),
        creation_revision: u64::try_from(row.get::<_, i64>(13))
            .map_err(|_| FleetError::Config("invalid artifact revision".into()))?,
        integrity_manifest_digest: row.get(14),
        chunk_size_bytes: u64::try_from(row.get::<_, i64>(15))
            .map_err(|_| FleetError::Config("invalid artifact chunk size".into()))?,
        chunk_count: u64::try_from(row.get::<_, i64>(16))
            .map_err(|_| FleetError::Config("invalid artifact chunk count".into()))?,
        replication_state: ArtifactReplicationState::parse(&replication).ok_or_else(|| {
            FleetError::Config(format!("invalid artifact replication state: {replication}"))
        })?,
        reference_count: u64::try_from(row.get::<_, i64>(18))
            .map_err(|_| FleetError::Config("negative artifact reference count".into()))?,
        created_at: row.get(19),
        updated_at: row.get(20),
    })
}

fn row_to_artifact_replica(row: &tokio_postgres::Row) -> Result<ArtifactReplicaRecord, FleetError> {
    let status: String = row.get(5);
    Ok(ArtifactReplicaRecord {
        artifact_id: row.get(0),
        owner_key: row.get(1),
        host_id: row.get(2),
        failure_domain: row.get(3),
        storage_locator: row.get(4),
        status: ArtifactReplicaStatus::parse(&status)
            .ok_or_else(|| FleetError::Config(format!("invalid replica status: {status}")))?,
        content_digest: row.get(6),
        size_bytes: u64::try_from(row.get::<_, i64>(7))
            .map_err(|_| FleetError::Config("negative replica size".into()))?,
        integrity_manifest_digest: row.get(8),
        verified_at: row.get(9),
        created_at: row.get(10),
        updated_at: row.get(11),
    })
}

fn row_to_artifact_object_replica(
    row: &tokio_postgres::Row,
) -> Result<ArtifactObjectReplicaRecord, FleetError> {
    let status: String = row.get(5);
    Ok(ArtifactObjectReplicaRecord {
        artifact_id: row.get(0),
        owner_key: row.get(1),
        provider: row.get(2),
        manifest_digest: row.get(3),
        manifest_size_bytes: u64::try_from(row.get::<_, i64>(4))
            .map_err(|_| FleetError::Config("invalid object replica manifest size".into()))?,
        status: ArtifactReplicaStatus::parse(&status).ok_or_else(|| {
            FleetError::Config(format!("invalid object replica status: {status}"))
        })?,
        verified_at: row.get(6),
        created_at: row.get(7),
        updated_at: row.get(8),
    })
}

fn same_immutable_artifact(left: &ArtifactRecord, right: &ArtifactRecord) -> bool {
    left.artifact_id == right.artifact_id
        && left.owner_key == right.owner_key
        && left.host_id == right.host_id
        && left.storage_locator == right.storage_locator
        && left.kind == right.kind
        && left.content_digest == right.content_digest
        && left.size_bytes == right.size_bytes
        && left.immutable_image_digest == right.immutable_image_digest
        && left.agent_digest == right.agent_digest
        && left.boot_manifest_digest == right.boot_manifest_digest
        && left.parent_artifact_id == right.parent_artifact_id
        && left.source_vm_id == right.source_vm_id
        && left.creation_revision == right.creation_revision
        && left.integrity_manifest_digest == right.integrity_manifest_digest
        && left.chunk_size_bytes == right.chunk_size_bytes
        && left.chunk_count == right.chunk_count
}

fn row_to_branch(row: &tokio_postgres::Row) -> Result<BranchRecord, FleetError> {
    Ok(BranchRecord {
        branch_id: row.get(0),
        owner_key: row.get(1),
        name: row.get(2),
        head_artifact_id: row.get(3),
        source_vm_id: row.get(4),
        source_branch_id: row.get(5),
        revision: u64::try_from(row.get::<_, i64>(6))
            .map_err(|_| FleetError::Config("invalid branch revision".into()))?,
        created_at: row.get(7),
        updated_at: row.get(8),
    })
}

fn row_to_hibernation(row: &tokio_postgres::Row) -> Result<FleetHibernationRecord, FleetError> {
    let vm_id: Uuid = row.get(0);
    let owner_key: String = row.get(1);
    let revision = u64::try_from(row.get::<_, i64>(3))
        .map_err(|_| FleetError::Config("negative hibernation policy revision".into()))?;
    let allowlist = serde_json::from_str::<Vec<String>>(row.get::<_, &str>(4))
        .map_err(|error| FleetError::Config(format!("decode hibernation egress: {error}")))?;
    Ok(FleetHibernationRecord {
        vm_id,
        owner_key: owner_key.clone(),
        artifact_id: row.get(2),
        egress_policy: EgressPolicyRecord {
            vm_id,
            owner_key,
            revision,
            allowlist,
            allow_existing: row.get(5),
            created_at: row.get(6),
            updated_at: row.get(7),
        },
        created_at: row.get(8),
        updated_at: row.get(9),
    })
}

fn row_to_vm(row: &tokio_postgres::Row) -> Result<VmRecord, FleetError> {
    let status: String = row.get(4);
    let revision = u64::try_from(row.get::<_, i64>(5))
        .map_err(|_| FleetError::Config("negative VM revision in fleet row".into()))?;
    let startup_path: Option<String> = row.get(6);
    let memory_mib = u64::try_from(row.get::<_, i64>(7))
        .map_err(|_| FleetError::Config("negative VM memory in fleet row".into()))?;
    let vcpus = u8::try_from(row.get::<_, i16>(8))
        .map_err(|_| FleetError::Config("invalid VM vCPU count in fleet row".into()))?;
    Ok(VmRecord {
        id: row.get(0),
        host_id: row.get(1),
        owner_key: row.get(2),
        api_key_id: row.get(3),
        status: VmStatus::parse(&status).ok_or_else(|| {
            FleetError::Config(format!("invalid VM status in fleet row: {status}"))
        })?,
        revision,
        startup_path: startup_path.as_deref().and_then(VmStartupPath::parse),
        memory_mib,
        vcpus,
        kernel_path: row.get(9),
        rootfs_path: row.get(10),
        rootfs_read_only: row.get(11),
        cmdline: row.get(12),
        runtime_layout: None,
        // Process handles are node-local and must never be reconstructed from
        // the global ownership index.
        socket_path: None,
        pid: None,
        created_at: row.get(13),
        updated_at: row.get(14),
    })
}

fn normalize_vm_timestamps_for_postgres(vm: &VmRecord) -> VmRecord {
    let mut normalized = vm.clone();
    normalized.created_at = normalize_timestamp_for_postgres(normalized.created_at);
    normalized.updated_at = normalize_timestamp_for_postgres(normalized.updated_at);
    normalized
}

fn normalize_branch_timestamps_for_postgres(branch: &BranchRecord) -> BranchRecord {
    let mut normalized = branch.clone();
    normalized.created_at = normalize_timestamp_for_postgres(normalized.created_at);
    normalized.updated_at = normalize_timestamp_for_postgres(normalized.updated_at);
    normalized
}

fn normalize_timestamp_for_postgres(timestamp: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp(
        timestamp.timestamp(),
        timestamp.timestamp_subsec_micros() * 1_000,
    )
    .expect("normalizing a valid chrono timestamp preserves its range")
}

async fn release_vm_artifact_ref_tx(
    tx: &tokio_postgres::Transaction<'_>,
    vm: &VmRecord,
) -> Result<(), FleetError> {
    let Some(row) = tx
        .query_opt(
            "SELECT owner_key, artifact_id, vm_created_at
               FROM fleet_vm_artifact_refs WHERE vm_id = $1 FOR UPDATE",
            &[&vm.id],
        )
        .await?
    else {
        return Ok(());
    };
    let owner_key: String = row.get(0);
    let artifact_id: Uuid = row.get(1);
    let created_at: DateTime<Utc> = row.get(2);
    if vm.owner_key.as_deref() != Some(owner_key.as_str()) || vm.created_at != created_at {
        return Err(FleetError::Conflict(
            "refusing stale VM artifact reference release".into(),
        ));
    }
    tx.execute(
        "DELETE FROM fleet_vm_artifact_refs
          WHERE vm_id = $1 AND owner_key = $2 AND vm_created_at = $3",
        &[&vm.id, &owner_key, &created_at],
    )
    .await?;
    let released = tx
        .execute(
            "UPDATE fleet_artifacts
                SET reference_count = reference_count - 1, updated_at = NOW()
              WHERE artifact_id = $1 AND owner_key = $2 AND reference_count > 0",
            &[&artifact_id, &owner_key],
        )
        .await?;
    if released != 1 {
        return Err(FleetError::Conflict(
            "VM artifact reference count is missing".into(),
        ));
    }
    delete_unreferenced_artifact_chain_tx(tx, &owner_key, artifact_id).await?;
    Ok(())
}

async fn delete_unreferenced_artifact_chain_tx(
    tx: &tokio_postgres::Transaction<'_>,
    owner_key: &str,
    artifact_id: Uuid,
) -> Result<(), FleetError> {
    let mut candidate = Some(artifact_id);
    while let Some(current_id) = candidate.take() {
        let Some(row) = tx
            .query_opt(
                "SELECT parent_artifact_id, reference_count
                   FROM fleet_artifacts
                  WHERE artifact_id = $1 AND owner_key = $2 FOR UPDATE",
                &[&current_id, &owner_key],
            )
            .await?
        else {
            break;
        };
        if row.get::<_, i64>(1) != 0 {
            break;
        }
        let parent_id: Option<Uuid> = row.get(0);
        tx.execute(
            "DELETE FROM fleet_artifacts
              WHERE artifact_id = $1 AND owner_key = $2 AND reference_count = 0",
            &[&current_id, &owner_key],
        )
        .await
        .map_err(fleet_error_from_postgres)?;
        if let Some(parent_id) = parent_id {
            let released = tx
                .execute(
                    "UPDATE fleet_artifacts
                        SET reference_count = reference_count - 1, updated_at = NOW()
                      WHERE artifact_id = $1 AND owner_key = $2 AND reference_count > 0",
                    &[&parent_id, &owner_key],
                )
                .await?;
            if released != 1 {
                return Err(FleetError::Conflict(
                    "parent artifact has no reference to release".into(),
                ));
            }
            candidate = Some(parent_id);
        }
    }
    Ok(())
}

fn row_to_execution(row: &tokio_postgres::Row) -> Result<FleetExecutionRecord, FleetError> {
    let timeout_ms = u64::try_from(row.get::<_, i64>(6))
        .map_err(|_| FleetError::Config("negative execution timeout in fleet row".into()))?;
    let status: String = row.get(7);
    let duration_ms = row
        .get::<_, Option<i64>>(11)
        .map(u64::try_from)
        .transpose()
        .map_err(|_| FleetError::Config("negative execution duration in fleet row".into()))?;
    Ok(FleetExecutionRecord {
        record: ExecutionRecord {
            id: row.get(0),
            vm_id: row.get(1),
            command: row.get(5),
            timeout_ms,
            status: ExecutionStatus::parse(&status).ok_or_else(|| {
                FleetError::Config(format!("invalid execution status in fleet row: {status}"))
            })?,
            exit_code: row.get(8),
            stdout: row.get(9),
            stderr: row.get(10),
            duration_ms,
            error: row.get(12),
            created_at: row.get(13),
            updated_at: row.get(14),
        },
        owner_key: row.get(2),
        api_key_id: row.get(3),
        host_id: row.get(4),
    })
}

fn share_visibility_as_str(visibility: ShareVisibility) -> &'static str {
    match visibility {
        ShareVisibility::Public => "public",
        ShareVisibility::Private => "private",
    }
}

fn row_to_share(row: &tokio_postgres::Row) -> Result<ShareRecord, FleetError> {
    let id: Uuid = share_column(row, 0, "id")?;
    let slug: String = share_column(row, 1, "slug")?;
    let owner_key: String = share_column(row, 2, "owner_key")?;
    let vm_id: Uuid = share_column(row, 3, "vm_id")?;
    let guest_port: i32 = share_column(row, 4, "guest_port")?;
    let visibility: String = share_column(row, 5, "visibility")?;
    let token_version: i64 = share_column(row, 6, "token_version")?;
    let revoked_at: Option<String> = share_column(row, 7, "revoked_at")?;
    let created_at: String = share_column(row, 8, "created_at")?;
    let updated_at: String = share_column(row, 9, "updated_at")?;
    Ok(ShareRecord {
        id,
        slug,
        owner_key,
        vm_id,
        guest_port: u16::try_from(guest_port)
            .map_err(|_| FleetError::InvalidShareRow("invalid guest_port".into()))?,
        visibility: match visibility.as_str() {
            "public" => ShareVisibility::Public,
            "private" => ShareVisibility::Private,
            _ => return Err(FleetError::InvalidShareRow("invalid visibility".into())),
        },
        token_version: u64::try_from(token_version)
            .map_err(|_| FleetError::InvalidShareRow("invalid token_version".into()))?,
        revoked_at: revoked_at
            .as_deref()
            .map(|value| parse_share_timestamp("revoked_at", value))
            .transpose()?,
        created_at: parse_share_timestamp("created_at", &created_at)?,
        updated_at: parse_share_timestamp("updated_at", &updated_at)?,
    })
}

fn share_column<T>(row: &tokio_postgres::Row, index: usize, name: &str) -> Result<T, FleetError>
where
    for<'a> T: tokio_postgres::types::FromSql<'a>,
{
    row.try_get(index)
        .map_err(|error| FleetError::InvalidShareRow(format!("{name}: {error}")))
}

fn parse_share_timestamp(column: &str, value: &str) -> Result<DateTime<Utc>, FleetError> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| {
            FleetError::InvalidShareRow(format!("invalid {column} timestamp: {error}"))
        })
}

fn u64_to_sql_i64(value: u64) -> Result<i64, FleetError> {
    i64::try_from(value).map_err(|_| FleetError::Config("share token version is too large".into()))
}

fn env_positive_u64(name: &str, default: u64) -> Result<u64, FleetError> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| FleetError::Config(format!("{name} must be a positive integer"))),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(FleetError::Config(format!("read {name}: {error}"))),
    }
}

fn fleet_error_from_postgres(error: tokio_postgres::Error) -> FleetError {
    if error.code().is_some_and(|code| {
        code == &tokio_postgres::error::SqlState::UNIQUE_VIOLATION
            || code == &tokio_postgres::error::SqlState::FOREIGN_KEY_VIOLATION
            || code == &tokio_postgres::error::SqlState::CHECK_VIOLATION
    }) {
        FleetError::Conflict(error.to_string())
    } else {
        FleetError::Postgres(error)
    }
}

fn make_rustls_connector() -> Result<MakeRustlsConnect, FleetError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    if let Ok(path) = std::env::var("TARIT_RDS_CA_FILE") {
        if !path.is_empty() {
            let extra =
                rustls_native_certs::load_certs_from_paths(Some(std::path::Path::new(&path)), None);
            for cert in extra.certs {
                let _ = roots.add(cert);
            }
        }
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

/// Pull peer roster from Postgres into local SQLite cache.
pub async fn sync_peers_from_postgres(
    fleet: &PostgresFleet,
    local_store: &tarit_store::Store,
) -> Result<(), FleetError> {
    for host in fleet.list_hosts().await? {
        local_store
            .upsert_host(&host)
            .map_err(|e| FleetError::Config(format!("local store: {e}")))?;
    }
    Ok(())
}

/// Push local host heartbeat to Postgres.
pub async fn heartbeat_local_host(
    fleet: &PostgresFleet,
    host: HostRecord,
) -> Result<(), FleetError> {
    fleet.upsert_host(&host).await
}

/// Mark stale peers unhealthy (optional housekeeping).
pub async fn touch_vm_in_fleet(fleet: &PostgresFleet, vm: &VmRecord) -> Result<(), FleetError> {
    fleet.upsert_vm(vm).await.map(|_| ())
}

/// Build a host record for heartbeat from scheduler state.
pub fn host_record_from_capacity(
    host_id: &str,
    boot_session_id: Uuid,
    peer_certificate_sha256: Option<String>,
    rpc_addr: Option<String>,
    sandbox_count: usize,
    free_vcpus: u64,
    free_memory_mib: u64,
) -> HostRecord {
    HostRecord {
        host_id: host_id.to_string(),
        boot_session_id: Some(boot_session_id),
        peer_certificate_sha256,
        rpc_addr,
        sandbox_count,
        free_vcpus,
        free_memory_mib,
        healthy: true,
        last_heartbeat: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tarit_types::ShareRecord;

    // Each configured integration test initializes the same schema. Concurrent
    // PostgreSQL DDL can deadlock even though test rows use unique UUIDs, so
    // serialize only these external-DB cases.
    static POSTGRES_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn test_vm(id: Uuid, host_id: &str, owner_key: &str) -> VmRecord {
        let now = Utc::now();
        VmRecord {
            id,
            host_id: host_id.into(),
            owner_key: Some(owner_key.into()),
            api_key_id: Some("key-id".into()),
            status: VmStatus::Creating,
            revision: 1,
            startup_path: None,
            memory_mib: 256,
            vcpus: 1,
            kernel_path: "/opt/tarit/vmlinux".into(),
            rootfs_path: Some("/opt/tarit/rootfs.ext4".into()),
            rootfs_read_only: true,
            cmdline: "console=ttyS0".into(),
            runtime_layout: None,
            socket_path: None,
            pid: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn test_artifact(owner_key: &str, host_id: &str, suffix: Uuid) -> ArtifactRecord {
        let now = Utc::now();
        ArtifactRecord {
            artifact_id: Uuid::new_v4(),
            owner_key: owner_key.into(),
            host_id: host_id.into(),
            storage_locator: format!("/private/{suffix}/{}", Uuid::new_v4()),
            kind: ArtifactKind::VmSnapshot,
            status: ArtifactStatus::Available,
            content_digest: format!("sha256:{}", Uuid::new_v4()),
            size_bytes: 8192,
            immutable_image_digest: "sha256:image".into(),
            agent_digest: "sha256:agent".into(),
            boot_manifest_digest: "sha256:boot".into(),
            parent_artifact_id: None,
            source_vm_id: None,
            creation_revision: 1,
            integrity_manifest_digest: format!("sha256:{}", Uuid::new_v4()),
            chunk_size_bytes: 4096,
            chunk_count: 2,
            replication_state: ArtifactReplicationState::Ready,
            reference_count: 0,
            created_at: now,
            updated_at: now,
        }
    }

    fn test_volume(owner_key: &str, host_id: &str, suffix: Uuid) -> VolumeRecord {
        let now = Utc::now();
        VolumeRecord {
            id: Uuid::new_v4(),
            owner_key: owner_key.into(),
            name: format!("volume-{suffix}"),
            provider: "local_block".into(),
            storage_class: VolumeStorageClass::Block,
            size_bytes: 64 * 1024 * 1024,
            status: VolumeStatus::Creating,
            capabilities: VolumeCapabilities {
                read_only_many: true,
                read_write_once: true,
                read_write_many: false,
                snapshots: false,
                clones: false,
            },
            host_id: Some(host_id.into()),
            region: Some("test-region".into()),
            zone: Some("test-zone-a".into()),
            generation: 1,
            revision: 1,
            last_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn test_share(slug: String, owner_key: &str) -> ShareRecord {
        let now = chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).unwrap();
        ShareRecord {
            id: Uuid::new_v4(),
            slug,
            owner_key: owner_key.into(),
            vm_id: Uuid::new_v4(),
            guest_port: 8080,
            visibility: ShareVisibility::Private,
            token_version: 2,
            revoked_at: Some(chrono::DateTime::from_timestamp(1_700_000_001, 987_654_321).unwrap()),
            created_at: now,
            updated_at: now,
        }
    }

    fn assert_share_eq(actual: &ShareRecord, expected: &ShareRecord) -> Result<(), FleetError> {
        if actual.id == expected.id
            && actual.slug == expected.slug
            && actual.owner_key == expected.owner_key
            && actual.vm_id == expected.vm_id
            && actual.guest_port == expected.guest_port
            && actual.visibility == expected.visibility
            && actual.token_version == expected.token_version
            && actual.revoked_at == expected.revoked_at
            && actual.created_at == expected.created_at
            && actual.updated_at == expected.updated_at
        {
            Ok(())
        } else {
            Err(FleetError::Config("share round-trip mismatch".into()))
        }
    }

    async fn cleanup_test_shares(fleet: &PostgresFleet, ids: &[Uuid]) -> Result<(), FleetError> {
        let client = fleet.pool.get().await?;
        for id in ids {
            client
                .execute("DELETE FROM fleet_shares WHERE id = $1", &[id])
                .await?;
        }
        Ok(())
    }

    #[test]
    fn fleet_schema_defines_share_constraints() {
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_shares"));
        assert!(FLEET_SCHEMA.contains("slug TEXT NOT NULL UNIQUE"));
        assert!(FLEET_SCHEMA.contains("guest_port BETWEEN 1 AND 65535"));
        assert!(FLEET_SCHEMA.contains("visibility IN ('public', 'private')"));
        assert!(FLEET_SCHEMA
            .contains("revoked_at TEXT,\n  created_at TEXT NOT NULL,\n  updated_at TEXT NOT NULL"));
    }

    #[test]
    fn fleet_schema_defines_fencing_quota_and_global_operations() {
        assert!(FLEET_SCHEMA.contains("boot_session_id UUID"));
        assert!(FLEET_SCHEMA.contains("peer_certificate_sha256 TEXT"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_snapshots"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_artifact_object_replicas"));
        assert!(FLEET_SCHEMA.contains("snapshot_id UUID PRIMARY KEY"));
        assert!(FLEET_SCHEMA.contains("generation BIGINT NOT NULL DEFAULT 1"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS tenant_vm_reservations"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_vm_fork_operations"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_executions"));
        assert!(FLEET_SCHEMA.contains("fleet_executions_host_status"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_artifacts"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_artifact_replicas"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_artifact_repair_leases"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_vm_artifact_refs"));
        assert!(FLEET_SCHEMA.contains("vm_created_at TIMESTAMPTZ NOT NULL"));
        assert!(FLEET_SCHEMA.contains("holder_boot_session_id UUID NOT NULL"));
        assert!(FLEET_SCHEMA.contains("lease_token UUID NOT NULL"));
        assert!(FLEET_SCHEMA.contains("PRIMARY KEY (artifact_id, host_id)"));
        assert!(FLEET_SCHEMA.contains("failure_domain TEXT NOT NULL"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_branches"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_volumes"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_vm_volume_attachments"));
        assert!(FLEET_SCHEMA.contains("fleet_volume_writer_lookup"));
    }

    #[tokio::test]
    async fn fork_operation_is_source_bound_and_recoverable_in_postgres() -> Result<(), FleetError>
    {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            eprintln!("skipping PostgreSQL fork operation test: TARIT_TEST_DATABASE_URL is absent");
            return Ok(());
        };
        if database_url.is_empty() {
            return Ok(());
        }
        let _database_guard = POSTGRES_TEST_LOCK.lock().await;
        let fleet = PostgresFleet::connect(&database_url).await?;
        let now = Utc::now();
        let operation = ForkOperationRecord {
            child_vm_id: Uuid::new_v4(),
            source_vm_id: Uuid::new_v4(),
            owner_key: format!("fork-owner-{}", Uuid::new_v4()),
            source_host_id: "source-host".into(),
            target_host_id: "target-host".into(),
            target_boot_session_id: Some(Uuid::new_v4()),
            status: ForkOperationStatus::Preparing,
            child_created_at: None,
            created_at: now,
            updated_at: now,
        };
        let expiry = now + chrono::Duration::minutes(1);
        let result = async {
            assert_eq!(
                fleet.claim_fork_operation(&operation, 2, expiry).await?,
                ForkOperationClaimOutcome::New
            );
            assert_eq!(
                fleet.claim_fork_operation(&operation, 2, expiry).await?,
                ForkOperationClaimOutcome::InProgress
            );
            let restarted_operation = ForkOperationRecord {
                target_boot_session_id: Some(Uuid::new_v4()),
                updated_at: now + chrono::Duration::seconds(1),
                ..operation.clone()
            };
            assert_eq!(
                fleet
                    .claim_fork_operation(&restarted_operation, 2, expiry)
                    .await?,
                ForkOperationClaimOutcome::Resumed
            );
            assert_eq!(
                fleet
                    .claim_fork_operation(&restarted_operation, 2, expiry)
                    .await?,
                ForkOperationClaimOutcome::InProgress
            );
            assert_eq!(
                fleet
                    .get_fork_operation(operation.child_vm_id)
                    .await?
                    .expect("fork operation")
                    .target_boot_session_id,
                restarted_operation.target_boot_session_id
            );
            let wrong_source = ForkOperationRecord {
                source_vm_id: Uuid::new_v4(),
                ..operation.clone()
            };
            assert!(matches!(
                fleet.claim_fork_operation(&wrong_source, 2, expiry).await,
                Err(FleetError::Conflict(_))
            ));

            fleet
                .release_vm_quota(&operation.owner_key, operation.child_vm_id)
                .await?;
            assert_eq!(
                fleet.claim_fork_operation(&operation, 2, expiry).await?,
                ForkOperationClaimOutcome::Resumed
            );
            let child_created_at = Utc::now();
            fleet
                .commit_fork_operation(
                    operation.child_vm_id,
                    operation.source_vm_id,
                    &operation.owner_key,
                    child_created_at,
                    Utc::now(),
                )
                .await?;
            let committed = fleet
                .get_fork_operation(operation.child_vm_id)
                .await?
                .ok_or_else(|| FleetError::Config("committed fork operation is missing".into()))?;
            assert_eq!(committed.status, ForkOperationStatus::Committed);
            assert_eq!(committed.source_vm_id, operation.source_vm_id);
            assert_eq!(
                committed.child_created_at,
                Some(normalize_timestamp_for_postgres(child_created_at))
            );

            fleet
                .release_vm_quota(&operation.owner_key, operation.child_vm_id)
                .await?;
            assert!(matches!(
                fleet
                    .reserve_vm_quota(
                        &operation.owner_key,
                        operation.child_vm_id,
                        usize::MAX,
                        expiry,
                    )
                    .await,
                Err(FleetError::Conflict(_))
            ));
            Ok::<(), FleetError>(())
        }
        .await;

        let client = fleet.pool.get().await?;
        client
            .execute(
                "DELETE FROM tenant_vm_reservations WHERE id = $1",
                &[&operation.child_vm_id],
            )
            .await?;
        client
            .execute(
                "DELETE FROM fleet_vm_fork_operations WHERE child_vm_id = $1",
                &[&operation.child_vm_id],
            )
            .await?;
        result
    }

    #[tokio::test]
    async fn fleet_volumes_are_tenant_scoped_cas_fenced_and_single_writer() -> Result<(), FleetError>
    {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            eprintln!("skipping PostgreSQL volume test: TARIT_TEST_DATABASE_URL is absent");
            return Ok(());
        };
        if database_url.is_empty() {
            return Ok(());
        }
        let _database_guard = POSTGRES_TEST_LOCK.lock().await;
        let fleet = PostgresFleet::connect(&database_url).await?;
        let suffix = Uuid::new_v4();
        let owner = format!("volume-owner-{suffix}");
        let foreign = format!("volume-foreign-{suffix}");
        let host = format!("volume-host-{suffix}");
        let volume = test_volume(&owner, &host, suffix);
        let mut rwx_a = test_volume(&owner, &host, Uuid::new_v4());
        rwx_a.status = VolumeStatus::Available;
        rwx_a.capabilities.read_write_once = false;
        rwx_a.capabilities.read_write_many = true;
        let mut rwx_b = test_volume(&owner, &host, Uuid::new_v4());
        rwx_b.status = VolumeStatus::Available;
        rwx_b.capabilities.read_write_once = false;
        rwx_b.capabilities.read_write_many = true;
        let vm_a = test_vm(Uuid::new_v4(), &host, &owner);
        let vm_b = test_vm(Uuid::new_v4(), &host, &owner);
        let vm_c = test_vm(Uuid::new_v4(), &host, &owner);

        let result = async {
            assert_eq!(fleet.insert_volume(&volume).await?, volume);
            assert_eq!(fleet.insert_volume(&volume).await?.id, volume.id);
            let mut conflicting = volume.clone();
            conflicting.size_bytes += 1;
            assert!(matches!(
                fleet.insert_volume(&conflicting).await,
                Err(FleetError::Conflict(_))
            ));
            assert!(matches!(
                fleet.get_volume(&foreign, volume.id).await,
                Err(FleetError::NotFound)
            ));
            assert_eq!(fleet.list_volumes(&owner).await?.len(), 1);

            let available = fleet
                .transition_volume(
                    &owner,
                    volume.id,
                    VolumeTransition {
                        expected_status: VolumeStatus::Creating,
                        expected_revision: 1,
                        status: VolumeStatus::Available,
                        last_error: None,
                        updated_at: Utc::now(),
                    },
                )
                .await?;
            assert_eq!(available.status, VolumeStatus::Available);
            assert_eq!(available.revision, 2);
            assert!(matches!(
                fleet
                    .transition_volume(
                        &owner,
                        volume.id,
                        VolumeTransition {
                            expected_status: VolumeStatus::Creating,
                            expected_revision: 1,
                            status: VolumeStatus::Error,
                            last_error: Some("stale"),
                            updated_at: Utc::now(),
                        },
                    )
                    .await,
                Err(FleetError::Conflict(_))
            ));

            for vm in [&vm_a, &vm_b, &vm_c] {
                fleet.upsert_vm(vm).await?;
            }
            fleet.insert_volume(&rwx_a).await?;
            fleet.insert_volume(&rwx_b).await?;
            let rwx_attachment = |vm_id, volume_id, device_index| VmVolumeAttachmentRecord {
                vm_id,
                volume_id,
                device_index,
                owner_key: owner.clone(),
                mode: VolumeAttachmentMode::ReadWrite,
                volume_generation: 1,
                created_at: Utc::now(),
            };
            let forward = [
                rwx_attachment(vm_b.id, rwx_a.id, 0),
                rwx_attachment(vm_b.id, rwx_b.id, 1),
            ];
            let reverse = [
                rwx_attachment(vm_c.id, rwx_b.id, 0),
                rwx_attachment(vm_c.id, rwx_a.id, 1),
            ];
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::try_join!(
                    fleet.bind_vm_volumes(&forward),
                    fleet.bind_vm_volumes(&reverse)
                )
            })
            .await
            .map_err(|_| FleetError::Conflict("multi-volume bind deadlocked".into()))??;
            assert_eq!(fleet.volume_attachment_count(&owner, rwx_a.id).await?, 2);
            assert_eq!(fleet.volume_attachment_count(&owner, rwx_b.id).await?, 2);
            fleet.unbind_vm_volumes(&owner, vm_b.id).await?;
            fleet.unbind_vm_volumes(&owner, vm_c.id).await?;
            let attachment = |vm_id, device_index, mode| VmVolumeAttachmentRecord {
                vm_id,
                volume_id: volume.id,
                device_index,
                owner_key: owner.clone(),
                mode,
                volume_generation: 1,
                created_at: Utc::now(),
            };
            let writer = attachment(vm_a.id, 0, VolumeAttachmentMode::ReadWrite);
            fleet.bind_vm_volumes(std::slice::from_ref(&writer)).await?;
            fleet.bind_vm_volumes(std::slice::from_ref(&writer)).await?;
            let listed = fleet.list_vm_volume_attachments(&owner, vm_a.id).await?;
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].vm_id, writer.vm_id);
            assert_eq!(listed[0].volume_id, writer.volume_id);
            assert_eq!(listed[0].device_index, writer.device_index);
            assert_eq!(listed[0].owner_key, writer.owner_key);
            assert_eq!(listed[0].mode, writer.mode);
            assert_eq!(listed[0].volume_generation, writer.volume_generation);
            assert_eq!(fleet.volume_attachment_count(&owner, volume.id).await?, 1);
            assert!(matches!(
                fleet
                    .bind_vm_volumes(&[attachment(vm_b.id, 0, VolumeAttachmentMode::ReadOnly)])
                    .await,
                Err(FleetError::Conflict(_))
            ));
            assert!(matches!(
                fleet
                    .begin_volume_delete(&owner, volume.id, VolumeStatus::Available, 2, Utc::now())
                    .await,
                Err(FleetError::Conflict(_))
            ));

            fleet.delete_vm(&vm_a).await?;
            let reader_b = attachment(vm_b.id, 0, VolumeAttachmentMode::ReadOnly);
            let reader_c = attachment(vm_c.id, 0, VolumeAttachmentMode::ReadOnly);
            fleet.bind_vm_volumes(&[reader_b]).await?;
            fleet.bind_vm_volumes(&[reader_c]).await?;
            assert_eq!(fleet.volume_attachment_count(&owner, volume.id).await?, 2);
            fleet.delete_vm(&vm_b).await?;
            fleet.delete_vm(&vm_c).await?;
            assert_eq!(fleet.volume_attachment_count(&owner, volume.id).await?, 0);

            let deleting = fleet
                .begin_volume_delete(&owner, volume.id, VolumeStatus::Available, 2, Utc::now())
                .await?;
            assert_eq!(deleting.status, VolumeStatus::Deleting);
            assert_eq!(deleting.revision, 3);
            fleet.delete_volume_metadata(&owner, volume.id, 3).await?;
            assert!(matches!(
                fleet.get_volume(&owner, volume.id).await,
                Err(FleetError::NotFound)
            ));
            Ok::<(), FleetError>(())
        }
        .await;

        let client = fleet.pool.get().await?;
        client
            .execute(
                "DELETE FROM fleet_vm_volume_attachments WHERE volume_id = $1",
                &[&volume.id],
            )
            .await?;
        client
            .execute(
                "DELETE FROM fleet_vms WHERE id = ANY($1)",
                &[&vec![vm_a.id, vm_b.id, vm_c.id]],
            )
            .await?;
        client
            .execute("DELETE FROM fleet_volumes WHERE id = $1", &[&volume.id])
            .await?;
        client
            .execute(
                "DELETE FROM fleet_volumes WHERE id = ANY($1)",
                &[&vec![rwx_a.id, rwx_b.id]],
            )
            .await?;
        result
    }

    #[tokio::test]
    async fn artifact_replication_and_branch_cas_use_postgres_when_database_is_configured(
    ) -> Result<(), FleetError> {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping PostgreSQL artifact/branch test: TARIT_TEST_DATABASE_URL is absent"
            );
            return Ok(());
        };
        if database_url.is_empty() {
            return Ok(());
        }
        let _database_guard = POSTGRES_TEST_LOCK.lock().await;
        let fleet = PostgresFleet::connect_with_replication_policy(&database_url, 2, 2).await?;
        let suffix = Uuid::new_v4();
        let owner = format!("tenant-{suffix}");
        let foreign_owner = format!("foreign-{suffix}");
        let host_a = format!("host-a-{suffix}");
        let host_b = format!("host-b-{suffix}");
        let host_c = format!("host-c-{suffix}");
        let session_a = Uuid::new_v4();
        let session_b = Uuid::new_v4();
        let session_c = Uuid::new_v4();
        let vm_id = Uuid::new_v4();
        let mut first = test_artifact(&owner, &host_a, suffix);
        first.source_vm_id = Some(vm_id);
        let second = test_artifact(&owner, &host_a, suffix);
        let foreign = test_artifact(&foreign_owner, &host_a, suffix);
        let branch = BranchRecord {
            branch_id: Uuid::new_v4(),
            owner_key: owner.clone(),
            name: format!("main-{suffix}"),
            head_artifact_id: first.artifact_id,
            source_vm_id: None,
            source_branch_id: None,
            revision: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let result = async {
            fleet
                .upsert_host(&host_record_from_capacity(
                    &host_a,
                    session_a,
                    None,
                    Some("https://host-a.invalid".into()),
                    0,
                    8,
                    8192,
                ))
                .await?;
            fleet
                .upsert_host(&host_record_from_capacity(
                    &host_c,
                    session_c,
                    None,
                    Some("https://host-c.invalid".into()),
                    0,
                    8,
                    8192,
                ))
                .await?;
            fleet
                .upsert_host(&host_record_from_capacity(
                    &host_b,
                    session_b,
                    None,
                    Some("https://host-b.invalid".into()),
                    0,
                    8,
                    8192,
                ))
                .await?;
            fleet.insert_artifact(&first).await?;
            fleet.insert_artifact(&first).await?;
            fleet.insert_artifact(&second).await?;
            fleet.insert_artifact(&foreign).await?;
            if fleet
                .list_artifact_replicas(&owner, first.artifact_id)
                .await?
                .len()
                != 1
            {
                return Err(FleetError::Config(
                    "idempotent artifact replay duplicated primary replica".into(),
                ));
            }
            let now = Utc::now();
            let replica = ArtifactReplicaRecord {
                artifact_id: first.artifact_id,
                owner_key: owner.clone(),
                host_id: host_b.clone(),
                failure_domain: "zone-b".into(),
                storage_locator: format!("/private/{suffix}/replica-b"),
                status: ArtifactReplicaStatus::Available,
                content_digest: first.content_digest.clone(),
                size_bytes: first.size_bytes,
                integrity_manifest_digest: first.integrity_manifest_digest.clone(),
                verified_at: Some(now),
                created_at: now,
                updated_at: now,
            };
            if fleet.upsert_artifact_replica(&replica).await? != ArtifactReplicationState::Ready {
                return Err(FleetError::Config(
                    "two verified failure domains did not become ready".into(),
                ));
            }
            let object_replica = ArtifactObjectReplicaRecord {
                artifact_id: first.artifact_id,
                owner_key: owner.clone(),
                provider: "aws_s3_immutable_object".into(),
                manifest_digest: format!("sha256:{:064x}", suffix.as_u128()),
                manifest_size_bytes: 4096,
                status: ArtifactReplicaStatus::Available,
                verified_at: Some(now),
                created_at: now,
                updated_at: now,
            };
            fleet
                .upsert_artifact_object_replica(&object_replica)
                .await?;
            fleet
                .upsert_artifact_object_replica(&object_replica)
                .await?;
            if fleet
                .list_artifact_object_replicas(&owner, first.artifact_id)
                .await?
                != vec![object_replica.clone()]
                || !fleet
                    .list_artifact_object_replicas(&foreign_owner, first.artifact_id)
                    .await?
                    .is_empty()
            {
                return Err(FleetError::Config(
                    "object replica was not idempotent and tenant scoped".into(),
                ));
            }
            let mut rebound_object = object_replica.clone();
            rebound_object.manifest_digest =
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
            if !matches!(
                fleet.upsert_artifact_object_replica(&rebound_object).await,
                Err(FleetError::Conflict(_))
            ) {
                return Err(FleetError::Config(
                    "object replica immutable identity was rebound".into(),
                ));
            }
            let mut unverified_object = object_replica;
            unverified_object.verified_at = None;
            if !matches!(
                fleet
                    .upsert_artifact_object_replica(&unverified_object)
                    .await,
                Err(FleetError::Conflict(_))
            ) {
                return Err(FleetError::Config(
                    "unverified object replica was accepted as available".into(),
                ));
            }
            let second_replica = ArtifactReplicaRecord {
                artifact_id: second.artifact_id,
                storage_locator: format!("/private/{suffix}/replica-b-second"),
                content_digest: second.content_digest.clone(),
                size_bytes: second.size_bytes,
                integrity_manifest_digest: second.integrity_manifest_digest.clone(),
                ..replica.clone()
            };
            if fleet.upsert_artifact_replica(&second_replica).await?
                != ArtifactReplicationState::Ready
            {
                return Err(FleetError::Config(
                    "second branch head did not satisfy replication policy".into(),
                ));
            }
            let mut hibernated_vm = test_vm(vm_id, &host_a, &owner);
            hibernated_vm.status = VmStatus::Hibernated;
            hibernated_vm = normalize_vm_timestamps_for_postgres(&hibernated_vm);
            fleet.upsert_vm(&hibernated_vm).await?;
            let policy = EgressPolicyRecord {
                vm_id,
                owner_key: owner.clone(),
                revision: 1,
                allowlist: vec!["203.0.113.10/32:443/tcp".into()],
                allow_existing: false,
                created_at: hibernated_vm.created_at,
                updated_at: hibernated_vm.updated_at,
            };
            let durable_hibernation = FleetHibernationRecord {
                vm_id,
                owner_key: owner.clone(),
                artifact_id: first.artifact_id,
                egress_policy: policy.clone(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            fleet.upsert_hibernation(&durable_hibernation).await?;
            if fleet
                .get_artifact(&owner, first.artifact_id)
                .await?
                .reference_count
                != 1
            {
                return Err(FleetError::Config(
                    "hibernation did not retain its artifact".into(),
                ));
            }
            if !matches!(
                fleet
                    .claim_hibernated_vm(
                        &owner,
                        vm_id,
                        &host_b,
                        session_b,
                        Utc::now() - chrono::Duration::seconds(15),
                    )
                    .await,
                Err(FleetError::Conflict(_))
            ) {
                return Err(FleetError::Config(
                    "healthy hibernated owner was stolen".into(),
                ));
            }
            if !matches!(
                fleet
                    .claim_hibernated_vm(
                        &foreign_owner,
                        vm_id,
                        &host_b,
                        session_b,
                        Utc::now() - chrono::Duration::seconds(15),
                    )
                    .await,
                Err(FleetError::NotFound)
            ) {
                return Err(FleetError::Config(
                    "cross-tenant hibernated claim was not hidden".into(),
                ));
            }
            let client = fleet.pool.get().await?;
            client
                .execute(
                    "UPDATE fleet_hosts SET last_heartbeat = NOW() - INTERVAL '1 hour'
                     WHERE host_id = $1",
                    &[&host_a],
                )
                .await?;
            let restarted =
                PostgresFleet::connect_with_replication_policy(&database_url, 2, 2).await?;
            if restarted
                .get_artifact(&owner, first.artifact_id)
                .await?
                .replication_state
                != ArtifactReplicationState::Degraded
            {
                return Err(FleetError::Config(
                    "startup trusted a replica on a stale host".into(),
                ));
            }
            let lease = fleet
                .try_acquire_artifact_repair_lease(
                    first.artifact_id,
                    &host_c,
                    session_c,
                    "zone-c",
                    Utc::now() + chrono::Duration::seconds(30),
                )
                .await?
                .ok_or_else(|| FleetError::Config("repair lease was not acquired".into()))?;
            if fleet
                .try_acquire_artifact_repair_lease(
                    first.artifact_id,
                    &host_c,
                    session_c,
                    "zone-c",
                    Utc::now() + chrono::Duration::seconds(30),
                )
                .await?
                .is_some()
                || fleet
                    .renew_artifact_repair_lease(
                        first.artifact_id,
                        &host_c,
                        Uuid::new_v4(),
                        lease,
                        Utc::now() + chrono::Duration::seconds(30),
                    )
                    .await?
            {
                return Err(FleetError::Config(
                    "repair lease did not fence a duplicate or stale session".into(),
                ));
            }
            if !fleet
                .renew_artifact_repair_lease(
                    first.artifact_id,
                    &host_c,
                    session_c,
                    lease,
                    Utc::now() + chrono::Duration::seconds(30),
                )
                .await?
            {
                return Err(FleetError::Config("repair lease did not renew".into()));
            }
            fleet
                .release_artifact_repair_lease(first.artifact_id, &host_c, session_c, lease)
                .await?;
            let claimed = fleet
                .claim_hibernated_vm(
                    &owner,
                    vm_id,
                    &host_b,
                    session_b,
                    Utc::now() - chrono::Duration::seconds(15),
                )
                .await?;
            if claimed.host_id != host_b || claimed.status != VmStatus::Hibernated {
                return Err(FleetError::Config(
                    "stale hibernated owner was not fenced and re-placed".into(),
                ));
            }
            if !matches!(
                fleet
                    .claim_hibernated_vm(
                        &owner,
                        vm_id,
                        &host_b,
                        Uuid::new_v4(),
                        Utc::now() - chrono::Duration::seconds(15),
                    )
                    .await,
                Err(FleetError::Conflict(_))
            ) {
                return Err(FleetError::Config(
                    "stale target boot session was accepted".into(),
                ));
            }
            let replacement_policy = EgressPolicyRecord {
                revision: 2,
                allow_existing: true,
                updated_at: DateTime::from_timestamp_micros(Utc::now().timestamp_micros())
                    .expect("current timestamp fits"),
                ..policy
            };
            fleet
                .update_hibernation_egress(&owner, vm_id, 1, &replacement_policy)
                .await?;
            if fleet.get_hibernation(&owner, vm_id).await?.egress_policy != replacement_policy {
                return Err(FleetError::Config(
                    "durable hibernation egress did not round-trip".into(),
                ));
            }
            fleet
                .upsert_host(&host_record_from_capacity(
                    &host_a,
                    Uuid::new_v4(),
                    None,
                    Some("https://host-a.invalid".into()),
                    0,
                    8,
                    8192,
                ))
                .await?;
            fleet
                .refresh_artifact_replication_health(Utc::now() - chrono::Duration::seconds(15))
                .await?;
            if fleet
                .get_artifact(&owner, first.artifact_id)
                .await?
                .replication_state
                != ArtifactReplicationState::Ready
            {
                return Err(FleetError::Config(
                    "fresh verified replica hosts did not restore readiness".into(),
                ));
            }
            let created = fleet.insert_branch(&branch).await?;
            let replay = fleet.insert_branch(&branch).await?;
            if created != replay
                || fleet
                    .get_artifact(&owner, first.artifact_id)
                    .await?
                    .reference_count
                    != 2
            {
                return Err(FleetError::Config(
                    "branch replay was not reference-idempotent".into(),
                ));
            }
            if !matches!(
                fleet
                    .delete_artifact_if_unreferenced(&owner, first.artifact_id)
                    .await,
                Err(FleetError::Conflict(_))
            ) {
                return Err(FleetError::Config(
                    "branch head artifact was deleted while referenced".into(),
                ));
            }
            if !matches!(
                fleet
                    .update_branch_head(
                        &owner,
                        branch.branch_id,
                        1,
                        foreign.artifact_id,
                        Utc::now()
                    )
                    .await,
                Err(FleetError::NotFound)
            ) {
                return Err(FleetError::Config(
                    "cross-tenant branch head was accepted".into(),
                ));
            }
            let updated = fleet
                .update_branch_head(&owner, branch.branch_id, 1, second.artifact_id, Utc::now())
                .await?;
            if updated.revision != 2 || updated.head_artifact_id != second.artifact_id {
                return Err(FleetError::Config("branch CAS did not advance".into()));
            }
            if !matches!(
                fleet
                    .update_branch_head(&owner, branch.branch_id, 1, first.artifact_id, Utc::now())
                    .await,
                Err(FleetError::Conflict(_))
            ) {
                return Err(FleetError::Config("stale branch CAS was accepted".into()));
            }
            fleet.delete_branch(&owner, branch.branch_id).await?;
            if !matches!(
                fleet.get_artifact(&owner, second.artifact_id).await,
                Err(FleetError::NotFound)
            ) {
                return Err(FleetError::Config(
                    "branch deletion did not remove its unreferenced head".into(),
                ));
            }
            fleet.delete_vm(&claimed).await?;
            if !matches!(
                fleet.get_hibernation(&owner, vm_id).await,
                Err(FleetError::NotFound)
            ) {
                return Err(FleetError::Config(
                    "VM deletion retained its hibernation binding".into(),
                ));
            }
            if !matches!(
                fleet.get_artifact(&owner, first.artifact_id).await,
                Err(FleetError::NotFound)
            ) {
                return Err(FleetError::Config(
                    "VM deletion did not collect its final hibernation artifact".into(),
                ));
            }
            Ok::<(), FleetError>(())
        }
        .await;
        let client = fleet.pool.get().await?;
        client
            .execute(
                "DELETE FROM fleet_branches WHERE branch_id = $1",
                &[&branch.branch_id],
            )
            .await?;
        client
            .execute("DELETE FROM fleet_hibernations WHERE vm_id = $1", &[&vm_id])
            .await?;
        client
            .execute("DELETE FROM fleet_vms WHERE id = $1", &[&vm_id])
            .await?;
        for artifact in [&first, &second, &foreign] {
            client
                .execute(
                    "DELETE FROM fleet_artifacts WHERE artifact_id = $1",
                    &[&artifact.artifact_id],
                )
                .await?;
        }
        client
            .execute(
                "DELETE FROM fleet_hosts WHERE host_id = ANY($1)",
                &[&vec![host_a, host_b, host_c]],
            )
            .await?;
        result
    }

    #[test]
    fn vm_timestamp_normalization_matches_postgres_precision() {
        let mut vm = test_vm(Uuid::new_v4(), "host-a", "tenant-a");
        vm.created_at = DateTime::from_timestamp(1_700_000_000, 123_456_789).unwrap();
        vm.updated_at = DateTime::from_timestamp(1_700_000_001, 987_654_321).unwrap();

        let normalized = normalize_vm_timestamps_for_postgres(&vm);

        assert_eq!(normalized.created_at.timestamp_subsec_nanos(), 123_456_000);
        assert_eq!(normalized.updated_at.timestamp_subsec_nanos(), 987_654_000);
        assert_eq!(normalized.id, vm.id);
        assert_eq!(normalized.revision, vm.revision);
    }

    #[tokio::test]
    async fn replication_policy_rejects_impossible_failure_domain_minimum() {
        assert!(matches!(
            PostgresFleet::connect_with_replication_policy("unused", 1, 2).await,
            Err(FleetError::Config(_))
        ));
    }

    #[tokio::test]
    async fn opaque_snapshot_locator_round_trips_when_database_is_configured(
    ) -> Result<(), FleetError> {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            return Ok(());
        };
        if database_url.is_empty() {
            return Ok(());
        }
        let _database_guard = POSTGRES_TEST_LOCK.lock().await;
        let fleet = PostgresFleet::connect(&database_url).await?;
        let snapshot = SnapshotRecord {
            snapshot_id: Uuid::new_v4(),
            path: format!("/private/{}.ram", Uuid::new_v4()),
            overlay_path: None,
            host_id: "host-a".into(),
            owner_key: Some("tenant-a".into()),
            api_key_id: Some("key-a".into()),
            vm_id: Uuid::new_v4(),
            ephemeral_owner_vm_id: None,
            memory_mib: Some(256),
            vcpus: Some(1),
            kernel_path: Some("/private/vmlinux".into()),
            rootfs_path: None,
            rootfs_read_only: Some(true),
            cmdline: Some("console=ttyS0".into()),
            content_digest: Some("sha256:test".into()),
            size_bytes: Some(4096),
            created_at: Utc::now(),
        };
        fleet.upsert_snapshot(&snapshot).await?;
        let location = fleet
            .get_snapshot_location(snapshot.snapshot_id)
            .await?
            .ok_or(FleetError::NotFound)?;
        assert_eq!(location.snapshot_id, snapshot.snapshot_id);
        assert_eq!(location.host_id, snapshot.host_id);
        assert_eq!(location.owner_key, snapshot.owner_key.as_deref().unwrap());
        assert_eq!(location.snapshot_path, snapshot.path);
        let client = fleet.pool.get().await?;
        client
            .execute(
                "DELETE FROM fleet_snapshots WHERE snapshot_id = $1",
                &[&snapshot.snapshot_id],
            )
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn vm_claims_and_deletes_are_incarnation_fenced_when_database_is_configured(
    ) -> Result<(), FleetError> {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            return Ok(());
        };
        if database_url.is_empty() {
            return Ok(());
        }
        let _database_guard = POSTGRES_TEST_LOCK.lock().await;
        let fleet = PostgresFleet::connect(&database_url).await?;
        let id = Uuid::new_v4();
        let owner = format!("tenant-{id}");
        let mut vm = test_vm(id, "host-a", &owner);
        vm.created_at = DateTime::from_timestamp(1_700_000_000, 123_456_789).unwrap();
        vm.updated_at = DateTime::from_timestamp(1_700_000_001, 987_654_321).unwrap();
        let result = async {
            let first_generation = fleet.upsert_vm(&vm).await?;
            let retry_generation = fleet.upsert_vm(&vm).await?;
            assert_eq!(
                retry_generation, first_generation,
                "an identical retry must be idempotent at PostgreSQL timestamp precision"
            );
            let mut running = vm.clone();
            running.status = VmStatus::Running;
            running.revision += 1;
            running.updated_at += chrono::Duration::seconds(1);
            let second_generation = fleet.upsert_vm(&running).await?;
            assert!(second_generation > first_generation);

            let mut conflicting_retry = running.clone();
            conflicting_retry.cmdline = "different".into();
            assert!(matches!(
                fleet.upsert_vm(&conflicting_retry).await,
                Err(FleetError::Conflict(_))
            ));

            let stale_owner = VmRecord {
                host_id: "host-b".into(),
                ..running.clone()
            };
            assert!(matches!(
                fleet.upsert_vm(&stale_owner).await,
                Err(FleetError::Conflict(_))
            ));
            assert!(matches!(
                fleet.delete_vm(&stale_owner).await,
                Err(FleetError::Conflict(_))
            ));
            assert_eq!(fleet.get_vm_host(id).await?, Some("host-a".into()));
            fleet.delete_vm(&running).await?;
            assert_eq!(fleet.get_vm_host(id).await?, None);
            Ok::<(), FleetError>(())
        }
        .await;
        let client = fleet.pool.get().await?;
        client
            .execute("DELETE FROM tenant_vm_reservations WHERE id = $1", &[&id])
            .await?;
        client
            .execute("DELETE FROM fleet_vms WHERE id = $1", &[&id])
            .await?;
        result
    }

    #[tokio::test]
    async fn lazy_restore_vm_reference_is_idempotent_incarnation_fenced_and_released(
    ) -> Result<(), FleetError> {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            return Ok(());
        };
        if database_url.is_empty() {
            return Ok(());
        }
        let _database_guard = POSTGRES_TEST_LOCK.lock().await;
        let fleet = PostgresFleet::connect(&database_url).await?;
        let suffix = Uuid::new_v4();
        let owner = format!("tenant-{suffix}");
        let artifact = test_artifact(&owner, "host-a", suffix);
        let second_artifact = test_artifact(&owner, "host-a", suffix);
        fleet.insert_artifact(&artifact).await?;
        let mut vm = test_vm(Uuid::new_v4(), "host-a", &owner);
        vm.created_at = DateTime::from_timestamp(1_700_000_000, 123_456_789).unwrap();
        vm.updated_at = vm.created_at;
        let result = async {
            fleet
                .acquire_vm_artifact_ref(&vm, artifact.artifact_id)
                .await?;
            fleet
                .acquire_vm_artifact_ref(&vm, artifact.artifact_id)
                .await?;
            assert_eq!(
                fleet
                    .get_artifact(&owner, artifact.artifact_id)
                    .await?
                    .reference_count,
                1,
                "retrying the same restore must not double-acquire"
            );
            let mut reused_uuid = vm.clone();
            reused_uuid.created_at += chrono::Duration::seconds(1);
            assert!(matches!(
                fleet.release_vm_artifact_ref(&reused_uuid).await,
                Err(FleetError::Conflict(_))
            ));
            fleet.release_vm_artifact_ref(&vm).await?;
            assert!(matches!(
                fleet.get_artifact(&owner, artifact.artifact_id).await,
                Err(FleetError::NotFound)
            ));

            fleet.insert_artifact(&second_artifact).await?;
            fleet
                .acquire_vm_artifact_ref(&vm, second_artifact.artifact_id)
                .await?;
            fleet.upsert_vm(&vm).await?;
            fleet.delete_vm(&vm).await?;
            assert!(matches!(
                fleet
                    .get_artifact(&owner, second_artifact.artifact_id)
                    .await,
                Err(FleetError::NotFound)
            ));
            Ok::<(), FleetError>(())
        }
        .await;
        let client = fleet.pool.get().await?;
        client
            .execute(
                "DELETE FROM fleet_vm_artifact_refs WHERE vm_id = $1",
                &[&vm.id],
            )
            .await?;
        client
            .execute("DELETE FROM fleet_vms WHERE id = $1", &[&vm.id])
            .await?;
        client
            .execute(
                "DELETE FROM fleet_artifacts WHERE artifact_id = $1",
                &[&artifact.artifact_id],
            )
            .await?;
        client
            .execute(
                "DELETE FROM fleet_artifacts WHERE artifact_id = $1",
                &[&second_artifact.artifact_id],
            )
            .await?;
        result
    }

    #[allow(dead_code)]
    fn share_persistence_api_is_available(fleet: &PostgresFleet, share: &ShareRecord) {
        std::mem::drop(fleet.insert_share(share));
        std::mem::drop(fleet.get_share(share.id));
        std::mem::drop(fleet.get_share_by_slug(&share.slug));
        std::mem::drop(fleet.list_shares(&share.owner_key));
        std::mem::drop(fleet.update_share(share));
        std::mem::drop(fleet.update_share_if_current(share, share.token_version));
    }

    #[tokio::test]
    async fn share_compare_and_swap_uses_postgres_when_database_is_configured(
    ) -> Result<(), FleetError> {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping PostgreSQL share compare-and-swap test: TARIT_TEST_DATABASE_URL is absent"
            );
            return Ok(());
        };
        if database_url.is_empty() {
            eprintln!(
                "skipping PostgreSQL share compare-and-swap test: TARIT_TEST_DATABASE_URL is empty"
            );
            return Ok(());
        }

        let _database_guard = POSTGRES_TEST_LOCK.lock().await;
        let fleet = PostgresFleet::connect(&database_url).await?;
        let suffix = Uuid::new_v4();
        let mut share = test_share(format!("share-{suffix}-cas"), &format!("tenant-{suffix}"));
        share.revoked_at = None;
        let result = async {
            fleet.insert_share(&share).await?;

            let updated = ShareRecord {
                guest_port: 9090,
                token_version: share.token_version + 1,
                updated_at: share.updated_at + chrono::Duration::seconds(1),
                ..share.clone()
            };
            fleet
                .update_share_if_current(&updated, share.token_version)
                .await?;
            assert_share_eq(
                &fleet
                    .get_share(share.id)
                    .await?
                    .ok_or_else(|| FleetError::Config("updated share is missing".into()))?,
                &updated,
            )?;

            let stale = ShareRecord {
                visibility: ShareVisibility::Public,
                token_version: share.token_version + 1,
                ..share.clone()
            };
            if !matches!(
                fleet
                    .update_share_if_current(&stale, share.token_version)
                    .await,
                Err(FleetError::Conflict(_))
            ) {
                return Err(FleetError::Config(
                    "stale compare-and-swap share update did not conflict".into(),
                ));
            }

            Ok::<(), FleetError>(())
        }
        .await;

        result.and(cleanup_test_shares(&fleet, &[share.id]).await)
    }

    #[tokio::test]
    async fn share_persistence_round_trip_matches_sqlite_when_database_is_configured(
    ) -> Result<(), FleetError> {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping PostgreSQL share integration test: TARIT_TEST_DATABASE_URL is absent"
            );
            return Ok(());
        };
        if database_url.is_empty() {
            eprintln!(
                "skipping PostgreSQL share integration test: TARIT_TEST_DATABASE_URL is empty"
            );
            return Ok(());
        }

        let _database_guard = POSTGRES_TEST_LOCK.lock().await;
        let fleet = PostgresFleet::connect(&database_url).await?;
        let suffix = Uuid::new_v4();
        let tenant_a = format!("tenant-a-{suffix}");
        let tenant_b = format!("tenant-b-{suffix}");
        let mut first = test_share(format!("share-{suffix}-first"), &tenant_a);
        let mut second = test_share(format!("share-{suffix}-second"), &tenant_a);
        second.created_at += chrono::Duration::seconds(1);
        let other_tenant = test_share(format!("share-{suffix}-other"), &tenant_b);
        let missing = test_share(format!("share-{suffix}-missing"), &tenant_a);
        let ids = [first.id, second.id, other_tenant.id, missing.id];

        let result = async {
            fleet.insert_share(&first).await?;
            fleet.insert_share(&second).await?;
            fleet.insert_share(&other_tenant).await?;

            assert_share_eq(
                &fleet
                    .get_share(first.id)
                    .await?
                    .ok_or_else(|| FleetError::Config("inserted share is missing".into()))?,
                &first,
            )?;
            assert_share_eq(
                &fleet
                    .get_share_by_slug(&first.slug)
                    .await?
                    .ok_or_else(|| FleetError::Config("inserted slug is missing".into()))?,
                &first,
            )?;

            let duplicate_slug = ShareRecord {
                id: Uuid::new_v4(),
                ..first.clone()
            };
            if !matches!(
                fleet.insert_share(&duplicate_slug).await,
                Err(FleetError::Conflict(_))
            ) {
                return Err(FleetError::Config(
                    "duplicate share slug was accepted".into(),
                ));
            }

            let listed = fleet.list_shares(&tenant_a).await?;
            if listed.iter().map(|share| share.id).collect::<Vec<_>>() != vec![second.id, first.id]
            {
                return Err(FleetError::Config(
                    "tenant shares were not listed newest-first".into(),
                ));
            }
            if fleet
                .list_shares(&tenant_b)
                .await?
                .iter()
                .any(|share| share.id == first.id)
            {
                return Err(FleetError::Config(
                    "tenant shares leaked across owners".into(),
                ));
            }

            first.owner_key = tenant_b;
            first.guest_port = 9090;
            first.visibility = ShareVisibility::Public;
            first.token_version += 1;
            first.updated_at += chrono::Duration::seconds(1);
            fleet.update_share(&first).await?;
            let updated = fleet
                .get_share(first.id)
                .await?
                .ok_or_else(|| FleetError::Config("updated share is missing".into()))?;
            if updated.owner_key != tenant_a {
                return Err(FleetError::Config("share owner was changed".into()));
            }
            first.owner_key = tenant_a;
            assert_share_eq(&updated, &first)?;

            if !matches!(
                fleet.update_share(&missing).await,
                Err(FleetError::NotFound)
            ) {
                return Err(FleetError::Config(
                    "missing share update did not return not found".into(),
                ));
            }

            Ok::<(), FleetError>(())
        }
        .await;

        result.and(cleanup_test_shares(&fleet, &ids).await)
    }
}
