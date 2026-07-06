# 01 — Repo init & workspace skeleton

*Goal: a compiling, empty Rust workspace with the full crate graph laid out,
mirroring the PRD architecture. No KVM needed yet — this is pure structure.*

## What I did

### 1. `git init -b main` and basic repo hygiene

Initialized the repo, set local git identity. Wrote `.gitignore` covering
Cargo `/target`, editor files, and VM/snapshot artifacts (`*.img`, `vmlinux`,
`*.snap`, `*.mem`). Kept the PRD `.md` tracked (it's the source spec).

### 2. `AGENTS.md`

Recorded the toolchain, commands (`cargo check/build/test/clippy/fmt`), the
KVM-behind-a-feature convention, and the layout. This file is the contract
for anyone (human or agent) working in the repo.

### 3. The workspace `Cargo.toml`

A single `[workspace]` with `resolver = "2"` and **one curated block of
`[workspace.dependencies]`** pinning every rust-vmm crate version. PRD §3 is
explicit: "Pin crate versions and bind them through a single workspace ...
feature combinations across crates can become mutually incompatible, so a
curated lockfile is part of the architecture, not an afterthought."

Pinned set (see `Cargo.toml`):

| Crate | Version | Role |
|---|---|---|
| `kvm-bindings` | 0.10 (+`fam-wrappers`) | KVM structs |
| `kvm-ioctls` | 0.19 | `/dev/kvm` safe wrapper |
| `vm-memory` | 0.16 (+`backend-mmap`, `backend-atomic`) | Guest RAM |
| `linux-loader` | 0.14 | kernel/loader |
| `virtio-queue` | 0.17 | virtqueue |
| `vm-superio` | 0.8 | serial/RTC |
| `event-manager` | 0.4 | epoll loop |
| `seccompiler` | 0.4 | seccomp-BPF |
| `vmm-sys-util` | 0.12 | eventfd / tempfile / ioctl |

Plus supporting libs (`serde`, `nix`, `clap`, `bincode`, `crc32fast`).

### 4. Crate graph — 10 member crates + 1 binary

Laid out exactly as the PRD architecture (§4, §11) demands:

```
crates/
  vmm-core            KVM VM/vCPU, run loop, config, state machine
  vmm-memory-backend  vm-memory mmap, dirty bitmap, UFFD handler
  vmm-loader          linux-loader: kernel, zero page, cmdline
  vmm-devices         MMIO bus, virtio-mmio, blk, net, serial, Persist
  vmm-snapshot        Persist state file (CRC), mem file, full/diff
  vmm-net             tap, per-VM netns, nftables egress, rate limiter
  vmm-jailer          chroot, namespaces, cgroups, seccomp
  vmm-migration       mTLS transport, remote UFFD page server (scaffold)
  vmm-api             REST over UDS control plane
  vmm-integration     end-to-end tests (boot smoke, snapshot, egress)
src/
  main.rs             the vmm binary: CLI (run/restore/serve) + wiring
```

### 5. The KVM-behind-a-feature contract

Every crate that touches KVM exposes a `kvm` cargo feature and gates the
`kvm-ioctls`/`kvm-bindings` deps behind it. This is the PRD convention
("KVM is behind a trait so non-KVM logic is unit-testable on macOS") made
concrete: `cargo test --workspace` runs on macOS for everything *except*
KVM-dependent code, which is `#[ignore]`'d or feature-gated and runs only on
Linux+KVM.

### 6. First real code

Not just empty crates — each one has its core types sketched and unit-tested:

- `vmm-core`: `VmConfig` (serde), `VmState` lifecycle (with transition tests),
  `Vcpu`/`VcpuId`.
- `vmm-memory-backend`: `GuestMemory` (single-region mmap, with rejection of
  unaligned/zero sizes), `DirtyBitmap` (mark/mark_range/drain/merge, with
  tests), `UffdHandler` stub.
- `vmm-loader`: `default_cmdline()` (Firecracker-style), `build_cmdline()`
  with the 4096-byte x86 zero-page limit enforced and tested.
- `vmm-devices`: `MmioBus` (insert/read/write with overlap rejection and
  dispatch tests), `Persist` trait (with a `Counter` round-trip test),
  `Serial`/`VirtioBlk`/`VirtioNet` scaffolds implementing `Persist`.
- `vmm-snapshot`: `Snapshot` format (magic `"VMSN"`, version, flags, CRC'd
  state + mem), `create()` builder, `validate()` that rejects bad magic,
  bad version, and CRC mismatches (tampered-state test).
- `vmm-net`: `EgressPolicy` (default-deny, `allow_all`/`deny_all`),
  `TokenBucket` rate limiter (refill/consume/starvation/cap tests).
- `vmm-jailer`: `Jailer` config, `SeccompProfile` (vcpu vs device allowlists).
- `vmm-migration` / `vmm-api`: typed negotiation / RPC scaffolds.
- `src/main.rs`: a `clap` CLI with `run` / `restore` / `serve` subcommands
  that wire the crates and log what they'd do.

### 7. Integration tests

`crates/vmm-integration/` has `boot_smoke.rs`, `snapshot_roundtrip.rs`, and
`egress_denial.rs`, each with `#[ignore]`'d tests pointing at the milestones
that will fill them in (M6, M8, M10, M12).

## What worked

- The crate graph mirrors the PRD architecture exactly — one crate per
  architectural box in §4. Anyone reading the PRD can navigate the code.
- Unit tests for the non-KVM pieces (state machine, dirty bitmap, cmdline,
  MMIO bus, Persist round-trip, CRC, token bucket) all pass on macOS with
  no KVM. That's the design paying off immediately.
- `clap` CLI wires all the crates and gives a usable `vmm --help` from day
  one — good for morale and for the journal's "run it" step.

## What went wrong (full failure log, in order)

This milestone burned through **eleven** compile/test failures before it went
green. Recording every one — even the embarrassing ones — because that's the
point of this journal.

### F1. Wrong crate name: `virtio-device` does not exist on crates.io

```
error: no matching package named `virtio-device` found
```

The published crate is `virtio-devices` (plural). I'd guessed the name from
the PRD §3 table, which lists `virtio-device / virtio-bindings`. `cargo
search` revealed the truth; renamed the dependency in both the workspace
`Cargo.toml` and `vmm-devices/Cargo.toml`. **Lesson:** the PRD names crates
conceptually; always verify against `cargo search` / crates.io before pinning.

### F2. `[[bin]] path` pointing at the wrong location

```
error: can't find bin `vmm` at path `.../src/src/main.rs`
```

The binary package lives at `src/` (directory), so `path = "src/main.rs"`
resolved to `src/src/main.rs`. Fixed to `path = "main.rs"`. **Lesson:** when
the package directory *is* `src/`, the bin path is relative to the package
root, not to the conventional `src/`.

### F3. `event-manager` and `seccompiler` fail to compile on macOS

```
error[E0425]: cannot find value `SECCOMP_FILTER_FLAG_TSYNC` in crate `libc`
error[E0432]: unresolved import `vmm_sys_util::epoll`
```

Both crates use Linux-only APIs (`libc::prctl`, `libc::SYS_seccomp`, and
`vmm_sys_util::epoll` which was removed in v0.12). PRD §3 explicitly warns
about this: "feature combinations across crates can become mutually
incompatible." The fix that matches the PRD convention: **gate them behind
`[target.'cfg(target_os = "linux")'.dependencies]`** so the device/jailer
crates still compile on macOS for state-serialization tests, while the epoll
I/O loop and seccomp filters only build on Linux.

### F4. `MmioRange::contains` was a syntax error masquerading as a tautology

```rust
self.base..self.end() == self.base..self.end() && (self.base..self.end()).contains(&addr)
//                                        ^^ expected one of 7 possible tokens
```

I'd written a "defensive" check that compared two identical ranges with `==`,
which is both meaningless (ranges don't impl `PartialEq`) and a syntax error
(`..` can't appear in that position). Simplified to
`(self.base..self.end()).contains(&addr)`. **Lesson:** never write `a == a`-
style guards; they're dead code at best, broken at worst.

### F5. `dump_memory` referenced `GuestMemory::as_ptr()` which didn't exist

I wrote the snapshot dumper calling `guest.as_ptr()` before implementing the
method. Fixed by adding `as_ptr()` to `GuestMemory` that walks
`GuestMemoryMmap`'s region iterator.

### F6. `vm_memory::GuestMemory` trait not in scope → `.iter()` missing

```
error[E0599]: no method named `iter` found for struct `Arc<GuestMemoryMmap>`
```

`iter()` comes from the `GuestMemory` trait, not inherent on
`GuestMemoryMmap`. Added `use vm_memory::GuestMemory as _;` to bring the
trait into scope without polluting the namespace.

### F7. Wrong destructure of `GuestMemory::iter()`'s yield type

```
error[E0308]: expected `GuestRegionMmap`, found `(_, _)`
   --> .map(|(_, r)| r.as_ptr())
```

I assumed `iter()` yielded `(&GuestAddress, &GuestRegionMmap)` — it actually
yields `&Self::R` (i.e. `&GuestRegionMmap`) directly. Fixed the closure to
`.map(|r| r.as_ptr())`.

### F8. `GuestMemoryMmap::from_ranges` takes `&[(GuestAddress, usize)]`, not `Vec<GuestRegionMmap>`

I'd manually built a `MmapRegion` + `GuestRegionMmap` and tried to pass a
`Vec<GuestRegionMmap>`. The actual API takes a slice of `(GuestAddress, size)`
tuples and builds the regions internally. Simplified to
`GuestMemoryMmap::from_ranges(&[(GuestAddress(0), size_bytes as usize)])`.
**Lesson:** check the real signature before constructing; the "manual build"
path was unnecessary complexity that didn't match the API.

### F9. `vm_superio::Serial::default()` doesn't exist

```
error[E0599]: no function or associated item named `default` found for struct
`vm_superio::Serial<T, EV, W>`
```

`Serial` in vm-superio 0.8 is generic over a trigger + event + output sink
and has no `Default` impl. For the scaffold I replaced it with a plain
in-house `Serial` struct (byte counter + LSR byte); M7 will wire the real
vm-superio `Serial` with an IRQ trigger and stdout sink.

### F10. `vmm-loader` missing `Result` type alias

```
error[E0432]: unresolved import `crate::error::Result`
```

The `error.rs` module defined `LoaderError` but not a `Result<T>` alias.
Added `pub type Result<T> = std::result::Result<T, LoaderError>;`.

### F11. `vmm-core` missing `serde` dependency

```
error[E0432]: unresolved import `serde`
```

`vmm-core`'s `config.rs` and `state.rs` use `#[derive(Serialize,
Deserialize)]` but `serde` wasn't in the crate's `[dependencies]`. Added it.

### F12. `vmm-loader` lib.rs didn't re-export `default_cmdline` / `load`

```
error[E0425]: cannot find value `default_cmdline` in crate `vmm_loader`
error[E0425]: cannot find function `load` in crate `vmm_loader`
```

The binary called `vmm_loader::default_cmdline` and `vmm_loader::load`, but
`lib.rs` only re-exported `build_cmdline` and `LoadedKernel`. Added the two
missing re-exports.

### F13. `Persist` test failed to compile: `Deserialize` derive not resolving

```
error[E0277]: the trait bound `CounterState: serde::Deserialize<'de>` is not satisfied
help: the following other types implement trait `Deserialize<'de>`: ...
```

The test module did `use super::*` but the parent `persist.rs` didn't import
`serde::{Serialize, Deserialize}` — so the derive macros resolved but the
trait bounds didn't. Clippy's confused suggestion (`Deserialize → Serialize`)
was a red herring. Fixed by adding `use serde::{Deserialize, Serialize};` to
the test module.

### F14. `DirtyBitmap` test assertion wrong: `0x1000` and `0x1fff` are the same page

```
assertion `left == right` failed
  left: 1
 right: 2
```

The test marked `0x1000` and `0x1fff` expecting two PFNs — but both are in
page 1 (`0x1000 / 0x1000 = 1`, `0x1fff / 0x1000 = 1`). The code was correct;
the test assumption was wrong. Fixed the test to use `0x1000` and `0x2fff`
(genuinely different pages). **Lesson:** page-granularity math is easy to
fumble — always compute the PFN by hand before asserting.

### F15. Clippy: `Result<_, ()>` rejected (`result_unit_err`)

The `MmioBus::read`/`write` returned `Result<_, ()>`. Clippy (correctly)
demands a real error type. Introduced `MmioError` (Unmapped / Device) and
propagated it through `MmioDevice` trait.

### F16. Clippy: `manual_div_ceil`

```rust
let end = (gpa + len + PAGE_SIZE as u64 - 1) / PAGE_SIZE as u64;
```

Rust 1.95 has `u64::div_ceil`. Replaced with
`(gpa + len).div_ceil(PAGE_SIZE as u64)`.

### F17. Clippy: `manual_is_multiple_of`

```rust
size_bytes % 4096 != 0
```

Replaced with `!size_bytes.is_multiple_of(4096)` (also new in 1.95).

### F18. Clippy: `unnecessary_cast` on `as *const u8`

```rust
std::slice::from_raw_parts(guest.as_ptr() as *const u8, ...)
```

`as_ptr()` already returns `*const u8` (after F5/F7); the cast was redundant.

### F19. Clippy: unused `mut` on `Counter` test variable

`let mut c = Counter { n: 42 }` — `save()` takes `&self`, so `mut` was
unnecessary. Removed.

## What I learned

- **Design the crate graph before writing a line of logic.** Mapping PRD §4
  boxes to crates one-to-one meant every "where does X go?" question had an
  obvious answer, and the `[workspace.dependencies]` block forced me to
  resolve the rust-vmm version matrix up front instead of discovering
  incompatibilities during a late-night build.
- **The "KVM behind a feature" convention is trivial to set up on day one
  and painful to retrofit.** Worth doing in M0. The companion pattern —
  `[target.'cfg(target_os = "linux")'.dependencies]` for Linux-only crates
  like `event-manager` and `seccompiler` — is what actually makes the
  macOS dev loop work.
- **Persist from day one** (PRD §1) is just a trait with `save`/`restore` —
  cheap to land now, and it shapes how every device is written from here on.
- **`cargo search` is the source of truth for crate names**, not the PRD's
  conceptual table. `virtio-device` vs `virtio-devices` cost me a round trip.
- **Page-granularity math is the first thing to miscompute.** The dirty
  bitmap's `mark_range` rounding and the test's PFN assumptions both need
  hand-computed checks. This will matter even more in M13 (live snapshot
  dirty tracking).
- **Clippy on `-D warnings` from M0** catches a whole class of "it compiles
  but it's wrong" issues (unit error, manual ceil, redundant casts). Keep it
  on for every milestone.

## Commands to reproduce

```sh
cargo check --workspace          # should pass on macOS, no KVM needed
cargo test --workspace            # 28 unit tests pass on macOS, 6 KVM-gated ignored
cargo clippy --workspace --all-targets -- -D warnings   # clean
cargo fmt --all -- --check        # clean
```

## Next

`02-loader.md` — wire `linux-loader` for real: parse `vmlinux`/`bzImage`,
construct the x86 zero page, write the cmdline, and add golden-byte tests
against a known-good layout (PRD §12.1).
