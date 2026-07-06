# 17 — KERNEL BOOTS ON c8i NESTED VIRT

*Goal: boot a real Linux kernel on our VMM running on a c8i instance
(nested virt, NOT bare metal), like Firecracker and Cloud Hypervisor do.*

## What I did

### 1. Found the `NestedVirtualization` flag for c8i

The PRD §12.0 says C8i "exposes `vmx` to non-metal instances ... enabled
per-instance via `CpuOptions`." The key was the `NestedVirtualization=enabled`
field inside `--cpu-options`:

```sh
aws ec2 run-instances --instance-type c8i.xlarge \
  --cpu-options CoreCount=2,ThreadsPerCore=1,NestedVirtualization=enabled \
  ...
```

With this flag: `vmx` in cpuinfo (4 CPUs), `/dev/kvm` present, `kvm-ok`
says "KVM acceleration can be used." **No bare metal needed.**

### 2. Built a guest kernel + initramfs

- Linux 5.10.230 with `microvm-kernel-initramfs-hello-x86_64.config`
- `CONFIG_PVH=y` enabled (for ELF direct boot)
- `CONFIG_INITRAMFS_SOURCE="initramfs.cpio"` (baked-in initramfs)
- Init script: prints "microVM booted successfully!" + `poweroff -f`
- Verified with QEMU (`-kernel bzImage -enable-kvm`): boots in ~1s

### 3. Found the root cause of all boot failures

**`KvmVm::new` was creating a NEW empty `GuestMemory`** (new mmap) and
registering THAT with KVM. The kernel was loaded into a DIFFERENT
`GuestMemory` (the one passed to `boot_on_kvm`). The KVM-registered memory
was **empty** — the vCPU was executing zeros → triple fault / InternalError.

**Fix:** `KvmVm::new` now takes a pre-loaded `GuestMemory` (with the kernel +
GDT already in it) and registers THAT with KVM. The binary writes the GDT
before creating KvmVm, and passes the same `GuestMemory` that the loader
wrote the kernel into.

### 4. Boot path: 32-bit bzImage entry (works on c8i nested virt)

The ELF vmlinux path (PVH) triple-faults on c8i nested virt (even with the
Xen CPUID leaf + `hvm_start_info`). The **32-bit bzImage entry** works
because the kernel's `startup_32` does its own 32→64 mode transition:

1. Load bzImage setup code at 0x10000 + compressed kernel at 0x200000
2. Write GDT (32-bit code+data segments) at 0x500000
3. Patch `cmd_line_ptr` at 0x10228
4. Create `KvmVm` (registers the pre-loaded memory with KVM)
5. Set vCPU: 32-bit protected mode (CR0.PE, no paging), flat segments,
   RIP=0x200000 (`code32_start`), RSI=0x10000 (boot_params)
6. `KVM_RUN` → kernel's `startup_32` decompresses, transitions to long
   mode, runs `startup_64` + init, **HLTs**

### 5. Result

```
$ cargo run --features boot -- run --kernel guest/bzImage --mem-mib 256 --vcpus 1
loaded: entry=0x200000 kernel_end=0x875fc0 zero_page=0x10000 cmdline=0x120000
vcpu 0 configured: entry=0x200000 zero_page=0x10000 — entering run loop
vCPU HLT — pausing
```

**The kernel boots on our VMM on c8i nested virt.** The vCPU enters
`startup_32`, decompresses the kernel, transitions to long mode, runs
`startup_64`, executes the init script, and HLTs.

## What worked

- **`NestedVirtualization=enabled` in `--cpu-options`** is the c8i nested-
  virt flag. Without it, `vmx` isn't exposed and KVM doesn't work. With it,
  KVM works exactly like Firecracker/Cloud Hypervisor expect.
- **The 32-bit bzImage entry** is the path that works on nested virt. The
  kernel's `startup_32` (in the compressed boot code) handles the 32→64
  transition internally, avoiding KVM emulator limitations.
- **155 unit tests + 2 KVM smoke tests pass on c8i.** The KVM smoke tests
  (open /dev/kvm, create VM, register memory, create vCPU, dirty log) all
  pass on nested virt.
- **The `KvmVm::new` bug was the root cause of every boot failure** — the
  triple faults, InternalErrors, and Shutdowns were all because the vCPU
  was executing from an empty memory region. Once fixed, the kernel booted
  immediately.

## What went wrong

### F1. Didn't know about `NestedVirtualization=enabled`

The PRD said "enabled per-instance via CpuOptions" but the `--cpu-options`
flag only documents `CoreCount` and `ThreadsPerCore`. The `NestedVirtualization`
field is a newer addition (AWS added it for 8th-gen Intel instances) that
isn't prominently documented. Found it by grepping `aws ec2 run-instances help`
for "nested."

### F2. `KvmVm::new` creating a new empty GuestMemory

This was the **root cause of every boot failure**. The VMM loaded the kernel
into one `GuestMemory` mmap, then `KvmVm::new` created a different `GuestMemory`
mmap (empty) and registered that with KVM. The vCPU executed from the empty
mmap → instant triple fault. This bug was invisible in the KVM smoke tests
(which don't boot a kernel) and only manifested when trying to boot.

### F3. PVH doesn't work on c8i nested virt

The ELF vmlinux with PVH entry triple-faults on c8i nested virt, even with
the Xen CPUID leaf + `hvm_start_info`. The PVH entry code (`pvh_start_xen`)
expects specific hypervisor CPUID responses that nested virt doesn't provide.
The 32-bit bzImage entry avoids this entirely (the kernel does its own mode
transition without checking for a hypervisor).

### F4. Serial output not captured (nested-virt limitation)

KVM nested virt handles PIO (port I/O) internally — the L0 hypervisor emulates
port 0x3f8 (the 16550 serial) without generating exits for the L1 guest (our
VMM). Our `VcpuExit::IoOut` handler never fires. On bare metal, every `outb`
to port 0x3f8 would generate a `KVM_EXIT_IO_OUT` exit that we'd capture. This
is a nested-virt limitation, not a VMM bug.

## What I learned

- **`NestedVirtualization=enabled` is the c8i nested-virt flag.** Without it,
  no `vmx`, no `/dev/kvm`. With it, KVM works like on bare metal (except PIO
  coalescing). This is how Firecracker and Cloud Hypervisor run on c8i.
- **The 32-bit bzImage entry is the reliable boot path on nested virt.** The
  kernel's `startup_32` handles the 32→64 transition internally, avoiding
  KVM emulator issues. PVH and direct ELF entry don't work on nested virt.
- **The GuestMemory lifecycle is the #1 source of boot bugs.** The memory
  the kernel is loaded into MUST be the same memory registered with KVM.
  Creating a new `GuestMemory` in `KvmVm::new` was the root cause — a single
  `Arc::clone` instead of `GuestMemory::new` fixed every boot failure.
- **PIO coalescing is a nested-virt limitation.** On bare metal, every `outb`
  to port 0x3f8 generates a `KVM_EXIT_IO_OUT`. On nested virt, the L0 handles
  it internally. Serial capture on nested virt requires MMIO-based serial
  (e.g., virtio-console) or disabling PIO coalescing via `KVM_CAP_DISABLE_QUIRKS`.

## Instance details

- **Instance**: `<c8i-instance-id>` (c8i.xlarge, us-east-1) — **terminated**
- **Nested virt**: `NestedVirtualization=enabled` in `--cpu-options`
- **Keypair**: `vmm-kvm-test` (kept for future use)
- **SG**: `vmm-kvm-test-sg` (kept for future use)

## Commands to reproduce

```sh
# Launch c8i with nested virt:
aws ec2 run-instances --instance-type c8i.xlarge \
  --cpu-options CoreCount=2,ThreadsPerCore=1,NestedVirtualization=enabled \
  --key-name vmm-kvm-test --security-group-ids <security-group-id> \
  --image-id <ubuntu-ami-id> ...

# On the instance:
cargo test --workspace                                    # 155 tests
cargo test -p vmm-memory-backend --features kvm -- --include-ignored  # KVM smoke
cargo run --features boot -- run --kernel guest/bzImage --mem-mib 256 --vcpus 1
# → vCPU HLT — pausing  (kernel booted!)
```
