mod api;
mod audit;
mod autoscale;
mod cli;
mod cluster;
mod config;
mod gateway;
mod image;
mod internal;
mod metrics;
mod net;
mod openapi;
mod ops;
mod peer;
mod pty;
mod scheduler;
mod share_gateway;
mod shares;
mod ssh_keys;
mod supervisor;
mod usage;
mod warmpool;

use anyhow::Context;
use api::{router, AppState};
use clap::Parser;
use config::Config;
use peer::PeerClient;
use scheduler::Scheduler;
use std::collections::HashMap;
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
        let config = Config::from_env().context("load config")?;
        run_server(config).await
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

async fn run_server(mut config: Config) -> anyhow::Result<()> {
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
        share,
        ssh,
    } = bind_server_listeners(&config).await?;

    std::fs::create_dir_all(&config.socket_dir).ok();
    std::fs::create_dir_all(&config.images_dir).ok();
    if let Some(parent) = config.db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let store = Store::open(&config.db_path).context("open store")?;
    image::resolve_warm_pool_images(&mut config, &store).context("resolve warm-pool images")?;
    let persisted_vms = match store.list_vms() {
        Ok(vms) => vms,
        Err(e) => {
            tracing::warn!("failed to load persisted VMs during startup: {e}");
            Vec::new()
        }
    };
    let live_vm_ids = persisted_vms
        .iter()
        .filter(|vm| {
            vm.host_id == config.host_id
                && matches!(vm.status, VmStatus::Running | VmStatus::Paused)
        })
        .map(|vm| vm.id)
        .collect::<Vec<_>>();
    let supervisor = Arc::new(VmmSupervisor::new_with_live_vms(
        config.clone(),
        live_vm_ids,
    ));
    let scheduler = Arc::new(Scheduler::new(config.clone()));
    // Build the peer HTTP client off the async runtime. `reqwest::blocking`
    // spins up its own current-thread runtime; constructing it inside a tokio
    // context panics on current tokio ("Cannot drop a runtime ... from within
    // an asynchronous context"). A plain OS thread has no ambient runtime, so
    // construction is safe there. All runtime peer calls already run via
    // spawn_blocking, so this only moves the one-time construction off-thread.
    let peer = {
        let secret = config.peer_secret.clone();
        std::thread::spawn(move || PeerClient::new(secret))
            .join()
            .map_err(|_| anyhow::anyhow!("peer client init thread panicked"))?
    };

    // Register self in local roster for single-host / scheduler.
    {
        let cap = scheduler.local_capacity(1, 256);
        let host = tarit_store::HostRecord {
            host_id: config.host_id.clone(),
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
        for vm in persisted_vms {
            c.insert(vm.id, vm);
        }
    }
    let (store_tx, mut store_rx) = tokio::sync::mpsc::unbounded_channel::<api::StoreWrite>();
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
        let fleet_sync = spawn_fleet_sync(
            Arc::clone(&fleet),
            Arc::clone(&store),
            config.clone(),
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
        pty_registry: Arc::new(pty::PtyRegistry::default()),
        supervisor: Arc::clone(&supervisor),
        scheduler: scheduler.clone(),
        peer: Arc::new(peer),
        shares,
        fleet,
        metrics: Arc::new(metrics::Metrics::default()),
        share_runtime: Arc::clone(&share_runtime),
    };

    // Start every background worker only after all listener binds succeeded.
    let store_writer = {
        let store = Arc::clone(&state.store);
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
                if let Ok(s) = store.lock() {
                    match op {
                        api::StoreWrite::Vm(rec) => {
                            let _ = s.insert_vm(&rec);
                        }
                        api::StoreWrite::Exec(rec) => {
                            let _ = s.insert_execution(&rec);
                        }
                        api::StoreWrite::Usage(ev) => {
                            let _ = s.enqueue_usage(&ev);
                        }
                        api::StoreWrite::Audit(ev) => {
                            let _ = s.enqueue_audit(&ev);
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

    let shutdown_signal_task = spawn_shutdown_signal(shutdown.clone(), shutdown_rx.clone());
    let worker_tasks = BackgroundTasks::new(
        shutdown.clone(),
        [
            Some(store_writer),
            fleet_sync,
            Some(usage_meter),
            outbox_flusher,
            Some(shutdown_signal_task),
        ],
        warm_pool,
        autoscaler,
    );

    let (app, share_app) = server_routers(state.clone());
    tracing::info!("control listener listening on http://{}", config.listen);
    if let Some(share_addr) = config.share_listen {
        tracing::info!("share listener listening on http://{}", share_addr);
    }
    let control_server = spawn_http_server(control, app, shutdown_rx.clone());
    let share_server =
        share.map(|listener| spawn_http_server(listener, share_app, shutdown_rx.clone()));
    let ssh_server = ssh.map(|listener| spawn_ssh_server(listener, state.clone()));
    let outcome = supervise_servers(
        control_server,
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

struct ServerListeners {
    control: tokio::net::TcpListener,
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
    Ok(ServerListeners {
        control,
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

fn server_routers(state: AppState) -> (axum::Router, axum::Router) {
    (
        router(state.clone()).merge(internal::internal_router(state.clone())),
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
    Share(Result<anyhow::Result<()>, tokio::task::JoinError>),
    Ssh(Result<anyhow::Result<()>, tokio::task::JoinError>),
}

async fn supervise_servers(
    mut control: ServerHandle,
    mut share: Option<ServerHandle>,
    mut ssh: Option<ServerHandle>,
    shutdown: ShutdownCoordinator,
    shutdown_rx: watch::Receiver<Option<&'static str>>,
    drain_timeout: Duration,
) -> LifecycleOutcome {
    let event = match (share.as_mut(), ssh.as_mut()) {
        (Some(share), Some(ssh)) => {
            tokio::select! {
                biased;
                reason = wait_for_shutdown(shutdown_rx.clone()) => ServerEvent::Shutdown(reason),
                result = &mut control => ServerEvent::Control(result),
                result = &mut *share => ServerEvent::Share(result),
                result = &mut *ssh => ServerEvent::Ssh(result),
            }
        }
        (Some(share), None) => {
            tokio::select! {
                biased;
                reason = wait_for_shutdown(shutdown_rx.clone()) => ServerEvent::Shutdown(reason),
                result = &mut control => ServerEvent::Control(result),
                result = &mut *share => ServerEvent::Share(result),
            }
        }
        (None, Some(ssh)) => {
            tokio::select! {
                biased;
                reason = wait_for_shutdown(shutdown_rx.clone()) => ServerEvent::Shutdown(reason),
                result = &mut control => ServerEvent::Control(result),
                result = &mut *ssh => ServerEvent::Ssh(result),
            }
        }
        (None, None) => {
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
        elapsed_ms = started.elapsed().as_millis(),
        "shutdown drain summary: reaped local VMs"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header::HOST, Request, StatusCode},
    };
    use std::path::PathBuf;
    use tokio::net::TcpListener;
    use tower::ServiceExt;

    fn test_shutdown(tx: watch::Sender<Option<&'static str>>) -> ShutdownCoordinator {
        ShutdownCoordinator::new(tx, Arc::new(VmmSupervisor::new(test_config())))
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

        let error = run_server(config).await.unwrap_err();

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
    fn server_routers_keep_control_and_share_routes_separate() {
        let (control, share) = server_routers(test_state());
        let share_host = "calm-red-fox.shares.example.com";
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let control_test = control.clone();
        let share_test = share.clone();

        runtime.block_on(async move {
            let control_response = control_test
                .oneshot(
                    Request::builder()
                        .uri("/")
                        .header(HOST, share_host)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(control_response.status(), StatusCode::UNAUTHORIZED);

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
            vmm_bin: PathBuf::from("target/taritd-main-test/vmm"),
            kernel: PathBuf::from("target/taritd-main-test/kernel"),
            rootfs: PathBuf::from("target/taritd-main-test/rootfs"),
            socket_dir: PathBuf::from("target/taritd-main-test/sockets"),
            db_path: PathBuf::from("target/taritd-main-test/fleet.db"),
            net_state_path: PathBuf::from("target/taritd-main-test/net-state.json"),
            images_dir: PathBuf::from("target/taritd-main-test/images"),
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
        let (store_tx, _store_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState {
            config: config.clone(),
            audit_outbox: Arc::new(audit::LocalAuditOutbox::new(Arc::clone(&store))),
            store,
            exec_cache: Arc::new(RwLock::new(HashMap::new())),
            vm_cache: Arc::new(RwLock::new(HashMap::new())),
            store_tx,
            pty_registry: Arc::new(pty::PtyRegistry::default()),
            supervisor: Arc::new(VmmSupervisor::new(config.clone())),
            scheduler: Arc::new(Scheduler::new(config)),
            peer: Arc::new(PeerClient::new("peer-secret".into())),
            shares,
            fleet: None,
            metrics: Arc::new(metrics::Metrics::default()),
            share_runtime: Arc::new(share_gateway::ShareRuntime::default()),
        }
    }
}

fn spawn_fleet_sync(
    fleet: Arc<PostgresFleet>,
    store: Arc<Mutex<Store>>,
    config: Config,
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
                Some(config.rpc_addr.clone()),
                cap.sandbox_count,
                cap.free_vcpus,
                cap.free_memory_mib,
            );
            if fleet.upsert_host(&host).await.is_err() {
                tracing::warn!("fleet heartbeat failed");
                continue;
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
