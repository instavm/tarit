#!/usr/bin/env bash
# One-time quickstart setup: install a verified guest kernel and pre-pull an
# Ubuntu rootfs with the exec agent and runtime tools. Doing this once means
# starting a microVM later is instant (no kernel build, no OCI pull at boot).
#
# Run from the repo root (needs root for the OCI unpack):
#   sudo ./scripts/setup-guest.sh
#
# Output:
#   guest-assets/vmlinux       vsock + virtio-blk guest kernel
#   guest-assets/rootfs.ext4   Ubuntu rootfs with the agent as init
#
# Override with env vars: OUT_DIR, IMAGE, VMM, JOBS, KERNEL_DOWNLOAD.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="${OUT_DIR:-$REPO/guest-assets}"
IMAGE="${IMAGE:-docker://ubuntu:22.04}"
VMM="${VMM:-$(command -v vmm || echo "$REPO/vmm/target/release/vmm")}"
AGENT="$REPO/vmm/guest/agent/vmm-agent"
export JOBS="${JOBS:-$(nproc 2>/dev/null || echo 2)}"
# shellcheck source=../vmm/guest/kernel-version.env disable=SC1091
. "$REPO/vmm/guest/kernel-version.env"
KERNEL_MARKER="$OUT_DIR/kernel.version"
ROOTFS_TOOLS_MARKER="$OUT_DIR/rootfs.tools.version"
ROOTFS_TOOLS_VERSION=1

mkdir -p "$OUT_DIR"

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

if [ ! -x "$VMM" ] && ! command -v "$VMM" >/dev/null 2>&1; then
  echo "error: vmm binary not found. Build it first: sudo make install (or make vmm)." >&2
  exit 1
fi

# 1. Guest kernel (vsock + virtio-blk). Prefer the pinned release artifact,
# then fall back visibly to the checksum-pinned source build.
KERNEL_READY=0
if [ -f "$OUT_DIR/vmlinux" ] && [ -f "$KERNEL_MARKER" ]; then
  read -r installed_version installed_sha256 < "$KERNEL_MARKER" || true
  if [ "${installed_version:-}" = "$KERNEL_VERSION" ] &&
      [ "${installed_sha256:-}" = "$(sha256_file "$OUT_DIR/vmlinux")" ]; then
    KERNEL_READY=1
  fi
fi

if [ "$KERNEL_READY" != "1" ]; then
  if [ "${KERNEL_DOWNLOAD:-1}" != "0" ] &&
      OUT="$OUT_DIR/vmlinux" bash "$REPO/vmm/guest/download-kernel.sh"; then
    :
  else
    echo "warning: verified release kernel unavailable; building from pinned source" >&2
    if command -v apt-get >/dev/null 2>&1; then
      echo "== ensuring kernel build deps =="
      apt-get update -qq
      apt-get install -y -qq build-essential bc flex bison libelf-dev libssl-dev curl xz-utils >/dev/null
    fi
    echo "== building guest kernel from pinned source (one-time, a few minutes) =="
    OUT="$OUT_DIR/vmlinux.new" CONFIG_OUT="$OUT_DIR/kernel.config.new" \
      WORKDIR="${KBUILD_DIR:-$OUT_DIR/kernel-build}" \
      bash "$REPO/vmm/guest/build-minimal-kernel.sh"
    mv "$OUT_DIR/vmlinux.new" "$OUT_DIR/vmlinux"
    mv "$OUT_DIR/kernel.config.new" "$OUT_DIR/kernel.config"
  fi
  printf '%s %s\n' "$KERNEL_VERSION" "$(sha256_file "$OUT_DIR/vmlinux")" > "$KERNEL_MARKER.new"
  mv "$KERNEL_MARKER.new" "$KERNEL_MARKER"
else
  echo "== kernel $KERNEL_VERSION already at $OUT_DIR/vmlinux =="
fi

# 2. Guest exec agent (static).
if [ ! -x "$AGENT" ]; then
  echo "== building guest agent =="
  make -C "$REPO/vmm/guest/agent"
fi

# 3. Ubuntu rootfs with the agent baked in as init. Skip if present.
if [ ! -f "$OUT_DIR/rootfs.ext4" ]; then
  echo "== pulling $IMAGE into a rootfs (one-time) =="
  rm -f "$ROOTFS_TOOLS_MARKER"
  "$VMM" pull "$IMAGE" --output "$OUT_DIR/rootfs.ext4" --agent "$AGENT"
else
  echo "== rootfs already at $OUT_DIR/rootfs.ext4 =="
fi

if [ ! -f "$ROOTFS_TOOLS_MARKER" ] ||
    [ "$(cat "$ROOTFS_TOOLS_MARKER")" != "$ROOTFS_TOOLS_VERSION" ]; then
  "$REPO/vmm/guest/install-runtime-tools.sh" "$OUT_DIR/rootfs.ext4"
  printf '%s\n' "$ROOTFS_TOOLS_VERSION" > "$ROOTFS_TOOLS_MARKER.new"
  mv "$ROOTFS_TOOLS_MARKER.new" "$ROOTFS_TOOLS_MARKER"
fi

cat <<EOF

guest assets ready:
  kernel: $OUT_DIR/vmlinux
  rootfs: $OUT_DIR/rootfs.ext4

start a microVM and run code in it:
  sudo vmm serve --socket /tmp/vm.sock &
  sudo vmm --socket /tmp/vm.sock create --kernel $OUT_DIR/vmlinux --rootfs $OUT_DIR/rootfs.ext4
  sleep 12 && sudo vmm --socket /tmp/vm.sock exec "uname -a"
EOF
