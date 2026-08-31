//! SQLite persistence for VM and execution records.

use chrono::{DateTime, Utc};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use std::path::Path;
use std::time::Duration;
use tarit_types::{
    ArtifactKind, ArtifactRecord, ArtifactReplicaRecord, ArtifactReplicaStatus,
    ArtifactReplicationState, ArtifactStatus, AuditEvent, BranchRecord, EgressPolicyRecord,
    ExecutionRecord, ExecutionStatus, ForkOperationRecord, ForkOperationStatus, ShareRecord,
    ShareVisibility, SshKeyRecord, UsageEvent, UsageKind, VmRecord, VmRuntimeLayout, VmStartupPath,
    VmStatus, VmVolumeAttachmentRecord, VolumeAttachmentMode, VolumeCapabilities, VolumeRecord,
    VolumeStatus, VolumeStorageClass,
};
use uuid::Uuid;

/// Cluster roster entry for one orchestrator host.
#[derive(Debug, Clone)]
pub struct HostRecord {
    pub host_id: String,
    /// Unique identity of the currently running taritd process on this host.
    /// Peer requests are fenced against it so a process from an earlier boot
    /// cannot keep acting after a replacement heartbeat is published.
    pub boot_session_id: Option<Uuid>,
    /// SHA-256 of the peer TLS leaf certificate for this process. Fleet peers
    /// compare it with the certificate presented on the live TLS connection,
    /// preventing one CA-trusted host from impersonating another host id.
    pub peer_certificate_sha256: Option<String>,
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
    /// Registry manifest digest resolved exactly once at admission. Legacy
    /// rows have no digest and must not be admitted as trusted images.
    pub source_digest: Option<String>,
    /// Digest of the exact ext4 bytes published on this host.
    pub rootfs_digest: Option<String>,
    /// Digest of the guest agent injected into the admitted rootfs.
    pub agent_digest: Option<String>,
    /// SHA-256 of the trusted cosign public key used for verification.
    pub provenance_key_digest: Option<String>,
    pub provenance_verified_at: Option<DateTime<Utc>>,
    pub golden_snapshot_path: Option<String>,
}

/// Ownership record for a node-local snapshot file, so restore can verify that
/// the caller owns the snapshot before its path is handed to the VMM (R-006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRecord {
    /// Opaque public identity; the backing path and host never cross the public API.
    pub snapshot_id: Uuid,
    pub path: String,
    /// Snapshot-owned copy of the VM's private CoW overlay. This must never
    /// point at the live VM overlay: deleting the source VM must not invalidate
    /// a snapshot, and separate restores must not share a writable upper.
    pub overlay_path: Option<String>,
    pub host_id: String,
    pub owner_key: Option<String>,
    pub api_key_id: Option<String>,
    pub vm_id: Uuid,
    /// VM whose private live-fork or hibernation transition retains this
    /// snapshot. Ordinary user-created snapshots leave this unset.
    pub ephemeral_owner_vm_id: Option<Uuid>,
    /// Resource shape and boot inputs captured with the snapshot ownership row.
    /// These are optional only for rows created before the metadata migration;
    /// production restore must fail closed when they are absent.
    pub memory_mib: Option<u64>,
    pub vcpus: Option<u8>,
    pub kernel_path: Option<String>,
    pub rootfs_path: Option<String>,
    pub rootfs_read_only: Option<bool>,
    pub cmdline: Option<String>,
    /// Digest over the ordered RAM and optional disk artifacts, computed from
    /// exact open inodes before publication. Absent only on legacy rows.
    pub content_digest: Option<String>,
    pub size_bytes: Option<u64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HibernationRecord {
    pub vm_id: Uuid,
    pub owner_key: String,
    pub snapshot_path: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkOperationClaimOutcome {
    New,
    Resumed,
    InProgress,
    Committed,
    QuotaExceeded,
}

pub struct VolumeTransition<'a> {
    pub expected_status: VolumeStatus,
    pub expected_revision: u64,
    pub status: VolumeStatus,
    pub last_error: Option<&'a str>,
    pub updated_at: DateTime<Utc>,
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
               rootfs_read_only INTEGER NOT NULL DEFAULT 0,
               cmdline TEXT NOT NULL,
               runtime_overlay_path TEXT,
               runtime_jail_path TEXT,
               runtime_artifact_paths TEXT,
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
             CREATE TABLE IF NOT EXISTS vm_fork_operations (
               child_vm_id TEXT PRIMARY KEY NOT NULL,
               source_vm_id TEXT NOT NULL,
               owner_key TEXT NOT NULL,
               source_host_id TEXT NOT NULL,
               target_host_id TEXT NOT NULL,
               status TEXT NOT NULL CHECK (status IN ('preparing','committed')),
               child_created_at TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS vm_fork_operations_source
               ON vm_fork_operations(owner_key, source_vm_id);
             CREATE TABLE IF NOT EXISTS hosts (
               host_id TEXT PRIMARY KEY NOT NULL,
               boot_session_id TEXT,
               peer_certificate_sha256 TEXT,
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
               source_digest TEXT,
               rootfs_digest TEXT,
               agent_digest TEXT,
               provenance_key_digest TEXT,
               provenance_verified_at TEXT,
               golden_snapshot_path TEXT,
               PRIMARY KEY (name, tag)
             );
             CREATE TABLE IF NOT EXISTS volumes (
               id TEXT PRIMARY KEY NOT NULL,
               owner_key TEXT NOT NULL,
               name TEXT NOT NULL,
               provider TEXT NOT NULL,
               storage_class TEXT NOT NULL,
               size_bytes INTEGER NOT NULL,
               status TEXT NOT NULL,
               read_only_many INTEGER NOT NULL,
               read_write_once INTEGER NOT NULL,
               read_write_many INTEGER NOT NULL,
               snapshots INTEGER NOT NULL,
               clones INTEGER NOT NULL,
               host_id TEXT,
               region TEXT,
               zone TEXT,
               generation INTEGER NOT NULL,
               revision INTEGER NOT NULL,
               last_error TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               UNIQUE(owner_key, name)
             );
             CREATE INDEX IF NOT EXISTS idx_volumes_owner_created
               ON volumes(owner_key, created_at DESC);
             CREATE TABLE IF NOT EXISTS vm_volume_attachments (
               vm_id TEXT NOT NULL,
               volume_id TEXT NOT NULL,
               device_index INTEGER NOT NULL,
               owner_key TEXT NOT NULL,
               mode TEXT NOT NULL,
               volume_generation INTEGER NOT NULL,
               created_at TEXT NOT NULL,
               PRIMARY KEY (vm_id, volume_id),
               UNIQUE (vm_id, device_index),
               FOREIGN KEY (vm_id) REFERENCES vms(id) ON DELETE CASCADE,
               FOREIGN KEY (volume_id) REFERENCES volumes(id) ON DELETE RESTRICT
             );
             DROP INDEX IF EXISTS idx_volume_single_writer;
             CREATE INDEX IF NOT EXISTS idx_volume_writer_lookup
               ON vm_volume_attachments(volume_id) WHERE mode = 'read_write';
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
               snapshot_id TEXT UNIQUE,
               overlay_path TEXT,
               host_id TEXT NOT NULL,
               owner_key TEXT,
               api_key_id TEXT,
               vm_id TEXT NOT NULL,
               ephemeral_owner_vm_id TEXT,
               memory_mib INTEGER,
               vcpus INTEGER,
               kernel_path TEXT,
               rootfs_path TEXT,
               rootfs_read_only INTEGER,
               cmdline TEXT,
               content_digest TEXT,
               size_bytes INTEGER,
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
             CREATE TABLE IF NOT EXISTS artifacts (
               artifact_id TEXT PRIMARY KEY NOT NULL,
               owner_key TEXT NOT NULL,
               host_id TEXT NOT NULL,
               storage_locator TEXT NOT NULL UNIQUE,
               kind TEXT NOT NULL CHECK (kind IN ('vm_snapshot','memory','disk','kernel','rootfs','agent')),
               status TEXT NOT NULL CHECK (status IN ('staging','available','deleting','corrupt')),
               content_digest TEXT NOT NULL,
               size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
               immutable_image_digest TEXT NOT NULL,
               agent_digest TEXT NOT NULL,
               boot_manifest_digest TEXT NOT NULL,
               parent_artifact_id TEXT REFERENCES artifacts(artifact_id) ON DELETE RESTRICT,
               source_vm_id TEXT,
               creation_revision INTEGER NOT NULL CHECK (creation_revision > 0),
               integrity_manifest_digest TEXT NOT NULL,
               chunk_size_bytes INTEGER NOT NULL CHECK (chunk_size_bytes > 0),
               chunk_count INTEGER NOT NULL CHECK (chunk_count > 0),
               replication_state TEXT NOT NULL CHECK (replication_state IN ('pending','ready','degraded')),
               reference_count INTEGER NOT NULL DEFAULT 0 CHECK (reference_count >= 0),
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS artifact_replicas (
               artifact_id TEXT NOT NULL,
               owner_key TEXT NOT NULL,
               host_id TEXT NOT NULL,
               failure_domain TEXT NOT NULL,
               storage_locator TEXT NOT NULL UNIQUE,
               status TEXT NOT NULL CHECK (status IN ('staging','available','corrupt','deleting')),
               content_digest TEXT NOT NULL,
               size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
               integrity_manifest_digest TEXT NOT NULL,
               verified_at TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               PRIMARY KEY (artifact_id, host_id),
               FOREIGN KEY (artifact_id) REFERENCES artifacts(artifact_id) ON DELETE CASCADE
             );
             CREATE TABLE IF NOT EXISTS branches (
               branch_id TEXT PRIMARY KEY NOT NULL,
               owner_key TEXT NOT NULL,
               name TEXT NOT NULL,
               head_artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id) ON DELETE RESTRICT,
               source_vm_id TEXT,
               source_branch_id TEXT REFERENCES branches(branch_id) ON DELETE SET NULL,
               revision INTEGER NOT NULL CHECK (revision > 0),
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               UNIQUE(owner_key, name)
             );
             CREATE TABLE IF NOT EXISTS hibernations (
               vm_id TEXT PRIMARY KEY NOT NULL,
               owner_key TEXT NOT NULL,
               snapshot_path TEXT NOT NULL REFERENCES snapshots(path) ON DELETE RESTRICT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS vm_egress_policies (
               vm_id TEXT PRIMARY KEY NOT NULL,
               owner_key TEXT NOT NULL,
               revision INTEGER NOT NULL CHECK (revision > 0),
               allowlist_json TEXT NOT NULL,
               allow_existing INTEGER NOT NULL DEFAULT 0,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS usage_outbox_unsent ON usage_outbox(sent);
             CREATE INDEX IF NOT EXISTS audit_outbox_unsent ON audit_outbox(sent);
             CREATE INDEX IF NOT EXISTS shares_owner ON shares(owner_key, created_at DESC);
             CREATE INDEX IF NOT EXISTS shares_vm ON shares(vm_id);
             CREATE INDEX IF NOT EXISTS artifacts_owner_created
               ON artifacts(owner_key, created_at DESC);
             CREATE INDEX IF NOT EXISTS artifacts_parent ON artifacts(parent_artifact_id);
             CREATE INDEX IF NOT EXISTS artifact_replicas_artifact_status
               ON artifact_replicas(artifact_id, status);
             CREATE INDEX IF NOT EXISTS artifact_replicas_failure_domain
               ON artifact_replicas(failure_domain, status);
             CREATE INDEX IF NOT EXISTS branches_owner_created
               ON branches(owner_key, created_at DESC);
             CREATE INDEX IF NOT EXISTS branches_head ON branches(head_artifact_id);
             CREATE INDEX IF NOT EXISTS hibernations_owner
               ON hibernations(owner_key, updated_at DESC);
             CREATE INDEX IF NOT EXISTS vm_egress_policies_owner
               ON vm_egress_policies(owner_key, updated_at DESC);
             CREATE INDEX IF NOT EXISTS vm_quota_reservations_owner_expiry
               ON vm_quota_reservations(owner_key, expires_at);",
        )?;
        ensure_column(&conn, "vms", "owner_key", "TEXT")?;
        ensure_column(&conn, "vms", "api_key_id", "TEXT")?;
        ensure_column(&conn, "vms", "revision", "INTEGER NOT NULL DEFAULT 1")?;
        ensure_column(&conn, "vms", "startup_path", "TEXT")?;
        ensure_column(
            &conn,
            "vms",
            "rootfs_read_only",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(&conn, "vms", "runtime_overlay_path", "TEXT")?;
        ensure_column(&conn, "vms", "runtime_jail_path", "TEXT")?;
        ensure_column(&conn, "vms", "runtime_artifact_paths", "TEXT")?;
        ensure_column(&conn, "snapshots", "memory_mib", "INTEGER")?;
        ensure_column(&conn, "snapshots", "overlay_path", "TEXT")?;
        ensure_column(&conn, "snapshots", "vcpus", "INTEGER")?;
        ensure_column(&conn, "snapshots", "kernel_path", "TEXT")?;
        ensure_column(&conn, "snapshots", "rootfs_path", "TEXT")?;
        ensure_column(&conn, "snapshots", "rootfs_read_only", "INTEGER")?;
        ensure_column(&conn, "snapshots", "cmdline", "TEXT")?;
        ensure_column(&conn, "snapshots", "content_digest", "TEXT")?;
        ensure_column(&conn, "snapshots", "snapshot_id", "TEXT")?;
        ensure_column(&conn, "hosts", "boot_session_id", "TEXT")?;
        ensure_column(&conn, "hosts", "peer_certificate_sha256", "TEXT")?;
        ensure_column(&conn, "snapshots", "size_bytes", "INTEGER")?;
        ensure_column(&conn, "snapshots", "ephemeral_owner_vm_id", "TEXT")?;
        ensure_column(&conn, "images", "source_digest", "TEXT")?;
        ensure_column(&conn, "images", "rootfs_digest", "TEXT")?;
        ensure_column(&conn, "images", "agent_digest", "TEXT")?;
        ensure_column(&conn, "images", "provenance_key_digest", "TEXT")?;
        ensure_column(&conn, "images", "provenance_verified_at", "TEXT")?;
        // Existing records predate authenticated boot metadata. The empty
        // sentinel is intentionally unusable by restore and forces a fresh
        // snapshot rather than inventing trust during migration.
        ensure_column(
            &conn,
            "artifacts",
            "boot_manifest_digest",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        {
            let mut statement =
                conn.prepare("SELECT path FROM snapshots WHERE snapshot_id IS NULL")?;
            let legacy_paths = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            for path in legacy_paths {
                conn.execute(
                    "UPDATE snapshots SET snapshot_id = ?1 WHERE path = ?2 AND snapshot_id IS NULL",
                    params![Uuid::new_v4().to_string(), path],
                )?;
            }
        }
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS snapshots_public_id ON snapshots(snapshot_id)",
            [],
        )?;
        conn.execute(
            "UPDATE snapshots SET rootfs_read_only = 1 WHERE rootfs_read_only IS NULL",
            [],
        )?;
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
              vcpus, kernel_path, rootfs_path, rootfs_read_only, cmdline, runtime_overlay_path,
              runtime_jail_path, runtime_artifact_paths, socket_path, pid, created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)
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
               rootfs_read_only = excluded.rootfs_read_only,
               cmdline = excluded.cmdline,
               runtime_overlay_path = excluded.runtime_overlay_path,
               runtime_jail_path = excluded.runtime_jail_path,
               runtime_artifact_paths = excluded.runtime_artifact_paths,
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
                vm.rootfs_read_only,
                vm.cmdline,
                vm.runtime_layout
                    .as_ref()
                    .and_then(|layout| layout.overlay_path.as_deref()),
                vm.runtime_layout
                    .as_ref()
                    .and_then(|layout| layout.jail_path.as_deref()),
                vm.runtime_layout
                    .as_ref()
                    .map(|layout| serde_json::to_string(&layout.artifact_paths))
                    .transpose()
                    .map_err(|error| StoreError::Conflict(format!(
                        "encode VM runtime artifact paths: {error}"
                    )))?,
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
                        memory_mib, vcpus, kernel_path, rootfs_path, rootfs_read_only, cmdline,
                        runtime_overlay_path, runtime_jail_path, runtime_artifact_paths,
                        socket_path, pid, created_at, updated_at
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
               path, overlay_path, host_id, owner_key, api_key_id, vm_id, ephemeral_owner_vm_id, memory_mib, vcpus,
               kernel_path, rootfs_path, rootfs_read_only, cmdline, content_digest, size_bytes,
               created_at, snapshot_id
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                snap.path,
                snap.overlay_path,
                snap.host_id,
                snap.owner_key,
                snap.api_key_id,
                snap.vm_id.to_string(),
                snap.ephemeral_owner_vm_id.map(|id| id.to_string()),
                snap.memory_mib,
                snap.vcpus,
                snap.kernel_path,
                snap.rootfs_path,
                snap.rootfs_read_only,
                snap.cmdline,
                snap.content_digest,
                snap.size_bytes.map(u64_to_sql_i64).transpose()?,
                snap.created_at.to_rfc3339(),
                snap.snapshot_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Look up the ownership record for a snapshot path, if one exists.
    pub fn get_snapshot(&self, path: &str) -> Result<Option<SnapshotRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT path, overlay_path, host_id, owner_key, api_key_id, vm_id, ephemeral_owner_vm_id, memory_mib, vcpus,
                        kernel_path, rootfs_path, rootfs_read_only, cmdline, content_digest,
                        size_bytes, created_at, snapshot_id
                 FROM snapshots WHERE path = ?1",
                params![path],
                row_to_snapshot,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn get_snapshot_by_id(
        &self,
        snapshot_id: Uuid,
    ) -> Result<Option<SnapshotRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT path, overlay_path, host_id, owner_key, api_key_id, vm_id, ephemeral_owner_vm_id, memory_mib, vcpus,
                        kernel_path, rootfs_path, rootfs_read_only, cmdline, content_digest,
                        size_bytes, created_at, snapshot_id
                 FROM snapshots WHERE snapshot_id = ?1",
                params![snapshot_id.to_string()],
                row_to_snapshot,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn list_snapshots(&self) -> Result<Vec<SnapshotRecord>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT path, overlay_path, host_id, owner_key, api_key_id, vm_id, ephemeral_owner_vm_id, memory_mib, vcpus,
                    kernel_path, rootfs_path, rootfs_read_only, cmdline, content_digest,
                    size_bytes, created_at, snapshot_id
             FROM snapshots",
        )?;
        let snapshots = statement
            .query_map([], row_to_snapshot)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(snapshots)
    }

    pub fn list_ephemeral_snapshots_for_vm(
        &self,
        vm_id: Uuid,
    ) -> Result<Vec<SnapshotRecord>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT path, overlay_path, host_id, owner_key, api_key_id, vm_id, ephemeral_owner_vm_id, memory_mib, vcpus,
                    kernel_path, rootfs_path, rootfs_read_only, cmdline, content_digest,
                    size_bytes, created_at, snapshot_id
             FROM snapshots WHERE ephemeral_owner_vm_id = ?1",
        )?;
        let snapshots = statement
            .query_map(params![vm_id.to_string()], row_to_snapshot)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(snapshots)
    }

    pub fn bind_snapshot_ephemeral_owner(&self, path: &str, vm_id: Uuid) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE snapshots SET ephemeral_owner_vm_id = ?2
             WHERE path = ?1 AND ephemeral_owner_vm_id IS NULL",
            params![path, vm_id.to_string()],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "snapshot is missing or already has a lifecycle owner".into(),
            ));
        }
        Ok(())
    }

    pub fn delete_snapshot(&self, path: &str) -> Result<(), StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM snapshots WHERE path = ?1", params![path])?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    pub fn upsert_hibernation(&self, record: &HibernationRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO hibernations (vm_id, owner_key, snapshot_path, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(vm_id) DO UPDATE SET
               owner_key = excluded.owner_key,
               snapshot_path = excluded.snapshot_path,
               updated_at = excluded.updated_at",
            params![
                record.vm_id.to_string(),
                record.owner_key,
                record.snapshot_path,
                record.created_at.to_rfc3339(),
                record.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_hibernation(
        &self,
        owner_key: &str,
        vm_id: Uuid,
    ) -> Result<HibernationRecord, StoreError> {
        self.conn
            .query_row(
                "SELECT vm_id, owner_key, snapshot_path, created_at, updated_at
                 FROM hibernations WHERE vm_id = ?1 AND owner_key = ?2",
                params![vm_id.to_string(), owner_key],
                row_to_hibernation,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    pub fn list_hibernations(&self) -> Result<Vec<HibernationRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT vm_id, owner_key, snapshot_path, created_at, updated_at
             FROM hibernations ORDER BY created_at, vm_id",
        )?;
        let records = stmt
            .query_map([], row_to_hibernation)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(records)
    }

    pub fn delete_hibernation(
        &self,
        owner_key: &str,
        vm_id: Uuid,
    ) -> Result<HibernationRecord, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let record = tx
            .query_row(
                "SELECT vm_id, owner_key, snapshot_path, created_at, updated_at
                 FROM hibernations WHERE vm_id = ?1 AND owner_key = ?2",
                params![vm_id.to_string(), owner_key],
                row_to_hibernation,
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        let changed = tx.execute(
            "DELETE FROM hibernations WHERE vm_id = ?1 AND owner_key = ?2",
            params![vm_id.to_string(), owner_key],
        )?;
        if changed != 1 {
            return Err(StoreError::NotFound);
        }
        tx.commit()?;
        Ok(record)
    }

    /// Insert an immutable artifact. A parent artifact, when present, must be
    /// available and owned by the same tenant; its reference is acquired in
    /// the same transaction as the child row.
    pub fn insert_artifact(&self, artifact: &ArtifactRecord) -> Result<(), StoreError> {
        if artifact.reference_count != 0 {
            return Err(StoreError::Conflict(
                "new artifact reference_count must be zero".into(),
            ));
        }
        let tx = self.conn.unchecked_transaction()?;
        if let Some(existing) = tx
            .query_row(
                "SELECT artifact_id, owner_key, host_id, storage_locator, kind, status,
                        content_digest, size_bytes, immutable_image_digest, agent_digest,
                        boot_manifest_digest, parent_artifact_id, source_vm_id, creation_revision,
                        integrity_manifest_digest, chunk_size_bytes, chunk_count,
                        replication_state, reference_count, created_at, updated_at
                 FROM artifacts WHERE artifact_id = ?1 AND owner_key = ?2",
                params![artifact.artifact_id.to_string(), artifact.owner_key],
                row_to_artifact,
            )
            .optional()?
        {
            if same_immutable_artifact(&existing, artifact) {
                return Ok(());
            }
            return Err(StoreError::Conflict(
                "artifact id already exists with different content".into(),
            ));
        }
        if let Some(parent_id) = artifact.parent_artifact_id {
            let changed = tx.execute(
                "UPDATE artifacts SET reference_count = reference_count + 1, updated_at = ?3
                 WHERE artifact_id = ?1 AND owner_key = ?2 AND status = 'available'
                   AND replication_state = 'ready'",
                params![
                    parent_id.to_string(),
                    artifact.owner_key,
                    artifact.updated_at.to_rfc3339()
                ],
            )?;
            if changed == 0 {
                return Err(StoreError::NotFound);
            }
        }
        tx.execute(
            "INSERT INTO artifacts (
               artifact_id, owner_key, host_id, storage_locator, kind, status, content_digest,
               size_bytes, immutable_image_digest, agent_digest, boot_manifest_digest,
               parent_artifact_id, source_vm_id, creation_revision, integrity_manifest_digest,
               chunk_size_bytes, chunk_count,
               replication_state, reference_count, created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,0,?19,?20)",
            params![
                artifact.artifact_id.to_string(),
                artifact.owner_key,
                artifact.host_id,
                artifact.storage_locator,
                artifact.kind.as_str(),
                artifact.status.as_str(),
                artifact.content_digest,
                u64_to_sql_i64(artifact.size_bytes)?,
                artifact.immutable_image_digest,
                artifact.agent_digest,
                artifact.boot_manifest_digest,
                artifact.parent_artifact_id.map(|id| id.to_string()),
                artifact.source_vm_id.map(|id| id.to_string()),
                u64_to_sql_i64(artifact.creation_revision)?,
                artifact.integrity_manifest_digest,
                u64_to_sql_i64(artifact.chunk_size_bytes)?,
                u64_to_sql_i64(artifact.chunk_count)?,
                artifact.replication_state.as_str(),
                artifact.created_at.to_rfc3339(),
                artifact.updated_at.to_rfc3339(),
            ],
        )
        .map_err(|error| constraint_conflict(error, "artifact already exists"))?;
        let replica_status = match artifact.status {
            ArtifactStatus::Available => ArtifactReplicaStatus::Available,
            ArtifactStatus::Corrupt => ArtifactReplicaStatus::Corrupt,
            ArtifactStatus::Deleting => ArtifactReplicaStatus::Deleting,
            ArtifactStatus::Staging => ArtifactReplicaStatus::Staging,
        };
        tx.execute(
            "INSERT INTO artifact_replicas (
               artifact_id, owner_key, host_id, failure_domain, storage_locator, status,
               content_digest, size_bytes, integrity_manifest_digest, verified_at,
               created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                artifact.artifact_id.to_string(),
                artifact.owner_key,
                artifact.host_id,
                artifact.host_id,
                artifact.storage_locator,
                replica_status.as_str(),
                artifact.content_digest,
                u64_to_sql_i64(artifact.size_bytes)?,
                artifact.integrity_manifest_digest,
                if replica_status == ArtifactReplicaStatus::Available {
                    Some(artifact.updated_at.to_rfc3339())
                } else {
                    None
                },
                artifact.created_at.to_rfc3339(),
                artifact.updated_at.to_rfc3339(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Add or update one physical replica, rejecting any metadata that does
    /// not exactly match the immutable logical artifact. Replication readiness
    /// is derived from verified available copies and distinct failure domains.
    pub fn upsert_artifact_replica(
        &self,
        replica: &ArtifactReplicaRecord,
        min_replicas: u64,
        min_failure_domains: u64,
    ) -> Result<ArtifactReplicationState, StoreError> {
        if min_replicas == 0 || min_failure_domains == 0 {
            return Err(StoreError::Conflict(
                "replication minima must be positive".into(),
            ));
        }
        let tx = self.conn.unchecked_transaction()?;
        let artifact = tx
            .query_row(
                "SELECT artifact_id, owner_key, host_id, storage_locator, kind, status,
                        content_digest, size_bytes, immutable_image_digest, agent_digest,
                        boot_manifest_digest, parent_artifact_id, source_vm_id, creation_revision,
                        integrity_manifest_digest, chunk_size_bytes, chunk_count,
                        replication_state, reference_count, created_at, updated_at
                 FROM artifacts WHERE artifact_id = ?1 AND owner_key = ?2",
                params![replica.artifact_id.to_string(), replica.owner_key],
                row_to_artifact,
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        if replica.content_digest != artifact.content_digest
            || replica.size_bytes != artifact.size_bytes
            || replica.integrity_manifest_digest != artifact.integrity_manifest_digest
        {
            return Err(StoreError::Conflict(
                "replica metadata does not match immutable artifact".into(),
            ));
        }
        if replica.status == ArtifactReplicaStatus::Available && replica.verified_at.is_none() {
            return Err(StoreError::Conflict(
                "available replica requires verified_at".into(),
            ));
        }
        tx.execute(
            "INSERT INTO artifact_replicas (
               artifact_id, owner_key, host_id, failure_domain, storage_locator, status,
               content_digest, size_bytes, integrity_manifest_digest, verified_at,
               created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
             ON CONFLICT(artifact_id, host_id) DO UPDATE SET
               failure_domain=excluded.failure_domain,
               storage_locator=excluded.storage_locator,
               status=excluded.status,
               content_digest=excluded.content_digest,
               size_bytes=excluded.size_bytes,
               integrity_manifest_digest=excluded.integrity_manifest_digest,
               verified_at=excluded.verified_at,
               updated_at=excluded.updated_at
             WHERE artifact_replicas.owner_key=excluded.owner_key",
            params![
                replica.artifact_id.to_string(),
                replica.owner_key,
                replica.host_id,
                replica.failure_domain,
                replica.storage_locator,
                replica.status.as_str(),
                replica.content_digest,
                u64_to_sql_i64(replica.size_bytes)?,
                replica.integrity_manifest_digest,
                replica.verified_at.map(|value| value.to_rfc3339()),
                replica.created_at.to_rfc3339(),
                replica.updated_at.to_rfc3339(),
            ],
        )
        .map_err(|error| constraint_conflict(error, "replica locator already exists"))?;
        let state = recompute_artifact_replication_sqlite(
            &tx,
            &replica.owner_key,
            replica.artifact_id,
            min_replicas,
            min_failure_domains,
            replica.updated_at,
        )?;
        tx.commit()?;
        Ok(state)
    }

    pub fn list_artifact_replicas(
        &self,
        owner_key: &str,
        artifact_id: Uuid,
    ) -> Result<Vec<ArtifactReplicaRecord>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT artifact_id, owner_key, host_id, failure_domain, storage_locator, status,
                    content_digest, size_bytes, integrity_manifest_digest, verified_at,
                    created_at, updated_at
             FROM artifact_replicas WHERE artifact_id = ?1 AND owner_key = ?2
             ORDER BY host_id",
        )?;
        let replicas = statement
            .query_map(
                params![artifact_id.to_string(), owner_key],
                row_to_artifact_replica,
            )?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(replicas)
    }

    pub fn get_artifact(
        &self,
        owner_key: &str,
        artifact_id: Uuid,
    ) -> Result<ArtifactRecord, StoreError> {
        self.conn
            .query_row(
                "SELECT artifact_id, owner_key, host_id, storage_locator, kind, status,
                        content_digest, size_bytes, immutable_image_digest, agent_digest,
                        boot_manifest_digest, parent_artifact_id, source_vm_id, creation_revision,
                        integrity_manifest_digest, chunk_size_bytes, chunk_count,
                        replication_state, reference_count, created_at, updated_at
                 FROM artifacts WHERE artifact_id = ?1 AND owner_key = ?2",
                params![artifact_id.to_string(), owner_key],
                row_to_artifact,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    pub fn list_artifacts(&self, owner_key: &str) -> Result<Vec<ArtifactRecord>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT artifact_id, owner_key, host_id, storage_locator, kind, status,
                    content_digest, size_bytes, immutable_image_digest, agent_digest,
                    boot_manifest_digest, parent_artifact_id, source_vm_id, creation_revision,
                    integrity_manifest_digest, chunk_size_bytes, chunk_count,
                    replication_state, reference_count, created_at, updated_at
             FROM artifacts WHERE owner_key = ?1 ORDER BY created_at DESC",
        )?;
        let artifacts = statement
            .query_map(params![owner_key], row_to_artifact)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(artifacts)
    }

    /// Create a tenant branch and acquire its head artifact reference atomically.
    pub fn insert_branch(&self, branch: &BranchRecord) -> Result<(), StoreError> {
        if branch.revision == 0 {
            return Err(StoreError::Conflict(
                "branch revision must be positive".into(),
            ));
        }
        let tx = self.conn.unchecked_transaction()?;
        if let Some(existing) = tx
            .query_row(
                "SELECT branch_id, owner_key, name, head_artifact_id, source_vm_id,
                        source_branch_id, revision, created_at, updated_at
                 FROM branches WHERE branch_id = ?1 AND owner_key = ?2",
                params![branch.branch_id.to_string(), branch.owner_key],
                row_to_branch,
            )
            .optional()?
        {
            if existing.name == branch.name
                && existing.head_artifact_id == branch.head_artifact_id
                && existing.source_vm_id == branch.source_vm_id
                && existing.source_branch_id == branch.source_branch_id
            {
                return Ok(());
            }
            return Err(StoreError::Conflict(
                "branch id already exists with different content".into(),
            ));
        }
        if let Some(source_branch_id) = branch.source_branch_id {
            let source_exists = tx.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM branches WHERE branch_id = ?1 AND owner_key = ?2
                 )",
                params![source_branch_id.to_string(), branch.owner_key],
                |row| row.get::<_, bool>(0),
            )?;
            if !source_exists {
                return Err(StoreError::NotFound);
            }
        }
        if let Some(source_vm_id) = branch.source_vm_id {
            let source_exists = tx.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM vms WHERE id = ?1 AND owner_key = ?2
                 )",
                params![source_vm_id.to_string(), branch.owner_key],
                |row| row.get::<_, bool>(0),
            )?;
            if !source_exists {
                return Err(StoreError::NotFound);
            }
        }
        let changed = tx.execute(
            "UPDATE artifacts SET reference_count = reference_count + 1, updated_at = ?3
             WHERE artifact_id = ?1 AND owner_key = ?2 AND status = 'available'
               AND replication_state = 'ready'",
            params![
                branch.head_artifact_id.to_string(),
                branch.owner_key,
                branch.updated_at.to_rfc3339()
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        tx.execute(
            "INSERT INTO branches (
               branch_id, owner_key, name, head_artifact_id, source_vm_id, source_branch_id,
               revision, created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                branch.branch_id.to_string(),
                branch.owner_key,
                branch.name,
                branch.head_artifact_id.to_string(),
                branch.source_vm_id.map(|id| id.to_string()),
                branch.source_branch_id.map(|id| id.to_string()),
                u64_to_sql_i64(branch.revision)?,
                branch.created_at.to_rfc3339(),
                branch.updated_at.to_rfc3339(),
            ],
        )
        .map_err(|error| constraint_conflict(error, "branch id or name already exists"))?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_branch(&self, owner_key: &str, branch_id: Uuid) -> Result<BranchRecord, StoreError> {
        self.conn
            .query_row(
                "SELECT branch_id, owner_key, name, head_artifact_id, source_vm_id,
                        source_branch_id, revision, created_at, updated_at
                 FROM branches WHERE branch_id = ?1 AND owner_key = ?2",
                params![branch_id.to_string(), owner_key],
                row_to_branch,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    pub fn list_branches(&self, owner_key: &str) -> Result<Vec<BranchRecord>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT branch_id, owner_key, name, head_artifact_id, source_vm_id,
                    source_branch_id, revision, created_at, updated_at
             FROM branches WHERE owner_key = ?1 ORDER BY created_at DESC",
        )?;
        let branches = statement
            .query_map(params![owner_key], row_to_branch)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(branches)
    }

    /// Compare-and-swap a branch head and transfer the durable artifact
    /// reference in the same transaction.
    pub fn update_branch_head(
        &self,
        owner_key: &str,
        branch_id: Uuid,
        expected_revision: u64,
        new_head_artifact_id: Uuid,
        updated_at: DateTime<Utc>,
    ) -> Result<BranchRecord, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let current = tx
            .query_row(
                "SELECT branch_id, owner_key, name, head_artifact_id, source_vm_id,
                        source_branch_id, revision, created_at, updated_at
                 FROM branches WHERE branch_id = ?1 AND owner_key = ?2",
                params![branch_id.to_string(), owner_key],
                row_to_branch,
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        if current.revision != expected_revision {
            return Err(StoreError::Conflict(format!(
                "branch revision is {}, expected {expected_revision}",
                current.revision
            )));
        }

        if current.head_artifact_id != new_head_artifact_id {
            let acquired = tx.execute(
                "UPDATE artifacts SET reference_count = reference_count + 1, updated_at = ?3
                 WHERE artifact_id = ?1 AND owner_key = ?2 AND status = 'available'
                   AND replication_state = 'ready'",
                params![
                    new_head_artifact_id.to_string(),
                    owner_key,
                    updated_at.to_rfc3339()
                ],
            )?;
            if acquired == 0 {
                return Err(StoreError::NotFound);
            }
        }
        let next_revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| StoreError::Conflict("branch revision overflow".into()))?;
        let changed = tx.execute(
            "UPDATE branches SET head_artifact_id = ?4, revision = ?5, updated_at = ?6
             WHERE branch_id = ?1 AND owner_key = ?2 AND revision = ?3",
            params![
                branch_id.to_string(),
                owner_key,
                u64_to_sql_i64(expected_revision)?,
                new_head_artifact_id.to_string(),
                u64_to_sql_i64(next_revision)?,
                updated_at.to_rfc3339(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict("branch changed concurrently".into()));
        }
        if current.head_artifact_id != new_head_artifact_id {
            let released = tx.execute(
                "UPDATE artifacts SET reference_count = reference_count - 1, updated_at = ?3
                 WHERE artifact_id = ?1 AND owner_key = ?2 AND reference_count > 0",
                params![
                    current.head_artifact_id.to_string(),
                    owner_key,
                    updated_at.to_rfc3339()
                ],
            )?;
            if released != 1 {
                return Err(StoreError::Conflict(
                    "old branch head has no reference to release".into(),
                ));
            }
        }
        let updated = tx.query_row(
            "SELECT branch_id, owner_key, name, head_artifact_id, source_vm_id,
                    source_branch_id, revision, created_at, updated_at
             FROM branches WHERE branch_id = ?1",
            params![branch_id.to_string()],
            row_to_branch,
        )?;
        tx.commit()?;
        Ok(updated)
    }

    pub fn delete_branch(
        &self,
        owner_key: &str,
        branch_id: Uuid,
    ) -> Result<BranchRecord, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let branch = tx
            .query_row(
                "SELECT branch_id, owner_key, name, head_artifact_id, source_vm_id,
                        source_branch_id, revision, created_at, updated_at
                 FROM branches WHERE branch_id = ?1 AND owner_key = ?2",
                params![branch_id.to_string(), owner_key],
                row_to_branch,
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        tx.execute(
            "DELETE FROM branches WHERE branch_id = ?1 AND owner_key = ?2",
            params![branch_id.to_string(), owner_key],
        )?;
        let released = tx.execute(
            "UPDATE artifacts SET reference_count = reference_count - 1, updated_at = ?3
             WHERE artifact_id = ?1 AND owner_key = ?2 AND reference_count > 0",
            params![
                branch.head_artifact_id.to_string(),
                owner_key,
                Utc::now().to_rfc3339()
            ],
        )?;
        if released != 1 {
            return Err(StoreError::Conflict(
                "branch head has no reference to release".into(),
            ));
        }
        tx.commit()?;
        Ok(branch)
    }

    /// Delete an unreferenced artifact and release its optional parent
    /// reference. Artifacts with branches, children, or future VM references
    /// remain protected by the counter and foreign-key constraints.
    pub fn delete_artifact_if_unreferenced(
        &self,
        owner_key: &str,
        artifact_id: Uuid,
    ) -> Result<ArtifactRecord, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let artifact = tx
            .query_row(
                "SELECT artifact_id, owner_key, host_id, storage_locator, kind, status,
                        content_digest, size_bytes, immutable_image_digest, agent_digest,
                        boot_manifest_digest, parent_artifact_id, source_vm_id, creation_revision,
                        integrity_manifest_digest, chunk_size_bytes, chunk_count,
                        replication_state, reference_count, created_at, updated_at
                 FROM artifacts WHERE artifact_id = ?1 AND owner_key = ?2",
                params![artifact_id.to_string(), owner_key],
                row_to_artifact,
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        if artifact.reference_count != 0 {
            return Err(StoreError::Conflict(format!(
                "artifact has {} references",
                artifact.reference_count
            )));
        }
        tx.execute(
            "DELETE FROM artifacts WHERE artifact_id = ?1 AND owner_key = ?2 AND reference_count = 0",
            params![artifact_id.to_string(), owner_key],
        )
        .map_err(|error| constraint_conflict(error, "artifact is still referenced"))?;
        if let Some(parent_id) = artifact.parent_artifact_id {
            let released = tx.execute(
                "UPDATE artifacts SET reference_count = reference_count - 1, updated_at = ?3
                 WHERE artifact_id = ?1 AND owner_key = ?2 AND reference_count > 0",
                params![parent_id.to_string(), owner_key, Utc::now().to_rfc3339()],
            )?;
            if released != 1 {
                return Err(StoreError::Conflict(
                    "parent artifact has no reference to release".into(),
                ));
            }
        }
        tx.commit()?;
        Ok(artifact)
    }

    /// Atomically forget a local physical replica after its files have been
    /// removed and the global fleet record is confirmed absent.
    pub fn delete_local_replica_metadata_if_unreferenced(
        &self,
        owner_key: &str,
        artifact_id: Uuid,
        snapshot_path: &str,
    ) -> Result<ArtifactRecord, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let artifact = tx
            .query_row(
                "SELECT artifact_id, owner_key, host_id, storage_locator, kind, status,
                        content_digest, size_bytes, immutable_image_digest, agent_digest,
                        boot_manifest_digest, parent_artifact_id, source_vm_id, creation_revision,
                        integrity_manifest_digest, chunk_size_bytes, chunk_count,
                        replication_state, reference_count, created_at, updated_at
                   FROM artifacts WHERE artifact_id = ?1 AND owner_key = ?2",
                params![artifact_id.to_string(), owner_key],
                row_to_artifact,
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        if artifact.reference_count != 0 || artifact.storage_locator != snapshot_path {
            return Err(StoreError::Conflict(
                "local replica metadata is referenced or has a different locator".into(),
            ));
        }
        if tx.execute(
            "DELETE FROM snapshots
              WHERE snapshot_id = ?1 AND path = ?2 AND owner_key = ?3",
            params![artifact_id.to_string(), snapshot_path, owner_key],
        )? != 1
        {
            return Err(StoreError::Conflict(
                "local replica snapshot metadata does not match".into(),
            ));
        }
        if tx.execute(
            "DELETE FROM artifacts
              WHERE artifact_id = ?1 AND owner_key = ?2 AND reference_count = 0",
            params![artifact_id.to_string(), owner_key],
        )? != 1
        {
            return Err(StoreError::Conflict(
                "local replica artifact remained referenced".into(),
            ));
        }
        if let Some(parent_id) = artifact.parent_artifact_id {
            if tx.execute(
                "UPDATE artifacts SET reference_count = reference_count - 1, updated_at = ?3
                  WHERE artifact_id = ?1 AND owner_key = ?2 AND reference_count > 0",
                params![parent_id.to_string(), owner_key, Utc::now().to_rfc3339()],
            )? != 1
            {
                return Err(StoreError::Conflict(
                    "parent artifact has no reference to release".into(),
                ));
            }
        }
        tx.commit()?;
        Ok(artifact)
    }

    pub fn get_egress_policy(
        &self,
        owner_key: &str,
        vm_id: Uuid,
    ) -> Result<Option<EgressPolicyRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT vm_id, owner_key, revision, allowlist_json, allow_existing,
                        created_at, updated_at
                 FROM vm_egress_policies WHERE vm_id = ?1 AND owner_key = ?2",
                params![vm_id.to_string(), owner_key],
                row_to_egress_policy,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Persist a desired policy with compare-and-swap semantics. A missing row
    /// behaves as the implicit default-deny revision 1 resource, so the first
    /// update must expect revision 1 and creates revision 2.
    pub fn update_egress_policy(
        &self,
        owner_key: &str,
        vm_id: Uuid,
        expected_revision: u64,
        allowlist: &[String],
        allow_existing: bool,
        now: DateTime<Utc>,
    ) -> Result<EgressPolicyRecord, StoreError> {
        if expected_revision == 0 {
            return Err(StoreError::Conflict(
                "egress policy revision must be positive".into(),
            ));
        }
        let tx = self.conn.unchecked_transaction()?;
        let current = tx
            .query_row(
                "SELECT vm_id, owner_key, revision, allowlist_json, allow_existing,
                        created_at, updated_at
                 FROM vm_egress_policies WHERE vm_id = ?1 AND owner_key = ?2",
                params![vm_id.to_string(), owner_key],
                row_to_egress_policy,
            )
            .optional()?;
        let current_revision = current.as_ref().map_or(1, |policy| policy.revision);
        if current_revision != expected_revision {
            return Err(StoreError::Conflict(format!(
                "egress policy revision is {current_revision}, expected {expected_revision}"
            )));
        }
        let revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| StoreError::Conflict("egress policy revision overflow".into()))?;
        let created_at = current.as_ref().map_or(now, |policy| policy.created_at);
        let allowlist_json = serde_json::to_string(allowlist)
            .map_err(|error| StoreError::Conflict(format!("encode egress policy: {error}")))?;
        let changed = tx.execute(
            "INSERT INTO vm_egress_policies (
               vm_id, owner_key, revision, allowlist_json, allow_existing, created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(vm_id) DO UPDATE SET
               revision = excluded.revision,
               allowlist_json = excluded.allowlist_json,
               allow_existing = excluded.allow_existing,
               updated_at = excluded.updated_at
             WHERE vm_egress_policies.owner_key = excluded.owner_key
               AND vm_egress_policies.revision = ?8",
            params![
                vm_id.to_string(),
                owner_key,
                u64_to_sql_i64(revision)?,
                allowlist_json,
                allow_existing as i64,
                created_at.to_rfc3339(),
                now.to_rfc3339(),
                u64_to_sql_i64(expected_revision)?,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::NotFound);
        }
        let policy = tx.query_row(
            "SELECT vm_id, owner_key, revision, allowlist_json, allow_existing,
                    created_at, updated_at
             FROM vm_egress_policies WHERE vm_id = ?1 AND owner_key = ?2",
            params![vm_id.to_string(), owner_key],
            row_to_egress_policy,
        )?;
        tx.commit()?;
        Ok(policy)
    }

    /// Install an already-CAS-validated fleet policy during hibernated VM
    /// recovery. This is not a public mutation path: callers must first fence
    /// VM ownership in PostgreSQL and authenticate the tenant binding.
    pub fn upsert_recovered_egress_policy(
        &self,
        policy: &EgressPolicyRecord,
    ) -> Result<(), StoreError> {
        if policy.revision == 0 {
            return Err(StoreError::Conflict(
                "egress policy revision must be positive".into(),
            ));
        }
        let allowlist_json = serde_json::to_string(&policy.allowlist)
            .map_err(|error| StoreError::Conflict(format!("encode egress policy: {error}")))?;
        let changed = self.conn.execute(
            "INSERT INTO vm_egress_policies (
               vm_id, owner_key, revision, allowlist_json, allow_existing, created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(vm_id) DO UPDATE SET
               owner_key = excluded.owner_key,
               revision = excluded.revision,
               allowlist_json = excluded.allowlist_json,
               allow_existing = excluded.allow_existing,
               created_at = excluded.created_at,
               updated_at = excluded.updated_at
             WHERE vm_egress_policies.owner_key = excluded.owner_key",
            params![
                policy.vm_id.to_string(),
                policy.owner_key,
                u64_to_sql_i64(policy.revision)?,
                allowlist_json,
                policy.allow_existing as i64,
                policy.created_at.to_rfc3339(),
                policy.updated_at.to_rfc3339(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "egress policy belongs to another tenant".into(),
            ));
        }
        Ok(())
    }

    pub fn delete_egress_policy(&self, owner_key: &str, vm_id: Uuid) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM vm_egress_policies WHERE vm_id = ?1 AND owner_key = ?2",
            params![vm_id.to_string(), owner_key],
        )?;
        Ok(())
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
                    memory_mib, vcpus, kernel_path, rootfs_path, rootfs_read_only, cmdline,
                    runtime_overlay_path, runtime_jail_path, runtime_artifact_paths,
                    socket_path, pid, created_at, updated_at
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
            "SELECT EXISTS(
               SELECT 1 FROM vms WHERE id = ?1
               UNION ALL
               SELECT 1 FROM vm_fork_operations WHERE child_vm_id = ?1
             )",
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

    /// Atomically bind a fork id to its tenant and source while reserving its
    /// admission slot. A matching interrupted operation may reacquire an
    /// expired/released reservation; no other create or fork may share it.
    pub fn claim_fork_operation(
        &self,
        operation: &ForkOperationRecord,
        max_vms: usize,
        expires_at: DateTime<Utc>,
    ) -> Result<ForkOperationClaimOutcome, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "DELETE FROM vm_quota_reservations WHERE expires_at <= ?1",
            params![now],
        )?;
        let existing = tx
            .query_row(
                "SELECT child_vm_id, source_vm_id, owner_key, source_host_id, target_host_id,
                        status, child_created_at, created_at, updated_at
                 FROM vm_fork_operations WHERE child_vm_id = ?1",
                params![operation.child_vm_id.to_string()],
                row_to_fork_operation,
            )
            .optional()?;
        let outcome = if let Some(existing) = existing {
            if existing.source_vm_id != operation.source_vm_id
                || existing.owner_key != operation.owner_key
                || existing.source_host_id != operation.source_host_id
                || existing.target_host_id != operation.target_host_id
            {
                return Err(StoreError::Conflict(format!(
                    "fork child {} is already bound to another operation",
                    operation.child_vm_id
                )));
            }
            if existing.status == ForkOperationStatus::Committed {
                tx.commit()?;
                return Ok(ForkOperationClaimOutcome::Committed);
            }
            ForkOperationClaimOutcome::Resumed
        } else {
            let vm_exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM vms WHERE id = ?1)",
                params![operation.child_vm_id.to_string()],
                |row| row.get(0),
            )?;
            if vm_exists {
                return Err(StoreError::Conflict(format!(
                    "VM {} already exists",
                    operation.child_vm_id
                )));
            }
            tx.execute(
                "INSERT INTO vm_fork_operations (
                   child_vm_id, source_vm_id, owner_key, source_host_id, target_host_id,
                   status, child_created_at, created_at, updated_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,NULL,?7,?8)",
                params![
                    operation.child_vm_id.to_string(),
                    operation.source_vm_id.to_string(),
                    operation.owner_key,
                    operation.source_host_id,
                    operation.target_host_id,
                    ForkOperationStatus::Preparing.as_str(),
                    operation.created_at.to_rfc3339(),
                    operation.updated_at.to_rfc3339(),
                ],
            )?;
            ForkOperationClaimOutcome::New
        };

        let reservation_owner = tx
            .query_row(
                "SELECT owner_key FROM vm_quota_reservations WHERE id = ?1",
                params![operation.child_vm_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match reservation_owner {
            Some(owner) if owner != operation.owner_key => {
                return Err(StoreError::Conflict(format!(
                    "VM {} reservation belongs to another tenant",
                    operation.child_vm_id
                )));
            }
            Some(_) => {
                if outcome == ForkOperationClaimOutcome::Resumed {
                    tx.commit()?;
                    return Ok(ForkOperationClaimOutcome::InProgress);
                }
                tx.execute(
                    "UPDATE vm_quota_reservations SET expires_at = ?2 WHERE id = ?1",
                    params![operation.child_vm_id.to_string(), expires_at.to_rfc3339()],
                )?;
            }
            None => {
                let child_exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM vms WHERE id = ?1)",
                    params![operation.child_vm_id.to_string()],
                    |row| row.get(0),
                )?;
                if !child_exists {
                    let active: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM (
                           SELECT id FROM vms
                            WHERE owner_key = ?1 AND status IN ('creating','running','paused','suspended')
                           UNION
                           SELECT id FROM vm_quota_reservations
                            WHERE owner_key = ?1 AND expires_at > ?2
                         )",
                        params![operation.owner_key, now],
                        |row| row.get(0),
                    )?;
                    if active >= i64::try_from(max_vms).unwrap_or(i64::MAX) {
                        return Ok(ForkOperationClaimOutcome::QuotaExceeded);
                    }
                    tx.execute(
                        "INSERT INTO vm_quota_reservations (id, owner_key, expires_at)
                         VALUES (?1,?2,?3)",
                        params![
                            operation.child_vm_id.to_string(),
                            operation.owner_key,
                            expires_at.to_rfc3339()
                        ],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(outcome)
    }

    pub fn get_fork_operation(
        &self,
        child_vm_id: Uuid,
    ) -> Result<Option<ForkOperationRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT child_vm_id, source_vm_id, owner_key, source_host_id, target_host_id,
                        status, child_created_at, created_at, updated_at
                 FROM vm_fork_operations WHERE child_vm_id = ?1",
                params![child_vm_id.to_string()],
                row_to_fork_operation,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn commit_fork_operation(
        &self,
        child_vm_id: Uuid,
        source_vm_id: Uuid,
        owner_key: &str,
        child_created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE vm_fork_operations
             SET status = 'committed', child_created_at = ?4, updated_at = ?5
             WHERE child_vm_id = ?1 AND source_vm_id = ?2 AND owner_key = ?3
               AND (status = 'preparing'
                    OR (status = 'committed' AND child_created_at = ?4))",
            params![
                child_vm_id.to_string(),
                source_vm_id.to_string(),
                owner_key,
                child_created_at.to_rfc3339(),
                updated_at.to_rfc3339(),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Conflict(format!(
                "fork child {child_vm_id} operation changed concurrently"
            )));
        }
        Ok(())
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
            "INSERT INTO hosts (host_id, boot_session_id, peer_certificate_sha256, rpc_addr, sandbox_count, free_vcpus, free_memory_mib, healthy, last_heartbeat)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(host_id) DO UPDATE SET
               boot_session_id = excluded.boot_session_id,
               peer_certificate_sha256 = excluded.peer_certificate_sha256,
               rpc_addr = excluded.rpc_addr,
               sandbox_count = excluded.sandbox_count,
               free_vcpus = excluded.free_vcpus,
               free_memory_mib = excluded.free_memory_mib,
               healthy = excluded.healthy,
               last_heartbeat = excluded.last_heartbeat",
            params![
                host.host_id,
                host.boot_session_id.map(|id| id.to_string()),
                host.peer_certificate_sha256,
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
            "SELECT host_id, boot_session_id, peer_certificate_sha256, rpc_addr, sandbox_count, free_vcpus, free_memory_mib, healthy, last_heartbeat
             FROM hosts ORDER BY host_id",
        )?;
        let rows = stmt.query_map([], |row| {
            let hb: String = row.get(8)?;
            Ok(HostRecord {
                host_id: row.get(0)?,
                boot_session_id: row
                    .get::<_, Option<String>>(1)?
                    .and_then(|value| Uuid::parse_str(&value).ok()),
                peer_certificate_sha256: row.get(2)?,
                rpc_addr: row.get(3)?,
                sandbox_count: row.get::<_, i64>(4)? as usize,
                free_vcpus: row.get::<_, i64>(5)? as u64,
                free_memory_mib: row.get::<_, i64>(6)? as u64,
                healthy: row.get::<_, i64>(7)? != 0,
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
               name, tag, rootfs_path, created_at, size_bytes, source_ref, source_digest,
               rootfs_digest, agent_digest, provenance_key_digest, provenance_verified_at,
               golden_snapshot_path
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
             ON CONFLICT(name, tag) DO UPDATE SET
               rootfs_path = excluded.rootfs_path,
               created_at = excluded.created_at,
               size_bytes = excluded.size_bytes,
               source_ref = excluded.source_ref,
               source_digest = excluded.source_digest,
               rootfs_digest = excluded.rootfs_digest,
               agent_digest = excluded.agent_digest,
               provenance_key_digest = excluded.provenance_key_digest,
               provenance_verified_at = excluded.provenance_verified_at,
               golden_snapshot_path = excluded.golden_snapshot_path",
            params![
                image.name,
                image.tag,
                image.rootfs_path,
                image.created_at.to_rfc3339(),
                image.size_bytes as i64,
                image.source_ref,
                image.source_digest,
                image.rootfs_digest,
                image.agent_digest,
                image.provenance_key_digest,
                image.provenance_verified_at.map(|time| time.to_rfc3339()),
                image.golden_snapshot_path,
            ],
        )?;
        Ok(())
    }

    pub fn get_image(&self, name: &str, tag: &str) -> Result<ImageRecord, StoreError> {
        self.conn
            .query_row(
                "SELECT name, tag, rootfs_path, created_at, size_bytes, source_ref, source_digest,
                        rootfs_digest, agent_digest, provenance_key_digest,
                        provenance_verified_at, golden_snapshot_path
                 FROM images WHERE name = ?1 AND tag = ?2",
                params![name, tag],
                row_to_image,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    pub fn get_image_by_rootfs_path(
        &self,
        rootfs_path: &str,
    ) -> Result<Option<ImageRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT name, tag, rootfs_path, created_at, size_bytes, source_ref, source_digest,
                        rootfs_digest, agent_digest, provenance_key_digest,
                        provenance_verified_at, golden_snapshot_path
                 FROM images WHERE rootfs_path = ?1",
                params![rootfs_path],
                row_to_image,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn get_image_by_source_digest(
        &self,
        source_digest: &str,
    ) -> Result<Option<ImageRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT name, tag, rootfs_path, created_at, size_bytes, source_ref, source_digest,
                        rootfs_digest, agent_digest, provenance_key_digest,
                        provenance_verified_at, golden_snapshot_path
                 FROM images WHERE source_digest = ?1 ORDER BY created_at DESC LIMIT 1",
                params![source_digest],
                row_to_image,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn list_images(&self) -> Result<Vec<ImageRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT name, tag, rootfs_path, created_at, size_bytes, source_ref, source_digest,
                    rootfs_digest, agent_digest, provenance_key_digest,
                    provenance_verified_at, golden_snapshot_path
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

    /// Insert an immutable desired volume record. Exact replay is idempotent;
    /// reusing an id or tenant-local name for different properties conflicts.
    pub fn insert_volume(&self, volume: &VolumeRecord) -> Result<VolumeRecord, StoreError> {
        if volume.owner_key.is_empty()
            || volume.name.is_empty()
            || volume.provider.is_empty()
            || volume.generation == 0
            || volume.revision == 0
        {
            return Err(StoreError::Conflict(
                "volume identity, provider, generation, and revision are required".into(),
            ));
        }
        let inserted = self.conn.execute(
            "INSERT INTO volumes (
               id, owner_key, name, provider, storage_class, size_bytes, status,
               read_only_many, read_write_once, read_write_many, snapshots, clones,
               host_id, region, zone, generation, revision, last_error, created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)
             ON CONFLICT DO NOTHING",
            params![
                volume.id.to_string(),
                volume.owner_key,
                volume.name,
                volume.provider,
                volume.storage_class.as_str(),
                u64_to_sql_i64(volume.size_bytes)?,
                volume.status.as_str(),
                volume.capabilities.read_only_many as i64,
                volume.capabilities.read_write_once as i64,
                volume.capabilities.read_write_many as i64,
                volume.capabilities.snapshots as i64,
                volume.capabilities.clones as i64,
                volume.host_id,
                volume.region,
                volume.zone,
                u64_to_sql_i64(volume.generation)?,
                u64_to_sql_i64(volume.revision)?,
                volume.last_error,
                volume.created_at.to_rfc3339(),
                volume.updated_at.to_rfc3339(),
            ],
        )?;
        if inserted == 1 {
            return Ok(volume.clone());
        }
        let existing = self
            .get_volume(&volume.owner_key, volume.id)
            .or_else(|_| self.get_volume_by_name(&volume.owner_key, &volume.name))?;
        if same_immutable_volume(&existing, volume) {
            Ok(existing)
        } else {
            Err(StoreError::Conflict(
                "volume id or name already exists with different immutable properties".into(),
            ))
        }
    }

    pub fn get_volume(&self, owner_key: &str, id: Uuid) -> Result<VolumeRecord, StoreError> {
        self.conn
            .query_row(
                "SELECT id, owner_key, name, provider, storage_class, size_bytes, status,
                        read_only_many, read_write_once, read_write_many, snapshots, clones,
                        host_id, region, zone, generation, revision, last_error, created_at, updated_at
                 FROM volumes WHERE id = ?1 AND owner_key = ?2",
                params![id.to_string(), owner_key],
                row_to_volume,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    fn get_volume_by_name(&self, owner_key: &str, name: &str) -> Result<VolumeRecord, StoreError> {
        self.conn
            .query_row(
                "SELECT id, owner_key, name, provider, storage_class, size_bytes, status,
                        read_only_many, read_write_once, read_write_many, snapshots, clones,
                        host_id, region, zone, generation, revision, last_error, created_at, updated_at
                 FROM volumes WHERE name = ?1 AND owner_key = ?2",
                params![name, owner_key],
                row_to_volume,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    pub fn list_volumes(&self, owner_key: &str) -> Result<Vec<VolumeRecord>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT id, owner_key, name, provider, storage_class, size_bytes, status,
                    read_only_many, read_write_once, read_write_many, snapshots, clones,
                    host_id, region, zone, generation, revision, last_error, created_at, updated_at
             FROM volumes WHERE owner_key = ?1 ORDER BY created_at, id",
        )?;
        let volumes = statement
            .query_map(params![owner_key], row_to_volume)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(volumes)
    }

    pub fn transition_volume(
        &self,
        owner_key: &str,
        id: Uuid,
        transition: VolumeTransition<'_>,
    ) -> Result<VolumeRecord, StoreError> {
        let changed = self.conn.execute(
            "UPDATE volumes
             SET status = ?5, revision = revision + 1, last_error = ?6, updated_at = ?7
             WHERE id = ?1 AND owner_key = ?2 AND status = ?3 AND revision = ?4",
            params![
                id.to_string(),
                owner_key,
                transition.expected_status.as_str(),
                u64_to_sql_i64(transition.expected_revision)?,
                transition.status.as_str(),
                transition.last_error,
                transition.updated_at.to_rfc3339(),
            ],
        )?;
        if changed != 1 {
            return if self.get_volume(owner_key, id).is_ok() {
                Err(StoreError::Conflict("stale volume transition".into()))
            } else {
                Err(StoreError::NotFound)
            };
        }
        self.get_volume(owner_key, id)
    }

    pub fn begin_volume_delete(
        &self,
        owner_key: &str,
        id: Uuid,
        expected_status: VolumeStatus,
        expected_revision: u64,
        updated_at: DateTime<Utc>,
    ) -> Result<VolumeRecord, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let attached: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM vm_volume_attachments WHERE volume_id = ?1)",
            params![id.to_string()],
            |row| row.get(0),
        )?;
        if attached {
            return Err(StoreError::Conflict("volume is attached to a VM".into()));
        }
        let next_revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| StoreError::Conflict("volume revision exhausted".into()))?;
        let changed = tx.execute(
            "UPDATE volumes SET status = 'deleting', revision = ?1, last_error = NULL,
             updated_at = ?2 WHERE id = ?3 AND owner_key = ?4 AND status = ?5 AND revision = ?6",
            params![
                u64_to_sql_i64(next_revision)?,
                updated_at.to_rfc3339(),
                id.to_string(),
                owner_key,
                expected_status.as_str(),
                u64_to_sql_i64(expected_revision)?,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict("stale volume deletion claim".into()));
        }
        tx.commit()?;
        self.get_volume(owner_key, id)
    }

    pub fn delete_volume_metadata(
        &self,
        owner_key: &str,
        id: Uuid,
        expected_revision: u64,
    ) -> Result<(), StoreError> {
        let attached: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM vm_volume_attachments WHERE volume_id = ?1)",
            params![id.to_string()],
            |row| row.get(0),
        )?;
        if attached {
            return Err(StoreError::Conflict("volume is attached to a VM".into()));
        }
        let changed = self.conn.execute(
            "DELETE FROM volumes
             WHERE id = ?1 AND owner_key = ?2 AND status = 'deleting' AND revision = ?3",
            params![
                id.to_string(),
                owner_key,
                u64_to_sql_i64(expected_revision)?
            ],
        )?;
        if changed != 1 {
            return if self.get_volume(owner_key, id).is_ok() {
                Err(StoreError::Conflict("stale volume deletion".into()))
            } else {
                Err(StoreError::NotFound)
            };
        }
        Ok(())
    }

    /// Atomically acquire the desired VM-volume bindings. The transaction is
    /// the durable RWO fence; providers advertising RWX may bind many writers.
    pub fn bind_vm_volumes(
        &self,
        attachments: &[VmVolumeAttachmentRecord],
    ) -> Result<(), StoreError> {
        if attachments.len() > 15 {
            return Err(StoreError::Conflict(
                "a VM supports at most 15 persistent data volumes".into(),
            ));
        }
        let tx = self.conn.unchecked_transaction()?;
        for attachment in attachments {
            if attachment.owner_key.is_empty()
                || attachment.volume_generation == 0
                || usize::from(attachment.device_index) >= attachments.len()
            {
                return Err(StoreError::Conflict(
                    "invalid VM volume attachment identity".into(),
                ));
            }
            let volume = tx
                .query_row(
                    "SELECT owner_key, status, generation, read_only_many, read_write_once,
                            read_write_many
                     FROM volumes WHERE id = ?1",
                    params![attachment.volume_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            sql_i64_to_u64(row.get(2)?, 2, "invalid volume generation")?,
                            row.get::<_, i64>(3)? != 0,
                            row.get::<_, i64>(4)? != 0,
                            row.get::<_, i64>(5)? != 0,
                        ))
                    },
                )
                .optional()?
                .ok_or(StoreError::NotFound)?;
            if volume.0 != attachment.owner_key
                || volume.1 != VolumeStatus::Available.as_str()
                || volume.2 != attachment.volume_generation
                || (attachment.mode == VolumeAttachmentMode::ReadOnly && !volume.3)
                || (attachment.mode == VolumeAttachmentMode::ReadWrite && !volume.4 && !volume.5)
            {
                return Err(StoreError::Conflict(
                    "volume is unavailable, belongs to another tenant, or lacks the requested access mode"
                        .into(),
                ));
            }
            let existing = tx
                .query_row(
                    "SELECT device_index, owner_key, mode, volume_generation
                     FROM vm_volume_attachments WHERE vm_id = ?1 AND volume_id = ?2",
                    params![
                        attachment.vm_id.to_string(),
                        attachment.volume_id.to_string()
                    ],
                    |row| {
                        Ok((
                            row.get::<_, u8>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            sql_i64_to_u64(row.get(3)?, 3, "invalid attachment generation")?,
                        ))
                    },
                )
                .optional()?;
            if let Some(existing) = existing {
                if existing
                    != (
                        attachment.device_index,
                        attachment.owner_key.clone(),
                        attachment.mode.as_str().to_string(),
                        attachment.volume_generation,
                    )
                {
                    return Err(StoreError::Conflict(
                        "VM volume attachment replay changed immutable properties".into(),
                    ));
                }
                continue;
            }
            let conflicting_access: bool = !volume.5
                && tx.query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM vm_volume_attachments
                       WHERE volume_id = ?1 AND (?2 = 'read_write' OR mode = 'read_write')
                     )",
                    params![attachment.volume_id.to_string(), attachment.mode.as_str()],
                    |row| row.get(0),
                )?;
            if conflicting_access {
                return Err(StoreError::Conflict(
                    "read-write volume attachment is exclusive".into(),
                ));
            }
            tx.execute(
                "INSERT INTO vm_volume_attachments
                 (vm_id, volume_id, device_index, owner_key, mode, volume_generation, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    attachment.vm_id.to_string(),
                    attachment.volume_id.to_string(),
                    attachment.device_index,
                    attachment.owner_key,
                    attachment.mode.as_str(),
                    u64_to_sql_i64(attachment.volume_generation)?,
                    attachment.created_at.to_rfc3339(),
                ],
            )
            .map_err(|error| {
                constraint_conflict(
                    error,
                    "volume is already attached read-write or device index is occupied",
                )
            })?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_vm_volume_attachments(
        &self,
        owner_key: &str,
        vm_id: Uuid,
    ) -> Result<Vec<VmVolumeAttachmentRecord>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT vm_id, volume_id, device_index, owner_key, mode, volume_generation, created_at
             FROM vm_volume_attachments WHERE vm_id = ?1 AND owner_key = ?2
             ORDER BY device_index",
        )?;
        let rows = statement.query_map(params![vm_id.to_string(), owner_key], |row| {
            let mode = row.get::<_, String>(4)?;
            let vm_id = row.get::<_, String>(0)?;
            let volume_id = row.get::<_, String>(1)?;
            Ok(VmVolumeAttachmentRecord {
                vm_id: parse_uuid_col(&vm_id, 0)?,
                volume_id: parse_uuid_col(&volume_id, 1)?,
                device_index: row.get(2)?,
                owner_key: row.get(3)?,
                mode: VolumeAttachmentMode::parse(&mode).ok_or_else(|| {
                    invalid_text_error(4, format!("invalid attachment mode: {mode}"))
                })?,
                volume_generation: sql_i64_to_u64(row.get(5)?, 5, "invalid attachment generation")?,
                created_at: parse_ts(&row.get::<_, String>(6)?)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn unbind_vm_volumes(&self, owner_key: &str, vm_id: Uuid) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM vm_volume_attachments WHERE vm_id = ?1 AND owner_key = ?2",
            params![vm_id.to_string(), owner_key],
        )?;
        Ok(())
    }

    pub fn volume_attachment_count(
        &self,
        owner_key: &str,
        volume_id: Uuid,
    ) -> Result<u64, StoreError> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM vm_volume_attachments WHERE owner_key = ?1 AND volume_id = ?2",
            params![owner_key, volume_id.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        sql_i64_to_u64(count, 0, "invalid volume attachment count").map_err(StoreError::from)
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
    let runtime_overlay_path: Option<String> = row.get(13)?;
    let runtime_jail_path: Option<String> = row.get(14)?;
    let runtime_artifact_paths: Option<String> = row.get(15)?;
    let runtime_layout = match (
        runtime_overlay_path,
        runtime_jail_path,
        runtime_artifact_paths,
    ) {
        (None, None, None) => None,
        (overlay_path, jail_path, artifact_paths) => {
            let artifact_paths = artifact_paths
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        15,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?
                .unwrap_or_default();
            Some(VmRuntimeLayout {
                overlay_path,
                jail_path,
                artifact_paths,
            })
        }
    };
    let created_at: String = row.get(18)?;
    let updated_at: String = row.get(19)?;
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
        rootfs_read_only: row.get(11)?,
        cmdline: row.get(12)?,
        runtime_layout,
        socket_path: row.get(16)?,
        pid: row.get(17)?,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
    })
}

fn row_to_fork_operation(row: &rusqlite::Row<'_>) -> Result<ForkOperationRecord, rusqlite::Error> {
    let child_vm_id: String = row.get(0)?;
    let source_vm_id: String = row.get(1)?;
    let status: String = row.get(5)?;
    let child_created_at: Option<String> = row.get(6)?;
    let created_at: String = row.get(7)?;
    let updated_at: String = row.get(8)?;
    let status = ForkOperationStatus::parse(&status).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            format!("invalid fork operation status {status}").into(),
        )
    })?;
    Ok(ForkOperationRecord {
        child_vm_id: parse_uuid_col(&child_vm_id, 0)?,
        source_vm_id: parse_uuid_col(&source_vm_id, 1)?,
        owner_key: row.get(2)?,
        source_host_id: row.get(3)?,
        target_host_id: row.get(4)?,
        status,
        child_created_at: child_created_at.as_deref().map(parse_ts).transpose()?,
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
    let provenance_verified_at: Option<String> = row.get(10)?;
    Ok(ImageRecord {
        name: row.get(0)?,
        tag: row.get(1)?,
        rootfs_path: row.get(2)?,
        created_at: parse_ts(&created_at)?,
        size_bytes: size_bytes.max(0) as u64,
        source_ref: row.get(5)?,
        source_digest: row.get(6)?,
        rootfs_digest: row.get(7)?,
        agent_digest: row.get(8)?,
        provenance_key_digest: row.get(9)?,
        provenance_verified_at: provenance_verified_at
            .as_deref()
            .map(parse_ts)
            .transpose()?,
        golden_snapshot_path: row.get(11)?,
    })
}

fn row_to_volume(row: &rusqlite::Row<'_>) -> Result<VolumeRecord, rusqlite::Error> {
    let id: String = row.get(0)?;
    let storage_class: String = row.get(4)?;
    let status: String = row.get(6)?;
    let created_at: String = row.get(18)?;
    let updated_at: String = row.get(19)?;
    Ok(VolumeRecord {
        id: parse_uuid_col(&id, 0)?,
        owner_key: row.get(1)?,
        name: row.get(2)?,
        provider: row.get(3)?,
        storage_class: VolumeStorageClass::parse(&storage_class).ok_or_else(|| {
            invalid_text_error(4, format!("invalid volume storage class: {storage_class}"))
        })?,
        size_bytes: sql_i64_to_u64(row.get(5)?, 5, "invalid volume size")?,
        status: VolumeStatus::parse(&status)
            .ok_or_else(|| invalid_text_error(6, format!("invalid volume status: {status}")))?,
        capabilities: VolumeCapabilities {
            read_only_many: row.get::<_, i64>(7)? != 0,
            read_write_once: row.get::<_, i64>(8)? != 0,
            read_write_many: row.get::<_, i64>(9)? != 0,
            snapshots: row.get::<_, i64>(10)? != 0,
            clones: row.get::<_, i64>(11)? != 0,
        },
        host_id: row.get(12)?,
        region: row.get(13)?,
        zone: row.get(14)?,
        generation: sql_i64_to_u64(row.get(15)?, 15, "invalid volume generation")?,
        revision: sql_i64_to_u64(row.get(16)?, 16, "invalid volume revision")?,
        last_error: row.get(17)?,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
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

fn row_to_egress_policy(row: &rusqlite::Row<'_>) -> Result<EgressPolicyRecord, rusqlite::Error> {
    let vm_id: String = row.get(0)?;
    let revision = sql_i64_to_u64(row.get(2)?, 2, "invalid egress revision")?;
    let allowlist_json: String = row.get(3)?;
    let created_at: String = row.get(5)?;
    let updated_at: String = row.get(6)?;
    let allowlist = serde_json::from_str(&allowlist_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(EgressPolicyRecord {
        vm_id: parse_uuid_col(&vm_id, 0)?,
        owner_key: row.get(1)?,
        revision,
        allowlist,
        allow_existing: row.get::<_, i64>(4)? != 0,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
    })
}

fn row_to_artifact(row: &rusqlite::Row<'_>) -> Result<ArtifactRecord, rusqlite::Error> {
    let artifact_id: String = row.get(0)?;
    let kind: String = row.get(4)?;
    let status: String = row.get(5)?;
    let parent_artifact_id: Option<String> = row.get(11)?;
    let source_vm_id: Option<String> = row.get(12)?;
    let replication_state: String = row.get(17)?;
    let created_at: String = row.get(19)?;
    let updated_at: String = row.get(20)?;
    Ok(ArtifactRecord {
        artifact_id: parse_uuid_col(&artifact_id, 0)?,
        owner_key: row.get(1)?,
        host_id: row.get(2)?,
        storage_locator: row.get(3)?,
        kind: ArtifactKind::parse(&kind)
            .ok_or_else(|| invalid_text_error(4, format!("invalid artifact kind: {kind}")))?,
        status: ArtifactStatus::parse(&status)
            .ok_or_else(|| invalid_text_error(5, format!("invalid artifact status: {status}")))?,
        content_digest: row.get(6)?,
        size_bytes: sql_i64_to_u64(row.get(7)?, 7, "invalid artifact size")?,
        immutable_image_digest: row.get(8)?,
        agent_digest: row.get(9)?,
        boot_manifest_digest: row.get(10)?,
        parent_artifact_id: parse_optional_uuid_col(parent_artifact_id, 11)?,
        source_vm_id: parse_optional_uuid_col(source_vm_id, 12)?,
        creation_revision: sql_i64_to_u64(row.get(13)?, 13, "invalid creation revision")?,
        integrity_manifest_digest: row.get(14)?,
        chunk_size_bytes: sql_i64_to_u64(row.get(15)?, 15, "invalid chunk size")?,
        chunk_count: sql_i64_to_u64(row.get(16)?, 16, "invalid chunk count")?,
        replication_state: ArtifactReplicationState::parse(&replication_state).ok_or_else(
            || {
                invalid_text_error(
                    17,
                    format!("invalid artifact replication state: {replication_state}"),
                )
            },
        )?,
        reference_count: sql_i64_to_u64(row.get(18)?, 18, "invalid reference count")?,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
    })
}

fn row_to_artifact_replica(
    row: &rusqlite::Row<'_>,
) -> Result<ArtifactReplicaRecord, rusqlite::Error> {
    let artifact_id: String = row.get(0)?;
    let status: String = row.get(5)?;
    let verified_at: Option<String> = row.get(9)?;
    let created_at: String = row.get(10)?;
    let updated_at: String = row.get(11)?;
    Ok(ArtifactReplicaRecord {
        artifact_id: parse_uuid_col(&artifact_id, 0)?,
        owner_key: row.get(1)?,
        host_id: row.get(2)?,
        failure_domain: row.get(3)?,
        storage_locator: row.get(4)?,
        status: ArtifactReplicaStatus::parse(&status)
            .ok_or_else(|| invalid_text_error(5, format!("invalid replica status: {status}")))?,
        content_digest: row.get(6)?,
        size_bytes: sql_i64_to_u64(row.get(7)?, 7, "invalid replica size")?,
        integrity_manifest_digest: row.get(8)?,
        verified_at: verified_at.as_deref().map(parse_ts).transpose()?,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
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

fn recompute_artifact_replication_sqlite(
    tx: &rusqlite::Transaction<'_>,
    owner_key: &str,
    artifact_id: Uuid,
    min_replicas: u64,
    min_failure_domains: u64,
    updated_at: DateTime<Utc>,
) -> Result<ArtifactReplicationState, StoreError> {
    let (available, failure_domains): (i64, i64) = tx.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT failure_domain)
         FROM artifact_replicas
         WHERE artifact_id = ?1 AND owner_key = ?2 AND status = 'available'
           AND verified_at IS NOT NULL",
        params![artifact_id.to_string(), owner_key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let state = if available == 0 {
        ArtifactReplicationState::Pending
    } else if u64::try_from(available).unwrap_or(0) >= min_replicas
        && u64::try_from(failure_domains).unwrap_or(0) >= min_failure_domains
    {
        ArtifactReplicationState::Ready
    } else {
        ArtifactReplicationState::Degraded
    };
    let changed = tx.execute(
        "UPDATE artifacts SET replication_state = ?3, updated_at = ?4
         WHERE artifact_id = ?1 AND owner_key = ?2",
        params![
            artifact_id.to_string(),
            owner_key,
            state.as_str(),
            updated_at.to_rfc3339()
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::NotFound);
    }
    Ok(state)
}

fn row_to_branch(row: &rusqlite::Row<'_>) -> Result<BranchRecord, rusqlite::Error> {
    let branch_id: String = row.get(0)?;
    let head_artifact_id: String = row.get(3)?;
    let source_vm_id: Option<String> = row.get(4)?;
    let source_branch_id: Option<String> = row.get(5)?;
    let created_at: String = row.get(7)?;
    let updated_at: String = row.get(8)?;
    Ok(BranchRecord {
        branch_id: parse_uuid_col(&branch_id, 0)?,
        owner_key: row.get(1)?,
        name: row.get(2)?,
        head_artifact_id: parse_uuid_col(&head_artifact_id, 3)?,
        source_vm_id: parse_optional_uuid_col(source_vm_id, 4)?,
        source_branch_id: parse_optional_uuid_col(source_branch_id, 5)?,
        revision: sql_i64_to_u64(row.get(6)?, 6, "invalid branch revision")?,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
    })
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> Result<SnapshotRecord, rusqlite::Error> {
    let vm_id: String = row.get(5)?;
    let ephemeral_owner_vm_id: Option<String> = row.get(6)?;
    let size_bytes: Option<i64> = row.get(14)?;
    let created_at: String = row.get(15)?;
    let snapshot_id: String = row.get(16)?;
    Ok(SnapshotRecord {
        snapshot_id: parse_uuid_col(&snapshot_id, 16)?,
        path: row.get(0)?,
        overlay_path: row.get(1)?,
        host_id: row.get(2)?,
        owner_key: row.get(3)?,
        api_key_id: row.get(4)?,
        vm_id: parse_uuid_col(&vm_id, 5)?,
        ephemeral_owner_vm_id: parse_optional_uuid_col(ephemeral_owner_vm_id, 6)?,
        memory_mib: row.get(7)?,
        vcpus: row.get(8)?,
        kernel_path: row.get(9)?,
        rootfs_path: row.get(10)?,
        rootfs_read_only: row.get(11)?,
        cmdline: row.get(12)?,
        content_digest: row.get(13)?,
        size_bytes: size_bytes
            .map(|value| sql_i64_to_u64(value, 14, "invalid snapshot size"))
            .transpose()?,
        created_at: parse_ts(&created_at)?,
    })
}

fn row_to_hibernation(row: &rusqlite::Row<'_>) -> Result<HibernationRecord, rusqlite::Error> {
    let vm_id: String = row.get(0)?;
    let created_at: String = row.get(3)?;
    let updated_at: String = row.get(4)?;
    Ok(HibernationRecord {
        vm_id: parse_uuid_col(&vm_id, 0)?,
        owner_key: row.get(1)?,
        snapshot_path: row.get(2)?,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
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

fn sql_i64_to_u64(value: i64, column: usize, message: &str) -> Result<u64, rusqlite::Error> {
    u64::try_from(value).map_err(|_| invalid_integer_error(column, message))
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

fn constraint_conflict(error: rusqlite::Error, message: &str) -> StoreError {
    if matches!(
        &error,
        rusqlite::Error::SqliteFailure(db_error, _)
            if db_error.code == rusqlite::ErrorCode::ConstraintViolation
    ) {
        StoreError::Conflict(message.into())
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
    use tarit_types::{ShareRecord, ShareVisibility, VmRuntimeLayout};

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

    fn test_vm(id: Uuid, owner_key: &str) -> VmRecord {
        let now = Utc::now();
        VmRecord {
            id,
            host_id: "host-a".into(),
            owner_key: Some(owner_key.into()),
            api_key_id: None,
            status: VmStatus::Creating,
            revision: 1,
            startup_path: Some(VmStartupPath::Cold),
            memory_mib: 256,
            vcpus: 1,
            kernel_path: "/kernel".into(),
            rootfs_path: Some("/rootfs".into()),
            rootfs_read_only: false,
            cmdline: "console=ttyS0".into(),
            runtime_layout: None,
            socket_path: None,
            pid: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn test_artifact(owner_key: &str, locator: &str) -> ArtifactRecord {
        let now = Utc::now();
        ArtifactRecord {
            artifact_id: Uuid::new_v4(),
            owner_key: owner_key.into(),
            host_id: "host-a".into(),
            storage_locator: locator.into(),
            kind: ArtifactKind::VmSnapshot,
            status: ArtifactStatus::Available,
            content_digest: format!("sha256:{}", Uuid::new_v4()),
            size_bytes: 8192,
            immutable_image_digest: "sha256:image".into(),
            agent_digest: "sha256:agent".into(),
            boot_manifest_digest: "sha256:boot".into(),
            parent_artifact_id: None,
            source_vm_id: Some(Uuid::new_v4()),
            creation_revision: 1,
            integrity_manifest_digest: "sha256:manifest".into(),
            chunk_size_bytes: 4096,
            chunk_count: 2,
            replication_state: ArtifactReplicationState::Ready,
            reference_count: 0,
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
    fn artifact_branch_cas_transfers_references_without_cross_tenant_leaks() {
        let store = Store::open(":memory:").unwrap();
        let first = test_artifact("tenant-a", "/private/first");
        let second = test_artifact("tenant-a", "/private/second");
        let foreign = test_artifact("tenant-b", "/private/foreign");
        store.insert_artifact(&first).unwrap();
        store.insert_artifact(&second).unwrap();
        store.insert_artifact(&foreign).unwrap();

        assert!(matches!(
            store.get_artifact("tenant-b", first.artifact_id),
            Err(StoreError::NotFound)
        ));
        let now = Utc::now();
        let branch = BranchRecord {
            branch_id: Uuid::new_v4(),
            owner_key: "tenant-a".into(),
            name: "main".into(),
            head_artifact_id: first.artifact_id,
            source_vm_id: None,
            source_branch_id: None,
            revision: 1,
            created_at: now,
            updated_at: now,
        };
        store.insert_branch(&branch).unwrap();
        assert_eq!(
            store
                .get_artifact("tenant-a", first.artifact_id)
                .unwrap()
                .reference_count,
            1
        );

        let updated = store
            .update_branch_head(
                "tenant-a",
                branch.branch_id,
                1,
                second.artifact_id,
                Utc::now(),
            )
            .unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.head_artifact_id, second.artifact_id);
        assert_eq!(
            store
                .get_artifact("tenant-a", first.artifact_id)
                .unwrap()
                .reference_count,
            0
        );
        assert_eq!(
            store
                .get_artifact("tenant-a", second.artifact_id)
                .unwrap()
                .reference_count,
            1
        );
        assert!(matches!(
            store.update_branch_head(
                "tenant-a",
                branch.branch_id,
                1,
                first.artifact_id,
                Utc::now()
            ),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(
            store.update_branch_head(
                "tenant-a",
                branch.branch_id,
                2,
                foreign.artifact_id,
                Utc::now()
            ),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.delete_artifact_if_unreferenced("tenant-a", second.artifact_id),
            Err(StoreError::Conflict(_))
        ));

        store.delete_branch("tenant-a", branch.branch_id).unwrap();
        assert_eq!(
            store
                .get_artifact("tenant-a", second.artifact_id)
                .unwrap()
                .reference_count,
            0
        );
        store
            .delete_artifact_if_unreferenced("tenant-a", second.artifact_id)
            .unwrap();
        assert!(matches!(
            store.get_artifact("tenant-a", second.artifact_id),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn artifact_parent_reference_prevents_premature_gc() {
        let store = Store::open(":memory:").unwrap();
        let parent = test_artifact("tenant-a", "/private/parent");
        store.insert_artifact(&parent).unwrap();
        let child = ArtifactRecord {
            parent_artifact_id: Some(parent.artifact_id),
            ..test_artifact("tenant-a", "/private/child")
        };
        store.insert_artifact(&child).unwrap();
        assert_eq!(
            store
                .get_artifact("tenant-a", parent.artifact_id)
                .unwrap()
                .reference_count,
            1
        );
        assert!(matches!(
            store.delete_artifact_if_unreferenced("tenant-a", parent.artifact_id),
            Err(StoreError::Conflict(_))
        ));
        store
            .delete_artifact_if_unreferenced("tenant-a", child.artifact_id)
            .unwrap();
        assert_eq!(
            store
                .get_artifact("tenant-a", parent.artifact_id)
                .unwrap()
                .reference_count,
            0
        );
    }

    #[test]
    fn artifact_insert_is_idempotent_and_replication_is_failure_domain_derived() {
        let store = Store::open(":memory:").unwrap();
        let mut artifact = test_artifact("tenant-a", "/private/primary");
        artifact.replication_state = ArtifactReplicationState::Pending;
        store.insert_artifact(&artifact).unwrap();
        store.insert_artifact(&artifact).unwrap();
        assert_eq!(
            store
                .list_artifact_replicas("tenant-a", artifact.artifact_id)
                .unwrap()
                .len(),
            1,
            "idempotent replay must not duplicate the primary replica"
        );

        let now = Utc::now();
        let replica = ArtifactReplicaRecord {
            artifact_id: artifact.artifact_id,
            owner_key: artifact.owner_key.clone(),
            host_id: "host-b".into(),
            failure_domain: "zone-b".into(),
            storage_locator: "/private/replica-b".into(),
            status: ArtifactReplicaStatus::Available,
            content_digest: artifact.content_digest.clone(),
            size_bytes: artifact.size_bytes,
            integrity_manifest_digest: artifact.integrity_manifest_digest.clone(),
            verified_at: Some(now),
            created_at: now,
            updated_at: now,
        };
        assert_eq!(
            store.upsert_artifact_replica(&replica, 2, 2).unwrap(),
            ArtifactReplicationState::Ready
        );
        assert_eq!(
            store
                .get_artifact("tenant-a", artifact.artifact_id)
                .unwrap()
                .replication_state,
            ArtifactReplicationState::Ready
        );

        let mut corrupt = replica.clone();
        corrupt.status = ArtifactReplicaStatus::Corrupt;
        corrupt.verified_at = None;
        corrupt.updated_at = Utc::now();
        assert_eq!(
            store.upsert_artifact_replica(&corrupt, 2, 2).unwrap(),
            ArtifactReplicationState::Degraded
        );
        let mut wrong = replica;
        wrong.host_id = "host-c".into();
        wrong.failure_domain = "zone-c".into();
        wrong.storage_locator = "/private/replica-c".into();
        wrong.content_digest = "sha256:wrong".into();
        assert!(matches!(
            store.upsert_artifact_replica(&wrong, 2, 2),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(
            store.list_artifact_replicas("tenant-b", artifact.artifact_id),
            Ok(replicas) if replicas.is_empty()
        ));
    }

    #[test]
    fn hibernation_lookup_is_tenant_scoped_and_protects_its_snapshot() {
        let store = Store::open(":memory:").unwrap();
        let vm_id = Uuid::new_v4();
        let snapshot = SnapshotRecord {
            snapshot_id: Uuid::new_v4(),
            path: "/private/hibernate.ram".into(),
            overlay_path: None,
            host_id: "host-a".into(),
            owner_key: Some("tenant-a".into()),
            api_key_id: Some("key-a".into()),
            vm_id,
            ephemeral_owner_vm_id: None,
            memory_mib: Some(256),
            vcpus: Some(1),
            kernel_path: Some("kernel".into()),
            rootfs_path: None,
            rootfs_read_only: Some(false),
            cmdline: Some("console=ttyS0".into()),
            content_digest: Some("sha256:test".into()),
            size_bytes: Some(42),
            created_at: Utc::now(),
        };
        store.insert_snapshot(&snapshot).unwrap();
        let record = HibernationRecord {
            vm_id,
            owner_key: "tenant-a".into(),
            snapshot_path: snapshot.path.clone(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.upsert_hibernation(&record).unwrap();
        assert_eq!(store.get_hibernation("tenant-a", vm_id).unwrap(), record);
        assert!(matches!(
            store.get_hibernation("tenant-b", vm_id),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.delete_snapshot(&snapshot.path),
            Err(StoreError::Sqlite(_))
        ));
        store.delete_hibernation("tenant-a", vm_id).unwrap();
        store.delete_snapshot(&snapshot.path).unwrap();
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
            snapshot_id: Uuid::new_v4(),
            path: "/run/tarit/snap-1.snap".into(),
            overlay_path: Some("/run/tarit/snap-1.cow".into()),
            host_id: "node0".into(),
            owner_key: Some("tenant-a".into()),
            api_key_id: Some("key-1".into()),
            vm_id,
            ephemeral_owner_vm_id: None,
            memory_mib: Some(512),
            vcpus: Some(2),
            kernel_path: Some("/opt/tarit/vmlinux".into()),
            rootfs_path: Some("/opt/tarit/rootfs.ext4".into()),
            rootfs_read_only: Some(false),
            cmdline: Some("console=ttyS0".into()),
            content_digest: Some("sha256:test".into()),
            size_bytes: Some(42),
            created_at: Utc::now(),
        };
        store.insert_snapshot(&snap).unwrap();
        assert_eq!(
            store.get_snapshot_by_id(snap.snapshot_id).unwrap().unwrap(),
            snap
        );
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

        let child_id = Uuid::new_v4();
        let ephemeral = SnapshotRecord {
            snapshot_id: Uuid::new_v4(),
            path: "/run/tarit/fork-private.snap".into(),
            ephemeral_owner_vm_id: Some(child_id),
            ..snap
        };
        store.insert_snapshot(&ephemeral).unwrap();
        assert_eq!(
            store.list_ephemeral_snapshots_for_vm(child_id).unwrap(),
            vec![ephemeral]
        );
        assert!(store
            .list_ephemeral_snapshots_for_vm(Uuid::new_v4())
            .unwrap()
            .is_empty());

        store
            .bind_snapshot_ephemeral_owner(&replaced.path, child_id)
            .unwrap();
        assert_eq!(
            store
                .get_snapshot(&replaced.path)
                .unwrap()
                .unwrap()
                .ephemeral_owner_vm_id,
            Some(child_id)
        );
        assert!(matches!(
            store.bind_snapshot_ephemeral_owner(&replaced.path, Uuid::new_v4()),
            Err(StoreError::Conflict(_))
        ));
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
    fn fork_operation_is_source_bound_recoverable_and_incarnation_fenced() {
        let store = Store::open(":memory:").unwrap();
        let now = Utc::now();
        let operation = ForkOperationRecord {
            child_vm_id: Uuid::new_v4(),
            source_vm_id: Uuid::new_v4(),
            owner_key: "tenant-a".into(),
            source_host_id: "host-a".into(),
            target_host_id: "host-b".into(),
            status: ForkOperationStatus::Preparing,
            child_created_at: None,
            created_at: now,
            updated_at: now,
        };
        let expiry = now + chrono::Duration::minutes(1);

        assert_eq!(
            store.claim_fork_operation(&operation, 2, expiry).unwrap(),
            ForkOperationClaimOutcome::New
        );
        assert_eq!(
            store.claim_fork_operation(&operation, 2, expiry).unwrap(),
            ForkOperationClaimOutcome::InProgress
        );

        let wrong_source = ForkOperationRecord {
            source_vm_id: Uuid::new_v4(),
            ..operation.clone()
        };
        assert!(matches!(
            store.claim_fork_operation(&wrong_source, 2, expiry),
            Err(StoreError::Conflict(_))
        ));

        store
            .release_vm_quota(&operation.owner_key, operation.child_vm_id)
            .unwrap();
        assert_eq!(
            store.claim_fork_operation(&operation, 2, expiry).unwrap(),
            ForkOperationClaimOutcome::Resumed
        );

        let child_created_at = now + chrono::Duration::seconds(1);
        store
            .commit_fork_operation(
                operation.child_vm_id,
                operation.source_vm_id,
                &operation.owner_key,
                child_created_at,
                Utc::now(),
            )
            .unwrap();
        let committed = store
            .get_fork_operation(operation.child_vm_id)
            .unwrap()
            .unwrap();
        assert_eq!(committed.status, ForkOperationStatus::Committed);
        assert_eq!(committed.child_created_at, Some(child_created_at));
        assert!(matches!(
            store.commit_fork_operation(
                operation.child_vm_id,
                operation.source_vm_id,
                &operation.owner_key,
                Utc::now(),
                Utc::now(),
            ),
            Err(StoreError::Conflict(_))
        ));

        store
            .release_vm_quota(&operation.owner_key, operation.child_vm_id)
            .unwrap();
        assert_eq!(
            store
                .reserve_vm_quota(
                    &operation.owner_key,
                    operation.child_vm_id,
                    usize::MAX,
                    expiry,
                )
                .unwrap(),
            VmQuotaReservationOutcome::IdConflict,
            "a committed fork id must never be recycled into another VM incarnation"
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
            source_digest: Some(format!("sha256:{}", "1".repeat(64))),
            rootfs_digest: Some(format!("sha256:{}", "2".repeat(64))),
            agent_digest: Some(format!("sha256:{}", "3".repeat(64))),
            provenance_key_digest: None,
            provenance_verified_at: None,
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
    fn volume_crud_is_tenant_scoped_idempotent_and_revision_fenced() {
        let store = Store::open(":memory:").unwrap();
        let now = Utc::now();
        let volume = VolumeRecord {
            id: Uuid::new_v4(),
            owner_key: "tenant-a".into(),
            name: "workspace".into(),
            provider: "local_block".into(),
            storage_class: VolumeStorageClass::Block,
            size_bytes: 4 * 1024 * 1024,
            status: VolumeStatus::Creating,
            capabilities: VolumeCapabilities {
                read_only_many: true,
                read_write_once: true,
                read_write_many: false,
                snapshots: false,
                clones: false,
            },
            host_id: Some("host-a".into()),
            region: None,
            zone: None,
            generation: 1,
            revision: 1,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        assert_eq!(store.insert_volume(&volume).unwrap(), volume);
        assert_eq!(store.insert_volume(&volume).unwrap(), volume);
        assert!(matches!(
            store.insert_volume(&VolumeRecord {
                size_bytes: volume.size_bytes * 2,
                ..volume.clone()
            }),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(
            store.get_volume("tenant-b", volume.id),
            Err(StoreError::NotFound)
        ));
        assert_eq!(
            store.list_volumes("tenant-a").unwrap(),
            vec![volume.clone()]
        );

        let available = store
            .transition_volume(
                "tenant-a",
                volume.id,
                VolumeTransition {
                    expected_status: VolumeStatus::Creating,
                    expected_revision: 1,
                    status: VolumeStatus::Available,
                    last_error: None,
                    updated_at: now,
                },
            )
            .unwrap();
        assert_eq!(available.revision, 2);
        assert_eq!(available.status, VolumeStatus::Available);
        assert!(matches!(
            store.transition_volume(
                "tenant-a",
                volume.id,
                VolumeTransition {
                    expected_status: VolumeStatus::Available,
                    expected_revision: 1,
                    status: VolumeStatus::Deleting,
                    last_error: None,
                    updated_at: now,
                },
            ),
            Err(StoreError::Conflict(_))
        ));
        let deleting = store
            .transition_volume(
                "tenant-a",
                volume.id,
                VolumeTransition {
                    expected_status: VolumeStatus::Available,
                    expected_revision: 2,
                    status: VolumeStatus::Deleting,
                    last_error: None,
                    updated_at: now,
                },
            )
            .unwrap();
        assert!(matches!(
            store.delete_volume_metadata("tenant-a", volume.id, 2),
            Err(StoreError::Conflict(_))
        ));
        store
            .delete_volume_metadata("tenant-a", volume.id, deleting.revision)
            .unwrap();
        assert!(matches!(
            store.get_volume("tenant-a", volume.id),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn volume_attachments_are_atomic_tenant_scoped_and_single_writer_fenced() {
        let store = Store::open(":memory:").unwrap();
        let now = Utc::now();
        let volume = VolumeRecord {
            id: Uuid::new_v4(),
            owner_key: "tenant-a".into(),
            name: "data".into(),
            provider: "local_block".into(),
            storage_class: VolumeStorageClass::Block,
            size_bytes: 4 * 1024 * 1024,
            status: VolumeStatus::Available,
            capabilities: VolumeCapabilities {
                read_only_many: true,
                read_write_once: true,
                read_write_many: false,
                snapshots: false,
                clones: false,
            },
            host_id: Some("host-a".into()),
            region: None,
            zone: None,
            generation: 7,
            revision: 1,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        let first_vm = Uuid::new_v4();
        let second_vm = Uuid::new_v4();
        store.insert_vm(&test_vm(first_vm, "tenant-a")).unwrap();
        store.insert_vm(&test_vm(second_vm, "tenant-a")).unwrap();
        store.insert_volume(&volume).unwrap();

        let first = VmVolumeAttachmentRecord {
            vm_id: first_vm,
            volume_id: volume.id,
            device_index: 0,
            owner_key: "tenant-a".into(),
            mode: VolumeAttachmentMode::ReadWrite,
            volume_generation: 7,
            created_at: now,
        };
        store.bind_vm_volumes(std::slice::from_ref(&first)).unwrap();
        store.bind_vm_volumes(std::slice::from_ref(&first)).unwrap();
        assert_eq!(
            store
                .list_vm_volume_attachments("tenant-a", first_vm)
                .unwrap(),
            vec![first]
        );
        assert!(store
            .list_vm_volume_attachments("tenant-b", first_vm)
            .unwrap()
            .is_empty());

        let second = VmVolumeAttachmentRecord {
            vm_id: second_vm,
            volume_id: volume.id,
            device_index: 0,
            owner_key: "tenant-a".into(),
            mode: VolumeAttachmentMode::ReadOnly,
            volume_generation: 7,
            created_at: now,
        };
        assert!(matches!(
            store.bind_vm_volumes(&[second]),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(
            store.begin_volume_delete("tenant-a", volume.id, VolumeStatus::Available, 1, now),
            Err(StoreError::Conflict(_))
        ));

        store.unbind_vm_volumes("tenant-a", first_vm).unwrap();
        let deleting = store
            .begin_volume_delete("tenant-a", volume.id, VolumeStatus::Available, 1, now)
            .unwrap();
        assert_eq!(deleting.status, VolumeStatus::Deleting);
    }

    #[test]
    fn read_write_many_volume_allows_multiple_writers() {
        let store = Store::open(":memory:").unwrap();
        let now = Utc::now();
        let first_vm = Uuid::new_v4();
        let second_vm = Uuid::new_v4();
        store.insert_vm(&test_vm(first_vm, "tenant-a")).unwrap();
        store.insert_vm(&test_vm(second_vm, "tenant-a")).unwrap();
        let volume = VolumeRecord {
            id: Uuid::new_v4(),
            owner_key: "tenant-a".into(),
            name: "shared-data".into(),
            provider: "test_rwx".into(),
            storage_class: VolumeStorageClass::Filesystem,
            size_bytes: 4 * 1024 * 1024,
            status: VolumeStatus::Available,
            capabilities: VolumeCapabilities {
                read_only_many: true,
                read_write_once: false,
                read_write_many: true,
                snapshots: false,
                clones: false,
            },
            host_id: None,
            region: None,
            zone: None,
            generation: 3,
            revision: 1,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_volume(&volume).unwrap();
        for vm_id in [first_vm, second_vm] {
            store
                .bind_vm_volumes(&[VmVolumeAttachmentRecord {
                    vm_id,
                    volume_id: volume.id,
                    device_index: 0,
                    owner_key: "tenant-a".into(),
                    mode: VolumeAttachmentMode::ReadWrite,
                    volume_generation: 3,
                    created_at: now,
                }])
                .unwrap();
        }
        assert_eq!(
            store
                .volume_attachment_count("tenant-a", volume.id)
                .unwrap(),
            2
        );
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
            rootfs_read_only: true,
            cmdline: "console=ttyS0".into(),
            runtime_layout: Some(VmRuntimeLayout {
                overlay_path: Some("/run/tarit/overlays/vm.cow".into()),
                jail_path: Some("/srv/tarit/jails/tarit-vm".into()),
                artifact_paths: vec![
                    "/run/tarit/overlays/vm.cow".into(),
                    "/srv/tarit/jails/tarit-vm".into(),
                ],
            }),
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
        assert!(store.get_vm(vm.id).unwrap().rootfs_read_only);
        assert_eq!(
            store.get_vm(vm.id).unwrap().runtime_layout,
            vm.runtime_layout
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

    #[test]
    fn legacy_snapshot_null_rootfs_mode_is_backfilled_read_only() {
        let path = std::env::current_dir().unwrap().join(format!(
            "target/store-legacy-snapshot-{}.db",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let created_at = Utc::now();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE snapshots (
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
                   rootfs_read_only INTEGER,
                   cmdline TEXT,
                   created_at TEXT NOT NULL
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO snapshots (
                   path, host_id, vm_id, memory_mib, vcpus, kernel_path, rootfs_path,
                   rootfs_read_only, cmdline, created_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,NULL,?8,?9)",
                params![
                    "legacy.ram",
                    "host-a",
                    Uuid::new_v4().to_string(),
                    256u64,
                    1u8,
                    "kernel",
                    "rootfs",
                    "console=ttyS0",
                    created_at.to_rfc3339(),
                ],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .get_snapshot("legacy.ram")
                .unwrap()
                .unwrap()
                .rootfs_read_only,
            Some(true)
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
    }

    #[test]
    fn host_boot_session_round_trips_and_is_replaced_by_new_heartbeat() {
        let store = Store::open(":memory:").unwrap();
        let first_session = Uuid::new_v4();
        let mut host = HostRecord {
            host_id: "node-a".into(),
            boot_session_id: Some(first_session),
            peer_certificate_sha256: Some("certificate-a".into()),
            rpc_addr: Some("https://node-a.internal:8443".into()),
            sandbox_count: 1,
            free_vcpus: 7,
            free_memory_mib: 4096,
            healthy: true,
            last_heartbeat: Utc::now(),
        };
        store.upsert_host(&host).unwrap();
        assert_eq!(
            store.list_hosts().unwrap()[0].boot_session_id,
            Some(first_session)
        );
        assert_eq!(
            store.list_hosts().unwrap()[0]
                .peer_certificate_sha256
                .as_deref(),
            Some("certificate-a")
        );

        let replacement = Uuid::new_v4();
        host.boot_session_id = Some(replacement);
        host.peer_certificate_sha256 = Some("certificate-b".into());
        store.upsert_host(&host).unwrap();
        let hosts = store.list_hosts().unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].boot_session_id, Some(replacement));
        assert_eq!(
            hosts[0].peer_certificate_sha256.as_deref(),
            Some("certificate-b")
        );
    }

    #[test]
    fn egress_policy_is_tenant_scoped_revisioned_and_deletable() {
        let store = Store::open(":memory:").unwrap();
        let vm_id = Uuid::new_v4();
        let now = Utc::now();
        assert_eq!(store.get_egress_policy("tenant-a", vm_id).unwrap(), None);

        let first = store
            .update_egress_policy(
                "tenant-a",
                vm_id,
                1,
                &["1.1.1.1:443/tcp".into()],
                false,
                now,
            )
            .unwrap();
        assert_eq!(first.revision, 2);
        assert_eq!(first.allowlist, vec!["1.1.1.1:443/tcp"]);
        assert!(matches!(
            store.update_egress_policy("tenant-a", vm_id, 1, &[], false, Utc::now()),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(store.get_egress_policy("tenant-b", vm_id).unwrap(), None);
        assert!(matches!(
            store.update_egress_policy("tenant-b", vm_id, 1, &[], false, Utc::now()),
            Err(StoreError::NotFound)
        ));

        let second = store
            .update_egress_policy(
                "tenant-a",
                vm_id,
                first.revision,
                &["10.0.0.0/8".into()],
                true,
                Utc::now(),
            )
            .unwrap();
        assert_eq!(second.revision, 3);
        assert!(second.allow_existing);
        store.delete_egress_policy("tenant-a", vm_id).unwrap();
        assert_eq!(store.get_egress_policy("tenant-a", vm_id).unwrap(), None);
        let recovered = EgressPolicyRecord {
            vm_id,
            owner_key: "tenant-a".into(),
            revision: 7,
            allowlist: vec!["203.0.113.9/32:443/tcp".into()],
            allow_existing: false,
            created_at: now,
            updated_at: Utc::now(),
        };
        store.upsert_recovered_egress_policy(&recovered).unwrap();
        assert_eq!(
            store.get_egress_policy("tenant-a", vm_id).unwrap(),
            Some(recovered.clone())
        );
        assert!(matches!(
            store.upsert_recovered_egress_policy(&EgressPolicyRecord {
                owner_key: "tenant-b".into(),
                ..recovered
            }),
            Err(StoreError::Conflict(_))
        ));
    }
}
