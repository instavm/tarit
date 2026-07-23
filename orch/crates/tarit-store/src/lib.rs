//! SQLite persistence for VM and execution records.

use chrono::{DateTime, Utc};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use std::path::Path;
use std::time::Duration;
use tarit_types::{
    AuditEvent, ExecutionRecord, ExecutionStatus, ShareRecord, ShareVisibility, SshKeyRecord,
    UsageEvent, UsageKind, VmRecord, VmStartupPath, VmStatus,
};
use uuid::Uuid;

/// Cluster roster entry for one orchestrator host.
#[derive(Debug, Clone)]
pub struct HostRecord {
    pub host_id: String,
    pub rpc_addr: Option<String>,
    pub sandbox_count: usize,
    pub free_vcpus: u64,
    pub free_memory_mib: u64,
    pub healthy: bool,
    pub last_heartbeat: DateTime<Utc>,
}

/// Registered immutable rootfs image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRecord {
    pub name: String,
    pub tag: String,
    pub rootfs_path: String,
    pub created_at: DateTime<Utc>,
    pub size_bytes: u64,
    pub source_ref: String,
    pub golden_snapshot_path: Option<String>,
}

/// Ownership record for a node-local snapshot file, so restore can verify that
/// the caller owns the snapshot before its path is handed to the VMM (R-006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRecord {
    pub path: String,
    /// Snapshot-owned copy of the VM's private CoW overlay. This must never
    /// point at the live VM overlay: deleting the source VM must not invalidate
    /// a snapshot, and separate restores must not share a writable upper.
    pub overlay_path: Option<String>,
    pub host_id: String,
    pub owner_key: Option<String>,
    pub api_key_id: Option<String>,
    pub vm_id: Uuid,
    /// Resource shape and boot inputs captured with the snapshot ownership row.
    /// These are optional only for rows created before the metadata migration;
    /// production restore must fail closed when they are absent.
    pub memory_mib: Option<u64>,
    pub vcpus: Option<u8>,
    pub kernel_path: Option<String>,
    pub rootfs_path: Option<String>,
    pub cmdline: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("not found")]
    NotFound,

    #[error("conflict: {0}")]
    Conflict(String),
}

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmQuotaReservationOutcome {
    Reserved,
    QuotaExceeded,
    IdConflict,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        // WAL + NORMAL sync turns each write from an fsync-per-statement (rollback
        // journal, ~5-70ms) into an appended WAL frame (~100us), and busy_timeout
        // lets a blocked reader/writer wait instead of erroring. This is what lets
        // the single shared connection sustain a 200-wide burst (create + exec +
        // 15ms status polling) without serializing on fsync.
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS vms (
               id TEXT PRIMARY KEY NOT NULL,
               host_id TEXT NOT NULL,
               owner_key TEXT,
               api_key_id TEXT,
               status TEXT NOT NULL,
               revision INTEGER NOT NULL DEFAULT 1,
               startup_path TEXT,
               memory_mib INTEGER NOT NULL,
               vcpus INTEGER NOT NULL,
               kernel_path TEXT NOT NULL,
               rootfs_path TEXT,
               cmdline TEXT NOT NULL,
               socket_path TEXT,
               pid INTEGER,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS executions (
               id TEXT PRIMARY KEY NOT NULL,
               vm_id TEXT NOT NULL,
               command TEXT NOT NULL,
               timeout_ms INTEGER NOT NULL,
               status TEXT NOT NULL,
               exit_code INTEGER,
               stdout TEXT,
               stderr TEXT,
               duration_ms INTEGER,
               error TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS hosts (
               host_id TEXT PRIMARY KEY NOT NULL,
               rpc_addr TEXT,
               sandbox_count INTEGER NOT NULL DEFAULT 0,
               free_vcpus INTEGER NOT NULL DEFAULT 0,
               free_memory_mib INTEGER NOT NULL DEFAULT 0,
               healthy INTEGER NOT NULL DEFAULT 1,
               last_heartbeat TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS ssh_keys (
               id TEXT PRIMARY KEY NOT NULL,
               owner_key TEXT NOT NULL,
               fingerprint TEXT NOT NULL,
               public_key TEXT NOT NULL,
               key_type TEXT NOT NULL,
               created_at TEXT NOT NULL,
               is_active INTEGER NOT NULL DEFAULT 1
             );
             CREATE TABLE IF NOT EXISTS images (
               name TEXT NOT NULL,
               tag TEXT NOT NULL,
               rootfs_path TEXT NOT NULL,
               created_at TEXT NOT NULL,
               size_bytes INTEGER NOT NULL,
               source_ref TEXT NOT NULL,
               golden_snapshot_path TEXT,
               PRIMARY KEY (name, tag)
             );
             CREATE TABLE IF NOT EXISTS usage_outbox (
               id TEXT PRIMARY KEY,
               api_key_id TEXT NOT NULL,
               owner_key TEXT NOT NULL,
               host_id TEXT NOT NULL,
               vm_id TEXT NOT NULL,
               kind TEXT NOT NULL,
               seconds REAL,
               duration_ms INTEGER,
               window_start TEXT NOT NULL,
               window_end TEXT NOT NULL,
               created_at TEXT NOT NULL,
               sent INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS audit_outbox (
               id TEXT PRIMARY KEY,
               api_key_id TEXT NOT NULL,
               owner_key TEXT NOT NULL,
               host_id TEXT NOT NULL,
               vm_id TEXT,
               action TEXT NOT NULL,
               outcome TEXT NOT NULL,
               detail TEXT,
               created_at TEXT NOT NULL,
               sent INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS billing_watermark (
               vm_id TEXT PRIMARY KEY,
               last_billed_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS snapshots (
               path TEXT PRIMARY KEY NOT NULL,
               overlay_path TEXT,
               host_id TEXT NOT NULL,
               owner_key TEXT,
               api_key_id TEXT,
               vm_id TEXT NOT NULL,
               memory_mib INTEGER,
               vcpus INTEGER,
               kernel_path TEXT,
               rootfs_path TEXT,
               cmdline TEXT,
               created_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS vm_quota_reservations (
               id TEXT PRIMARY KEY NOT NULL,
               owner_key TEXT NOT NULL,
               expires_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS shares (
               id TEXT PRIMARY KEY NOT NULL,
               slug TEXT NOT NULL UNIQUE,
               owner_key TEXT NOT NULL,
               vm_id TEXT NOT NULL,
               guest_port INTEGER NOT NULL CHECK (guest_port BETWEEN 1 AND 65535),
               visibility TEXT NOT NULL CHECK (visibility IN ('public', 'private')),
               token_version INTEGER NOT NULL DEFAULT 0,
               revoked_at TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS usage_outbox_unsent ON usage_outbox(sent);
             CREATE INDEX IF NOT EXISTS audit_outbox_unsent ON audit_outbox(sent);
             CREATE INDEX IF NOT EXISTS shares_owner ON shares(owner_key, created_at DESC);
             CREATE INDEX IF NOT EXISTS shares_vm ON shares(vm_id);
             CREATE INDEX IF NOT EXISTS vm_quota_reservations_owner_expiry
               ON vm_quota_reservations(owner_key, expires_at);",
        )?;
        ensure_column(&conn, "vms", "owner_key", "TEXT")?;
        ensure_column(&conn, "vms", "api_key_id", "TEXT")?;
        ensure_column(&conn, "vms", "revision", "INTEGER NOT NULL DEFAULT 1")?;
        ensure_column(&conn, "vms", "startup_path", "TEXT")?;
        ensure_column(&conn, "snapshots", "memory_mib", "INTEGER")?;
        ensure_column(&conn, "snapshots", "overlay_path", "TEXT")?;
        ensure_column(&conn, "snapshots", "vcpus", "INTEGER")?;
        ensure_column(&conn, "snapshots", "kernel_path", "TEXT")?;
        ensure_column(&conn, "snapshots", "rootfs_path", "TEXT")?;
        ensure_column(&conn, "snapshots", "cmdline", "TEXT")?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_ssh_keys_fingerprint_active ON ssh_keys (fingerprint, is_active)",
            [],
        )?;
        Ok(Self { conn })
    }

    pub fn insert_vm(&self, vm: &VmRecord) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "INSERT INTO vms (
              id, host_id, owner_key, api_key_id, status, revision, startup_path, memory_mib,
              vcpus, kernel_path, rootfs_path, cmdline, socket_path, pid, created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
             ON CONFLICT(id) DO UPDATE SET
               owner_key = excluded.owner_key,
               api_key_id = excluded.api_key_id,
               status = excluded.status,
               revision = excluded.revision,
               startup_path = excluded.startup_path,
               memory_mib = excluded.memory_mib,
               vcpus = excluded.vcpus,
               kernel_path = excluded.kernel_path,
               rootfs_path = excluded.rootfs_path,
               cmdline = excluded.cmdline,
               socket_path = excluded.socket_path,
               pid = excluded.pid,
               updated_at = excluded.updated_at
             WHERE vms.host_id = excluded.host_id
               AND vms.created_at = excluded.created_at
               AND vms.revision < excluded.revision",
            params![
                vm.id.to_string(),
                vm.host_id,
                vm.owner_key,
                vm.api_key_id,
                vm.status.as_str(),
                u64_to_sql_i64(vm.revision)?,
                vm.startup_path.map(VmStartupPath::as_str),
                vm.memory_mib,
                vm.vcpus,
                vm.kernel_path,
                vm.rootfs_path,
                vm.cmdline,
                vm.socket_path,
                vm.pid,
                vm.created_at.to_rfc3339(),
                vm.updated_at.to_rfc3339(),
            ],
        )?;
        if changed == 0 {
            let current = self.get_vm(vm.id)?;
            if current.host_id != vm.host_id || current.created_at != vm.created_at {
                return Err(StoreError::Conflict(format!(
                    "VM {} belongs to another resource incarnation",
                    vm.id
                )));
            }
            if current.revision == vm.revision && current != *vm {
                return Err(StoreError::Conflict(format!(
                    "VM {} has two different records at revision {}",
                    vm.id, vm.revision
                )));
            }
            // A strictly newer durable record already won. Treat the delayed
            // write as an idempotent no-op instead of regressing it.
        }
        Ok(())
    }

    pub fn get_vm(&self, id: Uuid) -> Result<VmRecord, StoreError> {
        self.conn
            .query_row(
                "SELECT id, host_id, owner_key, api_key_id, status, revision, startup_path,
                        memory_mib, vcpus, kernel_path, rootfs_path, cmdline, socket_path, pid,
                        created_at, updated_at
                 FROM vms WHERE id = ?1",
                params![id.to_string()],
                row_to_vm,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Record ownership of a node-local snapshot file. `INSERT OR REPLACE` so a
    /// path that is re-snapshotted keeps a single current owner record.
    pub fn insert_snapshot(&self, snap: &SnapshotRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO snapshots (
               path, overlay_path, host_id, owner_key, api_key_id, vm_id, memory_mib, vcpus,
               kernel_path, rootfs_path, cmdline, created_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                snap.path,
                snap.overlay_path,
                snap.host_id,
                snap.owner_key,
                snap.api_key_id,
                snap.vm_id.to_string(),
                snap.memory_mib,
                snap.vcpus,
                snap.kernel_path,
                snap.rootfs_path,
                snap.cmdline,
                snap.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Look up the ownership record for a snapshot path, if one exists.
    pub fn get_snapshot(&self, path: &str) -> Result<Option<SnapshotRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT path, overlay_path, host_id, owner_key, api_key_id, vm_id, memory_mib, vcpus,
                        kernel_path, rootfs_path, cmdline, created_at
                 FROM snapshots WHERE path = ?1",
                params![path],
                row_to_snapshot,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn insert_share(&self, share: &ShareRecord) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO shares (
               id, slug, owner_key, vm_id, guest_port, visibility, token_version, revoked_at,
               created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    share.id.to_string(),
                    share.slug,
                    share.owner_key,
                    share.vm_id.to_string(),
                    i64::from(share.guest_port),
                    share_visibility_as_str(share.visibility),
                    u64_to_sql_i64(share.token_version)?,
                    share.revoked_at.as_ref().map(|ts| ts.to_rfc3339()),
                    share.created_at.to_rfc3339(),
                    share.updated_at.to_rfc3339(),
                ],
            )
            .map_err(share_error_from_sqlite)?;
        Ok(())
    }

    pub fn get_share(&self, id: Uuid) -> Result<ShareRecord, StoreError> {
        self.conn
            .query_row(
                "SELECT id, slug, owner_key, vm_id, guest_port, visibility, token_version,
                        revoked_at, created_at, updated_at
                 FROM shares WHERE id = ?1",
                params![id.to_string()],
                row_to_share,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    pub fn get_share_by_slug(&self, slug: &str) -> Result<Option<ShareRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT id, slug, owner_key, vm_id, guest_port, visibility, token_version,
                        revoked_at, created_at, updated_at
                 FROM shares WHERE slug = ?1",
                params![slug],
                row_to_share,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn list_shares(&self, owner_key: &str) -> Result<Vec<ShareRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, slug, owner_key, vm_id, guest_port, visibility, token_version,
                    revoked_at, created_at, updated_at
             FROM shares WHERE owner_key = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![owner_key], row_to_share)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn update_share(&self, share: &ShareRecord) -> Result<(), StoreError> {
        let updated = self
            .conn
            .execute(
                "UPDATE shares SET
              slug = ?2, vm_id = ?3, guest_port = ?4, visibility = ?5, token_version = ?6,
              revoked_at = ?7, updated_at = ?8
             WHERE id = ?1",
                params![
                    share.id.to_string(),
                    share.slug,
                    share.vm_id.to_string(),
                    i64::from(share.guest_port),
                    share_visibility_as_str(share.visibility),
                    u64_to_sql_i64(share.token_version)?,
                    share.revoked_at.as_ref().map(|ts| ts.to_rfc3339()),
                    share.updated_at.to_rfc3339(),
                ],
            )
            .map_err(share_error_from_sqlite)?;
        if updated == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Update an active share only when it still has the version read by the
    /// caller. This protects token rotation and terminal revocation from
    /// concurrent writers.
    pub fn update_share_if_current(
        &self,
        share: &ShareRecord,
        expected_token_version: u64,
    ) -> Result<(), StoreError> {
        let updated = self
            .conn
            .execute(
                "UPDATE shares SET
               slug = ?2, vm_id = ?3, guest_port = ?4, visibility = ?5, token_version = ?6,
               revoked_at = ?7, updated_at = ?8
             WHERE id = ?1 AND token_version = ?9 AND revoked_at IS NULL",
                params![
                    share.id.to_string(),
                    share.slug,
                    share.vm_id.to_string(),
                    i64::from(share.guest_port),
                    share_visibility_as_str(share.visibility),
                    u64_to_sql_i64(share.token_version)?,
                    share.revoked_at.as_ref().map(|ts| ts.to_rfc3339()),
                    share.updated_at.to_rfc3339(),
                    u64_to_sql_i64(expected_token_version)?,
                ],
            )
            .map_err(share_error_from_sqlite)?;
        if updated == 0 {
            return Err(StoreError::Conflict(
                "share was modified or revoked concurrently".into(),
            ));
        }
        Ok(())
    }

    pub fn list_vms(&self) -> Result<Vec<VmRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, host_id, owner_key, api_key_id, status, revision, startup_path,
                    memory_mib, vcpus, kernel_path, rootfs_path, cmdline, socket_path, pid,
                    created_at, updated_at
             FROM vms ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_vm)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Atomically reserve one tenant VM slot in single-host mode. Active VM rows
    /// and unexpired reservations are counted in one SQLite transaction, so a
    /// concurrent create burst cannot pass a check-then-create quota race.
    pub fn reserve_vm_quota(
        &self,
        owner_key: &str,
        id: Uuid,
        max_vms: usize,
        expires_at: DateTime<Utc>,
    ) -> Result<VmQuotaReservationOutcome, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "DELETE FROM vm_quota_reservations WHERE expires_at <= ?1",
            params![now],
        )?;
        let existing_owner = tx
            .query_row(
                "SELECT owner_key FROM vm_quota_reservations WHERE id = ?1",
                params![id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(existing_owner) = existing_owner {
            tx.commit()?;
            let _ = existing_owner;
            return Ok(VmQuotaReservationOutcome::IdConflict);
        }
        let already_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM vms WHERE id = ?1)",
            params![id.to_string()],
            |row| row.get(0),
        )?;
        if already_exists {
            tx.commit()?;
            return Ok(VmQuotaReservationOutcome::IdConflict);
        }
        let active: i64 = tx.query_row(
            "SELECT COUNT(*) FROM (
               SELECT id FROM vms
                WHERE owner_key = ?1 AND status IN ('creating','running','paused','suspended')
               UNION
               SELECT id FROM vm_quota_reservations
                WHERE owner_key = ?1 AND expires_at > ?2
             )",
            params![owner_key, now],
            |row| row.get(0),
        )?;
        if active >= i64::try_from(max_vms).unwrap_or(i64::MAX) {
            tx.commit()?;
            return Ok(VmQuotaReservationOutcome::QuotaExceeded);
        }
        tx.execute(
            "INSERT INTO vm_quota_reservations (id, owner_key, expires_at)
             VALUES (?1,?2,?3)",
            params![id.to_string(), owner_key, expires_at.to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(VmQuotaReservationOutcome::Reserved)
    }

    pub fn release_vm_quota(&self, owner_key: &str, id: Uuid) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM vm_quota_reservations WHERE id = ?1 AND owner_key = ?2",
            params![id.to_string(), owner_key],
        )?;
        Ok(())
    }

    pub fn healthcheck(&self) -> Result<(), StoreError> {
        self.conn.query_row("SELECT 1", [], |_| Ok(()))?;
        Ok(())
    }

    pub fn update_vm(&self, vm: &VmRecord) -> Result<(), StoreError> {
        let n = self.conn.execute(
            "UPDATE vms SET
               host_id = ?2, owner_key = ?3, api_key_id = ?4, status = ?5, revision = ?6,
               startup_path = ?7, memory_mib = ?8, vcpus = ?9, kernel_path = ?10,
               rootfs_path = ?11, cmdline = ?12, socket_path = ?13, pid = ?14, updated_at = ?15
             WHERE id = ?1 AND revision < ?6",
            params![
                vm.id.to_string(),
                vm.host_id,
                vm.owner_key,
                vm.api_key_id,
                vm.status.as_str(),
                u64_to_sql_i64(vm.revision)?,
                vm.startup_path.map(VmStartupPath::as_str),
                vm.memory_mib,
                vm.vcpus,
                vm.kernel_path,
                vm.rootfs_path,
                vm.cmdline,
                vm.socket_path,
                vm.pid,
                vm.updated_at.to_rfc3339(),
            ],
        )?;
        if n == 0 {
            match self.get_vm(vm.id) {
                Ok(current)
                    if current.host_id != vm.host_id || current.created_at != vm.created_at =>
                {
                    return Err(StoreError::Conflict(format!(
                        "VM {} belongs to another resource incarnation",
                        vm.id
                    )))
                }
                Ok(current) if current.revision == vm.revision && current != *vm => {
                    return Err(StoreError::Conflict(format!(
                        "VM {} has two different records at revision {}",
                        vm.id, vm.revision
                    )))
                }
                // Exact retry or a delayed transition after a newer revision
                // already committed. Neither may regress durable state.
                Ok(_) => return Ok(()),
                Err(StoreError::NotFound) => return Err(StoreError::NotFound),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn update_vm_status(&self, id: Uuid, status: VmStatus) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        let n = self.conn.execute(
            "UPDATE vms SET status = ?2, revision = revision + 1, updated_at = ?3 WHERE id = ?1",
            params![id.to_string(), status.as_str(), now],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    pub fn delete_vm(&self, id: Uuid) -> Result<(), StoreError> {
        let n = self
            .conn
            .execute("DELETE FROM vms WHERE id = ?1", params![id.to_string()])?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    pub fn insert_execution(&self, exec: &ExecutionRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO executions (
               id, vm_id, command, timeout_ms, status, exit_code, stdout, stderr,
               duration_ms, error, created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                exec.id.to_string(),
                exec.vm_id.to_string(),
                exec.command,
                exec.timeout_ms,
                exec.status.as_str(),
                exec.exit_code,
                exec.stdout,
                exec.stderr,
                exec.duration_ms,
                exec.error,
                exec.created_at.to_rfc3339(),
                exec.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_execution(&self, id: Uuid) -> Result<ExecutionRecord, StoreError> {
        self.conn
            .query_row(
                "SELECT id, vm_id, command, timeout_ms, status, exit_code, stdout, stderr,
                        duration_ms, error, created_at, updated_at
                 FROM executions WHERE id = ?1",
                params![id.to_string()],
                row_to_execution,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    pub fn list_executions(&self, vm_id: Option<Uuid>) -> Result<Vec<ExecutionRecord>, StoreError> {
        match vm_id {
            Some(vm_id) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, vm_id, command, timeout_ms, status, exit_code, stdout, stderr,
                            duration_ms, error, created_at, updated_at
                     FROM executions WHERE vm_id = ?1 ORDER BY created_at DESC",
                )?;
                let rows = stmt.query_map(params![vm_id.to_string()], row_to_execution)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(StoreError::from)
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, vm_id, command, timeout_ms, status, exit_code, stdout, stderr,
                            duration_ms, error, created_at, updated_at
                     FROM executions ORDER BY created_at DESC",
                )?;
                let rows = stmt.query_map([], row_to_execution)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(StoreError::from)
            }
        }
    }

    pub fn update_execution(&self, exec: &ExecutionRecord) -> Result<(), StoreError> {
        let n = self.conn.execute(
            "UPDATE executions SET
               status = ?2, exit_code = ?3, stdout = ?4, stderr = ?5,
               duration_ms = ?6, error = ?7, updated_at = ?8
             WHERE id = ?1",
            params![
                exec.id.to_string(),
                exec.status.as_str(),
                exec.exit_code,
                exec.stdout,
                exec.stderr,
                exec.duration_ms,
                exec.error,
                exec.updated_at.to_rfc3339(),
            ],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    pub fn upsert_host(&self, host: &HostRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO hosts (host_id, rpc_addr, sandbox_count, free_vcpus, free_memory_mib, healthy, last_heartbeat)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(host_id) DO UPDATE SET
               rpc_addr = excluded.rpc_addr,
               sandbox_count = excluded.sandbox_count,
               free_vcpus = excluded.free_vcpus,
               free_memory_mib = excluded.free_memory_mib,
               healthy = excluded.healthy,
               last_heartbeat = excluded.last_heartbeat",
            params![
                host.host_id,
                host.rpc_addr,
                host.sandbox_count as i64,
                host.free_vcpus as i64,
                host.free_memory_mib as i64,
                host.healthy as i64,
                host.last_heartbeat.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_hosts(&self) -> Result<Vec<HostRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT host_id, rpc_addr, sandbox_count, free_vcpus, free_memory_mib, healthy, last_heartbeat
             FROM hosts ORDER BY host_id",
        )?;
        let rows = stmt.query_map([], |row| {
            let hb: String = row.get(6)?;
            Ok(HostRecord {
                host_id: row.get(0)?,
                rpc_addr: row.get(1)?,
                sandbox_count: row.get::<_, i64>(2)? as usize,
                free_vcpus: row.get::<_, i64>(3)? as u64,
                free_memory_mib: row.get::<_, i64>(4)? as u64,
                healthy: row.get::<_, i64>(5)? != 0,
                last_heartbeat: parse_ts(&hb)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn insert_ssh_key(&self, key: &SshKeyRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO ssh_keys (
               id, owner_key, fingerprint, public_key, key_type, created_at, is_active
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                key.id.to_string(),
                key.owner_key,
                key.fingerprint,
                key.public_key,
                key.key_type,
                key.created_at.to_rfc3339(),
                key.is_active as i64,
            ],
        )?;
        Ok(())
    }

    pub fn list_ssh_keys(&self, owner_key: &str) -> Result<Vec<SshKeyRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, owner_key, fingerprint, public_key, key_type, created_at, is_active
             FROM ssh_keys WHERE owner_key = ?1 AND is_active = 1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![owner_key], row_to_ssh_key)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_active_ssh_key_by_fingerprint(
        &self,
        fingerprint: &str,
    ) -> Result<Option<SshKeyRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT id, owner_key, fingerprint, public_key, key_type, created_at, is_active
                 FROM ssh_keys
                 WHERE fingerprint = ?1 AND is_active = 1
                 ORDER BY created_at DESC
                 LIMIT 1",
                params![fingerprint],
                row_to_ssh_key,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn delete_ssh_key(&self, owner_key: &str, id: Uuid) -> Result<(), StoreError> {
        let n = self.conn.execute(
            "UPDATE ssh_keys SET is_active = 0 WHERE owner_key = ?1 AND id = ?2 AND is_active = 1",
            params![owner_key, id.to_string()],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    pub fn upsert_image(&self, image: &ImageRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO images (
               name, tag, rootfs_path, created_at, size_bytes, source_ref, golden_snapshot_path
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(name, tag) DO UPDATE SET
               rootfs_path = excluded.rootfs_path,
               created_at = excluded.created_at,
               size_bytes = excluded.size_bytes,
               source_ref = excluded.source_ref,
               golden_snapshot_path = excluded.golden_snapshot_path",
            params![
                image.name,
                image.tag,
                image.rootfs_path,
                image.created_at.to_rfc3339(),
                image.size_bytes as i64,
                image.source_ref,
                image.golden_snapshot_path,
            ],
        )?;
        Ok(())
    }

    pub fn get_image(&self, name: &str, tag: &str) -> Result<ImageRecord, StoreError> {
        self.conn
            .query_row(
                "SELECT name, tag, rootfs_path, created_at, size_bytes, source_ref, golden_snapshot_path
                 FROM images WHERE name = ?1 AND tag = ?2",
                params![name, tag],
                row_to_image,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    pub fn list_images(&self) -> Result<Vec<ImageRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT name, tag, rootfs_path, created_at, size_bytes, source_ref, golden_snapshot_path
             FROM images ORDER BY name, tag",
        )?;
        let rows = stmt.query_map([], row_to_image)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn delete_image(&self, name: &str, tag: &str) -> Result<ImageRecord, StoreError> {
        let image = self.get_image(name, tag)?;
        self.conn.execute(
            "DELETE FROM images WHERE name = ?1 AND tag = ?2",
            params![name, tag],
        )?;
        Ok(image)
    }

    pub fn enqueue_usage(&self, e: &UsageEvent) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO usage_outbox (
               id, api_key_id, owner_key, host_id, vm_id, kind, seconds, duration_ms,
               window_start, window_end, created_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                e.id.to_string(),
                e.api_key_id,
                e.owner_key,
                e.host_id,
                e.vm_id.to_string(),
                e.kind.as_str(),
                e.seconds,
                e.duration_ms,
                e.window_start.to_rfc3339(),
                e.window_end.to_rfc3339(),
                e.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_unsent_usage(&self, limit: usize) -> Result<Vec<UsageEvent>, StoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT id, api_key_id, owner_key, host_id, vm_id, kind, seconds, duration_ms,
                    window_start, window_end, created_at
             FROM usage_outbox WHERE sent = 0 ORDER BY created_at LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_usage)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn mark_usage_sent(&self, ids: &[Uuid]) -> Result<(), StoreError> {
        mark_outbox_sent(&self.conn, "usage_outbox", ids)
    }

    pub fn enqueue_audit(&self, e: &AuditEvent) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO audit_outbox (
               id, api_key_id, owner_key, host_id, vm_id, action, outcome, detail, created_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                e.id.to_string(),
                e.api_key_id,
                e.owner_key,
                e.host_id,
                e.vm_id.as_ref().map(|id| id.to_string()),
                e.action,
                e.outcome,
                e.detail,
                e.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_unsent_audit(&self, limit: usize) -> Result<Vec<AuditEvent>, StoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT id, api_key_id, owner_key, host_id, vm_id, action, outcome, detail, created_at
             FROM audit_outbox WHERE sent = 0 ORDER BY created_at LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_audit)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn mark_audit_sent(&self, ids: &[Uuid]) -> Result<(), StoreError> {
        mark_outbox_sent(&self.conn, "audit_outbox", ids)
    }

    pub fn set_billing_watermark(&self, vm_id: Uuid, ts: DateTime<Utc>) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO billing_watermark (vm_id, last_billed_at) VALUES (?1,?2)
             ON CONFLICT(vm_id) DO UPDATE SET last_billed_at = excluded.last_billed_at",
            params![vm_id.to_string(), ts.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn get_billing_watermark(&self, vm_id: Uuid) -> Result<Option<DateTime<Utc>>, StoreError> {
        self.conn
            .query_row(
                "SELECT last_billed_at FROM billing_watermark WHERE vm_id = ?1",
                params![vm_id.to_string()],
                |row| {
                    let ts: String = row.get(0)?;
                    parse_ts(&ts)
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn clear_billing_watermark(&self, vm_id: Uuid) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM billing_watermark WHERE vm_id = ?1",
            params![vm_id.to_string()],
        )?;
        Ok(())
    }

    pub fn prune_sent_outbox(&self, older_than: DateTime<Utc>) -> Result<(), StoreError> {
        let older_than = older_than.to_rfc3339();
        self.conn.execute(
            "DELETE FROM usage_outbox WHERE sent = 1 AND created_at < ?1",
            params![older_than],
        )?;
        self.conn.execute(
            "DELETE FROM audit_outbox WHERE sent = 1 AND created_at < ?1",
            params![older_than],
        )?;
        Ok(())
    }
}

fn row_to_vm(row: &rusqlite::Row<'_>) -> Result<VmRecord, rusqlite::Error> {
    let id: String = row.get(0)?;
    let status: String = row.get(4)?;
    let revision_i64: i64 = row.get(5)?;
    let revision = u64::try_from(revision_i64)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(5, revision_i64))?;
    let startup_path: Option<String> = row.get(6)?;
    let created_at: String = row.get(14)?;
    let updated_at: String = row.get(15)?;
    Ok(VmRecord {
        id: parse_uuid_col(&id, 0)?,
        host_id: row.get(1)?,
        owner_key: row.get(2)?,
        api_key_id: row.get(3)?,
        status: VmStatus::parse(&status).unwrap_or(VmStatus::Error),
        revision,
        startup_path: startup_path.as_deref().and_then(VmStartupPath::parse),
        memory_mib: row.get(7)?,
        vcpus: row.get(8)?,
        kernel_path: row.get(9)?,
        rootfs_path: row.get(10)?,
        cmdline: row.get(11)?,
        socket_path: row.get(12)?,
        pid: row.get(13)?,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
    })
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(());
        }
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    )?;
    Ok(())
}

fn mark_outbox_sent(conn: &Connection, table: &str, ids: &[Uuid]) -> Result<(), StoreError> {
    if ids.is_empty() {
        return Ok(());
    }

    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!("UPDATE {table} SET sent = 1 WHERE id IN ({placeholders})");
    let id_strings = ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
    conn.execute(&sql, params_from_iter(id_strings))?;
    Ok(())
}

fn row_to_execution(row: &rusqlite::Row<'_>) -> Result<ExecutionRecord, rusqlite::Error> {
    let id: String = row.get(0)?;
    let vm_id: String = row.get(1)?;
    let status: String = row.get(4)?;
    let created_at: String = row.get(10)?;
    let updated_at: String = row.get(11)?;
    Ok(ExecutionRecord {
        id: Uuid::parse_str(&id).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?,
        vm_id: Uuid::parse_str(&vm_id).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
        })?,
        command: row.get(2)?,
        timeout_ms: row.get(3)?,
        status: ExecutionStatus::parse(&status).unwrap_or(ExecutionStatus::Failed),
        exit_code: row.get(5)?,
        stdout: row.get(6)?,
        stderr: row.get(7)?,
        duration_ms: row.get(8)?,
        error: row.get(9)?,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
    })
}

fn row_to_ssh_key(row: &rusqlite::Row<'_>) -> Result<SshKeyRecord, rusqlite::Error> {
    let id: String = row.get(0)?;
    let created_at: String = row.get(5)?;
    Ok(SshKeyRecord {
        id: Uuid::parse_str(&id).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?,
        owner_key: row.get(1)?,
        fingerprint: row.get(2)?,
        public_key: row.get(3)?,
        key_type: row.get(4)?,
        created_at: parse_ts(&created_at)?,
        is_active: row.get::<_, i64>(6)? != 0,
    })
}

fn row_to_image(row: &rusqlite::Row<'_>) -> Result<ImageRecord, rusqlite::Error> {
    let created_at: String = row.get(3)?;
    let size_bytes: i64 = row.get(4)?;
    Ok(ImageRecord {
        name: row.get(0)?,
        tag: row.get(1)?,
        rootfs_path: row.get(2)?,
        created_at: parse_ts(&created_at)?,
        size_bytes: size_bytes.max(0) as u64,
        source_ref: row.get(5)?,
        golden_snapshot_path: row.get(6)?,
    })
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> Result<SnapshotRecord, rusqlite::Error> {
    let vm_id: String = row.get(5)?;
    let created_at: String = row.get(11)?;
    Ok(SnapshotRecord {
        path: row.get(0)?,
        overlay_path: row.get(1)?,
        host_id: row.get(2)?,
        owner_key: row.get(3)?,
        api_key_id: row.get(4)?,
        vm_id: parse_uuid_col(&vm_id, 5)?,
        memory_mib: row.get(6)?,
        vcpus: row.get(7)?,
        kernel_path: row.get(8)?,
        rootfs_path: row.get(9)?,
        cmdline: row.get(10)?,
        created_at: parse_ts(&created_at)?,
    })
}

fn row_to_share(row: &rusqlite::Row<'_>) -> Result<ShareRecord, rusqlite::Error> {
    let id: String = row.get(0)?;
    let vm_id: String = row.get(3)?;
    let guest_port: i64 = row.get(4)?;
    let visibility: String = row.get(5)?;
    let token_version: i64 = row.get(6)?;
    let revoked_at: Option<String> = row.get(7)?;
    let created_at: String = row.get(8)?;
    let updated_at: String = row.get(9)?;
    Ok(ShareRecord {
        id: parse_uuid_col(&id, 0)?,
        slug: row.get(1)?,
        owner_key: row.get(2)?,
        vm_id: parse_uuid_col(&vm_id, 3)?,
        guest_port: u16::try_from(guest_port)
            .map_err(|_| invalid_integer_error(4, "invalid guest port"))?,
        visibility: parse_share_visibility(&visibility, 5)?,
        token_version: u64::try_from(token_version)
            .map_err(|_| invalid_integer_error(6, "invalid token version"))?,
        revoked_at: revoked_at.as_deref().map(parse_ts).transpose()?,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
    })
}

fn row_to_usage(row: &rusqlite::Row<'_>) -> Result<UsageEvent, rusqlite::Error> {
    let id: String = row.get(0)?;
    let vm_id: String = row.get(4)?;
    let kind: String = row.get(5)?;
    let window_start: String = row.get(8)?;
    let window_end: String = row.get(9)?;
    let created_at: String = row.get(10)?;
    Ok(UsageEvent {
        id: parse_uuid_col(&id, 0)?,
        api_key_id: row.get(1)?,
        owner_key: row.get(2)?,
        host_id: row.get(3)?,
        vm_id: parse_uuid_col(&vm_id, 4)?,
        kind: UsageKind::parse(&kind)
            .ok_or_else(|| invalid_text_error(5, format!("invalid usage kind: {kind}")))?,
        seconds: row.get(6)?,
        duration_ms: row.get(7)?,
        window_start: parse_ts(&window_start)?,
        window_end: parse_ts(&window_end)?,
        created_at: parse_ts(&created_at)?,
    })
}

fn row_to_audit(row: &rusqlite::Row<'_>) -> Result<AuditEvent, rusqlite::Error> {
    let id: String = row.get(0)?;
    let vm_id: Option<String> = row.get(4)?;
    let created_at: String = row.get(8)?;
    Ok(AuditEvent {
        id: parse_uuid_col(&id, 0)?,
        api_key_id: row.get(1)?,
        owner_key: row.get(2)?,
        host_id: row.get(3)?,
        vm_id: parse_optional_uuid_col(vm_id, 4)?,
        action: row.get(5)?,
        outcome: row.get(6)?,
        detail: row.get(7)?,
        created_at: parse_ts(&created_at)?,
    })
}

fn parse_uuid_col(s: &str, column: usize) -> Result<Uuid, rusqlite::Error> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn parse_optional_uuid_col(
    value: Option<String>,
    column: usize,
) -> Result<Option<Uuid>, rusqlite::Error> {
    value
        .as_deref()
        .map(|s| parse_uuid_col(s, column))
        .transpose()
}

fn invalid_text_error(column: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    )
}

fn invalid_integer_error(column: usize, message: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Integer,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    )
}

fn share_visibility_as_str(visibility: ShareVisibility) -> &'static str {
    match visibility {
        ShareVisibility::Public => "public",
        ShareVisibility::Private => "private",
    }
}

fn parse_share_visibility(
    visibility: &str,
    column: usize,
) -> Result<ShareVisibility, rusqlite::Error> {
    match visibility {
        "public" => Ok(ShareVisibility::Public),
        "private" => Ok(ShareVisibility::Private),
        _ => Err(invalid_text_error(
            column,
            format!("invalid share visibility: {visibility}"),
        )),
    }
}

fn u64_to_sql_i64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|e| StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))
}

fn share_error_from_sqlite(error: rusqlite::Error) -> StoreError {
    if matches!(
        &error,
        rusqlite::Error::SqliteFailure(db_error, _)
            if db_error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || db_error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
    ) {
        StoreError::Conflict("share slug already exists".into())
    } else {
        StoreError::Sqlite(error)
    }
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>, rusqlite::Error> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tarit_types::{ShareRecord, ShareVisibility};

    fn test_share(slug: &str, owner_key: &str) -> ShareRecord {
        let now = Utc::now();
        ShareRecord {
            id: Uuid::new_v4(),
            slug: slug.into(),
            owner_key: owner_key.into(),
            vm_id: Uuid::new_v4(),
            guest_port: 8080,
            visibility: ShareVisibility::Private,
            token_version: 2,
            revoked_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn assert_share_eq(actual: &ShareRecord, expected: &ShareRecord) {
        assert_eq!(actual.id, expected.id);
        assert_eq!(actual.slug, expected.slug);
        assert_eq!(actual.owner_key, expected.owner_key);
        assert_eq!(actual.vm_id, expected.vm_id);
        assert_eq!(actual.guest_port, expected.guest_port);
        assert_eq!(actual.visibility, expected.visibility);
        assert_eq!(actual.token_version, expected.token_version);
        assert_eq!(actual.revoked_at, expected.revoked_at);
        assert_eq!(actual.created_at, expected.created_at);
        assert_eq!(actual.updated_at, expected.updated_at);
    }

    #[test]
    fn share_round_trips_and_slug_is_unique() {
        let store = Store::open(":memory:").unwrap();
        let share = test_share("calm-red-fox", "tenant-a");

        store.insert_share(&share).unwrap();
        assert_share_eq(&store.get_share(share.id).unwrap(), &share);
        assert_share_eq(
            &store.get_share_by_slug("calm-red-fox").unwrap().unwrap(),
            &share,
        );

        let duplicate = ShareRecord {
            id: Uuid::new_v4(),
            ..share
        };
        assert!(matches!(
            store.insert_share(&duplicate),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn share_slug_conflicts_do_not_change_other_sqlite_errors() {
        let store = Store::open(":memory:").unwrap();
        let share = test_share("conflicting-share", "tenant-a");
        store.insert_share(&share).unwrap();

        let duplicate_id = ShareRecord {
            slug: "different-share".into(),
            ..share.clone()
        };
        assert!(matches!(
            store.insert_share(&duplicate_id),
            Err(StoreError::Conflict(_))
        ));

        let duplicate_slug = ShareRecord {
            id: Uuid::new_v4(),
            ..share.clone()
        };
        assert!(matches!(
            store.insert_share(&duplicate_slug),
            Err(StoreError::Conflict(_))
        ));

        let invalid_port = ShareRecord {
            id: Uuid::new_v4(),
            slug: "invalid-port-share".into(),
            guest_port: 0,
            ..share.clone()
        };
        assert!(matches!(
            store.insert_share(&invalid_port),
            Err(StoreError::Sqlite(_))
        ));

        let key = SshKeyRecord {
            id: Uuid::new_v4(),
            owner_key: "tenant-a".into(),
            fingerprint: "SHA256:conflict-test".into(),
            public_key: "ssh-ed25519 AAAA conflict-test".into(),
            key_type: "ssh-ed25519".into(),
            created_at: Utc::now(),
            is_active: true,
        };
        store.insert_ssh_key(&key).unwrap();
        assert!(matches!(
            store.insert_ssh_key(&key),
            Err(StoreError::Sqlite(_))
        ));
    }

    #[test]
    fn share_listing_is_tenant_scoped_ordered_and_updatable() {
        let store = Store::open(":memory:").unwrap();
        let mut older = test_share("older-share", "tenant-a");
        older.created_at -= chrono::Duration::seconds(1);
        let mut newer = test_share("newer-share", "tenant-a");
        newer.revoked_at = Some(Utc::now());
        let other_tenant = test_share("other-tenant-share", "tenant-b");
        store.insert_share(&older).unwrap();
        store.insert_share(&newer).unwrap();
        store.insert_share(&other_tenant).unwrap();

        let shares = store.list_shares("tenant-a").unwrap();
        assert_eq!(shares.len(), 2);
        assert_eq!(shares[0].id, newer.id);
        assert_eq!(shares[1].id, older.id);
        assert_eq!(shares[0].revoked_at, newer.revoked_at);
        assert!(store
            .list_shares("tenant-b")
            .unwrap()
            .iter()
            .all(|s| s.id != older.id));

        newer.guest_port = 9090;
        newer.visibility = ShareVisibility::Public;
        newer.token_version += 1;
        newer.updated_at = Utc::now();
        newer.owner_key = "tenant-b".into();
        store.update_share(&newer).unwrap();
        let persisted = store.get_share(newer.id).unwrap();
        assert_eq!(persisted.owner_key, "tenant-a");
        newer.owner_key = "tenant-a".into();
        assert_share_eq(&persisted, &newer);

        assert!(matches!(
            store.get_share(Uuid::new_v4()),
            Err(StoreError::NotFound)
        ));
        assert!(store.get_share_by_slug("missing-share").unwrap().is_none());
        assert!(matches!(
            store.update_share(&test_share("missing-share", "tenant-a")),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn snapshot_ownership_round_trips_and_replaces() {
        let store = Store::open(":memory:").unwrap();
        let vm_id = Uuid::new_v4();
        let snap = SnapshotRecord {
            path: "/run/tarit/snap-1.snap".into(),
            overlay_path: Some("/run/tarit/snap-1.cow".into()),
            host_id: "node0".into(),
            owner_key: Some("tenant-a".into()),
            api_key_id: Some("key-1".into()),
            vm_id,
            memory_mib: Some(512),
            vcpus: Some(2),
            kernel_path: Some("/opt/tarit/vmlinux".into()),
            rootfs_path: Some("/opt/tarit/rootfs.ext4".into()),
            cmdline: Some("console=ttyS0".into()),
            created_at: Utc::now(),
        };
        store.insert_snapshot(&snap).unwrap();
        assert_eq!(store.get_snapshot(&snap.path).unwrap(), Some(snap.clone()));
        assert_eq!(
            store.get_snapshot("/run/tarit/does-not-exist").unwrap(),
            None
        );

        // Re-snapshotting the same path replaces the owner record.
        let replaced = SnapshotRecord {
            owner_key: Some("tenant-b".into()),
            ..snap.clone()
        };
        store.insert_snapshot(&replaced).unwrap();
        assert_eq!(
            store.get_snapshot(&snap.path).unwrap().unwrap().owner_key,
            Some("tenant-b".into())
        );
    }

    #[test]
    fn quota_reservation_distinguishes_limit_from_id_conflict_and_expires() {
        let store = Store::open(":memory:").unwrap();
        let owner = "tenant-a";
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let expiry = Utc::now() + chrono::Duration::minutes(1);

        assert_eq!(
            store.reserve_vm_quota(owner, first, 1, expiry).unwrap(),
            VmQuotaReservationOutcome::Reserved
        );
        assert_eq!(
            store.reserve_vm_quota(owner, first, 1, expiry).unwrap(),
            VmQuotaReservationOutcome::IdConflict
        );
        assert_eq!(
            store.reserve_vm_quota(owner, second, 1, expiry).unwrap(),
            VmQuotaReservationOutcome::QuotaExceeded
        );

        store.release_vm_quota(owner, first).unwrap();
        assert_eq!(
            store.reserve_vm_quota(owner, second, 1, expiry).unwrap(),
            VmQuotaReservationOutcome::Reserved
        );
        store.release_vm_quota(owner, second).unwrap();

        let expired = Utc::now() - chrono::Duration::seconds(1);
        assert_eq!(
            store
                .reserve_vm_quota(owner, Uuid::new_v4(), 1, expired)
                .unwrap(),
            VmQuotaReservationOutcome::Reserved
        );
        assert_eq!(
            store
                .reserve_vm_quota(owner, Uuid::new_v4(), 1, expiry)
                .unwrap(),
            VmQuotaReservationOutcome::Reserved
        );
    }

    #[test]
    fn image_registry_crud_round_trips_records() {
        let store = Store::open(":memory:").unwrap();
        let created_at = Utc::now();
        let image = ImageRecord {
            name: "node".into(),
            tag: "20".into(),
            rootfs_path: "target/tarit-store-test/node__20.ext4".into(),
            created_at,
            size_bytes: 42,
            source_ref: "node:20-slim".into(),
            golden_snapshot_path: Some("target/tarit-store-test/node__20.snap".into()),
        };

        store.upsert_image(&image).unwrap();
        assert_eq!(store.get_image("node", "20").unwrap(), image);
        assert_eq!(store.list_images().unwrap().len(), 1);

        let updated = ImageRecord {
            size_bytes: 84,
            golden_snapshot_path: None,
            ..image.clone()
        };
        store.upsert_image(&updated).unwrap();
        assert_eq!(store.get_image("node", "20").unwrap(), updated);

        let deleted = store.delete_image("node", "20").unwrap();
        assert_eq!(deleted, updated);
        assert!(matches!(
            store.get_image("node", "20"),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn ssh_key_crud_is_scoped_by_owner() {
        let store = Store::open(":memory:").unwrap();
        let key = SshKeyRecord {
            id: Uuid::new_v4(),
            owner_key: "owner-a".into(),
            fingerprint: "SHA256:test".into(),
            public_key: "ssh-ed25519 AAAA test".into(),
            key_type: "ssh-ed25519".into(),
            created_at: Utc::now(),
            is_active: true,
        };
        store.insert_ssh_key(&key).unwrap();

        assert_eq!(store.list_ssh_keys("owner-a").unwrap().len(), 1);
        assert!(store.list_ssh_keys("owner-b").unwrap().is_empty());
        assert!(matches!(
            store.delete_ssh_key("owner-b", key.id),
            Err(StoreError::NotFound)
        ));

        store.delete_ssh_key("owner-a", key.id).unwrap();
        assert!(store.list_ssh_keys("owner-a").unwrap().is_empty());
    }

    #[test]
    fn ssh_key_lookup_by_fingerprint_only_returns_active_keys() {
        let store = Store::open(":memory:").unwrap();
        let key = SshKeyRecord {
            id: Uuid::new_v4(),
            owner_key: "owner-a".into(),
            fingerprint: "SHA256:test".into(),
            public_key: "ssh-ed25519 AAAA test".into(),
            key_type: "ssh-ed25519".into(),
            created_at: Utc::now(),
            is_active: true,
        };
        store.insert_ssh_key(&key).unwrap();

        let found = store
            .get_active_ssh_key_by_fingerprint("SHA256:test")
            .unwrap()
            .unwrap();
        assert_eq!(found.owner_key, "owner-a");

        store.delete_ssh_key("owner-a", key.id).unwrap();
        assert!(store
            .get_active_ssh_key_by_fingerprint("SHA256:test")
            .unwrap()
            .is_none());
    }

    #[test]
    fn usage_and_audit_outboxes_round_trip_and_mark_sent() {
        let store = Store::open(":memory:").unwrap();
        let now = Utc::now();
        let vm_id = Uuid::new_v4();
        let usage = UsageEvent {
            id: Uuid::new_v4(),
            api_key_id: "api-key-a".into(),
            owner_key: "owner-a".into(),
            host_id: "host-a".into(),
            vm_id,
            kind: UsageKind::VmRuntime,
            seconds: Some(12.5),
            duration_ms: None,
            window_start: now,
            window_end: now,
            created_at: now,
        };
        let audit = AuditEvent {
            id: Uuid::new_v4(),
            api_key_id: "api-key-a".into(),
            owner_key: "owner-a".into(),
            host_id: "host-a".into(),
            vm_id: Some(vm_id),
            action: "create".into(),
            outcome: "ok".into(),
            detail: Some("{\"vm\":\"created\"}".into()),
            created_at: now,
        };

        store.enqueue_usage(&usage).unwrap();
        store.enqueue_usage(&usage).unwrap();
        store.enqueue_audit(&audit).unwrap();
        store.enqueue_audit(&audit).unwrap();

        let usage_rows = store.list_unsent_usage(10).unwrap();
        assert_eq!(usage_rows.len(), 1);
        assert_eq!(usage_rows[0].id, usage.id);
        assert_eq!(usage_rows[0].api_key_id, usage.api_key_id);
        assert_eq!(usage_rows[0].kind, UsageKind::VmRuntime);
        assert_eq!(usage_rows[0].seconds, Some(12.5));

        let audit_rows = store.list_unsent_audit(10).unwrap();
        assert_eq!(audit_rows.len(), 1);
        assert_eq!(audit_rows[0].id, audit.id);
        assert_eq!(audit_rows[0].vm_id, Some(vm_id));
        assert_eq!(audit_rows[0].action, "create");
        assert_eq!(audit_rows[0].detail, audit.detail);

        store.mark_usage_sent(&[usage.id]).unwrap();
        store.mark_audit_sent(&[audit.id]).unwrap();
        assert!(store.list_unsent_usage(10).unwrap().is_empty());
        assert!(store.list_unsent_audit(10).unwrap().is_empty());
    }

    #[test]
    fn billing_watermark_round_trips_and_clears() {
        let store = Store::open(":memory:").unwrap();
        let vm_id = Uuid::new_v4();
        let ts = Utc::now();

        assert_eq!(store.get_billing_watermark(vm_id).unwrap(), None);
        store.set_billing_watermark(vm_id, ts).unwrap();
        assert_eq!(store.get_billing_watermark(vm_id).unwrap(), Some(ts));
        store.clear_billing_watermark(vm_id).unwrap();
        assert_eq!(store.get_billing_watermark(vm_id).unwrap(), None);
    }

    #[test]
    fn vm_api_key_id_round_trips() {
        let store = Store::open(":memory:").unwrap();
        let now = Utc::now();
        let vm = VmRecord {
            id: Uuid::new_v4(),
            host_id: "host-a".into(),
            owner_key: Some("owner-a".into()),
            api_key_id: Some("api-key-a".into()),
            status: VmStatus::Running,
            revision: 1,
            startup_path: Some(VmStartupPath::Cold),
            memory_mib: 256,
            vcpus: 1,
            kernel_path: "vmlinux".into(),
            rootfs_path: Some("rootfs.ext4".into()),
            cmdline: "console=ttyS0".into(),
            socket_path: Some("vm.sock".into()),
            pid: Some(42),
            created_at: now,
            updated_at: now,
        };

        store.insert_vm(&vm).unwrap();
        assert_eq!(
            store.get_vm(vm.id).unwrap().api_key_id,
            Some("api-key-a".into())
        );

        let mut updated = vm.clone();
        updated.api_key_id = Some("api-key-b".into());
        updated.revision += 1;
        updated.updated_at += chrono::Duration::milliseconds(1);
        store.update_vm(&updated).unwrap();
        assert_eq!(
            store.list_vms().unwrap()[0].api_key_id,
            Some("api-key-b".into())
        );

        let mut conflicting_retry = updated.clone();
        conflicting_retry.cmdline = "different".into();
        assert!(matches!(
            store.update_vm(&conflicting_retry),
            Err(StoreError::Conflict(_))
        ));

        let mut stale = vm.clone();
        stale.status = VmStatus::Paused;
        store.update_vm(&stale).unwrap();
        assert_eq!(store.get_vm(vm.id).unwrap(), updated);
    }
}
