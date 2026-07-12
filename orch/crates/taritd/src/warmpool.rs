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
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

/// Spawn the background replenishment loop. No-op unless the pool is enabled.
pub fn spawn_replenisher(
    sup: Arc<VmmSupervisor>,
    config: Config,
    scheduler: Arc<Scheduler>,
    mut shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> Option<JoinHandle<()>> {
    if !config.warm_pool.enabled {
        return None;
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

    let cancelled = Arc::new(AtomicBool::new(false));
    Some(tokio::spawn(async move {
        'replenish: loop {
            if shutdown_pending(&shutdown_rx) {
                cancelled.store(true, Ordering::Release);
                break;
            }
            let mut did_work = false;
            for class in &classes {
                if shutdown_pending(&shutdown_rx) {
                    cancelled.store(true, Ordering::Release);
                    break 'replenish;
                }
                let have = {
                    let sup = Arc::clone(&sup);
                    let (v, m) = (class.vcpus, class.memory_mib);
                    await_blocking(
                        tokio::task::spawn_blocking(move || sup.warm_count(v, m)),
                        &cancelled,
                        &mut shutdown_rx,
                    )
                    .await
                    .unwrap_or(0)
                };
                if cancelled.load(Ordering::Acquire) {
                    break 'replenish;
                }
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
                            let blocking_cancelled = Arc::clone(&cancelled);
                            match await_blocking(
                                tokio::task::spawn_blocking(move || {
                                    if blocking_cancelled.load(Ordering::Acquire) {
                                        return Err(tarit_types::OrchError::Overloaded {
                                            message: "taritd is shutting down".into(),
                                            retry_after_secs: 1,
                                        });
                                    }
                                    sup.create_golden_cancellable(&class, &blocking_cancelled)
                                }),
                                &cancelled,
                                &mut shutdown_rx,
                            )
                            .await
                            {
                                Ok(Ok(path)) => {
                                    sched.release();
                                    if cancelled.load(Ordering::Acquire) {
                                        continue;
                                    }
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
                        await_blocking(
                            tokio::task::spawn_blocking(move || sup.warm_count(v, m)),
                            &cancelled,
                            &mut shutdown_rx,
                        )
                        .await
                        .unwrap_or(0)
                    };
                    if cancelled.load(Ordering::Acquire) {
                        break 'replenish;
                    }
                    let need = class.refill_needed(have);
                    if need == 0 {
                        continue;
                    }
                    let to_spawn = need.min(conc);
                    let mut spawned = 0usize;
                    let mut set = JoinSet::new();
                    for _ in 0..to_spawn {
                        if shutdown_pending(&shutdown_rx) {
                            cancelled.store(true, Ordering::Release);
                            break;
                        }
                        if !scheduler.try_reserve() {
                            break;
                        }
                        spawned += 1;
                        let sup = Arc::clone(&sup);
                        let sched = Arc::clone(&scheduler);
                        let class = class.clone();
                        let snapshot_path = snapshot_path.clone();
                        let cancelled = Arc::clone(&cancelled);
                        set.spawn_blocking(move || {
                            if cancelled.load(Ordering::Acquire) {
                                sched.release();
                            } else if let Err(e) = sup.spawn_warm_restore_cancellable(
                                &class,
                                &snapshot_path,
                                &cancelled,
                            ) {
                                sched.release();
                                tracing::warn!("warm restore spawn failed: {e}");
                            }
                        });
                    }
                    if !await_blocking_set(&mut set, &cancelled, &mut shutdown_rx).await {
                        break 'replenish;
                    }
                    did_work |= spawned > 0;
                    continue;
                }
                // Keep bounded cold boots in flight while each blocking task
                // cooperatively observes shutdown before it creates a VM.
                let mut remaining = need;
                while remaining > 0 {
                    let mut set = JoinSet::new();
                    for _ in 0..remaining.min(conc) {
                        if shutdown_pending(&shutdown_rx) {
                            cancelled.store(true, Ordering::Release);
                            break;
                        }
                        if !scheduler.try_reserve() {
                            remaining = 0;
                            break;
                        }
                        remaining -= 1;
                        did_work = true;
                        let sup = Arc::clone(&sup);
                        let sched = Arc::clone(&scheduler);
                        let class = class.clone();
                        let cancelled = Arc::clone(&cancelled);
                        set.spawn_blocking(move || {
                            if cancelled.load(Ordering::Acquire) {
                                sched.release();
                            } else if let Err(e) = sup.spawn_warm_cancellable(&class, &cancelled) {
                                sched.release();
                                tracing::warn!("warm spawn failed: {e}");
                            }
                        });
                    }
                    if !await_blocking_set(&mut set, &cancelled, &mut shutdown_rx).await {
                        break 'replenish;
                    }
                }
            }
            // Idle only when the pool is full; under drain, loop immediately so
            // refill tracks the take rate instead of pausing 150ms every cycle.
            if !did_work {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(150)) => {}
                    _ = wait_for_shutdown(&mut shutdown_rx) => {
                        cancelled.store(true, Ordering::Release);
                        break;
                    }
                }
            }
        }
    }))
}

async fn await_blocking<T>(
    mut handle: JoinHandle<T>,
    cancelled: &AtomicBool,
    shutdown_rx: &mut watch::Receiver<Option<&'static str>>,
) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
{
    tokio::select! {
        result = &mut handle => result,
        _ = wait_for_shutdown(shutdown_rx) => {
            cancelled.store(true, Ordering::Release);
            handle.await
        }
    }
}

async fn await_blocking_set(
    set: &mut JoinSet<()>,
    cancelled: &AtomicBool,
    shutdown_rx: &mut watch::Receiver<Option<&'static str>>,
) -> bool {
    while !set.is_empty() {
        tokio::select! {
            _ = set.join_next() => {}
            _ = wait_for_shutdown(shutdown_rx) => {
                cancelled.store(true, Ordering::Release);
                while set.join_next().await.is_some() {}
                return false;
            }
        }
    }
    true
}

fn shutdown_pending(shutdown_rx: &watch::Receiver<Option<&'static str>>) -> bool {
    shutdown_rx.borrow().is_some()
}

async fn wait_for_shutdown(shutdown_rx: &mut watch::Receiver<Option<&'static str>>) {
    loop {
        if shutdown_pending(shutdown_rx) {
            return;
        }
        if shutdown_rx.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;
    use tokio::sync::watch;

    #[tokio::test]
    async fn shutdown_waits_for_cooperatively_cancelled_blocking_work() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(None);
        let completed = Arc::new(AtomicBool::new(false));
        let blocking_cancelled = Arc::clone(&cancelled);
        let blocking_completed = Arc::clone(&completed);
        let handle = tokio::task::spawn_blocking(move || {
            while !blocking_cancelled.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            blocking_completed.store(true, Ordering::Release);
        });

        shutdown_tx.send(Some("test")).unwrap();
        super::await_blocking(handle, &cancelled, &mut shutdown_rx)
            .await
            .unwrap();

        assert!(cancelled.load(Ordering::Acquire));
        assert!(completed.load(Ordering::Acquire));
    }
}
