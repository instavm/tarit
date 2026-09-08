#!/usr/bin/env bash
# Serial CPU/RAM boot, hibernation, and CLI wake qualification.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
UBUNTU="${TARIT_OCI_UBUNTU_ROOTFS:?set TARIT_OCI_UBUNTU_ROOTFS}"
ALPINE="${TARIT_OCI_ALPINE_ROOTFS:?set TARIT_OCI_ALPINE_ROOTFS}"
KERNEL_510="${TARIT_KERNEL_510:?set TARIT_KERNEL_510}"
KERNEL_66="${TARIT_KERNEL_66:?set TARIT_KERNEL_66}"
LOCK="${TARIT_QUALIFICATION_LOCK:-/run/lock/tarit-september-global.lock}"

[ "$(id -u)" -eq 0 ] || { echo "FAIL: matrix requires root" >&2; exit 1; }
for input in "$UBUNTU" "$ALPINE" "$KERNEL_510" "$KERNEL_66"; do
  test -r "$input" || { echo "FAIL: unreadable input: $input" >&2; exit 1; }
done
# The caller must stop or yield any continuous workload before dispatching.
# Never run large shapes concurrently with another qualification gate.
exec 9>"$LOCK"
flock -n 9 || { echo "FAIL: qualification worker is reserved" >&2; exit 1; }
for kernel_case in "5.10.:$KERNEL_510" "6.6.:$KERNEL_66"; do
  for image_case in "ubuntu:$UBUNTU" "alpine:$ALPINE"; do
    TARIT_LIFECYCLE_MODE=resource_shapes \
    TARIT_LIFECYCLE_MAX_VMS=4 \
    TARIT_LIFECYCLE_MAX_VCPUS=12 \
    TARIT_LIFECYCLE_MAX_MEMORY_MIB=4096 \
    TARIT_KERNEL="${kernel_case#*:}" \
    TARIT_EXPECT_KERNEL_PREFIX="${kernel_case%%:*}" \
    TARIT_ROOTFS="${image_case#*:}" \
    TARIT_EXPECT_OS_ID="${image_case%%:*}" \
      bash "$ROOT/orch/tests/e2e_lifecycle_state_machine.sh"
  done
done
echo "RESOURCE_SHAPE_MATRIX_PASS cases=96 mixed_fork_cases=4 scope=resource_lifecycle"
