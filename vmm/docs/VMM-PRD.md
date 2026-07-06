# Minimal rust-vmm MicroVM: Implementation PRD, Architecture & Testing Plan

<aside>
🎯

A code-grounded blueprint for a minimal, blazing-fast VMM built from <strong>rust-vmm</strong> crates that boots a Linux kernel, attaches storage volumes, snapshots and restores, suspends/resumes in milliseconds, takes <strong>live snapshots without pausing the guest</strong>, and enforces host-side egress controls the guest cannot override. It also migrates a running VM live across hosts. Scoped for the Tarit microVM substrate.

</aside>

## 0. Implementation PRD — overview

**Document type:** Implementation PRD (engineering + product requirements) for a minimal rust-vmm microVM substrate. §§1–10 are the architecture, §11 the phased delivery plan, §12 the test strategy.

**Problem / motivation:** Tarit needs a purpose-built, minimal VMM it fully controls — to hit sub-second sandbox starts, dense per-host packing, snapshot/restore/clone, and host-enforced egress — without inheriting the size and attack surface of a general-purpose hypervisor. Building on rust-vmm gives memory-safe building blocks so we only write our differentiators (snapshot/clone, live snapshot, egress, migration).

**In scope (v1):** the seven functional requirements in §1 + cross-host live migration (§9d).

**Out of scope (v1):** see Non-goals in §1.

**Objectives & key results:**

- **O1 — Fast:** cold boot <125 ms, restore <10 ms, >100 microVMs/host/s (§2).
- **O2 — Stateful:** full + diff + live snapshots, suspend/resume, clone N-from-1 (§9).
- **O3 — Portable:** cross-host live migration with <5 ms blackout (§9d).
- **O4 — Secure:** default-deny egress the guest cannot override; jailer + seccomp (§8, §10).
- **O5 — Lean:** <5 MiB VMM overhead per VM (§2).

**Success metrics / acceptance:** every target in §2 met at p50/p99 in CI perf gates (§12.5); full security red-team suite passing (§12.6); migration round-trip + convergence tests passing (§12.4).

**Stakeholders / owners (assign):** VMM core (eng owner), Security reviewer (egress/jailer/seccomp), Infra (CI host fleet), Product (sandbox lifecycle API).

**Milestone summary:** Phases 0–7 (§11); each phase ships behind tests with explicit exit criteria. Sequencing keeps the snapshot / dirty-tracking foundations (Phases 3–5) ahead of migration (Phase 7), which reuses them.

**Key dependencies & assumptions:** KVM-capable Linux hosts (bare-metal or nested-virt instances — see §12.0); a curated rust-vmm lockfile (§3); minimal guest kernels (virtio + serial only).

---

## 1. Design goals & non-goals

**Functional requirements (from the brief)**

- Boot an unmodified Linux kernel (direct kernel boot, no firmware/BIOS).
- Attach one or more storage volumes that can be added/detached per VM.
- Full snapshot + restore: serialize VM state to files and boot a fresh VM from them, repeatably.
- Fast suspend → resume (pause vCPUs + flush to a snapshot, resume in low ms).
- **Live snapshots** taken while the guest keeps running (no observable stop-the-world pause).
- Isolated per-VM networking with a host-enforced egress allowlist that the guest cannot bypass, disable, or reconfigure.
- Blazing-fast cold start and lifecycle operations.

**Design principles**

- **Delete every feature you can justify deleting.** Firecracker is ~117k LoC precisely because it strips PCI, BIOS, USB, GPU, and most legacy devices; its entire VM-create-and-run path is only a few hundred lines, while the rest is the productionization. We follow the same minimalist device model.
- **MMIO-only transport, no PCI.** virtio-mmio + direct kernel boot is the single biggest boot-time lever.
- **Host owns the security boundary.** Networking egress, rate limits, and jailing live in the host/VMM, never in the guest.
- **Snapshot-native from day one.** Memory layout, device state, and the run loop are all designed around serialize/restore and dirty-page tracking, not bolted on later.

**Non-goals (v1):** GPU/accelerator passthrough, Windows guests, SR-IOV, ACPI hotplug of CPUs, nested virtualization. (Cross-host **live migration is in scope** — see §9d — because the snapshot / dirty-tracking / UFFD / device-`Persist` machinery already provides ~90% of what it needs.)

---

## 2. Performance budget (targets)

| Operation | Target | Reference point |
| --- | --- | --- |
| Cold boot → guest <code>/sbin/init</code> | &lt; 125 ms | Firecracker spec: ≤125 ms with minimal kernel + rootfs |
| Restore from snapshot → running | &lt; 10 ms (eager small VM), &lt; 30 ms typical | Firecracker restore measured as low as ~4 ms |
| Suspend (pause + serialize, UFFD) | &lt; 30 ms | state file is small; memory backed lazily |
| Live snapshot guest pause window | &lt; 1–5 ms (only the final convergence stop) | QEMU background-snapshot / dirty-ring model |
| microVMs created per host per second | &gt; 100 | Firecracker: up to 150/s/host |
| Per-VM VMM memory overhead | &lt; 5 MiB | Firecracker design point |

---

## 3. rust-vmm crate selection

rust-vmm is a shared-ownership project (AWS, Intel, Red Hat, Google, Alibaba, Linaro, and others) providing reusable building blocks so you implement only your differentiators rather than re-writing KVM wrappers and virtio devices. The canonical way to wire them together is the official `rust-vmm/vmm-reference`, a thin binary = simple CLI + a `vmm` crate that ingests the building blocks; start there and prune.

| Crate | Role in our VMM |
| --- | --- |
| <code>kvm-ioctls</code> / <code>kvm-bindings</code> | Safe wrappers over <code>/dev/kvm</code>: create VM, set memory regions, create vCPUs, run loop, get/set CPU & device state (snapshot), dirty-log ioctls. |
| <code>vm-memory</code> | Guest physical memory abstraction (mmap'd regions). Decouples devices/loaders from the memory provider; the substrate for snapshot dumps and dirty tracking. (x86_64, aarch64, riscv64.) |
| <code>linux-loader</code> | Parse and load the kernel image (bzImage/ELF/PE), set up the zero page / boot params, kernel command line. |
| <code>vm-virtio</code> / <code>virtio-queue</code> | virtqueue (descriptor ring) handling, virtio v1.0+ device/queue abstractions. |
| <code>virtio-device</code> / <code>virtio-bindings</code> | Device trait scaffolding + generated virtio constants. |
| <code>vhost</code> / <code>vhost-user-backend</code> | Optional: offload block/net dataplane to an out-of-process or in-kernel backend (<code>VhostUserDaemon</code>, <code>VhostUserBackendMut</code>, <code>VringT</code>) for higher throughput and stronger isolation. |
| <code>vm-device</code> | Device manager, MMIO/PIO bus, interrupt routing abstractions. |
| <code>event-manager</code> | epoll-based event loop driving device I/O and the control plane. |
| <code>vm-allocator</code> | Guest address-space / IRQ / MMIO range allocation. |
| <code>vm-superio</code> | Minimal legacy devices: 16550 serial (console), i8042, RTC — kept to the bare minimum. |
| <code>vm-fdt</code> (aarch64) | Flattened device tree generation for ARM boot. |
| <code>linux-loader</code> + <code>seccompiler</code> | <code>seccompiler</code> compiles per-thread seccomp-BPF filters to lock down the VMM's syscall surface. |
| <code>vmm-sys-util</code> | Shared helpers: eventfd, tempfile, ioctl macros, terminal, signal handling. |

<aside>
📌

Pin crate versions and bind them through a single workspace, exactly as `vmm-reference` does — feature combinations across crates can become mutually incompatible, so a curated lockfile is part of the architecture, not an afterthought.

</aside>

---

## 4. High-level architecture

```
                       ┌──────────────────────────────────────────────┐
Control plane (API) ── │  VMM process (one per microVM)                │
REST/gRPC over UDS     │                                              │
                       │  ┌────────────┐   ┌──────────────────────┐  │
                       │  │ Control     │   │ event-manager (epoll)│  │
                       │  │ thread      │   │  device I/O loop      │  │
                       │  └─────┬──────┘   └─────────┬────────────┘  │
                       │        │                    │               │
                       │   ┌────▼─────┐        ┌─────▼──────┐        │
                       │   │ vCPU 0..N │  KVM   │ virtio-blk  │ vols  │
                       │   │ threads   │◄──────►│ virtio-net  │──tap  │
                       │   └────┬─────┘  ioctls │ virtio-vsock│       │
                       │        │               │ serial/RTC  │       │
                       │   ┌────▼───────────────▼────────────┐       │
                       │   │ vm-memory (mmap guest RAM)        │       │
                       │   │  + dirty-page tracking            │       │
                       │   └──────────────────────────────────┘       │
                       │   seccomp filter • jailer • netns • cgroups   │
                       └──────────────────────────────────────────────┘
      Host enforcement: network namespace + nftables/eBPF egress + rate limiter
```

**Threading model:** one thread per vCPU (each runs `KVM_RUN`), one device/event thread (epoll via `event-manager`), one control thread for the API socket. Guest RAM is a set of mmap'd `vm-memory` regions shared with KVM via `KVM_SET_USER_MEMORY_REGION`.

**Process model:** one VMM process per microVM (Firecracker model) for blast-radius isolation, wrapped by a jailer (chroot + namespaces + cgroups + dropped privileges + seccomp).

---

## 5. Boot path & blazing-fast start

1. Open `/dev/kvm`, `KVM_CREATE_VM`, set up guest memory regions via `vm-memory`.
2. `linux-loader` loads an **uncompressed `vmlinux`** + minimal ext4 rootfs (or initramfs), writes boot params / zero page, sets `cmdline` (e.g. `console=ttyS0 reboot=k panic=1 pci=off i8042.noaux ...`).
3. Create vCPUs, set registers/CPUID, wire a tiny device set over **virtio-mmio** (no PCI enumeration, no option ROMs, no BIOS).
4. Launch vCPU threads into `KVM_RUN`.

**Fast-start levers (ranked by impact):**

- **Direct kernel boot + virtio-mmio** — skips firmware/bootloader entirely; this is where microVMs win (SmolBSD-class guests boot in tens of ms; Firecracker hits ≤125 ms to init).
- **Restore-from-snapshot instead of boot** for the hot path — bring up the kernel + userspace + runtime once, snapshot, then clone. Restores are 1–2 orders of magnitude faster than boot.
- **Minimal guest kernel config** — strip drivers to virtio + serial; this alone removes large chunks of probe time.
- **Trim the device model** — fewer emulated devices = less state to init and to serialize later.
- **Pre-warm pools** — keep a small pool of paused/snapshot-restored VMs ready to hand out.
- **CPU templates** — precomputed CPUID/MSR templates to avoid per-boot feature negotiation.

---

## 6. Memory architecture (the foundation for snapshots)

Guest RAM is an mmap'd region owned by the VMM and registered with KVM. Three properties make everything else possible:

- **Backing file / anonymous mmap** — memory can be dumped to a `mem_file` and later mmap'd back in (full copy) or faulted in lazily.
- **Dirty-page tracking** — KVM can report which pages changed since the last reset, enabling diff snapshots and live snapshots (Section 9).
- **userfaultfd (UFFD)** — register guest memory ranges so page faults are resolved by a userspace handler instead of the kernel, which is the key to fast lazy restore.

<aside>
🧠

Memory is the long pole in both snapshot size and restore latency. Every other subsystem (volumes, net, devices) serializes in microseconds; RAM dominates. Design the memory backend first.

</aside>

---

## 7. Storage volumes

**Device:** `virtio-blk` over MMIO. Each volume = one virtio-blk device backed by a host file or block device (raw or qcow-like). One request virtqueue per device; requests follow the standard `virtio_blk_req` (type, sector, data, status).

**Two backend options:**

- **In-VMM block backend** (simplest): the device/event thread services the virtqueue directly against the backing file. Lowest complexity, fine for v1.
- **vhost-user-blk backend** (scale/isolation): move the dataplane into a separate process via `vhost-user-backend` (`VhostUserDaemon`). Better throughput, fault isolation, and lets storage be served by a software-defined storage layer. Adopt when throughput or multi-tenant density demands it.

**Volume lifecycle:**

- Attach at create time (config) — v1.
- **Snapshot interaction:** block device backing files are *not* part of the memory snapshot; they must be captured/quiesced separately. Firecracker explicitly leaves block backing files to the operator and only guarantees flush to host FS, not to physical media. Our plan: use **copy-on-write overlays** (e.g., per-clone overlay files / reflinks / thin LVM) so a snapshot references an immutable base + a CoW delta, and clones get independent overlays cheaply.
- **Rate limiting:** token-bucket rate limiter on IOPS/bandwidth per device (Firecracker ships this per microVM).

---

## 8. Isolated networking with host-enforced egress

**Topology:** each microVM gets a `virtio-net` device backed by a **host tap interface**, and that tap lives inside a **dedicated network namespace** per VM. Per-VM netns is the practical requirement for real isolation between many microVMs.

```
guest eth0 (virtio-net) ── tap0 ── [ per-VM network namespace ]
                                      │  veth pair / TC redirect
                                      ▼
                             host routing + NAT (SNAT)
                                      │
                         ┌────────────▼─────────────┐
                         │ EGRESS ENFORCEMENT (host) │
                         │  nftables allowlist        │
                         │  + eBPF/XDP fast path      │
                         │  + DNS-aware policy         │
                         │  + token-bucket rate limit  │
                         └────────────────────────────┘
```

**Why the guest cannot override it:** all policy is enforced on the *host* side of the tap, in the VM's network namespace and on the host routing path — code the guest kernel can never touch. The guest only sees a virtio-net NIC; it has no route to the nftables/eBPF rules, the netns config, or the rate limiter. Root inside the guest does not grant host access (KVM boundary).

**Egress control design:**

- **Default-deny** egress; allow only an explicit per-VM/per-session allowlist of CIDRs/ports.
- **nftables** verdict maps for the allowlist (Ubicloud, metal-stack, and Cloudflare all build production egress firewalls this way; nftables is the maintainable baseline).
- **eBPF/XDP** fast path for high-throughput filtering when rule counts or pps grow (netfilter throughput degrades as ruleset size grows; eBPF with JIT scales better).
- **DNS-aware allowlisting** for domain-based policies: resolve allowed domains and program the resulting IPs into nftables sets dynamically (metal-stack's DNS-based egress pattern). Pair with a controlled resolver so the guest can't smuggle traffic via arbitrary DNS.
- **Rate limiting / anti-noisy-neighbor:** token-bucket on the tap (Firecracker's built-in net rate limiter model).
- **Clones keep connectivity:** when restoring/cloning from a snapshot, the guest keeps its in-memory network config, so the host rewrites tap/netns/NAT mappings underneath (Firecracker's "network for clones" strategy — e.g., per-clone TC/iptables rewriting) to avoid IP collisions across many clones of one snapshot.

---

## 9. Snapshot, restore, suspend/resume & LIVE snapshots

This is the heart of the request. Three related but distinct capabilities:

### 9a. Full snapshot + restore (clone)

**Create:** pause vCPUs → serialize device state (every device implements a `save`/`Persist`-style trait) + vCPU/KVM state into a small **state file**, and dump guest RAM to a **memory file**. Firecracker validates the state file with a CRC and persists devices via a `Persist` trait; we mirror this.

**Restore:** create a fresh VMM, load device + vCPU state from the state file, and back guest RAM with the memory file. Two memory strategies:

- **Eager copy** — read the whole mem file in; simplest, restore time scales with RAM size.
- **Lazy via UFFD** — register guest memory with userfaultfd, hand the fd to an external page-fault handler over a UDS; the handler `mmap`s the snapshot file and resolves each fault with a single `UFFDIO_COPY` directly from the mapping — no file-I/O syscalls in the hot path. This is how Firecracker achieves sub-10 ms restores and is the recommended default.

**Clone = restore the same snapshot N times.** Boot+init once, snapshot, then stamp out many VMs; combined with CoW disk overlays and netns rewriting, this is the "clone a running VM" primitive.

> Optional enhancement: serve UFFD pages chunked/compressed from a remote cache instead of a local file (BuildBuddy's approach) for cross-host clone fan-out.
> 

### 9b. Suspend / resume

Suspend = pause vCPUs + take a snapshot (state file + memory); the process can then exit, freeing host RAM. Resume = restore (ideally UFFD-lazy) so the VM is runnable in milliseconds while pages fault in on demand. For "warm" suspend without freeing RAM, simply pause vCPUs and `MADV_FREE`/`MADV_COLD` cold pages.

### 9c. Live snapshots WITHOUT pausing the guest

The goal: capture a consistent memory image while the guest keeps executing, with only a sub-millisecond convergence stop. This reuses the **dirty-page tracking** machinery from live migration, applied locally ("background snapshot").

**Mechanism — iterative pre-copy / background snapshot:**

1. Enable KVM dirty-page tracking and mark a baseline.
2. While the guest runs, copy memory pages out to the snapshot buffer in the background.
3. Re-read the dirty set, copy only newly-dirtied pages; repeat. Each round the re-dirtied set shrinks toward the working set.
4. When the remaining dirty set is small enough that it can be copied within the target downtime, do a **brief final stop** (sub-ms to a few ms), copy the last dirty pages + device/CPU state, and resume.

**Two KVM tracking mechanisms (choose/support both):**

- **`KVM_GET_DIRTY_LOG` bitmap** — classic write-protect-and-scan. Simple; the dirty bitmap is reset on snapshot so the next diff captures only subsequent writes (this is exactly how Firecracker's **diff snapshots** work).
- **`KVM_CAP_DIRTY_LOG_RING`** — per-vCPU dirty *ring* buffer. Synchronization is cheaper and can run in the background, and it scales far better to large-memory VMs — "precopy starts to look like postcopy." Preferred for low pause windows on big guests.

**True copy-on-write variant (lowest pause):** write-protect all guest pages, then a background thread copies pages out; the guest only stalls on the first write to each not-yet-copied page (handled via UFFD write-protect / KVM write-protect). This gives an effectively asynchronous, nearly pause-free snapshot — the QEMU "background snapshot" model.

<aside>
⚙️

Live snapshot = the live-migration pre-copy loop pointed at local storage instead of a remote host. The dirty-ring + write-protect path is what keeps the guest-visible pause in the single-digit-millisecond range. Build dirty tracking into the memory layer from the start (Section 6) and this capability is mostly configuration of the same primitives.

</aside>

**Diff (incremental) snapshots:** because the dirty bitmap resets on each snapshot, you can persist only changed pages between checkpoints — cheap, frequent checkpoints for record/replay or time-travel debugging.

### 9d. Live migration across hosts

Cross-host live migration is the **same pre-copy / post-copy loop as the live snapshot (§9c), but the destination is a peer host instead of a local file.** Because the design already builds dirty-page tracking, device `Persist` serialization, and a UFFD page-fault handler, most of migration is already paid for; the net-new work is a network transport, a handshake, and a remote page server.

**Flow (pre-copy + post-copy hybrid):**

1. **Negotiation** — source and destination agree on protocol version, CPU feature set (CPUID/MSR template compatibility), device model, RAM size, and volume identity over an authenticated mTLS control channel.
2. **Destination prep** — destination spins up a paused VMM shell with matching memory regions and device config, and registers guest RAM with UFFD.
3. **Iterative pre-copy** — source enables dirty tracking and streams RAM pages to the destination, re-sending only re-dirtied pages each round (dirty-ring preferred), converging toward the working set.
4. **Brief stop-and-copy** — when the residual dirty set is small, pause source vCPUs and flush the final dirty pages + device/vCPU state (CRC'd state blob), targeting a sub-5 ms blackout.
5. **Post-copy fallback** — under a high dirty rate, hand control to the destination early and demand-fetch not-yet-transferred pages over the network via UFFD (`UFFDIO_COPY` sourced from a remote page server). This bounds downtime regardless of dirty rate.
6. **Cutover** — destination resumes vCPUs; source is torn down only after a commit ack.

**Transport:** a dedicated authenticated (mTLS) page/state channel, with optional zstd compression and multi-stream parallelism for large RAM (mirrors the chunked/compressed remote-page idea in §9a).

**Storage during migration** — volumes must be reachable from both hosts:

- **Shared backing** (NFS / Ceph / NVMe-oF): only memory + device state migrate; disk stays put. Simplest.
- **Storage migration**: live-copy the CoW overlay (+ base if not shared) using block-level dirty tracking (the same idea as memory pre-copy) before the memory cutover.

**Network continuity:** the guest keeps its in-memory IP; the destination host recreates the tap / netns / egress policy and re-points routing/NAT (the same "network for clones" rewriting from §8). On a shared L2/overlay a gratuitous ARP on cutover refreshes peers; across L3 the control plane updates the overlay/SDN mapping.

**Hard requirements / constraints:**

- **Host symmetry** — the destination KVM/kernel must expose a compatible vCPU feature set; enforce via CPU templates masked to a common fleet baseline, and reject incompatible targets at negotiation.
- **Identical device model & VMM version** (or a negotiated compatible subset) — reuse the same `Persist` schema versioning as snapshots.
- **No passthrough devices** — consistent with the remaining v1 non-goals (GPU/SR-IOV/VFIO state isn't migratable), so this stays clean.

<aside>
🔁

Net-new code is mostly transport + negotiation + a remote UFFD page server. The correctness-critical core (dirty tracking, consistent memory capture, device serialization) is shared with live snapshots — so building §9c well is exactly what makes §9d cheap.

</aside>

---

## 10. Security & jailing (host boundary)

- **Jailer wrapper:** chroot, PID/mount/network/user namespaces, cgroups (CPU/mem/IO/PID limits), drop to unprivileged uid/gid, `--resource-limit` style fd/file caps.
- **seccomp-BPF** via `seccompiler` — minimal per-thread syscall allowlists for vCPU vs device threads. The guest pokes virtio queues = the VMM processes attacker-controlled data with `unsafe` Rust, so a tight syscall filter is essential.
- **Memory isolation by KVM** — guest "physical" memory is just mapped pages in the VMM process; the guest can't read host memory or other VMs.
- **Network policy is host-side only** (Section 8) — guest cannot reach or alter it.

---

## 11. Implementation roadmap (phased)

| Phase | Deliverable | Key crates / work |
| --- | --- | --- |
| 0 — Spike | Boot a Linux kernel to a serial shell | <code>kvm-ioctls</code>, <code>vm-memory</code>, <code>linux-loader</code>, <code>vm-superio</code> serial; fork <code>vmm-reference</code> and prune |
| 1 — Devices | virtio-blk (rootfs + data volume) and virtio-net (tap) | <code>virtio-queue</code>, <code>vm-virtio</code>, <code>vm-device</code>, <code>event-manager</code> |
| 2 — Isolation | Per-VM netns + nftables default-deny egress + jailer + seccomp | netns/tap orchestration, <code>seccompiler</code>, cgroups |
| 3 — Snapshot/Restore | Full snapshot + eager restore; then UFFD lazy restore | <code>Persist</code> traits, state file + CRC, userfaultfd handler process |
| 4 — Suspend/Resume + Clones | Pause/serialize/resume; clone N from one snapshot; CoW disk overlays + netns rewrite | overlay/reflink storage, clone networking |
| 5 — Live snapshot | Dirty-bitmap diff snapshots → dirty-ring + write-protect background snapshot | <code>KVM_GET_DIRTY_LOG</code>, <code>KVM_CAP_DIRTY_LOG_RING</code>, UFFD write-protect |
| 6 — Egress hardening + perf | eBPF/XDP fast path, DNS-aware allowlists, rate limiters, CPU templates, warm pools | eBPF, nftables sets, token buckets |
| 7 — Live migration (cross-host) | Pre-copy + post-copy migration over mTLS; storage + network continuity; CPU-template negotiation | reuse dirty-ring / UFFD / Persist; remote page server, transport channel, handshake |

### Phase details (goal · deliverables · exit criteria · tests)

**Phase 0 — Spike: boot to serial shell**

- *Goal:* prove the KVM + loader + memory + serial path on real hardware.
- *Deliverables:* forked & pruned `vmm-reference`; CLI that boots a `vmlinux` + initramfs to a serial login; first CI KVM runner online.
- *Exit criteria:* kernel reaches a userspace shell deterministically; boot-to-init measured.
- *Tests:* 12.1 boot-param golden tests; 12.2 boot smoke test.

**Phase 1 — Devices: virtio-blk + virtio-net**

- *Goal:* attach a rootfs + data volume and a tap-backed NIC over virtio-mmio.
- *Deliverables:* virtio-queue/device wiring; in-VMM block backend; tap virtio-net; event-manager I/O loop.
- *Exit criteria:* guest mounts a volume and read/writes survive reboot; guest gets network via tap.
- *Tests:* 12.1 descriptor-parse + fuzz; 12.2 storage integrity + connectivity.

**Phase 2 — Isolation: netns + egress + jailer + seccomp**

- *Goal:* host-enforced default-deny egress + process confinement.
- *Deliverables:* per-VM netns/tap orchestration; nftables default-deny + allowlist; jailer (chroot / namespaces / cgroups / uid drop); per-thread seccomp filters.
- *Exit criteria:* guest reaches only allowlisted destinations; no in-guest action alters host policy; disallowed syscalls killed.
- *Tests:* 12.2 egress-denial (security-critical); 12.6 seccomp + jailer escape.

**Phase 3 — Snapshot / Restore**

- *Goal:* full snapshot + restore, then UFFD lazy restore.
- *Deliverables:* `Persist` traits on all devices; CRC'd state file; mem-file dump; UFFD page-fault handler process.
- *Exit criteria:* state continuity for in-flight compute; restore latency under budget with UFFD.
- *Tests:* 12.1 save/restore round-trip; 12.2 snapshot/restore correctness; 12.5 restore-latency gate; 12.6 snapshot tampering.

**Phase 4 — Suspend/Resume + Clones**

- *Goal:* pause/serialize/resume; clone N-from-1.
- *Deliverables:* CoW disk overlays (reflink / thin-LVM); clone netns/IP rewrite; warm-pool primitive.
- *Exit criteria:* N clones run independently with unique IPs and no disk cross-talk.
- *Tests:* 12.2 clone fan-out + storage-overlay isolation.

**Phase 5 — Live snapshot**

- *Goal:* background snapshot with a sub-ms–few-ms pause.
- *Deliverables:* dirty-bitmap diff snapshots → dirty-ring + write-protect background snapshot.
- *Exit criteria:* consistency harness passes under load; pause window < target across dirty rates.
- *Tests:* 12.3 full live-snapshot suite (consistency, dirty-tracking accuracy, convergence, diff equivalence).

**Phase 6 — Egress hardening + perf**

- *Goal:* production egress and the full performance budget.
- *Deliverables:* eBPF/XDP fast path; DNS-aware allowlists; rate limiters; CPU templates; warm pools.
- *Exit criteria:* all §2 perf gates green; red-team egress suite green.
- *Tests:* 12.5 perf gates; 12.6 egress-bypass red-team.

**Phase 7 — Live migration (cross-host)**

- *Goal:* pre-copy + post-copy migration between hosts.
- *Deliverables:* mTLS page/state transport; remote UFFD page server; CPU-template negotiation; storage (shared-backing + storage-migration) + network continuity.
- *Exit criteria:* round-trip equivalence; blackout < target with post-copy fallback; clean refusal on incompatible hosts; no split-brain on failure.
- *Tests:* 12.4 full live-migration suite; 12.5 migration-timing gate.

---

## 12. Automated testing plan

### 12.0 Test environment & provisioning (can we provision via the AWS CLI?)

**Yes — the entire KVM test fleet can be provisioned from the AWS CLI (`aws ec2 run-instances` / `terminate-instances`, or Terraform/CDK).** KVM only needs hardware-virtualization extensions exposed to the OS, which on EC2 means one of two options:

- **Bare-metal instances (`.metal`)** — the classic path; the OS runs directly on hardware and `/dev/kvm` works natively. Use `c6i.metal` / `m6i.metal` / `c5.metal` (x86_64) and Graviton `c7g.metal` / `c6g.metal` (aarch64). Highest fidelity for perf numbers; pricier and slower to launch.
- **Nested virtualization on virtual instances (new, since Feb 2026)** — AWS now exposes `vmx` to non-metal instances on the **C8i / M8i / R8i** families, enabled per-instance via `CpuOptions` (no extra charge). Cheaper and faster to spin up for functional CI, but treat its perf numbers as indicative — publish authoritative latency/throughput from bare metal.

**Recommended setup:**

- **Functional / unit / integration / security suites (12.1–12.3, 12.6):** nested-virt C8i/M8i/R8i instances, scripted up/down per CI batch — cheap and fast.
- **Perf gates (12.5):** dedicated bare-metal `.metal` hosts, warm-pooled with pinned CPUs — the source of truth for the §2 budget.
- **Live-migration matrix (12.4):** **two hosts** in the same VPC/subnet + a **cluster placement group** for low-latency, high-bandwidth links.
    - *Network cutover:* EC2 subnets aren't a true shared L2 and block MAC/IP spoofing, so test the gratuitous-ARP path over an **overlay (VXLAN/Geneve)** between hosts, and the L3 path via **secondary ENI / IP reassignment**.
    - *Shared-backing storage mode:* **EBS io2 Multi-Attach** (same-AZ, up to 16 instances) or **EFS/FSx (NFS)**; test the storage-migration mode against **local NVMe instance storage** (no shared disk).
- **Matrix:** x86_64 (Intel/AMD) + aarch64 (Graviton metal); plus a guest-kernel-version matrix.
- **Cost control:** keep 1–2 warm metal hosts for PR gating; run the 2-host migration + soak matrix nightly; automate teardown so metal hours aren't left running.

<aside>
✅

Provisioning is fully CLI/IaC-automatable. The net change vs. a year ago: you're no longer forced onto bare metal for functional CI — C8i/M8i/R8i nested virt covers correctness cheaply, while `.metal` remains the authority for the performance budget.

</aside>

### 12.1 Unit tests (per crate / module)

- Boot param / zero-page construction (golden-byte comparisons against known-good layouts).
- virtio-queue descriptor parsing: malformed/looping/overlapping descriptor chains must be rejected (fuzz-seeded).
- Device `save`/`restore` round-trip: serialize → deserialize → assert structural equality for every device.
- Rate-limiter token-bucket math (burst, refill, starvation).
- Egress policy compiler: allowlist spec → expected nftables/eBPF program (table-driven).

### 12.2 Integration tests (full microVM)

- **Boot smoke test:** kernel → guest agent reports "ready"; assert exit codes and console markers.
- **Storage:** write a known pattern to a volume inside the guest, snapshot/detach/reattach, verify integrity (checksums); verify CoW overlay isolation between clones.
- **Networking — connectivity:** allowed CIDR/domain reachable; assert success.
- **Networking — egress denial (security-critical):** from inside the guest, attempt to reach non-allowlisted IPs/ports, raw sockets, alternate DNS, IP spoofing, and ARP tricks — **all must fail**. Attempt to modify routes/iptables inside the guest and prove host policy is unaffected.
- **Snapshot/restore correctness:** snapshot a VM with in-progress compute (e.g., a counter + open file + TCP socket), restore, assert continuity.
- **Clone fan-out:** restore N clones from one snapshot; assert unique IPs, no disk cross-talk, independent execution.

### 12.3 Live-snapshot correctness (the hard one)

- **Memory-consistency harness:** guest runs a workload that continuously writes a verifiable structure (e.g., a Merkle log / checksummed ring buffer). Take a live snapshot under load; restore; verify the restored memory is a consistent point-in-time image (no torn pages).
- **Dirty-tracking accuracy:** instrument known write patterns; assert the dirty set reported by bitmap and by dirty-ring both cover exactly the written pages (no misses → silent corruption; false positives only cost perf).
- **Convergence under write pressure:** dirty-rate generator; assert the pre-copy loop converges and the final pause window stays under target (e.g., <5 ms) across low/medium/high dirty rates; assert graceful fallback (auto-converge / throttle) when dirty rate exceeds copy bandwidth.
- **Diff-snapshot equivalence:** base + sequence of diffs, when applied, must reproduce a full snapshot byte-for-byte.

### 12.4 Live-migration correctness (cross-host)

- **Round-trip equivalence:** migrate a VM mid-computation (counter + open file + live TCP connection) A→B→A; assert state continuity and that the TCP connection survives cutover.
- **Convergence vs downtime:** sweep guest dirty rates; assert pre-copy converges and the blackout stays under target, with automatic switch to post-copy when dirty rate exceeds link bandwidth.
- **Post-copy fault correctness:** kill convergence early, force demand-paging over the network; assert no missing/torn pages and bounded fault latency.
- **CPU/feature mismatch rejection:** attempt migration to a host with an incompatible CPU template; negotiation must refuse cleanly with no half-migrated state.
- **Failure / rollback:** drop the network mid-migration; the source must remain authoritative and resume cleanly (no split-brain, no double-run).
- **Storage modes:** validate both shared-backing and storage-migration paths preserve disk integrity (checksums) across cutover.
- **Network continuity:** assert egress policy + IP are re-applied on the destination and traffic resumes within target.

### 12.5 Performance & regression gates (run in CI, fail on regression)

- Cold-boot-to-init latency (p50/p99) vs budget (<125 ms).
- Restore latency: eager vs UFFD; p50/p99.
- Live-snapshot guest pause window distribution.
- Live-migration total time + blackout window (p50/p99) vs target, across dirty rates.
- microVM creation rate (VMs/sec/host) and per-VM memory overhead (a "memory cop" that fails the build if overhead exceeds threshold, à la Firecracker).
- virtio-blk/net throughput and IOPS with rate limiter on/off.
- Track all metrics over time; **block merges on statistically significant regressions.**

### 12.6 Security & fault-injection tests

- **seccomp coverage:** attempt disallowed syscalls from vCPU/device threads; assert the process is killed/denied.
- **Jailer escape attempts:** verify chroot/namespace/cgroup confinement; resource-limit enforcement (fd exhaustion, fork bombs, memory pressure).
- **Malicious-guest fuzzing:** fuzz virtio queues from inside the guest (the attacker-controlled boundary) — long-running, ASan/Miri where feasible; the VMM must never crash the host or corrupt other VMs.
- **Snapshot tampering:** corrupt state file / flip bits → restore must detect (CRC) and refuse, not execute in an invalid state.
- **Egress bypass red-team suite** (CI-gated): the denial tests in 12.2 plus DNS rebinding, fragmented packets, IPv6 leakage, and clone IP-collision attempts.

### 12.7 CI infrastructure

- Reuse the **`rust-vmm-ci`** shared pipeline conventions (the rust-vmm crates standardize on it) for lint/clippy/coverage/unit gates.
- **Bare-metal or nested-KVM runners** are required — KVM integration and perf numbers can't be validated on emulation-only CI; provision hardware-virtualization-capable runners.
- Matrix across x86_64 and aarch64; kernel-version matrix for the supported guest kernels.
- Nightly long-haul: soak tests (thousands of boot/snapshot/restore cycles) to catch leaks and snapshot-size creep across repeated snapshot→restore cycles.

---

## 13. Key risks & pitfalls

- **Live-snapshot torn state** — the #1 correctness risk; mitigated by dirty-ring + write-protect and the consistency harness (12.3). Get dirty tracking right before claiming "live."
- **Disk/memory snapshot skew** — block backing files aren't in the memory snapshot; without CoW overlays + quiesce you get inconsistent restores. Treat disk capture as a first-class part of snapshot.
- **Snapshot-size growth across repeated snapshot/restore cycles** — observed in other systems; soak-test for it.
- **Egress leakage paths** — DNS, IPv6, raw sockets, clone IP collisions. Default-deny + explicit red-team suite.
- **Crate feature incompatibilities** — pin versions; track upstream rust-vmm (e.g., recent `kvm-bindings`/`kvm-ioctls` version bumps).
- **seccomp over-restriction** breaking on new kernels — version the filters and test per kernel.
- **Migration host-symmetry drift** — divergent kernel / CPU / VMM versions across the fleet break migration; enforce CPU templates to a common baseline, version the device `Persist` schema, and reject incompatible targets at negotiation rather than mid-stream.

---

## 14. Prior art to mine (don't reinvent)

- **`rust-vmm/vmm-reference`** — the closest starting skeleton; fork and delete.
- **Firecracker** — reference for snapshot format, `Persist` traits, UFFD restore, diff snapshots, rate limiters, jailer, network-for-clones. KVM-only, ~117k LoC.
- **Cloud Hypervisor** — reference for dirty-page tracking for migration and a broader device set (~153k LoC, KVM + MSHV).
- **libkrun / smolvm** — existence proof that a small team can ship a focused rust-vmm-based VMM; libkrun drives `/dev/kvm` via `kvm-ioctls`/`kvm-bindings` and bundles a guest kernel.

<aside>
🔗

This maps directly onto the sandbox primitives Tarit targets — sub-200 ms boot, per-sandbox KVM isolation, deny-by-default per-session/per-VM egress, volumes, and checkpoint/restore/clone.

</aside>