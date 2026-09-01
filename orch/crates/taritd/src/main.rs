mod api;
mod audit;
mod autoscale;
mod cli;
mod cluster;
mod config;
mod disk;
mod gateway;
mod image;
mod internal;
mod metrics;
mod net;
mod openapi;
mod ops;
mod peer;
mod peer_tls;
mod pty;
mod scheduler;
mod share_gateway;
mod shares;
mod ssh_keys;
mod supervisor;
mod usage;
mod volume_provider;
mod warmpool;

use anyhow::{anyhow, Context};
use api::{router, AppState};
use clap::Parser;
use config::{CloudObjectStoreConfig, Config, PtyConnectionLimits};
use peer::PeerClient;
use scheduler::Scheduler;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use supervisor::VmmSupervisor;
use tarit_store::Store;
use tarit_types::VmStatus;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

use tarit_fleet::PostgresFleet;

const HTTP_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const BACKGROUND_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const STORE_QUEUE_CAPACITY: usize = 8_192;

#[derive(Clone)]
struct ShutdownCoordinator {
    tx: watch::Sender<Option<&'static str>>,
    supervisor: Arc<VmmSupervisor>,
}

impl ShutdownCoordinator {
    fn new(tx: watch::Sender<Option<&'static str>>, supervisor: Arc<VmmSupervisor>) -> Self {
        Self { tx, supervisor }
    }

    fn close_admission(&self) {
        self.supervisor.begin_shutdown();
    }

    fn request(&self, reason: &'static str) {
        self.close_admission();
        self.tx.send_if_modified(|current| {
            if current.is_none() {
                *current = Some(reason);
                true
            } else {
                false
            }
        });
    }
}

type FleetStartup = (
    Option<Arc<PostgresFleet>>,
    Option<JoinHandle<()>>,
    Option<JoinHandle<()>>,
);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    if cli.runs_server() {
        init_tracing();
        let preflight_taps = net::startup_preflight().context(
            "contain pre-existing Tarit TAPs before configuration, database, image, or VM discovery",
        )?;
        let config = Config::from_env().context("load config")?;
        let pty_limits = PtyConnectionLimits::from_env().context("load PTY connection limits")?;
        run_server(config, preflight_taps, pty_limits).await
    } else {
        cli::run_client(cli).await
    }
}

fn init_tracing() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "taritd=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}

fn persist_startup_vm_observation(
    store: &Store,
    vm: &tarit_types::VmRecord,
    context: &str,
) -> anyhow::Result<()> {
    store
        .insert_vm(vm)
        .with_context(|| format!("{context}: {}", vm.id))
}

fn is_identityless_legacy_creating(config: &Config, record: &tarit_types::VmRecord) -> bool {
    record.host_id == config.host_id
        && record.status == VmStatus::Creating
        && record.runtime_layout.is_none()
        && record.socket_path.as_deref().is_none_or(str::is_empty)
        && record.pid.is_none_or(|pid| pid == 0)
}

fn reconcile_legacy_creating_records(
    config: &Config,
    store: &Store,
    supervisor: &VmmSupervisor,
    records: &mut [tarit_types::VmRecord],
) -> anyhow::Result<()> {
    for record in records
        .iter_mut()
        .filter(|record| is_identityless_legacy_creating(config, record))
    {
        let fenced_revision = record.revision.checked_add(2).ok_or_else(|| {
            anyhow::anyhow!(
                "legacy Creating VM {} revision is exhausted; cleanup cannot be fenced",
                record.id
            )
        })?;
        let terminated = supervisor
            .reconcile_legacy_creating_runtime(record.id)
            .with_context(|| {
                format!(
                    "contain legacy Creating runtime and clean owned artifacts for VM {}",
                    record.id
                )
            })?;
        record.status = VmStatus::Error;
        record.revision = fenced_revision;
        record.updated_at = chrono::Utc::now();
        persist_startup_vm_observation(
            store,
            record,
            "persist terminal legacy Creating VM before runtime-layout backfill",
        )?;
        tracing::warn!(
            vm = %record.id,
            revision = record.revision,
            terminated,
            "reconciled identity-less legacy Creating VM before runtime-layout backfill"
        );
    }
    Ok(())
}

fn backfill_legacy_runtime_layouts(
    config: &Config,
    store: &Store,
    records: &mut [tarit_types::VmRecord],
) -> anyhow::Result<()> {
    for record in records {
        let Some(layout) = supervisor::infer_legacy_nonjailed_runtime_layout(config, record)
            .with_context(|| format!("infer legacy runtime layout for VM {}", record.id))?
        else {
            continue;
        };
        record.runtime_layout = Some(layout);
        record.revision = record.revision.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!(
                "legacy active VM {} revision is exhausted; drain required before upgrade",
                record.id
            )
        })?;
        record.updated_at = chrono::Utc::now();
        persist_startup_vm_observation(
            store,
            record,
            "persist inferred legacy runtime layout before adoption",
        )?;
        tracing::warn!(
            vm = %record.id,
            revision = record.revision,
            "persisted inferred legacy non-jailed runtime layout before adoption"
        );
    }
    Ok(())
}

async fn run_server(
    mut config: Config,
    preflight_taps: Vec<String>,
    pty_limits: PtyConnectionLimits,
) -> anyhow::Result<()> {
    tracing::info!(
        listen = %config.listen,
        host_id = %config.host_id,
        reap_on_shutdown = config.reap_on_shutdown,
        "starting taritd"
    );

    // Bind every configured listener before startup begins. Dropping this local
    // releases all sockets if any subsequent setup step fails.
    let ServerListeners {
        control,
        peer: peer_listener,
        share,
        ssh,
    } = bind_server_listeners(&config).await?;

    let artifact_object_store = config
        .cloud_object_store
        .as_ref()
        .map(CloudObjectStoreConfig::open)
        .transpose()
        .context("initialize immutable artifact object store")?
        .map(Arc::new);

    std::fs::create_dir_all(&config.socket_dir).ok();
    std::fs::create_dir_all(&config.images_dir).ok();
    if let Some(parent) = config.db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let store = Store::open(&config.db_path).context("open store")?;
    image::resolve_warm_pool_images(&mut config, &store).context("resolve warm-pool images")?;
    warmpool::validate_exact_classes(&config).context("validate warm-pool classes")?;
    let mut persisted_vms = store
        .list_vms()
        .context("load persisted VMs during startup")?;
    let persisted_hibernations = store
        .list_hibernations()
        .context("load pending hibernations during startup")?;
    let mut aborted_fleet_hibernations = Vec::new();
    let owned_vm_ids = persisted_vms
        .iter()
        .filter(|vm| {
            vm.host_id == config.host_id
                && !is_identityless_legacy_creating(&config, vm)
                && matches!(
                    vm.status,
                    VmStatus::Creating
                        | VmStatus::Running
                        | VmStatus::Paused
                        | VmStatus::Suspended
                        | VmStatus::Hibernated
                )
        })
        .map(|vm| vm.id)
        .collect::<Vec<_>>();
    let live_vm_ids = persisted_vms
        .iter()
        .filter(|vm| {
            vm.host_id == config.host_id
                && !is_identityless_legacy_creating(&config, vm)
                && matches!(
                    vm.status,
                    VmStatus::Creating | VmStatus::Running | VmStatus::Paused | VmStatus::Suspended
                )
        })
        .map(|vm| vm.id)
        .collect::<Vec<_>>();
    let scheduler = Arc::new(Scheduler::new(config.clone()));
    let supervisor = Arc::new(
        VmmSupervisor::new_with_live_vms(
            config.clone(),
            live_vm_ids.iter().copied(),
            &preflight_taps,
            Arc::clone(&scheduler),
        )
        .context("initialize fail-closed network recovery")?,
    );
    reconcile_legacy_creating_records(&config, &store, &supervisor, &mut persisted_vms)
        .context("reconcile legacy Creating VMs before runtime-layout backfill")?;
    backfill_legacy_runtime_layouts(&config, &store, &mut persisted_vms)
        .context("backfill legacy active VM runtime layouts")?;
    // Re-adopt VMs that survived this restart so the control plane can manage
    // them again. Their network policy was reconciled during supervisor
    // construction; this restores the exec/pause/snapshot/delete path. VMs that
    // can no longer be controlled (dead or reused PID, missing socket, or
    // missing network allocation) are marked terminal so the API never reports
    // an uncontrollable VM as running. Persisting that terminal state is
    // mandatory: if it fails, startup aborts rather than serve stale durable
    // Running/Paused/Suspended records for VMs that no longer exist.
    {
        let failures = supervisor
            .readopt_running_vms(&mut persisted_vms)
            .await
            .context("re-adopt locally owned VMMs")?;
        let failed_ids = failures
            .iter()
            .map(|failure| failure.id)
            .collect::<Vec<_>>();
        for failure in &failures {
            let vm = match persisted_vms.iter_mut().find(|vm| vm.id == failure.id) {
                Some(vm) => vm,
                None => {
                    anyhow::bail!(
                        "startup reconciliation lost persisted VM {} while fencing: {}",
                        failure.id,
                        failure.reason
                    );
                }
            };
            let recoverable_hibernation = persisted_hibernations
                .iter()
                .find(|hibernation| hibernation.vm_id == vm.id);
            vm.status = if recoverable_hibernation.is_some() {
                VmStatus::Hibernated
            } else {
                VmStatus::Error
            };
            // N+1 may have reached the fleet before the previous process
            // crashed with SQLite still at N. Fence the terminal observation
            // at N+2 and publish this exact record to every store.
            vm.revision = match vm.revision.checked_add(2) {
                Some(revision) => revision,
                None => {
                    anyhow::bail!(
                        "startup reconciliation exhausted VM {} revision while fencing: {}",
                        failure.id,
                        failure.reason
                    );
                }
            };
            vm.updated_at = chrono::Utc::now();
            if recoverable_hibernation.is_some() {
                vm.runtime_layout = None;
                vm.socket_path = None;
                vm.pid = None;
                tracing::warn!(vm = %failure.id, reason = %failure.reason,
                    "startup reconciliation recovered an interrupted hibernation");
            } else {
                tracing::warn!(vm = %failure.id, reason = %failure.reason,
                    "startup reconciliation fenced an unrecoverable VM record");
            }
            persist_startup_vm_observation(
                &store,
                vm,
                if recoverable_hibernation.is_some() {
                    "persist recovered interrupted hibernation"
                } else {
                    "persist terminal status for startup-reconciled VM"
                },
            )?;
        }
        for vm in &persisted_vms {
            if vm.host_id == config.host_id
                && matches!(
                    vm.status,
                    VmStatus::Running | VmStatus::Paused | VmStatus::Suspended
                )
                && !failed_ids.contains(&vm.id)
            {
                persist_startup_vm_observation(
                    &store,
                    vm,
                    "persist observed status for re-adopted VM",
                )?;
            }
        }
        // If the old VMM was successfully re-adopted, the process crashed
        // before hibernation teardown took effect. Abort that prepared intent;
        // the still-running VM remains authoritative and can be hibernated
        // again. Failed readoption above intentionally retains the row as the
        // durable resume source.
        for hibernation in &persisted_hibernations {
            let re_adopted = persisted_vms.iter().any(|vm| {
                vm.id == hibernation.vm_id
                    && matches!(
                        vm.status,
                        VmStatus::Running | VmStatus::Paused | VmStatus::Suspended
                    )
                    && !failed_ids.contains(&vm.id)
            });
            if re_adopted {
                store
                    .delete_hibernation(&hibernation.owner_key, hibernation.vm_id)
                    .with_context(|| {
                        format!(
                            "clear aborted hibernation for re-adopted VM {}",
                            hibernation.vm_id
                        )
                    })?;
                aborted_fleet_hibernations.push((hibernation.owner_key.clone(), hibernation.vm_id));
            }
        }
    }
    // Only sweep after every durable Creating/live record and every owned
    // unpersisted jail/cgroup runtime has been adopted or confirmed dead.
    // Otherwise GC could remove a live jail or free its UID/GID lease.
    let startup_references = artifact_references(
        &persisted_vms,
        &store
            .list_snapshots()
            .context("load durable snapshot references for startup GC")?,
    );
    let startup_gc = supervisor
        .sweep_owned_artifacts(startup_references)
        .context("sweep owned artifacts during startup")?;
    tracing::info!(
        removed_files = startup_gc.removed_files,
        removed_jails = startup_gc.removed_jails,
        "startup owned-artifact sweep completed"
    );
    // Build the peer HTTP client off the async runtime. `reqwest::blocking`
    // spins up its own current-thread runtime; constructing it inside a tokio
    // context panics on current tokio ("Cannot drop a runtime ... from within
    // an asynchronous context"). A plain OS thread has no ambient runtime, so
    // construction is safe there. All runtime peer calls already run via
    // spawn_blocking, so this only moves the one-time construction off-thread.
    let peer = {
        let secret = config.peer_secret.clone();
        let allow_insecure = config.allow_insecure_peer_http;
        let host_id = config.host_id.clone();
        let session_id = config.host_session_id;
        let tls = config.peer_tls.clone();
        std::thread::spawn(move || {
            PeerClient::new_for_host(secret, allow_insecure, host_id, session_id, tls.as_ref())
        })
        .join()
        .map_err(|_| anyhow::anyhow!("peer client init thread panicked"))??
    };

    let peer_certificate_sha256 = config
        .peer_tls
        .as_ref()
        .map(peer_tls::leaf_certificate_sha256)
        .transpose()
        .context("fingerprint peer TLS leaf certificate")?;

    // Register self in local roster for single-host / scheduler.
    {
        let cap = scheduler.local_capacity(1, 256);
        let host = tarit_store::HostRecord {
            host_id: config.host_id.clone(),
            boot_session_id: Some(config.host_session_id),
            peer_certificate_sha256: peer_certificate_sha256.clone(),
            rpc_addr: Some(config.rpc_addr.clone()),
            sandbox_count: cap.sandbox_count,
            free_vcpus: cap.free_vcpus,
            free_memory_mib: cap.free_memory_mib,
            healthy: true,
            last_heartbeat: chrono::Utc::now(),
        };
        store.upsert_host(&host).ok();
    }

    let store = Arc::new(Mutex::new(store));

    // Write-behind store: an in-memory VM cache is the read source of truth, and a
    // single background writer owns all SQLite mutation, so no request blocks on
    // the store mutex on the hot path. Load any persisted VMs into the cache first.
    let vm_cache: Arc<RwLock<HashMap<Uuid, tarit_types::VmRecord>>> =
        Arc::new(RwLock::new(HashMap::new()));
    {
        let mut c = vm_cache.write().unwrap();
        for vm in &persisted_vms {
            c.insert(vm.id, vm.clone());
        }
    }
    let (store_tx, mut store_rx) =
        tokio::sync::mpsc::channel::<api::StoreWrite>(STORE_QUEUE_CAPACITY);
    let (shutdown_tx, shutdown_rx) = watch::channel(None::<&'static str>);
    let shutdown = ShutdownCoordinator::new(shutdown_tx.clone(), Arc::clone(&supervisor));

    // Connect the global fleet registry (Postgres) if configured. In cluster
    // mode this drives cross-node placement, VM->owner routing, and membership;
    // single-host mode leaves it None and everything runs locally.
    let (fleet, fleet_sync, autoscaler): FleetStartup = if let Some(ref url) = config.database_url {
        let fleet = Arc::new(
            PostgresFleet::connect(url)
                .await
                .context("postgres fleet")?,
        );
        let initial_capacity = scheduler.local_capacity(1, 256);
        let initial_host = tarit_fleet::host_record_from_capacity(
            &config.host_id,
            config.host_session_id,
            peer_certificate_sha256.clone(),
            Some(config.rpc_addr.clone()),
            initial_capacity.sandbox_count,
            initial_capacity.free_vcpus,
            initial_capacity.free_memory_mib,
        );
        // Publish this process incarnation before serving or routing any peer
        // request. A previous process with the same host_id is fenced as soon
        // as this transaction commits.
        fleet
            .upsert_host(&initial_host)
            .await
            .context("publish initial host boot session")?;
        // SQLite and the local read cache already contain the restart-fenced
        // VMM observation. Publish that same record to the fleet before any
        // listener can serve or route traffic.
        for vm in persisted_vms
            .iter()
            .filter(|vm| owned_vm_ids.contains(&vm.id))
        {
            fleet
                .upsert_vm(vm)
                .await
                .with_context(|| format!("publish restart-reconciled VM {} to fleet", vm.id))?;
        }
        // A successfully re-adopted VMM is authoritative over an interrupted
        // hibernation prepared by the previous process. SQLite was cleared
        // before connecting to Postgres; now that the Running observation is
        // durably published, remove the matching fleet intent as well so its
        // artifact reference cannot leak. NotFound is already converged (for
        // example, if the previous process crashed after the fleet deletion).
        for (owner_key, vm_id) in &aborted_fleet_hibernations {
            match fleet.delete_hibernation(owner_key, *vm_id).await {
                Ok(()) | Err(tarit_fleet::FleetError::NotFound) => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("clear aborted fleet hibernation for re-adopted VM {vm_id}")
                    });
                }
            }
        }
        let interrupted = fleet
            .fail_incomplete_executions_for_host(
                &config.host_id,
                "accepting taritd restarted before execution reached a terminal state",
            )
            .await
            .context("reconcile incomplete fleet executions")?;
        if interrupted > 0 {
            tracing::warn!(interrupted, "marked interrupted global executions failed");
        }
        let fleet_sync = spawn_fleet_sync(
            Arc::clone(&fleet),
            Arc::clone(&store),
            config.clone(),
            peer_certificate_sha256.clone(),
            Arc::clone(&scheduler),
            shutdown_rx.clone(),
        );
        let autoscaler = autoscale::spawn(
            Arc::clone(&fleet),
            config.clone(),
            supervisor.admission_gate(),
            shutdown_rx.clone(),
        );
        tracing::info!("fleet: connected to global control-plane store");
        (Some(fleet), Some(fleet_sync), autoscaler)
    } else {
        (None, None, None)
    };

    let share_runtime = Arc::new(share_gateway::ShareRuntime::new(
        shutdown_tx.clone(),
        shutdown_rx.clone(),
    ));
    let shares = shares::ShareRepository::new(Arc::clone(&store), fleet.clone());
    let state = AppState {
        config: config.clone(),
        audit_outbox: Arc::new(audit::LocalAuditOutbox::new(Arc::clone(&store))),
        store,
        exec_cache: Arc::new(RwLock::new(HashMap::new())),
        vm_cache,
        store_tx,
        lifecycle: Arc::new(Mutex::new(HashMap::new())),
        activation_gates: Arc::new(Mutex::new(HashMap::new())),
        #[cfg(test)]
        lifecycle_faults: Arc::new(Mutex::new(Vec::new())),
        #[cfg(test)]
        lifecycle_pauses: Arc::new(Mutex::new(HashMap::new())),
        terminal_transition_gate: Arc::new(tokio::sync::Mutex::new(())),
        pty_registry: Arc::new(pty::PtyRegistry::new(pty_limits)),
        supervisor: Arc::clone(&supervisor),
        scheduler: scheduler.clone(),
        peer: Arc::new(peer),
        shares,
        artifact_object_store,
        fleet,
        metrics: Arc::new(metrics::Metrics::default()),
        share_runtime: Arc::clone(&share_runtime),
    };
    if let Some(provider) = state.artifact_object_store.as_ref() {
        tracing::info!(
            provider = provider.provider_name(),
            "immutable artifact object store configured"
        );
    }

    // Start every background worker only after all listener binds succeeded.
    let store_writer = {
        let store = Arc::clone(&state.store);
        let metrics = Arc::clone(&state.metrics);
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                let op = tokio::select! {
                    biased;
                    _ = wait_for_shutdown(shutdown_rx.clone()) => break,
                    op = store_rx.recv() => op,
                };
                let Some(op) = op else {
                    break;
                };
                match store.lock() {
                    Ok(s) => match op {
                        api::StoreWrite::VmDurable(rec, completion) => {
                            let result = s.insert_vm(&rec).map_err(api::store_err);
                            if let Err(error) = &result {
                                metrics.inc_store_write_failure();
                                tracing::error!(vm = %rec.id, %error, "persist durable VM record");
                            }
                            let _ = completion.send(result);
                        }
                        api::StoreWrite::Exec(rec) => {
                            if let Err(error) = s.insert_execution(&rec) {
                                metrics.inc_store_write_failure();
                                tracing::error!(execution = %rec.id, %error, "persist execution record");
                            }
                        }
                        api::StoreWrite::Usage(ev) => {
                            if let Err(error) = s.enqueue_usage(&ev) {
                                metrics.inc_store_write_failure();
                                tracing::error!(event = %ev.id, %error, "persist usage outbox event");
                            }
                        }
                        api::StoreWrite::Audit(ev) => {
                            if let Err(error) = s.enqueue_audit(&ev) {
                                metrics.inc_store_write_failure();
                                tracing::error!(event = %ev.id, %error, "persist audit outbox event");
                            }
                        }
                    },
                    Err(_) => {
                        metrics.inc_store_write_failure();
                        tracing::error!("store lock poisoned in persistence worker");
                        if let api::StoreWrite::VmDurable(_, completion) = op {
                            let _ = completion.send(Err(tarit_types::OrchError::Internal(
                                "store lock poisoned during shutdown persistence".into(),
                            )));
                        }
                    }
                }
            }
        })
    };
    let warm_pool = warmpool::spawn_replenisher(
        Arc::clone(&supervisor),
        config.clone(),
        Arc::clone(&scheduler),
        shutdown_rx.clone(),
    );

    // Usage metering (VM runtime seconds) plus write-behind flush of usage and
    // audit events to the primary store. The meter always runs; the flusher is a
    // no-op without a fleet (single-host keeps events in the local outbox).
    let meter_secs = std::env::var("TARIT_USAGE_METER_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let flush_secs = std::env::var("TARIT_USAGE_FLUSH_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let usage_meter = usage::spawn_usage_meter(state.clone(), meter_secs, shutdown_rx.clone());
    let outbox_flusher =
        usage::spawn_outbox_flusher(state.clone(), flush_secs, shutdown_rx.clone());
    let vm_exit_reconciler = spawn_vm_exit_reconciler(state.clone(), shutdown_rx.clone());
    let artifact_gc = spawn_artifact_gc(state.clone(), shutdown_rx.clone());
    let artifact_repair = spawn_artifact_repair(state.clone(), shutdown_rx.clone());

    let shutdown_signal_task = spawn_shutdown_signal(shutdown.clone(), shutdown_rx.clone());
    let worker_tasks = BackgroundTasks::new(
        shutdown.clone(),
        [
            Some(store_writer),
            fleet_sync,
            Some(usage_meter),
            outbox_flusher,
            Some(vm_exit_reconciler),
            Some(artifact_gc),
            Some(artifact_repair),
            Some(shutdown_signal_task),
        ],
        warm_pool,
        autoscaler,
    );

    let (app, peer_app, share_app) = server_routers(state.clone());
    tracing::info!("control listener listening on http://{}", config.listen);
    if let Some(peer_addr) = config.peer_listen {
        tracing::info!(
            transport = if config.peer_tls.is_some() {
                "mTLS"
            } else {
                "plaintext-development"
            },
            "peer listener listening on {peer_addr}"
        );
    }
    if let Some(share_addr) = config.share_listen {
        tracing::info!("share listener listening on http://{}", share_addr);
    }
    let control_server = spawn_http_server(control, app, shutdown_rx.clone());
    let peer_server = match peer_listener {
        Some(listener) => Some(spawn_peer_server(
            listener,
            peer_app,
            config.peer_tls.as_ref(),
            shutdown_rx.clone(),
        )?),
        None => None,
    };
    let share_server =
        share.map(|listener| spawn_http_server(listener, share_app, shutdown_rx.clone()));
    let ssh_server = ssh.map(|listener| spawn_ssh_server(listener, state.clone()));
    let outcome = supervise_servers(
        control_server,
        peer_server,
        share_server,
        ssh_server,
        shutdown,
        shutdown_rx,
        HTTP_DRAIN_TIMEOUT,
    )
    .await;

    let shutdown_state = state.clone();
    let shutdown_share_runtime = Arc::clone(&share_runtime);
    finalize_lifecycle(
        outcome,
        move || async move {
            shutdown_share_runtime.stop(HTTP_DRAIN_TIMEOUT).await;
        },
        move || async move {
            worker_tasks.stop().await;
        },
        move |reason| async move { shutdown_sweep(&shutdown_state, reason).await },
    )
    .await
}

fn artifact_references(
    vms: &[tarit_types::VmRecord],
    snapshots: &[tarit_store::SnapshotRecord],
) -> disk::ArtifactReferences {
    let active_vms = vms.iter().filter(|vm| {
        matches!(
            vm.status,
            VmStatus::Creating | VmStatus::Running | VmStatus::Paused | VmStatus::Suspended
        )
    });
    let active_vm_ids = active_vms.clone().map(|vm| vm.id).collect::<HashSet<_>>();
    let mut runtime_paths = HashSet::new();
    for vm in active_vms {
        if let Some(layout) = &vm.runtime_layout {
            runtime_paths.extend(layout.artifact_paths.iter().map(std::path::PathBuf::from));
            if let Some(path) = &layout.overlay_path {
                runtime_paths.insert(std::path::PathBuf::from(path));
            }
            if let Some(path) = &layout.jail_path {
                runtime_paths.insert(std::path::PathBuf::from(path));
            }
        }
    }
    let mut snapshot_paths = HashSet::new();
    for snapshot in snapshots {
        snapshot_paths.insert(std::path::PathBuf::from(&snapshot.path));
        snapshot_paths.insert(std::path::PathBuf::from(format!(
            "{}.integrity",
            snapshot.path
        )));
        if let Some(path) = &snapshot.overlay_path {
            snapshot_paths.insert(std::path::PathBuf::from(path));
        }
    }
    disk::ArtifactReferences {
        active_vm_ids,
        snapshot_paths,
        runtime_paths,
    }
}

fn spawn_artifact_gc(
    state: AppState,
    shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(state.supervisor.disk_sweep_interval());
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(shutdown_rx.clone()) => break,
                _ = interval.tick() => {}
            }
            let vms = state
                .vm_cache
                .read()
                .map(|cache| cache.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let mut snapshots = match state.store.lock() {
                Ok(store) => match store.list_snapshots() {
                    Ok(snapshots) => snapshots,
                    Err(error) => {
                        tracing::error!(%error, "load snapshot references for artifact GC failed");
                        continue;
                    }
                },
                Err(_) => {
                    tracing::error!("store lock poisoned during artifact GC");
                    continue;
                }
            };
            if let Some(fleet) = state.fleet.as_ref() {
                let mut removed_ids = HashSet::new();
                let minimum_age = chrono::Duration::seconds(
                    i64::try_from(state.config.disk_pressure.artifact_min_age_secs)
                        .unwrap_or(i64::MAX),
                );
                for snapshot in &snapshots {
                    if snapshot.host_id != state.config.host_id
                        || chrono::Utc::now() - snapshot.created_at < minimum_age
                    {
                        continue;
                    }
                    let Some(owner_key) = snapshot.owner_key.as_deref() else {
                        continue;
                    };
                    let local_artifact = match state.store.lock() {
                        Ok(store) => store.get_artifact(owner_key, snapshot.snapshot_id),
                        Err(_) => {
                            tracing::error!("store lock poisoned during replica GC");
                            break;
                        }
                    };
                    let Ok(local_artifact) = local_artifact else {
                        continue;
                    };
                    if local_artifact.reference_count != 0
                        || local_artifact.storage_locator != snapshot.path
                    {
                        continue;
                    }
                    match fleet.get_artifact(owner_key, snapshot.snapshot_id).await {
                        Ok(_) => continue,
                        Err(tarit_fleet::FleetError::NotFound) => {}
                        Err(error) => {
                            tracing::warn!(artifact = %snapshot.snapshot_id, %error,
                                "fleet lookup for physical replica GC failed");
                            continue;
                        }
                    }
                    let removed_files = match disk::delete_owned_snapshot_components(
                        &state.config.socket_dir,
                        snapshot,
                    ) {
                        Ok(removed) => removed,
                        Err(error) => {
                            tracing::error!(artifact = %snapshot.snapshot_id, %error,
                                "physical replica deletion failed");
                            continue;
                        }
                    };
                    let metadata_deleted = match state.store.lock() {
                        Ok(store) => store.delete_local_replica_metadata_if_unreferenced(
                            owner_key,
                            snapshot.snapshot_id,
                            &snapshot.path,
                        ),
                        Err(_) => {
                            tracing::error!("store lock poisoned during replica metadata GC");
                            continue;
                        }
                    };
                    match metadata_deleted {
                        Ok(_) => {
                            removed_ids.insert(snapshot.snapshot_id);
                            tracing::info!(artifact = %snapshot.snapshot_id, removed_files,
                                "unreferenced physical replica removed");
                        }
                        Err(error) => tracing::error!(artifact = %snapshot.snapshot_id, %error,
                            "physical replica bytes removed but metadata cleanup will retry"),
                    }
                }
                snapshots.retain(|snapshot| !removed_ids.contains(&snapshot.snapshot_id));
            }
            match state
                .supervisor
                .sweep_owned_artifacts(artifact_references(&vms, &snapshots))
            {
                Ok(report) if report.removed_files > 0 || report.removed_jails > 0 => {
                    tracing::info!(
                        removed_files = report.removed_files,
                        removed_jails = report.removed_jails,
                        "periodic owned-artifact sweep completed"
                    );
                }
                Ok(_) => {}
                Err(error) => tracing::error!(%error, "periodic owned-artifact sweep failed"),
            }
            match sweep_remote_artifact_namespaces(&state).await {
                Ok(removed) if removed > 0 => {
                    tracing::info!(
                        removed_objects = removed,
                        "unreferenced remote artifact namespaces removed"
                    );
                }
                Ok(_) => {}
                Err(error) => tracing::error!(%error, "remote artifact namespace sweep failed"),
            }
        }
    })
}

const MAX_REMOTE_OBJECT_GC_LISTING: usize = 100_000;

fn remote_artifact_namespace(relative_key: &str) -> Option<(String, uuid::Uuid)> {
    let mut parts = relative_key.split('/');
    if parts.next()? != "tenants" {
        return None;
    }
    let owner_digest = parts.next()?;
    if owner_digest.len() != 64
        || !owner_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || parts.next()? != "artifacts"
    {
        return None;
    }
    let artifact_id = parts.next()?.parse().ok()?;
    parts.next()?;
    Some((owner_digest.to_string(), artifact_id))
}

async fn sweep_remote_artifact_namespaces(state: &AppState) -> anyhow::Result<u64> {
    let Some(root_provider) = state.artifact_object_store.as_ref() else {
        return Ok(0);
    };
    let namespaces = root_provider
        .list_relative_objects(MAX_REMOTE_OBJECT_GC_LISTING)
        .await
        .map_err(|error| anyhow!("list remote artifact objects: {error}"))?
        .into_iter()
        .filter_map(|object| remote_artifact_namespace(&object.relative_key))
        .collect::<HashSet<_>>();
    let mut removed = 0_u64;
    for (owner_digest, artifact_id) in namespaces {
        let local_exists = state
            .store
            .lock()
            .map_err(|_| anyhow!("store lock poisoned during remote GC"))?
            .artifact_exists_by_id(artifact_id)
            .map_err(|error| anyhow!("query local artifact GC guard: {error}"))?;
        let fleet_exists = match state.fleet.as_ref() {
            Some(fleet) => fleet
                .artifact_exists_by_id(artifact_id)
                .await
                .map_err(|error| anyhow!("fleet artifact GC guard: {error}"))?,
            None => false,
        };
        if local_exists || fleet_exists {
            continue;
        }
        let provider = root_provider
            .scoped(&format!("tenants/{owner_digest}/artifacts/{artifact_id}"))
            .map_err(|error| anyhow!("scope remote artifact GC namespace: {error}"))?;
        provider
            .mark_namespace_deleting()
            .await
            .map_err(|error| anyhow!("mark remote artifact deleting: {error}"))?;

        // The marker is visible before this authoritative second check. A
        // publisher inserts metadata before inspecting the marker, so either
        // this check observes the new artifact or that publisher fails closed.
        let local_exists = state
            .store
            .lock()
            .map_err(|_| anyhow!("store lock poisoned during remote GC"))?
            .artifact_exists_by_id(artifact_id)
            .map_err(|error| anyhow!("recheck local artifact GC guard: {error}"))?;
        let fleet_exists = match state.fleet.as_ref() {
            Some(fleet) => fleet
                .artifact_exists_by_id(artifact_id)
                .await
                .map_err(|error| anyhow!("fleet artifact GC recheck: {error}"))?,
            None => false,
        };
        if local_exists || fleet_exists {
            provider
                .clear_namespace_deleting()
                .await
                .map_err(|error| anyhow!("clear remote artifact deletion marker: {error}"))?;
            continue;
        }
        removed = removed.saturating_add(
            provider
                .delete_marked_namespace()
                .await
                .map_err(|error| anyhow!("delete remote artifact namespace: {error}"))?,
        );
    }
    Ok(removed)
}

fn spawn_artifact_repair(
    state: AppState,
    shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(fleet) = state.fleet.clone() else {
            return;
        };
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(shutdown_rx.clone()) => break,
                _ = interval.tick() => {}
            }
            if state.supervisor.disk_pressure_snapshot().pressured {
                continue;
            }
            let artifacts = match fleet.list_degraded_artifacts(8).await {
                Ok(artifacts) => artifacts,
                Err(error) => {
                    tracing::warn!(%error, "list degraded artifacts for repair failed");
                    continue;
                }
            };
            for artifact in artifacts {
                let lease_token = match fleet
                    .try_acquire_artifact_repair_lease(
                        artifact.artifact_id,
                        &state.config.host_id,
                        state.config.host_session_id,
                        &state.config.zone,
                        chrono::Utc::now() + chrono::Duration::seconds(30),
                    )
                    .await
                {
                    Ok(Some(token)) => token,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(artifact = %artifact.artifact_id, %error,
                            "artifact repair lease acquisition failed");
                        continue;
                    }
                };
                let renew_fleet = Arc::clone(&fleet);
                let renew_host = state.config.host_id.clone();
                let renew_session = state.config.host_session_id;
                let renew_artifact = artifact.artifact_id;
                let renewal = tokio::spawn(async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(10));
                    interval.tick().await;
                    loop {
                        interval.tick().await;
                        match renew_fleet
                            .renew_artifact_repair_lease(
                                renew_artifact,
                                &renew_host,
                                renew_session,
                                lease_token,
                                chrono::Utc::now() + chrono::Duration::seconds(30),
                            )
                            .await
                        {
                            Ok(true) => {}
                            Ok(false) | Err(_) => break,
                        }
                    }
                });
                let identity = config::ApiIdentity {
                    tenant: artifact.owner_key.clone(),
                    role: config::ApiRole::User,
                    max_vms: None,
                    api_key_id: format!("artifact-repair:{}", state.config.host_id),
                };
                let result =
                    ops::localize_branch_artifact(&state, &artifact, &identity, false).await;
                renewal.abort();
                let _ = renewal.await;
                if let Err(error) = fleet
                    .release_artifact_repair_lease(
                        artifact.artifact_id,
                        &state.config.host_id,
                        state.config.host_session_id,
                        lease_token,
                    )
                    .await
                {
                    tracing::warn!(artifact = %artifact.artifact_id, %error,
                        "artifact repair lease release failed");
                }
                match result {
                    Ok(_) => tracing::info!(artifact = %artifact.artifact_id,
                        "artifact replica repair published"),
                    Err(error) => tracing::warn!(artifact = %artifact.artifact_id, %error,
                        "artifact replica repair attempt failed"),
                }
                break;
            }
        }
    })
}

fn spawn_vm_exit_reconciler(
    state: AppState,
    shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(200));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(shutdown_rx.clone()) => break,
                _ = interval.tick() => {}
            }
            for failure in ops::reconcile_unexpected_vmm_exits(&state).await {
                tracing::error!(%failure, "unexpected VMM exit reconciliation failed");
            }
        }
    })
}

struct ServerListeners {
    control: tokio::net::TcpListener,
    peer: Option<tokio::net::TcpListener>,
    share: Option<tokio::net::TcpListener>,
    ssh: Option<tokio::net::TcpListener>,
}

async fn bind_server_listeners(config: &Config) -> anyhow::Result<ServerListeners> {
    let (control, share) = bind_http_listeners(config.listen, config.share_listen).await?;
    let ssh = match config.ssh_gateway_enabled {
        true => Some(
            tokio::net::TcpListener::bind(config.ssh_gateway_addr)
                .await
                .with_context(|| format!("bind SSH gateway {}", config.ssh_gateway_addr))?,
        ),
        false => None,
    };
    let peer = match config.peer_listen {
        Some(address) => Some(
            tokio::net::TcpListener::bind(address)
                .await
                .with_context(|| format!("bind peer listener {address}"))?,
        ),
        None => None,
    };
    Ok(ServerListeners {
        control,
        peer,
        share,
        ssh,
    })
}

struct BackgroundTasks {
    shutdown: ShutdownCoordinator,
    handles: Vec<JoinHandle<()>>,
    warm_pool: Option<warmpool::Replenisher>,
    autoscaler: Option<JoinHandle<()>>,
}

impl BackgroundTasks {
    fn new<const N: usize>(
        shutdown: ShutdownCoordinator,
        handles: [Option<JoinHandle<()>>; N],
        warm_pool: Option<warmpool::Replenisher>,
        autoscaler: Option<JoinHandle<()>>,
    ) -> Self {
        Self {
            shutdown,
            handles: handles.into_iter().flatten().collect(),
            warm_pool,
            autoscaler,
        }
    }

    async fn stop(self) {
        self.stop_with_timeout(BACKGROUND_DRAIN_TIMEOUT).await;
    }

    async fn stop_with_timeout(self, timeout: Duration) {
        self.shutdown.request("shutdown");
        if let Some(autoscaler) = self.autoscaler {
            await_quiescent_task("autoscaler", autoscaler, timeout).await;
        }
        if let Some(warm_pool) = self.warm_pool {
            warm_pool.quiesce(timeout).await;
        }

        let deadline = tokio::time::Instant::now() + timeout;
        let mut timed_out = Vec::new();
        for mut handle in self.handles {
            if tokio::time::timeout_at(deadline, &mut handle)
                .await
                .is_err()
            {
                timed_out.push(handle);
            }
        }
        for handle in &timed_out {
            handle.abort();
        }
        for handle in timed_out {
            let _ = handle.await;
        }
    }
}

async fn await_quiescent_task(
    name: &'static str,
    mut task: JoinHandle<()>,
    warning_after: Duration,
) {
    match tokio::time::timeout(warning_after, &mut task).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(task = name, %error, "background task failed while stopping")
        }
        Err(_) => {
            tracing::warn!(
                task = name,
                "background task is still quiescing after shutdown; waiting before sweep"
            );
            if let Err(error) = task.await {
                tracing::warn!(task = name, %error, "background task failed while stopping");
            }
        }
    }
}

fn spawn_shutdown_signal(
    shutdown: ShutdownCoordinator,
    shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            reason = shutdown_signal() => shutdown.request(reason),
            _ = wait_for_shutdown(shutdown_rx.clone()) => {}
        }
    })
}

type ServerHandle = tokio::task::JoinHandle<anyhow::Result<()>>;

fn server_routers(state: AppState) -> (axum::Router, axum::Router, axum::Router) {
    (
        router(state.clone()),
        internal::internal_router(state.clone()),
        share_gateway::router(state),
    )
}

async fn bind_http_listeners(
    control_addr: std::net::SocketAddr,
    share_addr: Option<std::net::SocketAddr>,
) -> anyhow::Result<(tokio::net::TcpListener, Option<tokio::net::TcpListener>)> {
    let control = tokio::net::TcpListener::bind(control_addr)
        .await
        .with_context(|| format!("bind {control_addr}"))?;
    let share = match share_addr {
        Some(share_addr) => Some(
            tokio::net::TcpListener::bind(share_addr)
                .await
                .with_context(|| format!("bind share {share_addr}"))?,
        ),
        None => None,
    };
    Ok((control, share))
}

fn spawn_http_server(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> ServerHandle {
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = wait_for_shutdown(shutdown_rx).await;
            })
            .await
            .context("HTTP server serve")
    })
}

fn spawn_peer_server(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    tls: Option<&config::PeerTlsConfig>,
    shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> anyhow::Result<ServerHandle> {
    let Some(tls) = tls else {
        return Ok(spawn_http_server(listener, app, shutdown_rx));
    };
    let acceptor = tokio_rustls::TlsAcceptor::from(peer_tls::server_config(tls)?);
    Ok(tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let accepted = tokio::select! {
                biased;
                _ = wait_for_shutdown(shutdown_rx.clone()) => break,
                accepted = listener.accept() => accepted,
            };
            let (stream, address) = accepted.context("accept peer TLS connection")?;
            let acceptor = acceptor.clone();
            let app = app.clone();
            let connection_shutdown = shutdown_rx.clone();
            connections.spawn(async move {
                let stream = match tokio::time::timeout(
                    Duration::from_secs(5),
                    acceptor.accept(stream),
                )
                .await
                {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(error)) => {
                        tracing::warn!(peer = %address, %error, "rejected peer TLS handshake");
                        return;
                    }
                    Err(_) => {
                        tracing::warn!(peer = %address, "peer TLS handshake timed out");
                        return;
                    }
                };
                let peer_certificate_sha256 = stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|certificates| certificates.first())
                    .map(peer_tls::certificate_sha256);
                let Some(peer_certificate_sha256) = peer_certificate_sha256 else {
                    tracing::warn!(peer = %address, "peer TLS connection has no authenticated leaf certificate");
                    return;
                };
                let app = app.layer(axum::Extension(
                    internal::VerifiedPeerCertificate(peer_certificate_sha256),
                ));
                let io = hyper_util::rt::TokioIo::new(stream);
                let service = hyper_util::service::TowerToHyperService::new(app);
                let builder = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                );
                let connection = builder.serve_connection_with_upgrades(io, service);
                tokio::pin!(connection);
                tokio::select! {
                    result = &mut connection => {
                        if let Err(error) = result {
                            tracing::debug!(peer = %address, %error, "peer TLS connection closed");
                        }
                    }
                    _ = wait_for_shutdown(connection_shutdown) => {
                        connection.as_mut().graceful_shutdown();
                        if let Err(error) = connection.await {
                            tracing::debug!(peer = %address, %error, "peer TLS connection drain failed");
                        }
                    }
                }
            });
        }
        while connections.join_next().await.is_some() {}
        Ok(())
    }))
}

fn spawn_ssh_server(listener: tokio::net::TcpListener, state: AppState) -> ServerHandle {
    tokio::spawn(async move { gateway::run(state, listener).await })
}

struct LifecycleOutcome {
    reason: &'static str,
    error: Option<anyhow::Error>,
}

impl LifecycleOutcome {
    fn normal(reason: &'static str) -> Self {
        Self {
            reason,
            error: None,
        }
    }

    fn failed(reason: &'static str, error: anyhow::Error) -> Self {
        Self {
            reason,
            error: Some(error),
        }
    }
}

async fn finalize_lifecycle<
    StopBridges,
    StopBridgesFuture,
    StopWorkers,
    StopWorkersFuture,
    Sweep,
    SweepFuture,
>(
    outcome: LifecycleOutcome,
    stop_bridges: StopBridges,
    stop_workers: StopWorkers,
    sweep: Sweep,
) -> anyhow::Result<()>
where
    StopBridges: FnOnce() -> StopBridgesFuture,
    StopBridgesFuture: Future<Output = ()>,
    StopWorkers: FnOnce() -> StopWorkersFuture,
    StopWorkersFuture: Future<Output = ()>,
    Sweep: FnOnce(&'static str) -> SweepFuture,
    SweepFuture: Future<Output = anyhow::Result<()>>,
{
    stop_bridges().await;
    stop_workers().await;
    let sweep_result = sweep(outcome.reason).await;
    match outcome.error {
        Some(error) => Err(error),
        None => sweep_result,
    }
}

enum ServerEvent {
    Shutdown(&'static str),
    Control(Result<anyhow::Result<()>, tokio::task::JoinError>),
    Peer(Result<anyhow::Result<()>, tokio::task::JoinError>),
    Share(Result<anyhow::Result<()>, tokio::task::JoinError>),
    Ssh(Result<anyhow::Result<()>, tokio::task::JoinError>),
}

async fn supervise_servers(
    mut control: ServerHandle,
    mut peer: Option<ServerHandle>,
    mut share: Option<ServerHandle>,
    mut ssh: Option<ServerHandle>,
    shutdown: ShutdownCoordinator,
    shutdown_rx: watch::Receiver<Option<&'static str>>,
    drain_timeout: Duration,
) -> LifecycleOutcome {
    let event = match (peer.as_mut(), share.as_mut(), ssh.as_mut()) {
        (Some(peer), Some(share), Some(ssh)) => {
            tokio::select! {
                biased;
                reason = wait_for_shutdown(shutdown_rx.clone()) => ServerEvent::Shutdown(reason),
                result = &mut control => ServerEvent::Control(result),
                result = &mut *peer => ServerEvent::Peer(result),
                result = &mut *share => ServerEvent::Share(result),
                result = &mut *ssh => ServerEvent::Ssh(result),
            }
        }
        (Some(peer), Some(share), None) => {
            tokio::select! {
                biased;
                reason = wait_for_shutdown(shutdown_rx.clone()) => ServerEvent::Shutdown(reason),
                result = &mut control => ServerEvent::Control(result),
                result = &mut *peer => ServerEvent::Peer(result),
                result = &mut *share => ServerEvent::Share(result),
            }
        }
        (Some(peer), None, Some(ssh)) => {
            tokio::select! {
                biased;
                reason = wait_for_shutdown(shutdown_rx.clone()) => ServerEvent::Shutdown(reason),
                result = &mut control => ServerEvent::Control(result),
                result = &mut *peer => ServerEvent::Peer(result),
                result = &mut *ssh => ServerEvent::Ssh(result),
            }
        }
        (Some(peer), None, None) => {
            tokio::select! {
                biased;
                reason = wait_for_shutdown(shutdown_rx.clone()) => ServerEvent::Shutdown(reason),
                result = &mut control => ServerEvent::Control(result),
                result = &mut *peer => ServerEvent::Peer(result),
            }
        }
        (None, Some(share), Some(ssh)) => {
            tokio::select! {
                biased;
                reason = wait_for_shutdown(shutdown_rx.clone()) => ServerEvent::Shutdown(reason),
                result = &mut control => ServerEvent::Control(result),
                result = &mut *share => ServerEvent::Share(result),
                result = &mut *ssh => ServerEvent::Ssh(result),
            }
        }
        (None, Some(share), None) => {
            tokio::select! {
                biased;
                reason = wait_for_shutdown(shutdown_rx.clone()) => ServerEvent::Shutdown(reason),
                result = &mut control => ServerEvent::Control(result),
                result = &mut *share => ServerEvent::Share(result),
            }
        }
        (None, None, Some(ssh)) => {
            tokio::select! {
                biased;
                reason = wait_for_shutdown(shutdown_rx.clone()) => ServerEvent::Shutdown(reason),
                result = &mut control => ServerEvent::Control(result),
                result = &mut *ssh => ServerEvent::Ssh(result),
            }
        }
        (None, None, None) => {
            tokio::select! {
                biased;
                reason = wait_for_shutdown(shutdown_rx.clone()) => ServerEvent::Shutdown(reason),
                result = &mut control => ServerEvent::Control(result),
            }
        }
    };

    // Close VM admission at the lifecycle edge, before classifying a failed
    // task or giving any listener a drain timeout.
    shutdown.close_admission();

    let mut control_exited = false;
    let mut peer_exited = false;
    let mut share_exited = false;
    let mut ssh_exited = false;
    let mut first_error = None;
    let reason = match event {
        ServerEvent::Shutdown(reason) => reason,
        ServerEvent::Control(result) => {
            control_exited = true;
            classify_server_exit(
                "control",
                result,
                shutdown_rx.borrow().is_some(),
                &mut first_error,
            );
            shutdown_after_server_exit(&shutdown, &shutdown_rx, &first_error)
        }
        ServerEvent::Peer(result) => {
            peer_exited = true;
            classify_server_exit(
                "peer",
                result,
                shutdown_rx.borrow().is_some(),
                &mut first_error,
            );
            shutdown_after_server_exit(&shutdown, &shutdown_rx, &first_error)
        }
        ServerEvent::Share(result) => {
            share_exited = true;
            classify_server_exit(
                "share",
                result,
                shutdown_rx.borrow().is_some(),
                &mut first_error,
            );
            shutdown_after_server_exit(&shutdown, &shutdown_rx, &first_error)
        }
        ServerEvent::Ssh(result) => {
            ssh_exited = true;
            classify_server_exit(
                "SSH gateway",
                result,
                shutdown_rx.borrow().is_some(),
                &mut first_error,
            );
            shutdown_after_server_exit(&shutdown, &shutdown_rx, &first_error)
        }
    };

    tracing::info!(
        reason,
        drain_timeout_secs = drain_timeout.as_secs(),
        "shutdown signal received; draining HTTP listeners"
    );
    let deadline = tokio::time::Instant::now() + drain_timeout;
    if !control_exited {
        record_first_error(
            &mut first_error,
            drain_server("control", &mut control, deadline).await,
        );
    }
    if !peer_exited {
        if let Some(peer) = peer.as_mut() {
            record_first_error(&mut first_error, drain_server("peer", peer, deadline).await);
        }
    }
    if !share_exited {
        if let Some(share) = share.as_mut() {
            record_first_error(
                &mut first_error,
                drain_server("share", share, deadline).await,
            );
        }
    }
    if !ssh_exited {
        if let Some(ssh) = ssh.as_mut() {
            abort_server(ssh).await;
        }
    }

    match first_error {
        Some(error) => LifecycleOutcome::failed(reason, error),
        None => LifecycleOutcome::normal(reason),
    }
}

fn shutdown_after_server_exit(
    shutdown: &ShutdownCoordinator,
    shutdown_rx: &watch::Receiver<Option<&'static str>>,
    first_error: &Option<anyhow::Error>,
) -> &'static str {
    if first_error.is_some() {
        shutdown.request("server error");
    }
    shutdown_rx.borrow().as_ref().copied().unwrap_or("shutdown")
}

fn classify_server_exit(
    name: &str,
    result: Result<anyhow::Result<()>, tokio::task::JoinError>,
    shutdown_requested: bool,
    first_error: &mut Option<anyhow::Error>,
) {
    match server_result(name, result) {
        Ok(()) if !shutdown_requested => {
            record_first_error(
                first_error,
                Err(anyhow::anyhow!("{name} server exited unexpectedly")),
            );
        }
        Ok(()) => {}
        Err(error) => record_first_error(
            first_error,
            Err(error.context(format!("{name} server exited unexpectedly"))),
        ),
    }
}

fn record_first_error(first_error: &mut Option<anyhow::Error>, result: anyhow::Result<()>) {
    if let Err(error) = result {
        if first_error.is_none() {
            *first_error = Some(error);
        } else {
            tracing::error!(error = %error, "additional shutdown error");
        }
    }
}
async fn wait_for_shutdown(mut rx: watch::Receiver<Option<&'static str>>) -> &'static str {
    loop {
        if let Some(reason) = *rx.borrow() {
            return reason;
        }
        if rx.changed().await.is_err() {
            return "shutdown";
        }
    }
}

fn server_result(
    name: &str,
    result: Result<anyhow::Result<()>, tokio::task::JoinError>,
) -> anyhow::Result<()> {
    result.map_err(|error| anyhow::anyhow!("{name} server task panicked: {error}"))?
}

async fn drain_server(
    name: &str,
    server: &mut ServerHandle,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    match tokio::time::timeout_at(deadline, &mut *server).await {
        Ok(result) => server_result(name, result),
        Err(_) => {
            tracing::warn!(
                server = name,
                "HTTP drain timed out; aborting remaining connections"
            );
            server.abort();
            let _ = server.await;
            Ok(())
        }
    }
}

async fn abort_server(server: &mut ServerHandle) {
    server.abort();
    let _ = server.await;
}

#[cfg(unix)]
async fn shutdown_signal() -> &'static str {
    let sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    let Ok(mut sigterm) = sigterm else {
        tracing::warn!("failed to install SIGTERM handler; falling back to SIGINT only");
        let _ = tokio::signal::ctrl_c().await;
        return "SIGINT";
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = sigterm.recv() => "SIGTERM",
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "SIGINT"
}

async fn shutdown_sweep(state: &AppState, reason: &'static str) -> anyhow::Result<()> {
    let started = Instant::now();
    if !state.config.reap_on_shutdown {
        tracing::warn!(
            reason = reason,
            "shutdown drain summary: local VM reaping disabled by TARIT_REAP_ON_SHUTDOWN"
        );
        return Ok(());
    }

    let summary = ops::stop_all_local(state)
        .await
        .map_err(|e| anyhow::anyhow!("shutdown sweep failed: {e}"))?;
    tracing::info!(
        reason = reason,
        reaped_total = summary.total(),
        running = summary.running,
        warm = summary.warm,
        booting = summary.booting,
        internal_booting = summary.internal_booting,
        elapsed_ms = started.elapsed().as_millis(),
        "shutdown drain summary: reaped local VMs"
    );
    Ok(())
}

fn spawn_fleet_sync(
    fleet: Arc<PostgresFleet>,
    store: Arc<Mutex<Store>>,
    config: Config,
    peer_certificate_sha256: Option<String>,
    scheduler: Arc<Scheduler>,
    shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(shutdown_rx.clone()) => break,
                _ = interval.tick() => {}
            }
            let cap = scheduler.local_capacity(1, 256);
            let host = tarit_fleet::host_record_from_capacity(
                &config.host_id,
                config.host_session_id,
                peer_certificate_sha256.clone(),
                Some(config.rpc_addr.clone()),
                cap.sandbox_count,
                cap.free_vcpus,
                cap.free_memory_mib,
            );
            if fleet.upsert_host(&host).await.is_err() {
                tracing::warn!("fleet heartbeat failed");
                continue;
            }
            if let Err(error) = fleet
                .refresh_artifact_replication_health(
                    chrono::Utc::now() - chrono::Duration::seconds(15),
                )
                .await
            {
                tracing::warn!(%error, "fleet artifact health reconciliation failed");
            }
            match fleet.list_hosts().await {
                Ok(hosts) => {
                    if let Ok(guard) = store.lock() {
                        for host in hosts {
                            let _ = guard.upsert_host(&host);
                        }
                    }
                }
                Err(e) => tracing::warn!("fleet peer sync failed: {e}"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header::HOST, Request, StatusCode},
    };
    use std::path::{Path, PathBuf};
    #[cfg(target_os = "linux")]
    use std::{
        io::{Read, Write},
        os::unix::ffi::OsStrExt,
        process::{Command, Stdio},
        thread,
    };
    use tokio::net::TcpListener;
    use tower::ServiceExt;

    #[tokio::test]
    async fn peer_tls_listener_serves_trusted_client_and_rejects_missing_certificate() {
        let pki = peer_tls::tests::test_pki();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (shutdown_tx, shutdown_rx) = watch::channel(None);
        let app = axum::Router::new().route(
            "/probe",
            axum::routing::get(|| async { StatusCode::NO_CONTENT }),
        );
        let server = spawn_peer_server(listener, app, Some(&pki.server), shutdown_rx).unwrap();

        let mut authenticated_builder = reqwest::Client::builder()
            .no_proxy()
            .tls_built_in_root_certs(false)
            .identity(peer_tls::reqwest_identity(&pki.client).unwrap());
        let mut unauthenticated_builder = reqwest::Client::builder()
            .no_proxy()
            .tls_built_in_root_certs(false);
        for root in peer_tls::reqwest_roots(&pki.client).unwrap() {
            authenticated_builder = authenticated_builder.add_root_certificate(root.clone());
            unauthenticated_builder = unauthenticated_builder.add_root_certificate(root);
        }
        let authenticated = authenticated_builder.build().unwrap();
        let unauthenticated = unauthenticated_builder.build().unwrap();
        let url = format!("https://localhost:{port}/probe");

        assert_eq!(
            authenticated.get(&url).send().await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        assert!(unauthenticated.get(&url).send().await.is_err());

        shutdown_tx.send(Some("test")).unwrap();
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("peer TLS listener drains")
            .unwrap()
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    fn short_test_root(prefix: &str) -> PathBuf {
        let suffix = Uuid::new_v4().simple().to_string();
        PathBuf::from("target/t").join(format!("{prefix}-{suffix}"))
    }

    fn test_shutdown(tx: watch::Sender<Option<&'static str>>) -> ShutdownCoordinator {
        ShutdownCoordinator::new(tx, Arc::new(VmmSupervisor::new(test_config())))
    }

    #[test]
    fn artifact_gc_uses_persisted_runtime_layout_paths() {
        let now = chrono::Utc::now();
        let persisted_overlay = PathBuf::from("/old-layout/overlays/vm.cow");
        let persisted_jail = PathBuf::from("/old-layout/jails/vm");
        let vm = tarit_types::VmRecord {
            id: Uuid::new_v4(),
            host_id: "host-a".into(),
            owner_key: None,
            api_key_id: None,
            status: VmStatus::Running,
            revision: 1,
            startup_path: None,
            memory_mib: 256,
            vcpus: 1,
            kernel_path: "kernel".into(),
            rootfs_path: Some("rootfs".into()),
            rootfs_read_only: true,
            cmdline: "console=ttyS0".into(),
            runtime_layout: Some(tarit_types::VmRuntimeLayout {
                overlay_path: Some(persisted_overlay.display().to_string()),
                jail_path: Some(persisted_jail.display().to_string()),
                artifact_paths: vec!["/old-layout/control.sock".into()],
            }),
            socket_path: Some("/old-layout/control.sock".into()),
            pid: Some(42),
            created_at: now,
            updated_at: now,
        };

        let references = artifact_references(&[vm], &[]);
        assert!(references.runtime_paths.contains(&persisted_overlay));
        assert!(references.runtime_paths.contains(&persisted_jail));
        assert!(references
            .runtime_paths
            .contains(Path::new("/old-layout/control.sock")));
    }

    fn legacy_active_record(
        config: &Config,
        id: Uuid,
        socket_path: PathBuf,
        pid: Option<u32>,
    ) -> tarit_types::VmRecord {
        let now = chrono::Utc::now();
        tarit_types::VmRecord {
            id,
            host_id: config.host_id.clone(),
            owner_key: Some("tenant-a".into()),
            api_key_id: Some("test-key".into()),
            status: VmStatus::Running,
            revision: 7,
            startup_path: None,
            memory_mib: 256,
            vcpus: 1,
            kernel_path: config.kernel.display().to_string(),
            rootfs_path: Some(config.rootfs.display().to_string()),
            rootfs_read_only: true,
            cmdline: supervisor::DEFAULT_CMDLINE.into(),
            runtime_layout: None,
            socket_path: Some(socket_path.display().to_string()),
            pid,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn ambiguous_legacy_runtime_layout_requires_drain() {
        let config = test_config();
        let id = Uuid::new_v4();
        let mut records = vec![legacy_active_record(
            &config,
            id,
            config.socket_dir.join("not-the-vm-id.sock"),
            None,
        )];
        let store = Store::open(":memory:").unwrap();
        store.insert_vm(&records[0]).unwrap();

        let error = backfill_legacy_runtime_layouts(&config, &store, &mut records)
            .expect_err("ambiguous legacy layout must block startup");
        let error = format!("{error:#}");
        assert!(error.contains("drain required before upgrade"));
        assert!(error.contains("legacy UUID-scoped socket"));
        assert_eq!(store.get_vm(id).unwrap().runtime_layout, None);
    }

    #[test]
    fn identity_less_legacy_creating_is_fenced_before_layout_backfill() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/legacy-creating-upgrade-{}", Uuid::new_v4()));
        let mut config = test_config();
        config.vmm_bin = std::env::current_exe().unwrap();
        config.socket_dir = root.join("sockets");
        config.db_path = root.join("fleet.db");
        config.net_state_path = root.join("net-state.json");
        config.images_dir = root.join("images");
        let supervisor = VmmSupervisor::new(config.clone());
        let id = Uuid::new_v4();
        let socket_path = config.socket_dir.join(format!("{id}.sock"));
        let overlay_path = config.socket_dir.join("overlays").join(format!("{id}.cow"));
        std::fs::write(&socket_path, b"stale socket artifact").unwrap();
        std::fs::write(&overlay_path, b"stale overlay artifact").unwrap();
        let mut record = legacy_active_record(&config, id, socket_path.clone(), None);
        record.status = VmStatus::Creating;
        record.socket_path = None;
        let store = Store::open(":memory:").unwrap();
        store.insert_vm(&record).unwrap();
        let mut records = vec![record];

        reconcile_legacy_creating_records(&config, &store, &supervisor, &mut records).unwrap();
        backfill_legacy_runtime_layouts(&config, &store, &mut records).unwrap();

        let durable = store.get_vm(id).unwrap();
        assert_eq!(durable.status, VmStatus::Error);
        assert_eq!(durable.revision, 9);
        assert_eq!(durable.runtime_layout, None);
        assert!(!socket_path.exists());
        assert!(!overlay_path.exists());
        drop(supervisor);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn identity_less_legacy_creating_is_excluded_from_live_recovery() {
        let config = test_config();
        let id = Uuid::new_v4();
        let mut record = legacy_active_record(
            &config,
            id,
            config.socket_dir.join(format!("{id}.sock")),
            None,
        );
        record.status = VmStatus::Creating;
        record.socket_path = None;

        assert!(is_identityless_legacy_creating(&config, &record));
        record.host_id = "other-host".into();
        assert!(!is_identityless_legacy_creating(&config, &record));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_less_legacy_creating_terminates_discovered_owned_runtime() {
        let root = short_test_root("lcr");
        let mut config = test_config();
        config.vmm_bin = PathBuf::from("sh");
        config.socket_dir = root.join("sockets");
        config.db_path = root.join("fleet.db");
        config.net_state_path = root.join("net-state.json");
        config.images_dir = root.join("images");
        let supervisor = VmmSupervisor::new(config.clone());
        let id = Uuid::new_v4();
        let socket_path = config.socket_dir.join(format!("{id}.sock"));
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("read _line")
            .arg("tarit-vmm")
            .arg("serve")
            .arg("--socket")
            .arg(&socket_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut published = false;
        for _ in 0..200 {
            published =
                std::fs::read(format!("/proc/{}/cmdline", child.id())).is_ok_and(|cmdline| {
                    cmdline
                        .windows(socket_path.as_os_str().as_bytes().len())
                        .any(|window| window == socket_path.as_os_str().as_bytes())
                });
            if published {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(published, "legacy VMM stand-in did not publish its argv");
        let mut record = legacy_active_record(&config, id, socket_path.clone(), None);
        record.status = VmStatus::Creating;
        record.socket_path = None;
        let store = Store::open(":memory:").unwrap();
        store.insert_vm(&record).unwrap();
        let mut records = vec![record];

        reconcile_legacy_creating_records(&config, &store, &supervisor, &mut records).unwrap();

        child.wait().unwrap();
        assert_eq!(store.get_vm(id).unwrap().status, VmStatus::Error);
        assert!(!socket_path.exists());
        drop(listener);
        drop(supervisor);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_active_runtime_is_persisted_before_adoption() {
        let root = short_test_root("lla");
        let mut config = test_config();
        config.vmm_bin = PathBuf::from("sh");
        config.socket_dir = root.join("sockets");
        config.db_path = root.join("fleet.db");
        config.net_state_path = root.join("net-state.json");
        config.images_dir = root.join("images");
        config.kernel = root.join("kernel");
        config.rootfs = root.join("rootfs");
        std::fs::create_dir_all(config.socket_dir.join("overlays")).unwrap();

        let id = Uuid::new_v4();
        let socket_path = config.socket_dir.join(format!("{id}.sock"));
        let overlay_path = config.socket_dir.join("overlays").join(format!("{id}.cow"));
        std::fs::write(&overlay_path, b"legacy overlay").unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut length = [0_u8; 4];
            stream.read_exact(&mut length).unwrap();
            let mut body = vec![0; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut body).unwrap();
            let request: tarit_vmm_client::ApiRequest = serde_json::from_slice(&body).unwrap();
            assert!(matches!(request, tarit_vmm_client::ApiRequest::Status));
            let response = tarit_vmm_client::ApiResponse::Status(tarit_vmm_client::VmStatus {
                state: tarit_vmm_client::VmState::Paused,
                uptime_ms: 1,
                vcpus: 1,
                mem_mib: 256,
                volumes: 0,
                nets: 0,
                kernel: "kernel".into(),
                vcpu_alive: true,
            });
            let encoded = serde_json::to_vec(&response).unwrap();
            stream
                .write_all(&(encoded.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&encoded).unwrap();
            stream.flush().unwrap();
        });
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("read _line")
            .arg("tarit-vmm")
            .arg("serve")
            .arg("--socket")
            .arg(&socket_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut published = false;
        for _ in 0..200 {
            published =
                std::fs::read(format!("/proc/{}/cmdline", child.id())).is_ok_and(|cmdline| {
                    cmdline
                        .windows(socket_path.as_os_str().as_bytes().len())
                        .any(|window| window == socket_path.as_os_str().as_bytes())
                });
            if published {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(published, "legacy VMM stand-in did not publish its argv");

        let store = Store::open(":memory:").unwrap();
        let mut records = vec![legacy_active_record(
            &config,
            id,
            socket_path.clone(),
            Some(child.id()),
        )];
        store.insert_vm(&records[0]).unwrap();
        backfill_legacy_runtime_layouts(&config, &store, &mut records).unwrap();

        let durable = store.get_vm(id).unwrap();
        assert_eq!(durable.revision, 8);
        assert_eq!(
            durable.runtime_layout,
            Some(tarit_types::VmRuntimeLayout {
                overlay_path: Some(overlay_path.display().to_string()),
                jail_path: None,
                artifact_paths: vec![
                    socket_path.display().to_string(),
                    overlay_path.display().to_string(),
                ],
            })
        );

        let supervisor = Arc::new(VmmSupervisor::new(config));
        let warnings = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(supervisor.readopt_running_vms(&mut records))
            .unwrap();
        server.join().unwrap();
        assert!(warnings.is_empty());
        assert_eq!(records[0].status, VmStatus::Paused);
        assert_eq!(records[0].revision, 10);
        supervisor.stop_vm(id).unwrap();
        child.wait().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn share_bind_failure_releases_the_unserved_control_listener() {
        let occupied_share_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let share_addr = occupied_share_listener.local_addr().unwrap();
        let reserved_control_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_addr = reserved_control_listener.local_addr().unwrap();
        drop(reserved_control_listener);

        let error = bind_http_listeners(control_addr, Some(share_addr))
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains(&format!("bind share {share_addr}")));
        assert!(
            TcpListener::bind(control_addr).await.is_ok(),
            "a failed share bind must release the not-yet-served control listener"
        );
    }

    #[tokio::test]
    async fn share_bind_failure_has_no_worker_or_sweep_side_effects() {
        let occupied_share_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let share_addr = occupied_share_listener.local_addr().unwrap();
        let reserved_control_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_addr = reserved_control_listener.local_addr().unwrap();
        drop(reserved_control_listener);

        let root = PathBuf::from(format!("target/taritd-bind-failure-{}", Uuid::new_v4()));
        let mut config = test_config();
        config.listen = control_addr;
        config.share_listen = Some(share_addr);
        config.ssh_gateway_enabled = true;
        config.socket_dir = root.join("sockets");
        config.images_dir = root.join("images");
        config.db_path = root.join("fleet.db");
        config.net_state_path = root.join("net-state.json");
        config.ssh_gateway_host_key_path = root.join("ssh-host");

        let error = run_server(config, Vec::new(), PtyConnectionLimits::default())
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains(&format!("bind share {share_addr}")));
        assert!(
            TcpListener::bind(control_addr).await.is_ok(),
            "control listener must be released when share binding fails"
        );
        assert!(
            !root.exists(),
            "binding failure must precede store, worker, SSH-key, and sweep side effects"
        );
    }

    #[tokio::test]
    async fn ssh_bind_failure_releases_the_http_listeners() {
        let occupied_ssh_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ssh_addr = occupied_ssh_listener.local_addr().unwrap();
        let reserved_control_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_addr = reserved_control_listener.local_addr().unwrap();
        let reserved_share_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let share_addr = reserved_share_listener.local_addr().unwrap();
        drop((reserved_control_listener, reserved_share_listener));

        let mut config = test_config();
        config.listen = control_addr;
        config.share_listen = Some(share_addr);
        config.ssh_gateway_enabled = true;
        config.ssh_gateway_addr = ssh_addr;

        let error = match bind_server_listeners(&config).await {
            Ok(_) => panic!("SSH bind should fail"),
            Err(error) => error,
        };

        assert!(error
            .to_string()
            .contains(&format!("bind SSH gateway {ssh_addr}")));
        assert!(TcpListener::bind(control_addr).await.is_ok());
        assert!(TcpListener::bind(share_addr).await.is_ok());
    }

    #[test]
    fn server_routers_keep_control_peer_and_share_routes_separate() {
        let (control, peer, share) = server_routers(test_state());
        let share_host = "calm-red-fox.shares.example.com";
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let control_test = control.clone();
        let peer_test = peer.clone();
        let share_test = share.clone();

        runtime.block_on(async move {
            let control_response = control_test
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/")
                        .header(HOST, share_host)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            // A share-style request pointed at the control listener hits the
            // control router's not-found fallback, not a share handler.
            assert_eq!(control_response.status(), StatusCode::NOT_FOUND);

            let public_internal_response = control_test
                .oneshot(
                    Request::builder()
                        .uri("/internal/v1/vms")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(public_internal_response.status(), StatusCode::NOT_FOUND);

            let peer_public_response = peer_test
                .oneshot(
                    Request::builder()
                        .uri("/health")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            // Peer authentication is applied before routing, so an unsigned
            // public-looking request is rejected without revealing whether a
            // path exists on the internal listener.
            assert_eq!(peer_public_response.status(), StatusCode::UNAUTHORIZED);

            let share_response = share_test
                .oneshot(
                    Request::builder()
                        .uri("/health")
                        .header(HOST, share_host)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(share_response.status(), StatusCode::NOT_FOUND);
        });
        drop(control);
        drop(peer);
        drop(share);
        drop(runtime);
    }

    type Events = Arc<Mutex<Vec<&'static str>>>;

    fn event(events: &Events, value: &'static str) {
        events.lock().unwrap().push(value);
    }

    async fn shutdown_server(
        shutdown_rx: watch::Receiver<Option<&'static str>>,
        events: Events,
        name: &'static str,
    ) -> anyhow::Result<()> {
        wait_for_shutdown(shutdown_rx).await;
        event(&events, name);
        Ok(())
    }

    #[tokio::test]
    async fn normal_shutdown_closes_vm_admission_before_draining_servers() {
        let (shutdown_tx, shutdown_rx) = watch::channel(None::<&'static str>);
        let supervisor = Arc::new(VmmSupervisor::new(test_config()));
        let shutdown = ShutdownCoordinator::new(shutdown_tx.clone(), Arc::clone(&supervisor));
        let drain_supervisor = Arc::clone(&supervisor);
        let control_rx = shutdown_rx.clone();
        let control = tokio::spawn(async move {
            wait_for_shutdown(control_rx).await;
            assert!(
                drain_supervisor.admission_is_closed(),
                "normal shutdown must close admission before server drain"
            );
            Ok(())
        });
        let share = tokio::spawn(async move {
            wait_for_shutdown(shutdown_rx).await;
            Ok(())
        });

        shutdown_tx.send(Some("test")).unwrap();
        let outcome = supervise_servers(
            control,
            None,
            Some(share),
            None,
            shutdown,
            shutdown_tx.subscribe(),
            Duration::from_secs(1),
        )
        .await;

        assert!(outcome.error.is_none());
    }

    #[tokio::test]
    async fn fatal_server_exit_closes_vm_admission_before_sibling_drain() {
        let (shutdown_tx, shutdown_rx) = watch::channel(None::<&'static str>);
        let supervisor = Arc::new(VmmSupervisor::new(test_config()));
        let shutdown = ShutdownCoordinator::new(shutdown_tx.clone(), Arc::clone(&supervisor));
        let drain_supervisor = Arc::clone(&supervisor);
        let control = tokio::spawn(async { Err(anyhow::anyhow!("control failed")) });
        let share = tokio::spawn(async move {
            wait_for_shutdown(shutdown_rx).await;
            assert!(
                drain_supervisor.admission_is_closed(),
                "fatal server exit must close admission before sibling drain"
            );
            Ok(())
        });

        let outcome = supervise_servers(
            control,
            None,
            Some(share),
            None,
            shutdown,
            shutdown_tx.subscribe(),
            Duration::from_secs(1),
        )
        .await;

        assert!(outcome.error.is_some());
    }

    async fn finish_for_test(outcome: LifecycleOutcome, events: Events) -> anyhow::Result<()> {
        let stopped = Arc::clone(&events);
        let swept = Arc::clone(&events);
        finalize_lifecycle(
            outcome,
            || async {},
            move || async move {
                event(&stopped, "workers");
            },
            move |_| async move {
                event(&swept, "sweep");
                Ok(())
            },
        )
        .await
    }

    #[tokio::test]
    async fn unexpected_control_exit_drains_share_then_sweeps() {
        let (shutdown_tx, shutdown_rx) = watch::channel(None::<&'static str>);
        let events = Arc::new(Mutex::new(Vec::new()));
        let control = tokio::spawn(async { Err(anyhow::anyhow!("control failed")) });
        let share = tokio::spawn(shutdown_server(
            shutdown_rx.clone(),
            Arc::clone(&events),
            "share",
        ));

        let outcome = supervise_servers(
            control,
            None,
            Some(share),
            None,
            test_shutdown(shutdown_tx),
            shutdown_rx,
            Duration::from_secs(1),
        )
        .await;

        assert!(outcome
            .error
            .as_ref()
            .unwrap()
            .to_string()
            .contains("control server exited unexpectedly"));
        let error = finish_for_test(outcome, Arc::clone(&events))
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("control server exited unexpectedly"));
        assert_eq!(*events.lock().unwrap(), ["share", "workers", "sweep"]);
    }

    #[tokio::test]
    async fn unexpected_share_exit_drains_control_then_sweeps() {
        let (shutdown_tx, shutdown_rx) = watch::channel(None::<&'static str>);
        let events = Arc::new(Mutex::new(Vec::new()));
        let control = tokio::spawn(shutdown_server(
            shutdown_rx.clone(),
            Arc::clone(&events),
            "control",
        ));
        let share = tokio::spawn(async { Err(anyhow::anyhow!("share failed")) });

        let outcome = supervise_servers(
            control,
            None,
            Some(share),
            None,
            test_shutdown(shutdown_tx),
            shutdown_rx,
            Duration::from_secs(1),
        )
        .await;

        assert!(outcome
            .error
            .as_ref()
            .unwrap()
            .to_string()
            .contains("share server exited unexpectedly"));
        finish_for_test(outcome, Arc::clone(&events))
            .await
            .unwrap_err();
        assert_eq!(*events.lock().unwrap(), ["control", "workers", "sweep"]);
    }

    #[tokio::test]
    async fn drain_failure_still_awaits_sibling_and_runs_sweep() {
        let (shutdown_tx, shutdown_rx) = watch::channel(Some("test"));
        let events = Arc::new(Mutex::new(Vec::new()));
        let control = tokio::spawn(async { Err(anyhow::anyhow!("control drain failure")) });
        let share = tokio::spawn(shutdown_server(
            shutdown_rx.clone(),
            Arc::clone(&events),
            "share",
        ));

        let outcome = supervise_servers(
            control,
            None,
            Some(share),
            None,
            test_shutdown(shutdown_tx),
            shutdown_rx,
            Duration::from_secs(1),
        )
        .await;

        let error = finish_for_test(outcome, Arc::clone(&events))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("control drain failure"));
        assert_eq!(*events.lock().unwrap(), ["share", "workers", "sweep"]);
    }

    #[tokio::test]
    async fn first_server_error_is_preserved_after_other_drains() {
        let (shutdown_tx, shutdown_rx) = watch::channel(Some("test"));
        let events = Arc::new(Mutex::new(Vec::new()));
        let control = tokio::spawn(async { Err(anyhow::anyhow!("first control failure")) });
        let share = tokio::spawn(async { Err(anyhow::anyhow!("second share failure")) });

        let outcome = supervise_servers(
            control,
            None,
            Some(share),
            None,
            test_shutdown(shutdown_tx),
            shutdown_rx,
            Duration::from_secs(1),
        )
        .await;

        let error = finish_for_test(outcome, events).await.unwrap_err();
        assert!(error.to_string().contains("first control failure"));
    }

    #[tokio::test]
    async fn normal_shutdown_drains_servers_stops_workers_then_sweeps_once() {
        let (shutdown_tx, shutdown_rx) = watch::channel(Some("test"));
        let events = Arc::new(Mutex::new(Vec::new()));
        let control = tokio::spawn(shutdown_server(
            shutdown_rx.clone(),
            Arc::clone(&events),
            "control",
        ));
        let share = tokio::spawn(shutdown_server(
            shutdown_rx.clone(),
            Arc::clone(&events),
            "share",
        ));

        let outcome = supervise_servers(
            control,
            None,
            Some(share),
            None,
            test_shutdown(shutdown_tx),
            shutdown_rx,
            Duration::from_secs(1),
        )
        .await;

        finish_for_test(outcome, Arc::clone(&events)).await.unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            ["control", "share", "workers", "sweep"]
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| **event == "sweep")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn lifecycle_stops_bridges_before_workers_and_the_single_sweep() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let bridges = Arc::clone(&events);
        let workers = Arc::clone(&events);
        let sweep = Arc::clone(&events);

        finalize_lifecycle(
            LifecycleOutcome::normal("test"),
            move || async move {
                event(&bridges, "bridges");
            },
            move || async move {
                event(&workers, "workers");
            },
            move |_| async move {
                event(&sweep, "sweep");
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(*events.lock().unwrap(), ["bridges", "workers", "sweep"]);
    }

    #[tokio::test]
    async fn background_stop_aborts_and_awaits_a_stuck_worker_before_returning() {
        struct Notifier(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for Notifier {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }

        let (shutdown_tx, _shutdown_rx) = watch::channel(None);
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _notifier = Notifier(Some(dropped_tx));
            std::future::pending::<()>().await;
        });

        BackgroundTasks::new(test_shutdown(shutdown_tx), [Some(task)], None, None)
            .stop_with_timeout(Duration::from_millis(5))
            .await;

        dropped_rx.await.unwrap();
    }

    #[tokio::test]
    async fn held_warm_blocking_work_quiesces_before_sweep_without_creating_a_vm() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        };

        let (shutdown_tx, shutdown_rx) = watch::channel(None);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let created = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_created = Arc::clone(&created);
        let warm_worker = tokio::spawn(async move {
            let mut child = tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                release_rx.recv().unwrap();
                if !worker_cancelled.load(Ordering::Acquire) {
                    worker_created.store(true, Ordering::Release);
                }
            });
            tokio::select! {
                _ = wait_for_shutdown(shutdown_rx) => {
                    cancelled.store(true, Ordering::Release);
                    child.await.unwrap();
                }
                _ = &mut child => {}
            }
        });
        started_rx.await.unwrap();

        let workers = BackgroundTasks::new(
            test_shutdown(shutdown_tx),
            [],
            Some(warmpool::Replenisher::for_test(warm_worker)),
            None,
        );
        let swept = Arc::new(AtomicBool::new(false));
        let sweep_marker = Arc::clone(&swept);
        let mut lifecycle = tokio::spawn(async move {
            finalize_lifecycle(
                LifecycleOutcome::normal("test"),
                || async {},
                move || async move {
                    workers.stop_with_timeout(Duration::from_millis(5)).await;
                },
                move |_| async move {
                    sweep_marker.store(true, Ordering::Release);
                    Ok(())
                },
            )
            .await
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut lifecycle)
                .await
                .is_err(),
            "sweep must wait for the held spawn_blocking child"
        );
        assert!(!swept.load(Ordering::Acquire));
        assert!(!created.load(Ordering::Acquire));

        release_tx.send(()).unwrap();
        lifecycle.await.unwrap().unwrap();
        assert!(swept.load(Ordering::Acquire));
        assert!(
            !created.load(Ordering::Acquire),
            "the cancellation signal must make a late warm create harmless"
        );
    }

    fn test_config() -> Config {
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            api_keys: config::ApiKeyRegistry::from_plaintext_entries(vec![(
                "test-key".into(),
                "tenant-a".into(),
                config::ApiRole::Admin,
                0,
            )])
            .unwrap(),
            host_id: "test-host".into(),
            host_session_id: Uuid::nil(),
            vmm_bin: PathBuf::from("target/taritd-main-test/vmm"),
            kernel: PathBuf::from("target/taritd-main-test/kernel"),
            rootfs: PathBuf::from("target/taritd-main-test/rootfs"),
            socket_dir: PathBuf::from("target/taritd-main-test/sockets"),
            db_path: PathBuf::from("target/taritd-main-test/fleet.db"),
            net_state_path: PathBuf::from("target/taritd-main-test/net-state.json"),
            images_dir: PathBuf::from("target/taritd-main-test/images"),
            shared_block: None,
            cloud_object_store: None,
            image_admission_policy: crate::image::ImageAdmissionPolicy::default(),
            max_vms: 4,
            max_vcpus: 4,
            max_memory_mib: 1024,
            peer_secret: "peer-secret".into(),
            peer_listen: None,
            peer_tls: None,
            database_url: None,
            rpc_addr: "http://127.0.0.1:0".into(),
            allow_insecure_peer_http: true,
            enable_net: false,
            rootfs_read_only: false,
            metrics_expose_tenant_labels: false,
            api_max_in_flight: 128,
            api_requests_per_second: 10_000,
            api_request_timeout_ms: 5_000,
            api_max_body_bytes: 1024 * 1024,
            vm_cgroup_parent: None,
            vm_jail: None,
            vm_cgroup_pids_max: 1024,
            vm_io_quota: crate::config::VmIoQuotaConfig::default(),
            vm_net_quota: crate::config::VmNetQuotaConfig::default(),
            disk_pressure: crate::config::DiskPressureConfig::default(),
            warm_pool: config::WarmPoolConfig::default(),
            admission_timeout_ms: 1,
            reap_on_shutdown: true,
            region: "local".into(),
            zone: "local".into(),
            cloud: "onprem".into(),
            autoscale: config::AutoscaleConfig::default(),
            ssh_gateway_enabled: false,
            ssh_gateway_addr: "127.0.0.1:0".parse().unwrap(),
            ssh_gateway_host_key_path: PathBuf::from("target/taritd-main-test/ssh_host"),
            share_listen: Some("127.0.0.1:0".parse().unwrap()),
            share_domain: Some("shares.example.com".into()),
            share_token_key: Some([7; 32]),
            share_token_ttl_secs: 300,
            share_connect_timeout_ms: 1_000,
            share_idle_timeout_secs: 1,
        }
    }

    fn test_state() -> AppState {
        let config = test_config();
        let store = Arc::new(Mutex::new(Store::open(":memory:").unwrap()));
        let shares = shares::ShareRepository::new(Arc::clone(&store), None);
        let (store_tx, _store_rx) = tokio::sync::mpsc::channel(128);
        AppState {
            config: config.clone(),
            audit_outbox: Arc::new(audit::LocalAuditOutbox::new(Arc::clone(&store))),
            store,
            exec_cache: Arc::new(RwLock::new(HashMap::new())),
            vm_cache: Arc::new(RwLock::new(HashMap::new())),
            store_tx,
            lifecycle: Arc::new(Mutex::new(HashMap::new())),
            activation_gates: Arc::new(Mutex::new(HashMap::new())),
            lifecycle_faults: Arc::new(Mutex::new(Vec::new())),
            lifecycle_pauses: Arc::new(Mutex::new(HashMap::new())),
            terminal_transition_gate: Arc::new(tokio::sync::Mutex::new(())),
            pty_registry: Arc::new(pty::PtyRegistry::default()),
            supervisor: Arc::new(VmmSupervisor::new(config.clone())),
            scheduler: Arc::new(Scheduler::new(config)),
            peer: Arc::new(PeerClient::new("peer-secret".into())),
            shares,
            artifact_object_store: None,
            fleet: None,
            metrics: Arc::new(metrics::Metrics::default()),
            share_runtime: Arc::new(share_gateway::ShareRuntime::default()),
        }
    }

    #[test]
    fn remote_namespace_parser_accepts_only_versioned_artifact_paths() {
        let id = Uuid::new_v4();
        let owner = "a".repeat(64);
        assert_eq!(
            remote_artifact_namespace(&format!(
                "tenants/{owner}/artifacts/{id}/sha256-object.blob"
            )),
            Some((owner.clone(), id))
        );
        for key in [
            format!("tenants/{owner}/artifacts/legacy-object.blob"),
            format!("tenants/{owner}/boot/{id}/object.blob"),
            format!("tenants/{}/artifacts/{id}/object.blob", "A".repeat(64)),
            format!("tenants/{owner}/artifacts/{id}"),
        ] {
            assert_eq!(remote_artifact_namespace(&key), None, "{key}");
        }
    }

    #[test]
    fn remote_namespace_gc_deletes_only_metadata_orphans() {
        let backend: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let provider = Arc::new(
            tarit_volume::RemoteImmutableObjectProvider::new(
                "test_object",
                backend,
                "gc-test",
                1024,
            )
            .unwrap(),
        );
        let mut state = test_state();
        state.artifact_object_store = Some(Arc::clone(&provider));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let orphan_id = Uuid::new_v4();
            let owner_digest = "b".repeat(64);
            let orphan = provider
                .scoped(&format!("tenants/{owner_digest}/artifacts/{orphan_id}"))
                .unwrap();
            let orphan_object = orphan.put_if_absent(b"orphan").await.unwrap();
            assert!(sweep_remote_artifact_namespaces(&state).await.unwrap() >= 2);
            assert!(matches!(
                orphan.get_verified(&orphan_object).await,
                Err(tarit_volume::VolumeError::NotFound)
            ));

            let artifact_id = Uuid::new_v4();
            let owner_key = "tenant-a";
            let owner_binding = tarit_types::ArtifactObjectManifest::owner_binding(owner_key);
            let owner_digest = owner_binding.strip_prefix("sha256:").unwrap();
            let live = provider
                .scoped(&format!("tenants/{owner_digest}/artifacts/{artifact_id}"))
                .unwrap();
            let live_object = live.put_if_absent(b"live").await.unwrap();
            let now = chrono::Utc::now();
            state
                .store
                .lock()
                .unwrap()
                .insert_artifact(&tarit_types::ArtifactRecord {
                    artifact_id,
                    owner_key: owner_key.into(),
                    host_id: "test-host".into(),
                    storage_locator: "/private/snapshot".into(),
                    kind: tarit_types::ArtifactKind::VmSnapshot,
                    status: tarit_types::ArtifactStatus::Available,
                    content_digest: format!("sha256:{}", "1".repeat(64)),
                    size_bytes: 4,
                    immutable_image_digest: format!("sha256:{}", "2".repeat(64)),
                    agent_digest: format!("sha256:{}", "3".repeat(64)),
                    boot_manifest_digest: format!("sha256:{}", "4".repeat(64)),
                    parent_artifact_id: None,
                    source_vm_id: Some(Uuid::new_v4()),
                    creation_revision: 1,
                    integrity_manifest_digest: format!("sha256:{}", "5".repeat(64)),
                    chunk_size_bytes: 4096,
                    chunk_count: 1,
                    replication_state: tarit_types::ArtifactReplicationState::Ready,
                    reference_count: 0,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
            assert_eq!(sweep_remote_artifact_namespaces(&state).await.unwrap(), 0);
            assert_eq!(live.get_verified(&live_object).await.unwrap(), b"live");
            live.ensure_namespace_writable().await.unwrap();
        });
        drop(runtime);
        drop(state);
    }
}
