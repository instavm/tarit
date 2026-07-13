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
        let preflight_taps = net::startup_preflight().context(
            "contain pre-existing Tarit TAPs before configuration, database, image, or VM discovery",
        )?;
        let config = Config::from_env().context("load config")?;
        run_server(config, preflight_taps).await
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

async fn run_server(mut config: Config, preflight_taps: Vec<String>) -> anyhow::Result<()> {
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
    let persisted_vms = store
        .list_vms()
        .context("load persisted VMs during startup")?;
    let live_vm_ids = persisted_vms
        .iter()
        .filter(|vm| {
            vm.host_id == config.host_id
                && matches!(vm.status, VmStatus::Running | VmStatus::Paused)
        })
        .map(|vm| vm.id)
        .collect::<Vec<_>>();
    let scheduler = Arc::new(Scheduler::new(config.clone()));
    let supervisor = Arc::new(
        VmmSupervisor::new_with_live_vms(
            config.clone(),
            live_vm_ids,
            &preflight_taps,
            Arc::clone(&scheduler),
        )
        .context("initialize fail-closed network recovery")?,
    );
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
                match store.lock() {
                    Ok(s) => match op {
                        api::StoreWrite::Vm(rec) => {
                            let _ = s.insert_vm(&rec);
                        }
                        api::StoreWrite::VmDurable(rec, completion) => {
                            let result = s.insert_vm(&rec).map_err(api::store_err);
                            let _ = completion.send(result);
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
                    },
                    Err(_) => {
                        if let api::StoreWrite::VmDurable(_, completion) = op {
                            let _ = completion.send(Err(tarit_types::OrchError::Internal(
                                "store lock poisoned during shutdown persistence".into(),
                            )));
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
    warmpool::spawn_replenisher(Arc::clone(&supervisor), config.clone());

    let state = AppState {
        config: config.clone(),
        store,
        exec_cache: Arc::new(RwLock::new(HashMap::new())),
        vm_cache,
        store_tx,
        lifecycle: Arc::new(Mutex::new(HashMap::new())),
        #[cfg(test)]
        lifecycle_faults: Arc::new(Mutex::new(Vec::new())),
        terminal_transition_gate: Arc::new(tokio::sync::Mutex::new(())),
        pty_registry: Arc::new(pty::PtyRegistry::default()),
        supervisor: Arc::clone(&supervisor),
        scheduler: scheduler.clone(),
        peer: Arc::new(peer),
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
    let app = router(state.clone()).merge(internal::internal_router(state));
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("bind {}", config.listen))?;

    tracing::info!("listening on http://{}", config.listen);
    let (shutdown_tx, shutdown_rx) = watch::channel(None::<&'static str>);
    tokio::spawn(async move {
        let reason = shutdown_signal().await;
        let _ = shutdown_tx.send(Some(reason));
    });

    let server_shutdown_rx = shutdown_rx.clone();
    let mut serve_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = wait_for_shutdown(server_shutdown_rx).await;
            })
            .await
    });

    let reason = tokio::select! {
        result = &mut serve_handle => {
            result
                .map_err(|e| anyhow::anyhow!("server task panicked: {e}"))?
                .context("serve")?;
            let Some(reason) = *shutdown_rx.borrow() else {
                return Ok(());
            };
            reason
        }
        reason = wait_for_shutdown(shutdown_rx.clone()) => {
            tracing::info!(
                reason = reason,
                drain_timeout_secs = HTTP_DRAIN_TIMEOUT.as_secs(),
                "shutdown signal received; draining HTTP"
            );
            match tokio::time::timeout(HTTP_DRAIN_TIMEOUT, &mut serve_handle).await {
                Ok(result) => {
                    result
                        .map_err(|e| anyhow::anyhow!("server task panicked: {e}"))?
                        .context("serve")?;
                }
                Err(_) => {
                    tracing::warn!(
                        reason = reason,
                        drain_timeout_secs = HTTP_DRAIN_TIMEOUT.as_secs(),
                        "HTTP drain timed out; aborting remaining connections"
                    );
                    serve_handle.abort();
                    let _ = serve_handle.await;
                }
            }
            reason
        }
    };

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
