# 06 — Phase 0 boot spike (M6)

*Goal: wire the loader (M2) + memory (M3) + KvmVm (M5) + a serial device
(M4) into the `vmm run` binary path, and boot a `vmlinux` + initramfs to a
serial shell (PRD Phase 0).*

## What I did

Added a `boot` cargo feature to the `vmm` binary (forwarding to
`vmm-core/kvm`) and a `boot_on_kvm()` function, gated to
`target_arch = "x86_64" + target_os = "linux" + feature = "boot"`, that:

1. Builds the device list: one `VirtioMmio` transport stub per `--volume`
   at ascending MMIO addresses (0xd000_0000+), device_id = 2 (block). The
   full virtio-blk + queue wiring is M7; for the spike the guest just sees
   a transport.
2. Creates the `KvmVm` (opens `/dev/kvm`, `KVM_CREATE_VM`, registers guest
   memory, installs the MMIO bus) via the M5 API.
3. Creates vCPU 0, logs the entry point + zero-page address.
4. Calls `vm.run_vcpu(&mut vcpu)` — the M5 run loop that dispatches MMIO
   exits through the bus.

The `run` subcommand now flows: parse CLI → build `GuestMemory` → load
kernel (M2) → [`boot_on_kvm` if feature enabled] → run loop. On non-Linux
or without the `boot` feature, it stops at "loaded" and logs that the
integration test needs a real Linux+KVM host.

The integration test (`crates/vmm-integration/boot_smoke.rs`) stays
`#[ignore]`'d — it needs a real Linux+KVM host (per the M0 setup decision)
to actually boot `guest/vmlinux` + `guest/initramfs.cpio.gz` to a login
shell.

## What worked

- **The whole boot path is ~40 lines of composition.** `boot_on_kvm` wires
  three pre-built abstractions (GuestMemory, KvmVm, MmioBus) + the loader
  output. Nothing in M6 had to re-implement KVM setup, memory registration,
  MMIO dispatch, or kernel loading — it's all calls into M2/M3/M4/M5.
- **The `boot` feature keeps the macOS dev loop clean.** On macOS the
  binary compiles and the `run` subcommand loads the kernel (cross-checked
  on x86_64-linux via step 5); the actual `KvmVm` creation is gated behind
  `feature = "boot"` so it doesn't break the native build. On a real Linux
  host: `cargo run --features boot -- run --kernel ...`.

## What went wrong

### F1. `mem` unused on non-x86_64 (macOS arm64)

```
error: unused variable: `mem`
```

On macOS arm64, the `#[cfg(target_arch = "x86_64")]` load block is skipped,
so `mem` was unused. First fix was an ugly inline block that referenced
`&mem` in a log-statement-closure. Reverted and did it cleanly: the
non-x86_64 fallback path now logs `mem.size_bytes` in its warning. Trivial,
but the first attempt was bad enough to revert.

### F2. fmt: `KvmVm::new(...).map_err(...)` should be a single let-binding line

`cargo fmt` wanted the chained call on one line, not split. Trivial.

## What I learned

- **Gating the KVM boot path behind a `boot` feature (not just `cfg(os)`)**
  lets the binary stay a single target for both dev (macOS, no boot) and
  prod (Linux, `--features boot`). The alternative — two binaries — would
  double the build matrix.
- **"Spike" doesn't mean "skip the abstractions."** Phase 0 is sometimes
  written as throwaway code to "prove the path." Using the M2-M5 crates
  directly made the spike ~40 lines and *kept* it as the real boot path
  for M7. No throwaway code to delete later.
- **The integration test being `#[ignore]` is fine.** The code path is
  cross-type-checked; the runtime test waits for a real Linux host. The
  journal records this honestly rather than pretending the boot "works."

## Commands to reproduce

```sh
./ci/check.sh   # all 5 green; step 5 cross-checks the boot path on x86_64-linux

# On a real Linux+KVM host (not available here):
#   cargo run --features boot -- run \
#     --kernel guest/vmlinux \
#     --initramfs guest/initramfs.cpio.gz \
#     --mem-mib 256 --vcpus 1
#   cargo test --workspace --features kvm -- --include-ignored boot_smoke
```

## Next

`07-block-net.md` — Phase 1: real virtio-blk (file-backed, in-VMM backend)
+ virtio-net (tap-backed) + the event-manager epoll I/O loop. This is what
makes the spike useful (the guest can read/write a rootfs and talk to the
network).
