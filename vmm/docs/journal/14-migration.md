# 14 — Phase 7: live migration state machine (M15)

*Goal: pre-copy + post-copy migration between hosts (PRD Phase 7, §9d).*

## What I did

### `vmm-migration/src/state.rs`

`MigrationPhase { Negotiation, DestinationPrep, Precopy, StopAndCopy, Postcopy, Cutover, Done, Aborted }`
+ `MigrationState { phase, round, dirty_bytes }` + `advance(decision)`:

The state machine walks the PRD §9d flow:
1. **Negotiation** → source/dest agree on protocol version, CPU template,
   RAM, device model, volume identity (the `NegotiationRequest` from
   `negotiation.rs`).
2. **DestinationPrep** → dest spins up a paused VMM shell with matching
   memory regions + device config, registers guest RAM with UFFD.
3. **Precopy** → source enables dirty tracking + streams RAM pages; each
   round's `RoundDecision` (reused from `vmm_snapshot::live`, PRD §9d: "the
   same pre-copy / post-copy loop as the live snapshot") drives the
   transition:
   - `Continue` → stay in Precopy, increment round.
   - `FinalStop` → StopAndCopy (sub-5ms blackout to flush the residual +
     device/vCPU state).
   - `Diverging` → Postcopy fallback (hand control to dest early, demand-
     fetch pages over the network via UFFD — PRD §9d: "bounds downtime
     regardless of dirty rate").
4. **StopAndCopy / Postcopy** → Cutover → Done.

`MigrationPlan { negotiation, storage_plan: ClonePlan }` — the full plan:
the negotiation agreement + the storage-continuity plan (shared backing or
storage-migration, reusing the M12 `ClonePlan`).

Four tests: happy-path phases (Negotiation→DestPrep→Precopy→StopAndCopy→
Cutover→Done), postcopy-fallback-on-divergence, full-simulate-to-migration
(drives the pre-copy loop with the live module's `decide` + the migration
state machine), state serializes round-trip.

## What worked

- **Reusing `vmm_snapshot::live::RoundDecision`** as the input to the
  migration state machine is the PRD §9d thesis made concrete: "the same
  pre-copy / post-copy loop as the live snapshot." The convergence math
  from M13 drives the migration phase transitions directly — zero new
  math, just a new consumer.
- **`Diverging → Postcopy`** as a state transition (not an error) encodes
  the PRD §9d post-copy fallback cleanly. The migration doesn't fail when
  dirty rate exceeds bandwidth; it switches strategy.

## What went wrong

### F1. `ClonePlan` not re-exported from `vmm-snapshot`

```
error: unresolved import `vmm_snapshot::ClonePlan`
```

`ClonePlan` lived in `vmm_snapshot::clone` but wasn't in the crate root's
re-exports. Added `pub use clone::{build_clone_plan, clone_mac, CloneNetSpec,
CloneOverlay, ClonePlan};` to `vmm-snapshot/src/lib.rs`.

### F2. Unused `decide` import in the lib (used only in tests)

```
warning: unused import: `decide`
```

`decide` is only called inside `#[cfg(test)]`. Moved the import into the
test module.

## What I learned

- **The PRD §9d "90% of migration is already paid for" claim is real.** The
  migration state machine is ~80 lines of `match` over `RoundDecision` —
  the heavy lifting (dirty tracking, Persist serialization, UFFD) all came
  from M3/M10/M13. The net-new code is the phase transitions + the
  negotiation/storage-plan types.
- **A state machine that consumes a decision-enum is easy to test.** Each
  test feeds a sequence of `RoundDecision`s and asserts the phase sequence.
  No KVM, no network — just enum transitions. The `full_simulate_to_migration`
  test drives the *real* `decide()` + `advance()` loop, proving the two
  modules compose correctly.

## Commands to reproduce

```sh
./ci/check.sh   # all 5 green; 4 migration tests + the M13 convergence tests
```

## Summary — all 16 milestones (M0-M15) complete

Phases 0-3 (boot, devices, isolation, snapshot) are code-complete and
cross-type-checked against kvm-ioctls 0.19 on x86_64-linux. Phases 4-7
(clones, live snapshot, egress, migration) are scaffolded with the
correctness-critical pure-Rust logic (clone plans, pre-copy convergence,
DNS expansion, migration state machine) fully unit-tested on macOS.

What still needs a real Linux+KVM host:
- The boot smoke test (`boot_smoke.rs`, `#[ignore]`'d).
- The snapshot/restore integration (`snapshot_roundtrip.rs`).
- The egress-denial red-team (`egress_denial.rs`).
- The §2 perf gates (cold boot <125ms, restore <10ms, etc.).
- The §12.3 live-snapshot consistency harness under load.
- The §12.4 live-migration round-trip.

The KVM-gated runtime code (`KvmVm`, `run_vcpu`, memory registration,
dirty-log ioctl) is cross-type-checked but not executed here. Providing a
real Linux+KVM host unlocks all of the above via
`cargo test --workspace --features kvm -- --include-ignored`.
