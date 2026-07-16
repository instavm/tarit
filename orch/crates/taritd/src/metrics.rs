use crate::api::AppState;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
    Arc,
};
use tarit_types::{ShareVisibility, VmStatus};

const SHARE_VISIBILITY_COUNT: usize = 3;
const SHARE_STATUS_CLASS_COUNT: usize = 6;

#[repr(u8)]
#[derive(Clone, Copy)]
pub(crate) enum ShareMetricVisibility {
    Public = 0,
    Private = 1,
    Unknown = 2,
}

impl ShareMetricVisibility {
    fn label(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
            Self::Unknown => "unknown",
        }
    }
}

impl From<ShareVisibility> for ShareMetricVisibility {
    fn from(visibility: ShareVisibility) -> Self {
        match visibility {
            ShareVisibility::Public => Self::Public,
            ShareVisibility::Private => Self::Private,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy)]
enum ShareStatusClass {
    Informational = 0,
    Success = 1,
    Redirect = 2,
    ClientError = 3,
    ServerError = 4,
    Cancelled = 5,
}

impl ShareStatusClass {
    fn from_status(status: u16) -> Self {
        match status {
            100..=199 => Self::Informational,
            200..=299 => Self::Success,
            300..=399 => Self::Redirect,
            400..=499 => Self::ClientError,
            _ => Self::ServerError,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Informational => "1xx",
            Self::Success => "2xx",
            Self::Redirect => "3xx",
            Self::ClientError => "4xx",
            Self::ServerError => "5xx",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug)]
pub struct Metrics {
    vm_create_total: AtomicU64,
    vm_create_errors_total: AtomicU64,
    exec_total: AtomicU64,
    share_requests: [AtomicU64; SHARE_VISIBILITY_COUNT * SHARE_STATUS_CLASS_COUNT],
    share_auth_failures_total: AtomicU64,
    share_owner_failures_total: AtomicU64,
    share_target_failures_total: AtomicU64,
    share_bytes_in_total: AtomicU64,
    share_bytes_out_total: AtomicU64,
    active_share_http: AtomicU64,
    active_share_websockets: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            vm_create_total: AtomicU64::new(0),
            vm_create_errors_total: AtomicU64::new(0),
            exec_total: AtomicU64::new(0),
            share_requests: std::array::from_fn(|_| AtomicU64::new(0)),
            share_auth_failures_total: AtomicU64::new(0),
            share_owner_failures_total: AtomicU64::new(0),
            share_target_failures_total: AtomicU64::new(0),
            share_bytes_in_total: AtomicU64::new(0),
            share_bytes_out_total: AtomicU64::new(0),
            active_share_http: AtomicU64::new(0),
            active_share_websockets: AtomicU64::new(0),
        }
    }
}

pub(crate) struct ActiveShareHttp {
    metrics: Arc<Metrics>,
    visibility: AtomicU8,
    finished: AtomicBool,
}

impl ActiveShareHttp {
    pub(crate) fn set_visibility(&self, visibility: ShareMetricVisibility) {
        self.visibility.store(visibility as u8, Ordering::Relaxed);
    }

    pub(crate) fn finish(&self, status: u16) {
        if !self.finished.swap(true, Ordering::AcqRel) {
            self.metrics
                .observe_share(self.visibility(), ShareStatusClass::from_status(status));
        }
    }

    fn visibility(&self) -> ShareMetricVisibility {
        match self.visibility.load(Ordering::Relaxed) {
            value if value == ShareMetricVisibility::Public as u8 => ShareMetricVisibility::Public,
            value if value == ShareMetricVisibility::Private as u8 => {
                ShareMetricVisibility::Private
            }
            _ => ShareMetricVisibility::Unknown,
        }
    }
}

impl Drop for ActiveShareHttp {
    fn drop(&mut self) {
        if !self.finished.swap(true, Ordering::AcqRel) {
            self.metrics
                .observe_share(self.visibility(), ShareStatusClass::Cancelled);
        }
        decrement_gauge(&self.metrics.active_share_http);
    }
}

pub(crate) struct ActiveShareWebSocket {
    metrics: Arc<Metrics>,
}

impl Drop for ActiveShareWebSocket {
    fn drop(&mut self) {
        decrement_gauge(&self.metrics.active_share_websockets);
    }
}

impl Metrics {
    pub fn inc_vm_create_total(&self) {
        increment_counter(&self.vm_create_total, 1);
    }

    pub fn inc_vm_create_errors_total(&self) {
        increment_counter(&self.vm_create_errors_total, 1);
    }

    pub fn inc_exec_total(&self) {
        increment_counter(&self.exec_total, 1);
    }

    pub(crate) fn track_share_http(self: &Arc<Self>) -> ActiveShareHttp {
        increment_counter(&self.active_share_http, 1);
        ActiveShareHttp {
            metrics: Arc::clone(self),
            visibility: AtomicU8::new(ShareMetricVisibility::Unknown as u8),
            finished: AtomicBool::new(false),
        }
    }

    pub(crate) fn track_share_websocket(self: &Arc<Self>) -> ActiveShareWebSocket {
        increment_counter(&self.active_share_websockets, 1);
        ActiveShareWebSocket {
            metrics: Arc::clone(self),
        }
    }

    pub(crate) fn inc_share_auth_failures(&self) {
        increment_counter(&self.share_auth_failures_total, 1);
    }

    pub(crate) fn inc_share_owner_failures(&self) {
        increment_counter(&self.share_owner_failures_total, 1);
    }

    pub(crate) fn inc_share_target_failures(&self) {
        increment_counter(&self.share_target_failures_total, 1);
    }

    pub(crate) fn add_share_bytes_in(&self, bytes: u64) {
        increment_counter(&self.share_bytes_in_total, bytes);
    }

    pub(crate) fn add_share_bytes_out(&self, bytes: u64) {
        increment_counter(&self.share_bytes_out_total, bytes);
    }

    #[cfg(test)]
    pub(crate) fn active_share_websockets(&self) -> u64 {
        self.active_share_websockets.load(Ordering::Relaxed)
    }

    fn observe_share(&self, visibility: ShareMetricVisibility, status: ShareStatusClass) {
        let index = visibility as usize * SHARE_STATUS_CLASS_COUNT + status as usize;
        increment_counter(&self.share_requests[index], 1);
    }

    pub(crate) fn render_share_metrics(&self) -> String {
        let mut out = String::new();
        metric_header(
            &mut out,
            "taritd_share_requests_total",
            "counter",
            "Share gateway requests by bounded visibility and response status class.",
        );
        for visibility in [
            ShareMetricVisibility::Public,
            ShareMetricVisibility::Private,
            ShareMetricVisibility::Unknown,
        ] {
            for status in [
                ShareStatusClass::Informational,
                ShareStatusClass::Success,
                ShareStatusClass::Redirect,
                ShareStatusClass::ClientError,
                ShareStatusClass::ServerError,
                ShareStatusClass::Cancelled,
            ] {
                let index = visibility as usize * SHARE_STATUS_CLASS_COUNT + status as usize;
                let _ = writeln!(
                    out,
                    "taritd_share_requests_total{{visibility=\"{}\",status_class=\"{}\"}} {}",
                    visibility.label(),
                    status.label(),
                    self.share_requests[index].load(Ordering::Relaxed)
                );
            }
        }
        for (name, help, counter) in [
            (
                "taritd_share_auth_failures_total",
                "Share gateway authorization failures.",
                &self.share_auth_failures_total,
            ),
            (
                "taritd_share_owner_failures_total",
                "Share gateway owner resolution failures.",
                &self.share_owner_failures_total,
            ),
            (
                "taritd_share_target_failures_total",
                "Share gateway upstream target failures.",
                &self.share_target_failures_total,
            ),
            (
                "taritd_share_bytes_in_total",
                "Bytes read from share gateway clients.",
                &self.share_bytes_in_total,
            ),
            (
                "taritd_share_bytes_out_total",
                "Bytes written to share gateway clients.",
                &self.share_bytes_out_total,
            ),
        ] {
            metric_header(&mut out, name, "counter", help);
            let _ = writeln!(out, "{} {}", name, counter.load(Ordering::Relaxed));
        }
        for (name, help, gauge) in [
            (
                "taritd_share_active_http",
                "In-flight share gateway HTTP requests.",
                &self.active_share_http,
            ),
            (
                "taritd_share_active_websockets",
                "Active share gateway WebSocket bridges.",
                &self.active_share_websockets,
            ),
        ] {
            metric_header(&mut out, name, "gauge", help);
            let _ = writeln!(out, "{} {}", name, gauge.load(Ordering::Relaxed));
        }
        out
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

fn increment_counter(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(amount))
    });
}

fn decrement_gauge(gauge: &AtomicU64) {
    let _ = gauge.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(1))
    });
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
    let label_secret = state.config.peer_secret.as_bytes();
    for (tenant, count) in tenant_vm_counts(state) {
        let label = if expose_tenant {
            tenant
        } else {
            hashed_label(label_secret, &tenant)
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
        "Host RSS bytes for currently-running local VMM processes. The vm_id label is a stable hash unless TARIT_METRICS_EXPOSE_TENANT_LABELS=1.",
    );
    metric_header(
        &mut out,
        "taritd_vm_cpu_seconds_total",
        "counter",
        "Host CPU seconds consumed by currently-running local VMM processes. The vm_id label is a stable hash unless TARIT_METRICS_EXPOSE_TENANT_LABELS=1.",
    );
    for (vm_id, pid) in running_local_vms(state) {
        let vm_label = if expose_tenant {
            vm_id
        } else {
            hashed_label(label_secret, &vm_id)
        };
        if let Some(rss_bytes) = proc_rss_bytes(pid) {
            let _ = writeln!(
                out,
                "taritd_vm_memory_rss_bytes{{vm_id=\"{}\"}} {}",
                escape_label_value(&vm_label),
                rss_bytes
            );
        }
        if let Some(cpu_seconds) = proc_cpu_seconds_total(pid) {
            let _ = writeln!(
                out,
                "taritd_vm_cpu_seconds_total{{vm_id=\"{}\"}} {:.2}",
                escape_label_value(&vm_label),
                cpu_seconds
            );
        }
    }

    out.push_str(&state.metrics.render_share_metrics());

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

/// Stable, non-reversible label for a confidential identifier (tenant key or VM
/// id), so the unauthenticated `/metrics` endpoint does not enumerate them. The
/// value is keyed with the node's peer secret via HMAC-SHA256 so low-entropy
/// identifiers cannot be recovered by offline brute force. 48 bits of the tag
/// keep distinct values distinct on a dashboard without revealing identity.
fn hashed_label(secret: &[u8], value: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(value.as_bytes());
    let digest = mac.finalize().into_bytes();
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
            "taritd_share_requests_total",
            "taritd_share_auth_failures_total",
            "taritd_share_owner_failures_total",
            "taritd_share_target_failures_total",
            "taritd_share_bytes_in_total",
            "taritd_share_bytes_out_total",
            "taritd_share_active_http",
            "taritd_share_active_websockets",
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
    fn hashed_label_is_stable_and_opaque() {
        let secret = b"metrics-label-secret-key";
        let a = hashed_label(secret, "tenant-a");
        let b = hashed_label(secret, "tenant-a");
        let c = hashed_label(secret, "tenant-b");
        assert_eq!(a, b, "hash is stable for a value under one key");
        assert_ne!(a, c, "different values map to different labels");
        assert!(a.starts_with("h:"));
        assert!(!a.contains("tenant-a"));
        assert_eq!(a.len(), 14, "h: plus 12 hex chars");

        let other_key = hashed_label(b"a-different-secret-key-value", "tenant-a");
        assert_ne!(a, other_key, "the label is keyed by the peer secret");

        let vm = "0949fd9e-2084-4155-b5a7-ca0bfb54b920";
        let hashed_vm = hashed_label(secret, vm);
        assert!(hashed_vm.starts_with("h:"));
        assert!(
            !hashed_vm.contains(vm),
            "raw VM id must not survive hashing"
        );
    }

    #[test]
    fn share_metrics_are_bounded_secret_safe_and_raii_balanced() {
        let metrics = Arc::new(Metrics::default());

        let request = metrics.track_share_http();
        request.set_visibility(ShareMetricVisibility::Public);
        request.finish(200);
        drop(request);

        let cancelled = metrics.track_share_http();
        drop(cancelled);

        let websocket = metrics.track_share_websocket();
        drop(websocket);

        metrics.inc_share_auth_failures();
        metrics.inc_share_owner_failures();
        metrics.inc_share_target_failures();
        metrics.add_share_bytes_in(12);
        metrics.add_share_bytes_out(34);

        let rendered = metrics.render_share_metrics();

        for metric in [
            "taritd_share_requests_total",
            "taritd_share_auth_failures_total",
            "taritd_share_owner_failures_total",
            "taritd_share_target_failures_total",
            "taritd_share_bytes_in_total",
            "taritd_share_bytes_out_total",
            "taritd_share_active_http",
            "taritd_share_active_websockets",
        ] {
            assert!(
                rendered.contains(&format!("# HELP {metric} ")),
                "{metric} HELP"
            );
            assert!(
                rendered.contains(&format!("# TYPE {metric} ")),
                "{metric} TYPE"
            );
        }

        assert!(rendered
            .contains("taritd_share_requests_total{visibility=\"public\",status_class=\"2xx\"} 1"));
        assert!(rendered.contains(
            "taritd_share_requests_total{visibility=\"unknown\",status_class=\"cancelled\"} 1"
        ));
        assert!(rendered.contains("taritd_share_bytes_in_total 12"));
        assert!(rendered.contains("taritd_share_bytes_out_total 34"));
        assert!(rendered.contains("taritd_share_active_http 0"));
        assert!(rendered.contains("taritd_share_active_websockets 0"));

        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.starts_with("taritd_share_requests_total{"))
                .count(),
            18,
            "the visibility/status dimensions must stay bounded"
        );
        for secret_or_identifier in ["calm-red-fox", "share-token", "tenant-a", "token=", "slug="] {
            assert!(
                !rendered.contains(secret_or_identifier),
                "share metric output leaked {secret_or_identifier}"
            );
        }
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
            lifecycle: Arc::new(Mutex::new(HashMap::new())),
            lifecycle_faults: Arc::new(Mutex::new(Vec::new())),
            lifecycle_pauses: Arc::new(Mutex::new(HashMap::new())),
            terminal_transition_gate: Arc::new(tokio::sync::Mutex::new(())),
            pty_registry: Arc::new(PtyRegistry::default()),
            supervisor: Arc::new(VmmSupervisor::new(config.clone())),
            scheduler: Arc::new(Scheduler::new(config)),
            peer: Arc::new(PeerClient::new("peer-secret".into())),
            shares,
            fleet: None,
            metrics: Arc::new(Metrics::default()),
            share_runtime: Arc::new(crate::share_gateway::ShareRuntime::default()),
        }
    }
}
