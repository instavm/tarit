# Full Boot Problem — RESOLVED

## Summary

The kernel now **boots fully** on c8i nested virt with `--full-boot`.
The kernel reaches VFS mount, unpacks the initramfs, and panics only
because there's no root filesystem (expected). See `docs/journal/19-full-boot-fix.md`
for the full story.

## Root Causes Found and Fixed

### 1. Wrong MSR indices (fixed)

| MSR | Was | Now (matching Firecracker) |
|---|---|---|
| MSR_KERNEL_GS_BASE | 0xC0000100 | 0xC0000102 |
| MSR_LSTAR | missing | 0xC0000082 |
| MSR_CSTAR | missing | 0xC0000083 |
| MSR_SYSCALL_MASK | missing | 0xC0000084 |
| MSR_IA32_MISC_ENABLE | 0x1A4 | 0x1A0 = FAST_STRING |
| MSR_MTRRdefType | 0xC06 | 0x806 = (1<<11)\|6 |

### 2. Missing KVM_SET_IDENTITY_MAP_ADDR (fixed)

### 3. Wrong bzImage load address (fixed)

Was 0x200000, should be `code32_start` = 0x100000.

### 4. Missing E820 in zero page (fixed)

### 5. Missing init_size in boot_params (fixed)

Was 0x7ff (garbage), set to 0x4000000 (64 MiB).

### 6. Missing CR0 MP/NE bits (fixed)

FPU not detected without MP (bit 1) and NE (bit 5).

### 7. PVH vs LinuxBoot protocol (fixed)

`CONFIG_PVH` is NOT set in our kernel. The loader was writing
`hvm_start_info` (PVH) instead of `boot_params` (LinuxBoot).
Now detects PVH from ELF notes and falls back to LinuxBoot.

### 8. Initramfs overwritten by kernel (fixed)

Moved initramfs from `kernel_end + page_align` to fixed 0x8000000.

## Current State

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
Unpacking initramfs...
Freeing initrd memory: 336K
VFS: Cannot open root device "(null)" or unknown-block(0,0)
Kernel panic - not syncing: VFS: Unable to mount root fs on unknown-block(0,0)
```

## Why 0 KVM Exits Is Normal on Nested Virt

With in-kernel IRQCHIP on nested virt:
- HLT → L0 handles (no exit to L1)
- APIC timer → L0 handles (no exit)
- Serial PIO → L0 handles (no exit)

The kernel IS running — L0 handles everything internally.

## Remaining Work

1. **Initramfs /init** — the initramfs unpacks but no `/init` is found.
   Need a properly structured cpio with `/init` at the root.

2. **Serial capture** — serial output is coalesced by L0. Options:
   MMIO-based serial (virtio-console), or coalesced PIO ring from
   `kvm_run` mmap.

3. **virtio-mmio devices** — add virtio-blk/net for block + network.
