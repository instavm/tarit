#!/usr/bin/env bash
# Release gate for persistent-volume lifecycle across supported OCI guests,
# kernels, and storage placement modes.
set -Eeuo pipefail

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
CASE_RUNNER="$ROOT/orch/tests/e2e_persistent_volume_hibernate.sh"
UBUNTU_ROOTFS="${TARIT_OCI_UBUNTU_ROOTFS:?set TARIT_OCI_UBUNTU_ROOTFS}"
ALPINE_ROOTFS="${TARIT_OCI_ALPINE_ROOTFS:?set TARIT_OCI_ALPINE_ROOTFS}"
KERNEL_510="${TARIT_KERNEL_510:?set TARIT_KERNEL_510}"
KERNEL_66="${TARIT_KERNEL_66:?set TARIT_KERNEL_66}"

for path in "$CASE_RUNNER" "$UBUNTU_ROOTFS" "$ALPINE_ROOTFS" "$KERNEL_510" "$KERNEL_66"; do
  [ -f "$path" ] || { echo "FAIL: required matrix input is not a file: $path" >&2; exit 1; }
done

passed=0
for provider in local_block nfs_v4_1_block; do
  for kernel_case in "6.6:$KERNEL_66" "5.10:$KERNEL_510"; do
    kernel_name="${kernel_case%%:*}"
    kernel_path="${kernel_case#*:}"
    for image_case in "ubuntu:$UBUNTU_ROOTFS" "alpine:$ALPINE_ROOTFS"; do
      os_id="${image_case%%:*}"
      rootfs_path="${image_case#*:}"
      echo "== volume matrix: provider=$provider kernel=$kernel_name oci=$os_id =="
      TARIT_KERNEL="$kernel_path" \
        TARIT_ROOTFS="$rootfs_path" \
        TARIT_EXPECT_OS_ID="$os_id" \
        TARIT_EXPECT_KERNEL_PREFIX="$kernel_name." \
        TARIT_VOLUME_PROVIDER="$provider" \
        "$CASE_RUNNER"
      passed=$((passed + 1))
    done
  done
done

[ "$passed" -eq 8 ] || { echo "FAIL: incomplete matrix: passed=$passed expected=8" >&2; exit 1; }
echo "PASS: all 8 persistent-volume OCI/kernel/provider cases passed, including NFS outage recovery"
