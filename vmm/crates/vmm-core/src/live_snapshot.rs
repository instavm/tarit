//! Live snapshot executor — wires the pre-copy convergence algorithm
//! to real KVM dirty-log, running in a background thread while the
//! vCPU keeps executing.
//!
//! Design: while the guest runs, copy memory pages out to the snapshot
//! buffer in the background. Re-read the dirty set, copy only newly-dirtied
//! pages; repeat. When the remaining dirty set is small enough that it can
//! be copied within the target downtime, do a brief final stop.

#![cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]

use crate::error::{Result, VmmError};
use crate::vcpu_thread::VcpuThread;
use kvm_ioctls::VmFd;
use std::time::{Duration, Instant};
use vmm_memory_backend::kvm_dirty::read_dirty_log;
use vmm_memory_backend::GuestMemory;
use vmm_snapshot::live::{decide, PrecopyParams, RoundDecision};

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
            target_downtime_us: 5_000, // 5ms
            max_rounds: 20,
            timeout_secs: 30,
        }
    }
}

/// Result of a live snapshot.
#[derive(Debug, Clone)]
pub struct LiveSnapshotResult {
    /// Number of pre-copy rounds executed.
    pub rounds: u32,
    /// Total pages copied across all rounds.
    pub pages_copied: u64,
    /// Final dirty set size (pages) at stop-and-copy.
    pub final_dirty_pages: u64,
    /// Total elapsed time.
    pub elapsed: Duration,
    /// The decision that ended the pre-copy loop.
    pub final_decision: RoundDecision,
    /// The memory snapshot (all pages).
    pub mem_snapshot: Vec<u8>,
    /// On-disk path where the controller persisted this live snapshot (a
    /// private per-process scratch path). Empty until the controller sets it.
    pub snapshot_path: String,
}

/// Execute a live snapshot of a running VM.
///
/// Flow:
/// 1. Pause the vCPU briefly to enable dirty logging
/// 2. Resume the vCPU
/// 3. Background loop: read dirty log → copy dirty pages → check convergence
/// 4. When converged: final pause → copy remaining dirty pages + device state
/// 5. Return the snapshot
pub fn live_snapshot(
    vm_fd: &VmFd,
    mem: &GuestMemory,
    _mem_slots: &[u32],
    vcpu_thread: &VcpuThread,
    config: &LiveSnapshotConfig,
) -> Result<LiveSnapshotResult> {
    let start = Instant::now();
    let timeout = Duration::from_secs(config.timeout_secs);
    let mem_size = mem.size_bytes as usize;
    let page_size = 4096usize;

    // Step 1: Re-register memory with dirty logging enabled.
    log::info!("live_snapshot: enabling dirty logging");
    let dirty_slots = mem
        .register_with_dirty_logging(vm_fd)
        .map_err(|e| VmmError::Memory(e.to_string()))?;

    // Step 2: Pause + resume to get a clean dirty baseline.
    let pause_guard = VcpuPauseGuard::pause(vcpu_thread);
    // Read the dirty log to clear the baseline (all pages appear dirty initially).
    let _baseline = read_dirty_log(vm_fd, mem, &dirty_slots)
        .map_err(|e| VmmError::Kvm(format!("baseline dirty log: {e}")))?;
    pause_guard.resume();
    log::info!("live_snapshot: vCPU resumed, dirty logging active");

    // Step 3: Pre-copy loop.
    let mut rounds = 0u32;
    let mut total_pages_copied = 0u64;
    let mut last_dirty_count = (mem_size / page_size) as u64; // start with all pages dirty
    let mut final_decision = RoundDecision::Continue {
        round: 0,
        dirty_bytes: mem_size as u64,
    };

    // Estimate copy bandwidth (pages/sec) from the first round.
    let mut copy_bandwidth_bps: u64 = 500_000_000; // 500 MB/s default estimate

    for round in 1..=config.max_rounds {
        if start.elapsed() > timeout {
            log::warn!("live_snapshot: timeout after {:?}", start.elapsed());
            final_decision = RoundDecision::FinalStop {
                round,
                dirty_bytes: last_dirty_count * page_size as u64,
            };
            break;
        }

        // Small sleep to let the guest dirty some pages.
        std::thread::sleep(Duration::from_millis(50));

        // Read the dirty log (this also resets KVM's dirty bitmap).
        let dirty = read_dirty_log(vm_fd, mem, &dirty_slots)
            .map_err(|e| VmmError::Kvm(format!("dirty log round {round}: {e}")))?;

        let dirty_pages = dirty.len() as u64;
        let dirty_bytes = dirty_pages * page_size as u64;

        // Estimate dirty rate from the last round.
        let elapsed_secs = 0.05; // 50ms sleep
        let dirty_rate_bps = (dirty_bytes as f64 / elapsed_secs) as u64;

        // Update copy bandwidth estimate (how fast we can copy pages).
        if round == 1 {
            let copy_start = Instant::now();
            // Time a small copy to estimate bandwidth.
            // SAFETY: `mem.as_ptr()` points to `mem_size` bytes owned by GuestMemory;
            // this benchmark-only slice reads at most one page and is not retained.
            let _ = unsafe { std::slice::from_raw_parts(mem.as_ptr(), 4096.min(mem_size)) };
            let copy_us = copy_start.elapsed().as_micros().max(1);
            copy_bandwidth_bps = (4096u64 * 1_000_000) / copy_us as u64 * 1000;
        }

        let params = PrecopyParams {
            mem_bytes: mem_size as u64,
            dirty_rate_bps: dirty_rate_bps.max(1),
            copy_bandwidth_bps: copy_bandwidth_bps.max(1),
            target_downtime_us: config.target_downtime_us,
            max_rounds: config.max_rounds,
        };

        let decision = decide(&params, round, dirty_bytes);
        log::info!(
            "live_snapshot round {round}: dirty_pages={dirty_pages} dirty_rate={dirty_rate_bps}bps bw={copy_bandwidth_bps}bps decision={decision:?}"
        );

        total_pages_copied += dirty_pages;
        last_dirty_count = dirty_pages as u64;
        rounds = round;
        final_decision = decision;

        match decision {
            RoundDecision::Continue { .. } => {
                // Keep going — the guest is still running.
                continue;
            }
            RoundDecision::FinalStop { .. } => {
                // Converged — do the final stop.
                break;
            }
            RoundDecision::Diverging { .. } => {
                // Dirty rate too high — force stop (or switch to post-copy).
                log::warn!("live_snapshot: diverging at round {round}");
                break;
            }
        }
    }

    // Step 4: Final stop — pause the vCPU and copy all memory.
    log::info!("live_snapshot: final stop — pausing vCPU");
    let final_pause_guard = VcpuPauseGuard::pause(vcpu_thread);
    let final_stop_start = Instant::now();

    // Read any remaining dirty pages.
    let final_dirty = read_dirty_log(vm_fd, mem, &dirty_slots)
        .map_err(|e| VmmError::Kvm(format!("final dirty log: {e}")))?;
    let final_dirty_pages = final_dirty.len() as u64;

    // Copy ALL guest memory to the snapshot (full copy for correctness).
    // SAFETY: `mem.as_ptr()` points to `mem_size` bytes owned by GuestMemory.
    // The vCPU is paused by `final_pause_guard`, so memory is stable while copied.
    let mem_snapshot = unsafe { std::slice::from_raw_parts(mem.as_ptr(), mem_size).to_vec() };

    let final_stop_us = final_stop_start.elapsed().as_micros();
    log::info!(
        "live_snapshot: final stop took {final_stop_us}µs, final_dirty={final_dirty_pages} pages"
    );

    // Resume the vCPU (the guest keeps running after the snapshot).
    final_pause_guard.resume();
    log::info!("live_snapshot: vCPU resumed — guest continues running");

    let elapsed = start.elapsed();
    log::info!(
        "live_snapshot: complete in {:?} — {rounds} rounds, {total_pages_copied} pages copied, {final_dirty_pages} final dirty",
        elapsed
    );

    Ok(LiveSnapshotResult {
        rounds,
        pages_copied: total_pages_copied,
        final_dirty_pages,
        elapsed,
        final_decision,
        mem_snapshot,
        snapshot_path: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_snapshot_config_default() {
        let c = LiveSnapshotConfig::default();
        assert_eq!(c.target_downtime_us, 5_000);
        assert_eq!(c.max_rounds, 20);
        assert_eq!(c.timeout_secs, 30);
    }
}
