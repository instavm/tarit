# Build Journal — minimal rust-vmm microVM

A tutorial-style log of building a microVM manager from scratch with the
`rust-vmm` crates, following the PRD in this repo. Written in real time as
each milestone lands — honest about what worked and what didn't.

## Why this exists

Tarit needs a purpose-built, minimal VMM it fully controls — to hit
sub-second sandbox starts, dense per-host packing, snapshot/restore/clone, and
host-enforced egress — without inheriting the size and attack surface of a
general-purpose hypervisor. Building on `rust-vmm` gives memory-safe building
blocks so we only write our differentiators (snapshot/clone, live snapshot,
egress, migration).

The target (PRD §2): cold boot <125 ms, restore <10 ms, >100 microVMs/host/s,
<5 MiB VMM overhead/VM, live-snapshot pause <5 ms, migration blackout <5 ms.

## Reading order

| Post | Milestone | What it covers |
|---|---|---|
| [00 — Setup](journal/00-setup.md) | env | The "no KVM on macOS" problem; Colima attempt + why it failed (TCG, not nested virt); pivot to cross-compile type-checking |
| [01 — Repo init](journal/01-repo-init.md) | M0 | Workspace skeleton, crate graph, first `cargo check`, full failure log (19 failures) |
| [01b — vmm-reference fork decision](journal/01b-vmm-reference.md) | M1 | Why we did NOT vendor vmm-reference (too stale); study + cherry-pick configs instead |
| [02 — Loader](journal/02-loader.md) | M2 | `linux-loader`, zero page, cmdline |
| [03 — Memory backend](journal/03-memory.md) | M3 | `vm-memory` mmap, dirty bitmap, UFFD scaffolding |
| [04 — Devices](journal/04-devices.md) | M4 | MMIO bus, virtio-queue, serial, Persist trait |
| [05 — VMM core](journal/05-vmm-core.md) | M5 | KVM VM/vCPU, run loop, CPUID templates |
| [06 — Boot spike](journal/06-boot-spike.md) | M6 | Phase 0: boot to a serial shell |
| [07 — Block + net](journal/07-block-net.md) | M7 | Phase 1: virtio-blk + virtio-net |
| [08 — Isolation](journal/08-isolation.md) | M8/M9 | Phase 2: netns, nftables, jailer, seccomp |
| [09 — Snapshot/Restore](journal/09-snapshot.md) | M10 | Phase 3: Persist, CRC, UFFD lazy restore |
| [10 — API](journal/10-api.md) | M11 | Control plane over UDS |
| [11 — Clones](journal/11-clones.md) | M12 | Phase 4 scaffold: suspend/resume + clones |
| [12 — Live snapshot](journal/12-live-snap.md) | M13 | Phase 5 scaffold: dirty-ring + write-protect |
| [13 — Egress + perf](journal/13-egress-perf.md) | M14 | Phase 6 scaffold: eBPF/XDP, DNS, CPU templates |
| [14 — Migration](journal/14-migration.md) | M15 | Phase 7 scaffold: mTLS, remote UFFD page server |
| [15 — AWS + KVM](journal/15-aws-kvm.md) | env | Provisioned c6i.metal via AWS CLI; 155 tests + 2 KVM smoke tests pass on real hardware |
| [16 — Boot attempt](journal/16-boot-attempt.md) | boot | vCPU sregs + bzImage loading + serial capture; KVM_EXIT_INTERNAL_ERROR at 32→64 transition (needs tracing) |
| [17 — Boot success](journal/17-boot-success.md) | boot | **KERNEL BOOTS on c8i nested virt** via 32-bit bzImage entry; root cause was KvmVm::new creating empty GuestMemory |
| [18 — CLI polish](journal/18-cli-polish.md) | cli | CLI flags, API polish, OCI pull |
| [19 — Full boot fix](journal/19-full-boot-fix.md) | boot | **Full boot to VFS**: fixed 6 root-cause bugs (MSRs, CPUID, CR0, E820, init_size, PVH vs LinuxBoot); kernel boots to initramfs unpack |
| [20 — Faithful resume](journal/20-faithful-resume.md) | resume | **snapshot → restore → running guest** (full vCPU state capture/restore, ~102ms); flushed out seccomp/glibc SIGSYS, pause() hang, mutex-poison cascade |

Each post records: **what I did**, **what worked**, **what went wrong**, and
**what I learned**.

For a chronological commit-by-commit outline to write a long-form blog post from,
see [build-itinerary.md](blog/build-itinerary.md). The finished post:
[building-a-minimal-rust-vmm.md](blog/building-a-minimal-rust-vmm.md).
