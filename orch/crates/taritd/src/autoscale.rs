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

use tarit_fleet::PostgresFleet;

use crate::config::Config;

const TICK: Duration = Duration::from_secs(10);
const LEADER_TTL_SECS: i64 = 30;
const HOST_FRESH: Duration = Duration::from_secs(15);
const COOLDOWN: Duration = Duration::from_secs(60);

pub fn spawn(fleet: Arc<PostgresFleet>, config: Config) {
    if !config.autoscale.enabled {
        return;
    }
    tokio::spawn(async move {
        tracing::info!(
            min = config.autoscale.min_nodes,
            max = config.autoscale.max_nodes,
            "autoscaler: enabled"
        );
        let mut tick = tokio::time::interval(TICK);
        let mut last_action = Instant::now() - COOLDOWN;
        loop {
            tick.tick().await;

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
                actuate(&config, "scale_out", target, node_count, free_vcpus, None);
                last_action = Instant::now();
            } else if free_vcpus > a.scale_in_free_vcpus && node_count > a.min_nodes {
                // Drain the least-loaded node.
                if let Some(victim) = healthy.iter().min_by_key(|h| h.sandbox_count) {
                    let target = (node_count - 1).max(a.min_nodes);
                    actuate(
                        &config,
                        "scale_in",
                        target,
                        node_count,
                        free_vcpus,
                        Some(&victim.host_id),
                    );
                    last_action = Instant::now();
                }
            }
        }
    });
}

fn actuate(
    config: &Config,
    action: &str,
    target: usize,
    current: usize,
    free_vcpus: u64,
    victim: Option<&str>,
) {
    let decision = serde_json::json!({
        "action": action,
        "target_nodes": target,
        "current_nodes": current,
        "free_vcpus": free_vcpus,
        "victim": victim,
        "region": config.region,
        "zone": config.zone,
        "cloud": config.cloud,
    });

    match &config.autoscale.provider_cmd {
        Some(cmd) => {
            tracing::info!(action, target, "autoscaler: invoking provider");
            // The decision JSON is passed as $1 to the operator's provider
            // script (which wraps the cloud API). Never interpolated into the
            // shell, so it cannot inject.
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .arg("provider")
                .arg(decision.to_string())
                .output();
            match out {
                Ok(o) if o.status.success() => tracing::info!("autoscaler: provider ok"),
                Ok(o) => tracing::warn!(
                    "autoscaler: provider failed ({}): {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr)
                ),
                Err(e) => tracing::warn!("autoscaler: provider spawn failed: {e}"),
            }
        }
        None => tracing::info!(
            decision = %decision,
            "autoscaler: decision (noop provider — set TARIT_AUTOSCALE_PROVIDER_CMD to actuate)"
        ),
    }
}
