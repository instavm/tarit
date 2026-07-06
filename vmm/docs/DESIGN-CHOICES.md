# Design Choices

This document records every design decision where alternatives existed,
why we chose what we chose, and what the alternatives were.

## Boot Protocol

### Choice: ELF vmlinux via LinuxBoot protocol (64-bit long mode entry)

**Alternatives:**
1. **bzImage via 32-bit protected-mode entry**: the kernel's
   `startup_32` handles the 32→64 transition internally.
2. **ELF vmlinux via PVH** (Xen PVH boot protocol): the kernel enters
   directly at the PVH entry point with `hvm_start_info` at RSI.
3. **ELF vmlinux via LinuxBoot**: the kernel enters at `startup_64`
   in 64-bit long mode with `boot_params` at RSI.

**Why LinuxBoot:** The kernel was built without `CONFIG_PVH`, so PVH
is not available. The ELF vmlinux path is preferred over bzImage
because it skips the decompressor entirely: the kernel is already
uncompressed. This matches Firecracker's approach (Firecracker uses
ELF vmlinux with PVH when available, LinuxBoot otherwise).

**Why not bzImage for full boot:** The bzImage decompressor fails on
nested virt: it runs but writes nothing to the decompression target.
The decompressor return value on the stack shows it returned normally,
but no data was written. This is likely a gzip/SSE issue in the
decompressor on nested virt. The ELF vmlinux path bypasses the
decompressor entirely.

**Why bzImage for fast boot:** Fast boot (no IRQCHIP) uses bzImage
because it's the quickest path to HLT: the kernel's `startup_32`
decompresses, transitions to long mode, runs a few initcalls, and
HLTs. This gives us 33ms cold boot times.

## vCPU Register Setup

### Choice: Match Firecracker's MSR table exactly

**Alternatives:**
1. **Host passthrough**: let KVM use its default MSRs.
2. **Safe minimal MSRs**: set only SYSENTER + STAR + TSC.
3. **Firecracker's full MSR table**: 11 MSRs matching
   `create_boot_msr_entries()`.

**Why Firecracker's table:** The kernel's FPU detection, syscall
setup, and MTRR initialization all depend on specific MSR values. We
tried minimal MSRs and got "no FPU found" and "Out of memory" errors.
Firecracker's table is the proven reference: it sets:
- SYSENTER_CS/ESP/EIP (0x174-0x176) = 0
- STAR/LSTAR/CSTAR/SYSCALL_MASK (0xC0000081-0xC0000084) = 0
- KERNEL_GS_BASE (0xC0000102) = 0
- TSC (0x10) = 0
- MISC_ENABLE (0x1A0) = FAST_STRING bit
- MTRRdefType (0x2FF) = write-back (0x806)

### Choice: CR0 = PE | MP | ET | NE | PG

**Alternatives:**
1. `CR0 = PE | ET | PG` (what we had: missing MP and NE)
2. `CR0 = PE | MP | ET | NE | PG` (Firecracker's effective value)

**Why include MP and NE:** The kernel's FPU detection checks CR0.EM
(emulation, bit 2) = 0 and requires CR0.MP (monitor coprocessor, bit 1)
and CR0.NE (numeric error, bit 5) for proper FPU exception handling.
Without NE, FPU errors go to the legacy INTR line instead of the
local APIC, causing the kernel to think there's no FPU.

## CPUID

### Choice: Host passthrough with x2APIC masked

**Alternatives:**
1. **Safe minimal CPUID**: mask off all extended features, keep only
   FPU/SSE/PAE/APIC.
2. **Host passthrough**: pass through all host CPUID features.
3. **Firecracker's `cpuid.normalize()`**: normalizes topology, APIC
   ID, and feature bits.

**Why host passthrough:** The safe minimal CPUID caused triple-faults
because it masked MTRR and PAT, which the kernel's early boot code
requires. Host passthrough gives the kernel all the features it
expects. We mask only x2APIC (ECX[21]) because it causes issues on
nested virt.

**Why not `cpuid.normalize()`:** Firecracker's normalization is
complex (sets APIC ID, topology, masks features based on CPU template).
For our minimal VMM, host passthrough is simpler and works. We can add
normalization later if needed for snapshot portability.

## KVM IRQCHIP

### Choice: In-kernel IRQCHIP (KVM_CREATE_IRQCHIP)

**Alternatives:**
1. **In-kernel IRQCHIP** (KVM_CREATE_IRQCHIP): PIC, IOAPIC, LAPIC all
   in kernel.
2. **Split IRQCHIP** (KVM_CAP_SPLIT_IRQCHIP): LAPIC in kernel, PIC
   and IOAPIC in userspace.
3. **No IRQCHIP**: all interrupt handling in userspace.

**Why in-kernel:** Firecracker uses in-kernel IRQCHIP. It's the
fastest option: HLT, timer interrupts, and APIC access are all
handled by KVM without exiting to userspace. On nested virt, this means
0 KVM exits (L0 handles everything), which is the desired behavior for
performance. The downside is that serial PIO is also coalesced, but
that's a display issue, not a correctness issue.

## Memory Layout

### Choice: Single contiguous region at GPA 0

**Alternatives:**
1. **Single region at GPA 0**: one mmap, one KVM slot.
2. **Multiple regions with MMIO gap**: split RAM around the PCI hole
   (0x10000000-0x80000000).
3. **Firecracker's layout**: multiple regions for the MMIO gap + ACPI
   area.

**Why single region:** We're MMIO-only (no PCI), so there's no need
for a PCI hole. The E820 map marks 0xA0000-0x100000 as reserved (VGA +
BIOS area) and 0x10000000-0x80000000 as reserved (MMIO gap), but the
actual memory is one contiguous mmap. This is simpler and faster (one
KVM slot, one mmap).

## Snapshot Format

### Choice: Custom format (magic + state + raw memory)

**Alternatives:**
1. **Custom format**: `[8B magic][8B state_len][8B mem_len][1B diff][state][mem]`
2. **bincode serialization**: serialize the full VM state with bincode.
3. **Firecracker's format**: separate state file + memory file.

**Why custom:** bincode was too slow for 256MB memory dumps (15s for
serialization). The custom format writes raw memory directly to the
file with a small JSON state header. This gives 15ms snapshot times.
The format is shared between full snapshot and live snapshot so either
output can be restored interchangeably.

## Restore Strategy

### Choice: UFFD lazy restore with eager fallback

**Alternatives:**
1. **Eager copy**: read entire memory from file into mmap (85ms).
2. **UFFD lazy restore**: mmap the file, fault pages in on demand.
3. **Hybrid**: try UFFD, fall back to eager if unavailable.

**Why hybrid:** UFFD gives O(1) restore (the VM is "runnable" in <10ms),
but it may not be available on all hosts (e.g., nested virt may restrict
userfaultfd). The hybrid approach tries UFFD first and falls back to
eager copy. This matches Firecracker's approach of lazy restore with
fallback.

## vCPU Thread Model

### Choice: Synchronous for boot, threaded for live VMs

**Alternatives:**
1. **Synchronous**: `run_vcpu` blocks the caller until HLT/timeout.
2. **Dedicated thread**: `VcpuThread::spawn` runs KVM_RUN in a
   background thread with pause/resume/stop control.
3. **Always threaded**: every VM uses a dedicated vCPU thread.

**Why hybrid:** Synchronous boot is simpler and faster (no thread
spawn overhead, no synchronization). The 33ms fast boot would be
slower with thread spawn + barrier. For live VMs (that need to stay
running while the controller does snapshot/restore), the dedicated
thread is required. Firecracker uses dedicated threads for all vCPUs,
but they optimize for long-running VMs, not cold-boot latency.

## Jailer

### Choice: chroot + cap drop + seccomp + cgroup

**Alternatives:**
1. **No jailer**: run as root, no isolation.
2. **chroot only**: filesystem isolation.
3. **Full jailer**: chroot + namespaces + cap drop + seccomp + cgroup.

**Why full jailer:** The PRD requires host-owned security boundary.
The jailer creates a chroot, drops all capabilities (PR_CAPBSET_DROP +
capset + ambient clear), installs a seccomp BPF filter, and applies
cgroup limits. This matches Firecracker's jailer model. The seccomp
filter is per-thread (installed only on the vCPU thread, not the
controller thread) so the controller can still call openat/snapshot.

## Boot Timeout / Watchdog

### Choice: Watchdog thread with tgkill(SIGALRM)

**Alternatives:**
1. **alarm()**: process-level SIGALRM.
2. **Watchdog thread with tgkill**: spawn a thread that sleeps for
   the timeout, then sends SIGALRM to the exact vCPU thread via
   `tgkill(pid, tid, SIGALRM)`.
3. **KVM_SET_SIGNAL_MASK**: mask signals during KVM_RUN.

**Why tgkill:** `alarm()` targets the process, so on multi-threaded
processes the signal may land on the wrong thread. `tgkill` directs
the signal at the exact thread executing KVM_RUN. The watchdog polls a
cancel flag at 5ms granularity so it can exit promptly after a
successful HLT without firing the signal.

## Diagnostic Memory Dump

### Choice: Dump + scan after timeout

**Alternatives:**
1. **KVM trace**: enable KVM tracing to capture instruction-level
   execution.
2. **GDB stub**: attach GDB to the vCPU.
3. **Memory dump + string scan**: after timeout, dump guest memory
   and search for kernel log strings.

**Why memory dump:** KVM tracing is complex and slow. GDB stub requires
KVM_SET_GUEST_DEBUG which may not work on nested virt. The memory dump
is simple, fast, and directly reveals what the kernel wrote: boot
log, error messages, panic traces. This was the key diagnostic tool
that found all 8 bugs.
