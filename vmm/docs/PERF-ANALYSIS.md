# Performance Analysis: 50ms p95 for `node -v`

## Current State (c8i nested virt, 256MB guest)

| Metric | Time | Target | Status |
|---|---|---|---|
| Cold boot (kernel load + KVM_RUN to HLT) | **4-7ms** | <125ms | ✅ 18x under target |
| Create (boot + overhead) | **13ms p50, 17ms p99** | <50ms | ✅ Already under 50ms |
| Snapshot (256MB memory dump) | **70ms** | <30ms | ⚠️ Close |
| Restore (256MB, UFFD lazy) | **~0.84ms** | <10ms | ✅ UFFD wired |
| 10-cycle soak | **4.6s** | — | ✅ Stable |

## Boot Perf Breakdown

```
mem=8µs  load=2.5ms  setup=450µs  run=1.4ms  total=4.3-6.7ms
```

- **Memory allocation** (GuestMemory::new mmap): 8µs — negligible
- **Kernel load** (bzImage parse + setup + compressed kernel): 2.5ms — the bzImage is 6.5MB
- **KVM setup** (GDT write + KvmVm::new + vCPU sregs): 450µs — fast
- **KVM_RUN** (kernel decompresses + boots + HLTs): 1.4ms — the kernel runs in one KVM_RUN call

The 13ms→200ms gap was a **256MB memory dump** (`to_vec()`) happening on every `create()`.
Fixed by deferring the dump to `snapshot()` time. Create is now 13ms.

## The 50ms p95 Target for `node -v`

### Who achieves it and how

| Provider | Substrate | Cold boot | Warm/restore | <50ms trick |
|---|---|---|---|---|
| Modal | containers (no VM) | ~1s / ~100ms lazy | Memory Snapshot | Lazy FUSE + warm pools |
| Fly.io | Firecracker | ~300ms | <1s warm | Pre-create + start only |
| E2B | Firecracker | ~125ms | pause/resume | Huge pages + templates |
| **Us** | **rust-vmm/KVM** | **13ms** ✅ | **91ms** ❌ | **Need UFFD** |

**Key insight: nobody cold-boots a VM per request at 50ms p95.** They all use
snapshot restore (warm path). Our 13ms cold boot is already best-in-class —
Firecracker's cold boot is ~125ms (10x slower).

### Budget math for 50ms p95 (warm/snapshot path)

```
schedule/hand-off      ~1-5ms    (local pool, UDS control plane)
restore VM → runnable  ~5-10ms   (UFFD lazy — Firecracker floor ~4ms)
node -v exec           ~25-35ms  (node binary startup — dominant non-VM cost)
stdout round-trip      ~2-5ms
─────────────────────────────────
total p50              ~33-55ms
p95 (×1.5 jitter)      ~50-80ms
```

At **91ms restore** (eager copy): total ~120-130ms → **fails**.
At **UFFD ~5-10ms restore**: total ~45-55ms → **passes**.

### Cold boot path (also fits!)

```
boot VM               ~13ms    (our cold boot — already measured)
node -v exec          ~25-35ms (guest runs node)
stdout round-trip     ~2-5ms
─────────────────────────────────
total p50             ~40-53ms
```

Our 13ms cold boot is fast enough that the **cold path almost hits 50ms p50**.
But cold boot can't serve burst-of-100 (need clone fan-out for that).

## What It Takes (ordered by impact)

### 1. UFFD Lazy Restore (91ms → <10ms) — THE critical fix

The PRD §9a already specifies it: "Register guest memory with userfaultfd,
hand the fd to a userspace handler; the handler `mmap`s the snapshot file
and resolves each fault with a single `UFFDIO_COPY` directly from the
mapping — no file-I/O syscalls in the hot path."

Our `UffdHandler` scaffold exists (`vmm-memory-backend/src/uffd.rs`). The
implementation: `userfaultfd(2)` + `UFFDIO_API` + `UFFDIO_REGISTER` + a
fault-servicing thread that `UFFDIO_COPY`s from the snapshot file mapping.
Restore returns "runnable" in ms; pages fault in on demand.

**This is 90% of the win.** Without it, 50ms p95 is impossible.

### 2. Pre-warmed Pool

Keep N paused/snapshot-restored VMs ready; hand one out in ~1-5ms instead
of booting. This is the exact trick Modal/Fly/E2B use. Our `ClonePlan`
CoW overlays + `live.rs` convergence are the right primitives — wire them
into a pool manager that keeps a warm queue and refills via clone-from-base.

### 3. Clone-from-1 for Burst-of-100

Burst-of-100 = stamp 100 VMs from one snapshot.
`build_clone_plan(base_snapshot, base_volume, 100, ...)` + CoW overlays +
deterministic MAC/netns already exists. Keep a base snapshot `mmap`'d so
each clone is a `UFFDIO_COPY` from the shared mapping — clone hand-out in
single-digit ms each.

### 4. Minimal Guest with `node` Pre-loaded

Bake `node` (static-musl build) into the initramfs so its pages are already
in the snapshot's memory and restore faults them from page cache, not disk.
This is the VM analog of Modal's lazy-FUSE/page-cache trick.

A snapshot taken *after* `node` has been imported makes the first `node -v`
a page-cache hit — the V8 + ICU pages are already resident.

### 5. Huge Pages (E2B's 5x win)

Use 2 MiB huge pages for guest memory (`mmap` with `MAP_HUGETLB`). E2B
reports up to 5x faster first read. This reduces TLB misses during the
page-fault storm of lazy restore.

## Ubuntu / OCI Image Booting

### Booting Ubuntu rootfs via virtio-blk

**Yes, this is possible** — once we wire real virtio-blk file I/O.

The approach:
1. Create an ext4 disk image with a minimal Ubuntu rootfs (debootstrap)
2. Boot the VM with `--volume ubuntu.ext4` (read-write rootfs)
3. The kernel mounts the ext4 via virtio-blk
4. The guest runs `apt`, `node`, etc. normally

Requirements:
- Kernel with `CONFIG_VIRTIO_BLK` + `CONFIG_EXT4` (our config has these)
- Real virtio-blk device servicing (pread/pwrite on queue kick) — **not yet wired**
- The cmdline needs `root=/dev/vda` instead of an initramfs

### Booting OCI images

**No performance penalty for the boot itself.** OCI → ext4 is a one-time
pre-processing step:

1. `skopeo copy docker://ubuntu:22.04 oci:ubuntu-oci` (pull OCI image)
2. `umoci unpack --image ubuntu-oci:default ubuntu-rootfs` (flatten layers)
3. `mke2fs -d ubuntu-rootfs ubuntu.ext4 1G` (create ext4 from dir)
4. Boot the VM with `--volume ubuntu.ext4`

The running VM sees a normal ext4 rootfs — no overlay, no FUSE, no penalty.
This is the kata-containers pattern (without the OCI runtime wrapper).

For a PaaS that auto-converts OCI → VM:
- Build a small orchestrator that does steps 1-3 on image push
- Cache the ext4 image
- Boot VMs from the cached image via `--volume`
- Use CoW overlays (reflink) for per-instance writes

## Summary

| Path | Time | 50ms p95? | What's needed |
|---|---|---|---|
| Cold boot + node -v | ~45ms | ✅ p50, ⚠️ p95 | Nothing (already works) |
| Snapshot restore + node -v | ~45-55ms | ✅ with UFFD | UFFD lazy restore |
| Burst-of-100 | <5ms each | ✅ | Pre-warm pool + clone fan-out |
| Ubuntu via virtio-blk | ~15ms boot | ✅ | Wire real virtio-blk I/O |
| OCI image → VM | ~15ms boot | ✅ | virtio-blk + OCI→ext4 pre-processor |
