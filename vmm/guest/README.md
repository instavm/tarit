# Guest kernel + rootfs

The VMM does direct kernel boot (no firmware/BIOS). We need:

## Kernel

Tarit's shipped kernel is an **uncompressed `vmlinux` ELF** built with a
minimal config: virtio-mmio block, net, vsock, and 16550 serial. The loader
also accepts user-supplied `bzImage` kernels, but `make guest` and the release
workflow build, verify, and run `vmlinux`.

### Minimal cold-boot kernel

`guest/configs/minimal-x86_64.config` is the Linux 6.12 LTS config for the cold
create-to-exec path. It has virtio-mmio block, net, and vsock devices built in,
with modules and unused device families disabled.

The normal entry point downloads the release artifact and verifies its
repository-pinned SHA-256. If the artifact is unavailable, it falls back to the
same checksum-pinned source build:

```sh
# From the repository root.
sudo make guest
```

For a direct local build on Linux:

```sh
OUT=/tmp/vmlinux CONFIG_OUT=/tmp/minimal-x86_64.config \
  vmm/guest/build-minimal-kernel.sh
vmm/guest/verify-kernel-config.sh /tmp/minimal-x86_64.config
```

`vmm/guest/kernel-version.env` pins the LTS point release, kernel.org source
checksum, reproducible build timestamp, release tag, and expected artifact
checksum. The build fixes its user, host, timestamp, build number, source paths,
and toolchain container.

### Release and promotion

The `Guest kernel release` workflow builds `vmlinux`, the generated config, and
`SHA256SUMS`, then creates GitHub artifact attestations. Publishing is manual
and is blocked until the candidate passes all 19 `orch/tests/e2e_*.sh`
programs plus a minimum three-hour c8i soak. Point-release maintenance updates
the pins in `kernel-version.env`, rebuilds the config, and repeats that gate.
Lifecycle gates execute commands inside guests after create, resume, and
restore. Egress gates run real guest-side `curl` requests and prove deny,
allow, revoke, restart recovery, and slot reuse behavior.

### Ready-made configs (in `guest/kernel-configs/`)

`microvm-base-5.10.config` is retained only for the older reference builder.
The release builder consumes `guest/configs/minimal-x86_64.config`.

The rest are cherry-picked from `rust-vmm/vmm-reference`'s `resources/kernel/`
(Apache-2.0 OR BSD-3-Clause). These are minimal microVM kernel configs, not
full distro configs:

| File | Purpose |
|---|---|
| `microvm-kernel-5.4-x86_64.config` | Minimal x86_64 kernel for virtio-mmio + serial |
| `microvm-kernel-initramfs-hello-x86_64.config` | Same, but boots an initramfs to a hello-world shell (best for a first boot test) |
| `busybox_1_32_1_static_config` | Busybox userspace config (statically linked, tiny) |
| `make_kernel.sh` | Builds the kernel from a config |
| `make_busybox.sh` | Builds static busybox for the initramfs |
| `make_rootfs.sh` / `install_system.sh` | Builds an ext4 rootfs (alternative to initramfs) |

### Build (on a Linux host; these scripts do not run on macOS)

```sh
cd guest/kernel-configs
./make_busybox.sh        # produces a static busybox binary
./make_kernel.sh x86_64 microvm-kernel-initramfs-hello-x86_64.config
# → produces guest/vmlinux + guest/initramfs.cpio.gz
```

For a first boot test, the **initramfs-hello** config is the path of least
resistance: no block device needed, kernel boots straight to a shell.

## Rootfs / initramfs

A minimal ext4 rootfs or an initramfs. For a first boot test an
initramfs is easiest, since no block device is needed.

Place files at:
- `guest/vmlinux`
- `guest/initramfs.cpio.gz`

These are gitignored (see `.gitignore`).
