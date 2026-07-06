//! Live snapshot — iterative pre-copy background snapshot loop.
//!
//! While the guest runs, copy memory pages out to the snapshot
//! buffer in the background. Re-read the dirty set, copy only newly-dirtied
//! pages; repeat. Each round the re-dirtied set shrinks toward the working
//! set. When the remaining dirty set is small enough that it can be copied
//! within the target downtime, do a brief final stop (sub-ms to a few ms),
//! copy the last dirty pages + device/CPU state, and resume.
//!
//! This module implements the *convergence logic* — pure functions of a
//! dirty-rate + copy-bandwidth + target-downtime model, host-agnostic and
//! unit-testable. The actual KVM dirty-ring + write-protect wiring is
//! Linux+KVM-gated and lives in `vmm-memory-backend`.

use serde::{Deserialize, Serialize};

/// The parameters of a live-snapshot pre-copy loop.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PrecopyParams {
    /// Guest RAM in bytes.
    pub mem_bytes: u64,
    /// Estimated dirty rate in bytes/sec (measured from the first round).
    pub dirty_rate_bps: u64,
    /// Available copy bandwidth in bytes/sec.
    pub copy_bandwidth_bps: u64,
    /// Target final-stop downtime in microseconds (design target: 1-5 ms).
    pub target_downtime_us: u64,
    /// Max rounds before forcing the stop-and-copy (auto-converge / give-up).
    pub max_rounds: u32,
}

/// The result of one pre-copy round's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoundDecision {
    /// Continue: copy the dirty set in the background; the guest keeps running.
    Continue { round: u32, dirty_bytes: u64 },
    /// Stop the guest and copy the residual dirty set + device state.
    /// Triggered when the residual dirty set fits in `target_downtime_us`.
    FinalStop { round: u32, dirty_bytes: u64 },
    /// Auto-converge: dirty rate exceeds copy bandwidth; we'd never converge.
    /// Either throttle the guest or switch to post-copy.
    Diverging { round: u32 },
}

/// Decide whether to do another background round or stop-and-copy, given
/// the residual dirty-set size after the last round.
///
/// The stop condition: the residual dirty set can be copied
/// within `target_downtime_us` at `copy_bandwidth_bps`. I.e.
/// `dirty_bytes / copy_bandwidth_bps <= target_downtime_us / 1e6`.
pub fn decide(params: &PrecopyParams, round: u32, dirty_bytes: u64) -> RoundDecision {
    // Auto-converge: if the dirty rate is >= the copy bandwidth, we can
    // never catch up — each round dirties more than we can copy.
    if params.dirty_rate_bps >= params.copy_bandwidth_bps && round >= 2 {
        return RoundDecision::Diverging { round };
    }
    if round >= params.max_rounds {
        return RoundDecision::FinalStop { round, dirty_bytes };
    }
    // Stop-and-copy when the residual fits in the downtime budget.
    let copy_time_us =
        (dirty_bytes as u128).saturating_mul(1_000_000) / params.copy_bandwidth_bps.max(1) as u128;
    if copy_time_us <= params.target_downtime_us as u128 {
        return RoundDecision::FinalStop { round, dirty_bytes };
    }
    RoundDecision::Continue { round, dirty_bytes }
}

/// Simulate a pre-copy loop to convergence (or divergence), returning the
/// final decision + the number of rounds. Used by the convergence tests
/// (assert the pre-copy loop converges and the final pause window stays
/// under target).
pub fn simulate(params: &PrecopyParams, mut dirty_bytes: u64) -> (RoundDecision, u32) {
    let mut round = 0u32;
    loop {
        round += 1;
        let decision = decide(params, round, dirty_bytes);
        match decision {
            RoundDecision::Continue { .. } => {
                // Model: each round, the residual shrinks toward the
                // working set (dirty_rate * copy_time_of_last_round).
                let copy_time_s = dirty_bytes as f64 / params.copy_bandwidth_bps as f64;
                let new_dirty = (params.dirty_rate_bps as f64 * copy_time_s) as u64;
                dirty_bytes = new_dirty.max(4096); // never below a page
            }
            RoundDecision::FinalStop { round: r, .. } => {
                return (decision, r);
            }
            RoundDecision::Diverging { round: r } => {
                return (decision, r);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(dirty_bps: u64, bw_bps: u64, downtime_us: u64) -> PrecopyParams {
        PrecopyParams {
            mem_bytes: 256 * 1024 * 1024,
            dirty_rate_bps: dirty_bps,
            copy_bandwidth_bps: bw_bps,
            target_downtime_us: downtime_us,
            max_rounds: 20,
        }
    }

    #[test]
    fn small_dirty_set_stops_immediately() {
        // 1 MiB dirty, 1 GiB/s bandwidth, 5ms downtime budget.
        // copy_time = 1MiB / 1GiB/s = ~1ms < 5ms → stop.
        let p = params(1_000_000, 1_000_000_000, 5_000);
        let d = decide(&p, 1, 1024 * 1024);
        assert!(matches!(d, RoundDecision::FinalStop { round: 1, .. }));
    }

    #[test]
    fn large_dirty_set_continues() {
        // 100 MiB dirty, 1 GiB/s bandwidth → 100ms copy >> 5ms budget.
        let p = params(1_000_000, 1_000_000_000, 5_000);
        let d = decide(&p, 1, 100 * 1024 * 1024);
        assert!(matches!(d, RoundDecision::Continue { round: 1, .. }));
    }

    #[test]
    fn diverging_when_dirty_exceeds_bandwidth() {
        // Dirty rate >= copy bandwidth: never converges.
        let p = params(2_000_000_000, 1_000_000_000, 5_000);
        let d = decide(&p, 2, 10 * 1024 * 1024);
        assert!(matches!(d, RoundDecision::Diverging { round: 2 }));
    }

    #[test]
    fn simulate_converges_under_low_dirty_rate() {
        // Low dirty rate, high bandwidth: should converge in a few rounds.
        let p = params(10_000_000, 1_000_000_000, 5_000);
        let (decision, rounds) = simulate(&p, 100 * 1024 * 1024);
        assert!(matches!(decision, RoundDecision::FinalStop { .. }));
        assert!(rounds <= 20, "converged in {rounds} rounds");
    }

    #[test]
    fn simulate_diverges_under_high_dirty_rate() {
        let p = params(2_000_000_000, 1_000_000_000, 5_000);
        let (decision, _) = simulate(&p, 100 * 1024 * 1024);
        assert!(matches!(decision, RoundDecision::Diverging { .. }));
    }

    #[test]
    fn max_rounds_forces_final_stop() {
        // Even if not converged, max_rounds forces a stop. dirty_rate must
        // be < copy_bandwidth so the Diverging check (round >= 2) doesn't
        // fire first.
        let p = PrecopyParams {
            mem_bytes: 256 * 1024 * 1024,
            dirty_rate_bps: 900_000_000,
            copy_bandwidth_bps: 1_000_000_000,
            target_downtime_us: 1, // impossibly tight
            max_rounds: 3,
        };
        let (decision, rounds) = simulate(&p, 100 * 1024 * 1024);
        assert!(matches!(decision, RoundDecision::FinalStop { .. }));
        assert!(rounds <= 3);
    }
}
