//! Clone-N-from-1: suspend/resume + CoW disk overlays (PRD Phase 4, §9a, §9b).
//!
//! PRD §9a: "Clone = restore the same snapshot N times. Boot+init once,
//! snapshot, then stamp out many VMs; combined with CoW disk overlays and
//! netns rewriting, this is the 'clone a running VM' primitive."
//!
//! PRD §9b: "Suspend = pause vCPUs + take a snapshot; Resume = restore
//! (ideally UFFD-lazy)."
//!
//! This module owns the *clone plan*: given one base snapshot + a clone
//! count, produce the per-clone disk-overlay + netns-rewrite spec. The
//! actual reflink/netns syscalls are Linux-only and land with the runtime.

use serde::{Deserialize, Serialize};

/// A single clone's disk-overlay configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CloneOverlay {
    /// The clone's index (0..N-1).
    pub index: u32,
    /// Path to the clone's CoW overlay file (reflink/Copy-on-Write from the
    /// base volume). Each clone gets its own so writes don't cross-talk.
    pub overlay_path: String,
    /// The immutable base volume the overlay is backed by.
    pub base_path: String,
}

/// A single clone's network rewrite spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CloneNetSpec {
    pub index: u32,
    /// Unique tap name for this clone (avoids IP collisions, PRD §8).
    pub tap_name: String,
    /// Unique MAC for this clone.
    pub mac: [u8; 6],
    /// Unique netns name.
    pub netns: String,
}

/// The full plan for cloning a snapshot N times.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClonePlan {
    pub base_snapshot: String,
    pub overlays: Vec<CloneOverlay>,
    pub nets: Vec<CloneNetSpec>,
}

/// Build a clone plan for `n` clones of `base_snapshot`, each with its own
/// overlay file (CoW from `base_volume`), tap, MAC, and netns.
///
/// MAC assignment: index is written into the last two bytes of a
/// locally-administered unicast prefix (`02:00:00:00:XX:YY`), so clones
/// get unique, deterministic MACs that don't collide with each other or
/// with the base (index 0).
pub fn build_clone_plan(
    base_snapshot: &str,
    base_volume: &str,
    n: u32,
    overlay_dir: &str,
) -> ClonePlan {
    let mut overlays = Vec::with_capacity(n as usize);
    let mut nets = Vec::with_capacity(n as usize);
    for i in 0..n {
        overlays.push(CloneOverlay {
            index: i,
            overlay_path: format!("{overlay_dir}/clone-{i}.overlay"),
            base_path: base_volume.to_string(),
        });
        nets.push(CloneNetSpec {
            index: i,
            tap_name: format!("cln{i}tap0"),
            mac: clone_mac(i),
            netns: format!("cln{i}ns"),
        });
    }
    ClonePlan {
        base_snapshot: base_snapshot.to_string(),
        overlays,
        nets,
    }
}

/// Deterministic MAC for clone `i`: `02:00:00:00:HI:LO`.
pub fn clone_mac(i: u32) -> [u8; 6] {
    [
        0x02,
        0x00,
        0x00,
        0x00,
        ((i >> 8) & 0xFF) as u8,
        (i & 0xFF) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_has_n_clones() {
        let p = build_clone_plan("/base.snap", "/base.vol", 5, "/tmp");
        assert_eq!(p.overlays.len(), 5);
        assert_eq!(p.nets.len(), 5);
    }

    #[test]
    fn each_clone_has_unique_overlay_and_tap() {
        let p = build_clone_plan("/b.snap", "/b.vol", 4, "/tmp");
        let overlays: Vec<_> = p.overlays.iter().map(|o| &o.overlay_path).collect();
        let taps: Vec<_> = p.nets.iter().map(|n| &n.tap_name).collect();
        let macs: Vec<_> = p.nets.iter().map(|n| n.mac).collect();
        // All unique.
        assert_eq!(
            overlays
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            4
        );
        assert_eq!(
            taps.iter().collect::<std::collections::HashSet<_>>().len(),
            4
        );
        assert_eq!(
            macs.iter().collect::<std::collections::HashSet<_>>().len(),
            4
        );
    }

    #[test]
    fn macs_are_local_unicast_and_encode_index() {
        for i in [0u32, 1, 42, 255, 256, 65535] {
            let mac = clone_mac(i);
            assert_eq!(mac[0] & 0b11, 0b10, "clone {i}: not local unicast");
            assert_eq!(((mac[4] as u32) << 8) | mac[5] as u32, i);
        }
    }

    #[test]
    fn overlays_all_share_same_base() {
        let p = build_clone_plan("/b.snap", "/b.vol", 3, "/tmp");
        for o in &p.overlays {
            assert_eq!(o.base_path, "/b.vol");
        }
    }

    #[test]
    fn plan_serializes_round_trip() {
        let p = build_clone_plan("/b.snap", "/b.vol", 2, "/tmp");
        let s = serde_json::to_string(&p).unwrap();
        let back: ClonePlan = serde_json::from_str(&s).unwrap();
        assert_eq!(back.overlays.len(), p.overlays.len());
    }
}
