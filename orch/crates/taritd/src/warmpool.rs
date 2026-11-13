//! Warm-pool replenisher: keeps a buffer of pre-booted VMs ready so create()
//! can hand one out instantly instead of paying cold-boot latency.
//!
//! A background task tops each configured class up to its hysteresis target
//! after depth drops below the low watermark. Boots/restores are concurrent
//! only up to `replenish_concurrency` and never exceed the host's placement
//! capacity. When the take rate outpaces replenishment the pool simply drains
//! and create() falls back to a cold start, so the pool is a best-effort
//! accelerator, never a correctness dependency.

use crate::config::{Config, WarmClass};
use crate::scheduler::Scheduler;
use crate::supervisor::{VmSpawnConfig, VmmSupervisor};
use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tarit_store::Store;
use tarit_types::CreateVmRequest;
use tokio::sync::{oneshot, watch, Mutex as AsyncMutex};
use tokio::task::{JoinHandle, JoinSet};

pub(crate) fn validate_exact_classes(config: &Config) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for class in &config.warm_pool.classes {
        let spawn = VmSpawnConfig::from_warm_class(config, class);
        if !seen.insert(spawn) {
            anyhow::bail!(
                "warm-pool classes must resolve to unique spawn configurations; duplicate {} vCPU/{} MiB class",
                class.vcpus,
                class.memory_mib
            );
        }
    }
    Ok(())
}

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

/// The class taritd would learn for VMs of `spec`: none when learning is off,
/// a configured class already covers the shape, or a restore class cannot
/// reproduce it exactly (so a learned VM is always interchangeable with the
/// cold boot the create would otherwise get).
pub(crate) fn learnable_class(
    config: &Config,
    spec: &VmSpawnConfig,
    prewarm: Option<String>,
) -> Option<WarmClass> {
    let learn = &config.warm_pool.learn;
    if !config.warm_pool.enabled || learn.max_classes == 0 || learn.target == 0 {
        return None;
    }
    if config
        .warm_pool
        .classes
        .iter()
        .any(|class| VmSpawnConfig::from_warm_class(config, class) == *spec)
    {
        return None;
    }
    let class = WarmClass::learned(
        spec.vcpus,
        spec.memory_mib,
        spec.rootfs_path.clone(),
        learn.target,
        prewarm,
    );
    (VmSpawnConfig::from_warm_class(config, &class) == *spec).then_some(class)
}

/// How often the image seeder looks for newly registered images.
const IMAGE_SEED_INTERVAL: Duration = Duration::from_secs(10);

/// Learn a warm class for registered images, at the default VM shape: the
/// newest `learn.max_classes` when taritd starts, then each image registered
/// while it runs, so the first burst against an image is already served
/// from the pool. A seeded class nobody uses is retired like any other.
pub(crate) fn spawn_image_seeder(
    sup: Arc<VmmSupervisor>,
    config: Config,
    store: Arc<std::sync::Mutex<Store>>,
    mut shutdown_rx: watch::Receiver<Option<&'static str>>,
) -> Option<JoinHandle<()>> {
    let max_classes = config.warm_pool.learn.max_classes;
    if !config.warm_pool.enabled || max_classes == 0 || config.warm_pool.learn.target == 0 {
        return None;
    }
    Some(tokio::spawn(async move {
        // An image whose prewarm changes counts as new, so its class picks
        // the command up.
        let mut seen = HashSet::<(String, String, String, Option<String>)>::new();
        let mut startup = true;
        loop {
            let images = match store.lock() {
                Ok(store) => store.list_images().unwrap_or_else(|error| {
                    tracing::warn!(%error, "listing images for the warm pool failed");
                    Vec::new()
                }),
                Err(_) => Vec::new(),
            };
            let mut fresh = images
                .into_iter()
                .filter(|image| {
                    seen.insert((
                        image.name.clone(),
                        image.tag.clone(),
                        image.rootfs_path.clone(),
                        image.prewarm.clone(),
                    ))
                })
                .collect::<Vec<_>>();
            if std::mem::take(&mut startup) {
                fresh.sort_by_key(|image| std::cmp::Reverse(image.created_at));
                fresh.truncate(max_classes);
            }
            for image in fresh {
                if crate::image::verify_admitted_image(&image, &config.image_admission_policy)
                    .is_err()
                {
                    continue;
                }
                // What `{"image": "<name:tag>"}` resolves to.
                let request: CreateVmRequest = match serde_json::from_value(
                    serde_json::json!({ "rootfs_path": image.rootfs_path }),
                ) {
                    Ok(request) => request,
                    Err(_) => continue,
                };
                let spec = VmSpawnConfig::from_defaults(&config, &request);
                if let Some(class) = learnable_class(&config, &spec, image.prewarm.clone()) {
                    tracing::info!(image = %format!("{}:{}", image.name, image.tag), "warming a registered image");
                    if !sup.set_learned_prewarm(&spec, image.prewarm.clone()) {
                        sup.learn_warm_class(spec, class, max_classes);
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(IMAGE_SEED_INTERVAL) => {}
                _ = wait_for_shutdown(&mut shutdown_rx) => break,
            }
        }
    }))
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
    let configured = config.warm_pool.classes.clone();
    let conc = config.warm_pool.replenish_concurrency.max(1);
    tracing::info!(
        total_target = config.warm_pool.total_target(),
        classes = configured.len(),
        learn_classes = config.warm_pool.learn.max_classes,
        cpu_overcommit = config.warm_pool.cpu_overcommit,
        max_vcpus = config.max_vcpus,
        "warm pool enabled"
    );

    // A golden that cannot be built (missing rootfs, unregistered image, full
    // disk) is retried with backoff instead of on every 150 ms round.
    let mut golden_backoff = HashMap::<VmSpawnConfig, GoldenBackoff>::new();

    let cancelled = Arc::new(AtomicBool::new(false));
    let children = Arc::new(BlockingChildren::default());
    let worker_cancelled = Arc::clone(&cancelled);
    let worker_children = Arc::clone(&children);
    Some(Replenisher {
        handle: tokio::spawn(async move {
            let mut filling = HashMap::<VmSpawnConfig, bool>::new();
            // Why replenishment is paused. A pause that lasts is logged once,
            // and again when it ends, so an empty pool is never silent.
            let mut paused: Option<Pause> = None;
            'replenish: loop {
                worker_children.reap_finished().await;
                if worker_cancelled.load(Ordering::Acquire) || shutdown_pending(&shutdown_rx) {
                    worker_cancelled.store(true, Ordering::Release);
                    break;
                }
                match sup.refresh_disk_pressure() {
                    Ok(state) if state.pressured => {
                        let roots = state
                            .roots
                            .iter()
                            .filter(|root| root.pressured)
                            .map(|root| root.path.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        note_pause(&mut paused, format!("node disk pressure on {roots}"));
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                            _ = wait_for_shutdown(&mut shutdown_rx) => {
                                worker_cancelled.store(true, Ordering::Release);
                                break;
                            }
                        }
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(%error, "warm-pool disk pressure check failed");
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                            _ = wait_for_shutdown(&mut shutdown_rx) => {
                                worker_cancelled.store(true, Ordering::Release);
                                break;
                            }
                        }
                        continue;
                    }
                }
                // Creates waiting for capacity come first; refilling now would
                // take the slots they wait for.
                if sup.creates_waiting_for_capacity() {
                    note_pause(
                        &mut paused,
                        "creates are waiting for host capacity".to_string(),
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(150)) => {}
                        _ = wait_for_shutdown(&mut shutdown_rx) => {
                            worker_cancelled.store(true, Ordering::Release);
                            break;
                        }
                    }
                    continue;
                }
                if let Some(pause) = paused.take().filter(|pause| pause.logged) {
                    tracing::info!(
                        reason = %pause.reason,
                        paused_secs = pause.since.elapsed().as_secs(),
                        "warm-pool replenishment resumed"
                    );
                }
                let mut classes = configured.clone();
                classes.extend(sup.learned_warm_classes(config.warm_pool.learn.idle));
                let active = classes
                    .iter()
                    .map(|class| VmSpawnConfig::from_warm_class(&config, class))
                    .collect::<HashSet<_>>();
                filling.retain(|spec, _| active.contains(spec));
                // Retire what belonged to classes that are gone: their idle
                // warm VMs and golden snapshots.
                if sup.has_warm_outside(|spec| active.contains(spec)) {
                    let sup = Arc::clone(&sup);
                    let active = active.clone();
                    let _ = await_blocking(
                        worker_children
                            .spawn(move || {
                                while sup.reclaim_idle_warm_where(|spec| !active.contains(spec)) {}
                            })
                            .await,
                        &worker_cancelled,
                        &mut shutdown_rx,
                    )
                    .await;
                }
                sup.retain_goldens(|spec| active.contains(spec));
                let mut did_work = false;
                for class in &classes {
                    let key = VmSpawnConfig::from_warm_class(&config, class);
                    if worker_cancelled.load(Ordering::Acquire) || shutdown_pending(&shutdown_rx) {
                        worker_cancelled.store(true, Ordering::Release);
                        break 'replenish;
                    }
                    let have = {
                        let sup = Arc::clone(&sup);
                        let key = key.clone();
                        await_blocking(
                            worker_children.spawn(move || sup.warm_count(&key)).await,
                            &worker_cancelled,
                            &mut shutdown_rx,
                        )
                        .await
                        .unwrap_or(0)
                    };
                    if worker_cancelled.load(Ordering::Acquire) {
                        break 'replenish;
                    }
                    let need = class.refill_round(have, filling.entry(key.clone()).or_default());
                    if need == 0 {
                        continue;
                    }
                    if class.restore {
                        let golden_bundle = {
                            if let Some(bundle) = sup.golden(&key) {
                                Some(bundle)
                            } else if golden_backoff
                                .get(&key)
                                .is_some_and(|backoff| !backoff.due())
                            {
                                None
                            } else {
                                let golden_sup = Arc::clone(&sup);
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
                                                    .block_on(golden_sup.create_golden(class))
                                            }
                                        })
                                        .await,
                                    &worker_cancelled,
                                    &mut shutdown_rx,
                                )
                                .await
                                {
                                    Ok(Ok(bundle)) => {
                                        if worker_cancelled.load(Ordering::Acquire) {
                                            continue;
                                        }
                                        did_work = true;
                                        tracing::info!(
                                            vcpus = key.vcpus,
                                            memory_mib = key.memory_mib,
                                            rootfs = ?key.rootfs_path.as_ref(),
                                            snapshot_path = %bundle.snapshot_path(),
                                            overlay_path = ?bundle.overlay_path(),
                                            "warm golden snapshot created"
                                        );
                                        golden_backoff.remove(&key);
                                        sup.publish_golden(key.clone(), bundle);
                                    }
                                    Ok(Err(e)) => {
                                        let retry_in =
                                            golden_backoff.entry(key.clone()).or_default().failed();
                                        tracing::warn!(
                                            retry_in_secs = retry_in.as_secs(),
                                            "warm golden create failed: {e}"
                                        );
                                    }
                                    Err(e) => {
                                        let retry_in =
                                            golden_backoff.entry(key.clone()).or_default().failed();
                                        tracing::warn!(
                                            retry_in_secs = retry_in.as_secs(),
                                            "warm golden task failed: {e}"
                                        );
                                    }
                                }
                                None
                            }
                        };
                        let Some(golden_bundle) = golden_bundle else {
                            continue;
                        };
                        let have = {
                            let sup = Arc::clone(&sup);
                            let key = key.clone();
                            await_blocking(
                                worker_children.spawn(move || sup.warm_count(&key)).await,
                                &worker_cancelled,
                                &mut shutdown_rx,
                            )
                            .await
                            .unwrap_or(0)
                        };
                        if worker_cancelled.load(Ordering::Acquire) {
                            break 'replenish;
                        }
                        let need =
                            class.refill_round(have, filling.entry(key.clone()).or_default());
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
                            let golden_bundle = golden_bundle.clone();
                            let cancelled = Arc::clone(&worker_cancelled);
                            set.push(
                                worker_children
                                    .spawn(move || {
                                        if cancelled.load(Ordering::Acquire) {
                                        } else if let Err(e) = tokio::runtime::Handle::current()
                                            .block_on(sup.spawn_warm_restore(class, golden_bundle))
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

/// How long replenishment must stay paused for one reason before it is
/// logged. Creates waiting for capacity pause it briefly all the time under
/// load; only a pause that lasts explains an empty pool.
const PAUSE_LOG_AFTER: Duration = Duration::from_secs(5);

struct Pause {
    reason: String,
    since: std::time::Instant,
    logged: bool,
}

/// Record that replenishment is paused for `reason`; log it once it has
/// lasted `PAUSE_LOG_AFTER`. Returns whether this call logged it.
fn note_pause(paused: &mut Option<Pause>, reason: String) -> bool {
    if paused.as_ref().is_none_or(|pause| pause.reason != reason) {
        *paused = Some(Pause {
            reason,
            since: std::time::Instant::now(),
            logged: false,
        });
    }
    let Some(pause) = paused.as_mut() else {
        return false;
    };
    if pause.logged || pause.since.elapsed() < PAUSE_LOG_AFTER {
        return false;
    }
    pause.logged = true;
    tracing::warn!(
        reason = %pause.reason,
        paused_secs = pause.since.elapsed().as_secs(),
        "warm-pool replenishment paused"
    );
    true
}

const GOLDEN_RETRY_FIRST: Duration = Duration::from_secs(1);
const GOLDEN_RETRY_MAX: Duration = Duration::from_secs(60);

/// When a class whose golden failed to build may try again: one second after
/// the first failure, doubling up to a minute.
#[derive(Default)]
struct GoldenBackoff {
    delay: Option<Duration>,
    next_attempt: Option<std::time::Instant>,
}

impl GoldenBackoff {
    fn due(&self) -> bool {
        self.next_attempt
            .is_none_or(|at| std::time::Instant::now() >= at)
    }

    fn failed(&mut self) -> Duration {
        let delay = self.delay.map_or(GOLDEN_RETRY_FIRST, |delay| {
            (delay * 2).min(GOLDEN_RETRY_MAX)
        });
        self.delay = Some(delay);
        self.next_attempt = Some(std::time::Instant::now() + delay);
        delay
    }
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

    #[test]
    fn a_lasting_replenishment_pause_is_logged_once() {
        use super::{note_pause, PAUSE_LOG_AFTER};

        let mut paused = None;
        assert!(!note_pause(&mut paused, "disk".into()), "too early to log");
        let pause = paused.as_mut().expect("pause recorded");
        pause.since = pause
            .since
            .checked_sub(PAUSE_LOG_AFTER)
            .expect("monotonic clock");
        assert!(
            note_pause(&mut paused, "disk".into()),
            "lasting pause is logged"
        );
        assert!(!note_pause(&mut paused, "disk".into()), "logged only once");
        assert!(paused.as_ref().is_some_and(|pause| pause.logged));
        // A new reason starts a new pause.
        assert!(!note_pause(&mut paused, "capacity".into()));
        assert!(paused.as_ref().is_some_and(|pause| !pause.logged));
    }

    #[test]
    fn a_failing_golden_backs_off_up_to_a_minute() {
        let mut backoff = super::GoldenBackoff::default();
        assert!(backoff.due());
        assert_eq!(backoff.failed(), Duration::from_secs(1));
        assert!(!backoff.due());
        let delays: Vec<u64> = (0..8).map(|_| backoff.failed().as_secs()).collect();
        assert_eq!(delays, [2, 4, 8, 16, 32, 60, 60, 60]);
    }

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
