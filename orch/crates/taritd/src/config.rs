use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use uuid::Uuid;

/// One warm-pool VM class: a (vcpus, memory) shape kept pre-booted and ready.
#[derive(Debug, Clone)]
pub struct WarmClass {
    pub vcpus: u8,
    pub memory_mib: u64,
    /// Emergency minimum; refill is urgent below this depth.
    pub hard_floor: usize,
    /// Refill starts only when depth falls below this watermark.
    pub low_watermark: usize,
    /// How many idle VMs of this class to keep ready.
    pub target: usize,
    /// Safety ceiling; refill never intentionally grows past this depth.
    pub high_watermark: usize,
    /// Restore replenishment from a golden pre-booted snapshot instead of
    /// cold-booting every warm VM.
    pub restore: bool,
    /// Rootfs for this class; falls back to the host default when unset.
    pub rootfs: Option<PathBuf>,
    /// Registered image reference (`name[:tag]`) to resolve before warm refill.
    pub image: Option<String>,
}

impl WarmClass {
    fn from_spec(spec: WarmClassSpec) -> Result<Self> {
        let (default_hard_floor, default_low_watermark, default_high_watermark) =
            derive_watermarks(spec.target);
        let class = Self {
            vcpus: spec.vcpus,
            memory_mib: spec.memory_mib,
            hard_floor: spec.hard_floor.unwrap_or(default_hard_floor),
            low_watermark: spec.low_watermark.unwrap_or(default_low_watermark),
            target: spec.target,
            high_watermark: spec.high_watermark.unwrap_or(default_high_watermark),
            restore: spec.restore,
            rootfs: spec.rootfs,
            image: spec.image,
        };
        class.validate_watermarks()?;
        class.validate_image_rootfs()?;
        Ok(class)
    }

    fn validate_watermarks(&self) -> Result<()> {
        if self.hard_floor <= self.low_watermark
            && self.low_watermark <= self.target
            && self.target <= self.high_watermark
        {
            return Ok(());
        }
        bail!(
            "warm-pool watermarks for {} vCPU/{} MiB must satisfy hard_floor <= low_watermark <= target <= high_watermark (got {} <= {} <= {} <= {})",
            self.vcpus,
            self.memory_mib,
            self.hard_floor,
            self.low_watermark,
            self.target,
            self.high_watermark
        );
    }

    fn validate_image_rootfs(&self) -> Result<()> {
        if self.image.is_some() && self.rootfs.is_some() {
            bail!(
                "warm-pool class for {} vCPU/{} MiB cannot set both image and rootfs",
                self.vcpus,
                self.memory_mib
            );
        }
        Ok(())
    }

    pub fn refill_needed(&self, depth: usize) -> usize {
        refill_needed_for_depth(
            depth,
            self.hard_floor,
            self.low_watermark,
            self.target,
            self.high_watermark,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefillCgroupConfig {
    /// Absolute cgroup v2 path that receives refill VMM children. Unset disables
    /// cgroup placement so non-cgroup hosts keep working.
    pub path: Option<PathBuf>,
    /// cgroup v2 cpu.weight value for refill children. Default cgroup weight is
    /// 100; the default here is intentionally low so live work wins contention.
    pub cpu_weight: u64,
}

impl Default for RefillCgroupConfig {
    fn default() -> Self {
        Self {
            path: None,
            cpu_weight: 10,
        }
    }
}

/// Warm-pool policy: keep a buffer of pre-booted VMs so create() can hand one
/// out instantly instead of paying cold-boot latency, replenishing in the
/// background and cold-starting when the take rate outpaces replenishment.
#[derive(Debug, Clone)]
pub struct WarmPoolConfig {
    pub enabled: bool,
    /// CPU overcommit ratio (e.g. 4.0 = 400%): informs the effective vCPU
    /// ceiling used for placement when not pinned by TARIT_MAX_VCPUS.
    pub cpu_overcommit: f64,
    /// Max warm VMs to spawn concurrently while replenishing.
    pub replenish_concurrency: usize,
    /// Optional low-priority cgroup for background refill VMM children.
    pub refill_cgroup: RefillCgroupConfig,
    pub classes: Vec<WarmClass>,
}

impl Default for WarmPoolConfig {
    fn default() -> Self {
        // Default mix: 100% one class of 1 vCPU / 256 MiB, 400% CPU overcommit.
        Self {
            enabled: false,
            cpu_overcommit: 4.0,
            replenish_concurrency: 4,
            refill_cgroup: RefillCgroupConfig::default(),
            classes: vec![WarmClassSpec::default_class()
                .finish()
                .expect("default warm-pool watermarks are valid")],
        }
    }
}

impl WarmPoolConfig {
    pub fn total_target(&self) -> usize {
        self.classes.iter().map(|c| c.target).sum()
    }
}

fn derive_watermarks(target: usize) -> (usize, usize, usize) {
    if target == 0 {
        return (0, 0, 0);
    }
    let buffer = (target / 4).max(1);
    let low_watermark = target.saturating_sub(buffer).max(1);
    let hard_floor = low_watermark.saturating_sub(buffer);
    let high_watermark = target.saturating_add(buffer);
    (hard_floor, low_watermark, high_watermark)
}

pub(crate) fn refill_needed_for_depth(
    depth: usize,
    hard_floor: usize,
    low_watermark: usize,
    target: usize,
    high_watermark: usize,
) -> usize {
    debug_assert!(hard_floor <= low_watermark);
    debug_assert!(low_watermark <= target);
    debug_assert!(target <= high_watermark);
    if depth < low_watermark {
        target.min(high_watermark).saturating_sub(depth)
    } else {
        0
    }
}

/// Fleet autoscaling policy. A single leader-elected node runs the control loop
/// and actuates scaling through a pluggable provider command (per-cloud logic
/// lives in that script/webhook), so taritd stays cloud-SDK-free.
#[derive(Debug, Clone)]
pub struct AutoscaleConfig {
    pub enabled: bool,
    pub min_nodes: usize,
    pub max_nodes: usize,
    /// Scale OUT when the cluster's aggregate free vCPUs drop below this.
    pub scale_out_free_vcpus: u64,
    /// Scale IN when aggregate free vCPUs stay above this for the cooldown.
    pub scale_in_free_vcpus: u64,
    /// Command that actuates scaling; receives a JSON decision as argv[1].
    /// `None` = log-only (noop). This is the cross-cloud seam (EC2 ASG / GCP
    /// MIG / Terraform live behind it).
    pub provider_cmd: Option<String>,
}

impl Default for AutoscaleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_nodes: 1,
            max_nodes: 10,
            scale_out_free_vcpus: 2,
            scale_in_free_vcpus: 64,
            provider_cmd: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiRole {
    Admin,
    User,
}

impl ApiRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::User => "user",
        }
    }
}

impl FromStr for ApiRole {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "admin" => Ok(Self::Admin),
            "user" => Ok(Self::User),
            _ => bail!("API key role must be 'admin' or 'user'"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcmeDnsProvider {
    Cloudflare,
    Route53,
}

impl FromStr for AcmeDnsProvider {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        match raw {
            "cloudflare" => Ok(Self::Cloudflare),
            "route53" => Ok(Self::Route53),
            _ => bail!("TARIT_ACME_DNS_PROVIDER must be 'cloudflare' or 'route53'"),
        }
    }
}

#[allow(dead_code)] // Consumed by the ACME runtime added in subsequent tasks.
#[derive(Clone)]
pub struct AcmeConfig {
    pub identifier: String,
    pub directory_url: String,
    pub contact: String,
    pub provider: AcmeDnsProvider,
    pub kek: [u8; 32],
}

#[allow(dead_code)]
impl AcmeConfig {
    pub fn identifier(&self) -> &str {
        &self.identifier
    }
}

impl fmt::Debug for AcmeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcmeConfig")
            .field("identifier", &self.identifier)
            .field("directory_url", &self.directory_url)
            .field("contact", &self.contact)
            .field("provider", &self.provider)
            .field("kek", &format_args!("[REDACTED; {} bytes]", self.kek.len()))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiIdentity {
    pub tenant: String,
    pub role: ApiRole,
    pub max_vms: Option<usize>,
    /// Stable, non-secret id of the API key (hex of its hash). Used to attribute
    /// usage stats and audit events to the key that acted.
    pub api_key_id: String,
}

impl ApiIdentity {
    pub fn is_admin(&self) -> bool {
        self.role == ApiRole::Admin
    }
}

#[derive(Clone)]
struct ApiKeyEntry {
    key_hash: [u8; 32],
    identity: ApiIdentity,
}

#[derive(Clone, Default)]
pub struct ApiKeyRegistry {
    entries: Vec<ApiKeyEntry>,
}

impl fmt::Debug for ApiKeyRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyRegistry")
            .field("keys", &self.entries.len())
            .finish()
    }
}

impl ApiKeyRegistry {
    pub fn from_plaintext_entries(
        entries: impl IntoIterator<Item = (String, String, ApiRole, usize)>,
    ) -> Result<Self> {
        let mut registry = Self::default();
        for (key, tenant, role, max_vms) in entries {
            registry.push_plaintext_key(&key, tenant, role, max_vms)?;
        }
        if registry.entries.is_empty() {
            bail!("at least one API key must be configured");
        }
        Ok(registry)
    }

    pub fn resolve(&self, provided_key: &str) -> Option<ApiIdentity> {
        let provided_hash = hash_api_key(provided_key);
        let mut found = None;
        for entry in &self.entries {
            if constant_time_eq(&provided_hash, &entry.key_hash) {
                found = Some(entry.identity.clone());
            }
        }
        found
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn push_plaintext_key(
        &mut self,
        key: &str,
        tenant: String,
        role: ApiRole,
        max_vms: usize,
    ) -> Result<()> {
        if key.is_empty() {
            bail!("API keys must not be empty");
        }
        validate_tenant_id(&tenant)?;
        let key_hash = hash_api_key(key);
        if self
            .entries
            .iter()
            .any(|entry| constant_time_eq(&key_hash, &entry.key_hash))
        {
            bail!("duplicate API key configured");
        }
        self.entries.push(ApiKeyEntry {
            key_hash,
            identity: ApiIdentity {
                tenant,
                role,
                max_vms: quota_from_config(max_vms),
                api_key_id: hex_key_id(&key_hash),
            },
        });
        Ok(())
    }
}

/// TOML shape for the optional on-disk config file (see `WarmPoolConfig`).
#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    warm_pool: Option<WarmPoolFile>,
    #[serde(default)]
    api_keys: HashMap<String, ApiKeyFile>,
}

#[derive(Debug, Deserialize)]
struct ApiKeyFile {
    tenant: String,
    role: ApiRole,
    #[serde(default)]
    max_vms: usize,
}

#[derive(Debug, Deserialize)]
struct WarmPoolFile {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    cpu_overcommit: Option<f64>,
    #[serde(default)]
    replenish_concurrency: Option<usize>,
    #[serde(default)]
    refill_cgroup: Option<String>,
    #[serde(default)]
    refill_cpu_weight: Option<u64>,
    #[serde(default)]
    class: Vec<WarmClassFile>,
}

#[derive(Debug, Deserialize)]
struct WarmClassFile {
    vcpus: u8,
    memory_mib: u64,
    target: usize,
    #[serde(default)]
    hard_floor: Option<usize>,
    #[serde(default)]
    low_watermark: Option<usize>,
    #[serde(default)]
    high_watermark: Option<usize>,
    #[serde(default)]
    restore: Option<bool>,
    #[serde(default)]
    rootfs: Option<String>,
    #[serde(default)]
    image: Option<String>,
}

#[derive(Debug, Clone)]
struct WarmClassSpec {
    vcpus: u8,
    memory_mib: u64,
    target: usize,
    hard_floor: Option<usize>,
    low_watermark: Option<usize>,
    high_watermark: Option<usize>,
    restore: bool,
    rootfs: Option<PathBuf>,
    image: Option<String>,
}

impl WarmClassSpec {
    fn default_class() -> Self {
        Self {
            vcpus: 1,
            memory_mib: 256,
            target: 8,
            hard_floor: None,
            low_watermark: None,
            high_watermark: None,
            restore: false,
            rootfs: None,
            image: None,
        }
    }

    fn from_file(file: &WarmClassFile) -> Self {
        Self {
            vcpus: file.vcpus,
            memory_mib: file.memory_mib,
            target: file.target,
            hard_floor: file.hard_floor,
            low_watermark: file.low_watermark,
            high_watermark: file.high_watermark,
            restore: file.restore.unwrap_or(false),
            rootfs: file.rootfs.as_ref().map(|s| expand_path(s)),
            image: file.image.clone(),
        }
    }

    fn finish(self) -> Result<WarmClass> {
        WarmClass::from_spec(self)
    }
}

#[derive(Debug, Clone)]
struct WarmPoolDraft {
    enabled: bool,
    cpu_overcommit: f64,
    replenish_concurrency: usize,
    refill_cgroup: RefillCgroupConfig,
    classes: Vec<WarmClassSpec>,
}

impl Default for WarmPoolDraft {
    fn default() -> Self {
        Self {
            enabled: false,
            cpu_overcommit: 4.0,
            replenish_concurrency: 4,
            refill_cgroup: RefillCgroupConfig::default(),
            classes: vec![WarmClassSpec::default_class()],
        }
    }
}

impl WarmPoolDraft {
    fn finish(self) -> Result<WarmPoolConfig> {
        let classes = self
            .classes
            .into_iter()
            .map(WarmClassSpec::finish)
            .collect::<Result<Vec<_>>>()?;
        Ok(WarmPoolConfig {
            enabled: self.enabled,
            cpu_overcommit: self.cpu_overcommit,
            replenish_concurrency: self.replenish_concurrency,
            refill_cgroup: self.refill_cgroup,
            classes,
        })
    }
}

#[derive(Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub api_keys: ApiKeyRegistry,
    pub host_id: String,
    pub vmm_bin: PathBuf,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub socket_dir: PathBuf,
    pub db_path: PathBuf,
    /// Persistent per-VM network slot state. Env TARIT_NET_STATE; defaults next
    /// to TARIT_DB so restarts recover tap/IP ownership before allocating.
    pub net_state_path: PathBuf,
    pub images_dir: PathBuf,
    /// Max concurrent sandboxes on this host (placement guard).
    pub max_vms: usize,
    pub max_vcpus: u64,
    pub max_memory_mib: u64,
    pub peer_secret: String,
    /// Optional Postgres URL for global fleet sync (`tokio-postgres`, MIT/Apache-2.0).
    pub database_url: Option<String>,
    /// Address advertised to peers for HTTP RPC (e.g. http://10.0.0.1:8080).
    pub rpc_addr: String,
    /// Provision per-VM host networking (tap + /30 + NAT). Requires
    /// CAP_NET_ADMIN (run taritd as root). Off by default.
    pub enable_net: bool,
    /// Treat the rootfs as an immutable, shared read-only base (virtio-blk
    /// read-only + `ro` cmdline). Lets many VMs safely share one image without
    /// journal-recovery corruption; writes go to the agent's tmpfs mounts.
    /// Env TARIT_ROOTFS_READONLY. Off by default (rw, single-owner rootfs).
    pub rootfs_read_only: bool,
    /// Expose raw tenant names as labels on the unauthenticated `/metrics`
    /// endpoint. Off by default: tenant labels are replaced with a stable short
    /// hash so scraping cannot enumerate tenant identities. Only enable this
    /// when `/metrics` is bound to a trusted private network. Env
    /// TARIT_METRICS_EXPOSE_TENANT_LABELS.
    pub metrics_expose_tenant_labels: bool,
    /// Parent cgroup v2 path under which taritd places a per-VM cgroup for each
    /// `vmm serve` child (memory + PID limits), e.g. `/sys/fs/cgroup/tarit`.
    /// When unset, no per-VM cgroup is applied (host must be cgroup v2 and
    /// taritd must run as root for this to work). Env TARIT_VM_CGROUP_PARENT.
    pub vm_cgroup_parent: Option<String>,
    /// pids.max for each per-VM cgroup (fork-bomb ceiling). Env
    /// TARIT_VM_CGROUP_PIDS_MAX. Only used when `vm_cgroup_parent` is set.
    pub vm_cgroup_pids_max: u64,
    /// Warm-pool policy (loaded from the optional config file + env).
    pub warm_pool: WarmPoolConfig,
    /// How long a create() waits for a VM slot (warm backfill or a freed cold
    /// slot) before giving up, so bursts past capacity degrade in latency
    /// instead of erroring. Env TARIT_ADMISSION_TIMEOUT_MS.
    pub admission_timeout_ms: u64,
    /// Reap local `vmm serve` children on SIGTERM/SIGINT shutdown. Env
    /// TARIT_REAP_ON_SHUTDOWN; default true.
    pub reap_on_shutdown: bool,
    /// Topology labels advertised to the fleet for locality-aware placement and
    /// per-region autoscaling. Env TARIT_REGION / TARIT_ZONE / TARIT_CLOUD.
    pub region: String,
    pub zone: String,
    pub cloud: String,
    /// Fleet autoscaling policy (leader-elected; scales cloud instances).
    pub autoscale: AutoscaleConfig,
    /// SSH gateway listener. Disabled by default; enable with
    /// TARIT_SSH_GATEWAY=1.
    pub ssh_gateway_enabled: bool,
    pub ssh_gateway_addr: SocketAddr,
    pub ssh_gateway_host_key_path: PathBuf,
    /// Optional TCP listener for shared VM ports. When present, the normalized
    /// domain and a 32-byte token key are required.
    pub share_listen: Option<SocketAddr>,
    pub share_domain: Option<String>,
    pub share_token_key: Option<[u8; 32]>,
    pub share_token_ttl_secs: u64,
    pub share_connect_timeout_ms: u64,
    pub share_idle_timeout_secs: u64,
    pub acme_enabled: bool,
    pub acme_directory_url: String,
    pub acme_contact_email: Option<String>,
    pub acme_dns_provider: Option<AcmeDnsProvider>,
    pub acme_cloudflare_api_token: Option<String>,
    pub acme_cloudflare_zone_id: Option<String>,
    pub acme_route53_zone_id: Option<String>,
    pub acme_kek: Option<[u8; 32]>,
    pub share_tls_listen: Option<SocketAddr>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let listen = env::var("TARIT_LISTEN")
            .unwrap_or_else(|_| "0.0.0.0:8080".into())
            .parse::<SocketAddr>()
            .context("TARIT_LISTEN must be a valid socket address")?;

        let file_config = load_file_config()?;
        let api_keys = load_api_keys(file_config.as_ref())?;

        let host_id = env::var("TARIT_HOST_ID").unwrap_or_else(|_| default_hostname());

        let vmm_bin = expand_path(&env::var("TARIT_VMM_BIN").unwrap_or_else(|_| "vmm".into()));
        let kernel = expand_path(
            &env::var("TARIT_KERNEL").unwrap_or_else(|_| "/tmp/vmlinux.microvm".into()),
        );
        let rootfs = expand_path(
            &env::var("TARIT_ROOTFS").unwrap_or_else(|_| "/tmp/debian-rootfs.ext4".into()),
        );
        let socket_dir = expand_path(
            &env::var("TARIT_SOCKET_DIR").unwrap_or_else(|_| "~/.taritd/sockets".into()),
        );
        let db_path =
            expand_path(&env::var("TARIT_DB").unwrap_or_else(|_| "~/.taritd/fleet.db".into()));
        let net_state_path = env::var("TARIT_NET_STATE")
            .map(|s| expand_path(&s))
            .unwrap_or_else(|_| default_net_state_path(&db_path));
        let images_dir = expand_path(
            &env::var("TARIT_IMAGES_DIR").unwrap_or_else(|_| "~/.taritd/images".into()),
        );

        let max_vms = env::var("TARIT_MAX_VMS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(32);
        let max_vcpus_env = env::var("TARIT_MAX_VCPUS")
            .ok()
            .and_then(|s| s.parse().ok());
        let max_memory_mib = env::var("TARIT_MAX_MEMORY_MIB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(65_536);
        let database_url = env::var("TARIT_DATABASE_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let peer_secret =
            load_peer_secret_for_mode(env::var("TARIT_PEER_SECRET").ok(), database_url.is_some())?;
        let rpc_addr = env::var("TARIT_RPC_ADDR")
            .unwrap_or_else(|_| format!("http://{}:{}", listen.ip(), listen.port()));

        let enable_net = env_bool("TARIT_ENABLE_NET", false);

        let rootfs_read_only = env_bool("TARIT_ROOTFS_READONLY", false);

        let metrics_expose_tenant_labels = env_bool("TARIT_METRICS_EXPOSE_TENANT_LABELS", false);

        let vm_cgroup_parent = env::var("TARIT_VM_CGROUP_PARENT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let vm_cgroup_pids_max = env::var("TARIT_VM_CGROUP_PIDS_MAX")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1024);

        let warm_pool = load_warm_pool(file_config.as_ref())?;

        let admission_timeout_ms = env::var("TARIT_ADMISSION_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60_000);
        let reap_on_shutdown = env_bool("TARIT_REAP_ON_SHUTDOWN", true);

        // CPU overcommit: when the warm pool is on and TARIT_MAX_VCPUS is not
        // pinned, derive the vCPU ceiling from physical cores * overcommit so
        // "400% overcommit" actually raises how many VMs we will place.
        let max_vcpus = max_vcpus_env.unwrap_or_else(|| {
            if warm_pool.enabled {
                let cores = std::thread::available_parallelism()
                    .map(|n| n.get() as f64)
                    .unwrap_or(1.0);
                (cores * warm_pool.cpu_overcommit).ceil() as u64
            } else {
                64
            }
        });

        let region = env::var("TARIT_REGION").unwrap_or_else(|_| "local".into());
        let zone = env::var("TARIT_ZONE").unwrap_or_else(|_| region.clone());
        let cloud = env::var("TARIT_CLOUD").unwrap_or_else(|_| "onprem".into());

        let env_u64 = |k: &str, d: u64| env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d);
        let env_usize =
            |k: &str, d: usize| env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d);
        let autoscale = AutoscaleConfig {
            enabled: env_bool("TARIT_AUTOSCALE", false),
            min_nodes: env_usize("TARIT_AUTOSCALE_MIN", 1),
            max_nodes: env_usize("TARIT_AUTOSCALE_MAX", 10),
            scale_out_free_vcpus: env_u64("TARIT_AUTOSCALE_OUT_FREE_VCPUS", 2),
            scale_in_free_vcpus: env_u64("TARIT_AUTOSCALE_IN_FREE_VCPUS", 64),
            provider_cmd: env::var("TARIT_AUTOSCALE_PROVIDER_CMD")
                .ok()
                .filter(|s| !s.is_empty()),
        };

        let ssh_gateway_enabled = env_bool("TARIT_SSH_GATEWAY", false);
        let ssh_gateway_addr = env::var("TARIT_SSH_GATEWAY_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:2222".into())
            .parse::<SocketAddr>()
            .context("TARIT_SSH_GATEWAY_ADDR must be a valid socket address")?;
        let ssh_gateway_host_key_path = expand_path(
            &env::var("TARIT_SSH_GATEWAY_HOST_KEY")
                .unwrap_or_else(|_| "~/.taritd/ssh_host_ed25519".into()),
        );
        let share_listen_raw = env::var("TARIT_SHARE_LISTEN").ok();
        let share_domain_raw = env::var("TARIT_SHARE_DOMAIN").ok();
        let share_token_key_raw = env::var("TARIT_SHARE_TOKEN_KEY").ok();
        let share_token_ttl_secs_raw = env::var("TARIT_SHARE_TOKEN_TTL_SECS").ok();
        let share_connect_timeout_ms_raw = env::var("TARIT_SHARE_CONNECT_TIMEOUT_MS").ok();
        let share_idle_timeout_secs_raw = env::var("TARIT_SHARE_IDLE_TIMEOUT_SECS").ok();
        let (
            share_listen,
            share_domain,
            share_token_key,
            share_token_ttl_secs,
            share_connect_timeout_ms,
            share_idle_timeout_secs,
        ) = parse_share_config(
            share_listen_raw.as_deref(),
            share_domain_raw.as_deref(),
            share_token_key_raw.as_deref(),
            share_token_ttl_secs_raw.as_deref(),
            share_connect_timeout_ms_raw.as_deref(),
            share_idle_timeout_secs_raw.as_deref(),
        )?;
        let acme_enabled = env_bool("TARIT_ACME_ENABLED", false);
        let acme_config = parse_acme_config(
            acme_enabled,
            database_url.as_deref(),
            share_domain.as_deref(),
            env::var("TARIT_SHARE_TLS_LISTEN").ok().as_deref(),
            env::var("TARIT_ACME_DIRECTORY_URL").ok().as_deref(),
            env::var("TARIT_ACME_CONTACT_EMAIL").ok().as_deref(),
            env::var("TARIT_ACME_DNS_PROVIDER").ok().as_deref(),
            env::var("TARIT_ACME_CLOUDFLARE_API_TOKEN").ok().as_deref(),
            env::var("TARIT_ACME_CLOUDFLARE_ZONE_ID").ok().as_deref(),
            env::var("TARIT_ACME_ROUTE53_ZONE_ID").ok().as_deref(),
            env::var("TARIT_ACME_KEK").ok().as_deref(),
        )?;

        Ok(Self {
            listen,
            api_keys,
            host_id,
            vmm_bin,
            kernel,
            rootfs,
            socket_dir,
            db_path,
            net_state_path,
            images_dir,
            max_vms,
            max_vcpus,
            max_memory_mib,
            peer_secret,
            database_url,
            rpc_addr,
            enable_net,
            rootfs_read_only,
            metrics_expose_tenant_labels,
            vm_cgroup_parent,
            vm_cgroup_pids_max,
            warm_pool,
            admission_timeout_ms,
            reap_on_shutdown,
            region,
            zone,
            cloud,
            autoscale,
            ssh_gateway_enabled,
            ssh_gateway_addr,
            ssh_gateway_host_key_path,
            share_listen,
            share_domain,
            share_token_key,
            share_token_ttl_secs,
            share_connect_timeout_ms,
            share_idle_timeout_secs,
            acme_enabled: acme_config.enabled,
            acme_directory_url: acme_config.directory_url,
            acme_contact_email: acme_config.contact_email,
            acme_dns_provider: acme_config.dns_provider,
            acme_cloudflare_api_token: acme_config.cloudflare_api_token,
            acme_cloudflare_zone_id: acme_config.cloudflare_zone_id,
            acme_route53_zone_id: acme_config.route53_zone_id,
            acme_kek: acme_config.kek,
            share_tls_listen: acme_config.share_tls_listen,
        })
    }

    #[allow(dead_code)] // Consumed by the ACME runtime added in subsequent tasks.
    pub fn acme(&self) -> Option<AcmeConfig> {
        if !self.acme_enabled {
            return None;
        }

        Some(AcmeConfig {
            identifier: format!("*.{}", self.share_domain.as_ref()?),
            directory_url: self.acme_directory_url.clone(),
            contact: self.acme_contact_email.clone()?,
            provider: self.acme_dns_provider?,
            kek: self.acme_kek?,
        })
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field("api_keys", &self.api_keys)
            .field("host_id", &self.host_id)
            .field("vmm_bin", &self.vmm_bin)
            .field("kernel", &self.kernel)
            .field("rootfs", &self.rootfs)
            .field("socket_dir", &self.socket_dir)
            .field("db_path", &self.db_path)
            .field("net_state_path", &self.net_state_path)
            .field("images_dir", &self.images_dir)
            .field("max_vms", &self.max_vms)
            .field("max_vcpus", &self.max_vcpus)
            .field("max_memory_mib", &self.max_memory_mib)
            .field("peer_secret", &"[REDACTED]")
            .field(
                "database_url",
                &self.database_url.as_ref().map(|_| "[REDACTED]"),
            )
            .field("rpc_addr", &self.rpc_addr)
            .field("enable_net", &self.enable_net)
            .field("rootfs_read_only", &self.rootfs_read_only)
            .field(
                "metrics_expose_tenant_labels",
                &self.metrics_expose_tenant_labels,
            )
            .field("vm_cgroup_parent", &self.vm_cgroup_parent)
            .field("vm_cgroup_pids_max", &self.vm_cgroup_pids_max)
            .field("warm_pool", &self.warm_pool)
            .field("admission_timeout_ms", &self.admission_timeout_ms)
            .field("reap_on_shutdown", &self.reap_on_shutdown)
            .field("region", &self.region)
            .field("zone", &self.zone)
            .field("cloud", &self.cloud)
            .field("autoscale", &self.autoscale)
            .field("ssh_gateway_enabled", &self.ssh_gateway_enabled)
            .field("ssh_gateway_addr", &self.ssh_gateway_addr)
            .field("ssh_gateway_host_key_path", &self.ssh_gateway_host_key_path)
            .field("share_listen", &self.share_listen)
            .field("share_domain", &self.share_domain)
            .field(
                "share_token_key",
                &self.share_token_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field("share_token_ttl_secs", &self.share_token_ttl_secs)
            .field("share_connect_timeout_ms", &self.share_connect_timeout_ms)
            .field("share_idle_timeout_secs", &self.share_idle_timeout_secs)
            .field("acme_enabled", &self.acme_enabled)
            .field("acme_directory_url", &self.acme_directory_url)
            .field("acme_contact_email", &self.acme_contact_email)
            .field("acme_dns_provider", &self.acme_dns_provider)
            .field(
                "acme_cloudflare_api_token",
                &self
                    .acme_cloudflare_api_token
                    .as_ref()
                    .map(|_| "[REDACTED]"),
            )
            .field("acme_cloudflare_zone_id", &self.acme_cloudflare_zone_id)
            .field("acme_route53_zone_id", &self.acme_route53_zone_id)
            .field("acme_kek", &self.acme_kek.as_ref().map(|_| "[REDACTED]"))
            .field("share_tls_listen", &self.share_tls_listen)
            .finish()
    }
}

const DEFAULT_ACME_DIRECTORY_URL: &str = "https://acme-v02.api.letsencrypt.org/directory";

struct ParsedAcmeConfig {
    enabled: bool,
    directory_url: String,
    contact_email: Option<String>,
    dns_provider: Option<AcmeDnsProvider>,
    cloudflare_api_token: Option<String>,
    cloudflare_zone_id: Option<String>,
    route53_zone_id: Option<String>,
    kek: Option<[u8; 32]>,
    share_tls_listen: Option<SocketAddr>,
}

#[allow(clippy::too_many_arguments)]
fn parse_acme_config(
    enabled: bool,
    database_url: Option<&str>,
    share_domain: Option<&str>,
    share_tls_listen_raw: Option<&str>,
    directory_url_raw: Option<&str>,
    contact_email_raw: Option<&str>,
    dns_provider_raw: Option<&str>,
    cloudflare_api_token_raw: Option<&str>,
    cloudflare_zone_id_raw: Option<&str>,
    route53_zone_id_raw: Option<&str>,
    kek_raw: Option<&str>,
) -> Result<ParsedAcmeConfig> {
    let directory_url = directory_url_raw
        .unwrap_or(DEFAULT_ACME_DIRECTORY_URL)
        .to_string();

    if !enabled {
        return Ok(ParsedAcmeConfig {
            enabled,
            directory_url,
            contact_email: contact_email_raw.map(str::to_string),
            dns_provider: None,
            cloudflare_api_token: cloudflare_api_token_raw.map(str::to_string),
            cloudflare_zone_id: cloudflare_zone_id_raw.map(str::to_string),
            route53_zone_id: route53_zone_id_raw.map(str::to_string),
            kek: None,
            share_tls_listen: None,
        });
    }

    required_acme_value("TARIT_DATABASE_URL", database_url)?;
    required_acme_value("TARIT_SHARE_DOMAIN", share_domain)?;
    let share_tls_listen = required_acme_value("TARIT_SHARE_TLS_LISTEN", share_tls_listen_raw)?
        .parse::<SocketAddr>()
        .context("TARIT_SHARE_TLS_LISTEN must be a valid socket address")?;
    let directory_url =
        required_acme_value("TARIT_ACME_DIRECTORY_URL", Some(&directory_url))?.to_string();
    let contact_email =
        required_acme_value("TARIT_ACME_CONTACT_EMAIL", contact_email_raw)?.to_string();
    let dns_provider = required_acme_value("TARIT_ACME_DNS_PROVIDER", dns_provider_raw)?
        .parse::<AcmeDnsProvider>()?;
    let cloudflare_api_token = cloudflare_api_token_raw
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let cloudflare_zone_id = cloudflare_zone_id_raw
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let route53_zone_id = route53_zone_id_raw
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let kek = parse_acme_kek(required_acme_value("TARIT_ACME_KEK", kek_raw)?)?;

    if dns_provider == AcmeDnsProvider::Cloudflare && cloudflare_api_token.is_none() {
        bail!(
            "TARIT_ACME_CLOUDFLARE_API_TOKEN must be set when TARIT_ACME_DNS_PROVIDER is cloudflare"
        );
    }
    if dns_provider == AcmeDnsProvider::Cloudflare && cloudflare_zone_id.is_none() {
        bail!(
            "TARIT_ACME_CLOUDFLARE_ZONE_ID must be set when TARIT_ACME_DNS_PROVIDER is cloudflare"
        );
    }
    if dns_provider == AcmeDnsProvider::Route53 && route53_zone_id.is_none() {
        bail!("TARIT_ACME_ROUTE53_ZONE_ID must be set when TARIT_ACME_DNS_PROVIDER is route53");
    }

    Ok(ParsedAcmeConfig {
        enabled,
        directory_url,
        contact_email: Some(contact_email),
        dns_provider: Some(dns_provider),
        cloudflare_api_token,
        cloudflare_zone_id,
        route53_zone_id,
        kek: Some(kek),
        share_tls_listen: Some(share_tls_listen),
    })
}

fn required_acme_value<'a>(name: &str, value: Option<&'a str>) -> Result<&'a str> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("{name} must be set when TARIT_ACME_ENABLED is enabled"))
}

fn parse_acme_kek(raw: &str) -> Result<[u8; 32]> {
    if raw.len() != 64 {
        bail!("TARIT_ACME_KEK must decode to exactly 32 bytes");
    }

    let mut kek = [0u8; 32];
    for (index, pair) in raw.as_bytes().chunks_exact(2).enumerate() {
        let high = decode_hex_nibble(pair[0])
            .ok_or_else(|| anyhow::anyhow!("TARIT_ACME_KEK must be hexadecimal"))?;
        let low = decode_hex_nibble(pair[1])
            .ok_or_else(|| anyhow::anyhow!("TARIT_ACME_KEK must be hexadecimal"))?;
        kek[index] = (high << 4) | low;
    }
    Ok(kek)
}

fn decode_hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

type ShareConfig = (
    Option<SocketAddr>,
    Option<String>,
    Option<[u8; 32]>,
    u64,
    u64,
    u64,
);

fn parse_share_config(
    listen_raw: Option<&str>,
    domain_raw: Option<&str>,
    token_key_raw: Option<&str>,
    token_ttl_secs_raw: Option<&str>,
    connect_timeout_ms_raw: Option<&str>,
    idle_timeout_secs_raw: Option<&str>,
) -> Result<ShareConfig> {
    let share_listen = listen_raw
        .map(|raw| {
            raw.parse::<SocketAddr>()
                .context("TARIT_SHARE_LISTEN must be a valid socket address")
        })
        .transpose()?;
    let share_domain = domain_raw.map(normalize_share_domain).transpose()?;
    let share_token_key = token_key_raw.map(parse_share_token_key).transpose()?;

    if share_listen.is_some() && share_domain.is_none() {
        bail!("TARIT_SHARE_DOMAIN must be a normalized domain when TARIT_SHARE_LISTEN is enabled");
    }
    if share_listen.is_some() && share_token_key.is_none() {
        bail!("TARIT_SHARE_TOKEN_KEY must decode to exactly 32 bytes when TARIT_SHARE_LISTEN is enabled");
    }

    let share_token_ttl_secs =
        parse_positive_share_setting("TARIT_SHARE_TOKEN_TTL_SECS", token_ttl_secs_raw, 300)?;
    validate_share_token_ttl(share_token_ttl_secs)?;

    Ok((
        share_listen,
        share_domain,
        share_token_key,
        share_token_ttl_secs,
        parse_positive_share_setting(
            "TARIT_SHARE_CONNECT_TIMEOUT_MS",
            connect_timeout_ms_raw,
            10_000,
        )?,
        parse_positive_share_setting("TARIT_SHARE_IDLE_TIMEOUT_SECS", idle_timeout_secs_raw, 300)?,
    ))
}

fn normalize_share_domain(raw: &str) -> Result<String> {
    if raw.is_empty() || raw.trim() != raw {
        bail!("TARIT_SHARE_DOMAIN must be a normalized domain");
    }
    let normalized = raw.strip_suffix('.').unwrap_or(raw).to_ascii_lowercase();
    if normalized.is_empty()
        || normalized.len() > 253
        || normalized.parse::<std::net::IpAddr>().is_ok()
    {
        bail!("TARIT_SHARE_DOMAIN must be a normalized domain");
    }
    if normalized.split('.').any(|label| {
        label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    }) {
        bail!("TARIT_SHARE_DOMAIN must be a normalized domain");
    }
    Ok(normalized)
}

fn parse_share_token_key(raw: &str) -> Result<[u8; 32]> {
    let decoded = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| anyhow::anyhow!("TARIT_SHARE_TOKEN_KEY must be canonical base64url"))?;
    if URL_SAFE_NO_PAD.encode(&decoded) != raw {
        bail!("TARIT_SHARE_TOKEN_KEY must be canonical base64url");
    }
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("TARIT_SHARE_TOKEN_KEY must decode to exactly 32 bytes"))
}

fn parse_positive_share_setting(name: &str, raw: Option<&str>, default: u64) -> Result<u64> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    let value = raw
        .parse::<u64>()
        .with_context(|| format!("{name} must be a positive integer"))?;
    if value == 0 {
        bail!("{name} must be a positive integer");
    }
    Ok(value)
}

fn validate_share_token_ttl(ttl_secs: u64) -> Result<()> {
    let ttl = i64::try_from(ttl_secs)
        .context("TARIT_SHARE_TOKEN_TTL_SECS must fit token timestamp arithmetic")?;
    let expiry = Utc::now()
        .timestamp()
        .checked_add(ttl)
        .and_then(|timestamp| chrono::DateTime::from_timestamp(timestamp, 0));
    if expiry.is_none() {
        bail!("TARIT_SHARE_TOKEN_TTL_SECS must fit token timestamp arithmetic");
    }
    Ok(())
}

fn load_peer_secret_for_mode(raw: Option<String>, cluster_mode: bool) -> Result<String> {
    match raw {
        Some(secret) if secret.trim().len() >= 32 && secret != "dev-peer-secret" => Ok(secret),
        Some(_) => bail!(
            "TARIT_PEER_SECRET must be at least 32 characters and not the dev default"
        ),
        None if cluster_mode => bail!(
            "TARIT_PEER_SECRET must be set to a strong value when TARIT_DATABASE_URL is configured for a fleet"
        ),
        None => Ok(format!("local-{}", Uuid::new_v4().simple())),
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn share_listener_requires_domain_and_exactly_32_key_bytes() {
        let key = URL_SAFE_NO_PAD.encode([9u8; 32]);
        assert!(
            parse_share_config(Some("127.0.0.1:8443"), None, Some(&key), None, None, None,)
                .is_err()
        );
        assert!(parse_share_config(
            Some("127.0.0.1:8443"),
            Some("shares.example.test"),
            Some(&URL_SAFE_NO_PAD.encode([9u8; 31])),
            None,
            None,
            None,
        )
        .is_err());
    }

    #[test]
    fn acme_requires_domain_provider_kek_tls_listen_and_provider_zone() {
        let kek = hex::encode([7u8; 32]);
        assert!(parse_acme_config(
            true, None, None, None, None, None, None, None, None, None, None,
        )
        .is_err());
        let short_kek = hex::encode([7u8; 31]);
        assert!(parse_acme_config(
            true,
            Some("postgres://tarit.example/tarit"),
            Some("shares.example.com"),
            Some("0.0.0.0:8443"),
            Some("https://acme.example/dir"),
            Some("ops@example.com"),
            Some("cloudflare"),
            Some("tok"),
            None,
            None,
            Some(&short_kek),
        )
        .is_err());

        let cloudflare_without_zone = parse_acme_config(
            true,
            Some("postgres://tarit.example/tarit"),
            Some("shares.example.com"),
            Some("0.0.0.0:8443"),
            Some("https://acme.example/dir"),
            Some("ops@example.com"),
            Some("cloudflare"),
            Some("tok"),
            None,
            None,
            Some(&kek),
        );
        assert_eq!(
            cloudflare_without_zone.err().unwrap().to_string(),
            "TARIT_ACME_CLOUDFLARE_ZONE_ID must be set when TARIT_ACME_DNS_PROVIDER is cloudflare"
        );

        let route53_without_zone = parse_acme_config(
            true,
            Some("postgres://tarit.example/tarit"),
            Some("shares.example.com"),
            Some("0.0.0.0:8443"),
            Some("https://acme.example/dir"),
            Some("ops@example.com"),
            Some("route53"),
            None,
            None,
            None,
            Some(&kek),
        );
        assert_eq!(
            route53_without_zone.err().unwrap().to_string(),
            "TARIT_ACME_ROUTE53_ZONE_ID must be set when TARIT_ACME_DNS_PROVIDER is route53"
        );

        let acme = parse_acme_config(
            true,
            Some("postgres://tarit.example/tarit"),
            Some("shares.example.com"),
            Some("0.0.0.0:8443"),
            Some("https://acme.example/dir"),
            Some("ops@example.com"),
            Some("cloudflare"),
            Some("tok"),
            Some("zone-id"),
            None,
            Some(&kek),
        )
        .expect("valid ACME config");
        assert_eq!(acme.share_tls_listen, Some("0.0.0.0:8443".parse().unwrap()));

        let config = acme_test_config(true);
        assert_eq!(
            config.acme().expect("acme view present").identifier(),
            "*.shares.example.com"
        );
    }

    #[test]
    fn acme_disabled_leaves_plain_http_config_unchanged() {
        let config = acme_test_config(false);

        assert!(!config.acme_enabled);
        assert!(config.acme().is_none());
        assert!(config.share_tls_listen.is_none());
    }

    #[test]
    fn share_config_normalizes_domain_and_rejects_noncanonical_key() {
        let key = URL_SAFE_NO_PAD.encode([9u8; 32]);
        let config = parse_share_config(
            Some("127.0.0.1:8443"),
            Some("SHARES.Example.TEST."),
            Some(&key),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(config.1.as_deref(), Some("shares.example.test"));
        assert_eq!(config.2, Some([9u8; 32]));
        assert!(parse_share_config(
            Some("127.0.0.1:8443"),
            Some("shares.example.test"),
            Some(&format!("{key}=")),
            None,
            None,
            None,
        )
        .is_err());
    }

    #[test]
    fn share_token_ttl_must_fit_token_timestamp_arithmetic() {
        for ttl in [i64::MAX as u64, i64::MAX as u64 + 1] {
            assert!(
                parse_share_config(None, None, None, Some(&ttl.to_string()), None, None,).is_err()
            );
        }
    }

    #[test]
    fn peer_secret_requires_explicit_strong_value_for_cluster() {
        assert!(load_peer_secret_for_mode(None, true).is_err());
        assert!(load_peer_secret_for_mode(Some("dev-peer-secret".into()), true).is_err());
        assert!(load_peer_secret_for_mode(Some("short".into()), true).is_err());

        let strong = "0123456789abcdef0123456789abcdef".to_string();
        assert_eq!(
            load_peer_secret_for_mode(Some(strong.clone()), true).unwrap(),
            strong
        );
    }

    #[test]
    fn missing_single_node_peer_secret_is_not_the_public_dev_default() {
        let a = load_peer_secret_for_mode(None, false).unwrap();
        let b = load_peer_secret_for_mode(None, false).unwrap();

        assert_ne!(a, "dev-peer-secret");
        assert_ne!(a, b);
        assert!(a.len() >= 32);
        assert!(b.len() >= 32);
    }

    fn acme_test_config(acme_enabled: bool) -> Config {
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            api_keys: ApiKeyRegistry::from_plaintext_entries(vec![(
                "test-key".into(),
                "tenant-a".into(),
                ApiRole::Admin,
                0,
            )])
            .unwrap(),
            host_id: "test-host".into(),
            vmm_bin: PathBuf::from("target/taritd-config-test/vmm"),
            kernel: PathBuf::from("target/taritd-config-test/kernel"),
            rootfs: PathBuf::from("target/taritd-config-test/rootfs"),
            socket_dir: PathBuf::from("target/taritd-config-test/sockets"),
            db_path: PathBuf::from("target/taritd-config-test/fleet.db"),
            net_state_path: PathBuf::from("target/taritd-config-test/net-state.json"),
            images_dir: PathBuf::from("target/taritd-config-test/images"),
            max_vms: 4,
            max_vcpus: 4,
            max_memory_mib: 1024,
            peer_secret: "peer-secret".into(),
            database_url: acme_enabled.then(|| "postgres://tarit.example/tarit".into()),
            rpc_addr: "http://127.0.0.1:0".into(),
            enable_net: false,
            rootfs_read_only: false,
            metrics_expose_tenant_labels: false,
            vm_cgroup_parent: None,
            vm_cgroup_pids_max: 1024,
            warm_pool: WarmPoolConfig::default(),
            admission_timeout_ms: 1,
            reap_on_shutdown: true,
            region: "local".into(),
            zone: "local".into(),
            cloud: "onprem".into(),
            autoscale: AutoscaleConfig::default(),
            ssh_gateway_enabled: false,
            ssh_gateway_addr: "127.0.0.1:0".parse().unwrap(),
            ssh_gateway_host_key_path: PathBuf::from("target/taritd-config-test/ssh_host"),
            share_listen: Some("127.0.0.1:0".parse().unwrap()),
            share_domain: Some("shares.example.com".into()),
            share_token_key: Some([7; 32]),
            share_token_ttl_secs: 300,
            share_connect_timeout_ms: 1_000,
            share_idle_timeout_secs: 1,
            acme_enabled,
            acme_directory_url: "https://acme.example/dir".into(),
            acme_contact_email: acme_enabled.then(|| "ops@example.com".into()),
            acme_dns_provider: acme_enabled.then_some(AcmeDnsProvider::Cloudflare),
            acme_cloudflare_api_token: acme_enabled.then(|| "tok".into()),
            acme_cloudflare_zone_id: None,
            acme_route53_zone_id: None,
            acme_kek: acme_enabled.then_some([7; 32]),
            share_tls_listen: acme_enabled.then(|| "127.0.0.1:8443".parse().unwrap()),
        }
    }
}

fn load_file_config() -> Result<Option<FileConfig>> {
    let path = env::var("TARIT_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| expand_path("~/.taritd/config.toml"));
    if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read config file {}", path.display()))?;
        let file: FileConfig = toml::from_str(&text)
            .with_context(|| format!("parse config file {}", path.display()))?;
        Ok(Some(file))
    } else {
        Ok(None)
    }
}

fn load_api_keys(file_config: Option<&FileConfig>) -> Result<ApiKeyRegistry> {
    let mut entries = Vec::new();

    if let Some(file) = file_config {
        for (key, def) in &file.api_keys {
            entries.push((key.clone(), def.tenant.clone(), def.role, def.max_vms));
        }
    }

    match env::var("TARIT_API_KEYS") {
        Ok(raw) if raw.trim().is_empty() => bail!("TARIT_API_KEYS must not be empty when set"),
        Ok(raw) => entries.extend(parse_api_keys_env(&raw)?),
        Err(_) => {}
    }

    match env::var("TARIT_API_KEY") {
        Ok(key) if key.is_empty() => bail!("TARIT_API_KEY must not be empty"),
        Ok(key) => entries.push((key, "default".into(), ApiRole::Admin, 0)),
        Err(_) => {}
    }

    if entries.is_empty() {
        bail!("configure at least one API key with TARIT_API_KEY, TARIT_API_KEYS, or [api_keys] in TARIT_CONFIG");
    }

    ApiKeyRegistry::from_plaintext_entries(entries)
}

fn parse_api_keys_env(raw: &str) -> Result<Vec<(String, String, ApiRole, usize)>> {
    let mut entries = Vec::new();
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let parts: Vec<_> = item.split(':').map(str::trim).collect();
        if !(parts.len() == 3 || parts.len() == 4) {
            bail!("TARIT_API_KEYS entries must be key:tenant:role[:max_vms]");
        }
        let key = parts[0];
        let tenant = parts[1];
        if key.is_empty() {
            bail!("TARIT_API_KEYS entries must not contain empty keys");
        }
        if tenant.is_empty() {
            bail!("TARIT_API_KEYS entries must not contain empty tenants");
        }
        let role = parts[2].parse::<ApiRole>()?;
        let max_vms = if parts.len() == 4 {
            parts[3]
                .parse::<usize>()
                .context("TARIT_API_KEYS max_vms must be a non-negative integer")?
        } else {
            0
        };
        entries.push((key.to_string(), tenant.to_string(), role, max_vms));
    }
    if entries.is_empty() {
        bail!("TARIT_API_KEYS must include at least one entry");
    }
    Ok(entries)
}

/// Load warm-pool config from the optional TOML file (TARIT_CONFIG, else
/// ~/.taritd/config.toml), then apply env overrides. Missing file = defaults.
pub fn load_warm_pool_config() -> Result<WarmPoolConfig> {
    let file_config = load_file_config()?;
    load_warm_pool(file_config.as_ref())
}

fn load_warm_pool(file_config: Option<&FileConfig>) -> Result<WarmPoolConfig> {
    let mut draft = warm_pool_draft_from_file(file_config);
    apply_warm_pool_env(&mut draft);
    draft.finish()
}

fn warm_pool_draft_from_file(file_config: Option<&FileConfig>) -> WarmPoolDraft {
    let mut draft = WarmPoolDraft::default();

    if let Some(file) = file_config {
        if let Some(f) = &file.warm_pool {
            if let Some(e) = f.enabled {
                draft.enabled = e;
            }
            if let Some(o) = f.cpu_overcommit {
                draft.cpu_overcommit = o;
            }
            if let Some(c) = f.replenish_concurrency {
                draft.replenish_concurrency = c.max(1);
            }
            if let Some(path) = &f.refill_cgroup {
                draft.refill_cgroup.path = non_empty_path(path);
            }
            if let Some(weight) = f.refill_cpu_weight {
                draft.refill_cgroup.cpu_weight = normalize_cpu_weight(weight);
            }
            if !f.class.is_empty() {
                draft.classes = f.class.iter().map(WarmClassSpec::from_file).collect();
            }
        }
    }
    draft
}

fn apply_warm_pool_env(draft: &mut WarmPoolDraft) {
    // Env overrides (handy without a file): TARIT_WARM_POOL=1 enables it,
    // TARIT_WARM_POOL_TARGET sets the (single default class) target, and
    // TARIT_CPU_OVERCOMMIT sets the overcommit ratio.
    if let Ok(v) = env::var("TARIT_WARM_POOL") {
        draft.enabled = v == "1" || v.eq_ignore_ascii_case("true");
    }
    if let Some(o) = env::var("TARIT_CPU_OVERCOMMIT")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        draft.cpu_overcommit = o;
    }
    if let Some(t) = env::var("TARIT_WARM_POOL_TARGET")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
    {
        if let Some(first) = draft.classes.first_mut() {
            first.target = t;
        }
    }
    if let Some(v) = env_usize("TARIT_WARM_POOL_HARD_FLOOR") {
        if let Some(first) = draft.classes.first_mut() {
            first.hard_floor = Some(v);
        }
    }
    if let Some(v) = env_usize("TARIT_WARM_POOL_LOW_WATERMARK") {
        if let Some(first) = draft.classes.first_mut() {
            first.low_watermark = Some(v);
        }
    }
    if let Some(v) = env_usize("TARIT_WARM_POOL_HIGH_WATERMARK") {
        if let Some(first) = draft.classes.first_mut() {
            first.high_watermark = Some(v);
        }
    }
    if let Ok(v) = env::var("TARIT_WARM_POOL_RESTORE") {
        if let Some(first) = draft.classes.first_mut() {
            first.restore = v == "1" || v.eq_ignore_ascii_case("true");
        }
    }
    if let Ok(r) = env::var("TARIT_WARM_POOL_ROOTFS") {
        if !r.is_empty() {
            if let Some(first) = draft.classes.first_mut() {
                first.rootfs = Some(expand_path(&r));
            }
        }
    }
    if let Ok(image) = env::var("TARIT_WARM_POOL_IMAGE") {
        if !image.trim().is_empty() {
            if let Some(first) = draft.classes.first_mut() {
                first.image = Some(image.trim().to_string());
            }
        }
    }
    if let Ok(path) = env::var("TARIT_REFILL_CGROUP") {
        draft.refill_cgroup.path = non_empty_path(&path);
    }
    if let Some(weight) = env::var("TARIT_REFILL_CPU_WEIGHT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        draft.refill_cgroup.cpu_weight = normalize_cpu_weight(weight);
    }
}

fn hash_api_key(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}

/// A stable, non-secret identifier for an API key: the lowercase hex of its
/// SHA-256 hash. Safe to store and to expose to a user/billing layer.
fn hex_key_id(key_hash: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in key_hash {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn quota_from_config(max_vms: usize) -> Option<usize> {
    if max_vms == 0 {
        None
    } else {
        Some(max_vms)
    }
}

fn validate_tenant_id(tenant: &str) -> Result<()> {
    if tenant.is_empty() {
        bail!("tenant id must not be empty");
    }
    if !tenant
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        bail!("tenant id may only contain ASCII letters, digits, '.', '_', or '-'");
    }
    Ok(())
}

fn default_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

fn env_bool(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .as_deref()
        .and_then(parse_bool)
        .unwrap_or(default)
}

fn env_usize(key: &str) -> Option<usize> {
    env::var(key).ok().and_then(|s| s.parse().ok())
}

fn non_empty_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(expand_path(trimmed))
    }
}

fn normalize_cpu_weight(weight: u64) -> u64 {
    weight.clamp(1, 10_000)
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn default_net_state_path(db_path: &Path) -> PathBuf {
    let file_name = db_path
        .file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_else(|| "fleet.db".into());
    db_path.with_file_name(format!("{file_name}.net.json"))
}

pub fn expand_path(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_reap(raw: Option<&str>) -> bool {
        raw.and_then(parse_bool).unwrap_or(true)
    }

    #[test]
    fn reap_on_shutdown_defaults_true() {
        assert!(parse_reap(None));
        assert!(parse_reap(Some("unexpected")));
    }

    #[test]
    fn reap_on_shutdown_parses_boolean_values() {
        for value in ["1", "true", "TRUE", "yes", "on"] {
            assert!(parse_reap(Some(value)), "{value}");
        }
        for value in ["0", "false", "FALSE", "no", "off"] {
            assert!(!parse_reap(Some(value)), "{value}");
        }
    }

    #[test]
    fn parses_env_api_keys_with_roles_and_quotas() {
        let parsed = parse_api_keys_env("key1:tenant-a:user:20,key2:tenant_b:admin:0").unwrap();
        let registry = ApiKeyRegistry::from_plaintext_entries(parsed).unwrap();

        let user = registry.resolve("key1").unwrap();
        assert_eq!(user.tenant, "tenant-a");
        assert_eq!(user.role, ApiRole::User);
        assert_eq!(user.max_vms, Some(20));

        let admin = registry.resolve("key2").unwrap();
        assert_eq!(admin.tenant, "tenant_b");
        assert_eq!(admin.role, ApiRole::Admin);
        assert_eq!(admin.max_vms, None);
        assert!(registry.resolve("missing").is_none());
    }

    #[test]
    fn api_key_registry_hashes_and_does_not_debug_raw_keys() {
        let registry = ApiKeyRegistry::from_plaintext_entries(vec![(
            "super-secret-key".to_string(),
            "tenant-a".to_string(),
            ApiRole::User,
            1,
        )])
        .unwrap();

        assert_eq!(registry.len(), 1);
        assert!(registry.resolve("super-secret-key").is_some());
        assert!(registry.resolve("wrong").is_none());
        assert!(!format!("{registry:?}").contains("super-secret-key"));
        assert!(constant_time_eq(
            &hash_api_key("super-secret-key"),
            &hash_api_key("super-secret-key")
        ));
        assert!(!constant_time_eq(
            &hash_api_key("super-secret-key"),
            &hash_api_key("other-key")
        ));
    }

    #[test]
    fn warm_class_derives_hysteresis_watermarks_from_target() {
        let class = WarmClassSpec::default_class().finish().unwrap();

        assert_eq!(class.hard_floor, 4);
        assert_eq!(class.low_watermark, 6);
        assert_eq!(class.target, 8);
        assert_eq!(class.high_watermark, 10);
    }

    #[test]
    fn hysteresis_refill_decision_starts_below_low_and_fills_to_target() {
        let class = WarmClassSpec::default_class().finish().unwrap();

        assert_eq!(class.refill_needed(6), 0);
        assert_eq!(class.refill_needed(5), 3);
        assert_eq!(class.refill_needed(0), 8);
        assert_eq!(class.refill_needed(10), 0);
    }

    #[test]
    fn warm_pool_config_parses_explicit_watermarks_and_refill_cgroup() {
        let file: FileConfig = toml::from_str(
            r#"
            [warm_pool]
            enabled = true
            cpu_overcommit = 3.5
            replenish_concurrency = 0
            refill_cgroup = "/sys/fs/cgroup/taritd-refill"
            refill_cpu_weight = 10

            [[warm_pool.class]]
            vcpus = 2
            memory_mib = 512
            hard_floor = 3
            low_watermark = 5
            target = 8
            high_watermark = 11
            restore = true
            rootfs = "target/taritd-config-test/rootfs.ext4"
            "#,
        )
        .unwrap();

        let config = warm_pool_draft_from_file(Some(&file)).finish().unwrap();
        assert!(config.enabled);
        assert_eq!(config.cpu_overcommit, 3.5);
        assert_eq!(config.replenish_concurrency, 1);
        assert_eq!(
            config.refill_cgroup.path,
            Some(PathBuf::from("/sys/fs/cgroup/taritd-refill"))
        );
        assert_eq!(config.refill_cgroup.cpu_weight, 10);

        let class = &config.classes[0];
        assert_eq!(class.vcpus, 2);
        assert_eq!(class.memory_mib, 512);
        assert_eq!(class.hard_floor, 3);
        assert_eq!(class.low_watermark, 5);
        assert_eq!(class.target, 8);
        assert_eq!(class.high_watermark, 11);
        assert!(class.restore);
        assert_eq!(
            class.rootfs,
            Some(PathBuf::from("target/taritd-config-test/rootfs.ext4"))
        );
    }

    #[test]
    fn warm_pool_config_rejects_invalid_watermark_order() {
        let file: FileConfig = toml::from_str(
            r#"
            [warm_pool]
            enabled = true

            [[warm_pool.class]]
            vcpus = 1
            memory_mib = 256
            hard_floor = 7
            low_watermark = 6
            target = 8
            high_watermark = 10
            "#,
        )
        .unwrap();

        assert!(warm_pool_draft_from_file(Some(&file)).finish().is_err());
    }
}
