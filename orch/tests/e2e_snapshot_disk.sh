#!/usr/bin/env bash
# Prove that a snapshot owns its disk state and that each restore receives a
# separate writable upper. Requires Linux, KVM, root, and prepared guest assets.
set -euo pipefail

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD="${TARITD_BIN:-$ROOT/orch/target/release/taritd}"
VMM="${TARIT_VMM_BIN:-$ROOT/vmm/target/release/vmm}"
KERNEL="${TARIT_KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${TARIT_ROOTFS:-/tmp/vsock-rootfs.ext4}"
KEY="snapshot-disk-e2e-key"
PORT="${SNAPSHOT_DISK_E2E_PORT:-}"

for required in curl python3 setsid ps; do
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
DIR=$(mktemp -d "${TMPDIR:-/tmp}/tarit-snapshot-disk-e2e.XXXXXX")
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
  fi
  [ -z "${TARITD_PID:-}" ] || wait "$TARITD_PID" 2>/dev/null || true
  rm -rf -- "$DIR"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

api() {
  curl -fsS --max-time 60 -H "X-API-Key: $KEY" "$@"
}

json_field() {
  python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"
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
TARIT_MAX_VMS=4 \
TARIT_MAX_VCPUS=4 \
TARIT_MAX_MEMORY_MIB=2048 \
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

echo "== create source and checkpoint disk =="
SOURCE_JSON=$(api -H 'Content-Type: application/json' \
  -d '{"vcpus":1,"memory_mib":256}' "$BASE_URL/v1/vms")
SOURCE_ID=$(printf '%s' "$SOURCE_JSON" | json_field id)
exec_json "$SOURCE_ID" "sh -c 'echo snapshot-checkpoint > /root/tarit-snapshot-state; sync'" |
  grep -q '"exit_code":0'

echo "== snapshot, mutate source, then delete it =="
SNAPSHOT_START_MS=$(python3 -c 'import time; print(time.monotonic_ns() // 1000000)')
SNAPSHOT_JSON=$(api -H 'Content-Type: application/json' -d '{"diff":false}' \
  "$BASE_URL/v1/vms/$SOURCE_ID/snapshot")
SNAPSHOT_END_MS=$(python3 -c 'import time; print(time.monotonic_ns() // 1000000)')
SNAPSHOT_CAPTURE_MS=$((SNAPSHOT_END_MS - SNAPSHOT_START_MS))
SNAPSHOT_PATH=$(printf '%s' "$SNAPSHOT_JSON" | json_field path)
[ -f "$SNAPSHOT_PATH" ]
exec_json "$SOURCE_ID" "sh -c 'echo post-snapshot-mutation > /root/tarit-snapshot-state; sync'" |
  grep -q '"exit_code":0'
api -X DELETE "$BASE_URL/v1/vms/$SOURCE_ID" >/dev/null
[ ! -e "$DIR/sockets/overlays/$SOURCE_ID.cow" ] || {
  echo "FAIL: source VM overlay survived deletion"
  exit 1
}

echo "== restore twice from the snapshot-owned disk artifact =="
RESTORE_A=$(api -H 'Content-Type: application/json' \
  -d "$(python3 -c 'import json,sys; print(json.dumps({"snapshot_path":sys.argv[1]}))' "$SNAPSHOT_PATH")" \
  "$BASE_URL/v1/restore")
RESTORE_B=$(api -H 'Content-Type: application/json' \
  -d "$(python3 -c 'import json,sys; print(json.dumps({"snapshot_path":sys.argv[1]}))' "$SNAPSHOT_PATH")" \
  "$BASE_URL/v1/restore")
A_ID=$(printf '%s' "$RESTORE_A" | json_field id)
B_ID=$(printf '%s' "$RESTORE_B" | json_field id)
[ "$A_ID" != "$B_ID" ]
[ -f "$DIR/sockets/overlays/$A_ID.cow" ]
[ -f "$DIR/sockets/overlays/$B_ID.cow" ]

exec_json "$A_ID" 'cat /root/tarit-snapshot-state' | grep -q 'snapshot-checkpoint'
exec_json "$B_ID" 'cat /root/tarit-snapshot-state' | grep -q 'snapshot-checkpoint'
exec_json "$A_ID" "sh -c 'echo restore-a-private > /root/tarit-snapshot-state; sync'" |
  grep -q '"exit_code":0'
exec_json "$A_ID" 'cat /root/tarit-snapshot-state' | grep -q 'restore-a-private'
B_STATE=$(exec_json "$B_ID" 'cat /root/tarit-snapshot-state')
printf '%s' "$B_STATE" | grep -q 'snapshot-checkpoint'
if printf '%s' "$B_STATE" | grep -q 'restore-a-private'; then
  echo "FAIL: restored VMs shared writable disk state"
  exit 1
fi

api -X DELETE "$BASE_URL/v1/vms/$A_ID" >/dev/null
api -X DELETE "$BASE_URL/v1/vms/$B_ID" >/dev/null
echo "RESULT: SNAPSHOT_DISK_PASS source=$SOURCE_ID restore_a=$A_ID restore_b=$B_ID snapshot_capture_ms=$SNAPSHOT_CAPTURE_MS"
