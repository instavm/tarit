//! vmm-migration: cross-host live migration.
//!
//! Cross-host live migration is the same pre-copy / post-copy loop
//! as the live snapshot, but the destination is a peer host instead of
//! a local file. Because the design already builds dirty-page tracking,
//! device `Persist` serialization, and a UFFD page-fault handler, most of
//! migration is already paid for; the net-new work is a network transport,
//! a handshake, and a remote page server.
//!
//! Phase 7 scaffold (M15): negotiation, transport, migration state machine
//! (reuses `vmm_snapshot::live` convergence), remote UFFD page server.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod negotiation;
pub mod state;
pub mod transport;

pub use state::{MigrationPhase, MigrationPlan, MigrationState};
