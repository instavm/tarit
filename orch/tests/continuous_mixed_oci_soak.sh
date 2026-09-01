#!/usr/bin/env bash
# Supervised, bounded-storage lifecycle soak for long-running mixed OCI guests.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD="${TARITD_BIN:-$ROOT/orch/target/release/taritd}"
VMM="${TARIT_VMM_BIN:-$ROOT/vmm/target/release/vmm}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-/t}"
STATE_GATE="$ROOT/orch/tests/e2e_lifecycle_state_machine.sh"
DRIVER="$ROOT/orch/tests/continuous_lifecycle_soak.py"
CRASH_GATE="${TARIT_CONTINUOUS_CRASH_GATE:-$ROOT/orch/tests/e2e_runtime_crash_recovery.sh}"
INGRESS_GATE="${TARIT_CONTINUOUS_INGRESS_GATE:-$ROOT/orch/tests/e2e_live_fork_hibernate.sh}"
VOLUME_GATE="${TARIT_CONTINUOUS_VOLUME_GATE:-$ROOT/orch/tests/e2e_persistent_volume_hibernate.sh}"
WORKLOAD_SOURCE="$ROOT/orch/tests/clone_repair_workload.c"
GUEST_AGENT="${TARIT_TEST_GUEST_AGENT_BIN:-$ROOT/vmm/guest/agent/vmm-agent}"
RUN_ROOT="${TARIT_CONTINUOUS_RUN_ROOT:-$SOCKET_ROOT/tarit-continuous-soak}"
LOCK="${TARIT_CONTINUOUS_LOCK:-/run/lock/tarit-september-global.lock}"
EPOCH_SECONDS="${TARIT_CONTINUOUS_EPOCH_SECONDS:-1800}"
EPOCH_HIBERNATE_MIN_SECONDS="${TARIT_CONTINUOUS_EPOCH_HIBERNATE_MIN_SECONDS:-0}"
MIN_FREE_BYTES="${TARIT_CONTINUOUS_MIN_FREE_BYTES:-3221225472}"
MAX_SNAPSHOTS="${TARIT_CONTINUOUS_MAX_SNAPSHOTS:-2}"
CHAOS_EVERY_EPOCHS="${TARIT_CONTINUOUS_CHAOS_EVERY_EPOCHS:-1}"
INGRESS_EVERY_EPOCHS="${TARIT_CONTINUOUS_INGRESS_EVERY_EPOCHS:-1}"
VOLUME_EVERY_EPOCHS="${TARIT_CONTINUOUS_VOLUME_EVERY_EPOCHS:-1}"
MAX_EPOCHS="${TARIT_CONTINUOUS_MAX_EPOCHS:-0}"
TOTAL_WORKLOAD_SECONDS="${TARIT_CONTINUOUS_TOTAL_WORKLOAD_SECONDS:-0}"
STATUS_FILE="${TARIT_CONTINUOUS_STATUS_FILE:-$RUN_ROOT/status.json}"
CASES="${TARIT_CONTINUOUS_CASES:-}"
CASES_FILE="${TARIT_CONTINUOUS_CASES_FILE:-}"

for required in df flock gcc python3; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done
command -v setsid >/dev/null || { echo "FAIL: missing setsid" >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "FAIL: continuous soak must run as root" >&2; exit 1; }
test -x "$GUEST_AGENT" || { echo "FAIL: guest agent not executable: $GUEST_AGENT" >&2; exit 1; }
[[ "$EPOCH_SECONDS" =~ ^[1-9][0-9]*$ ]] && [ "$EPOCH_SECONDS" -ge 60 ] || {
  echo "FAIL: epoch duration must be at least 60 seconds" >&2
  exit 1
}
[[ "$EPOCH_HIBERNATE_MIN_SECONDS" =~ ^[0-9]+$ ]] || {
  echo "FAIL: invalid epoch hibernation minimum" >&2
  exit 1
}
[[ "$MIN_FREE_BYTES" =~ ^[0-9]+$ ]] || { echo "FAIL: invalid free-space floor" >&2; exit 1; }
[[ "$MAX_SNAPSHOTS" =~ ^[1-9][0-9]*$ ]] || { echo "FAIL: invalid snapshot limit" >&2; exit 1; }
[[ "$CHAOS_EVERY_EPOCHS" =~ ^[0-9]+$ ]] || { echo "FAIL: invalid chaos interval" >&2; exit 1; }
[[ "$INGRESS_EVERY_EPOCHS" =~ ^[0-9]+$ ]] || { echo "FAIL: invalid ingress interval" >&2; exit 1; }
[[ "$VOLUME_EVERY_EPOCHS" =~ ^[0-9]+$ ]] || { echo "FAIL: invalid volume interval" >&2; exit 1; }
[[ "$MAX_EPOCHS" =~ ^[0-9]+$ ]] || { echo "FAIL: invalid maximum epoch count" >&2; exit 1; }
[[ "$TOTAL_WORKLOAD_SECONDS" =~ ^[0-9]+$ ]] || {
  echo "FAIL: invalid cumulative workload duration" >&2
  exit 1
}
if [ "$MAX_EPOCHS" -gt 0 ] && [ "$TOTAL_WORKLOAD_SECONDS" -gt 0 ]; then
  echo "FAIL: maximum epochs and cumulative workload duration are mutually exclusive" >&2
  exit 1
fi
[ "$CHAOS_EVERY_EPOCHS" -eq 0 ] || test -r "$CRASH_GATE" || {
  echo "FAIL: runtime crash gate not readable: $CRASH_GATE" >&2
  exit 1
}
[ "$INGRESS_EVERY_EPOCHS" -eq 0 ] || test -r "$INGRESS_GATE" || {
  echo "FAIL: ingress gate not readable: $INGRESS_GATE" >&2
  exit 1
}
[ "$VOLUME_EVERY_EPOCHS" -eq 0 ] || test -r "$VOLUME_GATE" || {
  echo "FAIL: volume gate not readable: $VOLUME_GATE" >&2
  exit 1
}

install -d -m 0700 "$RUN_ROOT/history" "$RUN_ROOT/build" "$RUN_ROOT/failures"
WORKLOAD_BIN="$RUN_ROOT/build/clone-repair-workload"
gcc -std=c11 -O2 -Wall -Wextra -Werror -pedantic -static \
  "$WORKLOAD_SOURCE" -o "$WORKLOAD_BIN.next"
chmod 0755 "$WORKLOAD_BIN.next"
mv -f "$WORKLOAD_BIN.next" "$WORKLOAD_BIN"

if [ -n "$CASES_FILE" ]; then
  test -r "$CASES_FILE" || { echo "FAIL: unreadable case file: $CASES_FILE" >&2; exit 1; }
  mapfile -t case_rows < <(sed '/^[[:space:]]*$/d' "$CASES_FILE")
else
  mapfile -t case_rows < <(printf '%s\n' "$CASES" | sed '/^[[:space:]]*$/d')
fi
[ "${#case_rows[@]}" -gt 0 ] || { echo "FAIL: no continuous soak cases" >&2; exit 1; }
if [ "$TOTAL_WORKLOAD_SECONDS" -gt 0 ] &&
   [ "$TOTAL_WORKLOAD_SECONDS" -lt $((EPOCH_SECONDS * ${#case_rows[@]})) ]; then
  echo "FAIL: cumulative workload duration must cover every configured case epoch" >&2
  exit 1
fi

write_supervisor_status() {
  local state=$1 kind=$2 case_name=${3:-} seed_value=${4:-0} log_value=${5:-}
  python3 - "$STATUS_FILE" "$state" "$kind" "$case_name" "$seed_value" "$log_value" "$epoch" <<'PY'
import json
import os
import sys
import tempfile
import time

path, state, kind, case_name, seed, log, epoch = sys.argv[1:]
directory = os.path.dirname(os.path.abspath(path))
os.makedirs(directory, mode=0o700, exist_ok=True)
payload = {
    "schema_version": 1,
    "state": state,
    "latest_event": kind,
    "updated_at_unix": int(time.time()),
    "case": case_name or None,
    "seed": int(seed),
    "epoch": int(epoch),
    "log": log or None,
}
descriptor, temporary = tempfile.mkstemp(prefix=".status.", suffix=".tmp", dir=directory)
try:
    os.fchmod(descriptor, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        json.dump(payload, output, sort_keys=True)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, path)
except BaseException:
    try:
        os.close(descriptor)
    except OSError:
        pass
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    raise
PY
}

epoch_pid=""
terminate_epoch() {
  local exit_status=$1
  trap - INT TERM HUP
  if [ -n "$epoch_pid" ] && kill -0 "$epoch_pid" 2>/dev/null; then
    kill -TERM -- "-$epoch_pid" 2>/dev/null || true
    wait "$epoch_pid" 2>/dev/null || true
  fi
  exit "$exit_status"
}
trap 'terminate_epoch 130' INT
trap 'terminate_epoch 0' TERM
trap 'terminate_epoch 129' HUP

run_auxiliary_gate() {
  local kind=$1 gate=$2 marker=$3 name=$4 seed_value=$5 expected_os=$6
  shift 6
  local gate_timestamp gate_ref gate_log gate_status
  gate_timestamp=$(date -u +%Y%m%dT%H%M%SZ)
  gate_ref="history/${gate_timestamp}-${name}-${kind}.log"
  gate_log="$RUN_ROOT/$gate_ref"
  write_supervisor_status qualification "${kind}_start" "$name" "$seed_value" "$gate_ref"
  echo "CONTINUOUS_QUALIFICATION_START case=$name seed=$seed_value gate=$kind log=$gate_log"
  set +e
  setsid --wait flock -F -w 300 "$LOCK" env \
    ROOT="$ROOT" \
    TARITD_BIN="$TARITD" \
    TARIT_VMM_BIN="$VMM" \
    TARIT_KERNEL="$kernel" \
    TARIT_ROOTFS="$rootfs" \
    TARIT_TEST_CLONE_WORKLOAD_BIN="$WORKLOAD_BIN" \
    TARIT_TEST_GUEST_AGENT_BIN="$GUEST_AGENT" \
    TARIT_EXPECT_OS_ID="$expected_os" \
    TARIT_TEST_SOCKET_ROOT="$SOCKET_ROOT" \
    TARIT_E2E_KEEP_FAILED=0 \
    "$@" \
    bash "$gate" >"$gate_log" 2>&1 &
  epoch_pid=$!
  wait "$epoch_pid"
  gate_status=$?
  epoch_pid=""
  set -e
  if [ "$gate_status" -ne 0 ] || ! grep -Eq "$marker" "$gate_log"; then
    printf '{"timestamp":"%s","case":"%s","seed":%s,"status":%s,"kind":"%s","log":"%s"}\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$name" "$seed_value" "$gate_status" "$kind" "$gate_ref" \
      >>"$RUN_ROOT/failures/index.jsonl"
    write_supervisor_status failed "${kind}_failed" "$name" "$seed_value" "$gate_ref"
    echo "CONTINUOUS_QUALIFICATION_FAILED case=$name seed=$seed_value gate=$kind log=$gate_log" >&2
    tail -240 "$gate_log" >&2
    [ "$gate_status" -ne 0 ] && return "$gate_status"
    return 1
  fi
  printf '{"timestamp":"%s","case":"%s","seed":%s,"status":0,"kind":"%s","log":"%s"}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$name" "$seed_value" "$kind" "$gate_ref" \
    >>"$RUN_ROOT/qualification.jsonl"
  write_supervisor_status running "${kind}_pass" "$name" "$seed_value" "$gate_ref"
  echo "CONTINUOUS_QUALIFICATION_PASS case=$name seed=$seed_value gate=$kind log=$gate_log"
}

epoch=0
write_supervisor_status starting supervisor_start
while :; do
  row=${case_rows[$((epoch % ${#case_rows[@]}))]}
  IFS='|' read -r name kernel rootfs <<<"$row"
  [[ "$name" =~ ^[a-zA-Z0-9._-]+$ ]] || { echo "FAIL: invalid case name: $name" >&2; exit 1; }
  [ -r "$kernel" ] || { echo "FAIL: unreadable kernel: $kernel" >&2; exit 1; }
  [ -r "$rootfs" ] || { echo "FAIL: unreadable rootfs: $rootfs" >&2; exit 1; }
  free_bytes=$(df --output=avail -B1 "$SOCKET_ROOT" | tail -n 1 | tr -d ' ')
  [ "$free_bytes" -ge "$MIN_FREE_BYTES" ] || {
    echo "FAIL: continuous soak free-space floor reached: $free_bytes bytes" >&2
    exit 1
  }

  timestamp=$(date -u +%Y%m%dT%H%M%SZ)
  seed=$((10#$(date -u +%Y%m%d) + epoch))
  log_ref="history/${timestamp}-${name}-seed-${seed}.jsonl"
  log="$RUN_ROOT/$log_ref"
  ln -sfn -- "$log_ref" "$RUN_ROOT/current.jsonl"
  write_supervisor_status running epoch_start "$name" "$seed" "$log_ref"
  echo "CONTINUOUS_SOAK_EPOCH_START case=$name seed=$seed duration_s=$EPOCH_SECONDS log=$log"
  set +e
  setsid --wait flock -F -w 300 "$LOCK" env \
    ROOT="$ROOT" \
    TARITD_BIN="$TARITD" \
    TARIT_VMM_BIN="$VMM" \
    TARIT_KERNEL="$kernel" \
    TARIT_ROOTFS="$rootfs" \
    TARIT_TEST_SOCKET_ROOT="$SOCKET_ROOT" \
    TARIT_TEST_CLONE_WORKLOAD_BIN="$WORKLOAD_BIN" \
    TARIT_TEST_GUEST_AGENT_BIN="$GUEST_AGENT" \
    TARIT_LIFECYCLE_DRIVER="$DRIVER" \
    TARIT_LIFECYCLE_DRIVER_ARGS="--duration-seconds $EPOCH_SECONDS --interval-seconds 1 --anchors 3 --anchor-vcpus 1,2,4 --hibernate-hold-seconds 65 --guest-timer-seconds 5 --sibling-fork-timer-seconds 45 --epoch-hibernate-min-seconds $EPOCH_HIBERNATE_MIN_SECONDS --storage-path $SOCKET_ROOT --min-free-bytes $MIN_FREE_BYTES" \
    TARIT_LIFECYCLE_SEEDS="$seed" \
    TARIT_LIFECYCLE_MAX_VMS=6 \
    TARIT_LIFECYCLE_MAX_VCPUS=12 \
    TARIT_LIFECYCLE_MAX_MEMORY_MIB=2048 \
    TARIT_LIFECYCLE_MAX_SNAPSHOTS="$MAX_SNAPSHOTS" \
    TARIT_LIFECYCLE_STATUS_FILE="$STATUS_FILE" \
    TARIT_LIFECYCLE_CASE_NAME="$name" \
    TARIT_LIFECYCLE_EPOCH="$epoch" \
    TARIT_E2E_KEEP_FAILED=0 \
    TARIT_E2E_FAILURE_ARCHIVE_ROOT="$RUN_ROOT/failures" \
    "$STATE_GATE" >"$log" 2>&1 &
  epoch_pid=$!
  wait "$epoch_pid"
  status=$?
  epoch_pid=""
  set -e
  if [ "$status" -ne 0 ]; then
    printf '{"timestamp":"%s","case":"%s","seed":%s,"status":%s,"log":"%s"}\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$name" "$seed" "$status" "$log_ref" \
      >>"$RUN_ROOT/failures/index.jsonl"
    write_supervisor_status failed epoch_failed "$name" "$seed" "$log_ref"
    echo "CONTINUOUS_SOAK_FAILED case=$name seed=$seed log=$log" >&2
    tail -240 "$log" >&2
    exit "$status"
  fi
  grep -q '"event": "soak_pass"' "$log" || {
    echo "FAIL: soak epoch did not emit its pass record: $log" >&2
    exit 1
  }
  printf '{"timestamp":"%s","case":"%s","seed":%s,"status":0,"log":"%s"}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$name" "$seed" "$log_ref" \
    >>"$RUN_ROOT/epochs.jsonl"
  echo "CONTINUOUS_SOAK_EPOCH_PASS case=$name seed=$seed log=$log"

  completed_epochs=$((epoch + 1))
  expected_os=${name%%[0-9]*}
  case "$expected_os" in
    ubuntu|alpine) ;;
    *)
      echo "FAIL: cannot derive expected OCI OS ID from case name: $name" >&2
      exit 1
      ;;
  esac
  if [ "$CHAOS_EVERY_EPOCHS" -gt 0 ] && \
     [ $((completed_epochs % CHAOS_EVERY_EPOCHS)) -eq 0 ]; then
    run_auxiliary_gate runtime_crash "$CRASH_GATE" \
      '^RUNTIME_CRASH_RECOVERY_PASS ' "$name" "$seed" "$expected_os"
  fi
  if [ "$INGRESS_EVERY_EPOCHS" -gt 0 ] && \
     [ $((completed_epochs % INGRESS_EVERY_EPOCHS)) -eq 0 ]; then
    run_auxiliary_gate ingress_wake "$INGRESS_GATE" \
      '^PASS: live fork \+ hibernate \+ HTTP/PTy/SSH' "$name" "$seed" "$expected_os"
  fi
  if [ "$VOLUME_EVERY_EPOCHS" -gt 0 ] && \
     [ $((completed_epochs % VOLUME_EVERY_EPOCHS)) -eq 0 ]; then
    if [ $((completed_epochs % 2)) -eq 0 ]; then
      volume_provider=nfs_v4_1_block
    else
      volume_provider=local_block
    fi
    run_auxiliary_gate "volume_${volume_provider}" "$VOLUME_GATE" \
      '^VOLUME_E2E_PASS provider=' "$name" "$seed" "$expected_os" \
      "TARIT_VOLUME_PROVIDER=$volume_provider"
  fi

  find "$RUN_ROOT/history" -type f \( -name '*.jsonl' -o -name '*.log' \) -mtime +14 -delete
  sync -f "$SOCKET_ROOT" >/dev/null 2>&1 || true
  mount_target=$(findmnt -n -o TARGET -T "$SOCKET_ROOT" 2>/dev/null || true)
  if [ -n "$mount_target" ] && command -v fstrim >/dev/null 2>&1; then
    fstrim "$mount_target" >/dev/null 2>&1 || true
  fi
  epoch=$completed_epochs
  workload_seconds=$((completed_epochs * EPOCH_SECONDS))
  if [ "$TOTAL_WORKLOAD_SECONDS" -gt 0 ] &&
     [ "$workload_seconds" -ge "$TOTAL_WORKLOAD_SECONDS" ] &&
     [ "$completed_epochs" -ge "${#case_rows[@]}" ]; then
    write_supervisor_status stopped cumulative_duration_reached "$name" "$seed" "$log_ref"
    echo "CONTINUOUS_SOAK_COMPLETE epochs=$epoch workload_seconds=$workload_seconds cases=${#case_rows[@]}"
    exit 0
  fi
  if [ "$MAX_EPOCHS" -gt 0 ] && [ "$epoch" -ge "$MAX_EPOCHS" ]; then
    write_supervisor_status stopped maximum_epochs_reached "$name" "$seed" "$log_ref"
    echo "CONTINUOUS_SOAK_COMPLETE epochs=$epoch"
    exit 0
  fi
done
