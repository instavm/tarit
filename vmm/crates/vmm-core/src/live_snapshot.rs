//! Live snapshot executor — wires the pre-copy convergence algorithm
//! to real KVM dirty-log, running while the vCPU keeps executing.
//!
//! Design: while the guest runs, copy memory pages out to the snapshot
//! buffer. Re-read the dirty set, copy only newly-dirtied pages; repeat.
//! When the remaining dirty set is small enough that it can be copied
//! within the target downtime, do a brief final stop that copies only the
//! residual pages and captures vCPU + device state.

#![cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]

use crate::error::{Result, VmmError};
use crate::kvm::KvmVm;
use crate::vcpu_thread::VcpuThread;
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

struct VcpuPauseGuard<'a> {
    vcpu_thread: &'a VcpuThread,
    armed: bool,
}

impl<'a> VcpuPauseGuard<'a> {
    fn pause(vcpu_thread: &'a VcpuThread) -> Self {
        vcpu_thread.pause();
        Self {
            vcpu_thread,
            armed: true,
        }
    }

    fn resume(mut self) {
        if self.armed {
            self.vcpu_thread.resume();
            self.armed = false;
        }
    }
}

impl Drop for VcpuPauseGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.vcpu_thread.resume();
        }
    }
}

/// RAII guard around the caller's I/O quiesce hook: engaging it parks every
/// VMM thread that writes guest memory (net/vsock pumps); disengaging (or
/// dropping, on error paths) releases them.
struct IoQuiesceGuard<'a> {
    quiesce: &'a dyn Fn(bool),
    armed: bool,
}

impl<'a> IoQuiesceGuard<'a> {
    fn engage(quiesce: &'a dyn Fn(bool)) -> Self {
        quiesce(true);
        Self {
            quiesce,
            armed: true,
        }
    }

    fn disengage(mut self) {
        if self.armed {
            (self.quiesce)(false);
            self.armed = false;
        }
    }
}

impl Drop for IoQuiesceGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            (self.quiesce)(false);
        }
    }
}

/// Configuration for a live snapshot.
#[derive(Debug, Clone)]
pub struct LiveSnapshotConfig {
    /// Target final-stop downtime in microseconds.
    pub target_downtime_us: u64,
    /// Max pre-copy rounds before forcing stop.
    pub max_rounds: u32,
    /// Time budget for the entire live snapshot (hard limit).
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
    /// Size of the captured memory image. The image itself is streamed to
    /// `snapshot_path` and freed, so a live snapshot does not pin a second
    /// copy of guest RAM for the caller's lifetime.
    pub mem_bytes: u64,
    /// On-disk path where the controller persisted this live snapshot (a
    /// private per-process scratch path). Empty until the controller sets it.
    pub snapshot_path: String,
}

/// Everything a live snapshot produces. The controller consumes `mem_snapshot`
/// and `state_blob` to write the on-disk artifact, then drops them.
pub struct LiveSnapshotOutput {
    pub result: LiveSnapshotResult,
    /// The captured guest memory image.
    pub mem_snapshot: Vec<u8>,
    /// State blob captured during the final stop, carrying the vCPU registers,
    /// VM (irqchip/PIT/clock), serial and virtio state as of the blackout.
    pub state_blob: Vec<u8>,
    /// Every dirty bit this snapshot consumed from KVM. `KVM_GET_DIRTY_LOG`
    /// clears the bitmap it reports, so these must be replayed into the VM's
    /// host-dirty tracker or a later diff snapshot would silently omit them.
    pub consumed_dirty: DirtyBitmap,
}

/// Copy one guest page into `dest`. Returns false if the page lies outside
/// guest RAM (a stale bit for a region that is no longer registered).
fn copy_page(mem: &GuestMemory, dest: &mut [u8], gpa: u64) -> bool {
    let Ok(offset) = usize::try_from(gpa) else {
        return false;
    };
    let Some(end) = offset.checked_add(PAGE_SIZE as usize) else {
        return false;
    };
    if end > dest.len() {
        return false;
    }
    // SAFETY: `mem.as_ptr()` is valid for reads of `mem.size_bytes` bytes for
    // as long as `mem` lives (GuestMemory's documented contract), and `end` is
    // bounds-checked against `dest.len() == mem.size_bytes` above.
    let src = unsafe { std::slice::from_raw_parts(mem.as_ptr().add(offset), PAGE_SIZE as usize) };
    dest[offset..end].copy_from_slice(src);
    true
}

/// Copy every page named by `dirty` into `dest`, returning the pages copied.
fn copy_dirty_pages(mem: &GuestMemory, dest: &mut [u8], dirty: &DirtyBitmap) -> u64 {
    let mut copied = 0u64;
    for pfn in dirty.dirty_pfns() {
        if copy_page(mem, dest, pfn.saturating_mul(PAGE_SIZE)) {
            copied += 1;
        }
    }
    copied
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
/// 1. Pause the vCPU, enable dirty logging, clear the baseline, resume.
/// 2. Bulk round: copy all of guest RAM while the vCPU runs.
/// 3. Pre-copy rounds: read the dirty log, copy those pages, check convergence.
/// 4. Final stop: quiesce device I/O threads, pause the vCPU, copy the residual
///    dirty pages, capture vCPU + device state via `capture_state`, resume.
///
/// Dirty pages come from two sources, and both are consulted every round:
/// KVM's dirty log (guest vCPU writes) and the software host-dirty tracker
/// (virtio DMA — used rings, blk/net/vsock payloads — written by VMM threads,
/// which KVM's log cannot see).
///
/// `quiesce_io(true)` must park every non-vCPU thread that writes guest
/// memory and only return once they have acknowledged; `quiesce_io(false)`
/// releases them. It is engaged *before* the final vCPU pause — its
/// handshake costs guest I/O stall, never guest downtime.
///
/// `capture_state` runs while the vCPU is paused for the final stop, so the
/// state blob it returns is coherent with the memory image.
pub fn live_snapshot<F>(
    kvm_vm: &KvmVm,
    mem: &GuestMemory,
    vcpu_thread: &VcpuThread,
    config: &LiveSnapshotConfig,
    quiesce_io: &dyn Fn(bool),
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

    // Step 1: enable dirty logging with the vCPU paused. Re-registering memory
    // regions under a running vCPU races with in-flight guest accesses.
    let mut consumed_dirty = DirtyBitmap::new();
    let baseline_pages;
    {
        let pause_guard = VcpuPauseGuard::pause(vcpu_thread);
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
                pause_guard.resume();
                return Err(e);
            }
        }
        // Also drain the host-dirty (device DMA) baseline. The bulk copy below
        // captures these pages anyway; draining them here just keeps them out
        // of the first round's residual. They still flow into `consumed_dirty`
        // so the controller replays them for later diff snapshots.
        consumed_dirty.merge(&mem.drain_host_dirty());
        pause_guard.resume();
    }
    // The dirty set accumulates from here, so the first round's dirty rate must
    // be measured from here too — not from the end of the bulk copy, which would
    // divide a whole bulk-copy's worth of writes by one inter-round sleep and
    // overestimate the rate by an order of magnitude.
    let mut last_read = Instant::now();
    log::info!(
        "live_snapshot: dirty logging active ({baseline_pages} baseline pages), vCPU resumed"
    );

    // Step 2: bulk round — copy all of guest RAM while the guest runs. This is
    // also the bandwidth sample that drives the convergence decision.
    let mut dest = vec![0u8; mem_size];
    let bulk_start = Instant::now();
    // SAFETY: `mem.as_ptr()` is valid for reads of `mem.size_bytes` bytes for
    // as long as `mem` lives; `dest` was sized from that same value.
    dest.copy_from_slice(unsafe { std::slice::from_raw_parts(mem.as_ptr(), mem_size) });
    let bulk_elapsed = bulk_start.elapsed();
    let mut total_pages_copied = mem.size_bytes / PAGE_SIZE;
    let mut copy_bandwidth_bps =
        bandwidth_sample(mem.size_bytes, bulk_elapsed).unwrap_or(DEFAULT_COPY_BANDWIDTH_BPS);
    log::info!(
        "live_snapshot: bulk copy of {} bytes in {bulk_elapsed:?} ({copy_bandwidth_bps} B/s)",
        mem.size_bytes
    );

    // Step 3: pre-copy rounds.
    let mut rounds = 1u32;
    let mut final_decision = RoundDecision::Continue {
        round: 1,
        dirty_bytes: mem.size_bytes,
    };

    for round in 2..=config.max_rounds.max(2) {
        if start.elapsed() > timeout {
            log::warn!("live_snapshot: timeout after {:?}", start.elapsed());
            final_decision = RoundDecision::FinalStop {
                round,
                dirty_bytes: 0,
            };
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

        let dirty_pages = dirty.len() as u64;
        let dirty_bytes = dirty_pages * PAGE_SIZE;

        // Always copy what we just read. KVM cleared these bits, so leaving
        // them uncopied would drop those pages from the image entirely.
        let copy_start = Instant::now();
        let copied = copy_dirty_pages(mem, &mut dest, &dirty);
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
            RoundDecision::Continue { .. } => continue,
            RoundDecision::FinalStop { .. } => break,
            RoundDecision::Diverging { .. } => {
                log::warn!("live_snapshot: diverging at round {round}; forcing final stop");
                break;
            }
        }
    }

    // Step 4: final stop. Order matters for the "no noticeable pause" goal:
    //
    //   1. Quiesce the device I/O threads (net/vsock pumps) while the guest
    //      is still running — their ack handshake stalls guest I/O briefly
    //      but adds zero guest downtime.
    //   2. Pause the vCPU. In-flight MMIO (including virtio-blk DMA, which
    //      runs on the vCPU thread) completes before the pause is acked, so
    //      after this point nothing writes guest memory.
    //   3. Inside the pause do only O(residual) work: read both dirty
    //      sources, copy the residual pages, capture state.
    //
    // On any error below, the guards resume the vCPU and I/O threads as they
    // drop. Downtime is measured across the whole pause — including the
    // pause/resume handshakes — because that is the blackout the guest sees.
    log::info!("live_snapshot: final stop — quiescing I/O, pausing vCPU");
    let io_guard = IoQuiesceGuard::engage(quiesce_io);
    let final_stop_start = Instant::now();
    let final_pause_guard = VcpuPauseGuard::pause(vcpu_thread);

    let mut final_dirty = kvm_vm.read_dirty()?;
    final_dirty.merge(&mem.drain_host_dirty());
    let final_dirty_pages = final_dirty.len() as u64;
    consumed_dirty.merge(&final_dirty);
    total_pages_copied += copy_dirty_pages(mem, &mut dest, &final_dirty);
    // The vCPU is paused, so the registers and device state captured here are
    // coherent with the memory image assembled above.
    let state_blob = capture_state()?;

    final_pause_guard.resume();
    // `resume()` only requests the resume; wait for the vCPU to actually
    // leave its park before stopping the clock so the reported downtime is
    // the blackout the guest really saw.
    vcpu_thread.wait_resumed();
    let downtime = final_stop_start.elapsed();
    io_guard.disengage();
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
            mem_bytes: mem.size_bytes,
            snapshot_path: String::new(),
        },
        mem_snapshot: dest,
        state_blob,
        consumed_dirty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_snapshot_config_default() {
        let c = LiveSnapshotConfig::default();
        assert_eq!(c.target_downtime_us, 500);
        assert_eq!(c.max_rounds, 20);
        assert_eq!(c.timeout_secs, 30);
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
}
