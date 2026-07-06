#!/usr/bin/env bash
set -euo pipefail

# Builds a minimal x86_64 guest kernel (uncompressed vmlinux) for Tarit microVMs:
# virtio-blk + virtio-vsock + virtio-net + serial console, ACPI, ext4. No modules.
# Downloads the kernel source, applies the vendored microVM base config plus the
# Tarit overrides, and builds. Output: $OUT (default $WORKDIR/vmlinux.minimal).
#
# Needs a C toolchain and kernel build deps: make, gcc, bc, flex, bison,
# libelf-dev, libssl-dev, curl, xz-utils.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

KERNEL_VERSION=${KERNEL_VERSION:-5.10.230}
WORKDIR=${WORKDIR:-$HOME/kernel-build}
JOBS=${JOBS:-2}

KERNEL_URL=${KERNEL_URL:-https://cdn.kernel.org/pub/linux/kernel/v5.x/linux-${KERNEL_VERSION}.tar.xz}

SRC="${WORKDIR}/linux-${KERNEL_VERSION}"
TARBALL="${WORKDIR}/linux-${KERNEL_VERSION}.tar.xz"
# Vendored 5.10 microVM base config; override the file with BASE_CONFIG,
# or set BASE_CONFIG_URL to download one if the file is missing.
BASE_CONFIG=${BASE_CONFIG:-${SCRIPT_DIR}/kernel-configs/microvm-base-5.10.config}
BASE_CONFIG_URL=${BASE_CONFIG_URL:-}
OUT="${OUT:-${WORKDIR}/vmlinux.minimal}"
CONFIG_OUT="${CONFIG_OUT:-${WORKDIR}/minimal-x86_64.config.generated}"

mkdir -p "${WORKDIR}"
cd "${WORKDIR}"

if [ ! -f "${TARBALL}" ]; then
  curl -fL "${KERNEL_URL}" -o "${TARBALL}"
fi

if [ ! -f "${BASE_CONFIG}" ]; then
  if [ -n "${BASE_CONFIG_URL}" ]; then
    mkdir -p "$(dirname "${BASE_CONFIG}")"
    curl -fL "${BASE_CONFIG_URL}" -o "${BASE_CONFIG}"
  else
    echo "error: base config not found: ${BASE_CONFIG}" >&2
    echo "Copy kernel-configs/microvm-base-5.10.config next to this script," >&2
    echo "point BASE_CONFIG at a config file, or set BASE_CONFIG_URL." >&2
    exit 1
  fi
fi

if [ ! -d "${SRC}" ]; then
  tar -xf "${TARBALL}"
fi

cd "${SRC}"
cp "${BASE_CONFIG}" .config

./scripts/config --file .config \
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

make olddefconfig
make -j"${JOBS}" vmlinux

cp vmlinux "${OUT}"
cp .config "${CONFIG_OUT}"

# Required handoff path on c8i for VMM validation.
cp "${OUT}" /tmp/vmlinux.minimal

ls -lh "${OUT}" "${CONFIG_OUT}" /tmp/vmlinux.minimal
sha256sum "${OUT}" /tmp/vmlinux.minimal
