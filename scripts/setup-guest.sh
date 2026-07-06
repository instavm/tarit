#!/usr/bin/env bash
# One-time quickstart setup: build a vsock-capable guest kernel and pre-pull an
# Ubuntu rootfs with the exec agent baked in as init. Doing this once means
# starting a microVM later is instant (no kernel build, no OCI pull at boot).
#
# Run from the repo root (needs root for the OCI unpack):
#   sudo ./scripts/setup-guest.sh
#
# Output:
#   guest-assets/vmlinux       vsock + virtio-blk guest kernel
#   guest-assets/rootfs.ext4   Ubuntu rootfs with the agent as init
#
# Override with env vars: OUT_DIR, IMAGE, VMM, JOBS.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="${OUT_DIR:-$REPO/guest-assets}"
IMAGE="${IMAGE:-docker://ubuntu:22.04}"
VMM="${VMM:-$(command -v vmm || echo "$REPO/vmm/target/release/vmm")}"
AGENT="$REPO/vmm/guest/agent/vmm-agent"
export JOBS="${JOBS:-$(nproc 2>/dev/null || echo 2)}"

mkdir -p "$OUT_DIR"

if [ ! -x "$VMM" ] && ! command -v "$VMM" >/dev/null 2>&1; then
  echo "error: vmm binary not found. Build it first: sudo make install (or make vmm)." >&2
  exit 1
fi

# Kernel build dependencies (Debian/Ubuntu). Skipped if apt-get is absent.
if command -v apt-get >/dev/null 2>&1; then
  echo "== ensuring kernel build deps =="
  apt-get update -qq
  apt-get install -y -qq build-essential bc flex bison libelf-dev libssl-dev curl xz-utils >/dev/null
fi

# 1. Guest kernel (vsock + virtio-blk). Skip if already built.
if [ ! -f "$OUT_DIR/vmlinux" ]; then
  echo "== building guest kernel (one-time, a few minutes) =="
  OUT="$OUT_DIR/vmlinux" WORKDIR="${KBUILD_DIR:-$OUT_DIR/kernel-build}" \
    bash "$REPO/vmm/guest/build-minimal-kernel.sh"
else
  echo "== kernel already at $OUT_DIR/vmlinux =="
fi

# 2. Guest exec agent (static).
if [ ! -x "$AGENT" ]; then
  echo "== building guest agent =="
  make -C "$REPO/vmm/guest/agent"
fi

# 3. Ubuntu rootfs with the agent baked in as init. Skip if present.
if [ ! -f "$OUT_DIR/rootfs.ext4" ]; then
  echo "== pulling $IMAGE into a rootfs (one-time) =="
  "$VMM" pull "$IMAGE" --output "$OUT_DIR/rootfs.ext4" --agent "$AGENT"
else
  echo "== rootfs already at $OUT_DIR/rootfs.ext4 =="
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
