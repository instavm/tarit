//! Global control-plane store backed by PostgreSQL.
//!
//! Uses `tokio-postgres` + `deadpool-postgres` (both MIT OR Apache-2.0).

use chrono::{DateTime, Utc};
use deadpool_postgres::{Config as PoolConfig, Pool, Runtime};
use rustls::{ClientConfig, RootCertStore};
use tarit_store::HostRecord;
use tarit_types::{AuditEvent, ShareRecord, ShareVisibility, UsageEvent, UsageSummary, VmRecord};
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
    #[error("invalid fleet share row: {0}")]
    InvalidShareRow(String),
}

pub struct PostgresFleet {
    pool: Pool,
}

impl PostgresFleet {
    pub async fn connect(database_url: &str) -> Result<Self, FleetError> {
        let mut cfg = PoolConfig::new();
        cfg.url = Some(database_url.to_string());
        let tls = make_rustls_connector()?;
        let pool = cfg
            .create_pool(Some(Runtime::Tokio1), tls)
            .map_err(|e| FleetError::Config(e.to_string()))?;
        let client = pool.get().await?;
        client.batch_execute(FLEET_SCHEMA).await?;
        client
            .batch_execute("ALTER TABLE fleet_vms ADD COLUMN IF NOT EXISTS owner_key TEXT;")
            .await?;
        client
            .batch_execute("ALTER TABLE fleet_vms ADD COLUMN IF NOT EXISTS api_key_id TEXT;")
            .await?;
        Ok(Self { pool })
    }

    pub async fn upsert_host(&self, host: &HostRecord) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO fleet_hosts (host_id, rpc_addr, sandbox_count, free_vcpus, free_memory_mib, healthy, last_heartbeat)
                 VALUES ($1,$2,$3,$4,$5,$6,$7)
                 ON CONFLICT (host_id) DO UPDATE SET
                   rpc_addr = EXCLUDED.rpc_addr,
                   sandbox_count = EXCLUDED.sandbox_count,
                   free_vcpus = EXCLUDED.free_vcpus,
                   free_memory_mib = EXCLUDED.free_memory_mib,
                   healthy = EXCLUDED.healthy,
                   last_heartbeat = EXCLUDED.last_heartbeat",
                &[
                    &host.host_id,
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
                "SELECT host_id, rpc_addr, sandbox_count, free_vcpus, free_memory_mib, healthy, last_heartbeat
                 FROM fleet_hosts ORDER BY host_id",
                &[],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| HostRecord {
                host_id: row.get(0),
                rpc_addr: row.get(1),
                sandbox_count: row.get::<_, i64>(2) as usize,
                free_vcpus: row.get::<_, i64>(3) as u64,
                free_memory_mib: row.get::<_, i64>(4) as u64,
                healthy: row.get(5),
                last_heartbeat: row.get(6),
            })
            .collect())
    }

    pub async fn upsert_vm(&self, vm: &VmRecord) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO fleet_vms (id, host_id, owner_key, api_key_id, status, memory_mib, vcpus, kernel_path, rootfs_path, cmdline, created_at, updated_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
                 ON CONFLICT (id) DO UPDATE SET
                   host_id = EXCLUDED.host_id,
                   owner_key = EXCLUDED.owner_key,
                   api_key_id = EXCLUDED.api_key_id,
                   status = EXCLUDED.status,
                   memory_mib = EXCLUDED.memory_mib,
                   vcpus = EXCLUDED.vcpus,
                   kernel_path = EXCLUDED.kernel_path,
                   rootfs_path = EXCLUDED.rootfs_path,
                   cmdline = EXCLUDED.cmdline,
                   updated_at = EXCLUDED.updated_at",
                &[
                    &vm.id,
                    &vm.host_id,
                    &vm.owner_key,
                    &vm.api_key_id,
                    &vm.status.as_str(),
                    &(vm.memory_mib as i64),
                    &(vm.vcpus as i16),
                    &vm.kernel_path,
                    &vm.rootfs_path,
                    &vm.cmdline,
                    &vm.created_at,
                    &vm.updated_at,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn count_active_vms_for_owner(&self, owner_key: &str) -> Result<usize, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_one(
                "SELECT COUNT(*) FROM fleet_vms
                 WHERE owner_key = $1 AND status IN ('creating', 'running', 'paused')",
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
                "SELECT host_id, rpc_addr, sandbox_count, free_vcpus, free_memory_mib, healthy, last_heartbeat
                 FROM fleet_hosts WHERE host_id = $1",
                &[&host_id],
            )
            .await?;
        Ok(row.map(|row| HostRecord {
            host_id: row.get(0),
            rpc_addr: row.get(1),
            sandbox_count: row.get::<_, i64>(2) as usize,
            free_vcpus: row.get::<_, i64>(3) as u64,
            free_memory_mib: row.get::<_, i64>(4) as u64,
            healthy: row.get(5),
            last_heartbeat: row.get(6),
        }))
    }

    /// Remove a VM's ownership row (called when a VM is stopped/deleted) so the
    /// cluster no longer routes to a dead sandbox.
    pub async fn delete_vm(&self, id: Uuid) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        client
            .execute("DELETE FROM fleet_vms WHERE id = $1", &[&id])
            .await?;
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
  rpc_addr TEXT,
  sandbox_count BIGINT NOT NULL DEFAULT 0,
  free_vcpus BIGINT NOT NULL DEFAULT 0,
  free_memory_mib BIGINT NOT NULL DEFAULT 0,
  healthy BOOLEAN NOT NULL DEFAULT TRUE,
  last_heartbeat TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE IF NOT EXISTS fleet_vms (
  id UUID PRIMARY KEY,
  host_id TEXT NOT NULL,
  owner_key TEXT,
  api_key_id TEXT,
  status TEXT NOT NULL,
  memory_mib BIGINT NOT NULL,
  vcpus SMALLINT NOT NULL,
  kernel_path TEXT NOT NULL,
  rootfs_path TEXT,
  cmdline TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL
);
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
";

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

fn fleet_error_from_postgres(error: tokio_postgres::Error) -> FleetError {
    if error.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) {
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
    fleet.upsert_vm(vm).await
}

/// Build a host record for heartbeat from scheduler state.
pub fn host_record_from_capacity(
    host_id: &str,
    rpc_addr: Option<String>,
    sandbox_count: usize,
    free_vcpus: u64,
    free_memory_mib: u64,
) -> HostRecord {
    HostRecord {
        host_id: host_id.to_string(),
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
    use tarit_types::ShareRecord;

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

    #[allow(dead_code)]
    fn share_persistence_api_is_available(fleet: &PostgresFleet, share: &ShareRecord) {
        let _ = fleet.insert_share(share);
        let _ = fleet.get_share(share.id);
        let _ = fleet.get_share_by_slug(&share.slug);
        let _ = fleet.list_shares(&share.owner_key);
        let _ = fleet.update_share(share);
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
