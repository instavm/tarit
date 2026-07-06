# 03 — Memory backend (M3)

*Goal: wire `KVM_SET_USER_MEMORY_REGION` behind the `kvm` feature, add dirty-log
ioctl plumbing (`KVM_GET_DIRTY_LOG`), and flesh out the UFFD handler scaffold
(PRD §6, §9a, §9c).*

## What I did

Extended `vmm-memory-backend` with three new modules on top of the M0
`backend.rs` (mmap) + `dirty.rs` (pure-Rust bitmap):

### 1. `kvm.rs` — `KVM_SET_USER_MEMORY_REGION`

```rust
impl GuestMemory {
    pub fn register(&self, vm: &VmFd, flags: u32) -> Result<Vec<SlotId>, MemoryError>;
    pub fn register_with_dirty_logging(&self, vm: &VmFd) -> Result<...>;
}
```

Walks `GuestMemoryMmap`'s regions, builds a `kvm_userspace_memory_region`
struct for each (slot, guest_phys_addr, memory_size, userspace_addr = host
pointer), and calls `vm.set_user_memory_region()` (an `unsafe` ioctl — the
safety contract is that the host pointer is a valid mapping owned by the
caller, which our `GuestMemoryMmap` satisfies). `flags = KVM_MEM_LOG_DIRTY_PAGES`
enables the dirty bitmap for snapshot/migration.

### 2. `kvm_dirty.rs` — `KVM_GET_DIRTY_LOG` → `DirtyBitmap`

```rust
pub fn read_dirty_log(vm: &VmFd, mem: &GuestMemory, slots: &[u32])
    -> Result<DirtyBitmap, KvmDirtyError>;
```

For each registered slot, calls `vm.get_dirty_log(slot, region_len)`, which
returns a `Vec<u8>` — one bit per 4 KiB page. Walks the bytes, and for each
set bit, computes the corresponding GPA and marks it in a `DirtyBitmap`.
KVM resets its internal bitmap on read, so the next call captures only
writes since this one — exactly the diff-snapshot semantics (PRD §9a:
"the dirty bitmap resets on each snapshot").

### 3. `dirty.rs` — extracted `from_kvm_log` for host-agnostic testing

The bit-walking logic in `read_dirty_log` is pure arithmetic (byte array →
PFNs). I extracted it into `DirtyBitmap::from_kvm_log(region_start, log)` so
it's unit-testable on macOS without KVM. Three new tests cover: bit
decoding, non-zero region start, and long zero-run handling.

### 4. `uffd.rs` — real Linux scaffold

Fleshed out `UffdHandler` with cfg-gated fields (the userfaultfd raw fd +
the snapshot mapping pointer) and `register`/`serve` method stubs. Full
syscall wiring (userfaultfd(2) + UFFDIO_API + UFFDIO_REGISTER + fault
servicing thread doing UFFDIO_COPY) lands in M10.

## What worked

- **Extracting the KVM-log bit-walking into pure Rust.** The original design
  had the bit decoding inline in `read_dirty_log` (KVM-only). Moving it to
  `DirtyBitmap::from_kvm_log` meant three new tests run on macOS, validating
  the bit→PFN math without needing KVM. The KVM ioctl wrapper (`read_dirty_log`)
  is now a thin shell that calls the tested function.
- **Feature propagation.** `vmm-core`'s `kvm` feature now forwards to
  `vmm-memory-backend/kvm` via `kvm = ["...", "vmm-memory-backend/kvm"]` in
  `Cargo.toml`. So `--features vmm-core/kvm` activates the KVM code in both
  crates.

## What went wrong

### F1. The kvm feature wasn't propagating to vmm-memory-backend

This was the **most insidious failure of M3** — and it was a *process* bug,
not a code bug. My `check.sh` step 5 ran
`cargo check --features vmm-core/kvm`, which enables the `kvm` feature on
`vmm-core` but **does not** propagate it to `vmm-memory-backend` unless
explicitly declared. So the entire `kvm.rs` + `kvm_dirty.rs` modules (the
whole point of M3!) were silently **not being type-checked**. The cross-check
was green but vacuously — it wasn't actually checking the KVM memory code.

I only caught this when I manually ran
`cargo build -p vmm-memory-backend --target x86_64-unknown-linux-gnu --features kvm`
and it failed. Fix: added `"vmm-memory-backend/kvm"` to `vmm-core`'s `kvm`
feature in `Cargo.toml`, and updated `check.sh` to pass
`--features vmm-memory-backend/kvm` explicitly as well.

**Lesson:** `--features crate-a/feat` does NOT cascade through path
dependencies. Every crate that needs the feature must either be listed
explicitly or receive it via a forwarding declaration in the parent's
`[features]`. The cross-check being "green" is meaningless if the feature
isn't actually enabled on the crate you care about. Verify with
`cargo build -p <crate> --features <feat>` directly.

### F2. Missing `GuestMemoryRegion` trait for `.len()` / `.start_addr()`

```
error[E0599]: no method named `len` found for `&GuestRegionMmap`
help: trait `GuestMemoryRegion` which provides `len` is implemented but not in scope
```

`GuestMemoryBackend::iter()` yields `&Self::R` (= `&GuestRegionMmap`), but
`.len()` and `.start_addr()` are methods on the `GuestMemoryRegion` trait,
not inherent. Added `use vm_memory::GuestMemoryRegion;` to both `kvm.rs`
and `kvm_dirty.rs`. Same pattern as M2's `GuestMemoryBackend` import.

### F3. Missing `Address` trait for `.raw_value()`

```
error[E0599]: no method named `raw_value` found for `GuestAddress`
help: trait `Address` which provides `raw_value` is implemented
```

`GuestAddress::raw_value()` comes from the `Address` trait. Added
`use vm_memory::Address;` to both modules. vm-memory 0.18 puts nearly all
useful methods behind traits rather than inherent — you have to import each
one.

### F4. Dead-code warnings on `UffdHandler` fields

The `fd` and `snapshot_mapping` fields exist (cfg-gated to Linux) but the
`register`/`serve` methods that will use them are stubs in M3. Clippy
flagged both as dead code. Added `#[allow(dead_code)]` with a "Used in M10"
comment — the fields are part of the documented type shape and will be
exercised when the UFFD syscall wiring lands.

## What I learned

- **`--features crate/feat` doesn't cascade through path deps.** This is
  the kind of thing that makes a "green" CI quietly useless. Always verify
  that the feature is actually enabled on the specific crate whose code
  you're checking, with `cargo build -p <crate> --features <feat>`. The
  check script now passes `--features vmm-memory-backend/kvm` explicitly.
- **vm-memory 0.18 puts methods behind traits aggressively.** `iter` needs
  `GuestMemoryBackend`, `len`/`start_addr` need `GuestMemoryRegion`,
  `raw_value` needs `Address`. Importing them with `as _` keeps the methods
  reachable without polluting the namespace. Getting this right is a tax
  of the trait-heavy design; worth it for the abstraction, annoying in
  practice.
- **Extracting ioctl-result decoding into pure Rust is the pattern that
  makes KVM code testable on macOS.** The KVM ioctl returns bytes; the
  *interpretation* of those bytes (bit → PFN → GPA) is arithmetic. By
  putting the interpretation in a host-agnostic function, I get unit tests
  on macOS and a thin ioctl shell on Linux. I'll use this pattern again
  for the dirty ring (M13) and for CPUID/MSR state (M5).

## Commands to reproduce

```sh
./ci/check.sh
# 1/5 cargo check (native) — green
# 2/5 cargo test — 10 memory-backend tests pass on macOS (7 dirty bitmap
#       incl. 3 new from_kvm_log tests, 3 backend)
# 3/5 clippy — clean
# 4/5 fmt — clean
# 5/5 cross-check x86_64-linux + kvm feature on BOTH vmm-core AND
#       vmm-memory-backend — validates the KVM memory-region registration
#       and dirty-log ioctl wrappers (the F1 fix)
```

## Next

`04-devices.md` — `vmm-devices`: virtio-mmio transport (the register layout
a guest sees), virtio-queue descriptor chain parsing with fuzz-seeded
malformed-chain rejection (PRD §12.1), and the real 16550 serial + RTC via
`vm-superio`.
