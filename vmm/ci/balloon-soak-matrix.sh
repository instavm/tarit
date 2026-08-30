#!/usr/bin/env bash
# Real-KVM balloon liveness gate across OCI userspaces and supported kernels.
set -Eeuo pipefail

ROOT="${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_BIN="${VMM_BIN:-$ROOT/target/release/vmm}"
UBUNTU_ROOTFS="${TARIT_OCI_UBUNTU_ROOTFS:?set TARIT_OCI_UBUNTU_ROOTFS}"
ALPINE_ROOTFS="${TARIT_OCI_ALPINE_ROOTFS:?set TARIT_OCI_ALPINE_ROOTFS}"
KERNEL_510="${TARIT_KERNEL_510:?set TARIT_KERNEL_510}"
KERNEL_66="${TARIT_KERNEL_66:?set TARIT_KERNEL_66}"
CYCLES="${BALLOON_RESTORE_CYCLES:-20}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"
CASE_DIR=

cleanup() {
  if [[ -n "$CASE_DIR" ]]; then
    rm -rf -- "$CASE_DIR"
  fi
}
trap cleanup EXIT

for path in "$VMM_BIN" "$UBUNTU_ROOTFS" "$ALPINE_ROOTFS" "$KERNEL_510" "$KERNEL_66"; do
  [[ -f "$path" ]] || { echo "FAIL: required matrix input is not a file: $path" >&2; exit 1; }
done

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
