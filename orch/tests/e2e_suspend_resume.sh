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

for required in curl python3 setsid ps awk; do
  command -v "$required" >/dev/null || {
    echo "FAIL: required command '$required' is missing" >&2
    exit 1
  }
done
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
  rm -rf -- "$DIR"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

api() {
  curl -fsS --max-time 30 -H "X-API-Key: $KEY" "$@"
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

TARIT_API_KEY="$KEY" \
TARIT_LISTEN="127.0.0.1:$PORT" \
TARIT_RPC_ADDR="$BASE_URL" \
TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
TARIT_VMM_BIN="$VMM" \
TARIT_KERNEL="$KERNEL" \
TARIT_ROOTFS="$ROOTFS" \
TARIT_ROOTFS_READONLY=0 \
TARIT_ENABLE_NET=0 \
TARIT_SOCKET_DIR="$DIR/sockets" \
TARIT_DB="$DIR/fleet.db" \
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
VM_JSON=$(api -H 'Content-Type: application/json' -d '{"vcpus":1,"memory_mib":512}' "$BASE_URL/v1/vms")
VM_ID=$(printf '%s' "$VM_JSON" | json_field id)
printf '%s' "$VM_JSON" | grep -q '"status":"running"'
VMM_PID=$(vmm_pid_for_socket "$DIR/sockets/$VM_ID.sock")
kill -0 "$VMM_PID"

PREP=$(exec_json "$VM_ID" "mkdir -p /mnt/tarit-rss && mount -t tmpfs -o size=192m tmpfs /mnt/tarit-rss && dd if=/dev/zero of=/mnt/tarit-rss/fill bs=1M count=160 2>/dev/null && echo suspend-state-ok > /mnt/tarit-rss/state")
printf '%s' "$PREP" | grep -q '"exit_code":0'
RSS_BEFORE=$(rss_kib "$VMM_PID")

echo "== suspend and verify resource contract =="
SUSPENDED=$(api -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$VM_ID/suspend")
printf '%s' "$SUSPENDED" | grep -q '"status":"suspended"'

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
RESUMED=$(api -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$VM_ID/resume")
printf '%s' "$RESUMED" | grep -q '"status":"running"'
FIRST_EXEC=$(exec_json "$VM_ID" 'cat /mnt/tarit-rss/state')
END_MS=$(monotonic_ms)
printf '%s' "$FIRST_EXEC" | grep -q 'suspend-state-ok'
printf '%s' "$FIRST_EXEC" | grep -q '"exit_code":0'
RESUME_EXEC_MS=$((END_MS - START_MS))
[ "$RESUME_EXEC_MS" -le "$MAX_RESUME_EXEC_MS" ] || {
  echo "FAIL: resume-to-first-exec ${RESUME_EXEC_MS}ms exceeded ${MAX_RESUME_EXEC_MS}ms"
  exit 1
}

echo "== repeated transitions are idempotent =="
for _ in 1 2; do
  api -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$VM_ID/suspend" | grep -q '"status":"suspended"'
done
for _ in 1 2; do
  api -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$VM_ID/resume" | grep -q '"status":"running"'
done
exec_json "$VM_ID" 'cat /mnt/tarit-rss/state' | grep -q 'suspend-state-ok'

api -X DELETE "$BASE_URL/v1/vms/$VM_ID" >/dev/null
echo "RESULT: SUSPEND_PASS rss_before_kib=$RSS_BEFORE rss_after_kib=$RSS_AFTER rss_drop_kib=$RSS_DROP resume_first_exec_ms=$RESUME_EXEC_MS"
