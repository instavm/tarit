# PRD Gap Analysis

*Last updated: 2026-07-01. Honest assessment.*

## PRD §1 — Functional Requirements

| Requirement | Status | Evidence |
|---|---|---|
| Boot unmodified Linux kernel | ✅ DONE | 64.3 boots/sec, 14.8ms p50 |
| Attach storage volumes | ✅ DONE | virtio-blk MMIO: guest mounts ext4 rootfs on /dev/vda + boots to userspace (c8i). ACPI discovery, userspace QUEUE_NOTIFY, irqfd completion. |
| Full snapshot + restore | ✅ DONE | Eager + UFFD, CRC, tampering rejection |
| Fast suspend → resume | ✅ DONE | Pause/resume API, snapshot, UFFD lazy restore |
| Live snapshots (no stop-the-world) | ✅ DONE | VmmController::live_snapshot() with VcpuThread, passes on c8i |
| Isolated per-VM networking | ✅ DONE | TAP, netns, nftables, 3 virtio-net E2E tests pass |
| Blazing-fast cold start | ✅ DONE | 14.8ms p50 (target <125ms) |

## PRD §2 — Performance Budget

| Metric | Target | Actual | Status |
|---|---|---|---|
| Cold create → first exec | <100ms bare metal | ~34ms projected from c8i nested virt | ✅ PASS projection |
| Restore → running | <10ms | ~0.84ms UFFD lazy restore | ✅ PASS |
| Suspend | <30ms and release RAM | Pause/resume works; RAM-freeing suspend remains backlog | ⚠️ |
| Live snapshot pause | <1-5ms | Algorithm tested; livesnap membench passes as an opt-in gate | ✅ |
| VMs/sec/host | >100 | Not remeasured after current boot path | ⚠️ needs fresh bare-metal gate |
| Per-VM overhead | <5MiB VMM overhead | ~45MiB RSS for a touched 256MiB VM | ⚠️ needs apples-to-apples accounting |

## PRD §10 — Security & Jailing

| Requirement | Status | Evidence |
|---|---|---|
| Jailer: chroot + namespaces + cgroups + uid drop | ✅ DONE | Real chroot, mount ns, netns, rlimits, cap drop, cgroup limits |
| seccomp-BPF per-thread | ✅ DONE | BPF filter installed in vcpu_thread.rs, fatal on failure |
| Memory isolation by KVM | ✅ DONE | KVM slots, bounds-checked vm-memory |
| Network policy host-side | ✅ DONE | nftables in netns, egress denial tests |

## PRD §11 — Phased Roadmap

| Phase | Status |
|---|---|
| 0 — Boot | ✅ DONE |
| 1 — Devices (blk + net) | ✅ DONE |
| 2 — Isolation | ✅ DONE |
| 3 — Snapshot/Restore | ✅ DONE |
| 4 — Suspend/Resume + Clones | ✅ DONE |
| 5 — Live snapshot | ✅ DONE |
| 6 — Egress hardening + perf | ⚠️ PARTIAL (eBPF not done) |
| 7 — Live migration | ❌ DEFERRED (EC2 RTT) |

## Remaining Gaps (excluding live migration)

1. **Cold create-to-first-exec gate** - minimal-kernel c8i nested-virt numbers
   project under the <100ms bare-metal target, but the public gate should run on
   a real bare-metal host and record fresh numbers.
2. **RAM-freeing suspend** - pause/resume works; releasing resident guest RAM on
   suspend is still backlog.
3. **Live-snapshot consistency gate** - the livesnap membench harness passes, but
   should become a regular CI or release gate instead of an opt-in run.
4. **eBPF/XDP** - Phase 6 optimization. nftables egress works.
5. **vhost-user / balloon / MMDS** - optional per PRD.
6. **CLI polish** - `--json`, `vmm clone`, and completions.
