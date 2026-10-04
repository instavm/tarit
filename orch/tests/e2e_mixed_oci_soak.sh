#!/usr/bin/env bash
# Bounded real-KVM lifecycle soak across explicit kernel and OCI-rootfs cases.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
STATE_GATE="${TARIT_STATE_GATE:-$ROOT/orch/tests/e2e_lifecycle_state_machine.sh}"
TARITD="${TARITD_BIN:-$ROOT/orch/target/release/taritd}"
VMM="${TARIT_VMM_BIN:-$ROOT/vmm/target/release/vmm}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"
ROUNDS="${TARIT_SOAK_ROUNDS_PER_CASE:-1}"
STEPS="${TARIT_SOAK_STEPS:-40}"
SEEDS="${TARIT_SOAK_SEEDS:-7,202609,424242}"
MAX_SNAPSHOTS="${TARIT_SOAK_MAX_SNAPSHOTS:-8}"
CASES="${TARIT_SOAK_CASES:?set TARIT_SOAK_CASES to newline-separated name|kernel|rootfs entries}"
MIN_FREE_BYTES="${TARIT_SOAK_MIN_FREE_BYTES:-1073741824}"
CAPACITY_TOLERANCE_BYTES="${TARIT_SOAK_CAPACITY_TOLERANCE_BYTES:-134217728}"

for required in df findmnt pgrep python3 sync; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done
[ "$(id -u)" -eq 0 ] || { echo "FAIL: mixed OCI soak must run as root" >&2; exit 1; }
[[ "$ROUNDS" =~ ^[1-9][0-9]*$ ]] || { echo "FAIL: invalid soak round count" >&2; exit 1; }
[[ "$STEPS" =~ ^[1-9][0-9]*$ ]] || { echo "FAIL: invalid soak step count" >&2; exit 1; }
[[ "$MAX_SNAPSHOTS" =~ ^[1-9][0-9]*$ ]] || { echo "FAIL: invalid snapshot limit" >&2; exit 1; }
[[ "$MIN_FREE_BYTES" =~ ^[0-9]+$ ]] || { echo "FAIL: invalid minimum free bytes" >&2; exit 1; }
[[ "$CAPACITY_TOLERANCE_BYTES" =~ ^[0-9]+$ ]] || { echo "FAIL: invalid capacity tolerance" >&2; exit 1; }
[ -x "$STATE_GATE" ] || { echo "FAIL: state gate is not executable: $STATE_GATE" >&2; exit 1; }
[ -x "$TARITD" ] || { echo "FAIL: taritd is not executable: $TARITD" >&2; exit 1; }
[ -x "$VMM" ] || { echo "FAIL: VMM is not executable: $VMM" >&2; exit 1; }
[ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ] || {
  echo "FAIL: worker /dev/kvm is unavailable" >&2
  exit 1
}
grep -Eq '\b(vmx|svm)\b' /proc/cpuinfo || {
  echo "FAIL: worker nested virtualization is unavailable" >&2
  exit 1
}

LOG_DIR=$(mktemp -d "${TMPDIR:-/tmp}/tarit-mixed-soak.XXXXXX")
chmod 700 "$LOG_DIR"
KEEP_LOGS=0
cleanup() {
  local status=$?
  if [ "$status" -ne 0 ]; then
    KEEP_LOGS=1
    echo "FAIL: retained soak logs at $LOG_DIR" >&2
  fi
  if [ "$KEEP_LOGS" -eq 0 ]; then
    find "$LOG_DIR" -depth -delete 2>/dev/null || true
  fi
  return "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

available_bytes() {
  df --output=avail -B1 "$SOCKET_ROOT" | tail -n 1 | tr -d ' '
}
trim_storage() {
  local mount_target
  sync -f "$SOCKET_ROOT" >/dev/null 2>&1 || true
  mount_target=$(findmnt -n -o TARGET -T "$SOCKET_ROOT" 2>/dev/null || true)
  if [ -n "$mount_target" ] && command -v fstrim >/dev/null 2>&1; then
    fstrim "$mount_target" >/dev/null 2>&1 || true
  fi
}
assert_no_test_runtime() {
  if pgrep -f -- "^${VMM//./\\.} serve " >/dev/null; then
    echo "FAIL: soak round leaked the candidate VMM" >&2
    return 1
  fi
  if pgrep -f -- "^${TARITD//./\\.} serve" >/dev/null; then
    echo "FAIL: soak round leaked the candidate taritd" >&2
    return 1
  fi
  if findmnt -rn -o TARGET | grep -F '/tarit-sm.' >/dev/null; then
    echo "FAIL: soak round leaked a lifecycle mount" >&2
    return 1
  fi
}

IFS=',' read -r -a seed_values <<<"$SEEDS"
[ "${#seed_values[@]}" -gt 0 ] || { echo "FAIL: no soak seeds" >&2; exit 1; }
for seed in "${seed_values[@]}"; do
  [[ "$seed" =~ ^[0-9]+$ ]] || { echo "FAIL: invalid soak seed: $seed" >&2; exit 1; }
done

trim_storage
BASELINE_FREE=$(available_bytes)
[ "$BASELINE_FREE" -ge "$MIN_FREE_BYTES" ] || {
  echo "FAIL: only $BASELINE_FREE bytes free before soak" >&2
  exit 1
}

case_index=0
completed=0
while IFS='|' read -r name kernel rootfs; do
  [ -n "$name" ] || continue
  [[ "$name" =~ ^[a-zA-Z0-9._-]+$ ]] || { echo "FAIL: invalid case name: $name" >&2; exit 1; }
  [ -r "$kernel" ] || { echo "FAIL: unreadable kernel for $name: $kernel" >&2; exit 1; }
  [ -r "$rootfs" ] || { echo "FAIL: unreadable rootfs for $name: $rootfs" >&2; exit 1; }
  for round in $(seq 1 "$ROUNDS"); do
    seed=${seed_values[$((case_index % ${#seed_values[@]}))]}
    log="$LOG_DIR/${name}-round-${round}-seed-${seed}.log"
    echo "== soak case=$name round=$round seed=$seed steps=$STEPS =="
    env \
      TARITD_BIN="$TARITD" \
      TARIT_VMM_BIN="$VMM" \
      TARIT_KERNEL="$kernel" \
      TARIT_ROOTFS="$rootfs" \
      TARIT_TEST_SOCKET_ROOT="$SOCKET_ROOT" \
      TARIT_LIFECYCLE_SEEDS="$seed" \
      TARIT_LIFECYCLE_STEPS="$STEPS" \
      TARIT_LIFECYCLE_MAX_SNAPSHOTS="$MAX_SNAPSHOTS" \
      "$STATE_GATE" >"$log" 2>&1 || {
        tail -240 "$log" >&2
        exit 1
      }
    sentinel=$(grep -m1 "LIFECYCLE_STATE_MACHINE_PASS .*seeds=$seed steps_per_seed=$STEPS" "$log" || true)
    [ -n "$sentinel" ] || {
      echo "FAIL: $name round $round did not emit its pass sentinel" >&2
      tail -80 "$log" >&2
      exit 1
    }
    assert_no_test_runtime
    trim_storage
    current_free=$(available_bytes)
    [ "$current_free" -ge "$MIN_FREE_BYTES" ] || {
      echo "FAIL: only $current_free bytes free after $name round $round" >&2
      exit 1
    }
    consumed=$((BASELINE_FREE - current_free))
    if [ "$consumed" -gt "$CAPACITY_TOLERANCE_BYTES" ]; then
      echo "FAIL: $name round $round retained $consumed bytes after cleanup" >&2
      exit 1
    fi
    completed=$((completed + 1))
    case_index=$((case_index + 1))
    echo "   $sentinel free_bytes=$current_free"
  done
done <<<"$CASES"

[ "$completed" -gt 0 ] || { echo "FAIL: no soak cases ran" >&2; exit 1; }
assert_no_test_runtime
echo "MIXED_OCI_SOAK_PASS rounds=$completed steps_per_round=$STEPS seeds=$SEEDS"
