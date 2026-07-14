//! Global control-plane store backed by PostgreSQL.
//!
//! Uses `tokio-postgres` + `deadpool-postgres` (both MIT OR Apache-2.0).

use std::time::Duration;

use chrono::{DateTime, Utc};
use deadpool_postgres::{Config as PoolConfig, Pool, Runtime};
use rustls::{ClientConfig, RootCertStore};
use serde_json::Value;
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
    #[error("invalid fleet certificate row: {0}")]
    InvalidCertificateRow(String),
    #[error("invalid fleet ACME job row: {0}")]
    InvalidAcmeJobRow(String),
}

pub struct PostgresFleet {
    pool: Pool,
}

pub struct CertRefreshListener {
    _client: tokio_postgres::Client,
    receiver: tokio::sync::mpsc::Receiver<()>,
}

impl CertRefreshListener {
    pub async fn recv(&mut self) -> Option<()> {
        self.receiver.recv().await
    }
}

const FLEET_SCHEMA_LOCK_KEY: i64 = 0x5441_5249_5446_4C54;
const CERT_REFRESH_LISTEN_COMMAND: &str = "LISTEN tarit_cert_refresh";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertRecord {
    pub domain: String,
    pub cert_pem: String,
    pub key_sealed: Vec<u8>,
    pub key_nonce: Vec<u8>,
    pub generation: i64,
    pub not_after: DateTime<Utc>,
    pub sans: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcmeAccountSecret {
    pub sealed: Vec<u8>,
    pub nonce: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcmeJobState {
    Pending,
    Publishing,
    Ready,
    Validating,
    Finalizing,
    Active,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AcmeJob {
    pub id: Uuid,
    pub identifier: String,
    pub state: AcmeJobState,
    pub fence: i64,
    pub order_url: Option<String>,
    pub challenge: Option<Value>,
    pub provider_change_ids: Option<Value>,
    pub attempt: i64,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl PostgresFleet {
    pub async fn connect(database_url: &str) -> Result<Self, FleetError> {
        let mut cfg = PoolConfig::new();
        cfg.url = Some(database_url.to_string());
        let tls = make_rustls_connector()?;
        let pool = cfg
            .create_pool(Some(Runtime::Tokio1), tls)
            .map_err(|e| FleetError::Config(e.to_string()))?;
        let mut client = pool.get().await?;
        let ddl = client.transaction().await?;
        ddl.execute(
            "SELECT pg_advisory_xact_lock($1)",
            &[&FLEET_SCHEMA_LOCK_KEY],
        )
        .await?;
        ddl.batch_execute(FLEET_SCHEMA).await?;
        ddl.batch_execute("ALTER TABLE fleet_vms ADD COLUMN IF NOT EXISTS owner_key TEXT;")
            .await?;
        ddl.batch_execute("ALTER TABLE fleet_vms ADD COLUMN IF NOT EXISTS api_key_id TEXT;")
            .await?;
        ddl.commit().await?;
        Ok(Self { pool })
    }

    pub async fn cert_refresh_listener(
        database_url: &str,
    ) -> Result<CertRefreshListener, FleetError> {
        let (client, mut connection) =
            tokio_postgres::connect(database_url, make_rustls_connector()?).await?;
        let (sender, receiver) = tokio::sync::mpsc::channel(16);

        tokio::spawn(async move {
            loop {
                match std::future::poll_fn(|cx| connection.poll_message(cx)).await {
                    Some(Ok(tokio_postgres::AsyncMessage::Notification(notification)))
                        if notification.channel() == "tarit_cert_refresh" =>
                    {
                        let _ = sender.try_send(());
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        });

        client.batch_execute(CERT_REFRESH_LISTEN_COMMAND).await?;
        Ok(CertRefreshListener {
            _client: client,
            receiver,
        })
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

    pub async fn upsert_certificate(&self, certificate: &CertRecord) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        let sans = serialize_json(&certificate.sans, "sans")?;
        client
            .execute(
                "INSERT INTO fleet_certificates (
                   domain, cert_pem, key_sealed, key_nonce, generation, not_after, sans
                 ) VALUES ($1,$2,$3,$4,nextval('fleet_cert_generation'),$5,$6)
                 ON CONFLICT (domain) DO UPDATE SET
                   cert_pem = EXCLUDED.cert_pem,
                   key_sealed = EXCLUDED.key_sealed,
                   key_nonce = EXCLUDED.key_nonce,
                   not_after = EXCLUDED.not_after,
                   sans = EXCLUDED.sans,
                   generation = nextval('fleet_cert_generation')",
                &[
                    &certificate.domain,
                    &certificate.cert_pem,
                    &certificate.key_sealed,
                    &certificate.key_nonce,
                    &certificate.not_after,
                    &sans,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn publish_certificate(
        &self,
        certificate: &CertRecord,
        identifier: &str,
        fence: i64,
    ) -> Result<bool, FleetError> {
        let sans = serialize_json(&certificate.sans, "sans")?;
        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;
        let row = transaction
            .query_opt(
                "SELECT fence FROM fleet_acme_jobs WHERE identifier = $1 FOR UPDATE",
                &[&identifier],
            )
            .await?;
        let Some(row) = row else {
            transaction.rollback().await?;
            return Ok(false);
        };
        if row.get::<_, i64>(0) != fence {
            transaction.rollback().await?;
            return Ok(false);
        }

        transaction
            .execute(
                "INSERT INTO fleet_certificates (
                   domain, cert_pem, key_sealed, key_nonce, generation, not_after, sans
                 ) VALUES ($1,$2,$3,$4,nextval('fleet_cert_generation'),$5,$6)
                 ON CONFLICT (domain) DO UPDATE SET
                   cert_pem = EXCLUDED.cert_pem,
                   key_sealed = EXCLUDED.key_sealed,
                   key_nonce = EXCLUDED.key_nonce,
                   not_after = EXCLUDED.not_after,
                   sans = EXCLUDED.sans,
                   generation = nextval('fleet_cert_generation')",
                &[
                    &certificate.domain,
                    &certificate.cert_pem,
                    &certificate.key_sealed,
                    &certificate.key_nonce,
                    &certificate.not_after,
                    &sans,
                ],
            )
            .await?;
        let updated = transaction
            .execute(
                "UPDATE fleet_acme_jobs
                 SET state = 'active', updated_at = now()
                 WHERE identifier = $1 AND fence = $2",
                &[&identifier, &fence],
            )
            .await?;
        if updated == 0 {
            transaction.rollback().await?;
            return Ok(false);
        }
        transaction.commit().await?;
        Ok(true)
    }

    pub async fn get_certificate(&self, domain: &str) -> Result<Option<CertRecord>, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT domain, cert_pem, key_sealed, key_nonce, generation, not_after, sans
                 FROM fleet_certificates WHERE domain = $1",
                &[&domain],
            )
            .await?;
        row.map(|row| row_to_certificate(&row)).transpose()
    }

    pub async fn max_cert_generation(&self) -> Result<i64, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_one(
                "SELECT COALESCE(MAX(generation), 0) FROM fleet_certificates",
                &[],
            )
            .await?;
        Ok(row.get(0))
    }

    pub async fn certificates_since(&self, generation: i64) -> Result<Vec<CertRecord>, FleetError> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT domain, cert_pem, key_sealed, key_nonce, generation, not_after, sans
                 FROM fleet_certificates WHERE generation > $1 ORDER BY generation",
                &[&generation],
            )
            .await?;
        rows.iter().map(row_to_certificate).collect()
    }

    pub async fn get_acme_account(
        &self,
        directory_url: &str,
    ) -> Result<Option<AcmeAccountSecret>, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT sealed, nonce FROM fleet_acme_accounts WHERE directory_url = $1",
                &[&directory_url],
            )
            .await?;
        Ok(row.map(|row| AcmeAccountSecret {
            sealed: row.get(0),
            nonce: row.get(1),
        }))
    }

    pub async fn put_acme_account(
        &self,
        directory_url: &str,
        sealed: &[u8],
        nonce: &[u8],
    ) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO fleet_acme_accounts (directory_url, sealed, nonce, updated_at)
                 VALUES ($1,$2,$3,now())
                 ON CONFLICT (directory_url) DO UPDATE SET
                   sealed = EXCLUDED.sealed,
                   nonce = EXCLUDED.nonce,
                   updated_at = EXCLUDED.updated_at",
                &[&directory_url, &sealed, &nonce],
            )
            .await?;
        Ok(())
    }

    pub async fn claim_acme_job(
        &self,
        identifier: &str,
        holder: &str,
        lease: Duration,
    ) -> Result<Option<AcmeJob>, FleetError> {
        let client = self.pool.get().await?;
        let lease = duration_as_interval(lease)?;
        let row = client
            .query_opt(
                "INSERT INTO fleet_acme_jobs (
                   id, identifier, state, fence, lease_holder, lease_expires_at,
                   attempt, next_attempt_at, updated_at
                 ) VALUES ($1,$2,'pending',1,$3,now() + $4::text::interval,0,NULL,now())
                 ON CONFLICT (identifier) DO UPDATE SET
                   fence = fleet_acme_jobs.fence + 1,
                   lease_holder = EXCLUDED.lease_holder,
                   lease_expires_at = now() + $4::text::interval,
                   updated_at = now()
                 WHERE fleet_acme_jobs.lease_expires_at < now()
                   AND (fleet_acme_jobs.next_attempt_at IS NULL
                        OR fleet_acme_jobs.next_attempt_at <= now())
                 RETURNING id, identifier, state, fence, order_url, challenge,
                           provider_change_ids, attempt, next_attempt_at, last_error, updated_at",
                &[&Uuid::new_v4(), &identifier, &holder, &lease],
            )
            .await?;
        row.map(|row| row_to_acme_job(&row)).transpose()
    }

    pub async fn renew_acme_lease(
        &self,
        id: Uuid,
        fence: i64,
        holder: &str,
        lease: Duration,
    ) -> Result<bool, FleetError> {
        let client = self.pool.get().await?;
        let lease = duration_as_interval(lease)?;
        let updated = client
            .execute(
                "UPDATE fleet_acme_jobs
                 SET lease_expires_at = now() + $4::text::interval
                 WHERE id = $1 AND fence = $2 AND lease_holder = $3",
                &[&id, &fence, &holder, &lease],
            )
            .await?;
        Ok(updated > 0)
    }

    pub async fn save_acme_job(&self, job: &AcmeJob, fence: i64) -> Result<bool, FleetError> {
        let client = self.pool.get().await?;
        let challenge = serialize_optional_json(&job.challenge, "challenge")?;
        let provider_change_ids =
            serialize_optional_json(&job.provider_change_ids, "provider_change_ids")?;
        let updated = client
            .execute(
                "UPDATE fleet_acme_jobs SET
                   state = $2,
                   order_url = $3,
                   challenge = $4,
                   provider_change_ids = $5,
                   attempt = $6,
                   next_attempt_at = $7,
                   last_error = $8,
                   updated_at = $9
                 WHERE identifier = $1 AND fence = $10",
                &[
                    &job.identifier,
                    &acme_job_state_as_str(&job.state),
                    &job.order_url,
                    &challenge,
                    &provider_change_ids,
                    &job.attempt,
                    &job.next_attempt_at,
                    &job.last_error,
                    &job.updated_at,
                    &fence,
                ],
            )
            .await?;
        Ok(updated > 0)
    }

    pub async fn get_acme_job(&self, identifier: &str) -> Result<Option<AcmeJob>, FleetError> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT id, identifier, state, fence, order_url, challenge,
                        provider_change_ids, attempt, next_attempt_at, last_error, updated_at
                 FROM fleet_acme_jobs WHERE identifier = $1",
                &[&identifier],
            )
            .await?;
        row.map(|row| row_to_acme_job(&row)).transpose()
    }

    pub async fn notify_cert_refresh(&self) -> Result<(), FleetError> {
        let client = self.pool.get().await?;
        client.batch_execute("NOTIFY tarit_cert_refresh").await?;
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
CREATE SEQUENCE IF NOT EXISTS fleet_cert_generation;
CREATE TABLE IF NOT EXISTS fleet_certificates (
  domain TEXT PRIMARY KEY,
  cert_pem TEXT NOT NULL,
  key_sealed BYTEA NOT NULL,
  key_nonce BYTEA NOT NULL,
  generation BIGINT NOT NULL,
  not_after TIMESTAMPTZ NOT NULL,
  sans TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS fleet_acme_accounts (
  directory_url TEXT PRIMARY KEY,
  sealed BYTEA NOT NULL,
  nonce BYTEA NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS fleet_acme_jobs (
  id UUID PRIMARY KEY,
  identifier TEXT NOT NULL UNIQUE,
  state TEXT NOT NULL CHECK (state IN (
    'pending', 'publishing', 'ready', 'validating', 'finalizing', 'active', 'failed'
  )),
  fence BIGINT NOT NULL,
  lease_holder TEXT NOT NULL,
  lease_expires_at TIMESTAMPTZ NOT NULL,
  order_url TEXT,
  challenge TEXT,
  provider_change_ids TEXT,
  attempt BIGINT NOT NULL DEFAULT 0,
  next_attempt_at TIMESTAMPTZ,
  last_error TEXT,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
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

fn row_to_certificate(row: &tokio_postgres::Row) -> Result<CertRecord, FleetError> {
    let sans: String = certificate_column(row, 6, "sans")?;
    Ok(CertRecord {
        domain: certificate_column(row, 0, "domain")?,
        cert_pem: certificate_column(row, 1, "cert_pem")?,
        key_sealed: certificate_column(row, 2, "key_sealed")?,
        key_nonce: certificate_column(row, 3, "key_nonce")?,
        generation: certificate_column(row, 4, "generation")?,
        not_after: certificate_column(row, 5, "not_after")?,
        sans: serde_json::from_str(&sans)
            .map_err(|error| FleetError::InvalidCertificateRow(format!("sans: {error}")))?,
    })
}

fn row_to_acme_job(row: &tokio_postgres::Row) -> Result<AcmeJob, FleetError> {
    let state: String = acme_job_column(row, 2, "state")?;
    let challenge: Option<String> = acme_job_column(row, 5, "challenge")?;
    let provider_change_ids: Option<String> = acme_job_column(row, 6, "provider_change_ids")?;
    Ok(AcmeJob {
        id: acme_job_column(row, 0, "id")?,
        identifier: acme_job_column(row, 1, "identifier")?,
        state: acme_job_state_from_str(&state)?,
        fence: acme_job_column(row, 3, "fence")?,
        order_url: acme_job_column(row, 4, "order_url")?,
        challenge: parse_optional_json("challenge", challenge)?,
        provider_change_ids: parse_optional_json("provider_change_ids", provider_change_ids)?,
        attempt: acme_job_column(row, 7, "attempt")?,
        next_attempt_at: acme_job_column(row, 8, "next_attempt_at")?,
        last_error: acme_job_column(row, 9, "last_error")?,
        updated_at: acme_job_column(row, 10, "updated_at")?,
    })
}

fn share_column<T>(row: &tokio_postgres::Row, index: usize, name: &str) -> Result<T, FleetError>
where
    for<'a> T: tokio_postgres::types::FromSql<'a>,
{
    row.try_get(index)
        .map_err(|error| FleetError::InvalidShareRow(format!("{name}: {error}")))
}

fn certificate_column<T>(
    row: &tokio_postgres::Row,
    index: usize,
    name: &str,
) -> Result<T, FleetError>
where
    for<'a> T: tokio_postgres::types::FromSql<'a>,
{
    row.try_get(index)
        .map_err(|error| FleetError::InvalidCertificateRow(format!("{name}: {error}")))
}

fn acme_job_column<T>(row: &tokio_postgres::Row, index: usize, name: &str) -> Result<T, FleetError>
where
    for<'a> T: tokio_postgres::types::FromSql<'a>,
{
    row.try_get(index)
        .map_err(|error| FleetError::InvalidAcmeJobRow(format!("{name}: {error}")))
}

fn serialize_json<T: serde::Serialize>(value: &T, column: &str) -> Result<String, FleetError> {
    serde_json::to_string(value)
        .map_err(|error| FleetError::Config(format!("serialize {column} as JSON: {error}")))
}

fn serialize_optional_json(
    value: &Option<Value>,
    column: &str,
) -> Result<Option<String>, FleetError> {
    value
        .as_ref()
        .map(|value| serialize_json(value, column))
        .transpose()
}

fn parse_optional_json(column: &str, value: Option<String>) -> Result<Option<Value>, FleetError> {
    value
        .map(|value| {
            serde_json::from_str(&value)
                .map_err(|error| FleetError::InvalidAcmeJobRow(format!("{column}: {error}")))
        })
        .transpose()
}

fn acme_job_state_as_str(state: &AcmeJobState) -> &'static str {
    match state {
        AcmeJobState::Pending => "pending",
        AcmeJobState::Publishing => "publishing",
        AcmeJobState::Ready => "ready",
        AcmeJobState::Validating => "validating",
        AcmeJobState::Finalizing => "finalizing",
        AcmeJobState::Active => "active",
        AcmeJobState::Failed => "failed",
    }
}

fn acme_job_state_from_str(state: &str) -> Result<AcmeJobState, FleetError> {
    match state {
        "pending" => Ok(AcmeJobState::Pending),
        "publishing" => Ok(AcmeJobState::Publishing),
        "ready" => Ok(AcmeJobState::Ready),
        "validating" => Ok(AcmeJobState::Validating),
        "finalizing" => Ok(AcmeJobState::Finalizing),
        "active" => Ok(AcmeJobState::Active),
        "failed" => Ok(AcmeJobState::Failed),
        _ => Err(FleetError::InvalidAcmeJobRow(format!(
            "invalid state: {state}"
        ))),
    }
}

fn duration_as_interval(duration: Duration) -> Result<String, FleetError> {
    let microseconds = i64::try_from(duration.as_micros())
        .map_err(|_| FleetError::Config("ACME lease duration is too long".into()))?;
    Ok(format!("{microseconds} microseconds"))
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
    use std::time::Duration;

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

    async fn cleanup_acme_job(fleet: &PostgresFleet, identifier: &str) -> Result<(), FleetError> {
        let client = fleet.pool.get().await?;
        client
            .execute(
                "DELETE FROM fleet_acme_jobs WHERE identifier = $1",
                &[&identifier],
            )
            .await?;
        Ok(())
    }

    async fn cleanup_certificate(fleet: &PostgresFleet, domain: &str) -> Result<(), FleetError> {
        let client = fleet.pool.get().await?;
        client
            .execute(
                "DELETE FROM fleet_certificates WHERE domain = $1",
                &[&domain],
            )
            .await?;
        Ok(())
    }

    async fn cleanup_acme_account(
        fleet: &PostgresFleet,
        directory_url: &str,
    ) -> Result<(), FleetError> {
        let client = fleet.pool.get().await?;
        client
            .execute(
                "DELETE FROM fleet_acme_accounts WHERE directory_url = $1",
                &[&directory_url],
            )
            .await?;
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
    fn fleet_schema_defines_acme_tables() {
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_certificates"));
        assert!(FLEET_SCHEMA.contains("generation BIGINT"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_acme_accounts"));
        assert!(FLEET_SCHEMA.contains("CREATE TABLE IF NOT EXISTS fleet_acme_jobs"));
        assert!(FLEET_SCHEMA.contains("fence BIGINT"));
    }

    #[test]
    fn cert_refresh_listener_uses_expected_channel() {
        assert_eq!(CERT_REFRESH_LISTEN_COMMAND, "LISTEN tarit_cert_refresh");
    }

    #[tokio::test]
    async fn fleet_connect_is_concurrent_safe_when_database_is_configured() -> Result<(), FleetError>
    {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping PostgreSQL concurrent connect test: TARIT_TEST_DATABASE_URL is absent"
            );
            return Ok(());
        };
        if database_url.is_empty() {
            eprintln!(
                "skipping PostgreSQL concurrent connect test: TARIT_TEST_DATABASE_URL is empty"
            );
            return Ok(());
        }

        let mut connections = Vec::new();
        for _ in 0..8 {
            let database_url = database_url.clone();
            connections.push(tokio::spawn(async move {
                PostgresFleet::connect(&database_url).await
            }));
        }
        for connection in connections {
            connection
                .await
                .map_err(|error| FleetError::Config(format!("connect task failed: {error}")))??;
        }
        Ok(())
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
    async fn acme_job_claim_respects_backoff_next_attempt_at() -> Result<(), FleetError> {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            eprintln!("skipping PostgreSQL ACME backoff test: TARIT_TEST_DATABASE_URL is absent");
            return Ok(());
        };
        if database_url.is_empty() {
            eprintln!("skipping PostgreSQL ACME backoff test: TARIT_TEST_DATABASE_URL is empty");
            return Ok(());
        }

        let fleet = PostgresFleet::connect(&database_url).await?;
        let identifier = format!("*.backofftest-{}.example.com", Uuid::new_v4());
        let result = async {
            let claimed = fleet
                .claim_acme_job(&identifier, "node-a", Duration::from_millis(200))
                .await?
                .ok_or_else(|| FleetError::Config("initial ACME job claim was denied".into()))?;

            let mut failed = claimed.clone();
            failed.state = AcmeJobState::Failed;
            failed.attempt = 1;
            failed.next_attempt_at = Some(Utc::now() + chrono::Duration::seconds(3_600));
            failed.updated_at = Utc::now();
            if !fleet.save_acme_job(&failed, claimed.fence).await? {
                return Err(FleetError::Config("failed ACME job save was rejected".into()));
            }

            tokio::time::sleep(Duration::from_millis(400)).await;

            if fleet
                .claim_acme_job(&identifier, "node-b", Duration::from_millis(200))
                .await?
                .is_some()
            {
                return Err(FleetError::Config(
                    "expired-lease ACME job was reclaimed before its backoff elapsed".into(),
                ));
            }

            let mut ready = failed.clone();
            ready.next_attempt_at = Some(Utc::now() - chrono::Duration::seconds(10));
            ready.updated_at = Utc::now();
            if !fleet.save_acme_job(&ready, failed.fence).await? {
                return Err(FleetError::Config("ready ACME job save was rejected".into()));
            }

            if fleet
                .claim_acme_job(&identifier, "node-b", Duration::from_millis(200))
                .await?
                .is_none()
            {
                return Err(FleetError::Config(
                    "ACME job was not reclaimable after its backoff elapsed".into(),
                ));
            }
            Ok(())
        }
        .await;

        result.and(cleanup_acme_job(&fleet, &identifier).await)
    }

    #[tokio::test]
    async fn acme_job_claim_is_fenced_and_singleflight() -> Result<(), FleetError> {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            eprintln!("skipping PostgreSQL ACME job test: TARIT_TEST_DATABASE_URL is absent");
            return Ok(());
        };
        if database_url.is_empty() {
            eprintln!("skipping PostgreSQL ACME job test: TARIT_TEST_DATABASE_URL is empty");
            return Ok(());
        }

        let fleet = PostgresFleet::connect(&database_url).await?;
        let identifier = format!("*.claimtest-{}.example.com", Uuid::new_v4());
        let result = async {
            let first = fleet
                .claim_acme_job(&identifier, "node-a", Duration::from_secs(30))
                .await?
                .ok_or_else(|| FleetError::Config("initial ACME job claim was denied".into()))?;
            if fleet
                .get_acme_job(&identifier)
                .await?
                .as_ref()
                .map(|job| job.fence)
                != Some(first.fence)
            {
                return Err(FleetError::Config(
                    "claimed ACME job was not persisted".into(),
                ));
            }
            if !fleet
                .renew_acme_lease(first.id, first.fence, "node-a", Duration::from_secs(30))
                .await?
            {
                return Err(FleetError::Config(
                    "ACME job lease renewal was rejected for its holder".into(),
                ));
            }
            if fleet
                .renew_acme_lease(first.id, first.fence, "node-b", Duration::from_secs(30))
                .await?
            {
                return Err(FleetError::Config(
                    "ACME job lease renewal was accepted for another holder".into(),
                ));
            }
            if fleet
                .claim_acme_job(&identifier, "node-b", Duration::from_secs(30))
                .await?
                .is_some()
            {
                return Err(FleetError::Config(
                    "second ACME job claim succeeded while lease was live".into(),
                ));
            }
            let mut saved = first.clone();
            saved.state = AcmeJobState::Publishing;
            saved.order_url = Some("https://acme.example.test/order/1".into());
            saved.challenge = Some(serde_json::json!({"token": "challenge-token"}));
            saved.provider_change_ids = Some(serde_json::json!(["change-1"]));
            saved.attempt = 1;
            saved.next_attempt_at = Some(Utc::now() + chrono::Duration::seconds(30));
            saved.updated_at = Utc::now();
            if !fleet.save_acme_job(&saved, saved.fence).await? {
                return Err(FleetError::Config(
                    "current ACME job save was rejected".into(),
                ));
            }
            let fetched = fleet
                .get_acme_job(&identifier)
                .await?
                .ok_or_else(|| FleetError::Config("saved ACME job is missing".into()))?;
            if fetched.state != saved.state
                || fetched.order_url != saved.order_url
                || fetched.challenge != saved.challenge
                || fetched.provider_change_ids != saved.provider_change_ids
                || fetched.attempt != saved.attempt
            {
                return Err(FleetError::Config(
                    "saved ACME job did not round-trip".into(),
                ));
            }
            if fleet.save_acme_job(&first, first.fence - 1).await? {
                return Err(FleetError::Config(
                    "stale ACME job save was accepted".into(),
                ));
            }
            Ok(())
        }
        .await;

        result.and(cleanup_acme_job(&fleet, &identifier).await)
    }

    #[tokio::test]
    async fn certificate_generation_increases_and_changes_are_listed_when_database_is_configured(
    ) -> Result<(), FleetError> {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            eprintln!("skipping PostgreSQL certificate test: TARIT_TEST_DATABASE_URL is absent");
            return Ok(());
        };
        if database_url.is_empty() {
            eprintln!("skipping PostgreSQL certificate test: TARIT_TEST_DATABASE_URL is empty");
            return Ok(());
        }

        let fleet = PostgresFleet::connect(&database_url).await?;
        let domain = format!("*.certtest-{}.example.com", Uuid::new_v4());
        let mut certificate = CertRecord {
            domain: domain.clone(),
            cert_pem: "first certificate".into(),
            key_sealed: b"sealed key".to_vec(),
            key_nonce: b"nonce".to_vec(),
            generation: 0,
            not_after: Utc::now() + chrono::Duration::days(30),
            sans: vec![domain.clone()],
        };
        let result = async {
            fleet.upsert_certificate(&certificate).await?;
            let first = fleet
                .get_certificate(&domain)
                .await?
                .ok_or_else(|| FleetError::Config("first certificate is missing".into()))?;

            certificate.cert_pem = "second certificate".into();
            fleet.upsert_certificate(&certificate).await?;
            let second = fleet
                .get_certificate(&domain)
                .await?
                .ok_or_else(|| FleetError::Config("second certificate is missing".into()))?;
            if second.generation <= first.generation {
                return Err(FleetError::Config(
                    "certificate generation did not strictly increase".into(),
                ));
            }
            if fleet.max_cert_generation().await? < second.generation {
                return Err(FleetError::Config(
                    "maximum certificate generation is stale".into(),
                ));
            }
            if !fleet
                .certificates_since(first.generation)
                .await?
                .iter()
                .any(|cert| cert.domain == domain && cert.generation == second.generation)
            {
                return Err(FleetError::Config(
                    "certificate change is missing from incremental query".into(),
                ));
            }
            fleet.notify_cert_refresh().await?;
            Ok(())
        }
        .await;

        result.and(cleanup_certificate(&fleet, &domain).await)
    }

    #[tokio::test]
    async fn certificate_publish_rejects_stale_fences_when_database_is_configured(
    ) -> Result<(), FleetError> {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping PostgreSQL certificate publish test: TARIT_TEST_DATABASE_URL is absent"
            );
            return Ok(());
        };
        if database_url.is_empty() {
            eprintln!(
                "skipping PostgreSQL certificate publish test: TARIT_TEST_DATABASE_URL is empty"
            );
            return Ok(());
        }

        let fleet = PostgresFleet::connect(&database_url).await?;
        let suffix = Uuid::new_v4();
        let identifier = format!("*.publishtest-{suffix}.example.com");
        let domain = format!("*.cert-{suffix}.example.com");
        let certificate = CertRecord {
            domain: domain.clone(),
            cert_pem: "published certificate".into(),
            key_sealed: b"sealed key".to_vec(),
            key_nonce: b"nonce".to_vec(),
            generation: 0,
            not_after: Utc::now() + chrono::Duration::days(30),
            sans: vec![domain.clone()],
        };
        let result = async {
            let job = fleet
                .claim_acme_job(&identifier, "node-a", Duration::from_secs(30))
                .await?
                .ok_or_else(|| {
                    FleetError::Config("initial certificate job claim was denied".into())
                })?;
            let max_before = fleet.max_cert_generation().await?;

            if fleet
                .publish_certificate(&certificate, &identifier, job.fence - 1)
                .await?
            {
                return Err(FleetError::Config(
                    "stale ACME job fence published a certificate".into(),
                ));
            }
            if fleet.get_certificate(&domain).await?.is_some() {
                return Err(FleetError::Config(
                    "stale ACME job fence changed certificate storage".into(),
                ));
            }

            if !fleet
                .publish_certificate(&certificate, &identifier, job.fence)
                .await?
            {
                return Err(FleetError::Config(
                    "current ACME job fence could not publish a certificate".into(),
                ));
            }
            let published = fleet
                .get_certificate(&domain)
                .await?
                .ok_or_else(|| FleetError::Config("published certificate is missing".into()))?;
            if published.generation <= max_before
                || fleet.max_cert_generation().await? < published.generation
            {
                return Err(FleetError::Config(
                    "certificate publish did not advance generation".into(),
                ));
            }
            if fleet.get_acme_job(&identifier).await?.map(|job| job.state)
                != Some(AcmeJobState::Active)
            {
                return Err(FleetError::Config(
                    "certificate publish did not activate its ACME job".into(),
                ));
            }
            Ok(())
        }
        .await;

        result
            .and(cleanup_certificate(&fleet, &domain).await)
            .and(cleanup_acme_job(&fleet, &identifier).await)
    }

    #[tokio::test]
    async fn acme_account_round_trips_sealed_credentials_when_database_is_configured(
    ) -> Result<(), FleetError> {
        let Ok(database_url) = std::env::var("TARIT_TEST_DATABASE_URL") else {
            eprintln!("skipping PostgreSQL ACME account test: TARIT_TEST_DATABASE_URL is absent");
            return Ok(());
        };
        if database_url.is_empty() {
            eprintln!("skipping PostgreSQL ACME account test: TARIT_TEST_DATABASE_URL is empty");
            return Ok(());
        }

        let fleet = PostgresFleet::connect(&database_url).await?;
        let directory_url = format!("https://acme.example.test/{}", Uuid::new_v4());
        let sealed = b"already-encrypted-account-credential";
        let result = async {
            fleet
                .put_acme_account(&directory_url, sealed, b"account-nonce")
                .await?;
            let account = fleet
                .get_acme_account(&directory_url)
                .await?
                .ok_or_else(|| FleetError::Config("ACME account is missing".into()))?;
            if account
                != (AcmeAccountSecret {
                    sealed: sealed.to_vec(),
                    nonce: b"account-nonce".to_vec(),
                })
            {
                return Err(FleetError::Config(
                    "sealed ACME account credential and nonce did not round-trip".into(),
                ));
            }
            Ok(())
        }
        .await;

        result.and(cleanup_acme_account(&fleet, &directory_url).await)
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
