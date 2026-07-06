# 05 — VMM core: KVM VM + vCPU run loop (M5)

*Goal: `vmm-core` — KVM VM/vCPU creation, CPUID/MSR templates, the vCPU
`KVM_RUN` loop, and MMIO exit dispatch into the `MmioBus` from M4
(PRD §4, §5).*

## What I did

### 1. `cpu_template.rs` — CPUID/MSR templates (PRD §5, §9d)

`CpuTemplate { name, cpuid: Vec<CpuidEntry>, msr_clear: Vec<(u32, u64)> }`
with two constructors:
- `CpuTemplate::bare()` — no masking, expose the host's features.
- `CpuTemplate::masked_basic()` — zeroes CPUID leaf 7 subleaf 0's ECX/EDX
  (hides AVX512 extended features). The real Firecracker T2 masks dozens;
  this is the scaffold shape.

`by_name()` looks up a template for the migration negotiation (PRD §9d:
"reject incompatible targets at negotiation"). Four tests cover: bare has
no masking, masked_basic clears leaf 7, by_name finds/misses, and the
template serializes round-trip (so it can go over the migration channel).

### 2. `kvm.rs` — `KvmVm` + the run loop (Linux+KVM only)

`KvmVm::new(mem_size, mmio_devices, template)`:
1. Opens `/dev/kvm` via `Kvm::new()`.
2. `KVM_CREATE_VM` with type 0.
3. Builds the `GuestMemory`, registers each region via
   `KVM_SET_USER_MEMORY_REGION` (the M3 wiring).
4. Builds the `MmioBus` from the supplied `(MmioRange, MmioDevice)` pairs
   (the M4 abstraction).
5. Returns a ready-to-run VM (vCPUs created later).

`run_vcpu(vcpu_fd)` — the run loop:
- Calls `vcpu_fd.run()` → `VcpuExit`.
- `VcpuExit::MmioWrite(addr, data)` — assembles the little-endian bytes
  into a `u64`, dispatches to `mmio_bus.write(addr, val, len)`.
- `VcpuExit::MmioRead(addr, data)` — asks `mmio_bus.read(addr, len)`, writes
  the result back into the `&mut [u8]` data slice for KVM to complete the
  read on the next `KVM_RUN`.
- `VcpuExit::Hlt` → pause + return Ok.
- `VcpuExit::IoOut`/`IoIn` → log (M7 will dispatch to the serial/RTC at the
  port). Any other exit → log + stop.

## What worked

- **The M3 + M4 abstractions compose perfectly.** `KvmVm::new` takes a
  `Vec<(MmioRange, Box<dyn MmioDevice>)>`, builds the bus, and the run loop
  dispatches MMIO exits through that bus — exactly the Firecracker model,
  with zero glue code. The trait + bus investment from M0 paid off again.
- **`VcpuExit` is a clean enum** (MmioRead/MmioWrite/Hlt/IoOut/IoIn/...),
  not a struct with an `exit_reason` int + union. The match arms are typed;
  no manual `KVM_EXIT_MMIO` constant juggling. The kvm-ioctls 0.19 API is
  genuinely nicer than the older struct-based one vmm-reference used.

## What went wrong

### F1. `VcpuExit` API mismatch — no `exit_reason` / `mmio` fields

```
error[E0609]: no field `exit_reason` on type `VcpuExit<'_>`
error[E0609]: no field `mmio` on type `VcpuExit<'_>`
```

I'd written the run loop against the *old* kvm-ioctls API (a `VcpuRun` struct
with `exit_reason: u32` + a `mmio` union member). The modern 0.19 API is a
`VcpuExit` enum with `MmioRead(addr, &mut [u8])` / `MmioWrite(addr, &[u8])` /
`Hlt` / `IoOut` / `IoIn` variants. Rewrote the whole match block to pattern-
match the enum — cleaner than the struct version would have been. **Lesson:**
always read the actual enum/struct definition before writing a match against
it; kvm-ioctls has had several major API reshapes.

### F2. `CpuTemplate` with `&'static [...]` fields can't derive serde

```
error[E0277]: the trait bound `&[(u32, u64)]: Deserialize<'de>` is not satisfied
```

I made `CpuTemplate` a `const` with `&'static [CpuidEntry]` / `&'static
[(u32, u64)]` fields for "static fleet baselines." But serde can't
deserialize borrowed slices (no `'static` guarantee at decode time). The
migration negotiation (PRD §9d) needs to *receive* a template over the wire,
so the type must be fully owned. Switched to `Vec<CpuidEntry>` /
`Vec<(u32, u64)>` + `String` name, with `bare()` / `masked_basic()`
constructors. Added a serialize round-trip test to prove it's wire-safe.

### F3. `MemoryError` → `VmmError` conversion via `?` failed

```
error: `?` couldn't convert the error ... `From<MemoryError>` is not
implemented for `VmmError`
```

`VmmError::Memory(String)` is a stringly-typed variant, not a `#[from]` — I
didn't want to leak `MemoryError` across crate boundaries. The `?` operator
needs `From`. Fixed with explicit `.map_err(|e| VmmError::Memory(e.to_string()))?`.

### F4. fmt: `let mem = GuestMemory::new(x).map_err(...)?` should be one line

`cargo fmt` collapsed my two-line `let` + `map_err` into one. Trivial.

## What I learned

- **kvm-ioctls 0.19's `VcpuExit` enum is a major ergonomic win over the old
  struct+union.** Pattern-matching `MmioRead(addr, &mut [u8])` gives you the
  address and a typed mutable slice to fill in — no `mmio.phys_addr` /
  `mmio.data[i]` raw indexing. The match arms document themselves.
- **Owned types for anything that crosses a process boundary.** `&'static`
  is great for in-process constants but breaks serde; the migration channel
  (and the snapshot state file) both need owned `Vec`/`String`. The cost is
  one allocation per template; negligible.
- **The M0/M3/M4 layering is paying off.** `KvmVm::new` is ~30 lines of
  straight-line code that wires three pre-built abstractions (GuestMemory,
  MmioBus, CpuTemplate) together. Nothing in M5 had to re-implement memory
  registration, MMIO dispatch, or device wiring — it's all composition.

## Commands to reproduce

```sh
./ci/check.sh   # all 5 green; the cross-check (step 5) type-checks the
                # KvmVm + run_vcpu code against kvm-ioctls 0.19 on x86_64-linux
```

## Next

`06-boot-spike.md` — Phase 0: wire the loader (M2) + memory (M3) + KvmVm (M5)
+ a serial device (M4) into the `vmm run` binary path, and boot a
`vmlinux` + initramfs to a serial shell. The integration test (boot to
login) needs a real Linux+KVM host; the code path lands here and is
cross-checked.
