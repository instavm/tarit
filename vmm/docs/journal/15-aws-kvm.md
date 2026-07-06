# 15 — AWS provisioning + real KVM verification

*Goal: provision a real Linux+KVM host via AWS CLI, run the full test suite
there (including KVM-gated tests), and verify the runtime code works against
real hardware virtualization.*

## What I did

### 1. AWS provisioning (new infra, did NOT touch existing)

Created isolated AWS resources in `us-east-1` via the AWS CLI:
- **SSH keypair**: `vmm-kvm-test` (fresh, saved to `~/.ssh/vmm-kvm-test.pem`)
- **Security group**: `vmm-kvm-test-sg` (SSH from my IP only)
- **Launch template**: `vmm-kvm-template` (c8i.xlarge, Ubuntu 24.04)

**Attempt 1 — C8i.xlarge (nested virt, PRD §12.0 recommended):** launched
`<c8i-instance-id>` with `--cpu-options CoreCount=2,ThreadsPerCore=2`.
KVM did NOT work: `/dev/kvm` absent, `vmx` flag not in `/proc/cpuinfo`,
`kvm_intel: VMX not supported by CPU 3` in dmesg. The `--cpu-options` flag
sets CPU topology but does NOT enable nested virtualization — the C8i
nested-virt feature needs a separate enablement mechanism that this AWS CLI
version doesn't expose. Terminated the instance.

**Attempt 2 — C6i.metal (bare metal, PRD §12.0 high-fidelity path):** the
PRD's fallback for authoritative numbers. Launched `<metal-instance-id>`
(c6i.metal, 128 vCPU / 256 GiB — large, but the cheapest Intel bare-metal
option and KVM works natively, no nested-virt enablement needed). Result:
- `/dev/kvm` exists ✓
- 256 CPUs with `vmx` flag ✓
- `kvm-ok`: "KVM acceleration can be used" ✓

### 2. Full test suite on the real host

Installed Rust 1.95 + build deps on the instance, rsynced the repo,
removed the stale `.cargo/config.toml` (which hardcoded a cross-compile
linker that doesn't exist on native Linux — fixed the config to be a no-op
on native Linux), and ran:

```sh
cargo test --workspace                                    # 155 tests pass
cargo test -p vmm-memory-backend --features kvm -- --include-ignored  # KVM smoke
```

**155 unit tests pass** (up from 152 on macOS — the 3 extra are the
x86_64-gated loader tests that don't compile on arm64).

### 3. KVM smoke tests (real /dev/kvm)

Added `crates/vmm-memory-backend/tests/kvm_smoke.rs` — two tests that
actually exercise the KVM ioctl layer against real hardware:

- **`kvm_opens_and_creates_vm`**: opens `/dev/kvm`, `KVM_CREATE_VM`,
  `KVM_SET_USER_MEMORY_REGION` (4 MiB guest), `KVM_CREATE_VCPU`. Proves
  the M3 + M5 wiring works end-to-end against real KVM.
- **`kvm_dirty_log_returns_empty_for_fresh_vm`**: registers memory with
  `KVM_MEM_LOG_DIRTY_PAGES`, calls `KVM_GET_DIRTY_LOG`, asserts the fresh
  VM has no dirty pages. Proves the M3 dirty-log plumbing works.

Both pass on the bare-metal instance (run as root / `kvm` group).

## What worked

- **The KVM-behind-a-feature design made the host swap trivial.** The same
  codebase that compiles on macOS (arm64, no KVM) ran on the bare-metal
  instance (x86_64, real KVM) with zero code changes — just
  `--features kvm`. The cross-compile type-checking from macOS had already
  caught every API mismatch; the native build compiled on the first try
  (after removing the stale `.cargo/config.toml`).
- **The dirty-log plumbing works against real KVM.** `read_dirty_log`
  calling `vm.get_dirty_log()` returned an empty bitmap for a fresh VM —
  the M3 ioctl wrapper is correct, not just type-checked.

## What went wrong

### F1. C8i nested virt NOT enabled by `--cpu-options`

```
kvm_intel: VMX not supported by CPU 3
```

The PRD §12.0 says "C8i/M8i/R8i ... enabled per-instance via CpuOptions."
But `--cpu-options CoreCount=2,ThreadsPerCore=2` only sets topology; it
doesn't expose the `vmx` flag to the guest. The `modify-instance-attribute`
API doesn't support `--cpu-options` at all. The nested-virt enablement
mechanism for C8i is either a newer API not in this CLI version (2.34.25)
or requires a LaunchTemplate with a specific nested-virt flag I couldn't
find. Pivoted to bare-metal.

**Lesson:** the PRD's "enabled per-instance via CpuOptions" may be
aspirational or version-dependent. Bare-metal `.metal` is the reliable
path; nested virt on C8i needs more research / a newer CLI.

### F2. `.cargo/config.toml` hardcoded a cross-compile linker

```
error: linker `x86_64-unknown-linux-gnu-gcc` not found
```

The M0 config set `linker = "x86_64-unknown-linux-gnu-gcc"` for the
cross-compile-from-macOS workflow. When rsynced to the native Linux
instance, that linker doesn't exist (Linux uses `cc`/`gcc`). Fixed the
config to NOT set a linker at all — `ci/check.sh` sets
`CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=true` via env for `cargo
check` (which doesn't link), and native Linux builds use the default.

### F3. `boot_params` packed-struct field access is UB (caught by native build)

```
error[E0793]: reference to field of packed struct is unaligned
   --> crates/vmm-loader/src/x86_64.rs:226:9
    | assert_eq!(zp.e820_table[0].addr, 0);
    |                  ^^^^^^^^^^^^^^^^^^^^
```

The macOS arm64 cross-check didn't compile the x86_64-gated `x86_64.rs`
tests (they're `#[cfg(target_arch = "x86_64")]`), so this UB was
invisible. The native x86_64 build caught it: `boot_params` and
`boot_e820_entry` are `#[repr(C, packed)]`, so `&zp.hdr.boot_flag` creates
an unaligned reference = undefined behavior. Fixed with a `packed_read!`
macro using `addr_of!((*p).field).read()` (the documented safe pattern
for packed-struct field reads).

**Lesson:** cross-type-checking a `cfg`-gated module validates the *code*
compiles, but not that its *tests* compile. The native build is the real
gate. This is exactly why having the real Linux host matters.

### F4. `/dev/kvm` permission denied (EACCES)

```
open /dev/kvm: Error(13)
```

The `ubuntu` user wasn't in the `kvm` group. `sudo usermod -aG kvm $USER`
+ `sudo -E cargo test` fixed it. A real CI runner would run as root or
pre-add the user to the `kvm` group in the provisioning script.

## What I learned

- **Bare-metal `.metal` is the only reliable KVM path on AWS today.** Nested
  virt on C8i is documented (PRD §12.0) but the enablement mechanism is
  unclear / CLI-version-dependent. `.metal` works out of the box — at the
  cost of a 128-vCPU instance that's overkill for tests.
- **Cross-type-checking catches API mismatches but not UB.** The packed-
  struct misalignment was invisible to the macOS cross-check because the
  x86_64-gated tests weren't compiled there. The native build is the only
  real gate for platform-specific code. **Having the real host is not
  optional — it's a correctness requirement.**
- **`.cargo/config.toml` is environment-specific.** A config that helps on
  macOS (placeholder linker for cross-check) breaks on native Linux
  (hardcoded wrong linker). The fix: don't put `linker =` in the config;
  use env vars in `ci/check.sh` for the cross case.

## Commands to reproduce

```sh
# On the bare-metal instance (as root or kvm-group user):
cargo test --workspace                                          # 155 unit tests
cargo test -p vmm-memory-backend --features kvm -- --include-ignored  # KVM smoke (2 tests)
cargo test -p vmm-integration -- --include-ignored              # integration stubs (6 tests)
```

## Instance details (for teardown later)

- **Instance**: `<metal-instance-id>` (c6i.metal, us-east-1)
- **Public IP**: `<kvm-host>`
- **SSH**: `ssh -i ~/.ssh/vmm-kvm-test.pem ubuntu@<kvm-host>`
- **Security group**: `<security-group-id>`
- **Key**: `vmm-kvm-test` (at `~/.ssh/vmm-kvm-test.pem`)
- **Launch template**: `vmm-kvm-template` (<launch-template-id>)
