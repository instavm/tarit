# 04 — Devices (M4)

*Goal: virtio-mmio transport (register layout), virtio-queue descriptor-chain
parsing with malformed-chain rejection (PRD §12.1 fuzz-seeded), real 16550
serial via `vm-superio`.*

## What I did

### 1. `virtio/regs.rs` — the virtio-mmio register map

The full register offset table from the virtio v1.x spec §4.2 (MAGIC_VALUE
at 0x000 reads "virt", DEVICE_ID at 0x008, STATUS at 0x07c, the QUEUE_*
family for descriptor/available/used ring addresses, QUEUE_NOTIFY at 0x070,
INTERRUPT_STATUS/ACK at 0x080/0x084). Plus the `MAGIC = 0x74726976`
constant and tests verifying the magic spells "virt" and that all offsets
are 4-byte aligned.

### 2. `virtio/transport.rs` — real `MmioDevice` impl

`VirtioMmio` now holds the per-device state (irq, device_id, vendor_id,
version, status bitfield, feature-bank selectors, queue_sel) and implements
`MmioDevice` — decoding reads at the spec offsets and writing the status/
queue-sel/notify registers. `QUEUE_NOTIFY` logs (M7 will wake the I/O
thread). Three tests: magic reads back as "virt", status round-trips
through the register, the `MmioBus` dispatches reads to the device.

### 3. `virtio/queue.rs` — descriptor-chain validation (host-agnostic)

The PRD §12.1 fuzz-seeded requirement: malformed/looping/overlapping
descriptor chains must be rejected. The host-agnostic core is
`validate_chain(head, read_desc)` — follows a chain, enforces max length
(32), detects loops (any index visited twice → `QueueError::Loop`), and
requires at least one writable buffer. Five tests cover: a valid 2-descriptor
chain, loop rejection, too-long rejection, no-writable rejection, and the
minimal single-writable-descriptor chain. This is the validation layer
virtio-queue's `pop_descriptor_chain` does internally; extracted here so
it's testable on macOS without KVM.

### 4. `serial.rs` — real `vm-superio` 16550 wiring

Replaced the M0 byte-counter stub with a real `vm_superio::serial::Serial`
backed by a `NoopTrigger` (scaffold IRQ — M7 swaps in an eventfd) and
`std::io::sink()` (output — M7 swaps in stdout). Wrapped in `Mutex` so the
device is `Send + Sync`. `raw_byte` forwards THR writes to the underlying
Serial. Two tests: it accepts bytes without panicking, and `Persist`
round-trips the LSR.

## What worked

- **Extracting chain validation from the KVM path.** The same pattern as M3's
  `from_kvm_log` — virtio-queue's `pop_descriptor_chain` does ring-walking
  internally, but the *validation invariants* (max length, no loops, has
  writable) are pure functions of a `read_desc(idx)` closure. Lifting them
  out means the PRD §12.1 fuzz-seeded tests run on macOS.
- **The `MmioBus` from M0 took the real `VirtioMmio` device with zero
  changes** — the `MmioDevice` trait + `MmioBus::insert` from M0 are exactly
  the right abstraction. The investment in the bus + Persist trait in M0
  keeps paying off.

## What went wrong

### F1. `MmioReadResult` is `Result<u64, _>` but I returned `u32`

```
error[E0308]: expected `u64`, found `u32`
```

The bus returns `u64` (to accommodate any register width); my transport
read `u32` values and forgot to widen. Fixed `Ok(val) → Ok(val as u64)`.

### F2. Test assertions compared `u64` against `u32` constants

Cascading from F1: the tests `assert_eq!(read, MAGIC)` where `MAGIC: u32`
and `read: u64`. Fixed by comparing against `MAGIC as u64`, and for the
"virt" byte check, casting the read back to `u32` before `to_le_bytes()`.

### F3. Unused `MmioError` import after F1's `Ok(val as u64)`

The transport no longer constructed `MmioError` values (it always returns
`Ok`), so the import was dead. Removed.

### F4. Unused `Write` import in serial.rs

`std::io::Write` was imported for the `vm_superio::serial::Serial<W: Write>`
bound, but vm-superio re-exports the bound; only `Sink` (the concrete sink
type) was needed. Removed `Write`.

### F5. fmt: import ordering (`Serial as VmSerial, NoEvents` → `NoEvents, Serial as VmSerial`)

`cargo fmt` alphabetizes `use` items. Trivial.

## What I learned

- **The M0 `MmioBus` + `MmioDevice` + `Persist` abstractions were the right
  investment.** Real virtio devices plug into the bus with zero trait
  changes; Persist round-trips just work. Designing these in M0 (before any
  real device existed) is what's making M4 fast.
- **virtio-mmio STATUS is at 0x07c, not near the other status-like fields.**
  The spec scatters registers; my first draft put STATUS at 0x070 (which is
  actually QUEUE_NOTIFY). The `status_is_at_0x7c_per_spec` test guards this.
- **`u64` as the bus read width** is the right call — it accommodates 8/16/
  32-bit registers uniformly, at the cost of `as u64`/`as u32` casts at the
  device boundary. The alternative (per-width methods) is more typed but
  explodes the trait.

## Commands to reproduce

```sh
./ci/check.sh   # all 5 steps green; 16 device tests (M0's 6 + M4's 10)
```

## Next

`05-vmm-core.md` — `vmm-core`: KVM VM + vCPU creation, CPUID/MSR templates,
the vCPU `KVM_RUN` loop, and MMIO exit dispatch into the `MmioBus` from M4.
