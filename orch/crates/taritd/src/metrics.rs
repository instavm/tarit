use crate::api::AppState;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use tarit_types::VmStatus;

#[derive(Debug, Default)]
pub struct Metrics {
    vm_create_total: AtomicU64,
    vm_create_errors_total: AtomicU64,
    exec_total: AtomicU64,
}

impl Metrics {
    pub fn inc_vm_create_total(&self) {
        self.vm_create_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_vm_create_errors_total(&self) {
        self.vm_create_errors_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_exec_total(&self) {
        self.exec_total.fetch_add(1, Ordering::Relaxed);
    }

    fn vm_create_total(&self) -> u64 {
        self.vm_create_total.load(Ordering::Relaxed)
    }

    fn vm_create_errors_total(&self) -> u64 {
        self.vm_create_errors_total.load(Ordering::Relaxed)
    }

    fn exec_total(&self) -> u64 {
        self.exec_total.load(Ordering::Relaxed)
    }
}

pub fn render_metrics(state: &AppState) -> String {
    let mut out = String::new();

    metric_header(
        &mut out,
        "taritd_up",
        "gauge",
        "Whether this taritd process can render metrics.",
    );
    out.push_str("taritd_up 1\n");

    metric_header(
        &mut out,
        "taritd_vms",
        "gauge",
        "VM records in the local in-memory cache, counted by lifecycle status.",
    );
    for (status, count) in vm_status_counts(state) {
        let _ = writeln!(
            out,
            "taritd_vms{{status=\"{}\"}} {}",
            escape_label_value(status),
            count
        );
    }

    metric_header(
        &mut out,
        "taritd_tenant_vms",
        "gauge",
        if state.config.metrics_expose_tenant_labels {
            "Active VM records in the local in-memory cache, counted by tenant."
        } else {
            "Active VM records counted by tenant. The tenant label is a stable hash unless TARIT_METRICS_EXPOSE_TENANT_LABELS=1."
        },
    );
    let expose_tenant = state.config.metrics_expose_tenant_labels;
    for (tenant, count) in tenant_vm_counts(state) {
        let label = if expose_tenant {
            tenant
        } else {
            hashed_tenant_label(&tenant)
        };
        let _ = writeln!(
            out,
            "taritd_tenant_vms{{tenant=\"{}\"}} {}",
            escape_label_value(&label),
            count
        );
    }

    metric_header(
        &mut out,
        "taritd_warm_pool_depth",
        "gauge",
        "Idle warm-pool VMs currently parked per configured class.",
    );
    for (class, depth) in warm_pool_depths(state) {
        let _ = writeln!(
            out,
            "taritd_warm_pool_depth{{class=\"{}\"}} {}",
            escape_label_value(&class),
            depth
        );
    }
    metric_header(
        &mut out,
        "taritd_warm_pool_watermark",
        "gauge",
        "Effective warm-pool hysteresis watermarks per configured class.",
    );
    for (class, watermark, value) in warm_pool_watermarks(state) {
        let _ = writeln!(
            out,
            "taritd_warm_pool_watermark{{class=\"{}\",watermark=\"{}\"}} {}",
            escape_label_value(&class),
            watermark,
            value
        );
    }

    let capacity = state.scheduler.local_capacity(1, 256);
    metric_header(
        &mut out,
        "taritd_scheduler_free_vcpus",
        "gauge",
        "Scheduler-advertised local free vCPU capacity for a 1-vCPU placement.",
    );
    let _ = writeln!(out, "taritd_scheduler_free_vcpus {}", capacity.free_vcpus);
    metric_header(
        &mut out,
        "taritd_scheduler_free_memory_mib",
        "gauge",
        "Scheduler-advertised local free memory capacity for a 256-MiB placement.",
    );
    let _ = writeln!(
        out,
        "taritd_scheduler_free_memory_mib {}",
        capacity.free_memory_mib
    );

    metric_header(
        &mut out,
        "taritd_vm_create_total",
        "counter",
        "Successful public VM create requests handled by this taritd.",
    );
    let _ = writeln!(
        out,
        "taritd_vm_create_total {}",
        state.metrics.vm_create_total()
    );
    metric_header(
        &mut out,
        "taritd_vm_create_errors_total",
        "counter",
        "Failed public VM create requests handled by this taritd.",
    );
    let _ = writeln!(
        out,
        "taritd_vm_create_errors_total {}",
        state.metrics.vm_create_errors_total()
    );
    metric_header(
        &mut out,
        "taritd_exec_total",
        "counter",
        "Public exec requests accepted by this taritd.",
    );
    let _ = writeln!(out, "taritd_exec_total {}", state.metrics.exec_total());

    metric_header(
        &mut out,
        "taritd_vm_memory_rss_bytes",
        "gauge",
        "Host RSS bytes for currently-running local VMM processes.",
    );
    metric_header(
        &mut out,
        "taritd_vm_cpu_seconds_total",
        "counter",
        "Host CPU seconds consumed by currently-running local VMM processes.",
    );
    for (vm_id, pid) in running_local_vms(state) {
        if let Some(rss_bytes) = proc_rss_bytes(pid) {
            let _ = writeln!(
                out,
                "taritd_vm_memory_rss_bytes{{vm_id=\"{}\"}} {}",
                escape_label_value(&vm_id),
                rss_bytes
            );
        }
        if let Some(cpu_seconds) = proc_cpu_seconds_total(pid) {
            let _ = writeln!(
                out,
                "taritd_vm_cpu_seconds_total{{vm_id=\"{}\"}} {:.2}",
                escape_label_value(&vm_id),
                cpu_seconds
            );
        }
    }

    out
}

fn metric_header(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn vm_status_counts(state: &AppState) -> Vec<(&'static str, usize)> {
    let statuses = [
        VmStatus::Creating,
        VmStatus::Running,
        VmStatus::Paused,
        VmStatus::Stopped,
        VmStatus::Error,
    ];
    let mut counts = BTreeMap::new();
    for status in statuses {
        counts.insert(status.as_str(), 0usize);
    }
    if let Ok(cache) = state.vm_cache.read() {
        for vm in cache.values() {
            *counts.entry(vm.status.as_str()).or_default() += 1;
        }
    }
    counts.into_iter().collect()
}

/// Stable, non-reversible label for a tenant, so the unauthenticated `/metrics`
/// endpoint does not enumerate tenant names. 48 bits of SHA-256 is enough to
/// keep distinct tenants distinct on a dashboard without revealing identity.
fn hashed_tenant_label(tenant: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(tenant.as_bytes());
    let mut s = String::from("h:");
    for b in digest.iter().take(6) {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn tenant_vm_counts(state: &AppState) -> Vec<(String, usize)> {
    let mut counts = BTreeMap::new();
    if let Ok(cache) = state.vm_cache.read() {
        for vm in cache.values() {
            if matches!(
                vm.status,
                VmStatus::Creating | VmStatus::Running | VmStatus::Paused
            ) {
                let tenant = vm.owner_key.as_deref().unwrap_or("unknown");
                *counts.entry(tenant.to_string()).or_default() += 1;
            }
        }
    }
    counts.into_iter().collect()
}

fn warm_pool_depths(state: &AppState) -> Vec<(String, usize)> {
    let mut classes = BTreeSet::new();
    for class in &state.config.warm_pool.classes {
        classes.insert((class.vcpus, class.memory_mib));
    }
    classes
        .into_iter()
        .map(|(vcpus, memory_mib)| {
            (
                warm_pool_class_label(vcpus, memory_mib),
                state.supervisor.warm_count(vcpus, memory_mib),
            )
        })
        .collect()
}

fn warm_pool_class_label(vcpus: u8, memory_mib: u64) -> String {
    format!("{vcpus}vcpu_{memory_mib}mib")
}

fn warm_pool_watermarks(state: &AppState) -> Vec<(String, &'static str, usize)> {
    let mut out = Vec::new();
    for class in &state.config.warm_pool.classes {
        let label = warm_pool_class_label(class.vcpus, class.memory_mib);
        out.push((label.clone(), "hard_floor", class.hard_floor));
        out.push((label.clone(), "low_watermark", class.low_watermark));
        out.push((label.clone(), "target", class.target));
        out.push((label, "high_watermark", class.high_watermark));
    }
    out
}

fn running_local_vms(state: &AppState) -> Vec<(String, u32)> {
    state
        .vm_cache
        .read()
        .map(|cache| {
            cache
                .values()
                .filter(|vm| vm.host_id == state.config.host_id)
                .filter(|vm| vm.status == VmStatus::Running)
                .filter_map(|vm| vm.pid.map(|pid| (vm.id.to_string(), pid)))
                .collect()
        })
        .unwrap_or_default()
}

fn proc_rss_bytes(pid: u32) -> Option<u64> {
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let rss_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    Some(rss_pages.saturating_mul(page_size_bytes()))
}

fn proc_cpu_seconds_total(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat.get(close + 2..)?.split_whitespace().collect();
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some((utime + stime) as f64 / clock_ticks_per_second() as f64)
}

fn escape_label_value(value: &str) -> String {
    value
        .replace('\\', r"\\")
        .replace('\n', r"\n")
        .replace('"', r#"\""#)
}

#[cfg(target_os = "linux")]
fn page_size_bytes() -> u64 {
    sysconf_or(30, 4096)
}

#[cfg(not(target_os = "linux"))]
fn page_size_bytes() -> u64 {
    4096
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_second() -> u64 {
    sysconf_or(2, 100)
}

#[cfg(not(target_os = "linux"))]
fn clock_ticks_per_second() -> u64 {
    100
}

#[cfg(target_os = "linux")]
fn sysconf_or(name: i32, default: u64) -> u64 {
    extern "C" {
        fn sysconf(name: i32) -> isize;
    }
    let value = unsafe { sysconf(name) };
    u64::try_from(value)
        .ok()
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiKeyRegistry, ApiRole, AutoscaleConfig, Config, WarmPoolConfig};
    use crate::peer::PeerClient;
    use crate::pty::PtyRegistry;
    use crate::scheduler::Scheduler;
    use crate::supervisor::VmmSupervisor;
    use chrono::Utc;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, RwLock};
    use tarit_store::Store;
    use tarit_types::VmRecord;
    use uuid::Uuid;

    #[test]
    fn render_metrics_has_prometheus_shape() {
        let state = test_state();
        state.metrics.inc_vm_create_total();
        state.metrics.inc_vm_create_errors_total();
        state.metrics.inc_exec_total();

        let vm_id = Uuid::new_v4();
        let now = Utc::now();
        state.vm_cache.write().unwrap().insert(
            vm_id,
            VmRecord {
                id: vm_id,
                host_id: state.config.host_id.clone(),
                owner_key: Some("tenant-a".into()),
                api_key_id: None,
                status: VmStatus::Running,
                memory_mib: 256,
                vcpus: 1,
                kernel_path: "kernel".into(),
                rootfs_path: None,
                cmdline: "console=ttyS0".into(),
                socket_path: Some("socket".into()),
                pid: Some(std::process::id()),
                created_at: now,
                updated_at: now,
            },
        );

        let body = render_metrics(&state);

        for metric in [
            "taritd_up",
            "taritd_vms",
            "taritd_tenant_vms",
            "taritd_warm_pool_depth",
            "taritd_warm_pool_watermark",
            "taritd_scheduler_free_vcpus",
            "taritd_scheduler_free_memory_mib",
            "taritd_vm_create_total",
            "taritd_vm_create_errors_total",
            "taritd_exec_total",
            "taritd_vm_memory_rss_bytes",
            "taritd_vm_cpu_seconds_total",
        ] {
            assert!(body.contains(&format!("# HELP {metric} ")), "{metric} HELP");
            assert!(body.contains(&format!("# TYPE {metric} ")), "{metric} TYPE");
        }

        assert!(body.contains("taritd_up 1\n"));
        assert!(body.contains("taritd_vms{status=\"running\"} 1\n"));
        // Tenant labels are hashed by default so the unauthenticated /metrics
        // endpoint does not leak raw tenant names (R-012).
        assert!(
            !body.contains("tenant=\"tenant-a\""),
            "raw tenant name must not appear when labels are not exposed"
        );
        assert!(
            body.contains("taritd_tenant_vms{tenant=\"h:"),
            "tenant label should be a hash by default"
        );
        assert!(body.contains("taritd_warm_pool_depth{class=\"1vcpu_256mib\"} 0\n"));
        assert!(body.contains(
            "taritd_warm_pool_watermark{class=\"1vcpu_256mib\",watermark=\"target\"} 8\n"
        ));
        assert!(body.contains("taritd_vm_create_total 1\n"));
        assert!(body.contains("taritd_vm_create_errors_total 1\n"));
        assert!(body.contains("taritd_exec_total 1\n"));

        for line in body.lines().filter(|line| !line.starts_with('#')) {
            let (sample, value) = line.rsplit_once(' ').expect("sample has value");
            assert!(!sample.is_empty(), "sample name");
            value.parse::<f64>().expect("numeric sample value");
        }
    }

    #[test]
    fn hashed_tenant_label_is_stable_and_opaque() {
        let a = hashed_tenant_label("tenant-a");
        let b = hashed_tenant_label("tenant-a");
        let c = hashed_tenant_label("tenant-b");
        assert_eq!(a, b, "hash is stable for a tenant");
        assert_ne!(a, c, "different tenants map to different labels");
        assert!(a.starts_with("h:"));
        assert!(!a.contains("tenant-a"));
        assert_eq!(a.len(), 14, "h: plus 12 hex chars");
    }

    fn test_state() -> AppState {
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            api_keys: ApiKeyRegistry::from_plaintext_entries(vec![(
                "test-key".into(),
                "tenant-a".into(),
                ApiRole::Admin,
                0,
            )])
            .unwrap(),
            host_id: "test-host".into(),
            vmm_bin: PathBuf::from("target/taritd-metrics-test/vmm"),
            kernel: PathBuf::from("target/taritd-metrics-test/kernel"),
            rootfs: PathBuf::from("target/taritd-metrics-test/rootfs"),
            socket_dir: PathBuf::from("target/taritd-metrics-test/sockets"),
            db_path: PathBuf::from("target/taritd-metrics-test/fleet.db"),
            net_state_path: PathBuf::from("target/taritd-metrics-test/net-state.json"),
            images_dir: PathBuf::from("target/taritd-metrics-test/images"),
            max_vms: 4,
            max_vcpus: 4,
            max_memory_mib: 1024,
            peer_secret: "peer-secret".into(),
            database_url: None,
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
            ssh_gateway_host_key_path: PathBuf::from("target/taritd-metrics-test/ssh_host"),
            share_listen: None,
            share_domain: None,
            share_token_key: None,
            share_token_ttl_secs: 300,
            share_connect_timeout_ms: 10_000,
            share_idle_timeout_secs: 300,
        };
        let store = Arc::new(Mutex::new(Store::open(":memory:").unwrap()));
        let shares = crate::shares::ShareRepository::new(Arc::clone(&store), None);
        let (store_tx, _store_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState {
            config: config.clone(),
            audit_outbox: Arc::new(crate::audit::LocalAuditOutbox::new(Arc::clone(&store))),
            store,
            exec_cache: Arc::new(RwLock::new(HashMap::new())),
            vm_cache: Arc::new(RwLock::new(HashMap::new())),
            store_tx,
            pty_registry: Arc::new(PtyRegistry::default()),
            supervisor: Arc::new(VmmSupervisor::new(config.clone())),
            scheduler: Arc::new(Scheduler::new(config)),
            peer: Arc::new(PeerClient::new("peer-secret".into())),
            shares,
            fleet: None,
            metrics: Arc::new(Metrics::default()),
        }
    }
}
