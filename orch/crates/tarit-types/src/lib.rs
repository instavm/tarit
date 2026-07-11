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
    Stopped,
    Error,
}

impl VmStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "creating" => Some(Self::Creating),
            "running" => Some(Self::Running),
            "paused" => Some(Self::Paused),
            "stopped" => Some(Self::Stopped),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// Persistent record of a VM managed by taritd.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmRecord {
    pub id: Uuid,
    pub host_id: String,
    #[serde(default, skip_serializing)]
    pub owner_key: Option<String>,
    #[serde(default, skip_serializing)]
    pub api_key_id: Option<String>,
    pub status: VmStatus,
    pub memory_mib: u64,
    pub vcpus: u8,
    pub kernel_path: String,
    pub rootfs_path: Option<String>,
    pub cmdline: String,
    pub socket_path: Option<String>,
    pub pid: Option<u32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Whether a shared VM port is reachable publicly or only by its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareVisibility {
    Public,
    Private,
}

impl Default for ShareVisibility {
    fn default() -> Self {
        Self::Private
    }
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
pub struct UpdateShareRequest {
    pub vm_id: Option<Uuid>,
    pub guest_port: Option<u16>,
    pub visibility: Option<ShareVisibility>,
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

/// Request body for `POST /v1/vms`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
/// `ok`, `denied`, or `error`.
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
    pub const RESUME: &str = "resume";
    pub const SNAPSHOT: &str = "snapshot";
    pub const RESTORE: &str = "restore";
    pub const EXEC: &str = "exec";
    pub const ATTACH_PTY: &str = "attach_pty";
    pub const SSH_ATTEMPT: &str = "ssh_attempt";
    pub const UPDATE_EGRESS: &str = "update_egress";
    pub const CREATE_SHARE: &str = "create_share";
    pub const UPDATE_SHARE: &str = "update_share";
    pub const REVOKE_SHARE: &str = "revoke_share";
    pub const ISSUE_SHARE_TOKEN: &str = "issue_share_token";
}

/// Stable audit outcome values.
pub mod audit_outcome {
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

/// Restore request (`POST /v1/restore`). A snapshot file lives on the node that
/// took it; `host_id` (returned by the snapshot call) routes the restore to
/// that node so no cross-node file transfer is needed. `None` = restore on the
/// receiving node (single-host or a snapshot already local to it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreRequest {
    pub snapshot_path: String,
    #[serde(default)]
    pub host_id: Option<String>,
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_id: Option<String>,
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
            Self::Conflict(_) => 409,
            Self::Overloaded { .. } => 429,
            Self::Unauthorized => 401,
            Self::Forbidden(_) => 403,
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
    fn create_share_rejects_zero_port() {
        let req = CreateShareRequest {
            vm_id: Uuid::nil(),
            guest_port: 0,
            visibility: ShareVisibility::Private,
        };
        assert!(req.validate().is_err());
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
}
