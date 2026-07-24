#!/usr/bin/env bash
set -euo pipefail

# Builds a minimal x86_64 guest kernel (uncompressed vmlinux) for Tarit microVMs:
# virtio-blk + virtio-vsock + virtio-net + serial console, ACPI, ext4. No modules.
# Downloads the checksum-pinned kernel source, applies the vendored 6.12 config
# plus Tarit overrides, and builds reproducibly. Output: $OUT (default
# $WORKDIR/vmlinux.minimal).
#
# Needs a C toolchain and kernel build deps: make, gcc, bc, flex, bison,
# libelf-dev, libssl-dev, curl, xz-utils.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=kernel-version.env
. "${SCRIPT_DIR}/kernel-version.env"

WORKDIR=${WORKDIR:-$HOME/kernel-build}
JOBS=${JOBS:-2}

KERNEL_URL=${KERNEL_URL:-https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz}

SRC="${WORKDIR}/linux-${KERNEL_VERSION}"
TARBALL="${WORKDIR}/linux-${KERNEL_VERSION}.tar.xz"
BUILD="${WORKDIR}/build-${KERNEL_VERSION}"
BASE_CONFIG=${BASE_CONFIG:-${SCRIPT_DIR}/configs/minimal-x86_64.config}
OUT="${OUT:-${WORKDIR}/vmlinux.minimal}"
CONFIG_OUT="${CONFIG_OUT:-${WORKDIR}/minimal-x86_64.config.generated}"

mkdir -p "${WORKDIR}"
cd "${WORKDIR}"

if [ ! -f "${TARBALL}" ]; then
  curl --fail --location --retry 3 --retry-all-errors "${KERNEL_URL}" -o "${TARBALL}.tmp"
  mv "${TARBALL}.tmp" "${TARBALL}"
fi

printf '%s  %s\n' "${KERNEL_SOURCE_SHA256}" "${TARBALL}" | sha256sum --check --status || {
  echo "error: kernel source checksum mismatch: ${TARBALL}" >&2
  echo "expected sha256: ${KERNEL_SOURCE_SHA256}" >&2
  exit 1
}

[ -f "${BASE_CONFIG}" ] || {
  echo "error: base config not found: ${BASE_CONFIG}" >&2
  exit 1
}

rm -rf "${SRC}" "${BUILD}"
tar -xf "${TARBALL}"
mkdir -p "${BUILD}"
cp "${BASE_CONFIG}" "${BUILD}/.config"

"${SRC}/scripts/config" --file "${BUILD}/.config" \
  --set-str LOCALVERSION "" --disable LOCALVERSION_AUTO --set-str BUILD_SALT "" \
  --disable MODULES \
  --enable KVM_GUEST --enable PARAVIRT --enable PARAVIRT_CLOCK \
  --enable ACPI --enable PNP --enable PNPACPI --enable ACPI_SPCR_TABLE \
  --disable ACPI_AC --disable ACPI_BATTERY --disable ACPI_BUTTON \
  --disable ACPI_FAN --disable ACPI_DOCK --disable ACPI_APEI \
  --disable ACPI_TABLE_UPGRADE --disable ACPI_DEBUG --disable ACPI_CONFIGFS \
  --enable PCI --disable PCIEPORTBUS --disable HOTPLUG_PCI --disable VIRTIO_PCI \
  --disable USB --disable USB_SUPPORT --disable USB_GADGET --disable USB_ROLE_SWITCH \
  --disable HID --disable HIDRAW --disable UHID --disable USB_HID \
  --disable SCSI --disable SCSI_LOWLEVEL --disable ATA \
  --disable SERIO --disable SERIO_I8042 --disable KEYBOARD_ATKBD \
  --disable INPUT_KEYBOARD --disable INPUT_MOUSE --disable INPUT_JOYSTICK \
  --disable INPUT_TABLET --disable INPUT_TOUCHSCREEN --disable INPUT_MISC \
  --disable SOUND --disable SND --disable DRM --disable FB \
  --disable NETFILTER --disable BRIDGE --disable WIRELESS --disable WLAN \
  --disable BPF_SYSCALL --disable BPF_JIT \
  --disable BPF_PRELOAD --disable BPF_PRELOAD_UMD --disable CGROUP_BPF \
  --disable IPV6_SEG6_LWTUNNEL --disable IPV6_SEG6_HMAC --disable IPV6_SEG6_BPF \
  --disable BPF_STREAM_PARSER --disable LWTUNNEL_BPF \
  --enable VIRTIO --enable VIRTIO_MENU --enable VIRTIO_MMIO \
  --enable VIRTIO_MMIO_CMDLINE_DEVICES --enable VIRTIO_BLK --enable VIRTIO_NET \
  --disable VIRTIO_CONSOLE --enable VSOCKETS --enable VIRTIO_VSOCKETS \
  --disable VIRTIO_PMEM --disable VIRTIO_BALLOON --disable VIRTIO_MEM \
  --disable HW_RANDOM --disable HW_RANDOM_VIRTIO \
  --enable SERIAL_8250 --enable SERIAL_8250_CONSOLE --enable SERIAL_8250_PNP \
  --set-val SERIAL_8250_NR_UARTS 1 --set-val SERIAL_8250_RUNTIME_UARTS 1 \
  --enable EXT4_FS --enable EXT4_USE_FOR_EXT2 --enable JBD2 --enable FS_MBCACHE \
  --disable EXT4_DEBUG --disable SQUASHFS --disable ZSWAP --disable ZBUD --disable ZPOOL \
  --enable PROC_FS --enable SYSFS --enable TMPFS --enable DEVTMPFS --enable DEVTMPFS_MOUNT \
  --enable BINFMT_ELF --enable UNIX --enable INET --enable NET \
  --disable XFS_FS --disable BTRFS_FS --disable F2FS_FS --disable FUSE_FS \
  --disable OVERLAY_FS --disable NFS_FS --disable CIFS --disable 9P_FS \
  --disable DEBUG_INFO --disable DEBUG_KERNEL --disable KASAN --disable UBSAN \
  --disable GDB_SCRIPTS --disable IKCONFIG --disable IKHEADERS \
  --disable X86_CPU_RESCTRL --disable NUMA \
  --enable HYPERVISOR_GUEST --disable HYPERV --disable XEN --disable XEN_PV --disable XEN_PVH \
  --enable RANDOM_TRUST_CPU --enable CRYPTO_MANAGER_DISABLE_TESTS

export KBUILD_BUILD_TIMESTAMP="@${KERNEL_SOURCE_DATE_EPOCH}"
export KBUILD_BUILD_USER=tarit
export KBUILD_BUILD_HOST=kernel-builder
export KBUILD_BUILD_VERSION=1
export SOURCE_DATE_EPOCH="${KERNEL_SOURCE_DATE_EPOCH}"
export KCFLAGS="-fdebug-prefix-map=${SRC}=linux-${KERNEL_VERSION} -fdebug-prefix-map=${BUILD}=linux-${KERNEL_VERSION}"

make -C "${SRC}" O="${BUILD}" olddefconfig
make -C "${SRC}" O="${BUILD}" -j"${JOBS}" vmlinux

mkdir -p "$(dirname "${OUT}")" "$(dirname "${CONFIG_OUT}")"
cp "${BUILD}/vmlinux" "${OUT}"
cp "${BUILD}/.config" "${CONFIG_OUT}"

ls -lh "${OUT}" "${CONFIG_OUT}"
sha256sum "${OUT}" "${CONFIG_OUT}"
