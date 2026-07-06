//! vmm-snapshot: full + diff snapshot/restore (PRD §9).
//!
//!   - `Persist` traits on every device → state file (CRC'd).
//!   - Guest RAM → memory file (full copy or UFFD-lazy).
//!   - Diff snapshots use the dirty bitmap to record only changed pages.
//!   - Restore: create a fresh VMM, load state + memory; eager or UFFD-lazy.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod clone;
pub mod crc;
pub mod diff;
pub mod format;
pub mod live;
pub mod restore;
pub mod snapshot;
pub mod state;

pub use clone::{build_clone_plan, clone_mac, CloneNetSpec, CloneOverlay, ClonePlan};
pub use format::{Snapshot, SnapshotMeta};
