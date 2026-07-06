# Warm-pool replenishment: optimal design under CPU + latency constraints

## The two constraints

1. **Latency**: `create()` must hand out a ready VM instantly so Time-To-Interactive
   (create -> first `node -v`) stays well under 100 ms, even under a burst of 100
   concurrent creates.
2. **CPU**: cold systemd microVM boots are CPU-heavy (~5-10 s each). Refilling
   aggressively (e.g. 100 concurrent boots) **saturates the host and starves live
   execs**. Measured directly here: 100 concurrent cold boots during a burst made a
   subsequent staggered run time out at 30 s.

## What we found (benchmarked)

- Cold-boot warm pool, **bounded** refill (continuous pipeline, `replenish_concurrency=48`,
  `target=200`), ComputeSDK-correct params (staggered 200 ms, 120 s timeout), on
  c8i.metal-48xl: sequential p95 38 ms, burst(100) p95 56 ms, staggered p95 37 ms,
  100% success. All < 60 ms.
- Cranking refill concurrency to 100 (unbounded-ish) **regressed** staggered to 30 s
  (CPU starvation). "More aggressive" cold-boot refill is *worse*, not better.
- For reference, the fastest ComputeSDK providers are ~400 ms-1.7 s TTI
  (sequential/burst) [1]. A ready warm pool serves ~10x faster.

## Optimal architecture (research-backed)

Cold boot must become a **rare, offline template-build step**, not the runtime refill
path. Runtime replenishment should be **snapshot restore** of a pre-initialized golden,
protected by CPU-isolated, rate-limited admission.

```
Per template (offline / builder node):
  cold-boot one golden VM  ->  init systemd + agent + node runtime
  (optionally prime node: run `node -v` so its pages are resident)
  pause  ->  full snapshot (vmstate + memory file + read-only base disk)
  cache snapshot on each host

Per host (runtime):
  ready pool = already-restored idle clones
  clone = Firecracker-style snapshot restore:
      memory = MAP_PRIVATE of the snapshot file + UFFD lazy page-in (CoW)  [2][3]
      disk   = shared read-only base + per-clone CoW overlay               [2]
      net    = per-clone netns / TAP / MAC / IP                            [4]
  create() = atomic lease of a ready clone (no boot path)

Refill (background):
  continuous snapshot-restore pipeline (NOT big cold-boot batches)
  bounded by a max-in-flight semaphore + CPU-aware token bucket
  refill runs in a low-priority cgroup (low CPUWeight / CPUQuota / cpuset);
  live execs run in a reserved/high-weight cgroup -> refill never starves them  [5][6]
  cold boots only offline or severely throttled (<=1-2/host)
  hysteresis: hard_floor 100, low_watermark 110, target 140, high_watermark 160;
  trickle above low_watermark, priority (still CPU-gated) below it
```

### Why this satisfies both constraints

- **Latency**: `create()` leases an already-restored VM -> sub-100 ms TTI.
- **CPU**: snapshot restore skips kernel+systemd init, so it is cheap and fast
  (AWS SnapStart resumes from cached 512 KB chunks at ~1 ms L1 / single-digit ms L2
  per chunk [7]); bounded concurrency + a low-priority refill cgroup keep it from
  stealing CPU from live execs.

### Evidence

- E2B restores Firecracker snapshots via UFFD (`MemBackend: Uffd`), waits for UFFD
  readiness, then resumes; its filesystem-only fallback is an explicit cold boot that
  loses guest RAM -> memory-snapshot restore is the fast path [8].
- AWS Lambda SnapStart is the production clone-from-snapshot pattern (init once,
  snapshot, resume many) taking startup from seconds to sub-second [7].
- E2B bounds concurrent starts/resumes with a semaphore and returns ResourceExhausted
  when too many are in flight -> bounded refill, not unbounded [8].
- AWS Provisioned Concurrency keeps pre-initialized envs ready (double-digit ms) and
  recommends sizing = peak concurrency + ~10% buffer; for a "100-at-once" API keep an
  absolute burst buffer >= 100 ready per hot template [9].

## Current state in this repo

- Cold-boot pool with a **bounded continuous-pipeline** refill (implemented: no batch
  barrier, adaptive sleep, `warmpool.rs`). Works < 60 ms at ComputeSDK load.
- **Restore-based refill is available** (`restore = true`: `create_golden` +
  `spawn_warm_restore`, UFFD lazy restore ~ms). `taritd` cold-boots one golden
  per warm class, snapshots it after readiness, tears down the builder VM, then
  restores warm clones with a per-VM overlay override so restored clones do not
  share writable disk state.
- Refill-spawned VMM children can be moved into a low-weight cgroup v2 CPU group
  (`TARIT_REFILL_CGROUP`, `TARIT_REFILL_CPU_WEIGHT`, default weight 10) so
  default-weight live-created VMs win CPU contention; if cgroups are unavailable,
  refill logs and continues.
- Each class now has hysteresis sizing: `hard_floor <= low_watermark <= target <=
  high_watermark`. Refill starts below `low_watermark` and tops back up to
  `target`, with `target` deriving sensible defaults when the other watermarks
  are omitted.

## Remaining work toward the optimal design

1. **Use restore refill for latency-sensitive classes**: `restore = true` is safe
   for warm classes because every restored clone gets its own overlay; cold boot
   remains available with `restore = false`.
2. **CPU-aware token bucket**: pause refill when live-exec p95 > SLO or
   host CPU idle < 20%.

## Citations

- [1] ComputeSDK results: results/sequential_tti/latest.json, results/burst_tti/latest.json (computesdk/benchmarks)
- [2] Firecracker snapshot support (MAP_PRIVATE, shared memory/disk, CoW): https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md
- [3] Firecracker UFFD page-fault handling: https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/handling-page-faults-on-snapshot-resume.md
- [4] Firecracker network for clones: https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/network-for-clones.md
- [5] cgroup v2 CPU partitioning: https://docs.kernel.org/admin-guide/cgroup-v2.html
- [6] Firecracker jailer (cgroups): https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md
- [7] AWS Lambda SnapStart internals: https://aws.amazon.com/blogs/compute/under-the-hood-how-aws-lambda-snapstart-optimizes-function-startup-latency/
- [8] E2B infra (Firecracker UFFD restore + start semaphore): https://github.com/e2b-dev/infra (packages/orchestrator/pkg/sandbox/fc, .../uffd, .../server)
- [9] AWS Lambda Provisioned Concurrency: https://docs.aws.amazon.com/lambda/latest/dg/provisioned-concurrency.html
