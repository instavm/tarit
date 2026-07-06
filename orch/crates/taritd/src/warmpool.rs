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
use crate::scheduler::Scheduler;
use crate::supervisor::{VmSpawnConfig, VmmSupervisor};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Spawn the background replenishment loop. No-op unless the pool is enabled.
pub fn spawn_replenisher(sup: Arc<VmmSupervisor>, config: Config, scheduler: Arc<Scheduler>) {
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
                            if !scheduler.try_reserve() {
                                continue;
                            }
                            let sup = Arc::clone(&sup);
                            let sched = Arc::clone(&scheduler);
                            let class = class.clone();
                            match tokio::task::spawn_blocking(move || sup.create_golden(&class))
                                .await
                            {
                                Ok(Ok(path)) => {
                                    sched.release();
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
                                Ok(Err(e)) => {
                                    sched.release();
                                    tracing::warn!("warm golden create failed: {e}");
                                }
                                Err(e) => {
                                    sched.release();
                                    tracing::warn!("warm golden task failed: {e}");
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
                    let mut spawned = 0usize;
                    let mut set = tokio::task::JoinSet::new();
                    for _ in 0..to_spawn {
                        if !scheduler.try_reserve() {
                            break;
                        }
                        spawned += 1;
                        let sup = Arc::clone(&sup);
                        let sched = Arc::clone(&scheduler);
                        let class = class.clone();
                        let snapshot_path = snapshot_path.clone();
                        set.spawn_blocking(move || {
                            if let Err(e) = sup.spawn_warm_restore(&class, &snapshot_path) {
                                sched.release();
                                tracing::warn!("warm restore spawn failed: {e}");
                            }
                        });
                    }
                    while set.join_next().await.is_some() {}
                    did_work |= spawned > 0;
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
                        if !scheduler.try_reserve() {
                            remaining = 0;
                            break;
                        }
                        remaining -= 1;
                        did_work = true;
                        let sup = Arc::clone(&sup);
                        let sched = Arc::clone(&scheduler);
                        let class = class.clone();
                        set.spawn_blocking(move || {
                            if let Err(e) = sup.spawn_warm(&class) {
                                sched.release();
                                tracing::warn!("warm spawn failed: {e}");
                            }
                        });
                    }
                    if set.join_next().await.is_none() {
                        break;
                    }
                }
            }
            // Idle only when the pool is full; under drain, loop immediately so
            // refill tracks the take rate instead of pausing 150ms every cycle.
            if !did_work {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    });
}
