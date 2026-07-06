# 12 — Phase 5: live snapshot convergence (M13)

*Goal: background snapshot with a sub-ms–few-ms pause (PRD Phase 5, §9c,
§12.3).*

## What I did

### `vmm-snapshot/src/live.rs`

`PrecopyParams { mem_bytes, dirty_rate_bps, copy_bandwidth_bps, target_downtime_us, max_rounds }`
+ `RoundDecision { Continue | FinalStop | Diverging }` + `decide()` +
`simulate()`:

- `decide(params, round, dirty_bytes)` — the per-round stop/continue call.
  - **FinalStop** when `dirty_bytes / copy_bandwidth_bps <= target_downtime_us`
    (the residual fits in the downtime budget — PRD §9c: "When the remaining
    dirty set is small enough that it can be copied within the target downtime,
    do a brief final stop").
  - **Diverging** when `dirty_rate >= copy_bandwidth` after round 2 (we can
    never catch up — PRD §9c: "graceful fallback when dirty rate exceeds copy
    bandwidth"). Triggers post-copy (§9d).
  - **Continue** otherwise.
- `simulate(params, dirty_bytes)` — drives the loop to convergence (or
  divergence), modeling each round's residual as
  `dirty_rate_bps * (dirty_bytes / copy_bandwidth_bps)` (the pages re-dirtied
  during the last round's copy).

Six tests: small-dirty-stops-immediately, large-dirty-continues,
diverging-on-high-dirty-rate, simulate-converges-under-low-rate,
simulate-diverges-under-high-rate, max_rounds-forces-final-stop.

## What worked

- **The convergence math is genuinely testable as pure arithmetic.** The
  PRD §12.3 "assert the pre-copy loop converges and the final pause window
  stays under target" reduces to `dirty_bytes / bw <= downtime`, which is
  one `decide()` call. No KVM, no dirty-ring, no UFFD — just `u64` division.
- **`Diverging` as a first-class outcome** (not just an error) cleanly
  encodes the post-copy fallback trigger. The migration state machine
  (M15) consumes it directly.

## What went wrong

### F1. `max_rounds_forces_final_stop` failed — dirty_rate == copy_bandwidth

```
expected FinalStop, got Diverging
```

The test set `dirty_rate_bps == copy_bandwidth_bps`, which trips the
`Diverging` check at round 2 *before* `max_rounds` (3) is reached. The
`Diverging` check uses `>=`, so equality diverges. Fixed by making
`dirty_rate` strictly less than `copy_bandwidth` in the max-rounds test
(0.9 × bw) — the intent is "we *are* converging slowly, but max_rounds
forces a stop." **Lesson:** when two stop conditions interact, test them
in isolation (one with dirty_rate < bw, one with dirty_rate >= bw).

### F2. Clippy: `1 * 1024 * 1024` → identity op

The leading `1 *` is a no-op. `cargo fmt`/clippy flagged it. Removed.

## What I learned

- **The pre-copy convergence criterion is a one-liner**, but modeling the
  dirty-rate shrink across rounds is the subtle part. The simple model
  (residual = dirty_rate × last_round_copy_time) isn't accurate to real
  dirty-ring behavior, but it's enough to drive the state-machine tests.
  Real convergence tuning happens on a real Linux+KVM host with a real
  dirty-ring — the scaffold proves the *logic*, not the *numbers*.
- **Equality in divergence checks matters.** `dirty_rate >= bw` diverges;
  `dirty_rate == bw` is the knife-edge. Tests need to pick a side.

## Next

`13-egress-perf.md` — Phase 6: DNS-aware allowlists + warm pools.
