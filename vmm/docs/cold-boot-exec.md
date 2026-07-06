# Cold-boot-to-exec latency (north star: <100ms)

**Goal:** cold `create` → first code-exec finish in **<100ms** on bare metal
(beat Firecracker's ~125ms boot-to-init). No warm pool. Measured on the c8i
**nested-virt** host (KVM-in-a-VM), where every guest exit traps to L0, so
absolute numbers run ~10× a bare-metal host: use them for **relative** gains.

## Path

Cold path = minimal kernel → **agent as PID 1 (no systemd)** → command over the
exec channel → result. The guest agent (`guest/agent/vmm-agent.c`) runs as
`init=/usr/sbin/vmm-agent`, and serves `VMM_EXEC:<cmd>` → `VMM_EXEC_EXIT=<code>`
over **two channels**: a virtio-vsock stream (default, `crates/vmm-core/src/vsock_exec.rs`)
and the `/dev/ttyS0` serial console (fallback). The 16550 IRQ-4 fix keeps serial
exec interrupt-driven; vsock avoids the shared-console desync entirely.

## Measured (c8i nested-virt, agent rootfs on virtio-blk, 256 MiB, 1 vCPU)

`ci/coldboot-measure.sh` times `create` → first successful `exec`:

| cmdline | cold create→first-exec (nested) | bare-metal projection (~÷10) |
|---|---|---|
| verbose baseline (`earlycon`, `console=ttyS0`, `panic=1`) | ~2725 ms | ~270 ms |
| **fast** (`quiet loglevel=0` + boot-tuning) | **~1395 ms** | **~140 ms** |

The dominant cost is **kernel console spam over the emulated 16550** (one VM-exit
per byte). `quiet loglevel=0` roughly halves cold-boot-to-exec. This is now the
VMM's `default_cmdline()`. The exec round-trip itself is ~10-20 ms.

### Initramfs vs virtio-blk rootfs: measured, and the surprise

An initramfs boot (agent as `/init` + static busybox, `rdinit=/init`, no
`root=`, no virtio-blk) measured **~1401 ms**: essentially identical to the
rootfs fast-cmdline path (**~1395 ms**). So on this stack cold-boot is
**kernel-init-bound, not I/O-bound**: the virtio-blk probe + ext4 mount is only a
few ms. **The next real win is a stripped minimal kernel, not the boot medium.**
(Initramfs is still useful to keep the runtime off a disk image, and `ci/initramfs-test.sh`
demonstrates the path: `uname -a` returns in one 10 ms exec after ~1.4 s boot.)

## Fast cmdline (baked into `default_cmdline()`)

```
console=ttyS0 quiet loglevel=0 reboot=k panic=-1 nomodule pci=off
i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd swiotlb=noforce
cryptomgr.notests random.trust_cpu=on tsc=reliable no_timer_check
nowatchdog nokaslr
```
Callers add `root=/dev/vda rw init=/usr/sbin/vmm-agent` (rootfs) or
`rdinit=/init` (initramfs). We keep the UART (needed for the agent) rather than
Firecracker's `8250.nr_uarts=0`.

## Minimal kernel results

Built on c8i in `~/kernel-build` from Linux 5.10.230 and
Firecracker's `microvm-kernel-ci-x86_64-5.10-no-acpi.config`. The current output
is `/tmp/vmlinux.minimal.vsock`, 33 MiB, 33,984,312 bytes. The previous minimal
image without virtio-vsock was `/tmp/vmlinux.minimal`, 33 MiB, 33,959,048 bytes.
The previous `/tmp/vmlinux.microvm` is 46 MiB, 47,972,752 bytes.

Final config is saved as `guest/configs/minimal-x86_64.config`. Main deltas:

- `CONFIG_MODULES=n`.
- `CONFIG_KVM_GUEST=y`, `CONFIG_PARAVIRT=y`, `CONFIG_PARAVIRT_CLOCK=y`.
- Disabled SERIO, i8042, AT keyboard, HID, SCSI, ATA, USB, sound, DRM, FB,
  netfilter, bridge, wireless, squashfs, zswap, debug info, IKCONFIG.
- Kept virtio-mmio, virtio-blk, virtio-net, virtio-vsock, 8250 serial, ext4,
  proc, sysfs, devtmpfs, tmpfs.
- Kept ACPI. The VMM's DSDT is still the working virtio-mmio discovery path.
- Kept `CONFIG_PCI=y` but still boot with `pci=off`. With `CONFIG_PCI=n`, ACPI
  discovery did not produce `/dev/vda`. Adding `virtio_mmio.device=4K@0xd0000000:5`
  then failed with `virtio_blk: probe of virtio0 failed with error -22`.
- Left `CONFIG_HW_RANDOM` disabled, so `CONFIG_HW_RANDOM_VIRTIO` remains off.
  Keeping virtio-rng enabled previously made the current VMM vCPU thread
  terminate before virtio-blk probe. This build did not retest it.

Verbose boot timestamps, clean agent rootfs, same cmdline:

| stage | current 5.10.230 | minimal 5.10.230 |
|---|---:|---:|
| serial console enabled | 290 ms | 282 ms |
| PCI routing done | 486 ms | 471 ms |
| 8250 ttyS0 ready | 533 ms | 520 ms |
| virtio-blk vda seen | 636 ms | 625 ms |
| i8042 probe | 662 to 1362 ms | removed |
| IPv6 registered | 1372 ms | 647 ms |
| root mounted | 1427 ms | 705 ms |
| agent as init | 1463 ms | 730 ms |

The cut is mostly i8042/SERIO. Removing HID, SCSI, iSCSI, bridge/netfilter,
squashfs, and zswap removes the remaining probe noise. ACPI and PCI setup are
still visible and are the next kernel-side target only after device discovery no
longer depends on them.

Production fast cmdline, clean rootfs, 5 runs, 15 ms exec poll (`ci/coldboot-measure.sh`):

| config | create-return (nested) | cold create→first-exec (nested) | bare-metal projection (~÷10) |
|---|---:|---:|---:|
| microvm `/tmp/vmlinux.microvm` + vsock exec | 32 ms | 1009 ms | ~101 ms |
| microvm `/tmp/vmlinux.microvm` + serial exec | 32 ms | 1010 ms | ~101 ms |
| minimal `/tmp/vmlinux.minimal` + serial exec | 26 ms | 310 ms | ~31 ms |
| **minimal `/tmp/vmlinux.minimal.vsock` + vsock exec** | **24 ms** | **340 ms** | **~34 ms** |

The new minimal vsock number used `~/kernel-build/ws5-coldboot-measure.sh`
on c8i. It waits for the host log line `vsock exec: guest agent connected`, then
runs the first API exec over vsock. The VMM log confirmed `exec ... via vsock` in
all 5 runs.

Two findings:

1. **`create` returns in ~24-32 ms**: the API call sets up KVM + devices and
   spawns the vCPU thread, then returns while the guest boots in the background.
   The rest of cold-boot-to-exec is the guest kernel booting to the agent.
2. **The minimal kernel remains the win: 340 ms nested ≈ 34 ms bare metal with
   vsock exec**, well under the 100 ms north star. Tightening the exec poll from
   100 ms → 15 ms also removed the quantization that previously masked the kernel
   gain (was 496 ms).

**vsock vs serial makes little difference to cold-boot-exec** (340 ms vs 310 ms
on the minimal kernel): the time is dominated by the guest kernel booting to PID
1. The vsock exec channel is still the preferred default because it eliminates
the ttyS0 console/exec desync under concurrent IRQ load (25/25 rapid execs clean
vs serial's first-exec desync).

Validation: `uname -r` returned through one API exec over vsock on the minimal
vsock kernel: `5.10.230`. The guest showed the virtio-vsock driver bound at
`/sys/bus/virtio/drivers/vmw_vsock_virtio_transport/virtio2`. There is no
`/dev/vsock` node in this rootfs, but AF_VSOCK works and the host log confirmed
`exec ... via vsock`.

## Remaining work to hit <100ms bare metal (priority order)

1. **Skip more boot**: `acpi=off` (initramfs path, no ACPI virtio discovery),
   less memory, 1 vCPU; strip remaining kernel probe/setup after device discovery
   no longer depends on ACPI/PCI.
2. **initramfs boot**: measured NOT to help on the current kernel (I/O mount is
   only a few ms), but keeps the runtime off a disk image and pairs with `acpi=off`
   once virtio discovery no longer needs ACPI. `ci/initramfs-test.sh` has the path.
3. Node itself: its ELF + dynamic-linker load becomes material under 100ms; use a
   resident agent so the runtime is already warm, or a small static build.

## References
- Firecracker SLA ≤125ms to `/sbin/init` (serial console disabled),
  `resources/guest_configs` (no-ACPI), `docs/initrd.md`, boot-time test cmdline.
- REAP working-set prefetch; LightVM 2.3ms; Catalyzer sfork (design pressure).
