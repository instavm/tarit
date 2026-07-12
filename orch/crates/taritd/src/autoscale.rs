//! Fleet autoscaler: a leader-elected control loop that scales cloud instances
//! up when the cluster is running low on VM capacity and down when it is idle.
//!
//! Design points (see docs): only ONE node actuates (Postgres lease election),
//! decisions are rate-limited by a cooldown to avoid flapping, and scaling is
//! actuated through a pluggable provider command so taritd stays cloud-SDK-free
//! and works across EC2 / GCP / bare metal (the operator wires an ASG / MIG /
//! Terraform behind `TARIT_AUTOSCALE_PROVIDER_CMD`). New nodes self-register in
//! the fleet on boot via the normal heartbeat, so scale-out needs no extra
//! coordination.
//!
//! Scale-in emits a `drain` decision naming the least-loaded victim node; the
//! provider is responsible for draining it (the orchestrator marks it and its
//! VMs can be snapshot/evacuated) before termination. Since snapshots are
//! node-local, a stateful scale-in must evacuate first — the provider script
//! owns that policy.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::watch;

use tarit_fleet::PostgresFleet;

use crate::config::Config;
use crate::supervisor::VmAdmissionGate;

const TICK: Duration = Duration::from_secs(10);
const LEADER_TTL_SECS: i64 = 30;
const HOST_FRESH: Duration = Duration::from_secs(15);
const COOLDOWN: Duration = Duration::from_secs(60);

struct ScaleDecision<'a> {
    action: &'a str,
    target: usize,
    current: usize,
    free_vcpus: u64,
    victim: Option<&'a str>,
}

pub fn spawn(
    fleet: Arc<PostgresFleet>,
    config: Config,
    admission: Arc<VmAdmissionGate>,
    mut shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.autoscale.enabled {
        return None;
    }
    Some(tokio::spawn(async move {
        tracing::info!(
            min = config.autoscale.min_nodes,
            max = config.autoscale.max_nodes,
            "autoscaler: enabled"
        );
        let mut tick = tokio::time::interval(TICK);
        let mut last_action = Instant::now() - COOLDOWN;
        loop {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut shutdown_rx) => break,
                _ = tick.tick() => {}
            }

            // Leader election: exactly one node runs the control loop.
            match fleet
                .try_acquire_leader(&config.host_id, LEADER_TTL_SECS)
                .await
            {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!("autoscaler: leader election failed: {e}");
                    continue;
                }
            }

            let hosts = match fleet.list_hosts().await {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("autoscaler: list_hosts failed: {e}");
                    continue;
                }
            };
            let now = chrono::Utc::now();
            let healthy: Vec<_> = hosts
                .iter()
                .filter(|h| {
                    h.healthy
                        && (now - h.last_heartbeat)
                            .to_std()
                            .map(|d| d < HOST_FRESH)
                            .unwrap_or(false)
                })
                .collect();
            let node_count = healthy.len();
            let free_vcpus: u64 = healthy.iter().map(|h| h.free_vcpus).sum();

            if last_action.elapsed() < COOLDOWN {
                continue;
            }
            let a = &config.autoscale;

            if free_vcpus < a.scale_out_free_vcpus && node_count < a.max_nodes {
                let target = (node_count + 1).min(a.max_nodes);
                actuate(
                    &config,
                    ScaleDecision {
                        action: "scale_out",
                        target,
                        current: node_count,
                        free_vcpus,
                        victim: None,
                    },
                    Arc::clone(&admission),
                    shutdown_rx.clone(),
                )
                .await;
                last_action = Instant::now();
            } else if free_vcpus > a.scale_in_free_vcpus && node_count > a.min_nodes {
                // Drain the least-loaded node.
                if let Some(victim) = healthy.iter().min_by_key(|h| h.sandbox_count) {
                    let target = (node_count - 1).max(a.min_nodes);
                    actuate(
                        &config,
                        ScaleDecision {
                            action: "scale_in",
                            target,
                            current: node_count,
                            free_vcpus,
                            victim: Some(&victim.host_id),
                        },
                        Arc::clone(&admission),
                        shutdown_rx.clone(),
                    )
                    .await;
                    last_action = Instant::now();
                }
            }
        }
    }))
}

async fn wait_for_shutdown(shutdown_rx: &mut watch::Receiver<Option<&'static str>>) {
    loop {
        if shutdown_rx.borrow().is_some() {
            return;
        }
        if shutdown_rx.changed().await.is_err() {
            return;
        }
    }
}

async fn actuate(
    config: &Config,
    scale: ScaleDecision<'_>,
    admission: Arc<VmAdmissionGate>,
    shutdown_rx: watch::Receiver<Option<&'static str>>,
) {
    let decision = serde_json::json!({
        "action": scale.action,
        "target_nodes": scale.target,
        "current_nodes": scale.current,
        "free_vcpus": scale.free_vcpus,
        "victim": scale.victim,
        "region": config.region,
        "zone": config.zone,
        "cloud": config.cloud,
    });

    match &config.autoscale.provider_cmd {
        Some(cmd) => {
            tracing::info!(
                action = scale.action,
                target = scale.target,
                "autoscaler: invoking provider"
            );
            // The decision JSON is passed as $1 to the operator's provider
            // script (which wraps the cloud API). Never interpolated into the
            // shell, so it cannot inject.
            run_provider(cmd.clone(), decision.to_string(), admission, shutdown_rx).await;
        }
        None => tracing::info!(
            decision = %decision,
            "autoscaler: decision (noop provider — set TARIT_AUTOSCALE_PROVIDER_CMD to actuate)"
        ),
    }
}

async fn run_provider(
    provider_command: String,
    decision: String,
    admission: Arc<VmAdmissionGate>,
    mut shutdown_rx: watch::Receiver<Option<&'static str>>,
) {
    if shutdown_rx.borrow().is_some() {
        tracing::info!("autoscaler: skipping provider because shutdown has started");
        return;
    }

    let mut process = Command::new("sh");
    process
        .arg("-c")
        .arg(provider_command)
        .arg("provider")
        .arg(decision)
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);

    let child = match admission.enter() {
        Ok(_admission) => match process.spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!("autoscaler: provider spawn failed: {error}");
                return;
            }
        },
        Err(_) => {
            tracing::info!("autoscaler: skipping provider because VM admission is closed");
            return;
        }
    };
    let mut provider = ProviderChild::new(child);

    tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut shutdown_rx) => {
            tracing::info!("autoscaler: terminating provider for shutdown");
            if let Err(error) = provider.terminate() {
                tracing::warn!("autoscaler: provider termination failed: {error}");
            }
            match provider.wait().await {
                Ok(status) => tracing::info!(%status, "autoscaler: provider reaped after shutdown"),
                Err(error) => tracing::warn!("autoscaler: provider reap failed after shutdown: {error}"),
            }
        }
        result = provider.wait() => match result {
            Ok(status) if status.success() => tracing::info!("autoscaler: provider ok"),
            Ok(status) => tracing::warn!(%status, "autoscaler: provider failed"),
            Err(error) => tracing::warn!("autoscaler: provider wait failed: {error}"),
        }
    }
}

struct ProviderChild {
    child: Child,
    reaped: bool,
}

impl ProviderChild {
    fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn terminate(&mut self) -> std::io::Result<()> {
        terminate_provider_process_group(&mut self.child)
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let result = self.child.wait().await;
        if result.is_ok() {
            self.reaped = true;
        }
        result
    }
}

impl Drop for ProviderChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = terminate_provider_process_group(&mut self.child);
        }
    }
}

#[cfg(unix)]
fn terminate_provider_process_group(child: &mut Child) -> std::io::Result<()> {
    let Some(pid) = child.id() else {
        return Ok(());
    };
    let process_group = -(pid as libc::pid_t);
    // SAFETY: `process_group` is the negative PID assigned by
    // `Command::process_group(0)`, which asks the kernel to signal only that
    // provider process group.
    let result = unsafe { libc::kill(process_group, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(not(unix))]
fn terminate_provider_process_group(child: &mut Child) -> std::io::Result<()> {
    child.start_kill()
}

#[cfg(test)]
mod tests {
    use super::run_provider;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::watch;

    use crate::supervisor::VmAdmissionGate;

    #[tokio::test]
    async fn shutdown_terminates_and_reaps_a_running_provider_child() {
        let root = PathBuf::from(format!(
            "target/taritd-autoscale-provider-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let started = root.join("started");
        let survived = root.join("survived");
        let command = format!(
            "echo started > '{}'; (sleep 0.2; echo survived > '{}') & wait",
            started.display(),
            survived.display()
        );
        let (shutdown_tx, shutdown_rx) = watch::channel(None);
        let provider = tokio::spawn(run_provider(
            command,
            "{}".into(),
            Arc::new(VmAdmissionGate::default()),
            shutdown_rx,
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("provider must be running before shutdown");

        shutdown_tx.send(Some("test")).unwrap();
        tokio::time::timeout(Duration::from_secs(1), provider)
            .await
            .expect("shutdown must terminate and reap the provider child")
            .unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !survived.exists(),
            "shutdown must terminate the provider process group, not only its shell"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn closed_admission_skips_provider_spawn() {
        let root = PathBuf::from(format!(
            "target/taritd-autoscale-admission-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let started = root.join("started");
        let admission = Arc::new(VmAdmissionGate::default());
        admission.close();
        let (_shutdown_tx, shutdown_rx) = watch::channel(None);

        run_provider(
            format!("echo started > '{}'", started.display()),
            "{}".into(),
            admission,
            shutdown_rx,
        )
        .await;

        assert!(
            !started.exists(),
            "a planned autoscale provider must not start after admission closes"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
