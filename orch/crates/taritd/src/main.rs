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
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use supervisor::VmmSupervisor;
use tarit_store::Store;
use tarit_types::VmStatus;
use tokio::sync::watch;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

use tarit_fleet::PostgresFleet;

const HTTP_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

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
    {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            while let Some(op) = store_rx.recv().await {
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
        });
    }

    // Connect the global fleet registry (Postgres) if configured. In cluster
    // mode this drives cross-node placement, VM->owner routing, and membership;
    // single-host mode leaves it None and everything runs locally.
    let fleet: Option<Arc<PostgresFleet>> = if let Some(ref url) = config.database_url {
        let f = Arc::new(
            PostgresFleet::connect(url)
                .await
                .context("postgres fleet")?,
        );
        spawn_fleet_sync(
            Arc::clone(&f),
            Arc::clone(&store),
            config.clone(),
            Arc::clone(&scheduler),
        );
        autoscale::spawn(Arc::clone(&f), config.clone());
        tracing::info!("fleet: connected to global control-plane store");
        Some(f)
    } else {
        None
    };

    // Start the warm-pool replenisher (no-op unless enabled in config/env).
    warmpool::spawn_replenisher(
        Arc::clone(&supervisor),
        config.clone(),
        Arc::clone(&scheduler),
    );

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
    };

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
    usage::spawn_usage_meter(state.clone(), meter_secs);
    usage::spawn_outbox_flusher(state.clone(), flush_secs);

    if config.ssh_gateway_enabled {
        let gateway_state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = gateway::run(gateway_state).await {
                tracing::error!("SSH gateway stopped: {e:#}");
            }
        });
    }

    let shutdown_state = state.clone();
    let (listener, share_listener) =
        bind_http_listeners(config.listen, config.share_listen).await?;
    let (app, share_app) = server_routers(state);

    tracing::info!("control listener listening on http://{}", config.listen);
    if let Some(share_addr) = config.share_listen {
        tracing::info!("share listener listening on http://{}", share_addr);
    }
    let (shutdown_tx, shutdown_rx) = watch::channel(None::<&'static str>);
    tokio::spawn(async move {
        let reason = shutdown_signal().await;
        let _ = shutdown_tx.send(Some(reason));
    });

    let control_server = spawn_http_server(listener, app, shutdown_rx.clone());
    let share_server =
        share_listener.map(|listener| spawn_http_server(listener, share_app, shutdown_rx.clone()));
    let reason = supervise_http_servers(
        control_server,
        share_server,
        shutdown_rx,
        HTTP_DRAIN_TIMEOUT,
    )
    .await?;

    shutdown_sweep(&shutdown_state, reason).await?;

    Ok(())
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

type HttpServerHandle = tokio::task::JoinHandle<std::io::Result<()>>;

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
) -> HttpServerHandle {
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = wait_for_shutdown(shutdown_rx).await;
            })
            .await
    })
}

async fn supervise_http_servers(
    mut control: HttpServerHandle,
    mut share: Option<HttpServerHandle>,
    shutdown_rx: watch::Receiver<Option<&'static str>>,
    drain_timeout: Duration,
) -> anyhow::Result<&'static str> {
    let (reason, control_exited, share_exited) = match share.as_mut() {
        Some(share_server) => {
            tokio::select! {
                result = &mut control => {
                    let Some(reason) = *shutdown_rx.borrow() else {
                        abort_server(share_server).await;
                        return Err(unexpected_server_exit("control", result));
                    };
                    server_result("control", result)?;
                    (reason, true, false)
                }
                result = &mut *share_server => {
                    let Some(reason) = *shutdown_rx.borrow() else {
                        control.abort();
                        let _ = control.await;
                        return Err(unexpected_server_exit("share", result));
                    };
                    server_result("share", result)?;
                    (reason, false, true)
                }
                reason = wait_for_shutdown(shutdown_rx.clone()) => (reason, false, false),
            }
        }
        None => {
            tokio::select! {
                result = &mut control => {
                    let Some(reason) = *shutdown_rx.borrow() else {
                        return Err(unexpected_server_exit("control", result));
                    };
                    server_result("control", result)?;
                    (reason, true, false)
                }
                reason = wait_for_shutdown(shutdown_rx.clone()) => (reason, false, false),
            }
        }
    };

    tracing::info!(
        reason,
        drain_timeout_secs = drain_timeout.as_secs(),
        "shutdown signal received; draining HTTP listeners"
    );
    let deadline = tokio::time::Instant::now() + drain_timeout;
    if !control_exited {
        drain_server("control", &mut control, deadline).await?;
    }
    if !share_exited {
        if let Some(share) = share.as_mut() {
            drain_server("share", share, deadline).await?;
        }
    }
    Ok(reason)
}

fn unexpected_server_exit(
    name: &str,
    result: Result<std::io::Result<()>, tokio::task::JoinError>,
) -> anyhow::Error {
    match server_result(name, result) {
        Ok(()) => anyhow::anyhow!("{name} server exited unexpectedly"),
        Err(error) => error.context(format!("{name} server exited unexpectedly")),
    }
}

fn server_result(
    name: &str,
    result: Result<std::io::Result<()>, tokio::task::JoinError>,
) -> anyhow::Result<()> {
    result
        .map_err(|error| anyhow::anyhow!("{name} server task panicked: {error}"))?
        .with_context(|| format!("{name} server serve"))
}

async fn drain_server(
    name: &str,
    server: &mut HttpServerHandle,
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

async fn abort_server(server: &mut HttpServerHandle) {
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
    use std::{io, path::PathBuf};
    use tokio::net::TcpListener;
    use tower::ServiceExt;

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

    #[tokio::test]
    async fn shutdown_drains_both_servers_before_returning() {
        let (shutdown_tx, shutdown_rx) = watch::channel(None::<&'static str>);
        let drained = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let control_drained = Arc::clone(&drained);
        let control_rx = shutdown_rx.clone();
        let control = tokio::spawn(async move {
            wait_for_shutdown(control_rx).await;
            control_drained.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<(), io::Error>(())
        });
        let share_drained = Arc::clone(&drained);
        let share_rx = shutdown_rx.clone();
        let share = tokio::spawn(async move {
            wait_for_shutdown(share_rx).await;
            share_drained.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<(), io::Error>(())
        });

        shutdown_tx.send(Some("test")).unwrap();
        let reason =
            supervise_http_servers(control, Some(share), shutdown_rx, Duration::from_secs(1))
                .await
                .unwrap();

        assert_eq!(reason, "test");
        assert_eq!(
            drained.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "both listeners must drain before shutdown continues"
        );
    }

    #[tokio::test]
    async fn unexpected_share_server_exit_is_fatal() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(None::<&'static str>);
        let control = tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok::<(), io::Error>(())
        });
        let share = tokio::spawn(async { Ok::<(), io::Error>(()) });

        let error =
            supervise_http_servers(control, Some(share), shutdown_rx, Duration::from_secs(1))
                .await
                .unwrap_err();

        assert!(error
            .to_string()
            .contains("share server exited unexpectedly"));
    }

    #[tokio::test]
    async fn unexpected_control_server_exit_is_fatal() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(None::<&'static str>);
        let control = tokio::spawn(async { Ok::<(), io::Error>(()) });
        let share = tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok::<(), io::Error>(())
        });

        let error =
            supervise_http_servers(control, Some(share), shutdown_rx, Duration::from_secs(1))
                .await
                .unwrap_err();

        assert!(error
            .to_string()
            .contains("control server exited unexpectedly"));
    }

    fn test_state() -> AppState {
        let config = Config {
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
        };
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
        }
    }
}

fn spawn_fleet_sync(
    fleet: Arc<PostgresFleet>,
    store: Arc<Mutex<Store>>,
    config: Config,
    scheduler: Arc<Scheduler>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
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
    });
}
