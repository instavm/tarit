# 01 — vmm-reference fork decision (M1)

*Goal: vendor `rust-vmm/vmm-reference`, prune to the minimal device set, pin
crate versions (PRD §3, §14 recommendation).*

## What I did

Cloned `rust-vmm/vmm-reference` at HEAD to study its structure and decide
what to vendor:

```sh
git clone --depth 1 https://github.com/rust-vmm/vmm-reference
```

## What I found — and why I did NOT vendor it

**vmm-reference is years stale.** Its `Cargo.toml` pins:

| Crate | vmm-reference | Our M0 workspace | Current crates.io |
|---|---|---|---|
| `kvm-ioctls` | 0.11 | 0.19 | 0.25 |
| `kvm-bindings` | 0.5 | 0.10 | 0.14 |
| `vm-memory` | 0.7 | 0.16 | 0.18 |
| `linux-loader` | 0.4 (git-patched!) | 0.14 | 0.14 |
| `vm-superio` | 0.5 | 0.8 | 0.8 |
| `event-manager` | 0.2 | 0.4 | 0.4 |
| `vmm-sys-util` | 0.8 | 0.12 | 0.15 |
| edition | 2018 | 2021 | — |

It even has a `[patch.crates-io]` pointing `linux-loader` at an old git rev
because "a version > 4.0 [hasn't been] published." (One has, long since.)

Forking this would mean:
1. A massive version-bump migration (kvm-ioctls 0.11 → 0.19 alone changes
   most of the API; vm-memory 0.7 → 0.16 is a rewrite of the traits).
2. Dropping `[patch.crates-io]` git deps in favor of published crates.
3. Migrating edition 2018 → 2021.
4. *Then* the "prune" the PRD describes.

Our M0 workspace **already** pins modern, published, compiling versions and
builds clean. Vendoring vmm-reference would be a regression. The PRD's
"fork and prune" advice (§3, §14) was written assuming vmm-reference was
current; it isn't.

## The pivot: study, don't vendor

vmm-reference is still **architecturally invaluable** as a reference. Its
total size confirms the PRD's claim ("the entire VM-create-and-run path is
only a few hundred lines"):

```
266 lines  src/vmm/src/boot.rs          ← x86_64 zero-page + E820 construction
1232 lines src/vmm/src/lib.rs           ← VMM core: VM create, vcpu run loop
450 lines  src/devices/src/virtio/mod.rs ← virtio-mmio transport + queue
214 lines  src/devices/src/legacy/serial.rs ← 16550 UART wiring
101 lines  src/devices/src/legacy/rtc.rs
 42 lines  src/devices/src/legacy/i8042.rs
```

So the plan for the remaining milestones: **keep our M0 skeleton, study
vmm-reference's implementations for each subsystem, and re-implement against
modern rust-vmm APIs.** This is effectively "build from individual crates"
(our original plan option B) but informed by vmm-reference's hard-won
knowledge of:
- The exact x86_64 boot constants (`KERNEL_BOOT_FLAG_MAGIC = 0xaa55`,
  `KERNEL_HDR_MAGIC = 0x5372_6448`, `EBDA_START = 0x9fc00`, `E820_RAM = 1`,
  `KERNEL_MIN_ALIGNMENT_BYTES = 0x0100_0000`) — these go into M2.
- The E820 memory map construction (RAM + MMIO gap) — M2.
- The virtio-mmio register layout and kick/interrupt path — M4/M7.
- The 16550 serial wiring (which IRQ, which I/O ports) — M4.

## What I did take from vmm-reference

The guest kernel configs are **timeless** (they're kernel Kconfig files, not
Rust code), so I cherry-picked them into `guest/kernel-configs/`:

- `microvm-kernel-initramfs-hello-x86_64.config` — the minimal config for
  the M6 boot spike (boots to an initramfs shell).
- `microvm-kernel-5.4-x86_64.config` — the full microvm config.
- `busybox_1_32_1_static_config` + `make_busybox.sh` — for building the
  tiny initramfs userspace.
- `make_kernel.sh`, `make_rootfs.sh`, `install_system.sh` — build scripts.

These run on a Linux host (not macOS), but they're ready for M6 when we get
to a real Linux+KVM box.

## What worked

- The decision to pin modern crate versions in M0 (instead of inheriting
  vmm-reference's) was validated. Our workspace compiles; vmm-reference's
  would need a multi-day migration before it could.
- vmm-reference's `boot.rs` head gave me the exact x86_64 boot constants and
  the E820 construction approach I'll need for M2 — no need to spelunk the
  kernel's `Documentation/x86/boot.txt` from scratch.

## What went wrong

### F1. Blindly following "fork and prune" would have wasted days

The PRD's strongest recommendation (§3: "The canonical way to wire them
together is the official `rust-vmm/vmm-reference` ... start there and
prune"; §14: "Fork and delete") is **wrong as of 2026** because vmm-reference
hasn't tracked the rust-vmm crate version bumps. I caught this only because
the first thing I did was `cat Cargo.toml` on the clone. If I'd started
copying code, I'd have hit a wall of API mismatches at the first `kvm-ioctls`
call. **Lesson:** always check a reference's dependency versions against
current before vendoring — "the canonical way" has a half-life.

## What I learned

- **"Fork and prune" assumes the fork is current.** It wasn't. The PRD is
  an architecture document, not a build manifest — its crate-name and
  version guidance has to be re-verified against the registry.
- **vmm-reference's value in 2026 is architectural, not code.** The boot
  constants, the E820 map shape, the device-wiring patterns, and the guest
  kernel configs are the durable assets. The Rust code is not.
- **Our M0 workspace *is* the modern fork.** By pinning current crate
  versions up front and laying out the crate graph to match the PRD, we've
  already done the "fork and prune" — just without the legacy code.

## Next

`02-loader.md` — re-implement `vmm-loader`'s kernel loading against modern
`linux-loader 0.14`, using vmm-reference's `boot.rs` as the reference for
x86_64 zero-page construction (boot params, E820 map, cmdline pointer,
initramfs placement). Golden-byte tests for the zero page (PRD §12.1).
