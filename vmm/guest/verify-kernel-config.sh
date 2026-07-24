#!/usr/bin/env bash
set -euo pipefail

CONFIG="${1:?usage: verify-kernel-config.sh CONFIG}"

require_enabled() {
  grep -qx "CONFIG_$1=y" "$CONFIG" || {
    echo "error: required kernel option CONFIG_$1=y is missing" >&2
    exit 1
  }
}

require_disabled() {
  if grep -Eq "^CONFIG_$1=[ym]$" "$CONFIG"; then
    echo "error: forbidden kernel option CONFIG_$1 is enabled" >&2
    exit 1
  fi
}

for option in \
  64BIT KVM_GUEST ACPI VIRTIO VIRTIO_MMIO VIRTIO_MMIO_CMDLINE_DEVICES \
  VIRTIO_BLK VIRTIO_NET VSOCKETS VIRTIO_VSOCKETS SERIAL_8250 \
  SERIAL_8250_CONSOLE EXT4_FS PROC_FS SYSFS TMPFS DEVTMPFS \
  DEVTMPFS_MOUNT BINFMT_ELF UNIX INET; do
  require_enabled "$option"
done

for option in \
  MODULES VIRTIO_PCI USB_SUPPORT SCSI ATA SOUND DRM NETFILTER BRIDGE \
  WIRELESS WLAN BPF_SYSCALL BPF_JIT BPF_PRELOAD CGROUP_BPF \
  XFS_FS BTRFS_FS F2FS_FS FUSE_FS NFS_FS CIFS 9P_FS \
  DEBUG_KERNEL DEBUG_INFO KASAN UBSAN NUMA HYPERV XEN; do
  require_disabled "$option"
done

echo "kernel config verified: required virtio-only devices are built in; forbidden surfaces are disabled"
