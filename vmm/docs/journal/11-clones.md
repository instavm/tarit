# 11 — Phase 4: clones + CoW overlays (M12)

*Goal: suspend/resume + clone-N-from-1 with CoW disk overlays + per-clone
netns/IP rewrite (PRD Phase 4, §9a, §9b).*

## What I did

### `vmm-snapshot/src/clone.rs`

`ClonePlan { base_snapshot, overlays: Vec<CloneOverlay>, nets: Vec<CloneNetSpec> }`
+ `build_clone_plan(base_snapshot, base_volume, n, overlay_dir)`:
- Each clone gets its own `overlay_path` (CoW overlay file) backed by the
  same immutable `base_path` — writes don't cross-talk (PRD §9a: "clone =
  restore the same snapshot N times; combined with CoW disk overlays and
  netns rewriting").
- Each clone gets a unique tap name (`cln{i}tap0`), a deterministic MAC
  (`02:00:00:00:HI:LO` — locally-administered unicast, encodes the index),
  and a unique netns (`cln{i}ns`).

Five tests: plan has N clones, each clone's overlay/tap/MAC is unique, MACs
are local-unicast and encode the index, all overlays share the same base,
plan serializes round-trip.

## What worked

- **Deterministic MAC assignment** (`02:00:00:00:HI:LO`) means clone IPs
  never collide and are predictable for testing — no random MAC generation,
  no collision retry. The `(mac[4] as u32) << 8 | mac[5]` test proves the
  index round-trips through the MAC.

## What went wrong

No failures — this milestone was pure data-shape work on top of the M10
snapshot primitives.

## Next

`12-live-snap.md` — Phase 5: live snapshot pre-copy convergence loop.
