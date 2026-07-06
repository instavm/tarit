//! Live migration state machine (PRD §9d).
//!
//! PRD §9d: "Negotiation → Destination prep → Iterative pre-copy → Brief
//! stop-and-copy → Post-copy fallback → Cutover."
//!
//! This module owns the migration *state machine* — the sequence of phases
//! and their transitions — independent of the transport (which is the mTLS
//! channel in `transport.rs`). The convergence math reuses
//! [`vmm_snapshot::live`] (PRD §9d: "the same pre-copy / post-copy loop as
//! the live snapshot").

use serde::{Deserialize, Serialize};
use vmm_snapshot::live::RoundDecision;
use vmm_snapshot::ClonePlan;

/// The phases of a live migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MigrationPhase {
    /// Source + destination agree on protocol version, CPU template, RAM,
    /// device model, volume identity over the mTLS control channel.
    Negotiation,
    /// Destination spins up a paused VMM shell with matching memory regions
    /// + device config, registers guest RAM with UFFD.
    DestinationPrep,
    /// Source enables dirty tracking + streams RAM pages; re-sends only
    /// re-dirtied pages each round (dirty-ring preferred).
    Precopy,
    /// Pause source vCPUs, flush the final dirty pages + device/vCPU state
    /// (CRC'd state blob), targeting a sub-5ms blackout.
    StopAndCopy,
    /// Under a high dirty rate, hand control to the destination early and
    /// demand-fetch not-yet-transferred pages over the network via UFFD.
    Postcopy,
    /// Destination resumes vCPUs; source tears down only after a commit ack.
    Cutover,
    /// Migration complete.
    Done,
    /// Migration failed; source remains authoritative and resumes.
    Aborted,
}

/// The state of one migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationState {
    pub phase: MigrationPhase,
    pub round: u32,
    pub dirty_bytes: u64,
}

impl MigrationState {
    pub fn new() -> Self {
        Self {
            phase: MigrationPhase::Negotiation,
            round: 0,
            dirty_bytes: 0,
        }
    }

    /// Advance one step, given the pre-copy decision from `vmm_snapshot::live`.
    /// Returns the new phase.
    pub fn advance(&mut self, decision: RoundDecision) -> MigrationPhase {
        match (self.phase, decision) {
            (MigrationPhase::Negotiation, _) => self.phase = MigrationPhase::DestinationPrep,
            (MigrationPhase::DestinationPrep, _) => {
                self.phase = MigrationPhase::Precopy;
                self.round = 1;
            }
            (MigrationPhase::Precopy, RoundDecision::Continue { round, dirty_bytes }) => {
                self.round = round + 1;
                self.dirty_bytes = dirty_bytes;
            }
            (MigrationPhase::Precopy, RoundDecision::FinalStop { round, dirty_bytes }) => {
                self.round = round;
                self.dirty_bytes = dirty_bytes;
                self.phase = MigrationPhase::StopAndCopy;
            }
            (MigrationPhase::Precopy, RoundDecision::Diverging { round }) => {
                self.round = round;
                self.phase = MigrationPhase::Postcopy;
            }
            (MigrationPhase::StopAndCopy, _) => self.phase = MigrationPhase::Cutover,
            (MigrationPhase::Postcopy, _) => self.phase = MigrationPhase::Cutover,
            (MigrationPhase::Cutover, _) => self.phase = MigrationPhase::Done,
            (MigrationPhase::Done | MigrationPhase::Aborted, _) => {}
        }
        self.phase
    }
}

impl Default for MigrationState {
    fn default() -> Self {
        Self::new()
    }
}

/// A migration plan: the negotiation agreement + the clone plan for the
/// destination's storage continuity (PRD §9d: storage-migration or shared
/// backing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub negotiation: crate::negotiation::NegotiationRequest,
    pub storage_plan: ClonePlan,
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_snapshot::live::decide;
    use vmm_snapshot::live::PrecopyParams;

    fn params(dirty_bps: u64, bw_bps: u64) -> PrecopyParams {
        PrecopyParams {
            mem_bytes: 256 * 1024 * 1024,
            dirty_rate_bps: dirty_bps,
            copy_bandwidth_bps: bw_bps,
            target_downtime_us: 5_000,
            max_rounds: 20,
        }
    }

    #[test]
    fn happy_path_phases() {
        let mut s = MigrationState::new();
        assert_eq!(s.phase, MigrationPhase::Negotiation);

        // Negotiation → DestinationPrep
        s.advance(RoundDecision::Continue {
            round: 0,
            dirty_bytes: 0,
        });
        assert_eq!(s.phase, MigrationPhase::DestinationPrep);

        // DestinationPrep → Precopy
        s.advance(RoundDecision::Continue {
            round: 0,
            dirty_bytes: 0,
        });
        assert_eq!(s.phase, MigrationPhase::Precopy);

        // A few Continue rounds.
        s.advance(RoundDecision::Continue {
            round: 1,
            dirty_bytes: 50_000_000,
        });
        s.advance(RoundDecision::Continue {
            round: 2,
            dirty_bytes: 10_000_000,
        });
        assert_eq!(s.phase, MigrationPhase::Precopy);

        // FinalStop → StopAndCopy
        s.advance(RoundDecision::FinalStop {
            round: 3,
            dirty_bytes: 1_000_000,
        });
        assert_eq!(s.phase, MigrationPhase::StopAndCopy);
        assert_eq!(s.dirty_bytes, 1_000_000);

        // StopAndCopy → Cutover
        s.advance(RoundDecision::FinalStop {
            round: 3,
            dirty_bytes: 0,
        });
        assert_eq!(s.phase, MigrationPhase::Cutover);

        // Cutover → Done
        s.advance(RoundDecision::FinalStop {
            round: 3,
            dirty_bytes: 0,
        });
        assert_eq!(s.phase, MigrationPhase::Done);
    }

    #[test]
    fn postcopy_fallback_on_divergence() {
        let mut s = MigrationState::new();
        s.advance(RoundDecision::Continue {
            round: 0,
            dirty_bytes: 0,
        }); // → DestPrep
        s.advance(RoundDecision::Continue {
            round: 0,
            dirty_bytes: 0,
        }); // → Precopy
        assert_eq!(s.phase, MigrationPhase::Precopy);

        // Diverging → Postcopy (PRD §9d: "bounds downtime regardless of dirty rate")
        s.advance(RoundDecision::Diverging { round: 2 });
        assert_eq!(s.phase, MigrationPhase::Postcopy);

        // Postcopy → Cutover
        s.advance(RoundDecision::Continue {
            round: 2,
            dirty_bytes: 0,
        });
        assert_eq!(s.phase, MigrationPhase::Cutover);
    }

    #[test]
    fn full_simulate_to_migration() {
        // Drive the pre-copy loop with the live module's simulate, advancing
        // the migration state machine at each round.
        let p = params(10_000_000, 1_000_000_000);
        let mut s = MigrationState::new();
        s.advance(RoundDecision::Continue {
            round: 0,
            dirty_bytes: 0,
        }); // → DestPrep
        s.advance(RoundDecision::Continue {
            round: 0,
            dirty_bytes: 0,
        }); // → Precopy

        let mut dirty = 100 * 1024 * 1024;
        loop {
            let decision = decide(&p, s.round, dirty);
            s.advance(decision);
            if matches!(
                s.phase,
                MigrationPhase::StopAndCopy | MigrationPhase::Postcopy | MigrationPhase::Done
            ) {
                break;
            }
            // Model the dirty shrink.
            let copy_time_s = dirty as f64 / p.copy_bandwidth_bps as f64;
            dirty = (p.dirty_rate_bps as f64 * copy_time_s) as u64;
            dirty = dirty.max(4096);
        }
        assert!(
            matches!(s.phase, MigrationPhase::StopAndCopy),
            "should converge to StopAndCopy under low dirty rate"
        );
    }

    #[test]
    fn state_serializes_round_trip() {
        let s = MigrationState::new();
        let json = serde_json::to_string(&s).unwrap();
        let back: MigrationState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.phase, s.phase);
    }
}
