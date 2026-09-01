#!/usr/bin/env bash
# Real-KVM cross-node lifecycle and crash-recovery matrix.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
GATE="$ROOT/orch/tests/e2e_peer_artifact_replication.sh"
KERNEL_510="${TARIT_KERNEL_510:?set TARIT_KERNEL_510}"
KERNEL_66="${TARIT_KERNEL_66:?set TARIT_KERNEL_66}"
SOURCE_REVISION="${TARIT_SOURCE_REVISION:?set TARIT_SOURCE_REVISION}"
RUN_ROOT="${TARIT_CROSS_NODE_MATRIX_ROOT:-${TMPDIR:-/tmp}/tarit-cross-node-matrix}"

command -v tee >/dev/null || { echo "FAIL: missing tee" >&2; exit 1; }
for path in "$GATE" "$KERNEL_510" "$KERNEL_66"; do
  [ -f "$path" ] || { echo "FAIL: required matrix input is not a file: $path" >&2; exit 1; }
done
[[ "$SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]] || {
  echo "FAIL: TARIT_SOURCE_REVISION must be a full Git revision" >&2
  exit 1
}
install -d -m 0700 "$RUN_ROOT"

cases=(
  "baseline66|6.6|$KERNEL_66|0||0"
  "after-claim510|5.10|$KERNEL_510|1|after_claim|0"
  "after-snapshot66|6.6|$KERNEL_66|1|after_snapshot|0"
  "after-localize510|5.10|$KERNEL_510|1|after_localize|0"
  "after-bind66|6.6|$KERNEL_66|1|after_bind|0"
  "after-child510|5.10|$KERNEL_510|1|after_child|0"
  "after-commit66|6.6|$KERNEL_66|1|after_commit|0"
  "near-enospc510|5.10|$KERNEL_510|0||1"
)

passed=0
for row in "${cases[@]}"; do
  IFS='|' read -r name kernel_name kernel_path inject_death death_phase near_enospc <<<"$row"
  log="$RUN_ROOT/$name.log"
  echo "CROSS_NODE_MATRIX_CASE_START case=$name kernel=$kernel_name"
  set +e
  env \
    ROOT="$ROOT" \
    ORCH_ROOT="$ROOT/orch" \
    VMM_ROOT="$ROOT/vmm" \
    TARITD_BIN="${TARITD_BIN:-$ROOT/orch/target/release/taritd}" \
    VMM_BIN="${VMM_BIN:-$ROOT/vmm/target/release/vmm}" \
    AGENT="${TARIT_TEST_GUEST_AGENT_BIN:-$ROOT/vmm/guest/agent/vmm-agent}" \
    KERNEL="$kernel_path" \
    TARIT_EXPECT_KERNEL_PREFIX="$kernel_name." \
    TARIT_SOURCE_REVISION="$SOURCE_REVISION" \
    TARIT_TEST_CROSS_NODE_FORK_DEATH="$inject_death" \
    TARIT_TEST_CROSS_NODE_FORK_DEATH_PHASE="$death_phase" \
    TARIT_TEST_NEAR_ENOSPC="$near_enospc" \
    TARIT_E2E_KEEP_FAILED=1 \
    "$GATE" 2>&1 | tee "$log"
  status=${PIPESTATUS[0]}
  set -e
  if [ "$status" -ne 0 ] || ! grep -Fq "PASS source=$SOURCE_REVISION:" "$log"; then
    echo "CROSS_NODE_MATRIX_CASE_FAIL case=$name kernel=$kernel_name status=$status log=$log" >&2
    exit 1
  fi
  passed=$((passed + 1))
  echo "CROSS_NODE_MATRIX_CASE_PASS case=$name kernel=$kernel_name"
done

[ "$passed" -eq "${#cases[@]}" ] || {
  echo "FAIL: incomplete cross-node matrix: passed=$passed expected=${#cases[@]}" >&2
  exit 1
}
echo "CROSS_NODE_FAILURE_MATRIX_PASS cases=$passed kernels=5.10,6.6 source=$SOURCE_REVISION"
