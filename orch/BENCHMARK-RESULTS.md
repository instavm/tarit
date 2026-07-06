# taritd warm-pool TTI benchmark

Our own port of the ComputeSDK sandbox benchmark
(`/path/to/benchmarks`), run **through the
orchestrator** (`tarit-bench` → taritd HTTP API → VMM). Time-To-Interactive
(TTI) is the wall-clock from `POST /v1/vms` (create) to the first successful
`node -v` execution; teardown is not timed. Three modes: sequential, staggered,
burst — each reports p50/p95/p99 over 100 runs.

## Environment (read this before the numbers)

- **Host: c8i, 2 vCPU / 7 GB, NESTED virtualization.** This is a tiny, ~10x
  slower box. Absolute latencies are dominated by it, not by the orchestrator.
- Guest: **`node:20-slim` pulled as an OCI image** and booted as a microVM
  (`vmm pull --agent` injects the exec agent as PID 1). Rootfs mounted as a
  **shared read-only base** (one image, all VMs, no per-VM copy).
- VM class: **1 vCPU / 128 MiB** (node -v runs fine even at 64 MiB).
- Warm pool: **target 24**, hard cap **`max_vms=32`**, CPU overcommit 400%,
  replenish concurrency 4. Config read from `~/.taritd/config.toml` at startup.
- Command: `node -v` → `v20.20.2`.

## Results (100 runs each)

| mode | success | p50 | p95 | p99 | min | max | warm/cold |
|------|--------:|----:|----:|----:|----:|----:|-----------|
| sequential | 100% | 363 ms | 647 ms | 683 ms | **85 ms** | 5562 ms | mostly warm |
| staggered (200 ms) | 100% | 15 972 ms | 62 061 ms | 63 931 ms | 92 ms | 66 s | 58 / 42 |
| burst 100 (admission 60 s) | 96% | 17 844 ms | 42 084 ms | 66 229 ms | 1028 ms | 67 s | 26 / 70 |
| burst 100 (admission 180 s) | **100%** | 12 374 ms | 66 833 ms | 67 343 ms | 368 ms | 73 s | 28 / 72 |

Raw JSON: `bench-results/{sequential,staggered,burst}_tti/*.json` (ComputeSDK
schema). Warm-pool `create()` alone is **~15 ms** (measured directly); the best
end-to-end warm TTI observed was **85 ms** (create + node -v), near the <100 ms
north star even on this nested box.

## What the burst shows (the questions we set out to answer)

- **Does the pool backfill fast?** Yes. It sat at 24 ready, drained during the
  burst, and refilled to 24 afterwards. During the burst it can only refill as
  running sandboxes finish and free a slot (warm + assigned share the 32-slot
  cap), which is the correct behavior.
- **Warm exhausted → wait/cold-start, or service errors?** We **wait and
  cold-start** — graceful degradation, not errors. Of 100 concurrent requests:
  ~28 got an instant warm VM, ~72 cold-started as slots freed, a couple waited
  then got a warm VM. With a generous admission timeout the burst is **100%
  success with zero errors**. With a tight 60 s timeout, 4 requests that waited
  the full 60 s returned a clean **HTTP 409 backpressure** (not a crash).
- **Why is the latency high?** Physics of the box: cap 32 on **2 physical
  cores** is 16x CPU oversubscription, so each `node -v` that takes ~50 ms
  uncontended stretches to seconds. This is a hardware ceiling, not an
  orchestrator limit — the create path itself stays ~15 ms.

## Tuning notes / how to make these numbers good

- **Bigger box is the main lever.** On N real cores the same code keeps warm
  `create` ~15 ms and runs `node -v` uncontended, so warm TTI lands well under
  100 ms and cold under a few hundred ms. c8i's 2 nested cores are the cap here.
- On a small box, set `max_vms` near the core count (e.g. 4-8), not 32, to trade
  admission waiting for far lower per-request latency.
- `TARIT_ADMISSION_TIMEOUT_MS` chooses backpressure vs. patience: low = fast
  429/409 under overload; high = everyone waits and eventually succeeds.

## Reproduce

```sh
# start taritd with the warm pool (config at ~/.taritd/config.toml)
TARIT_ROOTFS_READONLY=1 TARIT_MAX_VMS=32 \
TARIT_CONFIG=~/.taritd/config.toml ./target/release/taritd

# run the benchmark through the orchestrator
tarit-bench all --url http://127.0.0.1:8080 --api-key <key> \
  --memory-mib 128 --rootfs /tmp/node-oci.ext4 --command 'node -v'
```


## Orchestrator warm-pool TTI (bare metal)

Run on 2026-07-02 UTC on c8i.metal-48xl bare metal, 192 vCPU / 377 GB, us-east-1, Ubuntu 24.04. `taritd` ran as a single node with warm pool enabled, target 8, max VMs 32, VMM kernel `/tmp/vmlinux.minimal.vsock`, rootfs `/tmp/bench-node-rootfs.ext4`, and `TARIT_ROOTFS_READONLY=1`.

| command | success | create-only p50 | create-only p95 | create to first exec p50 | create to first exec p95 | warm/cold | notes |
|---|---:|---:|---:|---:|---:|---|---|
| `node -v` | 20/20 | 12.304 ms | 15.053 ms | 5124.695 ms | 5249.390 ms | 20 / 0 | All handouts came from the warm pool. TTI was dominated by a 5 s UDS exec failure followed by one-shot fallback. |
| `echo ok` | 20/20 | 8.401 ms | 9.012 ms | 5120.239 ms | 5130.257 ms | 20 / 0 | Same fallback path as `node -v`. |

Commands:

```sh
cd /path/to/tarit/orch
cargo build --release -p taritd -p tarit-bench
sudo python3 /path/to/metal-bench/orch_bench_small.py
```

Raw result: `/path/to/metal-bench/orch-results.json` on the metal host.


## Warm-pool concurrency: 200x(1 vCPU / 512 MiB), node -v, n=100 (bare metal, 2026-07-02)

Same host (c8i.metal-48xl, 192 vCPU / 377 GB). `taritd` single node, warm pool target 200,
max VMs 260, rootfs `/tmp/bench-node-rootfs.ext4` (e2fsck-clean), `TARIT_ROOTFS_READONLY=1`.
TTI = start (create) to first `node -v` returning exit 0, measured per iteration (so burst
and staggered are per-request, not wall-clock).

### Fixes landed in this run

- **exec-fallback**: the warm class pointed at a missing rootfs (`/tmp/node-oci.ext4`);
  pointing it at the real node rootfs removed the 5.12 s UDS-exec fallback (5124 ms -> 37 ms
  sequential).
- **SQLite WAL**: store opened `journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout=5s`
  (was default rollback journal, fsync per write). ~2x on the concurrent modes.
- **In-memory exec-status cache**: `get_execution` (client polls every 15 ms) served from an
  `RwLock<HashMap>` write-through, not the single SQLite connection mutex.
- **stop_vm lock fix (the big one)**: remove the VM from the running map under a brief lock,
  then do the stop RPC + `child.kill()` + `child.wait()` UNLOCKED. Before, the global
  `running` mutex was held across `child.wait()`, so each per-iteration delete serialized
  every other iteration's exec. Burst p95 5841 ms -> 90 ms (65x).
- **Restore-based warm replenishment** (`restore = true`): boot one golden VM, wait until it
  is exec-ready, snapshot it, clone the rest via UFFD restore. Fill 200 VMs: cold ~9-18 s ->
  restore ~4 s (golden ready ~2.3 s + clones).
- **Readiness probe**: warm VMs and the restore golden only park after the guest agent can
  run a command, so a still-booting VM handed out mid-burst can't block on the first agent dial.

### Results (node -v, n=100)

| mode | p50 | p95 | success | before fixes (p95) |
|---|---:|---:|---:|---:|
| sequential | 36 ms | 36 ms | 100% | 37 ms |
| staggered (20 ms) | 34 ms | 35 ms | 100% | 10428 ms |
| burst (100 concurrent) | 72 ms | 90 ms | 100% | 5841 ms |

Isolation (burst 100, trivial `true`): p50 55 ms / p95 82 ms. That is the orchestrator +
microVM floor (create + exec dispatch + delete + 15 ms client poll), independent of node.
`node -v` adds 98 MB binary load + V8 init x100-concurrent on top.

### Gap to ComputeSDK (~20 ms warm burst): investigation + fix plan

Sequential and staggered are already <60 ms p95. Burst-100 p95 (90 ms) is floored by
orchestrator overhead (true-burst p95 82 ms) plus node startup. To close it:

1. **Take the store off the create/exec/delete hot path.** create (insert_vm),
   resolve_owner (get_vm_host SELECT) and delete all hit one `Mutex<SQLite>`; at 100-wide
   that serializes ~500 ops. Mirror the exec-status cache: keep VM records + ownership in
   memory (RwLock/DashMap), persist write-behind. Highest leverage.
2. **Defer VM teardown off the request path.** delete should unregister + return; push stop
   RPC + kill + wait to a background reaper. Removes all teardown contention.
3. **Kill the 15 ms poll.** A blocking/streaming exec endpoint (SSE or long-poll) reports
   completion the instant the agent replies. Worth ~7-15 ms of every TTI; likely how
   ComputeSDK reports ~20 ms.
4. **Pre-warm node in the golden snapshot.** Run `node -v` once before snapshotting so V8/JIT
   and the 98 MB binary are resident; clones skip the cold node cost (72 ms p50 vs 55 ms
   for `true`).
5. **Trim per-exec writes.** execute_async does insert_execution + Running + Completed (3
   writes) + resolve_owner (1 read). Collapse to one terminal write; drop the Running hop.
6. **Bound burst fan-out / shard the store lock** so 100 simultaneous node startups don't
   oversubscribe cores.

Items 1-3 are pure orchestrator changes (no VMM work) and should get burst under 60 ms.

### ComputeSDK-methodology result (write-behind store + sync exec + bounded refill)

Run to the ComputeSDK benchmark spec (METHODOLOGY.md: `node -v` TTI = create ->
first runCommand, sequential n=100, staggered concurrency=100 @ **200ms** delay,
burst concurrency=100, **120s** timeout, median-weighted score). Bare metal
(c8i.metal-48xl), warm pool target=200, per-VM CoW overlays, ~20 repeats of n=100
(~5500 iterations), **0 failures**:

| mode | p50 | p95 | p99 | success |
|---|--:|--:|--:|--:|
| sequential (n=100) | 35 ms | **38 ms** | 46 ms | 100% |
| burst (100 concurrent) | 48 ms | **57 ms** | 58 ms | 100% |
| staggered (100 @ 200 ms) | 35 ms | **39 ms** | 45 ms | 100% |

All modes < 60 ms p95. For reference, the fastest public ComputeSDK providers are
~400 ms-1.7 s (sequential/burst) [results/*/latest.json], so a ready warm pool is
~10x faster. What got here: `stop_vm` lock fix, SQLite WAL, in-memory exec + VM
caches with a **write-behind store**, a **synchronous `/v1/execute`** endpoint
(kills the 15 ms client poll), an honest live-agent exec (no fake fallback), and a
**bounded continuous-pipeline refill** (conc=48; conc=100 saturated the host and
starved live execs -> staggered 30 s).

Replenishment note: cold-boot refill is CPU-bound and cannot sustain heavy bursts;
the optimal design is snapshot-restore refill + CPU-isolated rate-limited admission
+ hysteresis. See `docs/REPLENISHMENT.md`.


### VM-operation latency (live taritd REST API, single 1 vCPU / 512 MiB node microVM)

Bare metal (c8i.metal-48xl). These are KVM-exit / memory-op bound, so they are much
faster on bare metal than nested. Driven through the orchestrator's public REST API
(so the numbers include HTTP + orchestrator + VMM RPC, not just the raw KVM op).

| op | p50 | p95 | notes |
|---|---:|---:|---|
| `GET /v1/vms/{id}/status` | 1.32 ms | 1.46 ms | live VMM status |
| `POST .../pause` (suspend) | 2.62 ms | 2.87 ms | KVM vCPU pause |
| `POST .../resume` | 1.39 ms | 1.55 ms | KVM vCPU resume |
| `POST .../snapshot` (full) | 178.7 ms | 342.9 ms | 512 MiB memory dump to disk |
| restore (`POST /v1/restore`) | see note | | warm-pool restore fill = golden ready ~2.3 s + 200 clones ~4 s total; VMM-level UFFD restore ~2.9 ms (see `vmm/docs/METAL-BENCHMARKS.md`) |

Reproduce with `scripts/bench-vmops.sh` against a running taritd.

---

## Machine + reproducing

Headline numbers above were produced on:

| | |
|---|---|
| Instance | **AWS c8i.metal-48xl** (bare metal, `lscpu` shows no hypervisor) |
| vCPU / RAM | **192 vCPU / 384 GiB** (393216 MiB) |
| Network | 75 Gbit |
| Region / AZ | us-east-1 / us-east-1d |
| OS | Ubuntu 24.04, Linux + KVM (`/dev/kvm`) |
| Rust | stable 1.95 |
| AMI (our run) | a private convenience image (Ubuntu 24.04 with the toolchain + assets pre-staged); any stock Ubuntu 24.04 AMI works with the steps below |

A nested KVM guest (e.g. c8i.xlarge with nested virt) also works but pays a ~10x
KVM-exit tax; the bare-metal type above is what these results were measured on.

### Steps

```sh
# 1. Launch the box (bare metal for the headline numbers)
aws ec2 run-instances --region us-east-1 --instance-type c8i.metal-48xl \
    --image-id <ubuntu-24.04-amd64-ami> --key-name <key> ...

# 2. On the host: toolchain + KVM
sudo apt-get update && sudo apt-get install -y build-essential pkg-config libssl-dev e2fsprogs
curl https://sh.rustup.rs -sSf | sh -s -- -y   # Rust stable

# 3. Build both workspaces
git clone https://github.com/instavm/tarit && cd tarit
(cd vmm  && cargo build --release)
(cd orch && cargo build --release)

# 4. Stage a guest kernel + a node rootfs (ext4 with node + the vmm-agent).
#    Kernel: a minimal x86_64 microVM kernel with virtio-mmio + vsock.
#    Rootfs: `vmm pull --agent node:20-slim` produces a node image with the agent
#            injected as init; or an ext4 with node + the vmm-agent unit.
#    export TARIT_KERNEL=/path/to/vmlinux  TARIT_ROOTFS=/path/to/node-rootfs.ext4

# 5. Run the warm-pool TTI benchmark (create -> node -v exit 0)
cd orch
./scripts/bench-warmpool.sh                 # restore pool, 200 VMs, n=100
MODE=cold ./scripts/bench-warmpool.sh       # cold-boot pool for comparison

# 6. (optional) single-VM op latency (pause/resume/snapshot/restore)
#    start taritd (bench-warmpool.sh leaves it running until it exits), then:
./scripts/bench-vmops.sh
```

`scripts/bench-warmpool.sh` prints the host type (bare metal vs nested), builds
`taritd` + `tarit-bench`, writes the warm-pool config, fills the pool, runs
sequential / staggered / burst at n=100, and tears everything down on exit.


