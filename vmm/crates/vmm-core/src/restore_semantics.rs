//! Clock + PRNG semantics for snapshot/restore/clone.
//!
//! When a VM is snapshotted and restored (or cloned N times), several
//! things must be handled correctly:
//!
//! **Clocks (kvmclock):**
//! - The guest uses kvmclock (KVM's paravirtualized clock) for time.
//! - On restore, the guest's clock reads the old TSC value → time jump.
//! - Tarit restores the saved KVM clock with `KVM_CLOCK_REALTIME` cleared,
//!   notifies restored vCPUs with `KVM_KVMCLOCK_CTRL`, and repairs realtime in
//!   the guest during the mandatory pre-admission clone-repair exchange. This
//!   advances wall time without incorrectly advancing monotonic timers.
//! - For clones: each clone gets its own kvmclock offset (KVM creates a
//!   fresh VM, so the clock starts from the host's current time).
//!
//! **PRNG (virtio-rng):**
//! - The guest's CRNG state is in the memory snapshot.
//! - If 100 clones share the same CRNG state → they produce the same
//!   random numbers → security vulnerability.
//! - Virtio-rng can feed fresh host entropy, but its presence and a clock jump
//!   do not prove that the cloned kernel CRNG has consumed it before returning
//!   bytes. Userspace PRNGs and cached tokens are outside the kernel CRNG.
//! - Secure multi-resume therefore still requires a VM generation device and
//!   a pre-admission userspace post-fork repair hook. Until those exist, the
//!   flag below describes required work, not a completed action.
//!
//! **What we need to do on restore:**
//! 1. Create a fresh KvmVm (new VM = new kvmclock base)
//! 2. Load the memory snapshot (guest CRNG state is in there)
//! 3. Load the device state (but DON'T restore rng bytes_served)
//! 4. Change the guest-visible VM generation before vCPUs resume
//! 5. Wait for kernel reseed and userspace repair before admitting traffic
//!
//! **What we DON'T need to do:**
//! - Assume an available entropy device has already reseeded cloned state
//! - Set a specific TSC value (KVM handles it via kvmclock)
//! - Worry about timer interrupts (the PIT/HPET is recreated by KvmVm::new)

use serde::{Deserialize, Serialize};

/// Clock restore configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClockRestoreConfig {
    /// Whether to reset the guest's clock on restore (default: true).
    /// KVM handles this via KVM_SET_CLOCK — the guest detects the jump
    /// and resyncs.
    pub reset_clock: bool,
    /// Whether to force CRNG re-seed on restore (default: true).
    /// This is a required action. It is not satisfied merely by wiring a fresh
    /// virtio-rng device.
    pub force_crng_reseed: bool,
}

impl ClockRestoreConfig {
    pub fn default_for_clone() -> Self {
        Self {
            reset_clock: true,
            force_crng_reseed: true,
        }
    }
}

/// Post-restore actions for a VM (or clone).
///
/// Call this after loading the memory snapshot + device state into a
/// fresh KvmVm. The actions ensure the guest's clock and PRNG are correct.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PostRestoreActions {
    /// The clock was reset (kvmclock jumped).
    pub clock_reset: bool,
    /// The CRNG must be re-seeded before the clone is admitted.
    pub crng_reseed_pending: bool,
}

/// Compute the post-restore actions for a VM.
///
/// Compute the required post-restore policy. Applying these requirements is a
/// separate operation; this pure value must never be treated as proof that the
/// guest observed a generation change or completed a reseed.
pub fn compute_post_restore(config: &ClockRestoreConfig) -> PostRestoreActions {
    PostRestoreActions {
        clock_reset: config.reset_clock,
        crng_reseed_pending: config.force_crng_reseed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_config_resets_everything() {
        let cfg = ClockRestoreConfig::default_for_clone();
        assert!(cfg.reset_clock);
        assert!(cfg.force_crng_reseed);
    }

    #[test]
    fn post_restore_marks_crng_reseed() {
        let cfg = ClockRestoreConfig::default_for_clone();
        let actions = compute_post_restore(&cfg);
        assert!(actions.clock_reset);
        assert!(actions.crng_reseed_pending);
    }

    #[test]
    fn post_restore_no_reset() {
        let cfg = ClockRestoreConfig {
            reset_clock: false,
            force_crng_reseed: false,
        };
        let actions = compute_post_restore(&cfg);
        assert!(!actions.clock_reset);
        assert!(!actions.crng_reseed_pending);
    }

    #[test]
    fn actions_serialize_round_trip() {
        let actions = PostRestoreActions {
            clock_reset: true,
            crng_reseed_pending: true,
        };
        let s = serde_json::to_string(&actions).unwrap();
        let back: PostRestoreActions = serde_json::from_str(&s).unwrap();
        assert_eq!(back.clock_reset, actions.clock_reset);
    }
}
