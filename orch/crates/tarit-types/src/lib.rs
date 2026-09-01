//! Shared types for the taritd host orchestrator.

use base64::{engine::general_purpose, Engine as _};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

/// Lifecycle state of a microVM on a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmStatus {
    Creating,
    Running,
    Paused,
    Suspended,
    Hibernated,
    Stopped,
    Error,
}

impl VmStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Suspended => "suspended",
            Self::Hibernated => "hibernated",
            Self::Stopped => "stopped",
            Self::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "creating" => Some(Self::Creating),
            "running" => Some(Self::Running),
            "paused" => Some(Self::Paused),
            "suspended" => Some(Self::Suspended),
            "hibernated" => Some(Self::Hibernated),
            "stopped" => Some(Self::Stopped),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// Definitive path used to bring a VM to its initial running state.
///
/// This is recorded at the actual lifecycle branch, rather than inferred by a
/// client, so performance tooling can prove which path it measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmStartupPath {
    Cold,
    Warm,
    SnapshotRestore,
}

impl VmStartupPath {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Warm => "warm",
            Self::SnapshotRestore => "snapshot_restore",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cold" => Some(Self::Cold),
            "warm" => Some(Self::Warm),
            "snapshot_restore" => Some(Self::SnapshotRestore),
            _ => None,
        }
    }
}

/// Persistent record of a VM managed by taritd.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmRuntimeLayout {
    pub overlay_path: Option<String>,
    pub jail_path: Option<String>,
    #[serde(default)]
    pub artifact_paths: Vec<String>,
}

/// Persistent record of a VM managed by taritd.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmRecord {
    pub id: Uuid,
    pub host_id: String,
    #[serde(default, skip_serializing)]
    pub owner_key: Option<String>,
    #[serde(default, skip_serializing)]
    pub api_key_id: Option<String>,
    pub status: VmStatus,
    /// Monotonic control-plane revision used to reject stale asynchronous
    /// persistence and fleet updates for this VM incarnation.
    #[serde(default = "default_vm_revision")]
    pub revision: u64,
    /// Actual boot path used for this VM. `None` is retained for records
    /// created by versions predating launch provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_path: Option<VmStartupPath>,
    pub memory_mib: u64,
    pub vcpus: u8,
    pub kernel_path: String,
    pub rootfs_path: Option<String>,
    /// Effective guest rootfs mount mode for this VM incarnation. This is
    /// durable so restart reconciliation and snapshots do not inherit a newer
    /// host-wide default.
    #[serde(default)]
    pub rootfs_read_only: bool,
    pub cmdline: String,
    /// Exact host paths allocated to this VM incarnation. Recovery must use
    /// this durable layout rather than deriving protection from newer config.
    #[serde(default, skip_serializing)]
    pub runtime_layout: Option<VmRuntimeLayout>,
    pub socket_path: Option<String>,
    pub pid: Option<u32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const fn default_vm_revision() -> u64 {
    1
}

/// Public control-plane view of a VM.
///
/// Process identifiers, Unix sockets, host filesystem paths, boot arguments,
/// tenant ownership metadata, and the physical host id are deliberately kept
/// out of this type. They are implementation details used by persistence and
/// authenticated peer RPC, not part of the tenant API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicVmRecord {
    pub id: Uuid,
    pub status: VmStatus,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_path: Option<VmStartupPath>,
    pub memory_mib: u64,
    pub vcpus: u8,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<&VmRecord> for PublicVmRecord {
    fn from(record: &VmRecord) -> Self {
        Self {
            id: record.id,
            status: record.status,
            revision: record.revision,
            startup_path: record.startup_path,
            memory_mib: record.memory_mib,
            vcpus: record.vcpus,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

impl From<VmRecord> for PublicVmRecord {
    fn from(record: VmRecord) -> Self {
        Self::from(&record)
    }
}

/// The immutable payload represented by an artifact record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    VmSnapshot,
    Memory,
    Disk,
    Kernel,
    Rootfs,
    Agent,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VmSnapshot => "vm_snapshot",
            Self::Memory => "memory",
            Self::Disk => "disk",
            Self::Kernel => "kernel",
            Self::Rootfs => "rootfs",
            Self::Agent => "agent",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "vm_snapshot" => Some(Self::VmSnapshot),
            "memory" => Some(Self::Memory),
            "disk" => Some(Self::Disk),
            "kernel" => Some(Self::Kernel),
            "rootfs" => Some(Self::Rootfs),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
}

/// Publication state of an immutable artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    Staging,
    Available,
    Deleting,
    Corrupt,
}

impl ArtifactStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Available => "available",
            Self::Deleting => "deleting",
            Self::Corrupt => "corrupt",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "staging" => Some(Self::Staging),
            "available" => Some(Self::Available),
            "deleting" => Some(Self::Deleting),
            "corrupt" => Some(Self::Corrupt),
            _ => None,
        }
    }
}

/// Whether an artifact satisfies the configured failure-domain replication policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactReplicationState {
    Pending,
    Ready,
    Degraded,
}

impl ArtifactReplicationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "ready" => Some(Self::Ready),
            "degraded" => Some(Self::Degraded),
            _ => None,
        }
    }
}

/// Integrity state of one physical copy of an immutable artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactReplicaStatus {
    Staging,
    Available,
    Corrupt,
    Deleting,
}

impl ArtifactReplicaStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Available => "available",
            Self::Corrupt => "corrupt",
            Self::Deleting => "deleting",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "staging" => Some(Self::Staging),
            "available" => Some(Self::Available),
            "corrupt" => Some(Self::Corrupt),
            "deleting" => Some(Self::Deleting),
            _ => None,
        }
    }
}

/// A private physical replica. Host identity, failure-domain placement, and
/// storage locators are control-plane details and must never cross the public
/// API boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReplicaRecord {
    pub artifact_id: Uuid,
    #[serde(skip_serializing)]
    pub owner_key: String,
    #[serde(skip_serializing)]
    pub host_id: String,
    #[serde(skip_serializing)]
    pub failure_domain: String,
    #[serde(skip_serializing)]
    pub storage_locator: String,
    pub status: ArtifactReplicaStatus,
    pub content_digest: String,
    pub size_bytes: u64,
    pub integrity_manifest_digest: String,
    pub verified_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Private durable index for an immutable artifact bundle stored outside any
/// worker filesystem. Provider namespace and manifest locator never cross the
/// public API boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactObjectReplicaRecord {
    pub artifact_id: Uuid,
    pub owner_key: String,
    pub provider: String,
    pub manifest_digest: String,
    pub manifest_size_bytes: u64,
    pub status: ArtifactReplicaStatus,
    pub verified_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Durable metadata for an immutable, tenant-owned artifact.
///
/// Host identity and the storage locator are persistence details and are never
/// serialized into the public API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub artifact_id: Uuid,
    #[serde(skip_serializing)]
    pub owner_key: String,
    #[serde(skip_serializing)]
    pub host_id: String,
    #[serde(skip_serializing)]
    pub storage_locator: String,
    pub kind: ArtifactKind,
    pub status: ArtifactStatus,
    pub content_digest: String,
    pub size_bytes: u64,
    pub immutable_image_digest: String,
    pub agent_digest: String,
    /// Digest of the canonical boot metadata needed to interpret this
    /// snapshot. This binds kernel, immutable image, agent, VM shape,
    /// command-line, and rootfs mode independently of node-local paths.
    pub boot_manifest_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_artifact_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_vm_id: Option<Uuid>,
    pub creation_revision: u64,
    pub integrity_manifest_digest: String,
    pub chunk_size_bytes: u64,
    pub chunk_count: u64,
    pub replication_state: ArtifactReplicationState,
    pub reference_count: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Path-independent boot inputs authenticated by an artifact record. A peer
/// may transport this structure, but the receiver must recompute its digest
/// and verify the exact local kernel and admitted image before guest execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactBootMetadata {
    pub version: u8,
    pub kernel_digest: String,
    pub immutable_image_digest: String,
    /// Digest of the exact generated filesystem bytes used as the immutable
    /// lower layer. The OCI manifest digest alone is insufficient because
    /// conversion metadata can make two ext4 outputs differ.
    pub rootfs_digest: String,
    pub agent_digest: String,
    pub memory_mib: u64,
    pub vcpus: u8,
    pub cmdline: String,
    pub rootfs_read_only: bool,
}

impl ArtifactBootMetadata {
    pub const VERSION: u8 = 2;

    pub fn digest(&self) -> Result<String, serde_json::Error> {
        use sha2::{Digest as _, Sha256};
        let encoded = serde_json::to_vec(self)?;
        Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
    }
}

/// Tenant-scoped named lineage pointer. Updating the head requires a CAS on
/// `revision`; artifact references are adjusted in the same durable transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRecord {
    pub branch_id: Uuid,
    #[serde(skip_serializing)]
    pub owner_key: String,
    pub name: String,
    pub head_artifact_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_vm_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_branch_id: Option<Uuid>,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBranchRequest {
    /// Optional caller-generated identity for idempotent replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<Uuid>,
    pub name: String,
    pub head_artifact_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_vm_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_branch_id: Option<Uuid>,
}

impl CreateBranchRequest {
    pub fn validate(&self) -> Result<(), OrchError> {
        let name = self.name.trim();
        if name.is_empty() || name.len() > 128 {
            return Err(OrchError::BadRequest(
                "branch name must contain 1..=128 bytes".into(),
            ));
        }
        if name != self.name || name.chars().any(char::is_control) {
            return Err(OrchError::BadRequest(
                "branch name must be trimmed and contain no control characters".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateBranchHeadRequest {
    pub expected_revision: u64,
    pub head_artifact_id: Uuid,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreBranchRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
}

/// Request to fork a running VM into a new isolated child. The child id may be
/// supplied for idempotent orchestration; host paths and placement are never
/// caller-controlled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkVmRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkExecutionPath {
    Local,
    CrossNode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkSnapshotTermination {
    Converged,
    Diverging,
    Timeout,
    MaxRounds,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkSnapshotMetrics {
    pub rounds: u32,
    pub pages_copied: u64,
    pub final_dirty_pages: u64,
    pub elapsed_us: u64,
    pub downtime_us: u64,
    pub termination: ForkSnapshotTermination,
}

/// Measurements for one newly executed fork. Idempotent replays return the
/// original child without inventing a second set of timings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkMetrics {
    pub path: ForkExecutionPath,
    pub source_resolution_us: u64,
    pub operation_claim_us: u64,
    pub snapshot_artifact_us: u64,
    pub child_ready_us: u64,
    pub operation_commit_us: u64,
    pub total_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_snapshot: Option<ForkSnapshotMetrics>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkVmResponse {
    pub source_vm_id: Uuid,
    pub vm: PublicVmRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<ForkMetrics>,
}

/// Durable phase of a source-bound fork operation. `Preparing` is written
/// before the child can be created, so a retry can distinguish interrupted
/// work from an unrelated VM-id collision. `Committed` is fenced to the exact
/// child incarnation by [`ForkOperationRecord::child_created_at`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkOperationStatus {
    Preparing,
    Committed,
}

impl ForkOperationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Committed => "committed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "preparing" => Some(Self::Preparing),
            "committed" => Some(Self::Committed),
            _ => None,
        }
    }
}

/// Idempotency and recovery record for one live-fork request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkOperationRecord {
    pub child_vm_id: Uuid,
    pub source_vm_id: Uuid,
    pub owner_key: String,
    pub source_host_id: String,
    pub target_host_id: String,
    /// Boot session currently entitled to prepare this child. A later target
    /// session may take over an interrupted operation; concurrent requests in
    /// the same session remain fenced by the live reservation.
    pub target_boot_session_id: Option<Uuid>,
    pub status: ForkOperationStatus,
    pub child_created_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Whether a shared VM port is public or requires a valid private-share token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareVisibility {
    Public,
    #[default]
    Private,
}

fn default_share_visibility() -> ShareVisibility {
    ShareVisibility::default()
}

/// Persistent tenant-owned VM port share record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareRecord {
    pub id: Uuid,
    pub slug: String,
    pub owner_key: String,
    pub vm_id: Uuid,
    pub guest_port: u16,
    pub visibility: ShareVisibility,
    pub token_version: u64,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Request body for creating a shared VM port.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateShareRequest {
    pub vm_id: Uuid,
    pub guest_port: u16,
    #[serde(default = "default_share_visibility")]
    pub visibility: ShareVisibility,
}

impl CreateShareRequest {
    pub fn validate(&self) -> Result<(), OrchError> {
        if self.guest_port == 0 {
            return Err(OrchError::BadRequest(
                "guest_port must be in 1..=65535".into(),
            ));
        }
        Ok(())
    }
}

/// Request body for updating a shared VM port.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateShareRequest {
    pub vm_id: Option<Uuid>,
    pub guest_port: Option<u16>,
    pub visibility: Option<ShareVisibility>,
}

impl UpdateShareRequest {
    pub fn validate(&self) -> Result<(), OrchError> {
        if self.guest_port == Some(0) {
            return Err(OrchError::BadRequest(
                "guest_port must be in 1..=65535".into(),
            ));
        }
        Ok(())
    }
}

/// A temporary bearer token for accessing a private share.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareTokenResponse {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

/// Persistent public SSH key record scoped to an API caller.
#[derive(Debug, Clone)]
pub struct SshKeyRecord {
    pub id: Uuid,
    pub owner_key: String,
    pub fingerprint: String,
    pub public_key: String,
    pub key_type: String,
    pub created_at: DateTime<Utc>,
    pub is_active: bool,
}

/// Provider-neutral persistent storage class. Object volumes intentionally do
/// not pretend to provide block or POSIX filesystem semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeStorageClass {
    Block,
    Filesystem,
    Object,
}

impl VolumeStorageClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Filesystem => "filesystem",
            Self::Object => "object",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "block" => Some(Self::Block),
            "filesystem" => Some(Self::Filesystem),
            "object" => Some(Self::Object),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeStatus {
    Creating,
    Available,
    Deleting,
    Error,
}

impl VolumeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Available => "available",
            Self::Deleting => "deleting",
            Self::Error => "error",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "creating" => Some(Self::Creating),
            "available" => Some(Self::Available),
            "deleting" => Some(Self::Deleting),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeCapabilities {
    pub read_only_many: bool,
    pub read_write_once: bool,
    pub read_write_many: bool,
    pub snapshots: bool,
    pub clones: bool,
}

/// Private durable volume record. Provider locators and credentials are never
/// fields on the public projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeRecord {
    pub id: Uuid,
    pub owner_key: String,
    pub name: String,
    pub provider: String,
    pub storage_class: VolumeStorageClass,
    pub size_bytes: u64,
    pub status: VolumeStatus,
    pub capabilities: VolumeCapabilities,
    pub host_id: Option<String>,
    pub region: Option<String>,
    pub zone: Option<String>,
    pub generation: u64,
    pub revision: u64,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicVolumeRecord {
    pub id: Uuid,
    pub name: String,
    pub provider: String,
    pub storage_class: VolumeStorageClass,
    pub size_bytes: u64,
    pub status: VolumeStatus,
    pub capabilities: VolumeCapabilities,
    pub region: Option<String>,
    pub zone: Option<String>,
    pub generation: u64,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<VolumeRecord> for PublicVolumeRecord {
    fn from(record: VolumeRecord) -> Self {
        Self {
            id: record.id,
            name: record.name,
            provider: record.provider,
            storage_class: record.storage_class,
            size_bytes: record.size_bytes,
            status: record.status,
            capabilities: record.capabilities,
            region: record.region,
            zone: record.zone,
            generation: record.generation,
            revision: record.revision,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateVolumeRequest {
    /// Optional client-selected identity for idempotent replay.
    pub id: Option<Uuid>,
    pub name: String,
    pub size_bytes: u64,
    #[serde(default = "default_local_block_provider")]
    pub provider: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeAttachmentMode {
    ReadOnly,
    ReadWrite,
}

impl VolumeAttachmentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::ReadWrite => "read_write",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read_only" => Some(Self::ReadOnly),
            "read_write" => Some(Self::ReadWrite),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmVolumeAttachmentRequest {
    pub volume_id: Uuid,
    pub mode: VolumeAttachmentMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmVolumeAttachmentRecord {
    pub vm_id: Uuid,
    pub volume_id: Uuid,
    pub device_index: u8,
    pub owner_key: String,
    pub mode: VolumeAttachmentMode,
    pub volume_generation: u64,
    pub created_at: DateTime<Utc>,
}

fn default_local_block_provider() -> String {
    "local_block".into()
}

/// Request body for `POST /v1/vms`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateVmRequest {
    pub id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_id: Option<String>,
    #[serde(default = "default_memory_mib")]
    pub memory_mib: u64,
    #[serde(default = "default_vcpus")]
    pub vcpus: u8,
    pub kernel_path: Option<String>,
    /// Registered image reference (`name[:tag]`) to resolve to a rootfs. If set,
    /// `rootfs_path` must be omitted. `image_ref` is accepted as a JSON alias.
    #[serde(default, alias = "image_ref")]
    pub image: Option<String>,
    pub rootfs_path: Option<String>,
    pub cmdline: Option<String>,
    #[serde(default)]
    pub volumes: Vec<VmVolumeAttachmentRequest>,
}

fn default_memory_mib() -> u64 {
    256
}

fn default_vcpus() -> u8 {
    1
}

/// Async command execution request (`POST /v1/execute_async`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub vm_id: Uuid,
    pub command: String,
    #[serde(default = "default_exec_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_exec_timeout_ms() -> u64 {
    30_000
}

/// Status of an async execution job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

impl ExecutionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Persistent execution job record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub id: Uuid,
    pub vm_id: Uuid,
    pub command: String,
    pub timeout_ms: u64,
    pub status: ExecutionStatus,
    pub exit_code: Option<i32>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What a usage event meters. `VmRuntime` records billable wall-clock seconds a
/// VM was alive in a window; `Exec` records one completed exec command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind {
    VmRuntime,
    Exec,
}

impl UsageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VmRuntime => "vm_runtime",
            Self::Exec => "exec",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "vm_runtime" => Some(Self::VmRuntime),
            "exec" => Some(Self::Exec),
            _ => None,
        }
    }
}

/// A raw usage stat emitted by a node and flushed to the primary store. This is
/// metering data only (which key, which VM, how many seconds/ms in a window).
/// A user/billing layer sits above the orchestrator and interprets these stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEvent {
    pub id: Uuid,
    /// Stable, non-secret id of the API key that owns the VM (hash of the key).
    pub api_key_id: String,
    /// Tenant the key maps to, carried for continuity.
    pub owner_key: String,
    pub host_id: String,
    pub vm_id: Uuid,
    pub kind: UsageKind,
    /// Billable wall-clock seconds for `VmRuntime` events.
    pub seconds: Option<f64>,
    /// Command duration for `Exec` events.
    pub duration_ms: Option<i64>,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// Aggregated usage stats per API key over a time range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    pub api_key_id: String,
    pub owner_key: String,
    pub vm_runtime_seconds: f64,
    pub exec_count: i64,
    pub exec_duration_ms: i64,
}

/// An audited action taken through the orchestrator, attributed to an API key.
/// `action` is a stable verb (see `audit_action` constants); `outcome` is
/// `attempt`, `ok`, `denied`, or `error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: Uuid,
    pub api_key_id: String,
    pub owner_key: String,
    pub host_id: String,
    pub vm_id: Option<Uuid>,
    pub action: String,
    pub outcome: String,
    /// Small human/JSON detail string (no secrets).
    pub detail: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Stable audit action verbs.
pub mod audit_action {
    pub const CREATE: &str = "create";
    pub const DELETE: &str = "delete";
    pub const PAUSE: &str = "pause";
    pub const SUSPEND: &str = "suspend";
    pub const RESUME: &str = "resume";
    pub const SNAPSHOT: &str = "snapshot";
    pub const FORK: &str = "fork";
    pub const HIBERNATE: &str = "hibernate";
    pub const CREATE_BRANCH: &str = "create_branch";
    pub const UPDATE_BRANCH: &str = "update_branch";
    pub const DELETE_BRANCH: &str = "delete_branch";
    pub const RESTORE: &str = "restore";
    pub const EXEC: &str = "exec";
    pub const ATTACH_PTY: &str = "attach_pty";
    pub const SSH_ATTEMPT: &str = "ssh_attempt";
    pub const UPDATE_EGRESS: &str = "update_egress";
    pub const CREATE_SHARE: &str = "create_share";
    pub const UPDATE_SHARE: &str = "update_share";
    pub const REVOKE_SHARE: &str = "revoke_share";
    pub const ISSUE_SHARE_TOKEN: &str = "issue_share_token";
    pub const CREATE_VOLUME: &str = "create_volume";
    pub const DELETE_VOLUME: &str = "delete_volume";
    pub const SET_BALLOON: &str = "set_balloon";
}

/// Stable audit outcome values.
pub mod audit_outcome {
    pub const ATTEMPT: &str = "attempt";
    pub const OK: &str = "ok";
    pub const DENIED: &str = "denied";
    pub const ERROR: &str = "error";
}

/// Snapshot request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRequest {
    #[serde(default)]
    pub diff: bool,
}

/// Public restore request. The opaque snapshot handle resolves to a private
/// host/path locator inside the control plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    pub snapshot_id: Uuid,
    #[serde(default)]
    pub id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotResponse {
    pub snapshot_id: Uuid,
}

/// Peer-only snapshot result. Public snapshot routes expose only the opaque
/// snapshot id; cluster fork coordination also carries source-side live
/// snapshot measurements without exposing a physical locator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerSnapshotResponse {
    pub snapshot_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_snapshot: Option<ForkSnapshotMetrics>,
}

/// OpenSSH SHA256 fingerprint for an RFC4253 public-key blob.
///
/// This returns the same string format as `ssh-keygen -lf` and
/// `ssh_key::PublicKey::fingerprint(HashAlg::Sha256)`: `SHA256:` plus
/// unpadded base64 of `sha256(key_blob)`.
pub fn openssh_sha256_fingerprint(key_blob: &[u8]) -> String {
    let digest = Sha256::digest(key_blob);
    format!("SHA256:{}", general_purpose::STANDARD_NO_PAD.encode(digest))
}

/// Live egress policy update (`PATCH /v1/egress/vm/:id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressUpdateRequest {
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub allow_existing: bool,
}

/// Durable desired egress policy. This resource is independent of a TAP or
/// nftables handle so hibernation and re-placement cannot erase policy intent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EgressPolicyRecord {
    pub vm_id: Uuid,
    // Public and peer responses deliberately omit tenant identity. A receiving
    // peer already has the signed request identity and must not infer authority
    // from this response object, so decode the omitted private field as empty.
    #[serde(default, skip_serializing)]
    pub owner_key: String,
    pub revision: u64,
    pub allowlist: Vec<String>,
    pub allow_existing: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PutEgressPolicyRequest {
    /// Compare-and-swap revision returned by the last GET/PUT.
    pub expected_revision: u64,
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub allow_existing: bool,
}

/// Standard health response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

impl Default for HealthResponse {
    fn default() -> Self {
        Self { status: "ok" }
    }
}

/// JSON error envelope returned by the HTTP API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

/// Orchestrator-level errors (HTTP mapping in taritd).
#[derive(Debug, Error)]
pub enum OrchError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("unprocessable: {0}")]
    Unprocessable(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("overloaded: {message}")]
    Overloaded {
        message: String,
        retry_after_secs: u64,
    },

    #[error("unauthorized")]
    Unauthorized,

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("unavailable: {0}")]
    Unavailable(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("vmm error: {0}")]
    Vmm(String),
}

impl OrchError {
    pub fn http_status(&self) -> u16 {
        match self {
            Self::NotFound(_) => 404,
            Self::BadRequest(_) => 400,
            Self::Unprocessable(_) => 422,
            Self::Conflict(_) => 409,
            Self::Overloaded { .. } => 429,
            Self::Unauthorized => 401,
            Self::Forbidden(_) => 403,
            Self::Unavailable(_) => 503,
            Self::Internal(_) | Self::Vmm(_) => 500,
        }
    }

    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::Overloaded {
                retry_after_secs, ..
            } => Some(*retry_after_secs),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openssh_sha256_fingerprint_matches_known_key_blob() {
        let blob = general_purpose::STANDARD
            .decode("AAAAC3NzaC1lZDI1NTE5AAAAIAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g")
            .unwrap();

        assert_eq!(
            openssh_sha256_fingerprint(&blob),
            "SHA256:mKqU+0K8OhKmA8bBQi9Rz0Q5l7/g160hIP+rJYSTNj4"
        );
    }

    #[test]
    fn share_visibility_round_trips() {
        let encoded = serde_json::to_string(&ShareVisibility::Private).unwrap();
        assert_eq!(encoded, "\"private\"");
        assert_eq!(
            serde_json::from_str::<ShareVisibility>(&encoded).unwrap(),
            ShareVisibility::Private
        );
    }

    #[test]
    fn artifact_enums_have_stable_storage_values() {
        for kind in [
            ArtifactKind::VmSnapshot,
            ArtifactKind::Memory,
            ArtifactKind::Disk,
            ArtifactKind::Kernel,
            ArtifactKind::Rootfs,
            ArtifactKind::Agent,
        ] {
            assert_eq!(ArtifactKind::parse(kind.as_str()), Some(kind));
        }
        for status in [
            ArtifactStatus::Staging,
            ArtifactStatus::Available,
            ArtifactStatus::Deleting,
            ArtifactStatus::Corrupt,
        ] {
            assert_eq!(ArtifactStatus::parse(status.as_str()), Some(status));
        }
        for state in [
            ArtifactReplicationState::Pending,
            ArtifactReplicationState::Ready,
            ArtifactReplicationState::Degraded,
        ] {
            assert_eq!(ArtifactReplicationState::parse(state.as_str()), Some(state));
        }
    }

    #[test]
    fn artifact_boot_digest_binds_every_path_independent_boot_input() {
        let metadata = ArtifactBootMetadata {
            version: ArtifactBootMetadata::VERSION,
            kernel_digest: format!("sha256:{}", "1".repeat(64)),
            immutable_image_digest: format!("sha256:{}", "2".repeat(64)),
            rootfs_digest: format!("sha256:{}", "3".repeat(64)),
            agent_digest: format!("sha256:{}", "4".repeat(64)),
            memory_mib: 256,
            vcpus: 1,
            cmdline: "console=ttyS0 root=/dev/vda ro".into(),
            rootfs_read_only: true,
        };
        let expected = metadata.digest().unwrap();
        for mut changed in [
            ArtifactBootMetadata {
                kernel_digest: format!("sha256:{}", "4".repeat(64)),
                ..metadata.clone()
            },
            ArtifactBootMetadata {
                immutable_image_digest: format!("sha256:{}", "4".repeat(64)),
                ..metadata.clone()
            },
            ArtifactBootMetadata {
                rootfs_digest: format!("sha256:{}", "5".repeat(64)),
                ..metadata.clone()
            },
            ArtifactBootMetadata {
                agent_digest: format!("sha256:{}", "6".repeat(64)),
                ..metadata.clone()
            },
            ArtifactBootMetadata {
                memory_mib: 512,
                ..metadata.clone()
            },
            ArtifactBootMetadata {
                vcpus: 2,
                ..metadata.clone()
            },
            ArtifactBootMetadata {
                cmdline: "console=ttyS0 init=/bin/sh".into(),
                ..metadata.clone()
            },
            ArtifactBootMetadata {
                rootfs_read_only: false,
                ..metadata.clone()
            },
        ] {
            assert_ne!(changed.digest().unwrap(), expected);
            changed.version = ArtifactBootMetadata::VERSION + 1;
            assert_ne!(changed.digest().unwrap(), expected);
        }
    }

    #[test]
    fn artifact_and_branch_json_hide_tenant_and_host_storage_identity() {
        let now = Utc::now();
        let artifact = ArtifactRecord {
            artifact_id: Uuid::new_v4(),
            owner_key: "tenant-secret".into(),
            host_id: "physical-node".into(),
            storage_locator: "/srv/private/artifact".into(),
            kind: ArtifactKind::VmSnapshot,
            status: ArtifactStatus::Available,
            content_digest: "sha256:content".into(),
            size_bytes: 42,
            immutable_image_digest: "sha256:image".into(),
            agent_digest: "sha256:agent".into(),
            boot_manifest_digest: "sha256:boot".into(),
            parent_artifact_id: None,
            source_vm_id: Some(Uuid::new_v4()),
            creation_revision: 1,
            integrity_manifest_digest: "sha256:manifest".into(),
            chunk_size_bytes: 4096,
            chunk_count: 1,
            replication_state: ArtifactReplicationState::Ready,
            reference_count: 0,
            created_at: now,
            updated_at: now,
        };
        let artifact_json = serde_json::to_value(&artifact).unwrap();
        for private in ["owner_key", "host_id", "storage_locator"] {
            assert!(
                artifact_json.get(private).is_none(),
                "artifact leaked {private}"
            );
        }

        let branch = BranchRecord {
            branch_id: Uuid::new_v4(),
            owner_key: artifact.owner_key.clone(),
            name: "main".into(),
            head_artifact_id: artifact.artifact_id,
            source_vm_id: artifact.source_vm_id,
            source_branch_id: None,
            revision: 1,
            created_at: now,
            updated_at: now,
        };
        assert!(serde_json::to_value(branch)
            .unwrap()
            .get("owner_key")
            .is_none());
    }

    #[test]
    fn egress_policy_peer_response_omits_private_owner_and_still_decodes() {
        let now = Utc::now();
        let policy = EgressPolicyRecord {
            vm_id: Uuid::new_v4(),
            owner_key: "tenant-secret".into(),
            revision: 2,
            allowlist: vec!["203.0.113.11/32:443/tcp".into()],
            allow_existing: true,
            created_at: now,
            updated_at: now,
        };
        let encoded = serde_json::to_vec(&policy).unwrap();
        let decoded: EgressPolicyRecord = serde_json::from_slice(&encoded).unwrap();
        assert!(decoded.owner_key.is_empty());
        assert_eq!(decoded.vm_id, policy.vm_id);
        assert_eq!(decoded.revision, policy.revision);
        assert_eq!(decoded.allowlist, policy.allowlist);
        assert_eq!(decoded.allow_existing, policy.allow_existing);
        assert!(!String::from_utf8(encoded)
            .unwrap()
            .contains("tenant-secret"));
    }

    #[test]
    fn snapshot_contract_uses_only_opaque_handles() {
        let snapshot_id = Uuid::new_v4();
        let response = serde_json::to_value(SnapshotResponse { snapshot_id }).unwrap();
        assert_eq!(response, serde_json::json!({ "snapshot_id": snapshot_id }));
        assert!(response.get("path").is_none());
        assert!(response.get("host_id").is_none());

        let request: RestoreRequest = serde_json::from_value(serde_json::json!({
            "snapshot_id": snapshot_id
        }))
        .unwrap();
        assert_eq!(request.snapshot_id, snapshot_id);
        assert!(serde_json::from_value::<RestoreRequest>(serde_json::json!({
            "snapshot_path": "/etc/shadow",
            "host_id": "attacker-selected"
        }))
        .is_err());
    }

    #[test]
    fn fork_metrics_are_structured_and_contain_no_host_locator() {
        let metrics = ForkMetrics {
            path: ForkExecutionPath::Local,
            source_resolution_us: 10,
            operation_claim_us: 20,
            snapshot_artifact_us: 30,
            child_ready_us: 40,
            operation_commit_us: 50,
            total_us: 150,
            live_snapshot: Some(ForkSnapshotMetrics {
                rounds: 3,
                pages_copied: 4096,
                final_dirty_pages: 4,
                elapsed_us: 25,
                downtime_us: 5,
                termination: ForkSnapshotTermination::Converged,
            }),
        };
        let value = serde_json::to_value(&metrics).unwrap();
        assert_eq!(value["path"], "local");
        assert_eq!(value["live_snapshot"]["termination"], "converged");
        for private in ["host_id", "snapshot_path", "overlay_path"] {
            assert!(
                value.get(private).is_none(),
                "fork metrics leaked {private}"
            );
        }
        assert_eq!(
            serde_json::from_value::<ForkMetrics>(value).unwrap(),
            metrics
        );
    }

    #[test]
    fn create_share_rejects_zero_port() {
        let req = CreateShareRequest {
            vm_id: Uuid::nil(),
            guest_port: 0,
            visibility: ShareVisibility::Private,
        };
        assert!(req.validate().is_err());
    }

    #[test]
    fn update_share_rejects_zero_port() {
        let req = UpdateShareRequest {
            guest_port: Some(0),
            ..Default::default()
        };

        assert!(matches!(
            req.validate(),
            Err(OrchError::BadRequest(message)) if message == "guest_port must be in 1..=65535"
        ));
    }

    #[test]
    fn create_share_defaults_to_private_visibility() {
        let req: CreateShareRequest = serde_json::from_str(
            r#"{"vm_id":"00000000-0000-0000-0000-000000000000","guest_port":8080}"#,
        )
        .unwrap();

        assert_eq!(req.visibility, ShareVisibility::Private);
    }

    #[test]
    fn share_audit_actions_are_stable() {
        assert_eq!(audit_action::CREATE_SHARE, "create_share");
        assert_eq!(audit_action::UPDATE_SHARE, "update_share");
        assert_eq!(audit_action::REVOKE_SHARE, "revoke_share");
        assert_eq!(audit_action::ISSUE_SHARE_TOKEN, "issue_share_token");
    }

    #[test]
    fn public_vm_record_contains_no_host_runtime_details() {
        let now = Utc::now();
        let internal = VmRecord {
            id: Uuid::new_v4(),
            host_id: "node-private".into(),
            owner_key: Some("tenant-a".into()),
            api_key_id: Some("key-id".into()),
            status: VmStatus::Running,
            revision: 2,
            startup_path: Some(VmStartupPath::Cold),
            memory_mib: 256,
            vcpus: 1,
            kernel_path: "/srv/private/vmlinux".into(),
            rootfs_path: Some("/srv/private/rootfs".into()),
            rootfs_read_only: true,
            cmdline: "console=ttyS0 secret=detail".into(),
            runtime_layout: None,
            socket_path: Some("/run/private/vm.sock".into()),
            pid: Some(42),
            created_at: now,
            updated_at: now,
        };
        let value = serde_json::to_value(PublicVmRecord::from(internal)).unwrap();
        for field in [
            "host_id",
            "owner_key",
            "api_key_id",
            "kernel_path",
            "rootfs_path",
            "cmdline",
            "socket_path",
            "pid",
        ] {
            assert!(value.get(field).is_none(), "public record leaked {field}");
        }
    }

    #[test]
    fn public_volume_record_contains_no_tenant_host_or_provider_locator() {
        let now = Utc::now();
        let public = PublicVolumeRecord::from(VolumeRecord {
            id: Uuid::new_v4(),
            owner_key: "tenant-secret".into(),
            name: "workspace".into(),
            provider: "local_block".into(),
            storage_class: VolumeStorageClass::Block,
            size_bytes: 1024 * 1024,
            status: VolumeStatus::Available,
            capabilities: VolumeCapabilities {
                read_only_many: true,
                read_write_once: true,
                read_write_many: false,
                snapshots: false,
                clones: false,
            },
            host_id: Some("private-host".into()),
            region: Some("region-a".into()),
            zone: Some("zone-a".into()),
            generation: 1,
            revision: 2,
            last_error: Some("/private/provider/path".into()),
            created_at: now,
            updated_at: now,
        });
        let value = serde_json::to_value(public).unwrap();
        for private in ["owner_key", "host_id", "last_error", "private_path"] {
            assert!(
                value.get(private).is_none(),
                "public volume leaked {private}"
            );
        }
        assert!(!value.to_string().contains("private-host"));
        assert!(!value.to_string().contains("/private/provider/path"));
    }
}
