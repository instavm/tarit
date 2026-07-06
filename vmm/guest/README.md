# Guest kernel + rootfs

The VMM does direct kernel boot (no firmware/BIOS) — PRD §5. We need:

## Kernel

An **uncompressed `vmlinux`** (ELF) or a `bzImage`, built with a minimal
config: virtio-mmio + virtio-blk + virtio-net + 16550 serial only. The PRD
(§5) calls this out as one of the biggest boot-time levers.

### Minimal cold-boot kernel

`guest/configs/minimal-x86_64.config` is the measured 5.10.230 config for the
cold create-to-exec path. Rebuild it on the c8i host in the isolated
`~/kernel-build` workspace:

```sh
scp guest/build-minimal-kernel.sh ubuntu@<kvm-host>:~/kernel-build/
ssh ubuntu@<kvm-host> 'cd ~/kernel-build && ./build-minimal-kernel.sh'
# output: ~/kernel-build/vmlinux.minimal and /tmp/vmlinux.minimal
```

The script starts from Firecracker's 5.10 no-ACPI microvm config, keeps minimal
ACPI for this VMM's DSDT virtio-mmio discovery, and emits an uncompressed ELF
`vmlinux`.

### Ready-made configs (in `guest/kernel-configs/`)

Cherry-picked from `rust-vmm/vmm-reference`'s `resources/kernel/` (Apache-2.0
OR BSD-3-Clause). These are minimal microVM kernel configs, not full distro
configs:

| File | Purpose |
|---|---|
| `microvm-kernel-5.4-x86_64.config` | Minimal x86_64 kernel for virtio-mmio + serial |
| `microvm-kernel-initramfs-hello-x86_64.config` | Same, but boots an initramfs to a hello-world shell (best for the M6 boot spike) |
| `busybox_1_32_1_static_config` | Busybox userspace config (statically linked, tiny) |
| `make_kernel.sh` | Builds the kernel from a config |
| `make_busybox.sh` | Builds static busybox for the initramfs |
| `make_rootfs.sh` / `install_system.sh` | Builds an ext4 rootfs (alternative to initramfs) |

### Build (on a Linux host — these scripts don't run on macOS)

```sh
cd guest/kernel-configs
./make_busybox.sh        # produces a static busybox binary
./make_kernel.sh x86_64 microvm-kernel-initramfs-hello-x86_64.config
# → produces guest/vmlinux + guest/initramfs.cpio.gz
```

For the M6 boot spike, the **initramfs-hello** config is the path of least
resistance: no block device needed, kernel boots straight to a shell.

## Rootfs / initramfs

A minimal ext4 rootfs or an initramfs. For the boot spike (M6) an
initramfs is easiest — no block device needed.

Place files at:
- `guest/vmlinux`
- `guest/initramfs.cpio.gz`

These are gitignored (see `.gitignore`).
