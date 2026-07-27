#!/usr/bin/env bash
# ci/livesnap-check.sh — live-snapshot memory-consistency gate.
#
# Runs the live-snapshot integration harness, which boots a VM with its vCPU
# executing in the background, writes a random payload into guest RAM, takes a
# live snapshot while the guest keeps running, restores it into a fresh
# controller and requires the payload's SHA256 to survive. It also asserts that
# the final-stop blackout is a residual copy rather than a full-RAM copy, that
# back-to-back live snapshots do not delete each other's artifacts, and that a
# diff snapshot taken after a live snapshot still restores consistently.
#
# Root/KVM. c8i only.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
AGENT="$ROOT/guest/agent/vmm-agent"
BAKE="$ROOT/guest/agent/bake-agent.sh"
BASE_ROOTFS="${BASE_ROOTFS:-/tmp/vsock-rootfs.ext4}"
ROOTFS="${ROOTFS:-/tmp/livesnap-rootfs.ext4}"

[ -r "$KERNEL" ] || {
  echo "error: kernel is not readable: $KERNEL" >&2
  exit 1
}
[ -r "$BASE_ROOTFS" ] || {
  echo "error: base rootfs is not readable: $BASE_ROOTFS" >&2
  exit 1
}

make -C "$ROOT/guest/agent" >/dev/null 2>&1 || true
cp -f "$BASE_ROOTFS" "$ROOTFS"
e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$BAKE" "$ROOTFS" "$AGENT" >/dev/null

echo "kernel: $KERNEL"
echo "rootfs: $ROOTFS"

cd "$ROOT"
VMM_TEST_KERNEL="$KERNEL" VMM_TEST_ROOTFS="$ROOTFS" RUST_LOG="${RUST_LOG:-warn}" \
  cargo test --features kvm -p vmm-integration --test comprehensive_tests \
  live_snapshot_consistency_harness -- --ignored --nocapture --test-threads=1
STATUS=$?

rm -f "$ROOTFS"
exit "$STATUS"
