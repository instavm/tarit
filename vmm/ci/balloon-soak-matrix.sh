#!/usr/bin/env bash
# Real-KVM balloon liveness gate across OCI userspaces and supported kernels.
set -Eeuo pipefail

ROOT="${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_BIN="${VMM_BIN:-$ROOT/target/release/vmm}"
GUEST_AGENT_BIN="${TARIT_TEST_GUEST_AGENT_BIN:-$ROOT/guest/agent/vmm-agent}"
UBUNTU_ROOTFS="${TARIT_OCI_UBUNTU_ROOTFS:?set TARIT_OCI_UBUNTU_ROOTFS}"
ALPINE_ROOTFS="${TARIT_OCI_ALPINE_ROOTFS:?set TARIT_OCI_ALPINE_ROOTFS}"
KERNEL_510="${TARIT_KERNEL_510:?set TARIT_KERNEL_510}"
KERNEL_66="${TARIT_KERNEL_66:?set TARIT_KERNEL_66}"
CYCLES="${BALLOON_RESTORE_CYCLES:-20}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"
CASE_DIR=
ROOTFS_MOUNT=

cleanup() {
  if [[ -n "$ROOTFS_MOUNT" ]] && mountpoint -q "$ROOTFS_MOUNT"; then
    umount -- "$ROOTFS_MOUNT" || true
  fi
  ROOTFS_MOUNT=
  if [[ -n "$CASE_DIR" ]]; then
    rm -rf -- "$CASE_DIR"
  fi
}
trap cleanup EXIT

[[ $(id -u) -eq 0 ]] || { echo "FAIL: balloon matrix must run as root" >&2; exit 1; }
for required in e2fsck install mount mountpoint sha256sum umount; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done
for path in "$VMM_BIN" "$GUEST_AGENT_BIN" "$UBUNTU_ROOTFS" "$ALPINE_ROOTFS" "$KERNEL_510" "$KERNEL_66"; do
  [[ -f "$path" ]] || { echo "FAIL: required matrix input is not a file: $path" >&2; exit 1; }
done
[[ -x "$VMM_BIN" && -x "$GUEST_AGENT_BIN" ]] || {
  echo "FAIL: VMM and guest agent must be executable" >&2
  exit 1
}
echo "balloon matrix guest agent sha256=$(sha256sum "$GUEST_AGENT_BIN" | awk '{print $1}')"

passed=0
for kernel_case in "6.6:$KERNEL_66" "5.10:$KERNEL_510"; do
  kernel_name="${kernel_case%%:*}"
  kernel_path="${kernel_case#*:}"
  for image_case in "ubuntu:$UBUNTU_ROOTFS" "alpine:$ALPINE_ROOTFS"; do
    os_id="${image_case%%:*}"
    rootfs_path="${image_case#*:}"
    CASE_DIR=$(mktemp -d "$SOCKET_ROOT/tarit-balloon-soak.XXXXXX")
    chmod 0700 "$CASE_DIR"
    rootfs_copy="$CASE_DIR/rootfs.ext4"
    cp --reflink=always --sparse=auto "$rootfs_path" "$rootfs_copy"
    cmp --silent "$rootfs_path" "$rootfs_copy"
    ROOTFS_MOUNT="$CASE_DIR/rootfs-mount"
    mkdir -m 0700 -- "$ROOTFS_MOUNT"
    mount -o loop,rw -- "$rootfs_copy" "$ROOTFS_MOUNT"
    install -D -m 0755 -- "$GUEST_AGENT_BIN" "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
    sync -f "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
    umount -- "$ROOTFS_MOUNT"
    ROOTFS_MOUNT=
    e2fsck -pf "$rootfs_copy" >/dev/null
    chmod 0444 "$rootfs_copy"
    socket="$CASE_DIR/vmm.sock"
    log="$CASE_DIR/vmm.log"
    echo "== balloon soak: kernel=$kernel_name oci=$os_id cycles=$CYCLES =="
    if ! VMM_BIN="$VMM_BIN" \
      KERNEL="$kernel_path" \
      ROOTFS="$rootfs_copy" \
      SOCKET="$socket" \
      LOG="$log" \
      BALLOON_RESTORE_CYCLES="$CYCLES" \
      BALLOON_CGROUP_ENFORCE=1 \
      "$ROOT/ci/balloon-validate.sh"; then
      echo "FAIL: retained balloon evidence at $CASE_DIR" >&2
      CASE_DIR=
      exit 1
    fi
    passed=$((passed + 1))
    rm -rf -- "$CASE_DIR"
    CASE_DIR=
  done
done

[[ "$passed" -eq 4 ]] || { echo "FAIL: incomplete balloon matrix" >&2; exit 1; }
echo "PASS: balloon restore/pressure soak passed all 4 OCI/kernel cases"
