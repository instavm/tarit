# Host Orchestrator & PaaS Control Plane: Implementation PRD, Architecture & Testing Plan

<aside>
🎯

The companion to the **rust-vmm microVM PRD**: a code-grounded blueprint for the layer *above* the VMM — a single **host orchestrator binary** (`taritd`) that turns a fleet of KVM hosts into a blazing-fast, cloud-agnostic PaaS. It terminates the public API, authenticates callers, schedules and reconciles microVMs, brokers SSH/exec into guests **without an SSH daemon in the VMM**, gossips fleet state, monitors health, autoscales, and orchestrates deploys and host drains via snapshot-relocation (live migration is explicitly out of scope — cloud inter-host latency makes it infeasible). Designed to run on nested-virt instances (EC2 C8i/M8i/R8i, GCP, Azure) today and bare metal later.

</aside>

## 0. Implementation PRD — overview

**Document type:** Implementation PRD (engineering + product requirements) for the Tarit host orchestrator and PaaS control plane. It assumes the microVM substrate from the VMM PRD (boot, volumes, snapshot/restore/clone, live snapshot, host-enforced egress) already exists, and specifies everything needed to operate that substrate as a multi-tenant platform.

**Problem / motivation:** A VMM that can boot, snapshot, restore, and clone a microVM is necessary but not sufficient for a PaaS. To run *hundreds of thousands* of sandboxes across many hosts and several clouds, we need an orchestration layer that handles the external API, identity, placement, fleet state, request routing, health, scaling, snapshot-based relocation, and deploys — the "rest of the owl" that [fly.io](http://fly.io) describes as the gap between *running one VM* and *running a platform*.[[1]](https://fly.io/blog/carving-the-scheduler-out-of-our-orchestrator/)

**Core architectural bet (read this first):** we copy [fly.io](http://fly.io)'s **shared-nothing, no-central-queue** model. There is **no global work queue on the hot path**. The control plane writes *desired state*; an eventually-consistent gossip layer (CRDT-over-SQLite, à la fly's **Corrosion**) replicates it to every host in ~1s p99; and a per-host **reconciler state machine** in `taritd` converges actual→desired locally with a durable embedded queue. This avoids the consensus/queue bottleneck that breaks down at fleet scale.[[2]](https://fly.io/blog/corrosion/)[[3]](https://fly.io/infra-log/2024-11-30/)

**In scope (v1):**

- One orchestrator binary per host (`taritd`) — agent, supervisor, reconciler, relocation driver, host safety governor.
- A stateless control-plane API server (`tarit-api`) — the PaaS front door + auth.
- Fleet state / service discovery via gossip CRDT SQLite (`mesh`).
- A data-plane request router (`tarit-proxy`) with default-deny ingress.
- Guest agent (`pilot`) for exec/SSH-without-sshd over vsock.
- Health/heartbeat, self-healing, metrics/logs/traces pipeline.
- Scheduling/bin-packing + autoscale + scale-to-zero + warm pools.
- Orchestrated deploys and host drains via snapshot-relocation (suspend → snapshot → restore), not live migration.
- Cloud-agnostic host bootstrap on nested-virt instances.
- Per-API-key metering (sandbox wall-time), VM-start rate limits, and global concurrent vCPU/memory quotas.

**Out of scope (v1):** live migration (pre-copy/post-copy across hosts) — infeasible on cloud inter-host links (see §9), likely deferred indefinitely; managed databases / add-on marketplace, multi-region global Anycast/BGP (single-region first; design for it), production multi-cloud parity, customer-facing billing UI, Kubernetes compatibility, complex service mesh, privileged containers, GPU scheduling, and a polished web dashboard (CLI + API first).

**Objectives & key results:**

- **O1 — Fast control plane:** API→sandbox-running p50 < 300 ms from a warm pool (cold image pull excluded); scheduling decision < 10 ms.
- **O2 — Fast, consistent fleet state:** machine state change visible fleet-wide p99 < 1 s via gossip.
- **O3 — Zero-downtime deploys without live migration:** replicated apps deploy via blue/green with no dropped connections; stateful single-instance sandboxes move via suspend→snapshot→restore relocation with a bounded pause (target < 2 s).
- **O4 — Self-healing:** detect a dead host and reschedule its reschedulable workloads in < 30 s.
- **O5 — Secure multi-tenancy:** default-deny ingress + per-org auth on exec; no SSH daemon anywhere in the guest or VMM.
- **O6 — Cloud-agnostic:** identical `taritd` image boots and joins the mesh on EC2, GCP, and Azure nested-virt instances with only a per-cloud provisioning shim.

**Stakeholders / owners (assign):** Orchestrator core (eng owner), Control-plane/API + auth, Networking/proxy, SRE/observability, Infra/provisioning (multi-cloud), Product (sandbox lifecycle API + CLI).

**Key dependencies & assumptions:** the VMM exposes a local control socket (UDS) with create/start/stop/pause/resume/snapshot/restore/exec/update-egress/list/security verbs and a `Persist`-versioned state schema; nested-virt KVM hosts; a container/OCI registry for images; an object store for snapshots and logs.

### 0a. Current VMM substrate status (post-PRD-gap-fixes)

Use the VMM as a fast substrate, but keep the orchestrator honest about what is production-ready today. Current status: **244+ tests passing on c8i nested virt** plus a **3-hour stress test: 505 cycles, 4,545 total checks, 0 failures, stable 15–24 ms boot p50, and no RSS growth**.

**Works E2E on c8i nested virt:**

- **Boot:** bzImage boot works; cold boot to HLT is **14–16 ms p50 / 61 ms p99** for a 256 MiB guest; full boot to `exec echo` is **16–24 ms p50 / 23 ms p99**. Fast boot remains ~6 ms; full boot has IRQCHIP/PIT.
- **Rootfs attach plumbing:** pre-existing ext4/raw rootfs can be attached via `--rootfs <path>` as `/dev/vda`; virtio-mmio register layout, kernel cmdline advertisement, irqfd/GSI routing, ioeventfd queue kicks, and IRQCHIP behavior for volumes are fixed.
- **OCI conversion:** `vmm pull <oci-ref> --output <path> --size <MiB>` exists; OCI pull + unpack + ext4 conversion works with skopeo+umoci on c8i.
- **UDS API:** create, stop, pause/resume, snapshot, restore, exec, live egress update, list, and security introspection.
- **CLI:** `vmm run`, `restore`, `serve`, `snapshot`, `list`, `exec`, `pull`, aliases, verbosity flags, and jail flags are present. CLI polish remains (`--json`, `vmm clone`, completions, config file).
- **Snapshot/restore:** full snapshot is ~73 ms p50 / 276 ms p99 for 256 MiB on nested virt; eager restore is ~85–99 ms on nested virt; UFFD lazy restore is <10 ms on bare metal when enabled. Snapshot `mem_len` validation rejects zero, non-page-aligned, and >64 GiB images; tampered magic is rejected.
- **Live snapshot:** now wired through `VmmController::live_snapshot()` and covered by page-alignment/non-empty/decision-logic tests. Use as an optimization path; keep full snapshot as the safe fallback for v1.
- **Security:** security audit complete; UFFD ioctl constants/source-page math, virtqueue bounds checks, descriptor caps, descriptor index validation, LazyRestore lifetime, snapshot validation, capability dropping, uid/gid rejection, hard-fail jail behavior, seccomp installation, and jail wiring are fixed.
- **Jailer:** real chroot, mount namespace, netns, rlimits, cgroup limits, seccomp install, capability dropping, and `--jail` wiring for `run`/`restore`.
- **Networking:** TAP creation, deny-all nftables egress, live egress update, DNAT port forwarding, DNS-aware egress allowlisting, token-bucket rate limiting, and per-VM netns scaffold. eBPF/XDP is a later high-throughput optimization, not a v1 blocker.
- **Storage:** virtio-blk has real pread/pwrite, queue walking, MMIO handling, read/write/flush/get-id support, E2E tests for read/write/flush/RO/OOB/OOB-guest/oversized descriptor behavior, and fuzz/property tests over descriptor chains.
- **CoW overlays:** `create_cow_overlay()` exists using `copy_file_range` / reflink semantics where supported and is tested for isolation.
- **Perf gates / soak:** comprehensive tests cover boot/snapshot/restore latency, creation rate, memory overhead, seccomp, jailer, CoW, snapshot tamper, and soak automation.

**Still not production-ready / orchestrator must not depend on it:**

- **Rootfs boot to login prompt on nested virt:** rootfs is attached and VM runs, but on c8i L0 coalesces MMIO exits and the guest virtio-blk driver does not reach the VMM. Treat full OCI/rootfs guest boot as **bare-metal-required** for now. For nested-virt cloud v1, the orchestrator must gate OCI/rootfs workloads behind a bare-metal validation pool or use the proven init/exec path until rootfs boot is proven on the target host class.
- **Live migration:** deferred; EC2 inter-host latency (~5 ms RTT) exceeds the <5 ms blackout target. Still out of scope for this PaaS design until bare-metal hosts with <1 ms RTT exist.
- **eBPF/XDP fast path:** nftables works and remains v1; eBPF/XDP is a Phase 6 optimization.
- **vhost-user backend:** optional higher-throughput storage backend, not v1.
- **Volume hotplug:** low priority; not critical for v1.
- **Serial output on nested virt:** not reliable because L0 coalesces PIO exits; orchestrator logging/debugging should prefer memory channel, guest agent, virtio-console later, or bare metal.

---

## 1. Design goals & non-goals

**Functional requirements (from the brief)**

- Terminate the **external API**, authenticate and authorize every request.
- **SSH / exec into a running sandbox like [fly.io](http://fly.io) / Kata** — but with **no SSH daemon inside the VMM**; brokered through a guest agent over a vsock control channel.
- **Monitor health** of hosts and sandboxes; self-heal.
- **Join clusters** of other hosts; share state; discover services.
- Drive **snapshot-relocation** (suspend → snapshot → restore) for host drains; use blue/green or rolling recreate for deploys. No live migration in v1.
- **Workers receive work from an API server** — define exactly where work originates and where any queue lives.
- **Scale up** (and down/to-zero) automatically.
- **Cloud-agnostic**, starting on **nested-virt** instances across EC2/GCP/Azure.

**Design principles**

- **Shared-nothing, gossip-replicated state; no central queue on the hot path.** Desired state in, reconcilers converge locally.[[4]](https://fly.io/docs/blueprints/shared-nothing/)
- **The host owns the boundary.** Auth, exec brokering, ingress/egress policy, and resource limits live in `taritd`/host, never the guest — consistent with the VMM PRD.
- **Eventually consistent over strongly consistent.** Use CRDTs + SWIM gossip; reserve a tiny strongly-consistent store only for things that truly need it (unique name allocation, IP assignment).[[5]](https://github.com/superfly/corrosion)
- **One binary, many roles.** `taritd` is a single static binary; its role (worker / seed / proxy) is configuration, simplifying multi-cloud deploys.
- **Reuse the VMM's snapshot/restore machinery.** Deploys, scaling, drains, and self-healing are all expressed as snapshot/restore/clone operations the VMM already supports — *not* live migration.
- **Reconcile, don't command.** Every actor loops toward desired state and is safe to crash/restart at any point.

**Non-goals (v1):** global Anycast/BGP edge, Raft/Paxos for machine state (explicitly rejected — consensus over long distances was fly's mistake to undo), Kubernetes compatibility, live migration (pre-copy/post-copy across hosts — infeasible on cloud inter-host links; see §9), managed databases, GPU, a full billing UI, complex service mesh, privileged containers, and a general add-on ecosystem.

### 1a. V1 critical path — keep this brutally small

The architecture is intentionally broad, but **v1 should optimize for extremely fast ephemeral sandboxes, strong host safety, and basic PaaS UX** — not global edge or every enterprise feature.

**Build first:**

1. Single-host `taritd` that supervises the current VMM UDS API (create/stop/pause/resume/snapshot/restore/exec/update-egress/list/security).
2. API key auth + idempotent operation model.
3. Image → rootfs pipeline and local cache. `vmm pull` conversion is available; **rootfs boot-to-login is bare-metal-gated** and must not block nested-virt orchestrator work that uses the proven init/exec path.
4. Start/stop/delete sandbox.
5. Basic scheduler and desired-state reconciliation.
6. Per-key VM-start rate limits + concurrent vCPU/RAM quotas.
7. Logs/events/inspect + exec via vsock `pilot`.
8. Host health, local admission, cgroup/cpuset resource fairness, backpressure, and quarantine.
9. Basic `tarit-proxy` routing.
10. Snapshot/restore-based scale-to-zero using full snapshot/restore as the safe path; optionally test live snapshot as an optimization. Live migration remains deferred.

**Defer until the core loop is reliable:** production multi-cloud parity, global Anycast, advanced CRDT regionalization, sophisticated autoscaling, customer-facing billing, complex secret rotation, storage migration for all cases, Kubernetes compatibility, live migration, and managed services.

---

## 2. System topology

```
                 ┌──────────────────────────────────────────────────────┐
Users / CLI ───▶ │  tarit-api  (stateless control plane, N replicas)      │
(REST/gRPC)      │   • authn/authz (org tokens, mTLS, SSO)                │
                 │   • validates + writes DESIRED STATE into the mesh     │
                 │   • allocates unique names / IPs (small CP store)      │
                 └───────────────┬──────────────────────────────────────┘
                                 │ writes desired state
                                 ▼
           ┌─────────────────────────────────────────────────────┐
           │  MESH: gossip CRDT-over-SQLite on every node          │
           │  (machine state, service discovery, host health)      │
           │  SWIM membership • CRDT conflict resolution • QUIC     │
           └───────┬───────────────────────────────┬──────────────┘
      replicates   │            replicates          │   replicates
                   ▼                                ▼
┌──────────────────────────────┐      ┌──────────────────────────────┐
│  WORKER HOST A                │      │  WORKER HOST B                │
│  ┌────────────────────────┐  │      │  ┌────────────────────────┐  │
│  │ taritd (orchestrator)   │  │ relo │  │ taritd                  │  │
│  │  • reconciler loop      │◀─┼─copy─┼─▶│  • reconciler loop      │  │
│  │  • durable local queue  │  │      │  │  • durable local queue  │  │
│  │  • VMM supervisor       │  │      │  │  • VMM supervisor       │  │
│  │  • health/heartbeat     │  │      │  │  • exec broker          │  │
│  └─────┬──────────────────┘  │      │  └────────────────────────┘  │
│   UDS  │   vsock              │      │                               │
│  ┌─────▼─────┐  ┌──────────┐  │      │  ┌──────────┐  ┌──────────┐  │
│  │ VMM proc  │  │ VMM proc │  │      │  │ VMM proc │  │ VMM proc │  │
│  │ +pilot    │  │ +pilot   │  │      │  │ +pilot   │  │ +pilot   │  │
│  └───────────┘  └──────────┘  │      │  └───────────┘  └──────────┘  │
│  tarit-proxy (data plane)     │      │  tarit-proxy (data plane)     │
└──────────────────────────────┘      └──────────────────────────────┘
         ▲                                       ▲
         └────────── client traffic (ingress, default-deny) ──────────┘
```

**Three planes, clearly separated:**

- **Control plane** (`tarit-api`): stateless, horizontally scalable, the only thing users talk to. It does *not* place work directly on hosts — it records intent.
- **State plane** (`mesh`): the gossip CRDT SQLite layer that every node runs; the single source of truth for "what should exist and where it is."
- **Data plane** (`tarit-proxy` + `taritd` + VMM + `pilot`): runs on every worker, executes the actual sandboxes and serves their traffic.

---

## 3. The orchestrator binary (`taritd`)

One `taritd` per host, analogous to fly's **flyd**.[[6]](https://www.youtube.com/watch?v=kPHmRgDTvNc) Responsibilities:

| Subsystem | Responsibility |
| --- | --- |
| Reconciler | Watch desired state for this host in the mesh; drive each sandbox through its lifecycle state machine until actual == desired. |
| VMM supervisor | Spawn/own one VMM process per sandbox over a local UDS; translate lifecycle verbs into VMM calls (create, start, stop, snapshot, restore, clone); reap on exit. |
| Durable local queue | Embedded crash-safe store (e.g. an embedded KV / WAL like Bolt/redb/SQLite) holding in-flight operations as resumable state machines. Survives `taritd` restarts. |
| Exec broker | Terminate authenticated exec/SSH sessions and relay them to the guest `pilot` over vsock (Section 7). |
| Health/heartbeat | Emit host + sandbox health into the mesh; run liveness/readiness probes; trigger self-heal. |
| Relocation driver | Source/destination half of snapshot-relocation (suspend → snapshot → copy → restore) for drains and deploys (Section 9). No live migration. |
| Resource governor | Track CPU/RAM/disk/IP budgets per host; create per-sandbox/per-key/per-org cgroups; enforce CPU weights/caps, memory hard limits, IO/network throttles, cpuset pinning for dedicated cores; advertise free capacity and pressure signals for scheduling. |
| Image/volume manager | Pull OCI images, build/cache ext4 rootfs + CoW overlays, manage snapshot artifacts (local NVMe + object store). |
| Local control socket | Authenticated host-local API for the proxy and for break-glass operators. |

<aside>
🧩

The key property: `taritd` is a **convergent state machine**, not a command executor. The API never "tells host A to start VM X." It writes "VM X should run, placement=host A" to the mesh; host A's reconciler notices and acts. This is what makes the platform self-healing and crash-tolerant — every operation is idempotent and resumable.

</aside>

---

## 4. Control plane: API, auth & lifecycle

**`tarit-api` is stateless** and sits behind a normal load balancer; scale it horizontally. It does five things:

1. **Authenticate** the caller (Section 4a).
2. **Authorize** against the org/project that owns the target resource.
3. **Validate** the request (quotas, image refs, resource shapes).
4. **Allocate** anything globally unique (sandbox name, private IP) from the small strongly-consistent CP store.
5. **Write desired state** into the mesh and return — *without waiting for the work to happen* (async, with a watchable status), or optionally long-poll the mesh until the sandbox reports `started`.

**Sandbox lifecycle states** (mirrors fly Machines): `created → starting → started → stopping → stopped → replacing → relocating → destroying → destroyed`, plus `suspended`. Every transition is recorded in the mesh; the reconciler owns the transitions.[[7]](https://fly.io/docs/machines/machine-states/)

### 4a. Authentication & authorization

- **External:** org-scoped API tokens (macaroon-style, attenuable) and/or OIDC/SSO for humans. Tokens carry caveats (org, project, action, expiry) so they can be safely delegated and narrowed.
- **Internal (node↔node, api↔node):** **mTLS** on every link with a short-lived cert from an internal CA; nodes get certs at bootstrap. The mesh transport (QUIC) is authenticated the same way.[[8]](https://qconlondon.com/presentation/apr2025/fast-eventual-consistency-inside-corrosion-distributed-system-powering-flyio)
- **Exec auth:** SSH access is gated by per-org certificates fetched by the broker, **not** static keys baked into guests — exactly fly's hallpass model (the only interesting thing hallpass does is pull per-org certs).[[9]](https://fly.io/blog/ssh-and-user-mode-ip-wireguard/)

### 4b. Where does the work come from? (the queue question, answered)

> **There is no central job queue on the hot path.** The "queue" is decomposed into three durable places, none of which is a chatty central broker:
> 

| Layer | What lives here | Why |
| --- | --- | --- |
| CP intent store | The desired-state record ("sandbox X should run on host A") | Durable statement of intent; the source of truth for reconcilers. |
| Mesh (gossip CRDT) | Replication of that intent + observed state to every node | Eventually consistent fan-out in ~1s; no single point that can back up. |
| Per-host embedded queue | The in-flight operation as a resumable state machine inside `taritd` | Crash-safe local execution; retries/backoff are local, not global. |

<aside>
⚠️

**Lesson from fly's outages:** a central broker (their Consul) that queues and retries state updates can melt the system when it recovers — flyd's backlog once drove Corrosion to **150 GB/s** of useless gossip and saturated the network.[[10]](https://fly.io/infra-log/2024-10-26/) We keep retry/backlog *local to each host* and make gossip carry compact CRDT deltas, not command streams.

</aside>

**When you DO want a real queue:** asynchronous, bursty, non-placement work — **image builds**, snapshot exports, log/metric rollups — runs on a separate durable task queue (e.g. NATS JetStream / Redis Streams / SQS-compatible) consumed by builder workers. This is deliberately *off* the sandbox-scheduling path so a build backlog can never stall sandbox starts.

### 4c. Metering, quotas & rate limiting (per API key)

Every credential (an API key scoped to an org/project) needs three *different* controls. Don't conflate them — they have different shapes, storage, and enforcement points.

| Mechanism | Question it answers | Shape | Where enforced |
| --- | --- | --- | --- |
| **Metering** | "How much has this key used?" | Cumulative ledger (vCPU-seconds, GiB-seconds, counts) | Async, off the hot path |
| **Rate limit** | "How *often* can this key start VMs?" | Token-bucket over time | Sync, at `tarit-api` admission |
| **Concurrent quota** | "How much vCPU/RAM can this key hold *right now*?" | Reservation / gauge | Sync, at `tarit-api` admission |

**What we meter (usage units):**

- **Sandbox wall-time → vCPU-seconds and GiB-seconds** (RAM×seconds), accrued only while a sandbox is `started`. Paused/stopped sandboxes accrue *no compute* (storage is metered separately) — this is what makes scale-to-zero cheap for the customer.
- **VM-starts** (count) — also the rate-limited action.
- **Exec sessions** and exec-seconds.
- **Egress bytes** (from the host egress enforcer, VMM PRD §8), **volume GiB-hours**, and **snapshot-storage GiB-hours**.

**How metering stays accurate across a distributed fleet:**

- Wall-time is derived from the **lifecycle state transitions already recorded in the mesh** (`started`→`stopped`/`suspended`/`destroyed`). Each `taritd` is the source of truth for *its* sandboxes and emits immutable, idempotent **usage events** — `{key_id, org_id, sandbox_id, vcpus, mem_mib, started_at, ended_at, reason}` — onto the off-path event stream (the Section 4b queue, never the scheduling path).
- A **metering aggregator** consumes the stream into a durable, append-only **usage ledger** (e.g. ClickHouse / Postgres + object store), bucketed by key/org/time. Dedupe on `(sandbox_id, interval_epoch)` so retries never double-count.
- **Relocation & restore correctness:** a suspend-relocation *closes* the usage interval on the source and *opens* a new one on the destination for the same `sandbox_id`; the aggregator stitches them so wall-time is continuous and counted **once**. A scale-to-zero stop/start produces two separate billed intervals with no charge in between.
- **Heartbeat ticks:** long-running sandboxes emit a periodic "still running" usage tick (e.g. every 60 s) so a host crash loses at most one interval; the reconciler closes orphaned intervals from the last known-good timestamp.

**Rate-limiting VM starts (per key):**

- Enforced **synchronously at `tarit-api` admission**, *before* any desired state is written — a rejected start must never touch the mesh or a host. Use a **token-bucket / GCRA** per key (burst + sustained rate), with separate buckets for other expensive verbs (`exec`, `snapshot`, `deploy`).
- Because `tarit-api` is N stateless replicas, the limiter must be **shared**. Two options: (a) a fast central counter (Redis/Memorystore atomic `INCR`+TTL or a GCRA Lua script) for hard global limits; or (b) **local-bucket-with-async-reconciliation** — each replica owns a slice of the budget, periodically rebalanced — when sub-ms latency matters and slight overshoot is tolerable.
- On exceed: return `429 Too Many Requests` with `Retry-After` and standard `RateLimit-*` headers. Never silently drop.

**Global concurrent quotas (vCPU / memory per key):**

- This is a **gauge, not a rate**: "sum of vCPUs and RAM across all *currently-running* sandboxes for this key ≤ quota." Enforce with a **reserve-on-admit / release-on-stop lease**, transacted in the same small strongly-consistent CP store that allocates names/IPs (Section 4):
    1. **On create/start:** atomically `reserved.vcpu += req.vcpu; reserved.mem += req.mem` *iff* the result stays within limits, else reject with `402/429` naming the limiting dimension.
    2. **On stop/destroy/relocate-out:** release the reservation (relocate-in re-reserves on the destination).
- **Reconciliation against ground truth:** a controller periodically recomputes actual usage per key from the mesh (the authoritative running set) and corrects the reserved counters, healing leaks from crashes or missed releases. The CP-store reservation gives **hot-path correctness**; the mesh reconciliation gives **eventual self-correction**.
- Quotas are **multi-dimensional**: concurrent vCPU, concurrent RAM (MiB), max concurrent sandboxes, max volumes / storage GiB, optionally max egress Mbps. A request must fit **every** dimension.

**Quota / plan model:**

- A `plan` defines default limits; an `api_key` (or its owning `org`) references a plan plus optional per-key overrides. Limits resolve `key override → org override → plan default`. Stored in the CP store, cached into the mesh for fast local reads by `tarit-api`.
- **Macaroon caveats** (Section 4a) can carry *attenuated* limits, so a delegated key is automatically more restrictive than its parent — e.g. a CI key minted from an org key capped at 8 vCPU and 20 starts/min.

```
create-sandbox(api_key, vcpu, mem)
  └─▶ tarit-api admission
        1) authn / authz                                   (§4a)
        2) RATE check: token-bucket(api_key,"vm.start")  ──fail─▶ 429 Retry-After
        3) QUOTA check+reserve: reserved+req ≤ limits?   ──fail─▶ 402/429 {dimension}
        4) write desired state → mesh
        5) emit "start" usage event → ledger
   … on stop / destroy / relocate-out:
        release reservation  +  emit "end" usage event → ledger
```

**Why enforce at the API, not the host:** admission control must be *global* — a key's quota spans the whole fleet — so it lives where requests converge (`tarit-api`). The host's `taritd` resource governor (Section 3) is the **second line of defense**: it rejects work that would exceed *host* capacity and enforces per-sandbox cgroup limits, but it does not own per-key global accounting.

<aside>
🧮

**Three numbers, three homes.** *Rate* lives in a fast shared counter (Redis/GCRA). *Concurrent quota* lives as a reservation in the strongly-consistent CP store, reconciled from the mesh. *Cumulative usage* lives in an append-only ledger fed by host-emitted events. Keeping the hot-path checks (rate + quota) out of the eventually-consistent mesh is what keeps admission correct under races; keeping metering off the lifecycle path is what keeps a billing-pipeline outage from ever blocking a sandbox.

</aside>

**Failure modes to handle explicitly:** double-reserve on retried requests (require client **idempotency keys**); reservation leak on host crash (reconcile from mesh); clock skew across hosts (use monotonic per-host intervals, server-stamp ledger time); and a metering-pipeline outage (buffer usage events durably at each host and replay — never block sandbox lifecycle on the billing path).

---

## 5. Fleet state, clustering & service discovery (`mesh`)

This is the spine of the platform. We adopt fly's **Corrosion** design directly: a SQLite database on every node, replicated by gossip, with CRDT conflict resolution.[[5]](https://github.com/superfly/corrosion)

**Mechanics:**

- **Local SQLite on every node** — `taritd`, `tarit-api`, and `tarit-proxy` all read fleet state from a *local* DB, so lookups are sub-millisecond (fly measured ~4.5 ms for a routing lookup against 5000 indexed rows).[[11]](https://github.com/fly-apps/fly-replay-proxy-example)
- **CRDT writes** via a cr-sqlite-style extension so concurrent writers on different hosts merge deterministically without locks or consensus.[[12]](https://superfly.github.io/corrosion/)
- **SWIM gossip membership** (Foca-style) for join/leave/failure detection — this is how a new host **"joins the cluster of other hosts"**: it contacts one or more seed nodes, SWIM disseminates its membership, and CRDT sync streams it the current state.[[13]](https://github.com/caio/foca)
- **QUIC peer-to-peer transport** for low-latency, multiplexed, authenticated sync.
- **Target convergence: p99 ~1 s** for a state change to be globally visible.[[8]](https://qconlondon.com/presentation/apr2025/fast-eventual-consistency-inside-corrosion-distributed-system-powering-flyio)

**Service discovery:** when a sandbox comes up, `taritd` writes its address/port/health into the mesh; `tarit-proxy` reads it locally to route. This is exactly how Corrosion doubles as both statekeeper and service-discovery cache.[[14]](https://community.fly.io/t/self-healing-machine-state-synchronization-and-service-discovery/26134)

**Scaling the mesh itself (plan for it now):** a single gossip cluster has a node ceiling. Adopt fly's **regionalization**: a per-region cluster holds fine-grained machine state; a small **global cluster** maps app→region for edge routing. This two-tier scheme is how they got past ~800-node single-cluster limits.[[15]](https://news.ycombinator.com/item?id=45680583)

<aside>
🧠

**Schema changes are dangerous in a gossip world.** A fleet-wide Corrosion schema change once triggered a major fly outage.[[3]](https://fly.io/infra-log/2024-11-30/) Treat the mesh schema as a versioned, backward-compatible contract; roll changes additively and gate them behind feature flags.

</aside>

---

## 6. Scheduling & placement

Placement is a **scoring/bin-packing** problem (the scheduler fly "carved out" of their orchestrator).[[1]](https://fly.io/blog/carving-the-scheduler-out-of-our-orchestrator/)

**Where it runs:** scheduling is a function of `tarit-api` (or a thin dedicated `tarit-scheduler` service) that reads free-capacity advertisements from the mesh and writes a placement decision (a desired-state record). It never pushes to hosts directly.

**Algorithm:**

- **Bin-packing** to maximize density and minimize per-host VMM overhead (the <5 MiB/VM budget from the VMM PRD pays off here).[[16]](https://developer.hashicorp.com/nomad/docs/job-scheduling)
- **Spread / anti-affinity** for replicas of the same app, so one host/AZ failure can't take down a service (Nomad's spread stanza).[[17]](https://developer.hashicorp.com/nomad/tutorials/archive/spread)
- **Constraints/affinity:** region, instance class, local-NVMe presence, CPU template compatibility (critical: a suspended snapshot can only restore/relocate onto hosts with a compatible CPU template — see VMM PRD §9d).
- **Preemption** for priority tiers.
- **Optimistic concurrency:** two schedulers may pick the same host; the CRDT + a capacity check at `taritd` admission resolves the race (reject + reschedule) rather than requiring a global lock.

**Storage anchoring caveat:** a sandbox with an attached local-NVMe volume is anchored to its host (fly's hard-won tradeoff).[[18]](https://fly.io/blog/machine-migrations/) The scheduler must treat "has a local volume" as a placement constraint and prefer shared-backing storage (NFS/Ceph/NVMe-oF) or storage-migration for anything that needs to move freely (ties into VMM PRD §9d).

---

## 7. SSH / exec into the guest — without an SSH daemon

The brief's exact ask: [**fly.io](http://fly.io) / Kata-style access with no real SSH daemon in the VMM.** The pattern both systems use is a **guest agent on a vsock control channel**, with the host brokering authenticated sessions.

**Components:**

- **`pilot` (guest agent + init):** a tiny PID-1-adjacent agent baked into the guest image, like fly's `init`/hallpass and Kata's `kata-agent`. It listens on a **virtio-vsock** port (not a TCP SSH port) and exposes an RPC surface: `exec`, `attach-tty`, `signal`, `put-file`, `port-probe`, `readiness`.[[19]](https://github.com/kata-containers/kata-containers/blob/main/docs/design/VSocks.md)
- **vsock transport:** the only guest↔host channel for control. vsock accepts multiple concurrent clients (unlike a serial port) and needs no guest IP/network, so exec works even before/without guest networking — exactly why Kata uses ttRPC-over-vsock.[[20]](https://medium.com/kata-containers/tracing-in-kata-containers-d97c099509a2)
- **Exec broker in `taritd`:** terminates the *real* SSH/websocket connection from the user on the host side, authenticates it against per-org certs (hallpass model), then proxies the session bytes into the guest `pilot` over vsock. **No `sshd` runs in the guest or the VMM**; the guest only sees a vsock RPC.[[9]](https://fly.io/blog/ssh-and-user-mode-ip-wireguard/)

```
user `insta ssh` ──TLS/SSH──▶ tarit-proxy ──▶ taritd exec broker
      (per-org cert auth, macaroon caveats)          │ vsock RPC
                                                      ▼
                                            guest `pilot` (no sshd)
                                               spawns PTY / exec
```

**Why this is better than sshd-in-guest:**

- **Smaller attack surface** — no listening SSH port in the guest, nothing to brute-force (fly users hit "too many auth failures" precisely because sshd was exposed); auth is centralized in the broker.[[21]](https://community.fly.io/t/ssh-into-vm-too-many-authentication-failures/24456)
- **Works without guest networking** and survives snapshot/restore/relocation (vsock is re-established by the new host's `taritd`).
- **Centralized policy & audit** — every session is authn'd, authz'd, and logged at the host.
- **Fleet exec** — "run this command on all instances" is a broker fan-out, not N separate sshd logins.[[22]](https://community.fly.io/t/is-there-a-way-to-run-a-command-via-fly-ssh-against-all-app-instances/12346)

---

## 8. Networking & request routing (data plane)

**`tarit-proxy`** runs on every worker (fly-proxy model): a Rust proxy that accepts client connections, matches them to a sandbox via the local mesh DB, terminates TLS, and load-balances to the right VMM.[[23]](https://fly.io/docs/reference/fly-proxy/)

**Key features:**

- **Default-deny ingress:** the proxy listens broadly but nothing is reachable unless an app explicitly declares a service — locked down by default.[[24]](https://fly.io/docs/networking/services/)
- **Local-cache routing:** routing decisions come from the local mesh SQLite (sub-ms), no central lookup.
- **Backhaul between hosts:** if the target sandbox is on another worker, proxy-to-proxy over an authenticated tunnel (WireGuard/QUIC) — so any edge can serve any app.[[23]](https://fly.io/docs/reference/fly-proxy/)
- **Dynamic request routing (`tarit-replay`):** a response-header mechanism (fly-replay) lets an app bounce a request to a specific region/host/sandbox — the basis for sticky sessions and "wake the right machine."[[25]](https://fly.io/docs/networking/dynamic-request-routing/)
- **Autostart/autostop:** the proxy starts a stopped sandbox on incoming traffic and stops idle ones — the mechanism behind scale-to-zero (Section 10).[[23]](https://fly.io/docs/reference/fly-proxy/)
- **Egress** stays exactly as the VMM PRD §8 specifies (per-VM netns + nftables/eBPF allowlist) — the orchestrator just programs the per-sandbox allowlist at create time.

**Multi-region (design-ahead):** BGP Anycast at the edge so the nearest datacenter accepts the connection and the proxy backhauls to the closest healthy sandbox.[[26]](https://fly.io/docs/reference/architecture/) v1 can ship single-region with a normal cloud LB and add Anycast later.

---

## 9. Deploys & host drains (without live migration)

**Why no live migration.** Pre-copy/post-copy live migration only hits its sub-millisecond blackout when source and destination share a fast, low-jitter link (ideally same rack / RDMA). Across cloud instances (EC2/GCP/Azure) the inter-host network has variable bandwidth and jitter and no RDMA, so the dirty-page pre-copy phase may never converge and the blackout becomes unbounded. We therefore **drop live migration from v1 and treat it as indefinitely deferred**, and move sandboxes with **latency-tolerant snapshot relocation** instead. (If we later run bare-metal hosts on a fast fabric, live migration can be revisited purely as an optimization — VMM PRD §9d.)

**The relocation primitive (replaces live migration):** `suspend → snapshot (memory + disk delta) → copy artifact to destination → restore → re-point routing → destroy source`. It is a *bulk copy with a single defined pause*, so it tolerates cloud inter-host latency: the only user-visible impact is a short pause (sub-second to a few seconds, scaling with working-set size and link bandwidth), never an unbounded blackout. Stopped sandboxes relocate with no pause at all (just a snapshot copy).

**Deploy strategies** (selectable per app):

- **Blue/green (preferred, truly zero-downtime):** stand up the new version alongside the old, health-check, flip routing in the mesh, then drain the old. Works for any stateless or replicated app and needs no migration at all.
- **Rolling recreate:** for replicated apps, replace replicas N-at-a-time on already-updated hosts; the proxy keeps routing to healthy replicas, so there is no downtime.
- **Suspend-relocate (stateful single-instance sandboxes):** when one long-lived sandbox must change hosts, use the relocation primitive above — a bounded pause, connections re-established after restore. Use sparingly; prefer making workloads replicated.
- **Recreate / immediate:** for stateless ephemeral sandboxes, just stop+start the new image (cheapest).

**Host drain flow (also used for rebalancing):**

1. `taritd` marks the host `draining` in the mesh (scheduler stops placing new work there).
2. For each sandbox: stateless/replicated ones are recreated elsewhere (blue/green style, no pause); stateful ones are **suspend-relocated** to a compatible destination (CPU template + storage reachable) during the drain window.[[18]](https://fly.io/blog/machine-migrations/)
3. `tarit-proxy` re-points routing to the destination as each sandbox cuts over (mesh update).
4. When empty, the host can be patched/terminated.

<aside>
🔁

**Stopped machines relocate for free.** fly notes that being able to move *stopped* Machines made rebalancing "drastically simpler and less expensive" — a stopped sandbox is just a snapshot + a capacity reservation, so moving it is a snapshot copy with **zero pause**.[[27]](https://fly.io/infra-log/) Lean on this: prefer stop-then-relocate where the workload tolerates it, and reserve suspend-relocate for sandboxes that must stay warm.

</aside>

**Relocation safety:** the destination must pass CPU-template / device-model / `Persist`-schema negotiation (VMM PRD §9d) so the restored snapshot boots correctly; the orchestrator enforces these as scheduling constraints *before* relocating, and rolls back cleanly (the source stays authoritative until the destination reports healthy) on any failure — no split-brain, no lost work.

---

## 10. Scaling — up, down, and to zero

**Three independent scaling axes:**

| Axis | Mechanism | Trigger |
| --- | --- | --- |
| Per-app horizontal | Scheduler creates/destroys sandbox replicas across hosts | Metric-based (RPS, queue depth, CPU) or manual; declarative count in app config. |
| Scale-to-zero / autostart | `tarit-proxy` stops idle sandboxes; starts (restores from snapshot) on first request | Idle timeout / incoming connection — sub-second restore from the VMM's UFFD snapshot path makes this cheap.[[28]](https://fly.io/docs/getting-started/essentials/) |
| Fleet capacity (add hosts) | Provisioner spins up new nested-virt instances that boot `taritd` and join the mesh | Fleet free-capacity high-watermark, or scheduled. |

**Warm pools:** keep a small pool of paused/snapshot-restored sandboxes per popular image so user-facing start latency hides the boot/pull cost (VMM PRD §5). The orchestrator maintains pool depth as desired state.

**Fleet autoscaling controller:** a control-loop service watches aggregate utilization in the mesh and calls the **cloud provisioner** (Section 11) to add/remove worker hosts, bin-packing pressure being the primary signal. Scale-down cordons + drains via Section 9 before terminating instances so no running work is lost.

---

## 11. Cloud-agnostic provisioning (nested-virt first)

**Goal:** the same `taritd` image runs on EC2, GCP, and Azure; only a thin **provisioner shim** per cloud differs.

**Host requirement:** nested virtualization exposing `/dev/kvm`. Today this is available on EC2 **C8i/M8i/R8i** (nested virt, per-instance `CpuOptions`), GCP nested virtualization, and Azure nested-capable v-series — matching the VMM PRD §12.0 test-fleet guidance. Bare metal stays the high-fidelity option for perf.

**Provisioner abstraction:**

```
interface CloudProvisioner {
  createHosts(count, shape, region) -> [HostHandle]   // run-instances / compute.insert / az vm create
  terminateHosts([HostHandle])
  attachVolume / detachVolume
  networking: VPC/subnet + security group equivalents
}
```

- **Cloud-agnostic core, cloud-specific drivers** (Terraform/Pulumi or direct SDK per cloud). Keep all cloud specifics behind this interface so the scheduler/autoscaler stay portable.
- **Host bootstrap:** cloud-init/ignition installs the static `taritd` binary, fetches an mTLS cert from the internal CA, points it at seed nodes, and it self-joins the mesh via SWIM. No per-cloud logic in `taritd` itself.
- **Storage drivers:** map "shared backing" to each cloud's primitive (EBS io2 Multi-Attach, GCP Hyperdisk multi-writer/Filestore, Azure shared disks/Files) and "local NVMe" to instance storage — the scheduler reads which is available from host metadata.
- **Networking portability:** per-VM netns + egress policy is host-side (VMM PRD §8), so it's identical across clouds; only the host-level VPC/subnet/SG provisioning differs.

<aside>
✅

Nested virt is the cost-effective default for early cloud-agnostic worker testing and for the proven boot/exec/snapshot path, but full rootfs boot-to-login and UFFD restore targets are bare-metal-gated until the nested-virt MMIO-exit limitation is resolved. The orchestrator treats "bare metal vs nested" as a host capability class for scheduling.

</aside>

---

## 12. Health, monitoring & self-healing

**How monitoring is done (the brief's question):** three layers, all flowing through or alongside the mesh.

1. **Membership/liveness (mesh/SWIM):** SWIM gossip detects a dead *host* in seconds via failure-detection pings; the node drops from membership and its workloads become reschedule candidates.[[12]](https://superfly.github.io/corrosion/)
2. **Sandbox health (`taritd` + `pilot`):** `taritd` runs per-sandbox liveness/readiness checks (TCP/HTTP probes via the proxy, or `pilot` in-guest checks over vsock) and writes status to the mesh. The proxy only routes to `started`+`ready` sandboxes.
3. **Metrics/logs/traces pipeline:** `taritd` and `pilot` export Prometheus-style metrics (boot time, restore latency, relocation pause, per-VM mem overhead, VMs/sec — the VMM PRD §2 budget, now measured in prod), ship structured logs to object storage, and emit OpenTelemetry traces for request paths. A central TSDB (VictoriaMetrics/Mimir) + log store aggregate fleet-wide. A "memory cop" alert fails the fleet if per-VM overhead regresses (VMM PRD §12.5).

**Self-healing loops:**

- **Dead host →** scheduler reschedules its reschedulable sandboxes elsewhere (stateless or shared-storage ones immediately; locally-anchored ones need restore-from-last-snapshot). Target < 30 s detection-to-reschedule.
- **Crashed sandbox →** `taritd` restarts per the app's restart policy.
- **State drift →** the reconciler continuously re-asserts desired state, so a host that missed an update self-corrects on the next sync. fly's whole "self-healing machine state" effort is this loop done well.[[14]](https://community.fly.io/t/self-healing-machine-state-synchronization-and-service-discovery/26134)
- **Crashed `taritd` →** systemd restarts it; the durable local queue lets it resume in-flight operations exactly where it left off.

---

## 13. Image & volume pipeline

- **OCI image → microVM rootfs:** pull the image, extract layers, and build an **ext4 rootfs** via the `vmm pull` path (or later use virtio-fs to mount the image directly, à la krunvm) — the same "Docker without Docker" transmogrification fly does.[[29]](https://github.com/stacklok/go-microvm)[[30]](https://containers-krunvm.mintlify.app/introduction) Builds run on the **off-path build queue** (Section 4b), not the scheduling path. On c8i nested virt, OCI conversion works; rootfs boot-to-login remains a bare-metal conformance gate.
- **Rootfs/snapshot caching:** cache built rootfs + warm snapshots on local NVMe and in the object store, keyed by image digest, so a second start of the same image is a snapshot-restore, not a rebuild.
- **Volumes:** CoW overlays per sandbox (VMM PRD §7); the volume manager handles attach/detach, overlay creation, and snapshot export. Shared-backing vs local-NVMe is recorded as host/volume metadata for the scheduler.
- **Registry:** a standard OCI registry (cloud-agnostic) holds user images; an internal artifact store holds snapshots.

---

## 14. Security & multi-tenancy

- **Auth everywhere:** external macaroon tokens + SSO; internal mTLS with short-lived certs; per-org certs for exec (Section 4a/7).
- **No guest-reachable control surface:** exec is vsock-brokered, egress/ingress are host-enforced, and the guest never holds platform credentials (hallpass-style cert pulling keeps secrets off the instance).[[9]](https://fly.io/blog/ssh-and-user-mode-ip-wireguard/)
- **Isolation inherited from the VMM:** KVM boundary + jailer + seccomp (VMM PRD §10); the orchestrator adds tenant-scoped quotas, network policy, and audit logging.
- **Blast-radius limits:** one VMM process per sandbox; one tenant's noisy/abusive sandbox is rate-limited (token buckets, VMM PRD §7/§8) and can't see another tenant's mesh data beyond what routing requires.
- **Supply chain:** sign `taritd`/`pilot`/kernel artifacts; verify on bootstrap.

---

## 15. Performance & SLO budget

| Operation | Target | Notes |
| --- | --- | --- |
| API → sandbox `started` (warm pool) | &lt; 300 ms p50 | Hides boot via snapshot-restore; excludes cold image pull. |
| Scheduling decision | &lt; 10 ms | Local mesh read + scoring. |
| Fleet state convergence | &lt; 1 s p99 | Gossip CRDT dissemination. |
| Routing lookup (proxy) | &lt; 5 ms | Local SQLite, indexed. |
| Blue/green deploy cutover | 0 dropped conns | Routing flip in the mesh; no migration. |
| Suspend-relocate pause (stateful) | &lt; 2 s typical | Snapshot + copy + restore; scales with RAM/link bandwidth. |
| Dead-host detect → reschedule | &lt; 30 s | SWIM detection + placement. |
| Exec session establishment | &lt; 200 ms | Auth + vsock attach. |
| New host join → schedulable | &lt; 60 s | Boot + cert + mesh sync. |
| Admission check (rate + quota) | &lt; 5 ms | Shared rate counter + CP-store reservation. |

---

## 16. Implementation roadmap (phased)

| Phase | Deliverable | Key work |
| --- | --- | --- |
| 0 — Single-host agent | `taritd` supervises the current VMM UDS API; local lifecycle API | VMM supervisor, embedded durable queue, reconciler skeleton, jailer integration, seccomp/cgroup smoke checks, parent cgroup/cpuset ownership from `taritd` |
| 1 — Control plane + auth | `tarit-api` with token/mTLS auth; desired-state model | auth, CP store for unique names/IPs + per-key rate limits & quota reservations, lifecycle state machine, operation IDs |
| 2 — Mesh | Gossip CRDT SQLite; SWIM join; service discovery | cr-sqlite-style CRDTs, Foca SWIM, QUIC transport, schema versioning |
| 3 — Scheduler | Bin-packing + spread + constraints; optimistic placement | capacity advertisement, scoring, preemption, host capability classes for nested virt vs bare metal |
| 4 — Exec without sshd | `pilot` guest agent + `taritd` exec broker over vsock | vsock RPC, per-org cert auth, PTY relay, fleet exec |
| 5 — Data plane | `tarit-proxy`: default-deny ingress, local-cache routing, autostart/stop, backhaul | TLS termination, replay header, WireGuard/QUIC backhaul |
| 6 — Health + self-healing | Heartbeats, probes, metrics/logs/traces, reschedule-on-failure | SWIM-driven detection, TSDB/log pipeline, reconcile drift, metering aggregator + usage ledger, perf/soak gate ingestion |
| 7 — Deploys + drains | Blue/green + rolling recreate; host drain via snapshot relocation (no live migration) | relocation driver (pause/full snapshot/restore safe path), optional live-snapshot optimization after fleet soak, routing cutover, stopped-machine relocate |
| 8 — Scaling | Per-app autoscale, scale-to-zero, warm pools, fleet autoscaler | metric controllers, pool depth, provisioner integration, full snapshot/restore on nested virt; UFFD lazy restore and rootfs boot acceleration on bare metal |
| 9 — Multi-cloud provisioning | Provisioner drivers for EC2/GCP/Azure nested virt plus bare-metal validation pools | cloud-init bootstrap, storage/network driver mapping, rootfs boot-to-login and UFFD gates on bare metal |
| 10 — Multi-region (design-ahead) | Regionalized mesh + edge Anycast routing | two-tier mesh, BGP/Anycast, cross-region backhaul |

---

## 17. Automated testing plan

- **17.1 Unit:** state-machine transitions (every lifecycle edge), scheduler scoring/bin-packing/spread (table-driven), CRDT merge determinism, auth token caveat enforcement, exec-broker auth.
- **17.2 Integration (multi-host, on nested-virt CI):** spin up an N-host mesh; assert a created sandbox lands, starts, becomes healthy, and is routable using the proven nested-virt execution path; kill a host and assert reschedule < 30 s; assert state convergence < 1 s. Track OCI → ext4 conversion in nested CI, but run **rootfs boot-to-login E2E on bare metal** until c8i nested virt can deliver the required MMIO exits.
- **17.3 Exec/security:** prove **no sshd** is reachable in guest; exec only succeeds with a valid per-org cert; session is audited; fleet-exec fans out correctly; vsock survives snapshot/restore.
- **17.4 Deploy/drain:** blue/green deploy of a replicated app — assert zero dropped connections through the routing flip; suspend-relocate of a stateful sandbox via **pause → full snapshot → restore** — assert a bounded pause and correct restore on the destination; host-drain empties cleanly; rollback on injected relocation failure leaves the source authoritative (no split-brain). Add a separate live-snapshot optimization test, but explicitly assert **no live-migration path is exercised**.
- **17.5 Scaling:** autoscale up under synthetic load; scale-to-zero then cold-start on request; fleet autoscaler adds/cordons/drains hosts correctly.
- **17.6 Chaos / fault injection:** partition the gossip network and assert convergence on heal (no permanent split-brain); kill `taritd` mid-operation and assert resume from the durable queue; schema-change rollout under load (guard against the fly Corrosion incident class).
- **17.7 Multi-cloud conformance:** the same suite runs against EC2, GCP, and Azure nested-virt fleets; only the provisioner driver changes.
- **17.8 Perf gates (CI, fail on regression):** every §15 SLO tracked over time; block merges on statistically significant regressions. Track the current substrate baselines separately: 14–16 ms p50 / 61 ms p99 cold boot to HLT, 16–24 ms p50 / 23 ms p99 full boot to `exec echo`, ~73 ms p50 / 276 ms p99 snapshot for 256 MiB, 85–99 ms eager restore on c8i nested virt, <10 ms UFFD restore on bare metal, and ~82 MiB per-VM RSS on c8i nested virt.
- **17.9 Metering, quotas & rate limits:** wall-time accuracy across stop/start and snapshot-relocation (no double-count, no gaps); VM-start rate limit returns 429 with correct `Retry-After`/`RateLimit-*` headers under burst; concurrent vCPU/RAM quota rejects an over-limit create and admits it after a release; reservation-leak reconciliation heals counters after a host kill; idempotent usage events survive retries; limit resolution (key→org→plan) and macaroon attenuation are enforced; the metering pipeline can be down without blocking sandbox lifecycle.

---

## 18. Key risks & pitfalls

- **Gossip storms / backlog amplification** — a recovering dependency or a chatty writer can melt the mesh (fly's 150 GB/s incident).[[10]](https://fly.io/infra-log/2024-10-26/) Mitigate: local retry/backoff, compact CRDT deltas, rate-limited gossip, backpressure.
- **Eventual-consistency surprises** — stale routing/placement during convergence windows. Mitigate: optimistic concurrency + admission checks at `taritd`; never assume the mesh is instantly consistent.
- **Schema migrations fleet-wide** — a known outage class.[[3]](https://fly.io/infra-log/2024-11-30/) Mitigate: additive, versioned, feature-flagged schema changes.
- **Storage anchoring vs relocation** — local-NVMe volumes pin sandboxes.[[18]](https://fly.io/blog/machine-migrations/) Mitigate: prefer shared-backing for movable workloads; treat anchoring as a scheduling constraint.
- **Mesh scale ceiling** — single gossip cluster caps out (~hundreds of nodes).[[15]](https://news.ycombinator.com/item?id=45680583) Mitigate: regionalize early in the design.
- **CPU-template drift across the fleet/clouds** — breaks snapshot restore on a different host (a suspended snapshot won't resume on an incompatible CPU). Mitigate: enforce a common CPU baseline per relocation domain; reject incompatible targets at negotiation (VMM PRD §13).
- **Exec broker as a single choke point** — it's now security-critical. Mitigate: stateless brokers, per-host locality, short-lived certs, thorough audit.
- **Nested-virt perf/functionality variance** — c8i nested virt is fast for boot/exec, but snapshot/restore miss bare-metal targets and rootfs boot-to-login is blocked by L0 MMIO-exit coalescing. Mitigate: keep nested-virt and bare-metal conformance lanes separate; run OCI/rootfs boot and UFFD restore gates on bare metal before promising them to customers.

---

## 19. Prior art to mine (don't reinvent)

- [**fly.io](http://fly.io) flyd / Corrosion / fly-proxy / hallpass / Pilot** — the closest end-to-end model for a microVM PaaS: per-host orchestrator, gossip CRDT state, edge proxy, sshd-less exec, machine migration. Mine their blog + infra log heavily.[[6]](https://www.youtube.com/watch?v=kPHmRgDTvNc)[[5]](https://github.com/superfly/corrosion)
- **Kata Containers** — the canonical guest-agent-over-vsock (ttRPC) design for exec without sshd.[[19]](https://github.com/kata-containers/kata-containers/blob/main/docs/design/VSocks.md)
- **firecracker-containerd** — integrating a Firecracker-class VMM with a container control plane; OCI image handling.[[31]](https://github.com/firecracker-microvm/firecracker-containerd)
- **HashiCorp Nomad** — bin-packing, spread, affinity, preemption, and the server/client + task-driver-plugin split.[[16]](https://developer.hashicorp.com/nomad/docs/job-scheduling)
- **krunvm / go-microvm** — OCI-image-as-rootfs pipelines and minimal microVM tooling.[[30]](https://containers-krunvm.mintlify.app/introduction)[[29]](https://github.com/stacklok/go-microvm)

---

## 20. Host lifecycle, admission & backpressure

The host is the real blast-radius boundary. A scheduler decision is only a *proposal*; `taritd` must still perform local admission before accepting work.

**Host lifecycle states:**

```
provisioning → bootstrapping → joining → warming → schedulable
schedulable → cordoned → draining → drained → retiring → terminated
schedulable → unhealthy → quarantined → recovered / retired
```

**Before a host becomes schedulable, it must pass:**

- `/dev/kvm` sanity check and VMM self-test.
- Current VMM UDS lifecycle smoke test: create → pause → resume → snapshot → restore → stop.
- CPU template compatibility check.
- Image-cache and snapshot-cache mount checks.
- Local disk/inode pressure check.
- Network path check to seeds, object store, registry, and proxy backhaul.
- Clock skew check.
- mTLS cert validity.
- Mesh sync completeness.
- VMM start/stop smoke test.

**Local admission gate (`taritd`):** even after placement, reject/defer if the host has changed since its last mesh advertisement:

- free RAM after host reserve (`taritd`, proxy, kernel, logging).
- free disk, inode pressure, and cache pressure.
- open FD, pid, cgroup, tap/veth/netns, and conntrack limits.
- per-host max VMM process count.
- per-host max concurrent starts, restores, image pulls, and snapshot exports.
- CPU steal/load and IO pressure.
- local network saturation.

**Backpressure rules:**

- Soft reject: ask scheduler to retry elsewhere.
- Hard reject: mark host `degraded` or `quarantined`.
- Emergency: cordon host and trigger drain.
- Always emit a customer-visible operation event (`NO_CAPACITY`, `HOST_DEGRADED`, etc.) instead of failing silently.

## 21. Disk, cache GC & artifact lifecycle

Local disk is a first-class resource, not an implementation detail. Fast starts depend on local cache; uncontrolled cache kills the host.

`taritd` owns:

- OCI layer cache GC.
- rootfs cache GC.
- warm snapshot GC.
- dead overlay cleanup.
- incomplete build/unpack cleanup.
- orphaned volume detection.
- per-tenant disk quotas.
- snapshot TTL / retention policies.
- artifact checksum validation.
- emergency cleanup before host quarantine.

**Watermarks:**

- **<70% disk:** normal.
- **70–80%:** background GC.
- **80–90%:** avoid cold image pulls and reduce warm-pool depth.
- **90–95%:** no new placements; aggressive GC.
- **>95%:** emergency GC + cordon/quarantine if not recovered.

GC must be idempotent and safe under `taritd` crash/restart. Never delete an artifact referenced by a running sandbox or a durable operation.

## 22. Secrets, config & environment injection

A PaaS needs a first-class way to inject secrets/config without baking them into images or snapshots.

**Objects:**

- Org/project/app secrets.
- Environment variables.
- Secret versions.
- Secret access audit log.

**Rules:**

- Control plane stores encrypted secret metadata and ciphertext.
- Host receives only the minimum secret bundle for sandboxes it runs.
- Delivery is at sandbox start via tmpfs/env/virtiofs/vsock, not baked into rootfs.
- Secrets are redacted from logs, exec output, events, and crash dumps.
- Rotation creates a new secret version and triggers restart/reload policy.
- Snapshots must not persist plaintext secrets unless explicitly allowed.
- `pilot` should be a delivery target, not a long-lived secret store.

## 23. Customer logs, events & debugging

Metrics are for operators; events are for users. v1 should expose logs/events before a dashboard.

**Per-sandbox streams:**

- stdout/stderr.
- boot console logs.
- guest kernel panic output.
- VMM stderr.
- last-N-KB crash ring buffer.
- lifecycle events.

**Customer-facing events:**

```
scheduled
image_pull_started
image_pull_completed
rootfs_ready
starting
started
ready
health_check_failed
oom_killed
stopped
restarted
quota_rejected
rate_limited
relocating
relocated
destroyed
```

**CLI/API:**

```
insta sandbox logs <id> --follow
insta sandbox events <id>
insta sandbox inspect <id>
insta operations get <operation_id>
```

Every failed operation must explain *why* it failed and whether it is retryable.

## 24. DNS, domains, TLS & private networking

The proxy/routing layer needs product-level network primitives.

**Public ingress:**

- App default domain.
- Custom domains.
- Automatic TLS issuance/renewal.
- Wildcard preview domains.
- HTTP/1.1, HTTP/2, WebSocket, raw TCP where declared.
- Request/body size limits.
- Proxy-level rate limits and DDoS hooks.

**Private networking:**

- Per-org/project private network namespace.
- Internal DNS:
    - `app.internal`
    - `sandbox-id.project.internal`
- Private service discovery from the mesh.
- Egress NAT identity.
- Optional static outbound IP.
- IPv6 plan.
- Conntrack and per-sandbox connection limits.

Ingress remains default-deny: nothing is public until a service is declared.

## 25. Resource isolation, CPU overcommit & noisy-neighbor controls

Quota says what a key may request; host isolation enforces what a sandbox may actually consume. **Resource classes are orchestrator/product policy.** Keep this entirely in the PaaS control plane, scheduler, and `taritd`; do not push plan logic or fairness policy into the microVM implementation.

### 25a. Orchestrator ownership boundary

| Layer | Owns |
| --- | --- |
| Control plane / API | Product shapes (`shared-2x512`, `reserved-2x1g`, `dedicated-4x4g`), plans, per-key/org quotas, VM-start rate limits, operation records. |
| Scheduler | Host selection, overcommit ratio, spread/anti-affinity, reserved/dedicated capacity accounting, avoiding hot hosts and overloaded tenants. |
| `taritd` | Local admission, cgroup creation/update, CPU weights/caps, memory hard limits, cpuset pinning, IO/network throttles, noisy-neighbor detection, throttling/quarantine. |
| Sandbox runtime | Runs inside the host resource envelope that `taritd` creates. It does not know customer plan, trust tier, quota, or overcommit policy. |

**Rule:** shared/burstable/reserved/dedicated CPU is a **PaaS resource contract**, not a low-level runtime feature. `taritd` owns the parent cgroup/cpuset policy and launches sandboxes through the jailer with the correct resource envelope. The jailer is now real defense-in-depth (chroot, namespaces, rlimits, capabilities, cgroup limits), while `taritd` remains the policy authority.

### 25b. Resource classes

| Class | Meaning | Orchestrator enforcement |
| --- | --- | --- |
| Shared / burstable | Cheap overcommitted CPU. Fair share under contention; can burst when host is idle. | cgroup v2 `cpu.weight` for proportional fairness + `cpu.max` for burst cap. Scheduler may overcommit within policy. |
| Reserved | Stronger CPU entitlement, not heavily overcommitted; may still share cores. | Scheduler accounts reserved vCPU close to 1:1 against host budget; `taritd` enforces entitlement with cgroup weight/cap. |
| Dedicated | Premium latency-sensitive CPU with exclusive host cores. | `cpuset.cpus` pinning by `taritd`; avoid SMT sibling sharing for strict isolation where possible. |

**V1 policy:** overcommit **shared CPU only**. Do **not** overcommit RAM in v1. Reserved/dedicated CPU should be treated as capacity that cannot be freely oversold.

### 25c. cgroup hierarchy

`taritd` creates a cgroup v2 hierarchy that allows enforcement at sandbox, API-key, and org levels:

```
/sys/fs/cgroup/tarit.slice/
  system/
    taritd
    tarit-proxy
    log-agent
    metrics-agent

  tenants/
    org_123/
      key_abc/
        sandbox_sbox_1/
        sandbox_sbox_2/

    org_456/
      key_def/
        sandbox_sbox_3/
```

**Per-sandbox controls:**

- `cpu.weight` — relative CPU share under contention.
- `cpu.max` — hard cap / burst cap.
- `memory.max` — hard memory limit.
- `memory.high` — early pressure signal before OOM.
- `pids.max` — fork-bomb protection.
- `io.max` / `io.weight` — disk bandwidth/IOPS fairness.
- `cpuset.cpus` — dedicated/reserved pinning where applicable.

**Parent controls:** key/org cgroups can enforce aggregate caps so one API key cannot start many small sandboxes and dominate a host.

### 25d. Shared CPU fairness

For shared/burstable CPU, fairness is **weighted fair sharing + hard burst caps**.

Example:

```
shared 1 vCPU:
  cpu.weight = 100
  cpu.max = 150000 100000   # up to ~1.5 CPUs burst

shared 2 vCPU:
  cpu.weight = 200
  cpu.max = 300000 100000   # up to ~3 CPUs burst
```

If the host is idle, sandboxes can burst. If the host is contended, Linux CFS gives each cgroup proportional CPU based on `cpu.weight`. `cpu.max` prevents any sandbox from burning the entire host indefinitely.

### 25e. Reserved and dedicated CPU

Reserved CPU is a scheduler guarantee first, cgroup-enforced second:

- Scheduler accounts reserved vCPU against host capacity conservatively.
- `taritd` still applies `cpu.weight` and `cpu.max`.
- Reserved CPU can optionally allow small burst, but not arbitrary oversubscription.

Dedicated CPU is a host-placement policy:

- Scheduler finds hosts with free dedicated-core capacity.
- `taritd` assigns the sandbox process tree to the selected cpuset.
- For stricter isolation, avoid placing another tenant on the SMT sibling of a dedicated core.
- Keep a separate system/IO pool so host daemons are not starved by dedicated tenants.

### 25f. Memory policy

**Do not overcommit RAM in v1.** CPU overcommit is recoverable through throttling; memory overcommit risks host OOM and unpredictable latency.

Rules:

- Requested sandbox memory + runtime overhead + host reserve must fit within physical memory budget.
- `memory.max` enforces the hard per-sandbox limit.
- `memory.high` provides early pressure signal before OOM.
- Swap disabled or tightly controlled for tenant workloads.
- If the sandbox exceeds memory, the sandbox OOMs or is killed; the host must not OOM.
- Emit `SANDBOX_OOM` with operation/event details.

### 25g. IO, network, and log fairness

CPU cgroups are not enough. A sandbox can DoS neighbors via disk, network, logs, conntrack, or process count.

Enforce per sandbox and optionally per key/org:

- disk IOPS and bandwidth (`io.max` / `io.weight`).
- network egress Mbps and packets/sec (`tc`, eBPF, nftables, or tap-level shaping).
- conntrack/open-connection limits.
- log bytes/sec and max retained log volume.
- snapshot/export rate limits.
- image-pull/build concurrency limits.
- blocked outbound ports by trust tier.

### 25h. Noisy-neighbor detector

`taritd` continuously monitors host and cgroup pressure.

**Host signals:**

- CPU PSI, memory PSI, IO PSI.
- run queue length per core.
- CPU steal time.
- IO wait and disk latency.
- conntrack usage.
- packet drops.
- host load and throttling.
- system slice starvation.

**Sandbox signals:**

- CPU usage vs entitlement.
- cgroup throttled time/percentage.
- memory pressure and OOM events.
- IO bytes/IOPS.
- network bytes/pps.
- failed egress attempts.
- process count/fork rate.
- sustained 100% CPU duration.

Classification:

```
normal → bursty → sustained_hot → noisy → abusive
```

Actions:

- `bursty`: allow.
- `sustained_hot`: reduce burst cap.
- `noisy`: throttle and emit customer-visible event.
- `abusive`: suspend/kill/quarantine according to policy.

Customer-visible events:

```
CPU_THROTTLED
IO_THROTTLED
NETWORK_THROTTLED
SANDBOX_OOM
NOISY_NEIGHBOR_DETECTED
SANDBOX_SUSPENDED_ABUSE
```

### 25i. Host reserve and placement backpressure

Always reserve resources for host daemons:

- `taritd`
- `tarit-proxy`
- logging/metrics agents
- kernel workers
- emergency break-glass access

Do not schedule new shared workloads if:

- CPU PSI exceeds threshold.
- memory pressure exceeds threshold.
- disk > 90%.
- conntrack > 80%.
- image pulls/restores are saturated.
- host reserve is violated.
- a single org already dominates the host.

This makes overcommit safe: the scheduler can be optimistic, but the host remains authoritative.

## 26. Snapshot, restore & volume consistency

Because live migration is out, snapshot/restore is the core reliability primitive. The current VMM has full snapshot/restore working E2E and live snapshot wired through `VmmController::live_snapshot()`. Orchestrator v1 should keep **full snapshots** as the safe scale-to-zero/drain/relocation path and treat live snapshot as an optimization path after fleet-level soak.

**Snapshot types:**

- Crash-consistent snapshot: default, fast.
- Application-consistent snapshot: optional `pilot` hooks:
    - `pre_snapshot`
    - filesystem freeze / flush
    - `post_snapshot`
- Memory snapshot + disk delta snapshot for warm restore.
- Disk-only snapshot for backup/clone.

**Correctness requirements:**

- Snapshot manifest with version, checksums, sizes, CPU template, kernel/VMM version, rootfs digest, volume list, and encryption metadata.
- Atomic publish: incomplete snapshots are never visible.
- Restore validation before routing traffic.
- Snapshot corruption detection.
- Encrypted at rest.
- TTL/retention policy.
- Partial upload cleanup.
- Restore-to-new-sandbox flow.
- Cross-region copy later.

Volumes need explicit consistency semantics: local-NVMe volumes are anchored unless copied; shared-backing volumes require attach/detach fencing to avoid double-writers.

## 27. Version compatibility & rolling upgrades

`taritd`, VMM, kernel, snapshot format, mesh schema, proxy, and `pilot` all need compatibility rules.

**Track:**

- `taritd` version.
- VMM version.
- guest kernel version.
- `pilot` protocol version.
- rootfs format version.
- snapshot format version.
- mesh schema version.
- proxy route schema version.

**Rules:**

- `pilot` performs a feature-negotiation handshake.
- `taritd N` supports VMM `N` and `N-1`.
- Snapshot format must be readable for at least one minor version.
- Mesh schema changes are additive and feature-flagged.
- Incompatible host upgrade requires cordon → drain → upgrade → self-test → uncordon.
- Canary first; then rolling upgrade; auto-rollback on SLO or health regression.

## 28. Abuse prevention & trust tiers

Public sandbox infrastructure will be abused. Build limits into the product from day one.

**Default new account / low-trust key:**

- max concurrent sandboxes: very small.
- max vCPU/RAM: small.
- max starts/min: small.
- restricted outbound ports.
- SMTP blocked by default.
- short max runtime.
- low storage quota.

**Trust tiers unlock:**

- higher starts/min.
- higher concurrent vCPU/RAM.
- longer runtimes.
- broader egress.
- more storage.
- custom domains.
- static outbound IP.

**Detection / response:**

- port scanning detection.
- crypto-mining heuristics.
- malware/phishing reports.
- egress anomaly detection.
- automated suspension/quarantine.
- abuse audit log.

## 29. Control-plane DR & mesh recovery

The hot path avoids a central queue, but the small CP store and seed set still need disaster recovery.

**Recoverable state:**

- org/project/app metadata.
- API keys and plans.
- quota reservations.
- name/IP allocator.
- desired-state records.
- operation records.
- mesh seed inventory.

**DR requirements:**

- CP store point-in-time restore.
- Quota reservation reconstruction from mesh running set.
- Name/IP allocator consistency check.
- Mesh bootstrap from durable snapshot.
- Seed-node replacement flow.
- Read-only/degraded API mode.
- Host reconnection storm backoff.
- Disaster-mode scheduler pause until state confidence is restored.

## 30. API idempotency, operations & error taxonomy

Every mutating API should produce an operation. User intent must be exactly-once; internal execution may be at-least-once.

```
Operation {
  id
  org_id
  api_key_id
  type: create_sandbox | start | stop | delete | snapshot | restore | deploy | drain
  idempotency_key
  status: queued | running | succeeded | failed | cancelled
  target_resource
  created_at
  updated_at
  timeout_at
  error_code
  error_message
  retryable
  events[]
}
```

**API rules:**

- Mutating requests accept `Idempotency-Key`.
- Response returns `operation_id`.
- Users can `GET /operations/:id`.
- Users can cancel cancellable operations.
- Long-running operations have timeout semantics.
- Retried requests with the same idempotency key return the same operation.

**Stable error taxonomy:**

```
QUOTA_EXCEEDED
RATE_LIMITED
NO_CAPACITY
IMAGE_PULL_FAILED
IMAGE_TOO_LARGE
ROOTFS_BUILD_FAILED
SNAPSHOT_RESTORE_FAILED
HOST_UNHEALTHY
SANDBOX_OOM
HEALTH_CHECK_FAILED
EXEC_AUTH_FAILED
VOLUME_ATTACH_FAILED
START_TIMEOUT
EGRESS_BLOCKED
SECRET_INJECTION_FAILED
DNS_CONFIG_FAILED
TLS_CERT_FAILED
```

Each error has a machine-readable code, human-readable message, retryable flag, remediation hint, and linked operation/event.

<aside>
🔗

This layer turns the VMM PRD's primitives (sub-second boot, snapshot/restore/clone, live snapshot, host-enforced egress) into a product: an API, a scheduler, a self-healing fleet, and zero-downtime deploys via blue/green + snapshot relocation (no live migration). The two documents are meant to be read together — the VMM is the engine, `taritd` is the car.

</aside>