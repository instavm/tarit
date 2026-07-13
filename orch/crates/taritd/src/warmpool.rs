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
use tokio::sync::{oneshot, watch, Mutex as AsyncMutex};
use tokio::task::{JoinHandle, JoinSet};

#[must_use = "warm-pool work must be quiesced before the supervisor sweep"]
pub(crate) struct Replenisher {
    handle: JoinHandle<()>,
    children: Arc<BlockingChildren>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct BlockingChildren {
    handles: AsyncMutex<JoinSet<()>>,
}

impl BlockingChildren {
    async fn spawn<T, Work>(&self, work: Work) -> oneshot::Receiver<T>
    where
        T: Send + 'static,
        Work: FnOnce() -> T + Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();
        self.handles.lock().await.spawn_blocking(move || {
            let _ = result_tx.send(work());
        });
        result_rx
    }

    async fn reap_finished(&self) {
        let mut handles = self.handles.lock().await;
        while let Some(result) = handles.try_join_next() {
            if let Err(error) = result {
                tracing::warn!(%error, "warm-pool blocking task failed");
            }
        }
    }

    async fn join_all(&self) {
        let mut handles = self.handles.lock().await;
        while let Some(result) = handles.join_next().await {
            if let Err(error) = result {
                tracing::warn!(%error, "warm-pool blocking task failed while stopping");
            }
        }
    }
}

impl Replenisher {
    pub(crate) async fn quiesce(self, warning_after: Duration) {
        self.cancelled.store(true, Ordering::Release);
        let mut handle = self.handle;
        match tokio::time::timeout(warning_after, &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "warm-pool task failed while stopping");
            }
            Err(_) => {
                tracing::warn!(
                    "warm-pool work is still quiescing after shutdown; waiting before sweep"
                );
                if let Err(error) = handle.await {
                    tracing::warn!(%error, "warm-pool task failed while stopping");
                }
            }
        }
        self.children.join_all().await;
    }

    #[cfg(test)]
    pub(crate) fn for_test(handle: JoinHandle<()>) -> Self {
        Self {
            handle,
            children: Arc::new(BlockingChildren::default()),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Spawn the background replenishment loop. No-op unless the pool is enabled.
pub(crate) fn spawn_replenisher(
    sup: Arc<VmmSupervisor>,
    config: Config,
    _scheduler: Arc<Scheduler>,
    mut shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> Option<Replenisher> {
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
    let children = Arc::new(BlockingChildren::default());
    let worker_cancelled = Arc::clone(&cancelled);
    let worker_children = Arc::clone(&children);
    Some(Replenisher {
        handle: tokio::spawn(async move {
            'replenish: loop {
                worker_children.reap_finished().await;
                if worker_cancelled.load(Ordering::Acquire) || shutdown_pending(&shutdown_rx) {
                    worker_cancelled.store(true, Ordering::Release);
                    break;
                }
                let mut did_work = false;
                for class in &classes {
                    if worker_cancelled.load(Ordering::Acquire) || shutdown_pending(&shutdown_rx) {
                        worker_cancelled.store(true, Ordering::Release);
                        break 'replenish;
                    }
                    let have = {
                        let sup = Arc::clone(&sup);
                        let (v, m) = (class.vcpus, class.memory_mib);
                        await_blocking(
                            worker_children.spawn(move || sup.warm_count(v, m)).await,
                            &worker_cancelled,
                            &mut shutdown_rx,
                        )
                        .await
                        .unwrap_or(0)
                    };
                    if worker_cancelled.load(Ordering::Acquire) {
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
                                let sup = Arc::clone(&sup);
                                let class = class.clone();
                                let blocking_cancelled = Arc::clone(&worker_cancelled);
                                match await_blocking(
                                    worker_children
                                        .spawn(move || {
                                            if blocking_cancelled.load(Ordering::Acquire) {
                                                Err(tarit_types::OrchError::Overloaded {
                                                    message: "taritd is shutting down".into(),
                                                    retry_after_secs: 1,
                                                })
                                            } else {
                                                tokio::runtime::Handle::current()
                                                    .block_on(sup.create_golden(class))
                                            }
                                        })
                                        .await,
                                    &worker_cancelled,
                                    &mut shutdown_rx,
                                )
                                .await
                                {
                                    Ok(Ok(path)) => {
                                        if worker_cancelled.load(Ordering::Acquire) {
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
                                        tracing::warn!("warm golden create failed: {e}");
                                    }
                                    Err(e) => {
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
                                worker_children.spawn(move || sup.warm_count(v, m)).await,
                                &worker_cancelled,
                                &mut shutdown_rx,
                            )
                            .await
                            .unwrap_or(0)
                        };
                        if worker_cancelled.load(Ordering::Acquire) {
                            break 'replenish;
                        }
                        let need = class.refill_needed(have);
                        if need == 0 {
                            continue;
                        }
                        let to_spawn = need.min(conc);
                        let mut spawned = 0usize;
                        let mut set = Vec::with_capacity(to_spawn);
                        for _ in 0..to_spawn {
                            if worker_cancelled.load(Ordering::Acquire)
                                || shutdown_pending(&shutdown_rx)
                            {
                                worker_cancelled.store(true, Ordering::Release);
                                break;
                            }
                            spawned += 1;
                            let sup = Arc::clone(&sup);
                            let class = class.clone();
                            let snapshot_path = snapshot_path.clone();
                            let cancelled = Arc::clone(&worker_cancelled);
                            set.push(
                                worker_children
                                    .spawn(move || {
                                        if cancelled.load(Ordering::Acquire) {
                                        } else if let Err(e) = tokio::runtime::Handle::current()
                                            .block_on(sup.spawn_warm_restore(class, snapshot_path))
                                        {
                                            tracing::warn!("warm restore spawn failed: {e}");
                                        }
                                    })
                                    .await,
                            );
                        }
                        if !await_blocking_set(&mut set, &worker_cancelled, &mut shutdown_rx).await
                        {
                            break 'replenish;
                        }
                        did_work |= spawned > 0;
                        continue;
                    }
                    // Keep bounded cold boots in flight while each blocking task
                    // cooperatively observes shutdown before it creates a VM.
                    let mut remaining = need;
                    while remaining > 0 {
                        let mut set = Vec::with_capacity(remaining.min(conc));
                        for _ in 0..remaining.min(conc) {
                            if worker_cancelled.load(Ordering::Acquire)
                                || shutdown_pending(&shutdown_rx)
                            {
                                worker_cancelled.store(true, Ordering::Release);
                                break;
                            }
                            remaining -= 1;
                            did_work = true;
                            let sup = Arc::clone(&sup);
                            let class = class.clone();
                            let cancelled = Arc::clone(&worker_cancelled);
                            set.push(
                                worker_children
                                    .spawn(move || {
                                        if cancelled.load(Ordering::Acquire) {
                                        } else if let Err(e) = tokio::runtime::Handle::current()
                                            .block_on(sup.spawn_warm(class))
                                        {
                                            tracing::warn!("warm spawn failed: {e}");
                                        }
                                    })
                                    .await,
                            );
                        }
                        if !await_blocking_set(&mut set, &worker_cancelled, &mut shutdown_rx).await
                        {
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
                            worker_cancelled.store(true, Ordering::Release);
                            break;
                        }
                    }
                }
            }
        }),
        children,
        cancelled,
    })
}

async fn await_blocking<T>(
    mut result: oneshot::Receiver<T>,
    cancelled: &AtomicBool,
    shutdown_rx: &mut watch::Receiver<Option<&'static str>>,
) -> Result<T, oneshot::error::RecvError>
where
    T: Send + 'static,
{
    if cancelled.load(Ordering::Acquire) {
        return result.await;
    }
    tokio::select! {
        result = &mut result => result,
        _ = wait_for_shutdown(shutdown_rx) => {
            cancelled.store(true, Ordering::Release);
            result.await
        }
    }
}

async fn await_blocking_set(
    set: &mut Vec<oneshot::Receiver<()>>,
    cancelled: &AtomicBool,
    shutdown_rx: &mut watch::Receiver<Option<&'static str>>,
) -> bool {
    while let Some(mut result) = set.pop() {
        if cancelled.load(Ordering::Acquire) {
            let _ = result.await;
            continue;
        }
        tokio::select! {
            _ = &mut result => {}
            _ = wait_for_shutdown(shutdown_rx) => {
                cancelled.store(true, Ordering::Release);
                let _ = result.await;
                while let Some(result) = set.pop() {
                    let _ = result.await;
                }
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
        mpsc, Arc,
    };
    use std::time::Duration;
    use tokio::sync::{oneshot, watch};

    #[tokio::test]
    async fn shutdown_waits_for_cooperatively_cancelled_blocking_work() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(None);
        let completed = Arc::new(AtomicBool::new(false));
        let blocking_cancelled = Arc::clone(&cancelled);
        let blocking_completed = Arc::clone(&completed);
        let children = Arc::new(super::BlockingChildren::default());
        let handle = children
            .spawn(move || {
                while !blocking_cancelled.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                blocking_completed.store(true, Ordering::Release);
            })
            .await;

        shutdown_tx.send(Some("test")).unwrap();
        super::await_blocking(handle, &cancelled, &mut shutdown_rx)
            .await
            .unwrap();
        children.join_all().await;

        assert!(cancelled.load(Ordering::Acquire));
        assert!(completed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn quiesce_awaits_registered_blocking_work_after_parent_abort() {
        let children = Arc::new(super::BlockingChildren::default());
        let cancelled = Arc::new(AtomicBool::new(false));
        let created = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let child_cancelled = Arc::clone(&cancelled);
        let child_created = Arc::clone(&created);
        let result = children
            .spawn(move || {
                let _ = started_tx.send(());
                release_rx.recv().unwrap();
                if !child_cancelled.load(Ordering::Acquire) {
                    child_created.store(true, Ordering::Release);
                }
            })
            .await;
        let parent = tokio::spawn(async move {
            let _ = result.await;
        });
        started_rx.await.unwrap();

        let replenisher = super::Replenisher {
            handle: parent,
            children: Arc::clone(&children),
            cancelled,
        };
        replenisher.handle.abort();
        let mut quiesce = tokio::spawn(replenisher.quiesce(Duration::from_millis(5)));

        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut quiesce)
                .await
                .is_err(),
            "registered blocking work must survive parent abort and hold shutdown"
        );
        assert!(!created.load(Ordering::Acquire));

        release_tx.send(()).unwrap();
        quiesce.await.unwrap();
        assert!(
            !created.load(Ordering::Acquire),
            "shutdown cancellation must make late warm creation harmless"
        );
    }
}
