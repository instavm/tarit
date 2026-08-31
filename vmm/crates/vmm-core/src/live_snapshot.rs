//! Live snapshot executor — wires the pre-copy convergence algorithm
//! to real KVM dirty-log, running while the vCPUs keep executing.
//!
//! Design: while the guest runs, copy memory pages out to the snapshot
//! buffer. Re-read the dirty set, copy only newly-dirtied pages; repeat.
//! When the remaining dirty set is small enough that it can be copied
//! within the target downtime, do a brief final stop that copies only the
//! residual pages and captures all-vCPU + device state.

#![cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]

use crate::error::{Result, VmmError};
use crate::kvm::KvmVm;
use crate::vcpu_thread::VcpuThread;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};
use vmm_memory_backend::dirty::DirtyBitmap;
use vmm_memory_backend::GuestMemory;
use vmm_snapshot::live::{decide, PrecopyParams, RoundDecision};

pub const PAGE_SIZE: u64 = 4096;

/// Minimum number of bytes a round must copy before its timing is trusted as
/// a bandwidth sample. Short copies are dominated by timer noise, and an
/// over-estimated bandwidth makes [`decide`] stop immediately — which is what
/// previously made the pre-copy loop a no-op.
const BANDWIDTH_SAMPLE_MIN_BYTES: u64 = 1 << 20;

/// Fallback bandwidth estimate when no round has produced a usable sample.
const DEFAULT_COPY_BANDWIDTH_BPS: u64 = 500_000_000;

/// Pause between pre-copy rounds, so a guest that is dirtying very little
/// does not turn the loop into a `KVM_GET_DIRTY_LOG` spin.
const ROUND_IDLE_SLEEP: Duration = Duration::from_millis(10);

#[cfg(feature = "test-failpoints")]
fn inject_live_snapshot_failure(phase: &str) -> Result<()> {
    if std::env::var_os("TARIT_TEST_LIVE_SNAPSHOT_FAIL_PHASE").as_deref()
        == Some(std::ffi::OsStr::new(phase))
    {
        return Err(VmmError::Snapshot(format!(
            "injected live snapshot failure at {phase}"
        )));
    }
    Ok(())
}

#[cfg(not(feature = "test-failpoints"))]
fn inject_live_snapshot_failure(_phase: &str) -> Result<()> {
    Ok(())
}

struct VcpuPauseGuard<'a> {
    vcpu_threads: Vec<&'a VcpuThread>,
    armed: bool,
}

impl<'a> VcpuPauseGuard<'a> {
    fn pause_all(vcpu_threads: &[&'a VcpuThread]) -> Result<Self> {
        if vcpu_threads.is_empty() {
            return Err(VmmError::Snapshot(
                "live snapshot has no vCPU threads".into(),
            ));
        }
        let guard = Self {
            vcpu_threads: vcpu_threads.to_vec(),
            armed: true,
        };
        // Arm every vCPU before waiting for any acknowledgement. The guard is
        // already active, so a partial request failure resumes every thread.
        for vcpu_thread in &guard.vcpu_threads {
            if let Err(error) = vcpu_thread.request_snapshot_pause() {
                return Err(finish_failed_vcpu_pause(error, guard));
            }
        }
        for vcpu_thread in &guard.vcpu_threads {
            if let Err(error) = vcpu_thread.wait_snapshot_paused() {
                return Err(finish_failed_vcpu_pause(error, guard));
            }
        }
        Ok(guard)
    }

    fn resume(mut self) -> Result<()> {
        if self.armed {
            for vcpu_thread in &self.vcpu_threads {
                vcpu_thread.resume();
            }
            // Every control flag is clear now. Disarm before waiting so a
            // failed acknowledgement does not issue a second, unobserved
            // resume request from Drop.
            self.armed = false;
            for vcpu_thread in &self.vcpu_threads {
                vcpu_thread.wait_snapshot_resumed()?;
            }
        }
        Ok(())
    }

    /// Disarm automatic resume while intentionally leaving every vCPU at the
    /// snapshot pause boundary. Used when device workers could not prove that
    /// they left quiescence; running the guest in that state would be unsafe.
    fn keep_paused(mut self) {
        self.armed = false;
    }
}

impl Drop for VcpuPauseGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            for vcpu_thread in &self.vcpu_threads {
                vcpu_thread.resume();
            }
        }
    }
}

/// RAII guard around the caller's I/O quiesce hook: after vCPUs stop producing
/// descriptors, engaging it drains and parks every VMM thread that writes
/// guest memory (net/vsock pumps). Disengaging, or dropping on an error path,
/// releases them.
struct IoQuiesceGuard<'a> {
    quiesce: &'a dyn Fn(bool) -> Result<()>,
    armed: bool,
}

/// KVM_GET_DIRTY_LOG clears the bits it returns. If any later snapshot step
/// fails, replay all consumed pages into the host tracker so a subsequent diff
/// cannot silently omit writes observed by this attempt.
struct DirtyReplayGuard<'a> {
    mem: &'a GuestMemory,
    dirty: DirtyBitmap,
    armed: bool,
}

impl<'a> DirtyReplayGuard<'a> {
    fn new(mem: &'a GuestMemory) -> Self {
        Self {
            mem,
            dirty: DirtyBitmap::new(),
            armed: true,
        }
    }

    fn merge(&mut self, dirty: &DirtyBitmap) {
        self.dirty.merge(dirty);
    }

    fn disarm(mut self) -> DirtyBitmap {
        self.armed = false;
        std::mem::replace(&mut self.dirty, DirtyBitmap::new())
    }
}

impl Drop for DirtyReplayGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for pfn in self.dirty.dirty_pfns() {
            self.mem
                .mark_host_dirty(pfn.saturating_mul(PAGE_SIZE), PAGE_SIZE);
        }
    }
}

impl<'a> IoQuiesceGuard<'a> {
    fn engage(quiesce: &'a dyn Fn(bool) -> Result<()>) -> Result<Self> {
        quiesce(true)?;
        Ok(Self {
            quiesce,
            armed: true,
        })
    }

    fn disengage(mut self) -> Result<()> {
        if self.armed {
            self.armed = false;
            (self.quiesce)(false)?;
        }
        Ok(())
    }
}

impl Drop for IoQuiesceGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = (self.quiesce)(false);
        }
    }
}

trait FinalIoRelease {
    fn release(self) -> Result<()>;
}

impl FinalIoRelease for IoQuiesceGuard<'_> {
    fn release(self) -> Result<()> {
        self.disengage()
    }
}

trait FinalVcpuRelease {
    fn resume(self) -> Result<()>;
    fn keep_paused(self);
}

impl FinalVcpuRelease for VcpuPauseGuard<'_> {
    fn resume(self) -> Result<()> {
        VcpuPauseGuard::resume(self)
    }

    fn keep_paused(self) {
        VcpuPauseGuard::keep_paused(self);
    }
}

/// A failed all-vCPU pause must not report a recoverable source until every
/// armed thread has acknowledged the compensating resume.
fn finish_failed_vcpu_pause<V: FinalVcpuRelease>(primary: VmmError, vcpus: V) -> VmmError {
    match vcpus.resume() {
        Ok(()) => primary,
        Err(resume_error) => VmmError::Snapshot(format!(
            "{primary}; failed to confirm vCPU rollback: {resume_error}"
        )),
    }
}

/// Leave the final-stop boundary in a fail-closed order. Device workers must
/// prove they are running before vCPUs can leave their pause. Capture failures
/// still restore the source when that ordering succeeds.
fn finish_final_stop<T, I, V>(capture: Result<T>, io: I, vcpus: V) -> Result<T>
where
    I: FinalIoRelease,
    V: FinalVcpuRelease,
{
    if let Err(io_error) = io.release() {
        vcpus.keep_paused();
        return Err(match capture {
            Ok(_) => io_error,
            Err(capture_error) => VmmError::Snapshot(format!(
                "{capture_error}; failed to resume I/O workers: {io_error}"
            )),
        });
    }

    let vcpu_resume = vcpus.resume();
    match (capture, vcpu_resume) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(capture_error), Err(resume_error)) => Err(VmmError::Snapshot(format!(
            "{capture_error}; failed to resume vCPUs: {resume_error}"
        ))),
    }
}

/// Configuration for a live snapshot.
#[derive(Debug, Clone)]
pub struct LiveSnapshotConfig {
    /// Target final-stop downtime in microseconds.
    pub target_downtime_us: u64,
    /// Max pre-copy rounds before forcing stop.
    pub max_rounds: u32,
    /// Time budget for background pre-copy. Once exhausted, the state machine
    /// enters the coherent final stop; residual copy and durable writeback are
    /// measured separately rather than hidden inside this budget.
    pub timeout_secs: u64,
}

impl Default for LiveSnapshotConfig {
    fn default() -> Self {
        Self {
            target_downtime_us: 500, // 0.5ms — the pause must stay imperceptible
            max_rounds: 20,
            timeout_secs: 30,
        }
    }
}

/// Result of a live snapshot.
#[derive(Debug, Clone)]
pub struct LiveSnapshotResult {
    /// Number of rounds executed. Round 1 is the bulk copy.
    pub rounds: u32,
    /// Pages actually copied across all rounds, including the residual pages
    /// copied during the final stop.
    pub pages_copied: u64,
    /// Residual dirty set size (pages) copied during the final stop.
    pub final_dirty_pages: u64,
    /// Total elapsed time.
    pub elapsed: Duration,
    /// The guest blackout: how long the vCPU was paused for the final stop.
    pub downtime: Duration,
    /// The decision that ended the pre-copy loop.
    pub final_decision: RoundDecision,
    /// Why pre-copy stopped. This is distinct from `final_decision` so timeout
    /// and max-round exits cannot masquerade as convergence.
    pub termination: LiveSnapshotTermination,
    /// Size of the captured memory image. The image itself is streamed to
    /// `snapshot_path` and freed, so a live snapshot does not pin a second
    /// copy of guest RAM for the caller's lifetime.
    pub mem_bytes: u64,
    /// On-disk path where the controller persisted this live snapshot (a
    /// private per-process scratch path). Empty until the controller sets it.
    pub snapshot_path: String,
    /// Optional private disk upper captured during the same final stop as RAM
    /// and device state. Empty until the controller performs that capture.
    pub overlay_path: Option<String>,
    /// Chunk-integrity sidecar generated while assembling the live snapshot.
    pub integrity_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveSnapshotTermination {
    Converged,
    Diverging,
    Timeout,
    MaxRounds,
}

/// Everything a live snapshot produces. Guest RAM is already in the caller's
/// private staging file; only bounded metadata remains in memory here.
pub struct LiveSnapshotOutput {
    pub result: LiveSnapshotResult,
    /// State blob captured during the final stop, carrying the vCPU registers,
    /// VM (irqchip/PIT/clock), serial and virtio state as of the blackout.
    pub state_blob: Vec<u8>,
    /// Every dirty bit this snapshot consumed from KVM. `KVM_GET_DIRTY_LOG`
    /// clears the bitmap it reports, so these must be replayed into the VM's
    /// host-dirty tracker or a later diff snapshot would silently omit them.
    pub consumed_dirty: DirtyBitmap,
}

/// Copy one contiguous half-open PFN run. Dirty bitmaps are ordered, so a
/// guest rewriting a large buffer should cost one positioned write per run,
/// not one syscall and one filesystem extent update per 4 KiB page.
fn copy_page_run(mem: &GuestMemory, dest: &File, start_pfn: u64, end_pfn: u64) -> Result<u64> {
    let pages = end_pfn
        .checked_sub(start_pfn)
        .ok_or_else(|| VmmError::Memory("dirty page run is reversed".into()))?;
    let gpa = start_pfn
        .checked_mul(PAGE_SIZE)
        .ok_or_else(|| VmmError::Memory("dirty page offset overflow".into()))?;
    let len = pages
        .checked_mul(PAGE_SIZE)
        .ok_or_else(|| VmmError::Memory("dirty page run length overflow".into()))?;
    let end = gpa
        .checked_add(len)
        .ok_or_else(|| VmmError::Memory("dirty page run end overflow".into()))?;
    if end > mem.size_bytes {
        return Err(VmmError::Memory(format!(
            "dirty page run {gpa:#x}..{end:#x} exceeds guest RAM"
        )));
    }
    let offset = usize::try_from(gpa)
        .map_err(|_| VmmError::Memory("guest page offset does not fit usize".into()))?;
    let len = usize::try_from(len)
        .map_err(|_| VmmError::Memory("dirty page run length does not fit usize".into()))?;
    // SAFETY: GuestMemory guarantees this mapping for `size_bytes`; the range
    // was checked above and the file write does not retain the pointer.
    let src = unsafe { std::slice::from_raw_parts(mem.as_ptr().add(offset), len) };
    dest.write_all_at(src, gpa)
        .map_err(|error| VmmError::Snapshot(format!("write staged RAM at {gpa:#x}: {error}")))?;
    Ok(pages)
}

/// Copy every page named by `dirty` into the staged file.
fn copy_dirty_pages(mem: &GuestMemory, dest: &File, dirty: &DirtyBitmap) -> Result<u64> {
    let mut copied = 0u64;
    let max_pfn = mem.size_bytes / PAGE_SIZE;
    let mut run_start = None;
    let mut run_end = 0u64;
    let mut pfns = dirty
        .dirty_pfns()
        .iter()
        .copied()
        .filter(|pfn| *pfn < max_pfn)
        .collect::<Vec<_>>();
    pfns.sort_unstable();
    for pfn in pfns {
        match run_start {
            Some(_) if pfn == run_end => run_end += 1,
            Some(start) => {
                copied += copy_page_run(mem, dest, start, run_end)?;
                run_start = Some(pfn);
                run_end = pfn + 1;
            }
            None => {
                run_start = Some(pfn);
                run_end = pfn + 1;
            }
        }
    }
    if let Some(start) = run_start {
        copied += copy_page_run(mem, dest, start, run_end)?;
    }
    Ok(copied)
}

fn drop_staged_cache(file: &File, len: u64) {
    use std::os::fd::AsRawFd;

    let Ok(len) = libc::off_t::try_from(len) else {
        return;
    };
    // SAFETY: the fd remains open and the numeric range fits off_t.
    let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, len, libc::POSIX_FADV_DONTNEED) };
    if rc != 0 {
        log::warn!(
            "live_snapshot: drop staged RAM cache failed: {}",
            std::io::Error::from_raw_os_error(rc)
        );
    }
}

/// Bytes/sec implied by copying `bytes` in `elapsed`, or `None` when the
/// sample is too small or too short to be meaningful.
fn bandwidth_sample(bytes: u64, elapsed: Duration) -> Option<u64> {
    if bytes < BANDWIDTH_SAMPLE_MIN_BYTES {
        return None;
    }
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return None;
    }
    let bps = bytes as f64 / secs;
    (bps.is_finite() && bps >= 1.0).then_some(bps as u64)
}

/// Bytes/sec at which the guest dirtied `bytes` over `elapsed`. Never zero:
/// [`decide`] compares this against the copy bandwidth.
fn dirty_rate_sample(bytes: u64, elapsed: Duration) -> u64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return u64::MAX;
    }
    let bps = bytes as f64 / secs;
    if !bps.is_finite() || bps < 1.0 {
        return 1;
    }
    bps as u64
}

/// Execute a live snapshot of a running VM.
///
/// Flow:
/// 1. Pause every vCPU, enable dirty logging, clear the baseline, resume.
/// 2. Bulk round: copy all of guest RAM while the vCPUs run.
/// 3. Pre-copy rounds: read the dirty log, copy those pages, check convergence.
/// 4. Final stop: quiesce device I/O threads, pause every vCPU, copy the
///    residual dirty pages, capture all-vCPU + device state, resume.
///
/// Dirty pages come from two sources, and both are consulted every round:
/// KVM's dirty log (guest vCPU writes) and the software host-dirty tracker
/// (virtio DMA — used rings, blk/net/vsock payloads — written by VMM threads,
/// which KVM's log cannot see).
///
/// `quiesce_io(true)` must park every non-vCPU thread that writes guest
/// memory and only return once they have acknowledged; `quiesce_io(false)`
/// releases them. It is engaged *before* the final all-vCPU pause — its
/// handshake costs guest I/O stall, never guest downtime.
///
/// `capture_state` runs while every vCPU is paused for the final stop, so the
/// state blob it returns is coherent with the memory image.
pub fn live_snapshot<F>(
    kvm_vm: &KvmVm,
    mem: &GuestMemory,
    vcpu_threads: &[&VcpuThread],
    config: &LiveSnapshotConfig,
    memory_file: &File,
    quiesce_io: &dyn Fn(bool) -> Result<()>,
    capture_state: F,
) -> Result<LiveSnapshotOutput>
where
    F: FnOnce() -> Result<Vec<u8>>,
{
    let start = Instant::now();
    let timeout = Duration::from_secs(config.timeout_secs);
    let mem_size = usize::try_from(mem.size_bytes)
        .map_err(|_| VmmError::Memory("guest memory too large to snapshot".into()))?;
    if mem_size == 0 {
        return Err(VmmError::Memory("guest memory is empty".into()));
    }
    memory_file
        .set_len(mem.size_bytes)
        .map_err(|error| VmmError::Snapshot(format!("size staged RAM image: {error}")))?;

    // Step 1: enable dirty logging with the vCPU paused. Re-registering memory
    // regions under a running vCPU races with in-flight guest accesses.
    let mut consumed_dirty = DirtyReplayGuard::new(mem);
    let baseline_pages;
    {
        let pause_guard = VcpuPauseGuard::pause_all(vcpu_threads)?;
        // Draining the baseline is what makes the first real read meaningful.
        // These bits are still replayed to the host-dirty tracker: if dirty
        // logging was already on, they are pages the guest wrote since the
        // previous snapshot, and dropping them would corrupt the next diff.
        let baseline = kvm_vm
            .enable_dirty_logging()
            .and_then(|()| kvm_vm.read_dirty());
        match baseline {
            Ok(baseline) => {
                baseline_pages = baseline.len() as u64;
                consumed_dirty.merge(&baseline);
            }
            Err(e) => {
                pause_guard.resume()?;
                return Err(e);
            }
        }
        // Also drain the host-dirty (device DMA) baseline. The bulk copy below
        // captures these pages anyway; draining them here just keeps them out
        // of the first round's residual. They still flow into `consumed_dirty`
        // so the controller replays them for later diff snapshots.
        consumed_dirty.merge(&mem.drain_host_dirty());
        pause_guard.resume()?;
    }
    // The dirty set accumulates from here, so the first round's dirty rate must
    // be measured from here too — not from the end of the bulk copy, which would
    // divide a whole bulk-copy's worth of writes by one inter-round sleep and
    // overestimate the rate by an order of magnitude.
    let mut last_read = Instant::now();
    log::info!(
        "live_snapshot: dirty logging active ({baseline_pages} baseline pages), all vCPUs resumed"
    );
    inject_live_snapshot_failure("dirty_logging")?;

    // Step 2: bulk round — copy all of guest RAM while the guest runs. This is
    // also the bandwidth sample that drives the convergence decision.
    let bulk_start = Instant::now();
    let lazy_fence = mem.lazy_snapshot_fence();
    let _bulk_read_guard = lazy_fence.as_ref().map(|fence| {
        fence
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    });
    // SAFETY: GuestMemory guarantees the mapping for `size_bytes`; the
    // positioned write copies it immediately and retains no pointer.
    let source = unsafe { std::slice::from_raw_parts(mem.as_ptr(), mem_size) };
    memory_file
        .write_all_at(source, 0)
        .map_err(|error| VmmError::Snapshot(format!("write bulk staged RAM: {error}")))?;
    drop(_bulk_read_guard);
    // Bulk writeback and cache eviction happen while the guest is running.
    // This keeps the staging image reclaimable instead of pinning a second
    // anonymous copy of guest RAM until publication.
    memory_file
        .sync_data()
        .map_err(|error| VmmError::Snapshot(format!("sync bulk staged RAM: {error}")))?;
    drop_staged_cache(memory_file, mem.size_bytes);
    let bulk_elapsed = bulk_start.elapsed();
    let mut total_pages_copied = mem.size_bytes / PAGE_SIZE;
    let mut copy_bandwidth_bps =
        bandwidth_sample(mem.size_bytes, bulk_elapsed).unwrap_or(DEFAULT_COPY_BANDWIDTH_BPS);
    log::info!(
        "live_snapshot: bulk copy of {} bytes in {bulk_elapsed:?} ({copy_bandwidth_bps} B/s)",
        mem.size_bytes
    );
    inject_live_snapshot_failure("bulk")?;

    // Step 3: pre-copy rounds.
    let mut rounds = 1u32;
    let mut final_decision = RoundDecision::Continue {
        round: 1,
        dirty_bytes: mem.size_bytes,
    };
    let mut termination = LiveSnapshotTermination::MaxRounds;
    let mut pending_final_dirty = DirtyBitmap::new();

    for round in 2..=config.max_rounds.max(2) {
        if start.elapsed() > timeout {
            log::warn!("live_snapshot: timeout after {:?}", start.elapsed());
            termination = LiveSnapshotTermination::Timeout;
            rounds = round;
            break;
        }

        std::thread::sleep(ROUND_IDLE_SLEEP);

        // A round's dirty set is the union of vCPU writes (KVM's log) and
        // device DMA writes (the software host-dirty tracker). Missing the
        // latter is what used to leave used rings and DMA'd payloads stale
        // in the image.
        let mut dirty = kvm_vm.read_dirty()?;
        dirty.merge(&mem.drain_host_dirty());
        let since_last_read = last_read.elapsed();
        last_read = Instant::now();
        consumed_dirty.merge(&dirty);
        inject_live_snapshot_failure("dirty_round")?;

        let dirty_pages = dirty.len() as u64;
        let dirty_bytes = dirty_pages * PAGE_SIZE;

        // Do not begin a round that the measured copy bandwidth says cannot
        // fit in the remaining pre-copy budget. KVM already cleared these
        // bits, so carry them into the final residual set; the final stop will
        // merge them with writes that occur after this read.
        let projected_copy =
            Duration::from_secs_f64(dirty_bytes as f64 / copy_bandwidth_bps.max(1) as f64);
        if start.elapsed().saturating_add(projected_copy) >= timeout {
            log::warn!(
                "live_snapshot: round {round} would exceed timeout: elapsed={:?} projected_copy={projected_copy:?}",
                start.elapsed()
            );
            pending_final_dirty.merge(&dirty);
            final_decision = RoundDecision::FinalStop { round, dirty_bytes };
            termination = LiveSnapshotTermination::Timeout;
            rounds = round;
            break;
        }

        // Always copy what we just read. KVM cleared these bits, so leaving
        // them uncopied would drop those pages from the image entirely.
        let copy_start = Instant::now();
        let lazy_fence = mem.lazy_snapshot_fence();
        let _dirty_read_guard = lazy_fence.as_ref().map(|fence| {
            fence
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        });
        let copied = copy_dirty_pages(mem, memory_file, &dirty)?;
        drop(_dirty_read_guard);
        if copied > 0 {
            memory_file.sync_data().map_err(|error| {
                VmmError::Snapshot(format!("sync pre-copy staged RAM: {error}"))
            })?;
            drop_staged_cache(memory_file, mem.size_bytes);
        }
        let copy_elapsed = copy_start.elapsed();
        total_pages_copied += copied;
        if let Some(sample) = bandwidth_sample(copied * PAGE_SIZE, copy_elapsed) {
            copy_bandwidth_bps = sample;
        }

        let params = PrecopyParams {
            mem_bytes: mem.size_bytes,
            dirty_rate_bps: dirty_rate_sample(dirty_bytes, since_last_read),
            copy_bandwidth_bps: copy_bandwidth_bps.max(1),
            target_downtime_us: config.target_downtime_us,
            max_rounds: config.max_rounds,
        };
        let decision = decide(&params, round, dirty_bytes);
        log::info!(
            "live_snapshot round {round}: dirty_pages={dirty_pages} copied={copied} \
             dirty_rate={}B/s bw={copy_bandwidth_bps}B/s decision={decision:?}",
            params.dirty_rate_bps
        );

        rounds = round;
        final_decision = decision;

        match decision {
            RoundDecision::Continue { .. } if round < config.max_rounds.max(2) => continue,
            RoundDecision::Continue { .. } => {
                termination = LiveSnapshotTermination::MaxRounds;
                break;
            }
            RoundDecision::FinalStop { .. } => {
                termination = LiveSnapshotTermination::Converged;
                break;
            }
            RoundDecision::Diverging { .. } => {
                log::warn!("live_snapshot: diverging at round {round}; forcing final stop");
                termination = LiveSnapshotTermination::Diverging;
                break;
            }
        }
    }

    // Step 4: final stop. Order matters for the "no noticeable pause" goal:
    //
    //   1. Pause every vCPU so the guest cannot publish a descriptor and kick
    //      after a device pump has acknowledged its pause. In-flight MMIO,
    //      including virtio-blk DMA on a vCPU thread, completes first.
    //   2. Drain and park the net/vsock pumps. Draining the queues consumes a
    //      kick that raced with the pause and prevents restoring a published
    //      descriptor without the non-persistent eventfd notification.
    //   3. Inside the pause do only O(residual) work: read both dirty
    //      sources, copy the residual pages, capture state.
    //
    // Capture errors restore I/O and then vCPUs. If either release cannot be
    // confirmed, the controller fences the source paused for explicit
    // recovery. Downtime covers the complete all-vCPU blackout, including the
    // pause/resume handshakes.
    log::info!("live_snapshot: final stop — pausing all vCPUs, draining I/O");
    let final_stop_start = Instant::now();
    let final_pause_guard = VcpuPauseGuard::pause_all(vcpu_threads)?;
    let io_guard = match IoQuiesceGuard::engage(quiesce_io) {
        Ok(guard) => guard,
        Err(error) => {
            if error.vcpus_may_resume_after_io_error() {
                return match final_pause_guard.resume() {
                    Ok(()) => Err(error),
                    Err(resume_error) => Err(VmmError::Snapshot(format!(
                        "{error}; failed to resume vCPUs after I/O quiescence failure: {resume_error}"
                    ))),
                };
            } else {
                final_pause_guard.keep_paused();
            }
            return Err(error);
        }
    };
    let capture_result = (|| -> Result<(Vec<u8>, u64)> {
        inject_live_snapshot_failure("final_pause")?;

        let mut final_dirty = kvm_vm.read_dirty()?;
        final_dirty.merge(&mem.drain_host_dirty());
        final_dirty.merge(&pending_final_dirty);
        let final_dirty_pages = final_dirty.len() as u64;
        consumed_dirty.merge(&final_dirty);
        let lazy_fence = mem.lazy_snapshot_fence();
        let _final_read_guard = lazy_fence.as_ref().map(|fence| {
            fence
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        });
        total_pages_copied += copy_dirty_pages(mem, memory_file, &final_dirty)?;
        drop(_final_read_guard);
        // Every vCPU is paused, so the registers and device state captured here
        // are coherent with the memory image assembled above.
        let state_blob = capture_state()?;
        inject_live_snapshot_failure("state_capture")?;
        Ok((state_blob, final_dirty_pages))
    })();

    // Release I/O workers first and wait for their pause acknowledgements to
    // clear. This closes the rapid resume/pause race before any vCPU can
    // publish new descriptors. An I/O release failure deliberately leaves the
    // vCPUs paused. Otherwise observe every vCPU leave its park so downtime
    // covers the complete all-vCPU blackout.
    let (state_blob, final_dirty_pages) =
        finish_final_stop(capture_result, io_guard, final_pause_guard)?;
    let downtime = final_stop_start.elapsed();
    // Final residual pages entered the page cache during blackout, but durable
    // writeback is not part of guest downtime.
    memory_file
        .sync_data()
        .map_err(|error| VmmError::Snapshot(format!("sync final staged RAM: {error}")))?;
    drop_staged_cache(memory_file, mem.size_bytes);
    log::info!("live_snapshot: final stop took {downtime:?}, residual {final_dirty_pages} pages");

    let elapsed = start.elapsed();
    log::info!(
        "live_snapshot: complete in {elapsed:?} — {rounds} rounds, \
         {total_pages_copied} pages copied, {final_dirty_pages} residual, {downtime:?} downtime"
    );

    Ok(LiveSnapshotOutput {
        result: LiveSnapshotResult {
            rounds,
            pages_copied: total_pages_copied,
            final_dirty_pages,
            elapsed,
            downtime,
            final_decision,
            termination,
            mem_bytes: mem.size_bytes,
            snapshot_path: String::new(),
            overlay_path: None,
            integrity_path: None,
        },
        state_blob,
        consumed_dirty: consumed_dirty.disarm(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeIoRelease {
        actions: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
        fail: bool,
    }

    impl FinalIoRelease for FakeIoRelease {
        fn release(self) -> Result<()> {
            self.actions.borrow_mut().push("io-release");
            if self.fail {
                Err(VmmError::IoQuiescence {
                    message: "worker did not resume".into(),
                    vcpus_may_resume: false,
                })
            } else {
                Ok(())
            }
        }
    }

    struct FakeVcpuRelease {
        actions: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
        fail_resume: bool,
    }

    impl FinalVcpuRelease for FakeVcpuRelease {
        fn resume(self) -> Result<()> {
            self.actions.borrow_mut().push("vcpu-resume");
            if self.fail_resume {
                Err(VmmError::Snapshot("vCPU did not resume".into()))
            } else {
                Ok(())
            }
        }

        fn keep_paused(self) {
            self.actions.borrow_mut().push("vcpu-keep-paused");
        }
    }

    #[test]
    fn live_snapshot_config_default() {
        let c = LiveSnapshotConfig::default();
        assert_eq!(c.target_downtime_us, 500);
        assert_eq!(c.max_rounds, 20);
        assert_eq!(c.timeout_secs, 30);
    }

    #[test]
    fn failed_snapshot_replays_consumed_dirty_pages() {
        let mem = GuestMemory::new(2 * PAGE_SIZE).unwrap();
        let mut dirty = DirtyBitmap::new();
        dirty.mark(PAGE_SIZE);
        {
            let mut guard = DirtyReplayGuard::new(&mem);
            guard.merge(&dirty);
        }
        let replayed = mem.drain_host_dirty();
        assert!(replayed.contains(PAGE_SIZE));
        assert_eq!(replayed.len(), 1);
    }

    #[test]
    fn successful_snapshot_returns_dirty_pages_without_replay() {
        let mem = GuestMemory::new(2 * PAGE_SIZE).unwrap();
        let mut dirty = DirtyBitmap::new();
        dirty.mark(PAGE_SIZE);
        let returned = {
            let mut guard = DirtyReplayGuard::new(&mem);
            guard.merge(&dirty);
            guard.disarm()
        };
        assert!(returned.contains(PAGE_SIZE));
        assert!(mem.drain_host_dirty().is_empty());
    }

    #[test]
    fn dirty_page_copy_preserves_sorted_contiguous_runs_and_ignores_stale_bits() {
        let mem = GuestMemory::new(4 * PAGE_SIZE).unwrap();
        let mem_len = usize::try_from(mem.size_bytes).unwrap();
        // SAFETY: the test owns the guest mapping for its full declared size.
        let bytes = unsafe { std::slice::from_raw_parts_mut(mem.as_ptr() as *mut u8, mem_len) };
        bytes[PAGE_SIZE as usize..(2 * PAGE_SIZE) as usize].fill(0x11);
        bytes[(2 * PAGE_SIZE) as usize..(3 * PAGE_SIZE) as usize].fill(0x22);

        let file = tempfile::tempfile().unwrap();
        file.set_len(mem.size_bytes).unwrap();
        let mut dirty = DirtyBitmap::new();
        dirty.mark(2 * PAGE_SIZE);
        dirty.mark(PAGE_SIZE);
        dirty.mark(9 * PAGE_SIZE);

        assert_eq!(copy_dirty_pages(&mem, &file, &dirty).unwrap(), 2);
        let mut copied = vec![0u8; (2 * PAGE_SIZE) as usize];
        file.read_exact_at(&mut copied, PAGE_SIZE).unwrap();
        assert!(copied[..PAGE_SIZE as usize]
            .iter()
            .all(|byte| *byte == 0x11));
        assert!(copied[PAGE_SIZE as usize..]
            .iter()
            .all(|byte| *byte == 0x22));
    }

    #[test]
    fn bandwidth_sample_rejects_short_copies() {
        // A sub-MiB copy is timer noise; trusting it produced the ~4 TB/s
        // estimate that made the pre-copy loop stop at round 1 every time.
        assert_eq!(bandwidth_sample(4096, Duration::from_micros(1)), None);
        assert_eq!(bandwidth_sample(0, Duration::from_millis(10)), None);
    }

    #[test]
    fn bandwidth_sample_measures_real_copies() {
        // 64 MiB in 64ms = 1 GiB/s.
        let bps = bandwidth_sample(64 << 20, Duration::from_millis(64)).expect("sample");
        let expected = (64u64 << 20) * 1000 / 64;
        assert!(
            bps.abs_diff(expected) < expected / 100,
            "got {bps}, want ~{expected}"
        );
    }

    #[test]
    fn bandwidth_sample_rejects_zero_duration() {
        assert_eq!(bandwidth_sample(64 << 20, Duration::ZERO), None);
    }

    #[test]
    fn dirty_rate_is_measured_over_real_elapsed_time() {
        // 10 MiB dirtied over 100ms = 100 MiB/s.
        let rate = dirty_rate_sample(10 << 20, Duration::from_millis(100));
        let expected = (10u64 << 20) * 10;
        assert!(
            rate.abs_diff(expected) < expected / 100,
            "got {rate}, want ~{expected}"
        );
    }

    #[test]
    fn dirty_rate_never_returns_zero() {
        // decide() compares this against copy bandwidth; zero would make an
        // actively-dirtying guest look idle.
        assert!(dirty_rate_sample(0, Duration::from_secs(1)) >= 1);
    }

    #[test]
    fn measured_bandwidth_lets_a_large_dirty_set_continue() {
        // Regression for the bogus estimator: with a realistic 1 GiB/s
        // bandwidth a 100 MiB residual must keep pre-copying, not stop.
        let params = PrecopyParams {
            mem_bytes: 256 << 20,
            dirty_rate_bps: 10_000_000,
            copy_bandwidth_bps: bandwidth_sample(1 << 30, Duration::from_secs(1)).expect("sample"),
            target_downtime_us: 5_000,
            max_rounds: 20,
        };
        assert!(matches!(
            decide(&params, 2, 100 << 20),
            RoundDecision::Continue { .. }
        ));
    }

    #[test]
    fn io_quiesce_failure_is_propagated_without_arming_release() {
        let calls = std::cell::RefCell::new(Vec::new());
        let quiesce = |paused| {
            calls.borrow_mut().push(paused);
            Err(VmmError::Device("quiescence failed".into()))
        };

        let error = match IoQuiesceGuard::engage(&quiesce) {
            Ok(_) => panic!("failed quiescence unexpectedly armed the guard"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("quiescence failed"));
        assert_eq!(&*calls.borrow(), &[true]);
    }

    #[test]
    fn io_quiesce_guard_releases_on_drop() {
        let calls = std::cell::RefCell::new(Vec::new());
        let quiesce = |paused| {
            calls.borrow_mut().push(paused);
            Ok(())
        };

        drop(IoQuiesceGuard::engage(&quiesce).expect("engage I/O quiescence"));

        assert_eq!(&*calls.borrow(), &[true, false]);
    }

    #[test]
    fn io_quiesce_release_failure_is_reported_once() {
        let calls = std::cell::RefCell::new(Vec::new());
        let quiesce = |paused| {
            calls.borrow_mut().push(paused);
            if paused {
                Ok(())
            } else {
                Err(VmmError::Device("resume failed".into()))
            }
        };

        let error = IoQuiesceGuard::engage(&quiesce)
            .expect("engage I/O quiescence")
            .disengage()
            .expect_err("release failure was ignored");

        assert!(error.to_string().contains("resume failed"));
        assert_eq!(&*calls.borrow(), &[true, false]);
    }

    #[test]
    fn final_stop_keeps_vcpus_paused_when_io_release_fails() {
        let actions = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let error = finish_final_stop(
            Ok(()),
            FakeIoRelease {
                actions: std::rc::Rc::clone(&actions),
                fail: true,
            },
            FakeVcpuRelease {
                actions: std::rc::Rc::clone(&actions),
                fail_resume: false,
            },
        )
        .expect_err("I/O release failure unexpectedly resumed the source");

        assert!(error.to_string().contains("worker did not resume"));
        assert_eq!(&*actions.borrow(), &["io-release", "vcpu-keep-paused"]);
    }

    #[test]
    fn final_stop_restores_source_after_capture_failure_when_io_is_running() {
        let actions = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let error = finish_final_stop::<(), _, _>(
            Err(VmmError::Snapshot("capture failed".into())),
            FakeIoRelease {
                actions: std::rc::Rc::clone(&actions),
                fail: false,
            },
            FakeVcpuRelease {
                actions: std::rc::Rc::clone(&actions),
                fail_resume: false,
            },
        )
        .expect_err("capture failure unexpectedly succeeded");

        assert!(error.to_string().contains("capture failed"));
        assert_eq!(&*actions.borrow(), &["io-release", "vcpu-resume"]);
    }

    #[test]
    fn failed_vcpu_pause_requires_confirmed_rollback() {
        let actions = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let error = finish_failed_vcpu_pause(
            VmmError::Snapshot("pause failed".into()),
            FakeVcpuRelease {
                actions: std::rc::Rc::clone(&actions),
                fail_resume: true,
            },
        );

        assert!(error.to_string().contains("pause failed"));
        assert!(error
            .to_string()
            .contains("failed to confirm vCPU rollback"));
        assert_eq!(&*actions.borrow(), &["vcpu-resume"]);
    }
}
