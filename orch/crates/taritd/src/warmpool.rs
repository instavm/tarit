//! Warm-pool replenisher: keeps a buffer of pre-booted VMs ready so create()
//! can hand one out instantly instead of paying cold-boot latency.
//!
//! A background task tops each configured class up to its hysteresis target
//! after depth drops below the low watermark. Boots/restores are concurrent
//! only up to `replenish_concurrency` and never exceed the host's placement
//! capacity. When the take rate outpaces replenishment the pool simply drains
//! and create() falls back to a cold start, so the pool is a best-effort
//! accelerator, never a correctness dependency.

use crate::config::Config;
use crate::supervisor::{VmSpawnConfig, VmmSupervisor};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tarit_types::OrchError;

/// Spawn the background replenishment loop. No-op unless the pool is enabled.
pub fn spawn_replenisher(sup: Arc<VmmSupervisor>, config: Config) {
    if !config.warm_pool.enabled {
        return;
    }

    let classes = config.warm_pool.classes.clone();
    let conc = config.warm_pool.replenish_concurrency.max(1);
    tracing::info!(
        total_target = config.warm_pool.total_target(),
        classes = classes.len(),
        cpu_overcommit = config.warm_pool.cpu_overcommit,
        max_vcpus = config.max_vcpus,
        "warm pool enabled"
    );

    let golden_snapshots = Arc::new(tokio::sync::Mutex::new(
        HashMap::<VmSpawnConfig, String>::new(),
    ));

    tokio::spawn(async move {
        loop {
            let mut did_work = false;
            let mut capacity_blocked = false;
            for class in &classes {
                let have = {
                    let sup = Arc::clone(&sup);
                    let (v, m) = (class.vcpus, class.memory_mib);
                    tokio::task::spawn_blocking(move || sup.warm_count(v, m))
                        .await
                        .unwrap_or(0)
                };
                let need = class.refill_needed(have);
                if need == 0 {
                    continue;
                }
                if class.restore {
                    let key = VmSpawnConfig::from_warm_class(&config, class);
                    let snapshot_path = {
                        let mut golden = golden_snapshots.lock().await;
                        if let Some(path) = golden.get(&key).cloned() {
                            Some(path)
                        } else {
                            let golden_sup = Arc::clone(&sup);
                            let class = class.clone();
                            match golden_sup.create_golden(class).await {
                                Ok(path) => {
                                    did_work = true;
                                    tracing::info!(
                                        vcpus = key.vcpus,
                                        memory_mib = key.memory_mib,
                                        rootfs = ?key.rootfs_path.as_ref(),
                                        snapshot_path = %path,
                                        "warm golden snapshot created"
                                    );
                                    golden.insert(key, path);
                                }
                                Err(e) => {
                                    tracing::warn!("warm golden create failed: {e}");
                                }
                            }
                            None
                        }
                    };
                    let Some(snapshot_path) = snapshot_path else {
                        continue;
                    };
                    let have = {
                        let sup = Arc::clone(&sup);
                        let (v, m) = (class.vcpus, class.memory_mib);
                        tokio::task::spawn_blocking(move || sup.warm_count(v, m))
                            .await
                            .unwrap_or(0)
                    };
                    let need = class.refill_needed(have);
                    if need == 0 {
                        continue;
                    }
                    let to_spawn = need.min(conc);
                    let mut set = tokio::task::JoinSet::new();
                    for _ in 0..to_spawn {
                        let sup = Arc::clone(&sup);
                        let class = class.clone();
                        let snapshot_path = snapshot_path.clone();
                        set.spawn(
                            async move { sup.spawn_warm_restore(class, snapshot_path).await },
                        );
                    }
                    while let Some(result) = set.join_next().await {
                        match result {
                            Ok(Ok(())) => did_work = true,
                            Ok(Err(OrchError::Overloaded { .. })) => capacity_blocked = true,
                            Ok(Err(error)) => {
                                tracing::warn!("warm restore spawn failed: {error}")
                            }
                            Err(error) => tracing::warn!("warm restore task failed: {error}"),
                        }
                    }
                    continue;
                }
                // Continuous refill pipeline: keep up to `conc` cold boots in
                // flight and launch a replacement the instant one finishes, so a
                // single slow boot never stalls the batch (the old code barriered
                // on the whole batch). Reserve a slot per warm VM (warm + assigned
                // respect max_vms); try_reserve fails when the host is full, so we
                // only backfill as slots free.
                let mut remaining = need;
                let mut set = tokio::task::JoinSet::new();
                loop {
                    while set.len() < conc && remaining > 0 {
                        remaining -= 1;
                        let sup = Arc::clone(&sup);
                        let class = class.clone();
                        set.spawn(async move { sup.spawn_warm(class).await });
                    }
                    let Some(result) = set.join_next().await else {
                        break;
                    };
                    match result {
                        Ok(Ok(())) => did_work = true,
                        Ok(Err(OrchError::Overloaded { .. })) => capacity_blocked = true,
                        Ok(Err(error)) => tracing::warn!("warm spawn failed: {error}"),
                        Err(error) => tracing::warn!("warm spawn task failed: {error}"),
                    }
                }
            }
            // Idle only when the pool is full; under drain, loop immediately so
            // refill tracks the take rate instead of pausing 150ms every cycle.
            if let Some(delay) = replenishment_delay(did_work, capacity_blocked) {
                tokio::time::sleep(delay).await;
            }
        }
    });
}

fn replenishment_delay(did_work: bool, capacity_blocked: bool) -> Option<Duration> {
    (!did_work || capacity_blocked).then_some(Duration::from_millis(150))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_capacity_replenishment_uses_the_bounded_idle_delay() {
        assert_eq!(
            replenishment_delay(true, true),
            Some(Duration::from_millis(150)),
            "an Overloaded refill attempt must not spin the replenishment loop"
        );
    }
}
