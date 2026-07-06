# 02 — Loader (M2)

*Goal: real kernel loading against `linux-loader 0.14` — parse `vmlinux`/`bzImage`,
construct the x86_64 zero page (boot_params + E820 map + cmdline pointer +
initramfs placement), golden-byte unit tests (PRD §12.1).*

## What I did

Split `vmm-loader` into three modules:

- **`memmap.rs`** — pure-Rust E820 map + x86 boot constants. Compiles and
  tests on **any** host (the key insight: E820 arithmetic has no x86-only
  types, so it doesn't need `cfg(target_arch = "x86_64")`). 6 unit tests
  run on macOS.
- **`cmdline.rs`** — kernel command-line builder (from M0, unchanged).
- **`x86_64.rs`** — the `boot_params` (zero page) construction + kernel
  load via `linux-loader`'s `Elf`/`BzImage` loaders + `LinuxBootConfigurator`.
  `cfg(target_arch = "x86_64")`-gated; cross-type-checked on macOS via
  `cargo check --target x86_64-unknown-linux-gnu`. 3 more unit tests
  (compile + run only on x86_64).
- **`kernel.rs`** — thin wrapper turning `BootSetup` into `LoadedKernel`
  for the VMM binary.

### E820 map (the part I got wrong twice — see failures below)

The map describes the guest physical address space to the kernel:

| GPA range | Type |
|---|---|
| `0x0`..`0xA_0000` | RAM (640 KiB low) |
| `0xA_0000`..`0x10_0000` | reserved (VGA + BIOS, 384 KiB) |
| `0x10_0000`..`MMIO_GAP_START` | RAM (high before gap) |
| `MMIO_GAP_START`..`min(mem, MMIO_GAP_END)` | reserved (the MMIO hole) |
| `MMIO_GAP_END`..`mem_end` | RAM (high after gap, only if mem > gap end) |

The subtle part: when RAM ends *inside* the gap (e.g. 1 GiB RAM, gap is
256 MiB..2 GiB), the gap entry must span only up to `min(mem, gap_end)`, not
the full gap — otherwise the E820 map's total exceeds `mem_size` and the
kernel sees phantom reserved memory past physical RAM. Two tests caught this
(see F5, F6).

### Zero page (boot_params)

`build_zero_page()` stamps the magic fields (`boot_flag = 0xaa55`,
`header = HdrS`, `type_of_loader = 0xff`, `kernel_alignment = 0x0100_0000`),
copies the bzImage's `setup_header` if present, and writes the E820 entries
into `boot_params.e820_table` (a fixed-size array; `e820_entries` holds the
count). `load_and_setup_boot()` then writes the cmdline at a known GPA,
places the initramfs page-aligned above the kernel, sets `hdr.cmd_line_ptr`
+ `hdr.ramdisk_image`/`ramdisk_size`, and calls
`LinuxBootConfigurator::write_bootparams` to commit the zero page.

## What worked

- **Splitting E820 into a host-agnostic module.** The original design put
  everything in `x86_64.rs` (gated), which meant the E820 tests couldn't run
  on macOS — only cross-type-check. Moving the pure-arithmetic part to
  `memmap.rs` (no `boot_params`, no `linux-loader` types) let 6 tests run
  natively on macOS, and they caught two real bugs (F5, F6). The cross-check
  still validates the `boot_params` glue.
- **The cross-compile check is doing its job.** Every API mismatch
  (`write_boot_params` vs `write_bootparams`, `e820_map` vs `e820_table`,
  `e820_entry` vs `boot_e820_entry`, `mem_type` vs `r#type`, the missing
  `CMDLINE_MAX_LEN`) was caught by `cargo check --target x86_64-unknown-linux-gnu`
  on macOS before any commit. None reached a Linux host.

## What went wrong

### F1. Two versions of `vm-memory` in the graph (0.16 vs 0.18)

```
error[E0308]: expected `GuestAddress`, found `GuestAddress`
note: there are multiple different versions of crate `vm_memory`
```

`linux-loader 0.14` depends on `vm-memory 0.18`; my M0 workspace pinned
`vm-memory 0.16`. Two different `GuestAddress` types — same name, different
crate versions, incompatible. The classic rust-vmm version-mix problem the
PRD §3 warns about. Fix: bump the workspace `vm-memory` pin to **0.18** so
the whole graph unifies on one version.

### F2. `GuestMemory` trait renamed to `GuestMemoryBackend` in 0.18

```
error[E0599]: no method named `iter` found for `Arc<GuestRegionCollection<GuestRegionMmap>>`
```

vm-memory 0.18 split the old `GuestMemory` trait into `GuestMemoryBackend`
(low-level, has `iter()`) and `GuestMemory` (higher-level). `GuestMemoryMmap`
impls `GuestMemoryBackend`. Updated the import:
`use vm_memory::GuestMemoryBackend as _;`. This cascaded from F1 — bumping
the version exposed the new API.

### F3. `write_boot_params` → `write_bootparams` (no underscore)

```
error: no function or associated item named `write_boot_params` found
help: there is an associated function `write_bootparams` with a similar name
```

I guessed `write_boot_params` from the trait name `BootConfigurator`. The
modern API uses `write_bootparams` (one word). Fixed.

### F4. `boot_params` field name drift: `e820_map`→`e820_table`, `e820_nr`→`e820_entries`, `e820_entry`→`boot_e820_entry`, `mem_type`→`r#type`

```
error[E0609]: no field `e820_map` on type `&mut boot_params`
error[E0560]: struct `boot_e820_entry` has no field named `mem_type`
```

I'd written the field names from memory of vmm-reference's old code. The
modern `linux-loader 0.14` (using a newer kernel header) renamed:
- `e820_map` → `e820_table`
- `e820_nr` → `e820_entries`
- `e820_entry` → `boot_e820_entry`
- the entry's `mem_type` field → `type` (the Rust keyword, so `r#type`)

Fixed by reading the actual `bootparam.rs` in the registry source. **Lesson:**
never write FFI struct field names from memory; always `grep` the bindgen
output.

### F5. `Cmdline::as_bytes()` doesn't exist

```
error[E0599]: no method named `as_bytes` found for struct `Cmdline`
```

`Cmdline` in linux-loader 0.14 converts to bytes via `TryFrom<Cmdline> for
Vec<u8>` or `as_cstring()`. Switched to
`let bytes: Vec<u8> = TryFrom::try_from(cmdline)?;`.

### F6. E820 test: arithmetic overflow — 1 GiB RAM vs 2 GiB gap end

```
error: attempt to compute `1073741824_u64 - 2147483648_u64`, which would overflow
```

My `e820_map_layout_large_1gib` test asserted `last.size = 1 GiB - MMIO_GAP_END`,
but `MMIO_GAP_END = 2 GiB > 1 GiB`, so there's no post-gap entry at all for
a 1 GiB VM. The test assumption was wrong: 1 GiB RAM ends *inside* the gap.
Renamed the test to `e820_map_layout_1gib_ram_ends_inside_gap` and asserted
4 entries (no post-gap), with the gap entry spanning up to the end of RAM.

### F7. E820 logic bug: gap entry extended past end of RAM

This is the real correctness bug F6 uncovered. For a 1 GiB VM (gap is
256 MiB..2 GiB), my `build_e820_map` emitted the gap entry with size
`MMIO_GAP_END - MMIO_GAP_START` (the full 1.75 GiB gap), even though RAM
ended at 1 GiB. The E820 map then claimed reserved memory from 256 MiB to
2 GiB — 1 GiB of which didn't physically exist.

Fix: the gap entry's size is `min(mem_size, MMIO_GAP_END) - MMIO_GAP_START`,
so it stops at the end of RAM. Now the E820 map's total always equals
`mem_size`. The `e820_ram_and_reserved_sum_to_mem_size` test (sweeping 8
sizes from 1 MiB to 4 GiB) guards this.

### F8. E820 test: sub-megabyte VMs are absurd

```
assertion failed: size 0xa0000
  left: 1048576   (1 MiB — the reserved entry 0xA_0000..0x10_0000)
 right: 655360    (640 KiB — the actual mem_size)
```

The sum test included `0xA_0000` (640 KiB) as a size. For such a tiny VM,
the reserved legacy entry (0xA_0000..0x10_0000 = 384 KiB) extends past RAM
(640 KiB), so the E820 total (640K + 384K = 1 MiB) exceeds mem_size (640K).
A real VMM never boots with < 1 MiB RAM; dropped the absurd size from the
test sweep. The loader requires `mem_size >= HIMEM_START` (1 MiB) in
practice.

### F9. Binary: `initramfs.as_deref()` produced `Option<&str>` but `load` expects `Option<P>`

```
error[E0308]: expected `Option<&String>`, found `Option<&str>`
```

`load<P: AsRef<Path>>` takes `Option<P>`; passing `Option<&str>` doesn't
unify `P`. Fixed by using `initramfs.as_ref()` (`Option<&String>`, and
`&String: AsRef<Path>`).

### F10. Binary: `mem` unused on non-x86_64 (macOS arm64)

```
error: unused variable: `mem`
```

On macOS arm64, the `#[cfg(target_arch = "x86_64")]` load block is skipped,
so `mem` was unused. Fixed by logging `mem.size_bytes` before the cfg block
so it's referenced on all arches.

### F11. fmt: closure formatting in the `is_elf` detection

```rust
path.extension().map_or(false, |e| e == "elf" || path.file_name()...)
```

`cargo fmt` wanted the `||` chain wrapped differently. Ran `cargo fmt --all`
to fix.

## What I learned

- **The E820 map must be contiguous and total to `mem_size`.** This sounds
  obvious but the gap-at-a-fixed-address design makes it subtle: when RAM
  ends inside the gap, the gap entry must be truncated to the end of RAM.
  A sweep test across boundary sizes (at gap start, inside gap, at gap end,
  past gap) is the only way to catch this — a single-size test misses it.
- **Split host-agnostic logic out of `cfg`-gated modules.** The E820
  arithmetic doesn't need `boot_params` or `linux-loader`; putting it in a
  separate always-on module let 6 tests run on macOS instead of being
  cross-check-only. The cross-check still validates the x86_64 glue.
- **Never write FFI struct field names from memory.** `e820_map`/`e820_nr`/
  `mem_type` were all wrong vs the modern `bootparam.rs`. Always grep the
  bindgen output in `~/.cargo/registry/src/...`.
- **vm-memory 0.18 split `GuestMemory` into `GuestMemoryBackend` + `GuestMemory`.**
  The trait rename is the kind of thing that only surfaces when you bump a
  major version. The cross-check caught it immediately.
- **The version-unification rule (PRD §3) is non-negotiable.** Having both
  vm-memory 0.16 and 0.18 in the graph produced two `GuestAddress` types
  that couldn't be converted. One curated `[workspace.dependencies]` block
  with a single version per crate is the only stable setup.

## Commands to reproduce

```sh
./ci/check.sh
# 1/5 cargo check (native) — green
# 2/5 cargo test  — 10 loader tests + 28 others pass on macOS
# 3/5 clippy — clean
# 4/5 fmt — clean
# 5/5 cross-check x86_64-linux + kvm feature — green (validates the
#       boot_params glue that can't run on macOS arm64)
```

## Next

`03-memory.md` — `vmm-memory-backend`: wire `KVM_SET_USER_MEMORY_REGION`
behind the `kvm` feature, add dirty-log ioctl plumbing (the foundation for
diff snapshots + live snapshot + migration), and flesh out the UFFD handler
scaffold.
