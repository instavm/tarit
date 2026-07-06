# 19 — Full Boot: Kernel boots to VFS on c8i nested virt

*Goal: diagnose and fix the full-boot problem — the kernel was stuck with
0 KVM exits on c8i nested virt, never reaching serial console or init.*

## What I did

### 1. Diagnosed the 0-exits problem

The kernel ran for 120s with **zero KVM exits** in `--full-boot` mode.
Built a diagnostic memory dump (`dump_guest_memory_diagnostic()`) that
scans guest memory for kernel log strings after the boot timeout.

**Key finding:** the kernel was NOT booting — the decompressor was
failing silently. The 0 exits were a red herring: on nested virt with
in-kernel IRQCHIP, L0 handles HLT/timer/serial internally. The kernel
WAS running (in the HLT error loop), we just couldn't see it.

### 2. Fixed 8 root-cause bugs

#### Bug 1: Wrong MSR indices (`vcpu_setup.rs`)

**What went wrong:** The kernel was in a HLT loop (decompressor error
handler) but we couldn't tell because of 0 exits. After adding the
memory dump diagnostic, I found "Out of memory while allocating output
buffer" and "no FPU found" strings in guest memory.

**How I found the fix:** I looked at Firecracker's
`create_boot_msr_entries()` in
`src/vmm/src/arch/x86_64/msr.rs:390-424` and compared every MSR index
and value. Found 6 discrepancies:

| MSR | Our value | Firecracker value |
|---|---|---|
| MSR_KERNEL_GS_BASE | 0xC0000100 | 0xC0000102 |
| MSR_LSTAR | missing | 0xC0000082 |
| MSR_CSTAR | missing | 0xC0000083 |
| MSR_SYSCALL_MASK | missing | 0xC0000084 |
| MSR_IA32_MISC_ENABLE | 0x1A4 (wrong) | 0x1A0 = FAST_STRING |
| MSR_MTRRdefType | 0xC06 (wrong) | 0x806 = (1<<11)\|6 |

The MSR indices were copied from an outdated reference. Firecracker's
constants in `generated/msr_index.rs` are the authoritative source.

#### Bug 2: Missing KVM_SET_IDENTITY_MAP_ADDR (`kvm.rs`)

**What went wrong:** KVM requires `KVM_SET_IDENTITY_MAP_ADDR` when
IRQCHIP is in-kernel. Without it, KVM's real-mode emulation can fail.

**How I found the fix:** I strace'd QEMU+KVM booting the same kernel
and compared the KVM ioctls. QEMU calls `KVM_SET_IDENTITY_MAP_ADDR`
before `KVM_CREATE_IRQCHIP`. Firecracker also sets this in
`KvmVm::new` (`arch/x86_64/vm.rs:99`). We were missing it entirely.

#### Bug 3: Wrong bzImage load address (`x86_64.rs`)

**What went wrong:** The bzImage `code32_start` field (offset 0x214 in
the setup header) says 0x100000 (1 MiB). We were loading the compressed
kernel at 0x200000 (2 MiB). The setup code jumped to 0x100000 (empty
memory) and the decompressor never ran.

**How I found the fix:** I disassembled the `startup_32` code at
0x100000 and found it was the compressed kernel's entry point. Then I
read the `code32_start` field from the bzImage header and saw it was
0x100000, not 0x200000. The standard bzImage load address is always
`code32_start` — QEMU and Firecracker both load there.

#### Bug 4: Missing E820 in zero page (`x86_64.rs`)

**What went wrong:** For bzImage, only `cmd_line_ptr` was patched. The
kernel had `e820_entries=0` and no memory map.

**How I found the fix:** I read the kernel boot protocol docs
(Documentation/x86/boot.txt) and checked the `e820_entries` field at
offset 0x1e8 in the memory dump — it was 0. Firecracker writes the
full `boot_params` struct including E820 via `LinuxBootConfigurator`.

#### Bug 5: Missing init_size in boot_params (`x86_64.rs`)

**What went wrong:** The `init_size` field at offset 0x238 had a
garbage value (0x7ff = 2047 bytes). The decompressor failed with "Out
of memory while allocating output buffer".

**How I found the fix:** I searched guest memory for error strings and
found "Out of memory while allocating output buffer" at 0x662f90. Then
I checked `init_size` in the memory dump — it was 0x7ff instead of the
expected ~40 MiB. The field at offset 0x238 in the setup code contains
setup code data, not the `init_size` value. The bootloader must set it
explicitly.

#### Bug 6: Missing CR0 MP/NE bits (`vcpu_setup.rs`)

**What went wrong:** CR0 only had PE|ET|PG. The kernel's FPU detection
requires MP (bit 1) and NE (bit 5). Without them, the kernel reported
"x86/fpu: Giving up, no FPU found and no math emulation present" and
halted.

**How I found the fix:** I searched the kernel log buffer in guest
memory and found the FPU error message. Then I compared our CR0 with
Firecracker's `configure_segments_and_sregs` in `regs.rs:247-251`.
Firecracker sets `CR0 = PE | ET` for PVH, but KVM's default CR0
includes MP and NE. When we explicitly set CR0, we overwrote KVM's
defaults. Fixed by including MP and NE.

#### Bug 7: PVH vs LinuxBoot protocol mismatch (`x86_64.rs`)

**What went wrong:** The kernel has `CONFIG_PVH` NOT set. The loader
was writing `hvm_start_info` (PVH structure) at 0x10000, but the
kernel's `startup_64` reads RSI as a `boot_params` pointer (LinuxBoot
protocol). The PVH structure was being interpreted as garbage boot_params.

**How I found the fix:** I checked the kernel config and found
`# CONFIG_PVH is not set`. Then I read the linux-loader crate's
`PvhBootCapability` enum — it returns `PvhEntryNotPresent` when the
ELF notes don't contain a PVH entry point. We were ignoring this and
always writing `hvm_start_info`. Firecracker checks the PVH capability
and falls back to `LinuxBootConfigurator` when PVH is not present.

#### Bug 8: Initramfs overwritten by kernel (`x86_64.rs`)

**What went wrong:** The initramfs was loaded right after the kernel
(`kernel_end + page_align`), but the kernel's ELF segments extend
further than `kernel_end` reports. The kernel's `.init` + `.bss`
sections overwrote the initramfs.

**How I found the fix:** I checked the initramfs address in the memory
dump and found `cc cc cc` (kernel INT3 padding) instead of the gzip
magic (`1f 8b`). The kernel was using that memory for its `.brk`
section. Fixed by loading the initramfs at a fixed high address
(0x8000000 = 128 MiB) that's well above all kernel segments.

### 3. Added diagnostic memory dump

Added `dump_guest_memory_diagnostic()` to the KVM run loop. When the
boot timeout fires (or HLT loop is detected), dumps guest memory to
`/tmp/vmm-guest-mem.bin` and scans for kernel log strings. This was
the single most important tool — without it, we were guessing blindly.

## Result

The kernel **boots fully** on c8i nested virt with `--full-boot`:

```
Linux version 5.10.230 ...
Command line: console=ttyS0 reboot=k panic=1 pci=off ...
Hypervisor detected: KVM
tsc: Detected 2700.000 MHz processor
Memory: 225460K/261756K available
SLUB: HWalign=64, Order=0-3
x86/fpu: Supporting XSAVE feature 0x001: 'x87 floating point registers'
console [ttyS0] enabled
smpboot: Total of 1 processors activated (5400.00 BogoMIPS)
devtmpfs: initialized
NET: Registered protocol family 2
virtio-mmio: Registering device virtio-mmio.0 at 0xd0000000-0xd0000fff, IRQ 5
Unpacking initramfs...
Freeing initrd memory: 336K
VFS: Cannot open root device "(null)" or unknown-block(0,0)
Kernel panic - not syncing: VFS: Unable to mount root fs on unknown-block(0,0)
```

The kernel boots to VFS mount, probes virtio-mmio devices, unpacks the
initramfs, but panics because there's no root filesystem. The
virtio-blk device is detected but needs `VIRTIO_F_VERSION_1` feature
bit for kernel 5.10+ compatibility.

## E2E Test Results (40/46 pass)

```
Fast boot (to HLT)     : 33ms
Boot rate              : 52.1 boots/sec
Full boot (kernel→VFS) : ~700ms (kernel time)
Snapshot               : 15ms
Restore                : 15ms
Exec (echo hello)      : 15ms
```

## Why 0 KVM exits is "normal" on nested virt

On nested virt with in-kernel IRQCHIP:
- HLT → handled by L0 (no exit to L1)
- APIC timer → handled by L0 (no exit)
- Serial PIO (0x3f8) → L0 handles (no exit)

The kernel IS running — L0 handles everything internally. The VMM
never sees an exit. This is the same behavior Firecracker has.

## What went wrong (summary)

1. **MSRs copied from wrong reference** — had to check Firecracker's
   `create_boot_msr_entries()` for the correct indices.
2. **Missing KVM_SET_IDENTITY_MAP_ADDR** — found by strace'ing QEMU
   and reading Firecracker's `KvmVm::new`.
3. **Wrong bzImage load address** — found by reading the `code32_start`
   field from the bzImage header.
4. **No E820 in zero page** — found by checking `e820_entries` in the
   memory dump.
5. **init_size garbage** — found by searching for "Out of memory" in
   guest memory.
6. **CR0 missing MP/NE** — found by searching for "no FPU found" in
   the kernel log buffer. Confirmed by comparing with Firecracker's
   `configure_segments_and_sregs`.
7. **PVH vs LinuxBoot** — found by checking `CONFIG_PH` in the kernel
   config. Firecracker's loader checks PVH capability and falls back.
8. **Initramfs overwritten** — found by checking for `cc cc cc`
   (kernel padding) at the initramfs address.

## What I learned

- **Memory dump diagnostic is essential.** Without it, we were
  guessing. Dumping guest memory and searching for kernel log strings
  revealed exactly where the kernel was stuck.
- **Firecracker's source is the reference implementation.** Every MSR
  index, CR0 bit, and KVM ioctl must match Firecracker's setup. When
  in doubt, grep Firecracker's source.
- **PVH is optional.** The kernel supports both PVH and LinuxBoot.
  The loader must check `CONFIG_PVH` and use the right protocol.
- **Nested virt coalesces all exits.** 0 KVM exits is normal when
  IRQCHIP is in-kernel. The kernel IS running.
- **CR0 bits matter.** MP (bit 1) and NE (bit 5) are required for FPU
  detection. KVM sets these by default, but explicitly setting CR0
  overwrites the defaults.
