#!/usr/bin/env bash
# Real-KVM recovery gate for process death after a forked child is durable but
# before the source-bound fork operation is committed or acknowledged.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD="${TARITD_BIN:?set TARITD_BIN to a test-failpoint taritd build}"
VMM="${TARIT_VMM_BIN:-$ROOT/vmm/target/release/vmm}"
KERNEL="${TARIT_KERNEL:?set TARIT_KERNEL to a KVM guest kernel}"
ROOTFS="${TARIT_ROOTFS:?set TARIT_ROOTFS to an agent-enabled OCI rootfs}"
EXPECTED_OS_ID="${TARIT_EXPECT_OS_ID:?set TARIT_EXPECT_OS_ID}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"
KEY="fork-process-death-e2e-key"

for required in curl python3 setsid sqlite3; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done
[ "$(id -u)" -eq 0 ] || { echo "FAIL: process-death gate must run as root" >&2; exit 1; }
test -x "$TARITD" && test -x "$VMM" && test -r "$KERNEL" && test -r "$ROOTFS"
[ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]
[[ "$EXPECTED_OS_ID" =~ ^[a-z0-9._-]+$ ]]

PORT=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
DIR=$(mktemp -d "$SOCKET_ROOT/tarit-fork-death.XXXXXX")
chmod 700 "$DIR"
mkdir -m 700 "$DIR/sockets" "$DIR/runtime" "$DIR/jails"
BASE_URL="http://127.0.0.1:$PORT"
TARITD_PID=""
TARITD_PGID=""
CHILD_ID=""

cleanup() {
  local status=$?
  if [ -n "$CHILD_ID" ]; then
    curl -fsS --max-time 5 -X DELETE -H "X-API-Key: $KEY" \
      "$BASE_URL/v1/vms/$CHILD_ID" >/dev/null 2>&1 || true
  fi
  if [ -n "$TARITD_PGID" ] && kill -0 -- "-$TARITD_PGID" 2>/dev/null; then
    kill -TERM -- "-$TARITD_PGID" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 -- "-$TARITD_PGID" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL -- "-$TARITD_PGID" 2>/dev/null || true
  fi
  [ -z "$TARITD_PID" ] || wait "$TARITD_PID" 2>/dev/null || true
  if [ "$status" -ne 0 ]; then
    echo "FAIL: fork process-death gate exited $status" >&2
    tail -200 "$DIR/taritd.log" >&2 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ] && [ "${TARIT_E2E_KEEP_FAILED:-0}" = 1 ]; then
    echo "FAIL: retained diagnostic directory: $DIR" >&2
  else
    find "$DIR" -depth -delete 2>/dev/null || true
  fi
  local mount_target
  mount_target=$(findmnt -n -o TARGET -T "$SOCKET_ROOT" 2>/dev/null || true)
  if [ -n "$mount_target" ] && command -v fstrim >/dev/null 2>&1; then
    sync -f "$SOCKET_ROOT" >/dev/null 2>&1 || true
    fstrim "$mount_target" >/dev/null 2>&1 || true
  fi
  return "$status"
}
trap cleanup EXIT

printf x >"$DIR/.reflink-source"
cp --reflink=always "$DIR/.reflink-source" "$DIR/.reflink-clone"
rm -f -- "$DIR/.reflink-source" "$DIR/.reflink-clone"
cp --reflink=always --sparse=auto -- "$ROOTFS" "$DIR/rootfs.ext4"
chmod 0444 "$DIR/rootfs.ext4"
ROOTFS="$DIR/rootfs.ext4"

start_taritd() {
  local pause_ms=$1
  TARIT_API_KEY="$KEY" \
  TARIT_LISTEN="127.0.0.1:$PORT" \
  TARIT_RPC_ADDR="$BASE_URL" \
  TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
  TARIT_HOST_ID=fork-death-c8i \
  TARIT_VMM_BIN="$VMM" \
  TARIT_KERNEL="$KERNEL" \
  TARIT_ROOTFS="$ROOTFS" \
  TARIT_ROOTFS_READONLY=0 \
  TARIT_ENABLE_NET=0 \
  TARIT_SOCKET_DIR="$DIR/sockets" \
  TARIT_DB="$DIR/fleet.db" \
  TARIT_CONFIG="$DIR/missing.toml" \
  TARIT_WARM_POOL=0 \
  TARIT_MAX_VMS=3 \
  TARIT_MAX_VCPUS=3 \
  TARIT_MAX_MEMORY_MIB=768 \
  TARIT_ADMISSION_TIMEOUT_MS=1000 \
  TARIT_VM_JAIL_BASE="$DIR/jails" \
  TARIT_VM_JAIL_UID_BASE=300000 \
  TARIT_VM_JAIL_GID_BASE=310000 \
  TARIT_VM_JAIL_ID_COUNT=3 \
  TARIT_VM_JAIL_SECCOMP=1 \
  TARIT_VM_JAIL_PID_NAMESPACE=1 \
  TARIT_VM_JAIL_NETWORK_NAMESPACE=1 \
  TARIT_REAP_ON_SHUTDOWN=true \
  TARIT_PRODUCTION=0 \
  TARIT_TEST_FORK_PAUSE_AFTER_CHILD_MS="$pause_ms" \
  RUST_LOG=taritd=info \
  TMPDIR="$DIR/runtime" \
  setsid "$TARITD" serve >>"$DIR/taritd.log" 2>&1 &
  TARITD_PID=$!
  TARITD_PGID=$TARITD_PID
  for _ in $(seq 1 100); do
    curl -fsS --max-time 1 "$BASE_URL/health" >/dev/null 2>&1 && return 0
    kill -0 "$TARITD_PID" 2>/dev/null || return 1
    sleep 0.2
  done
  return 1
}

api() {
  local method=$1 path=$2 body=${3:-}
  if [ -n "$body" ]; then
    curl -fsS --max-time 90 -X "$method" -H "X-API-Key: $KEY" \
      -H 'Content-Type: application/json' -d "$body" "$BASE_URL$path"
  else
    curl -fsS --max-time 90 -X "$method" -H "X-API-Key: $KEY" "$BASE_URL$path"
  fi
}

wait_exec() {
  local vm_id=$1 command=$2 expected=$3 output="$DIR/exec.json"
  local body
  body=$(python3 -c \
    'import json,sys; print(json.dumps({"vm_id":sys.argv[1],"command":sys.argv[2],"timeout_ms":30000}))' \
    "$vm_id" "$command")
  for _ in $(seq 1 90); do
    if api POST /v1/execute "$body" >"$output" 2>/dev/null && \
      python3 - "$output" "$expected" <<'PY'
import json,sys
row=json.load(open(sys.argv[1]))
assert row.get("status") == "completed" and row.get("exit_code") == 0, row
assert sys.argv[2] in row.get("stdout", ""), row
PY
    then
      return 0
    fi
    sleep 1
  done
  return 1
}

start_taritd 30000
SOURCE_ID=$(api POST /v1/vms '{"vcpus":1,"memory_mib":256}' | \
  python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["status"] == "running", row; print(row["id"])')
wait_exec "$SOURCE_ID" \
  "grep '^ID=$EXPECTED_OS_ID$' /etc/os-release && printf process-death-proof > /root/fork-proof && sync && echo seeded" \
  seeded
CHILD_ID=$(python3 - <<'PY'
import uuid
print(uuid.uuid4())
PY
)

curl -sS --max-time 90 -o "$DIR/first-fork.json" -w '%{http_code}' \
  -X POST -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  -d "{\"id\":\"$CHILD_ID\"}" "$BASE_URL/v1/vms/$SOURCE_ID/fork" \
  >"$DIR/first-fork-status" &
FORK_CURL_PID=$!

for _ in $(seq 1 900); do
  phase=$(sqlite3 "$DIR/fleet.db" \
    "select status from vm_fork_operations where child_vm_id='$CHILD_ID';" 2>/dev/null || true)
  child_status=$(sqlite3 "$DIR/fleet.db" \
    "select status from vms where id='$CHILD_ID';" 2>/dev/null || true)
  if [ "$phase" = preparing ] && [ "$child_status" = running ] && \
     grep -q 'test fork paused after child persistence' "$DIR/taritd.log"; then
    break
  fi
  sleep 0.1
done
[ "$phase" = preparing ] && [ "$child_status" = running ]

kill -KILL "$TARITD_PID"
wait "$TARITD_PID" 2>/dev/null || true
wait "$FORK_CURL_PID" 2>/dev/null || true
TARITD_PID=""
TARITD_PGID=""

start_taritd 0
api DELETE "/v1/vms/$SOURCE_ID" >/dev/null
RETRY_STATUS=$(curl -sS --max-time 90 -o "$DIR/retry.json" -w '%{http_code}' \
  -X POST -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  -d "{\"id\":\"$CHILD_ID\"}" "$BASE_URL/v1/vms/$SOURCE_ID/fork")
[ "$RETRY_STATUS" = 200 ]
python3 - "$DIR/retry.json" "$SOURCE_ID" "$CHILD_ID" <<'PY'
import json,sys
row=json.load(open(sys.argv[1]))
assert row.get("source_vm_id") == sys.argv[2], row
assert row.get("vm", {}).get("id") == sys.argv[3], row
assert row["vm"].get("status") == "running", row
PY
[ "$(sqlite3 "$DIR/fleet.db" "select count(*) from vms where id='$CHILD_ID';")" = 1 ]
[ "$(sqlite3 "$DIR/fleet.db" "select status from vm_fork_operations where child_vm_id='$CHILD_ID';")" = committed ]
[ "$(sqlite3 "$DIR/fleet.db" \
  "select count(*) from vm_fork_operations f join vms v on v.id=f.child_vm_id where f.child_vm_id='$CHILD_ID' and f.child_created_at=v.created_at;")" = 1 ]
wait_exec "$CHILD_ID" 'cat /root/fork-proof' process-death-proof

WRONG_SOURCE=$(python3 - <<'PY'
import uuid
print(uuid.uuid4())
PY
)
WRONG_STATUS=$(curl -sS --max-time 30 -o "$DIR/wrong-source.json" -w '%{http_code}' \
  -X POST -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  -d "{\"id\":\"$CHILD_ID\"}" "$BASE_URL/v1/vms/$WRONG_SOURCE/fork")
[ "$WRONG_STATUS" = 409 ]

echo "FORK_PROCESS_DEATH_PASS os=$EXPECTED_OS_ID kernel=$(basename "$(dirname "$KERNEL")")"
