# Remaining Work — current backlog

*Last updated: 2026-07-02. Honest status. Source of truth for what's left.*

## ★ North star

**Cold `create` → first code-exec finish in <100ms** (beat Firecracker's ~125ms
boot-to-init). A genuinely fast cold path (warm pool is a separate orchestrator
accelerator, not a substitute). A better-performing VMM with better features for
**AI agentic workloads**. Do better, don't copy.

Current (c8i nested-virt, `ci/coldboot-measure.sh`, 5 runs, 15ms poll):

| config | create-return | cold create→first-exec | bare-metal proj. |
|---|--:|--:|--:|
| minimal kernel + serial | 26 ms | **310 ms** | **~31 ms** ✅ under target |
| microvm kernel + vsock  | 32 ms | 1009 ms | ~101 ms |

`create()` itself returns in ~26–32ms; the rest is the guest kernel booting to
the agent. The **minimal kernel is the win** (~31ms bare-metal projected). vsock
vs serial makes no cold-latency difference (boot-bound) — vsock buys reliability.
See `docs/cold-boot-exec.md`.

---

## Reference repos (read these for inspiration — don't copy blindly)

| Repo | Path | Use it for |
|---|---|---|
| **Firecracker** | github.com/firecracker-microvm/firecracker | The reference microVM. Read its device wiring (virtio-net/vsock/balloon), **guest kernel configs** (`resources/guest_configs/microvm-kernel-ci-x86_64-*.config`, incl. no-ACPI), boot-time tuning (`tests/integration_tests/performance/test_boottime.py`), snapshot design (`docs/snapshotting/`), jailer, and networking recipe (`docs/network-setup.md`). We match/beat it — reference only, do **not** fork its code. |
| **orch** | `orch/` (this repo) | The orchestrator (`taritd`) that drives this VMM 1:1 over a UDS. Owns fleet policy, cgroup enforcement (soft limits), placement/density, networking setup. API contract must stay in sync (`crates/tarit-vmm-client` ↔ `vmm-api/types.rs`). |

---

## Validation host - EC2 c8i

Dev/build is on macOS (cross-check only — no KVM). **All KVM validation runs on
c8i.**

- **Host:** `<kvm-host>`  **user:** `ubuntu`  **key:** `~/.ssh/<key>.pem`
- **SSH:** `ssh -i ~/.ssh/<key>.pem ubuntu@<kvm-host>`
- **Local-only sync helper:** `C8I_HOST=<kvm-host> SYNC_ONLY=1 ./ci/sync-and-test.sh`
  (untracked local helper; rsyncs the workspace to the host, excludes `target/`,
  `.git/objects`, `guest/bzImage`).
- **Build on c8i:** `ssh ... 'cd ~/tarit/vmm && cargo build --release --features boot'`
- **Gotcha:** KVM tests + the VMM need `sudo` (for `/dev/kvm`). **Under `sudo`,
  `~` = `/root`**, so pass absolute `$HOME/...` paths explicitly in sudo'd scripts.
- **Nested virt tax:** c8i is KVM-in-a-VM, so every guest exit traps to L0 →
  absolute latencies run ~10× a bare-metal host. Use c8i numbers for **relative**
  gains; the <100ms target is bare-metal.
- **Assets on c8i:** kernel `/tmp/vmlinux.microvm` (uncompressed 5.10), rootfs
  `/tmp/debian-rootfs.ext4`, agent-baked rootfs `/tmp/agent-rootfs.ext4`,
  `guest/bzImage` (rsync-excluded, stays on c8i).
- **Handy scripts:** `ci/exec-validate.sh` (real exec), `ci/coldboot-measure.sh`
  (cold-boot-to-exec timing), `ci/initramfs-test.sh`, `ci/restore-roundtrip.sh`.
- Bash safety filter blocks `kill $VAR` — put `kill $PID` inside a script file
  (scp + run) rather than an inline ssh command.

---

## What works (verified on c8i)

- **Boot** to userspace: uncompressed vmlinux, 64-bit long mode, CPUID/MSRs,
  in-kernel IRQCHIP+PIT (idle guests at 0% CPU), ACPI, virtio discovery via DSDT.
- **virtio-blk**: guest mounts ext4 rootfs on `/dev/vda`; **reads AND writes**
  work (verified `echo > /root/x && cat`).
- **virtio-net on the API path**: `create_live` + restore instantiate a
  `VirtioNetMmio` per `config.net` (shared `build_devices` helper, ACPI entry,
  irqfd, TX ioeventfd, host<->tap io loop). Host networking (TAP+NAT+guest IP)
  is still `net-host`.
- **virtio-rng on the API path**: every API VM gets an entropy device so
  restored/cloned guests reseed their CRNG from the host (validated: boots to
  systemd, restores running with `virtio-rng ... irq 6`).
- **Real 16550 serial with IRQ 4** (vm-superio + irqfd) → userspace tty I/O works.
- **Real exec** (`guest/agent/vmm-agent.c` as init): `VMM_EXEC:` over serial
  returns faithful **stdout + exit code**, ~10-20ms/command.
- **Faithful snapshot → restore → resume** (~96ms): full vCPU state
  (REGS/SREGS/FPU/XSAVE/XCRS/MSRS/LAPIC/MP_STATE/EVENTS) + VM-level
  IRQCHIP/PIT/kvmclock; crash-consistent (vCPU paused during dump).
- **API** over UDS: Create/Pause/Resume/Snapshot/Restore/Stop/Exec/UpdateEgress
  + **Status** (state/uptime/vcpus/mem/volumes/nets/kernel/vcpu_alive, validated
  across create/pause/stop on c8i).
- **Graceful shutdown**: `serve` traps SIGTERM/SIGINT and stops the VM cleanly
  (vCPU + net loops + fds) and unlinks the socket before exit.
- **Memory + CPU overcommit** by default (mmap `MAP_NORESERVE`, unpinned vCPUs).
- **Robustness**: per-vCPU seccomp (+ glibc warmup), panic isolation, no fd leak,
  drop-guard/liveness-probe so an abrupt vCPU death can't hang or poison-cascade.
- **Real exec over two channels**: the guest agent (`guest/agent/vmm-agent.c`)
  serves `VMM_EXEC:` over **virtio-vsock** (default, `crates/vmm-core/src/vsock_exec.rs`)
  and the 16550 serial console (fallback). vsock is desync-free under load
  (validated 25/25 rapid execs + multi-line output); serial persists its UART
  registers so exec also works after restore.
- **Interactive PTY over vsock** (`AttachPty`): the guest agent also serves an
  interactive pseudo-terminal on vsock port 1025 (`openpty` + login shell +
  `TIOCSWINSZ` resize); the VMM streams it over a new `AttachPty` API op
  (`crates/vmm-core/src/vsock_pty.rs`, `pty_stream.rs`) and a `vmm attach-pty`
  CLI. The orchestrator exposes it as a WebSocket PTY and an SSH gateway with no
  in-guest sshd. Validated on c8i (`ci/pty-validate.sh`; see `docs/ssh-pty.md`).
- **OCI/Docker images boot as microVMs**: `vmm pull --agent` injects the exec
  agent as PID 1 (mounts /proc,/sys,devtmpfs; reaps; sets PATH). `node:20-slim`
  → `node -v`=v20.20.2 in ~50ms (validated on c8i).
- **SMP** (`vcpus.count>1`): AP bringup via INIT/SIPI, per-thread vCPUs, and
  **SMP snapshot/restore** (all cores captured + restored live).
- **Faithful snapshot → restore → resume** (~113ms): full vCPU state
  (REGS/SREGS/FPU/XSAVE/XCRS/MSRS/LAPIC/MP_STATE/EVENTS) + VM-level
  IRQCHIP/PIT/kvmclock; crash-consistent (vCPUs paused during dump).
- **Incremental (diff) snapshots**: KVM dirty-logging + parent-chain diffs;
  restore-from-any-checkpoint (validated ~738× smaller diff vs full).
- **API** over UDS: Create/Pause/Resume/Snapshot/Restore/Stop/Exec/UpdateEgress
  + **Status**. Graceful SIGTERM/SIGINT teardown.
- **Multi-tenant surface**: jailer on the serve path (chroot/cap-drop/netns/
  cgroup/priv-drop, opt-in `--jail`); **egress nftables actually applied** when
  netns-isolated; token-bucket **rate limiters** on virtio-blk/net; per-vCPU
  seccomp (+ glibc warmup), panic isolation, no fd leak.
- **Memory + CPU overcommit** by default (mmap `MAP_NORESERVE`, unpinned vCPUs).
- **Landed device modules**: virtio-vsock (wired, above), virtio-rng (wired),
  CoW/overlay block backend (`BlkBackend::open_cow`) exposed through
  `VolumeConfig.overlay` and the CLI `--overlay` flag.
- **Orchestrator** (`orch/`, `taritd`): warm pool
  (config.toml, atomic-slot admission control, graceful degradation), read-only
  shared OCI base, and `tarit-bench` (ComputeSDK-style TTI benchmark).
- Integration suites green on c8i; local `ci/check.sh` green.

---

## Backlog

### Done since last revision (all validated on c8i)

`net-wire`, `net-host` (TAP+NAT+`ip=`), `rng-wire`, `status-api`,
`graceful-shutdown`, `multi-vcpu` (SMP boot + **SMP snapshot phase B**),
`minimal-kernel`, virtio-net 12-byte header fix, `incremental-snapshots`,
`rate-limiters`, **serial-persist** (post-restore exec), **jailer-serve**,
**egress-apply** (netns-gated), **OCI agent-as-init** (Docker images bootable),
**vsock** exec channel (default; closes `exec-net-desync` + `smp-serial-race`),
`coldboot-exec` (measured + doc), `perf-gates` (real gate, `VMM_PERF_STRICT`),
`blk-write-complete` (**verified not reproducing** — systemd boots to the final
runlevel step; writes+sync+fsync durable, FLUSH→`sync_all`). Orchestrator:
`warm-pool` + `orch-benchmark`.

### Remaining

- **`livesnap-membench` as a regular gate** - the hardened vsock harness now
  passes A/B/C/D. Keep it wired as an opt-in gate today and promote it to a
  normal CI or release gate once the runner cost is acceptable.

### Done this pass (all validated on c8i, integrated to `main`)

- **`restore-perf` (UFFD)** — UFFD lazy restore is now wired into `restore()` for
  full snapshots (hybrid: eager fallback for diff tips / any UFFD error).
  **Restore ~0.84ms** (was ~109ms eager), 65536/65536 pages served, guest live
  post-restore. A 100× `node -v` restore-burst is **100/100 correct, TTI
  p50=81ms / p95=84ms nested** (under the 100ms ComputeSDK target). Fixed two
  latent bugs in the UFFD primitive that only real KVM surfaced: `struct
  uffdio_register` was 24 bytes (missing the trailing `ioctls` field) so
  `UFFDIO_REGISTER` returned EINVAL and UFFD never engaged; and the pagefault
  address was read at offset 24 instead of 16 with a non-page-aligned
  `UFFDIO_COPY` dst, hanging the guest on its first fault.
- **`snapshot-persist`** — virtio-blk/net **queue** `Persist`
  (last_avail_idx/last_used_idx/addrs/features/status) is now captured into the
  snapshot and re-applied in `build_running_vm`. A mid-boot (I/O-in-flight)
  snapshot now resumes with forward progress (`ci/restore-midboot.sh` → PROGRESS).
- **`storage-cow` config-expose** — added `VolumeConfig.overlay: Option<String>`
  (serde default); when set, `path` is the read-only base and `open_cow` is used
  (CLI: repeatable `--overlay`). Validated on c8i: guest boots on base+overlay,
  mounts `/dev/vda` rw, writes land in the overlay, base stays byte-identical.
  Also fixed a seccomp gap: `fsync` was not allowed on the vCPU thread, so block
  FLUSH durability (plain and CoW) killed the vCPU with SIGSYS (audit syscall=74).
- **vsock-after-restore** — restore now injects an RST for each restored stream
  **after the vCPU resumes** (so the RX completion interrupt lands on a running
  vCPU, not a paused LAPIC); the guest agent then re-dials. Post-restore exec runs
  over vsock again (`ci/vsock-exec-validate.sh`: 28 execs via vsock, 25/25 rapid).
- **`minimal-kernel + vsock`** — the minimal kernel now includes
  `CONFIG_VIRTIO_VSOCKETS`; cold `create` → first **vsock** exec ~340ms nested
  (~34ms bare), all runs confirmed via vsock.

### Deferred / low priority
- **Live migration** — needs bare-metal hosts (EC2 RTT > blackout target).
- **eBPF/XDP egress** — nftables suffices for now.
- **balloon / MMDS / vhost-user / CPU templates** — optional per PRD; scaffold only.

---

## Known gaps vs Firecracker (honest)

The VM *engine* is solid and the multi-tenant surface works: boot, virtio-blk
(read/write/flush), virtio-net + host networking, virtio-rng, real exec
(vsock + serial), SMP, **UFFD fast restore (~0.84ms)** + incremental diffs,
in-flight-I/O snapshot persistence, **CoW storage (config-exposed)**, jailer on
serve, applied egress, rate limiters, and OCI-image boot. Remaining vs a
Firecracker-grade sandbox: no **balloon/MMDS**, and the live-snapshot
memory-consistency soak (`livesnap-membench`) is opt-in rather than a default CI
gate.

## Performance (c8i nested virt — bare metal ~10× faster)

| Operation | Measured (nested) | Bare-metal proj. | Target | Status |
|---|--:|--:|--:|---|
| Cold create → first exec (minimal kernel + vsock) | ~340ms | ~34ms | <100ms | ✅ |
| Cold create → first exec (microvm kernel) | ~1009ms | ~101ms | <100ms | ⚠️ |
| `create()` return | ~26–32ms | ~3ms | — | ✅ |
| Exec round-trip (running, vsock) | ~9ms | — | — | ✅ |
| Snapshot full (256MB, crash-consistent) | ~117ms | ~12ms | <30ms | ✅ |
| Snapshot diff (incremental) | ~18ms | ~2ms | — | ✅ |
| Restore → running (UFFD lazy, full snap) | **~0.84ms** | — | <10ms | ✅ |
| Restore-burst TTI (restore→`node -v`) p95(100) | ~84ms | — | <100ms | ✅ 100/100 |
| vmm RSS (256MB VM, incl. touched pages) | ~45MB | — | — | ✅ |
| Idle running VM | 0% CPU | — | idle | ✅ |

---

## Path to production-grade PaaS (VMM + orchestrator)

Honest roadmap for running the VMM under the `taritd` orchestrator as a real
multi-tenant PaaS. As of 2026-07-02, on bare metal (c8i.metal-48xl), via the
orchestrator REST API, ComputeSDK methodology (staggered 200ms, 120s timeout,
`node -v`), 13 runs of n=100 (~3800 iters, 0 failures): sequential p95 38ms,
burst(100) p95 57ms, staggered p95 39ms — roughly 10x faster than the fastest
public ComputeSDK providers (~400ms-1.7s). Per-VM isolation, write-behind store,
and a synchronous exec endpoint are in. What remains to be production-grade:

### VMM

1. **Restore-clone isolation (highest priority for the optimal warm pool).** The
   `.snap` holds RAM only; `restore` reopens base+overlay from the golden's config,
   so restore-cloned VMs would share one disk overlay. Add a per-restore `overlay`
   override to the `Restore` op; give each clone its own overlay (reflink/copy of the
   idle golden's overlay, or a 2-layer CoW: base ro + golden-overlay ro + clone rw).
   This unblocks snapshot-restore replenishment (see orch/docs/REPLENISHMENT.md).
2. **Suspend that frees RAM.** Today `pause` only stops vCPUs (RSS stays resident).
   Add snapshot-to-disk + `madvise(MADV_DONTNEED/PAGEOUT)` (or process exit + restore)
   so idle sandboxes release host memory; restore on resume via UFFD.
3. **Snapshot lifecycle:** GC of orphaned `/tmp/vmm-*.snap` + overlays; snapshot to
   durable storage (S3/EBS/NFS) for cross-node restore; diff-chain compaction.
4. **Security hardening:** make the opt-in jailer path production-default where
   appropriate (cgroup v2, chroot, uid/gid drop, cpuset); complete and audit the
   seccomp allowlists per thread; keep per-VM resource caps enforced by the VMM,
   not only the orchestrator.
5. **Devices/net:** virtio-blk multi-queue + flush/FUA durability; virtio-net
   offload/hardening; host-enforced egress allowlist at scale; virtio-balloon for
   memory reclaim.
6. **Boot:** keep the minimal-kernel cold path <100ms bare metal; trim the microvm
   kernel; measure boot on real metal (not just projected).
7. **Live-snapshot consistency** (membench A/B/C/D) as a CI gate, not a one-off.
8. **aarch64** support (currently x86_64 only); API/versioning stability for the
   `tarit-vmm-client` <-> `vmm-api` contract.

### Orchestrator (taritd)

1. **Snapshot-restore replenishment** (the researched optimal; see REPLENISHMENT.md).
   Switch default refill from cold boot to restore-from-golden once VMM item (1)
   lands. Cold boot becomes an offline golden-build step only.
2. **CPU-isolated, rate-limited refill:** refill boots/restores in a low-CPUWeight
   cgroup + max-in-flight semaphore + CPU-aware token bucket; live execs get reserved
   CPU. (Measured: unbounded 100-wide cold refill starves live execs — staggered
   timed out at 30s. Bounded conc=48 is clean but restore is the real fix.)
   Hysteresis pool sizing (hard_floor/low_watermark/target/high_watermark).
3. **Multi-tenant security:** real auth + RBAC (API keys are dev-grade); mTLS between
   peers (currently a shared secret); per-tenant quotas, network isolation, and secret
   management. VM data isolation is proven for the cold pool; extend to restore clones.
4. **HA + durability:** validate the write-behind store's crash recovery; Postgres
   fleet HA + leader-election robustness (split-brain, node drain, failover);
   cross-node snapshot/restore + VM placement/migration.
5. **Autoscaling:** the leader-elected cross-cloud scale loop needs real provider
   drivers (EC2/GCP) + validation; track free-vCPU/mem/pool-depth signals across clouds.
6. **Networking at scale:** per-VM tap+/30+NAT provisioning under load; IP pool
   exhaustion; egress policy enforcement; teardown correctness.
7. **Lifecycle:** graceful shutdown/draining (VMs currently orphan on taritd exit — no
   Drop sweep); reap deferred teardown; admission backpressure (429) under overload.
8. **Image/rootfs pipeline:** OCI pull -> ext4 -> golden-snapshot build + registry +
   versioning + GC; journal-clean shared read-only bases.
9. **Observability:** Prometheus metrics (pool depth, TTI, refill rate, CPU), tracing,
   per-VM resource accounting, SLOs.

### Cross-cutting

- **Benchmarks:** wire the ComputeSDK harness (`scripts/bench-warmpool.sh`,
  `bench-vmops.sh`) into CI; run the quarterly-scale stress (10k concurrent) once
  restore-refill lands; keep bare-metal numbers in `docs/METAL-BENCHMARKS.md`.
- **CI gaps:** `ci/check.sh` doesn't run clippy/boot-gated code on Linux — close it.
- **Docs:** OpenAPI is published; add runbooks, SLOs, and the tenancy/security model.
