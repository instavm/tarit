#!/usr/bin/env bash
# Real-KVM OCI compatibility matrix across supported guest kernels.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
GATE="$ROOT/orch/tests/e2e_oci_compatibility.sh"
KERNEL_510="${TARIT_KERNEL_510:?set TARIT_KERNEL_510}"
KERNEL_66="${TARIT_KERNEL_66:?set TARIT_KERNEL_66}"
SOURCE_REVISION="${TARIT_SOURCE_REVISION:?set TARIT_SOURCE_REVISION}"
RUN_ROOT="${TARIT_OCI_KERNEL_MATRIX_ROOT:-${TMPDIR:-/tmp}/tarit-oci-kernel-matrix}"

command -v tee >/dev/null || { echo "FAIL: missing tee" >&2; exit 1; }
for path in "$GATE" "$KERNEL_510" "$KERNEL_66"; do
  [ -f "$path" ] || { echo "FAIL: required matrix input is not a file: $path" >&2; exit 1; }
done
[[ "$SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]] || {
  echo "FAIL: TARIT_SOURCE_REVISION must be a full Git revision" >&2
  exit 1
}
install -d -m 0700 "$RUN_ROOT"

passed=0
for kernel_case in "6.6|$KERNEL_66" "5.10|$KERNEL_510"; do
  IFS='|' read -r kernel_name kernel_path <<<"$kernel_case"
  log="$RUN_ROOT/kernel-$kernel_name.log"
  echo "OCI_KERNEL_MATRIX_CASE_START kernel=$kernel_name"
  set +e
  env \
    ROOT="$ROOT" \
    TARITD_BIN="${TARITD_BIN:-$ROOT/orch/target/release/taritd}" \
    TARIT_VMM_BIN="${TARIT_VMM_BIN:-$ROOT/vmm/target/release/vmm}" \
    TARIT_VMM_AGENT="${TARIT_TEST_GUEST_AGENT_BIN:-$ROOT/vmm/guest/agent/vmm-agent}" \
    TARIT_KERNEL="$kernel_path" \
    TARIT_EXPECT_KERNEL_PREFIX="$kernel_name." \
    TARIT_TEST_SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}" \
    TARIT_E2E_KEEP_FAILED=1 \
    "$GATE" 2>&1 | tee "$log"
  status=${PIPESTATUS[0]}
  set -e
  if [ "$status" -ne 0 ] ||
     ! grep -Fq "OCI_COMPATIBILITY_PASS cases=7 shell_images=6 expected_no_shell=1 kernel_prefix=$kernel_name." "$log"; then
    echo "OCI_KERNEL_MATRIX_CASE_FAIL kernel=$kernel_name status=$status log=$log" >&2
    exit 1
  fi
  passed=$((passed + 1))
  echo "OCI_KERNEL_MATRIX_CASE_PASS kernel=$kernel_name"
done

[ "$passed" -eq 2 ] || { echo "FAIL: incomplete OCI kernel matrix" >&2; exit 1; }
echo "OCI_KERNEL_MATRIX_PASS kernels=5.10,6.6 images_per_kernel=7 source=$SOURCE_REVISION"
