#!/usr/bin/env bash
# Validate the orchestrated suspend contract on a real Linux/KVM guest:
# resident guest memory is released, capacity remains reserved, live operations
# are rejected while suspended, and resume preserves state before returning.
set -euo pipefail

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD="${TARITD_BIN:-$ROOT/orch/target/release/taritd}"
VMM="${TARIT_VMM_BIN:-$ROOT/vmm/target/debug/vmm}"
KERNEL="${TARIT_KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${TARIT_ROOTFS:-/tmp/vsock-rootfs.ext4}"
KEY="suspend-e2e-key"
PORT="${SUSPEND_E2E_PORT:-}"
MIN_RSS_DROP_KIB="${SUSPEND_MIN_RSS_DROP_KIB:-32768}"
MAX_RESUME_EXEC_MS="${SUSPEND_RESUME_EXEC_MAX_MS:-5000}"
EXPECTED_KERNEL_PREFIX="${TARIT_EXPECT_KERNEL_RELEASE_PREFIX:-}"
EXPECTED_OS_ID="${TARIT_EXPECT_OS_ID:-}"
ENABLE_NET="${TARIT_TEST_ENABLE_NET:-0}"
TRANSITION_CLIENT="${TARIT_TEST_TRANSITION_CLIENT:-api}"

[[ "$EXPECTED_KERNEL_PREFIX" != *[[:space:]]* ]] || {
  echo "FAIL: TARIT_EXPECT_KERNEL_RELEASE_PREFIX must not contain whitespace" >&2
  exit 1
}
[[ "$EXPECTED_OS_ID" =~ ^[a-z0-9._-]*$ ]] || {
  echo "FAIL: TARIT_EXPECT_OS_ID contains unsupported characters" >&2
  exit 1
}
[[ "$ENABLE_NET" = 0 || "$ENABLE_NET" = 1 ]] || {
  echo "FAIL: TARIT_TEST_ENABLE_NET must be 0 or 1" >&2
  exit 1
}
[[ "$TRANSITION_CLIENT" = api || "$TRANSITION_CLIENT" = cli ]] || {
  echo "FAIL: TARIT_TEST_TRANSITION_CLIENT must be api or cli" >&2
  exit 1
}

for required in curl python3 setsid ps awk; do
  command -v "$required" >/dev/null || {
    echo "FAIL: required command '$required' is missing" >&2
    exit 1
  }
done
if [ "$ENABLE_NET" = 1 ]; then
  command -v ip >/dev/null || {
    echo "FAIL: required command 'ip' is missing" >&2
    exit 1
  }
  if ip -o link show | awk -F': ' '$2 ~ /^insta[0-9]+$/ { found=1 } END { exit !found }'; then
    echo "FAIL: pre-existing Tarit TAP would make network lifecycle ambiguous" >&2
    exit 1
  fi
fi
if [ -z "$PORT" ]; then
  PORT=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
fi
[[ "$PORT" =~ ^[0-9]+$ ]] && [ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || {
  echo "FAIL: SUSPEND_E2E_PORT must be between 1 and 65535" >&2
  exit 1
}
DIR=$(mktemp -d "${TMPDIR:-/tmp}/tarit-suspend-e2e.XXXXXX")
BASE_URL="http://127.0.0.1:$PORT"

mkdir -p "$DIR/sockets"

cleanup() {
  local status=$?
  if [ -n "${TARITD_PGID:-}" ] && kill -0 -- "-$TARITD_PGID" 2>/dev/null; then
    kill -TERM -- "-$TARITD_PGID" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 -- "-$TARITD_PGID" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL -- "-$TARITD_PGID" 2>/dev/null || true
  elif [ -n "${TARITD_PID:-}" ] && kill -0 "$TARITD_PID" 2>/dev/null; then
    kill -TERM "$TARITD_PID" 2>/dev/null || true
  fi
  if [ -n "${TARITD_PID:-}" ]; then
    wait "$TARITD_PID" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ]; then
    echo "FAIL: suspend/resume gate exited $status" >&2
    tail -240 "$DIR/taritd.log" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ] && [ "${TARIT_E2E_KEEP_FAILED:-0}" = 1 ]; then
    echo "FAIL: retained diagnostic directory: $DIR" >&2
  else
    find "$DIR" -depth -delete 2>/dev/null || true
  fi
  return "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

api() {
  curl -fsS --max-time 30 -H "X-API-Key: $KEY" "$@"
}

vm_transition() {
  local action=$1
  if [ "$TRANSITION_CLIENT" = cli ]; then
    TARIT_BASE_URL="$BASE_URL" TARIT_API_KEY="$KEY" \
      "$TARITD" --json vm "$action" "$VM_ID"
  else
    api -H 'Content-Type: application/json' -d '{}' \
      "$BASE_URL/v1/vms/$VM_ID/$action"
  fi
}

json_field() {
  python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"
}

monotonic_ms() {
  python3 -c 'import time; print(time.monotonic_ns() // 1000000)'
}

rss_kib() {
  awk '/^VmRSS:/ { print $2; found=1 } END { if (!found) exit 1 }' "/proc/$1/status"
}

vmm_pid_for_socket() {
  python3 - "$1" "$TARITD_PGID" <<'PY'
import os
import sys
from pathlib import Path

socket_path = os.fsencode(sys.argv[1])
expected_pgid = int(sys.argv[2])
matches = []
for proc in Path("/proc").iterdir():
    if not proc.name.isdigit():
        continue
    pid = int(proc.name)
    try:
        if os.getpgid(pid) != expected_pgid:
            continue
        argv = (proc / "cmdline").read_bytes().split(b"\0")
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        continue
    for index, argument in enumerate(argv[:-1]):
        if argument == b"--socket" and argv[index + 1] == socket_path:
            matches.append(pid)
            break
if len(matches) != 1:
    raise SystemExit(
        f"expected one VMM in process group {expected_pgid} for {socket_path!r}, "
        f"found {matches}"
    )
print(matches[0])
PY
}

exec_json() {
  local vm_id=$1 command=$2
  api -H 'Content-Type: application/json' \
    -d "$(python3 -c 'import json,sys; print(json.dumps({"vm_id":sys.argv[1],"command":sys.argv[2],"timeout_ms":30000}))' "$vm_id" "$command")" \
    "$BASE_URL/v1/execute"
}

assert_guest_security() {
  local vm_id=$1 result
  result=$(exec_json "$vm_id" \
    'test ! -e /dev/kvm && ! grep -Eq "(^|[[:space:]])(vmx|svm)([[:space:]]|$)" /proc/cpuinfo && echo virtualization-hidden')
  printf '%s' "$result" | python3 -c '
import json, sys
result = json.load(sys.stdin)
assert result["exit_code"] == 0, result
assert result.get("stdout", "").strip() == "virtualization-hidden", result
'
}

TARIT_API_KEY="$KEY" \
TARIT_LISTEN="127.0.0.1:$PORT" \
TARIT_RPC_ADDR="$BASE_URL" \
TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
TARIT_VMM_BIN="$VMM" \
TARIT_KERNEL="$KERNEL" \
TARIT_ROOTFS="$ROOTFS" \
TARIT_ROOTFS_READONLY=0 \
TARIT_ENABLE_NET="$ENABLE_NET" \
TARIT_SOCKET_DIR="$DIR/sockets" \
TARIT_DB="$DIR/fleet.db" \
TARIT_NET_STATE="$DIR/net-state.json" \
TARIT_CONFIG="$DIR/none.toml" \
TARIT_WARM_POOL=0 \
TARIT_MAX_VMS=1 \
TARIT_MAX_VCPUS=1 \
TARIT_MAX_MEMORY_MIB=512 \
TARIT_ADMISSION_TIMEOUT_MS=250 \
TARIT_REAP_ON_SHUTDOWN=true \
TARIT_PRODUCTION=0 \
RUST_LOG=taritd=info \
setsid "$TARITD" serve >"$DIR/taritd.log" 2>&1 &
TARITD_PID=$!
TARITD_PGID=$TARITD_PID

ready=0
for _ in $(seq 1 60); do
  if curl -fsS --max-time 1 "$BASE_URL/health" >/dev/null 2>&1; then
    ready=1
    break
  fi
  kill -0 "$TARITD_PID" 2>/dev/null || break
  sleep 0.25
done
if [ "$ready" -ne 1 ]; then
  echo "FAIL: taritd did not become healthy"
  tail -80 "$DIR/taritd.log"
  exit 1
fi
ACTUAL_PGID=$(ps -o pgid= -p "$TARITD_PID" | tr -d ' ')
[ "$ACTUAL_PGID" = "$TARITD_PGID" ] || {
  echo "FAIL: taritd did not start in its own process group"
  TARITD_PGID=
  exit 1
}

echo "== create and populate guest memory =="
CREATE_BODY="$DIR/create-vm.json"
CREATE_CODE=$(curl -sS --max-time 30 -o "$CREATE_BODY" -w '%{http_code}' \
  -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  -d '{"vcpus":1,"memory_mib":512}' "$BASE_URL/v1/vms")
if [ "$CREATE_CODE" != 201 ]; then
  echo "FAIL: VM create returned HTTP $CREATE_CODE" >&2
  sed -n '1,120p' "$CREATE_BODY" >&2
  exit 1
fi
VM_JSON=$(<"$CREATE_BODY")
VM_ID=$(printf '%s' "$VM_JSON" | json_field id)
printf '%s' "$VM_JSON" | grep -q '"status":"running"'
VMM_PID=$(vmm_pid_for_socket "$DIR/sockets/$VM_ID.sock")
kill -0 "$VMM_PID"
if [ "$ENABLE_NET" = 1 ]; then
  NET_TAP=""
  for _ in $(seq 1 40); do
    NET_TAP=$(ip -o link show | awk -F': ' '$2 ~ /^insta[0-9]+$/ { print $2; exit }')
    [ -n "$NET_TAP" ] && break
    sleep 0.1
  done
  [ -n "$NET_TAP" ] || {
    echo "FAIL: network-enabled VM has no TAP" >&2
    exit 1
  }
fi

if [ -n "$EXPECTED_KERNEL_PREFIX" ]; then
  KERNEL_IDENTITY=$(exec_json "$VM_ID" 'uname -r')
  printf '%s' "$KERNEL_IDENTITY" | python3 -c '
import json, sys
expected = sys.argv[1]
result = json.load(sys.stdin)
assert result["exit_code"] == 0, result
actual = result.get("stdout", "").strip()
assert actual.startswith(expected), (expected, actual)
' "$EXPECTED_KERNEL_PREFIX"
fi
if [ -n "$EXPECTED_OS_ID" ]; then
  # shellcheck disable=SC2016 # $ID expands inside the guest shell.
  OS_IDENTITY=$(exec_json "$VM_ID" '. /etc/os-release && printf "%s\n" "$ID"')
  printf '%s' "$OS_IDENTITY" | python3 -c '
import json, sys
expected = sys.argv[1]
result = json.load(sys.stdin)
assert result["exit_code"] == 0, result
actual = result.get("stdout", "").strip()
assert actual == expected, (expected, actual)
' "$EXPECTED_OS_ID"
fi
assert_guest_security "$VM_ID"
if [ "$ENABLE_NET" = 1 ]; then
  NETWORK_IDENTITY=$(exec_json "$VM_ID" \
    'test -d /sys/class/net/eth0 && grep -Eq "^eth0[[:space:]]+00000000[[:space:]]" /proc/net/route && cat /sys/class/net/eth0/address && echo network-ready')
  printf '%s' "$NETWORK_IDENTITY" | python3 -c '
import json, sys
result = json.load(sys.stdin)
assert result["exit_code"] == 0, result
output = result.get("stdout", "")
assert "network-ready" in output, result
'
fi

PREP=$(exec_json "$VM_ID" "mkdir -p /mnt/tarit-rss && mount -t tmpfs -o size=192m tmpfs /mnt/tarit-rss && dd if=/dev/zero of=/mnt/tarit-rss/fill bs=1M count=160 2>/dev/null && echo suspend-state-ok > /mnt/tarit-rss/state")
printf '%s' "$PREP" | grep -q '"exit_code":0'
RSS_BEFORE=$(rss_kib "$VMM_PID")

echo "== suspend and verify resource contract =="
SUSPENDED=$(vm_transition suspend)
printf '%s' "$SUSPENDED" | grep -q '"status":"suspended"'
if [ "$ENABLE_NET" = 1 ]; then
  ip link show "$NET_TAP" >/dev/null
fi

RSS_AFTER=$RSS_BEFORE
for _ in $(seq 1 20); do
  current=$(rss_kib "$VMM_PID")
  [ "$current" -lt "$RSS_AFTER" ] && RSS_AFTER=$current
  sleep 0.1
done
RSS_DROP=$((RSS_BEFORE - RSS_AFTER))
if [ "$RSS_DROP" -lt "$MIN_RSS_DROP_KIB" ] || [ $((RSS_AFTER * 100)) -gt $((RSS_BEFORE * 80)) ]; then
  echo "FAIL: suspend did not materially lower RSS (before=${RSS_BEFORE}KiB after=${RSS_AFTER}KiB drop=${RSS_DROP}KiB)"
  exit 1
fi

EXEC_CODE=$(curl -sS --max-time 10 -o "$DIR/suspended-exec.json" -w '%{http_code}' \
  -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  -d "{\"vm_id\":\"$VM_ID\",\"command\":\"true\"}" "$BASE_URL/v1/execute")
[ "$EXEC_CODE" = 409 ]
grep -q 'suspended' "$DIR/suspended-exec.json"

# Suspension releases RAM, not the admission reservation. With max_vms=1 a
# second create must still be rejected rather than oversubscribing the host.
CREATE_CODE=$(curl -sS --max-time 10 -o "$DIR/suspended-create.json" -w '%{http_code}' \
  -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  -d '{"vcpus":1,"memory_mib":128}' "$BASE_URL/v1/vms")
[ "$CREATE_CODE" = 429 ]

echo "== resume, first exec, and verify preserved state =="
START_MS=$(monotonic_ms)
RESUMED=$(vm_transition resume)
printf '%s' "$RESUMED" | grep -q '"status":"running"'
if [ "$ENABLE_NET" = 1 ]; then
  ip link show "$NET_TAP" >/dev/null
fi
FIRST_EXEC=$(exec_json "$VM_ID" 'cat /mnt/tarit-rss/state')
END_MS=$(monotonic_ms)
printf '%s' "$FIRST_EXEC" | grep -q 'suspend-state-ok'
printf '%s' "$FIRST_EXEC" | grep -q '"exit_code":0'
assert_guest_security "$VM_ID"
if [ "$ENABLE_NET" = 1 ]; then
  exec_json "$VM_ID" \
    'test -d /sys/class/net/eth0 && grep -Eq "^eth0[[:space:]]+00000000[[:space:]]" /proc/net/route && cat /sys/class/net/eth0/address && echo network-ready' | \
    python3 -c '
import json, sys
result = json.load(sys.stdin)
assert result["exit_code"] == 0, result
output = result.get("stdout", "")
assert "network-ready" in output, result
'
fi
RESUME_EXEC_MS=$((END_MS - START_MS))
[ "$RESUME_EXEC_MS" -le "$MAX_RESUME_EXEC_MS" ] || {
  echo "FAIL: resume-to-first-exec ${RESUME_EXEC_MS}ms exceeded ${MAX_RESUME_EXEC_MS}ms"
  exit 1
}

echo "== repeated transitions are idempotent =="
for _ in 1 2; do
  vm_transition suspend | grep -q '"status":"suspended"'
done
for _ in 1 2; do
  vm_transition resume | grep -q '"status":"running"'
done
exec_json "$VM_ID" 'cat /mnt/tarit-rss/state' | grep -q 'suspend-state-ok'

echo "== rapid suspend/resume transitions preserve worker handshakes =="
for cycle in $(seq 1 20); do
  vm_transition suspend | grep -q '"status":"suspended"'
  if [ "$ENABLE_NET" = 1 ]; then
    ip link show "$NET_TAP" >/dev/null
  fi
  vm_transition resume | grep -q '"status":"running"'
  exec_json "$VM_ID" "printf rapid-cycle-$cycle" | grep -q "rapid-cycle-$cycle"
done
exec_json "$VM_ID" 'cat /mnt/tarit-rss/state' | grep -q 'suspend-state-ok'
assert_guest_security "$VM_ID"

api -X DELETE "$BASE_URL/v1/vms/$VM_ID" >/dev/null
if [ "$ENABLE_NET" = 1 ]; then
  for _ in $(seq 1 40); do
    ip link show "$NET_TAP" >/dev/null 2>&1 || break
    sleep 0.1
  done
  if ip link show "$NET_TAP" >/dev/null 2>&1; then
    echo "FAIL: TAP leaked after VM deletion: $NET_TAP" >&2
    exit 1
  fi
fi
echo "RESULT: SUSPEND_PASS transition_client=$TRANSITION_CLIENT rss_before_kib=$RSS_BEFORE rss_after_kib=$RSS_AFTER rss_drop_kib=$RSS_DROP resume_first_exec_ms=$RESUME_EXEC_MS"
