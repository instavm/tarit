#!/usr/bin/env bash
# Real-KVM acceptance gate for atomic live fork and scale-to-zero activation.
set -Eeuo pipefail

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD="${TARITD_BIN:-$ROOT/orch/target/release/taritd}"
VMM="${TARIT_VMM_BIN:-$ROOT/vmm/target/release/vmm}"
KERNEL="${TARIT_KERNEL:?set TARIT_KERNEL to a KVM guest kernel}"
ROOTFS="${TARIT_ROOTFS:?set TARIT_ROOTFS to an agent-enabled rootfs}"
EXPECTED_OS_ID="${TARIT_EXPECT_OS_ID:-ubuntu}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"
CLONE_WORKLOAD_BIN="${TARIT_TEST_CLONE_WORKLOAD_BIN:-}"
GUEST_AGENT_BIN="${TARIT_TEST_GUEST_AGENT_BIN:-}"
KEY="live-fork-hibernate-e2e-key"
PORT="${FORK_HIBERNATE_E2E_PORT:-}"

for required in cmp cp curl python3 setsid pgrep ssh ssh-keygen; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done
[ "$(id -u)" -eq 0 ] || { echo "FAIL: live fork/hibernate gate must run as root" >&2; exit 1; }
[ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ] || {
  echo "FAIL: worker /dev/kvm is unavailable" >&2
  exit 1
}
[[ "$EXPECTED_OS_ID" =~ ^[a-z0-9._-]+$ ]] || {
  echo "FAIL: TARIT_EXPECT_OS_ID contains unsupported characters" >&2
  exit 1
}
if [ -z "$PORT" ]; then
  PORT=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
fi
SSH_PORT=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
SHARE_PORT=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
DIR=$(mktemp -d "$SOCKET_ROOT/tarit-fork-hibernate.XXXXXX")
chmod 700 "$DIR"
mkdir -m 700 "$DIR/sockets"
mkdir -m 700 "$DIR/runtime"
ROOTFS_MOUNT=""
BASE_URL="http://127.0.0.1:$PORT"

cleanup() {
  if [ -n "$ROOTFS_MOUNT" ] && mountpoint -q "$ROOTFS_MOUNT"; then
    umount "$ROOTFS_MOUNT" || true
  fi
  if [ -n "${TARITD_PGID:-}" ] && kill -0 -- "-$TARITD_PGID" 2>/dev/null; then
    kill -TERM -- "-$TARITD_PGID" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 -- "-$TARITD_PGID" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL -- "-$TARITD_PGID" 2>/dev/null || true
  fi
  [ -z "${TARITD_PID:-}" ] || wait "$TARITD_PID" 2>/dev/null || true
  if [ "${TARIT_TEST_KEEP_DIR:-0}" = 1 ]; then
    echo "retained E2E artifacts at $DIR" >&2
  else
    find "$DIR" -depth -delete 2>/dev/null || true
  fi
  mount_target=$(findmnt -n -o TARGET -T "$SOCKET_ROOT" 2>/dev/null || true)
  if [ -n "$mount_target" ] && command -v fstrim >/dev/null 2>&1; then
    sync -f "$SOCKET_ROOT" >/dev/null 2>&1 || true
    fstrim "$mount_target" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT
on_error() {
  local status=$?
  echo "FAIL: live fork/hibernate gate exited $status" >&2
  [ ! -f "$DIR/taritd.log" ] || tail -120 "$DIR/taritd.log" >&2
  return "$status"
}
trap on_error ERR
trap 'exit 130' INT
trap 'exit 143' TERM

printf x >"$DIR/.reflink-source"
if ! cp --reflink=always "$DIR/.reflink-source" "$DIR/.reflink-clone" 2>/dev/null; then
  echo "FAIL: TARIT_TEST_SOCKET_ROOT must be on a reflink-capable filesystem" >&2
  exit 1
fi
rm -f -- "$DIR/.reflink-source" "$DIR/.reflink-clone"
STAGED_ROOTFS="$DIR/rootfs.ext4"
cp --reflink=auto --sparse=always -- "$ROOTFS" "$STAGED_ROOTFS"
cmp -s -- "$ROOTFS" "$STAGED_ROOTFS" || {
  echo "FAIL: staged guest rootfs differs from the source" >&2
  exit 1
}
if [ -n "$CLONE_WORKLOAD_BIN" ] || [ -n "$GUEST_AGENT_BIN" ]; then
  for required in e2fsck install mount mountpoint umount; do
    command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
  done
  [ -z "$CLONE_WORKLOAD_BIN" ] || test -x "$CLONE_WORKLOAD_BIN" || {
    echo "FAIL: clone-repair workload is not executable: $CLONE_WORKLOAD_BIN" >&2
    exit 1
  }
  [ -z "$GUEST_AGENT_BIN" ] || test -x "$GUEST_AGENT_BIN" || {
    echo "FAIL: guest agent is not executable: $GUEST_AGENT_BIN" >&2
    exit 1
  }
  ROOTFS_MOUNT="$DIR/rootfs-mount"
  mkdir -m 700 "$ROOTFS_MOUNT"
  mount -o loop,rw "$STAGED_ROOTFS" "$ROOTFS_MOUNT"
  if [ -n "$CLONE_WORKLOAD_BIN" ]; then
    install -D -m 0755 "$CLONE_WORKLOAD_BIN" \
      "$ROOTFS_MOUNT/usr/local/bin/tarit-clone-repair-workload"
    sync -f "$ROOTFS_MOUNT/usr/local/bin/tarit-clone-repair-workload"
  fi
  if [ -n "$GUEST_AGENT_BIN" ]; then
    install -D -m 0755 "$GUEST_AGENT_BIN" "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
    sync -f "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
  fi
  umount "$ROOTFS_MOUNT"
  ROOTFS_MOUNT=""
  e2fsck -pf "$STAGED_ROOTFS" >/dev/null
fi
chmod 0444 "$STAGED_ROOTFS"
ROOTFS="$STAGED_ROOTFS"

api() { curl -fsS --max-time 90 -H "X-API-Key: $KEY" "$@"; }
json_field() { python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"; }
now_ms() { python3 -c 'import time; print(time.monotonic_ns() // 1000000)'; }
exec_request() {
  local vm_id=$1 command=$2
  api -H 'Content-Type: application/json' \
    -d "$(python3 -c 'import json,sys; print(json.dumps({"vm_id":sys.argv[1],"command":sys.argv[2],"timeout_ms":30000}))' "$vm_id" "$command")" \
    "$BASE_URL/v1/execute"
}
expect_exec() {
  local vm_id=$1 command=$2 expected=$3
  exec_request "$vm_id" "$command" | python3 -c '
import json,sys
r=json.load(sys.stdin); expected=sys.argv[1]
assert r["exit_code"] == 0, r
assert expected in r.get("stdout", ""), r
' "$expected"
}
vmm_pids_for_id() {
  local id=$1
  pgrep -f -- "${VMM} serve .*--socket ${DIR}/sockets/${id}\.sock" || true
}

start_taritd() {
  TARIT_API_KEY="$KEY" \
  TARIT_LISTEN="127.0.0.1:$PORT" \
  TARIT_RPC_ADDR="$BASE_URL" \
  TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
  TARIT_VMM_BIN="$VMM" \
  TARIT_KERNEL="$KERNEL" \
  TARIT_ROOTFS="$ROOTFS" \
  TARIT_ROOTFS_READONLY=0 \
  TARIT_ENABLE_NET="${TARIT_TEST_ENABLE_NET:-0}" \
  TARIT_UPLINK="${TARIT_TEST_UPLINK:-eth0}" \
  TARIT_SSH_GATEWAY=1 \
  TARIT_SSH_GATEWAY_ADDR="127.0.0.1:$SSH_PORT" \
  TARIT_SSH_GATEWAY_HOST_KEY="$DIR/ssh-host-ed25519" \
  TARIT_SHARE_LISTEN="127.0.0.1:$SHARE_PORT" \
  TARIT_SHARE_DOMAIN=shares.e2e.test \
  TARIT_SHARE_TOKEN_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA \
  TARIT_SOCKET_DIR="$DIR/sockets" \
  TARIT_DB="$DIR/fleet.db" \
  TARIT_CONFIG="$DIR/none.toml" \
  TARIT_WARM_POOL=0 \
  TARIT_MAX_VMS=6 \
  TARIT_MAX_VCPUS=6 \
  TARIT_MAX_MEMORY_MIB=1536 \
  TARIT_ADMISSION_TIMEOUT_MS=1000 \
  TARIT_REAP_ON_SHUTDOWN=true \
  TARIT_PRODUCTION=0 \
  RUST_LOG=taritd=info,taritd::gateway=debug,vmm_devices::virtio::vsock=debug,vmm_devices::virtio::vsock_io_loop=debug,vmm_core=info \
  TMPDIR="$DIR/runtime" \
  setsid "$TARITD" serve >>"$DIR/taritd.log" 2>&1 &
  TARITD_PID=$!
  TARITD_PGID=$TARITD_PID

  for _ in $(seq 1 80); do
    curl -fsS --max-time 1 "$BASE_URL/health" >/dev/null 2>&1 && break
    kill -0 "$TARITD_PID" 2>/dev/null || { tail -100 "$DIR/taritd.log"; exit 1; }
    sleep 0.25
  done
  curl -fsS "$BASE_URL/health" >/dev/null
}

stop_taritd() {
  kill -TERM "$TARITD_PID"
  wait "$TARITD_PID"
  TARITD_PID=
  TARITD_PGID=
}

start_taritd
ssh-keygen -q -t ed25519 -N '' -f "$DIR/ssh-client-ed25519"
SSH_PUBLIC_KEY=$(cat "$DIR/ssh-client-ed25519.pub")
register_ssh_key() {
  api -H 'Content-Type: application/json' \
    -d "$(python3 -c 'import json,sys; print(json.dumps({"public_key":sys.argv[1]}))' "$SSH_PUBLIC_KEY")" \
    "$BASE_URL/v1/ssh-keys" | grep -q '"fingerprint"'
}
register_ssh_key

echo "== live fork preserves RAM/disk state and isolates future writes =="
PARENT_JSON=$(api -H 'Content-Type: application/json' -d '{"vcpus":1,"memory_mib":256}' "$BASE_URL/v1/vms")
PARENT_ID=$(printf '%s' "$PARENT_JSON" | json_field id)
expect_exec "$PARENT_ID" "grep '^ID=$EXPECTED_OS_ID$' /etc/os-release" "ID=$EXPECTED_OS_ID"
echo "== cold-boot SSH PTY control =="
exec_request "$PARENT_ID" \
  "sh -c 'echo VSOCK_DIAG; grep -E \"vsock|vmw_vsock\" /proc/modules || true; ls -l /proc/net/vsock 2>&1 || true; ps -ef | grep [v]mm-agent'" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin).get("stdout", ""))'
SSH_COLD_OUTPUT=$(python3 "$ROOT/orch/tests/ssh_pty_test.py" \
  "$DIR/ssh-client-ed25519" "$SSH_PORT" "$PARENT_ID" 127.0.0.1 2>&1) || {
  printf '%s\n' "$SSH_COLD_OUTPUT" >&2
  echo "FAIL: cold-boot SSH PTY control failed" >&2
  exit 1
}
printf '%s\n' "$SSH_COLD_OUTPUT" | grep -q 'SSH_GW_PASS'
expect_exec "$PARENT_ID" "sh -c 'echo parent-before-fork > /root/tarit-fork-state; sync; echo memory-before-fork > /tmp/tarit-memory-state'" ""
FORK_START=$(now_ms)
FORK_BODY="$DIR/fork-response.json"
FORK_STATUS=$(curl -sS --max-time 90 -o "$FORK_BODY" -w '%{http_code}' -H "X-API-Key: $KEY" \
  -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$PARENT_ID/fork")
if [ "$FORK_STATUS" != 201 ]; then
  echo "FAIL: live fork returned HTTP $FORK_STATUS: $(cat "$FORK_BODY")" >&2
  tail -160 "$DIR/taritd.log" >&2
  exit 1
fi
FORK_JSON=$(cat "$FORK_BODY")
FORK_END=$(now_ms)
CHILD_ID=$(printf '%s' "$FORK_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["vm"]["id"])')
printf '%s' "$FORK_JSON" | grep -q '"status":"running"'
expect_exec "$PARENT_ID" 'cat /root/tarit-fork-state /tmp/tarit-memory-state' 'parent-before-fork'
expect_exec "$CHILD_ID" 'cat /root/tarit-fork-state /tmp/tarit-memory-state' 'memory-before-fork'
expect_exec "$PARENT_ID" "sh -c 'echo parent-after > /root/tarit-fork-state'" ""
expect_exec "$CHILD_ID" "sh -c 'echo child-after > /root/tarit-fork-state'" ""
expect_exec "$PARENT_ID" 'cat /root/tarit-fork-state' 'parent-after'
expect_exec "$CHILD_ID" 'cat /root/tarit-fork-state' 'child-after'
echo "live_fork_ready_ms=$((FORK_END-FORK_START))"

if [ -n "$CLONE_WORKLOAD_BIN" ]; then
  echo "== long-lived application state is repaired before clone admission =="
  expect_exec "$PARENT_ID" \
    "sh -c '/usr/local/bin/tarit-clone-repair-workload serve >/tmp/clone-workload.log 2>&1 & for i in \$(seq 1 100); do /usr/local/bin/tarit-clone-repair-workload state >/dev/null 2>&1 && break; sleep 0.05; done; /usr/local/bin/tarit-clone-repair-workload state'" \
    'clone=cold-boot'
  expect_exec "$PARENT_ID" \
    "mkdir -p /usr/libexec/tarit; printf '%s\\n' '#!/bin/sh' 'set -eu' 'test \"\$TARIT_POST_FORK\" = 1' 'exec /usr/local/bin/tarit-clone-repair-workload repair-signal \"\$TARIT_CLONE_ID\"' > /usr/libexec/tarit/post-fork; chmod 0755 /usr/libexec/tarit/post-fork; /usr/local/bin/tarit-clone-repair-workload cache inherited-session" \
    'stored'
  SOURCE_WORKLOAD_STATE=$(exec_request "$PARENT_ID" \
    '/usr/local/bin/tarit-clone-repair-workload state' | json_field stdout)
  SOURCE_TICKET=$(exec_request "$PARENT_ID" \
    '/usr/local/bin/tarit-clone-repair-workload ticket' | json_field stdout | tr -d '\r\n')
  SOURCE_NONCE=$(exec_request "$PARENT_ID" \
    '/usr/local/bin/tarit-clone-repair-workload issue' | json_field stdout | tr -d '\r\n')
  APP_FORK_JSON=$(api -H 'Content-Type: application/json' -d '{}' \
    "$BASE_URL/v1/vms/$PARENT_ID/fork")
  APP_CHILD_ID=$(printf '%s' "$APP_FORK_JSON" | python3 -c \
    'import json,sys; row=json.load(sys.stdin); assert row["vm"]["status"] == "running", row; print(row["vm"]["id"])')
  CHILD_WORKLOAD_STATE=$(exec_request "$APP_CHILD_ID" \
    '/usr/local/bin/tarit-clone-repair-workload state' | json_field stdout)
  CHILD_NONCE=$(exec_request "$APP_CHILD_ID" \
    '/usr/local/bin/tarit-clone-repair-workload issue' | json_field stdout | tr -d '\r\n')
  PARENT_WORKLOAD_STATE=$(exec_request "$PARENT_ID" \
    '/usr/local/bin/tarit-clone-repair-workload state' | json_field stdout)
  python3 - "$SOURCE_WORKLOAD_STATE" "$PARENT_WORKLOAD_STATE" \
    "$CHILD_WORKLOAD_STATE" "$SOURCE_NONCE" "$CHILD_NONCE" <<'PY'
import sys

def fields(value):
    return dict(item.split("=", 1) for item in value.split())

source, parent, child = map(fields, sys.argv[1:4])
source_nonce, child_nonce = sys.argv[4:]
assert source["clone"] == "cold-boot", source
assert parent["clone"] == "cold-boot", parent
assert child["clone"] not in {"cold-boot", source["clone"]}, child
for key in ("prng", "ticket", "prefix"):
    assert child[key] != source[key], (key, source, child)
assert source["cache"] == parent["cache"] == "inherited-session", (source, parent)
assert child["cache"] == "-", child
assert child["counter"] == "0", child
assert source_nonce.split("-", 1)[0] != child_nonce.split("-", 1)[0], (source_nonce, child_nonce)
PY
  expect_exec "$APP_CHILD_ID" \
    "/usr/local/bin/tarit-clone-repair-workload accept-ticket '$SOURCE_TICKET' | grep -qx rejected; /usr/local/bin/tarit-clone-repair-workload check inherited-session | grep -qx absent" \
    ''
  expect_exec "$PARENT_ID" \
    "/usr/local/bin/tarit-clone-repair-workload accept-ticket '$SOURCE_TICKET' | grep -qx accepted; /usr/local/bin/tarit-clone-repair-workload check inherited-session | grep -qx present" \
    ''
  api -X DELETE "$BASE_URL/v1/vms/$APP_CHILD_ID" >/dev/null
fi

echo "== public snapshot/restore uses an opaque handle, never a host path =="
SNAPSHOT_JSON=$(api -H 'Content-Type: application/json' -d '{"diff":false}' \
  "$BASE_URL/v1/vms/$PARENT_ID/snapshot")
SNAPSHOT_ID=$(printf '%s' "$SNAPSHOT_JSON" | python3 -c '
import json,sys,uuid
value=json.load(sys.stdin)
assert set(value) == {"snapshot_id"}, value
uuid.UUID(value["snapshot_id"])
print(value["snapshot_id"])
')
LEGACY_RESTORE_BODY="$DIR/legacy-restore-response.json"
LEGACY_RESTORE_STATUS=$(curl -sS --max-time 10 -o "$LEGACY_RESTORE_BODY" -w '%{http_code}' \
  -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  -d '{"snapshot_path":"/etc/shadow","host_id":"attacker-selected"}' "$BASE_URL/v1/restore")
[ "$LEGACY_RESTORE_STATUS" = 422 ] || {
  echo "FAIL: raw snapshot locator was not rejected: HTTP $LEGACY_RESTORE_STATUS $(cat "$LEGACY_RESTORE_BODY")" >&2
  exit 1
}
RESTORED_JSON=$(api -H 'Content-Type: application/json' \
  -d "{\"snapshot_id\":\"$SNAPSHOT_ID\"}" "$BASE_URL/v1/restore")
RESTORED_ID=$(printf '%s' "$RESTORED_JSON" | json_field id)
expect_exec "$RESTORED_ID" 'cat /root/tarit-fork-state' 'parent-after'
api -X DELETE "$BASE_URL/v1/vms/$RESTORED_ID" >/dev/null
for _ in $(seq 1 50); do
  [ -z "$(vmm_pids_for_id "$RESTORED_ID")" ] && break
  sleep 0.1
done
[ -z "$(vmm_pids_for_id "$RESTORED_ID")" ] || { echo "FAIL: opaque restore VMM did not stop" >&2; exit 1; }
mapfile -t OPAQUE_SNAPSHOT_FILES < <(python3 - "$DIR/fleet.db" "$SNAPSHOT_ID" <<'PY'
import sqlite3,sys
with sqlite3.connect(sys.argv[1]) as db:
    row=db.execute(
        "select path, overlay_path from snapshots where snapshot_id=?", (sys.argv[2],)
    ).fetchone()
    assert row, "opaque snapshot row missing"
    db.execute("delete from snapshots where snapshot_id=?", (sys.argv[2],))
for path in row:
    if path:
        print(path)
print(row[0] + ".integrity")
PY
)
for snapshot_file in "${OPAQUE_SNAPSHOT_FILES[@]}"; do
  case "$snapshot_file" in
    "$DIR"/*) rm -f -- "$snapshot_file" ;;
    *) echo "FAIL: opaque snapshot escaped test directory: $snapshot_file" >&2; exit 1 ;;
  esac
done

echo "== hibernate removes the VMM and concurrent HTTP calls single-flight resume =="
if [ "${TARIT_TEST_ENABLE_NET:-0}" = 1 ]; then
  echo "== durable egress policy is CAS-versioned and survives allocation teardown =="
  INITIAL_POLICY=$(api "$BASE_URL/v1/vms/$PARENT_ID/egress-policy")
  printf '%s' "$INITIAL_POLICY" | python3 -c '
import json,sys
p=json.load(sys.stdin)
assert p["revision"] == 1 and p["allowlist"] == [] and p["allow_existing"] is False, p
'
  POLICY_TWO=$(api -X PUT -H 'Content-Type: application/json' \
    -d '{"expected_revision":1,"allowlist":["8.8.8.8:443"],"allow_existing":false}' \
    "$BASE_URL/v1/vms/$PARENT_ID/egress-policy")
  printf '%s' "$POLICY_TWO" | python3 -c '
import json,sys
p=json.load(sys.stdin)
assert p["revision"] == 2 and p["allowlist"] == ["8.8.8.8:443/tcp"], p
'
  STALE_BODY="$DIR/stale-egress.json"
  STALE_STATUS=$(curl -sS --max-time 10 -o "$STALE_BODY" -w '%{http_code}' \
    -X PUT -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
    -d '{"expected_revision":1,"allowlist":[]}' \
    "$BASE_URL/v1/vms/$PARENT_ID/egress-policy")
  [ "$STALE_STATUS" = 409 ] || {
    echo "FAIL: stale egress CAS returned HTTP $STALE_STATUS: $(cat "$STALE_BODY")" >&2
    exit 1
  }
fi
if [ -n "$CLONE_WORKLOAD_BIN" ]; then
  HIBERNATE_WORKLOAD_STATE=$(exec_request "$PARENT_ID" \
    '/usr/local/bin/tarit-clone-repair-workload state' | json_field stdout)
  HIBERNATE_TICKET=$(exec_request "$PARENT_ID" \
    '/usr/local/bin/tarit-clone-repair-workload ticket' | json_field stdout | tr -d '\r\n')
fi
api -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$PARENT_ID/hibernate" | grep -q '"status":"hibernated"'
[ -z "$(vmm_pids_for_id "$PARENT_ID")" ] || { echo "FAIL: VMM survived hibernate"; exit 1; }
if [ "${TARIT_TEST_ENABLE_NET:-0}" = 1 ]; then
  POLICY_THREE=$(api -X PUT -H 'Content-Type: application/json' \
    -d '{"expected_revision":2,"allowlist":["1.1.1.1:443"],"allow_existing":true}' \
    "$BASE_URL/v1/vms/$PARENT_ID/egress-policy")
  printf '%s' "$POLICY_THREE" | python3 -c '
import json,sys
p=json.load(sys.stdin)
assert p["revision"] == 3 and p["allowlist"] == ["1.1.1.1:443/tcp"] and p["allow_existing"] is True, p
'
fi
RESUME_START=$(now_ms)
wake_pids=()
for index in $(seq 1 8); do
  (expect_exec "$PARENT_ID" "echo wake-$index" "wake-$index") &
  wake_pids+=("$!")
done
wake_failed=0
for wake_pid in "${wake_pids[@]}"; do
  wait "$wake_pid" || wake_failed=1
done
if [ "$wake_failed" -ne 0 ]; then
  echo "FAIL: one or more concurrent activation requests failed" >&2
  tail -200 "$DIR/taritd.log" >&2
  exit 1
fi
RESUME_END=$(now_ms)
PIDS=$(vmm_pids_for_id "$PARENT_ID")
[ "$(printf '%s\n' "$PIDS" | sed '/^$/d' | wc -l)" -eq 1 ] || {
  echo "FAIL: expected exactly one resumed VMM, got: $PIDS"; exit 1;
}
expect_exec "$PARENT_ID" 'cat /root/tarit-fork-state' 'parent-after'
if [ -n "$CLONE_WORKLOAD_BIN" ]; then
  RESUMED_WORKLOAD_STATE=$(exec_request "$PARENT_ID" \
    '/usr/local/bin/tarit-clone-repair-workload state' | json_field stdout)
  python3 - "$HIBERNATE_WORKLOAD_STATE" "$RESUMED_WORKLOAD_STATE" <<'PY'
import sys

def fields(value):
    return dict(item.split("=", 1) for item in value.split())

before, after = map(fields, sys.argv[1:])
assert after["clone"] != before["clone"], (before, after)
for key in ("prng", "ticket", "prefix"):
    assert after[key] != before[key], (key, before, after)
assert after["counter"] == "0" and after["cache"] == "-", after
PY
  expect_exec "$PARENT_ID" \
    "/usr/local/bin/tarit-clone-repair-workload accept-ticket '$HIBERNATE_TICKET' | grep -qx rejected; /usr/local/bin/tarit-clone-repair-workload check inherited-session | grep -qx absent" \
    ''
fi
if [ "${TARIT_TEST_ENABLE_NET:-0}" = 1 ]; then
  api "$BASE_URL/v1/vms/$PARENT_ID/egress-policy" | python3 -c '
import json,sys
p=json.load(sys.stdin)
assert p["revision"] == 3 and p["allowlist"] == ["1.1.1.1:443/tcp"] and p["allow_existing"] is True, p
'
  expect_exec "$PARENT_ID" \
    "if command -v busybox >/dev/null 2>&1; then busybox nc -w 5 1.1.1.1 443 </dev/null; elif command -v python3 >/dev/null 2>&1; then python3 -c 'import socket; s=socket.create_connection((\"1.1.1.1\",443),5); s.close()'; else timeout 5 bash -c 'exec 3<>/dev/tcp/1.1.1.1/443'; fi; echo EGRESS_ALLOWED" \
    'EGRESS_ALLOWED'
  python3 - "$DIR/fleet.db" "$PARENT_ID" <<'PY'
import json,sqlite3,sys
with sqlite3.connect(sys.argv[1]) as db:
    row=db.execute(
        "select owner_key,revision,allowlist_json,allow_existing from vm_egress_policies where vm_id=?",
        (sys.argv[2],),
    ).fetchone()
assert row and row[0] and row[1] == 3, row
assert json.loads(row[2]) == ["1.1.1.1:443/tcp"] and row[3] == 1, row
PY
fi
echo "hibernate_http_resume_ms=$((RESUME_END-RESUME_START))"

echo "== PTY API is also an activation source =="
api -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$CHILD_ID/hibernate" | grep -q '"status":"hibernated"'
[ -z "$(vmm_pids_for_id "$CHILD_ID")" ] || { echo "FAIL: child VMM survived hibernate"; exit 1; }
api -H 'Content-Type: application/json' -d '{"cols":80,"rows":24}' "$BASE_URL/v1/vms/$CHILD_ID/pty/sessions" | grep -q '"pty_id"'
api "$BASE_URL/v1/vms/$CHILD_ID" | grep -q '"status":"running"'
[ "$(vmm_pids_for_id "$CHILD_ID" | sed '/^$/d' | wc -l)" -eq 1 ]

echo "== corrupted hibernation artifact fails closed without booting =="
CORRUPT_JSON=$(api -H 'Content-Type: application/json' -d '{"vcpus":1,"memory_mib":256}' "$BASE_URL/v1/vms")
CORRUPT_ID=$(printf '%s' "$CORRUPT_JSON" | json_field id)
api -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$CORRUPT_ID/hibernate" | grep -q '"status":"hibernated"'
SNAPSHOT_PATH=$(python3 - "$DIR/fleet.db" "$CORRUPT_ID" <<'PY'
import sqlite3,sys
with sqlite3.connect(sys.argv[1]) as db:
    row=db.execute("select snapshot_path from hibernations where vm_id=?", (sys.argv[2],)).fetchone()
assert row, "missing hibernation row"
print(row[0])
PY
)
python3 - "$SNAPSHOT_PATH" <<'PY'
import os,struct,sys
fd=os.open(sys.argv[1], os.O_RDWR | os.O_NOFOLLOW)
try:
    header=os.pread(fd, 32, 0)
    assert len(header) == 32 and header[:4] == b"VMSN"
    state_len=struct.unpack("<Q", header[8:16])[0]
    memory_len=struct.unpack("<Q", header[20:28])[0]
    offset=32 + state_len
    for chunk_offset in range(0, memory_len, 64 * 1024):
        position=offset + chunk_offset
        byte=os.pread(fd, 1, position)
        assert byte
        os.pwrite(fd, bytes([byte[0] ^ 0xff]), position)
    os.fsync(fd)
finally:
    os.close(fd)
PY
ERROR_BODY="$DIR/corrupt-error.json"
STATUS=$(curl -sS --max-time 90 -o "$ERROR_BODY" -w '%{http_code}' -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  -d "{\"vm_id\":\"$CORRUPT_ID\",\"command\":\"true\",\"timeout_ms\":30000}" "$BASE_URL/v1/execute")
[ "$STATUS" = 200 ] || { echo "FAIL: corrupt resume returned HTTP $STATUS"; cat "$ERROR_BODY"; exit 1; }
python3 - "$ERROR_BODY" <<'PY'
import json,sys
with open(sys.argv[1]) as body:
    result=json.load(body)
assert result["status"] == "failed", result
assert result["exit_code"] is None, result
assert result.get("error") == "VM operation failed", result
PY
[ -z "$(vmm_pids_for_id "$CORRUPT_ID")" ] || { echo "FAIL: corrupt snapshot booted a VMM"; exit 1; }
grep -q 'snapshot integrity failure at chunk ' "$DIR/taritd.log" || {
  echo "FAIL: corrupted RAM was not rejected by the authenticated UFFD handler" >&2
  exit 1
}
grep -qv "$DIR" "$ERROR_BODY"

echo "== interrupted hibernation is recovered after control-plane restart =="
# Use a fresh artifact namespace for this phase. Snapshots from the preceding
# assertions are immutable independent resources and correctly remain retained
# after their VMs are deleted; keeping them would make this test depend on the
# capacity of its deliberately small reflink volume.
stop_taritd
find "$DIR/sockets" "$DIR/runtime" -depth -delete
rm -f -- "$DIR/fleet.db" "$DIR/fleet.db-shm" "$DIR/fleet.db-wal"
mkdir -m 700 "$DIR/sockets" "$DIR/runtime"
start_taritd
register_ssh_key
RECOVERY_BODY="$DIR/recovery-create.json"
RECOVERY_STATUS=$(curl -sS --max-time 90 -o "$RECOVERY_BODY" -w '%{http_code}' \
  -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  -d '{"vcpus":1,"memory_mib":256}' "$BASE_URL/v1/vms")
if [ "$RECOVERY_STATUS" != 201 ]; then
  echo "FAIL: recovery fixture create returned HTTP $RECOVERY_STATUS: $(cat "$RECOVERY_BODY")" >&2
  exit 1
fi
RECOVERY_JSON=$(cat "$RECOVERY_BODY")
RECOVERY_ID=$(printf '%s' "$RECOVERY_JSON" | json_field id)
# The create readiness probe and the serial guest agent share a single exec
# lane. Keep fixture setup out of that deliberately tiny post-probe handoff.
sleep 1
expect_exec "$RECOVERY_ID" "sh -c 'echo crash-window-state > /root/tarit-crash-window; sync'" ""
python3 - "$DIR/fleet.db" "$RECOVERY_ID" "$DIR/pre-hibernate-row.json" <<'PY'
import json,sqlite3,sys
columns=("status","revision","runtime_overlay_path","runtime_jail_path",
         "runtime_artifact_paths","socket_path","pid","updated_at")
with sqlite3.connect(sys.argv[1]) as db:
    row=db.execute(
        "select " + ",".join(columns) + " from vms where id=?", (sys.argv[2],)
    ).fetchone()
assert row and row[0] == "running", row
with open(sys.argv[3], "w") as output:
    json.dump(dict(zip(columns,row)), output)
PY
api -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$RECOVERY_ID/hibernate" | grep -q '"status":"hibernated"'
[ -z "$(vmm_pids_for_id "$RECOVERY_ID")" ] || { echo "FAIL: recovery VMM survived hibernate"; exit 1; }
# Recreate the durable crash window after the VMM stopped but before the
# Hibernated VM record was published. The committed hibernation intent remains
# while the VM row retains its previous Running identity.
python3 - "$DIR/fleet.db" "$RECOVERY_ID" "$DIR/pre-hibernate-row.json" <<'PY'
import json,sqlite3,sys
with open(sys.argv[3]) as source:
    row=json.load(source)
with sqlite3.connect(sys.argv[1]) as db:
    db.execute(
        "update vms set status=?, revision=?, runtime_overlay_path=?, runtime_jail_path=?, "
        "runtime_artifact_paths=?, socket_path=?, pid=?, updated_at=? where id=?",
        (row["status"],row["revision"],row["runtime_overlay_path"],row["runtime_jail_path"],
         row["runtime_artifact_paths"],row["socket_path"],row["pid"],row["updated_at"],sys.argv[2]))
PY
stop_taritd
start_taritd
api "$BASE_URL/v1/vms/$RECOVERY_ID" | grep -q '"status":"hibernated"'
[ -z "$(vmm_pids_for_id "$RECOVERY_ID")" ] || { echo "FAIL: startup booted recovery VM before ingress"; exit 1; }
expect_exec "$RECOVERY_ID" 'cat /root/tarit-crash-window' 'crash-window-state'
[ "$(vmm_pids_for_id "$RECOVERY_ID" | sed '/^$/d' | wc -l)" -eq 1 ]
python3 - "$DIR/fleet.db" "$RECOVERY_ID" <<'PY'
import sqlite3,sys
with sqlite3.connect(sys.argv[1]) as db:
    count=db.execute("select count(*) from hibernations where vm_id=?", (sys.argv[2],)).fetchone()[0]
assert count == 0, count
PY

if [ "${TARIT_TEST_SHARE:-0}" = 1 ]; then
  echo "== concurrent exec, PTY, SSH, and share ingress wait for clone repair =="
  expect_exec "$RECOVERY_ID" \
    "command -v busybox >/dev/null; mkdir -p /usr/libexec/tarit /run/tarit /tmp/tarit-share; printf '%s\\n' '#!/bin/sh' 'set -eu' 'test \"\$TARIT_POST_FORK\" = 1' 'test -f /run/tarit/cached-token' 'printf started > /run/tarit/mixed-hook-started' 'sleep 2' 'rm -f /run/tarit/cached-token' 'printf \"repaired:%s\\n\" \"\$TARIT_CLONE_ID\" > /run/tarit/mixed-userspace-token' 'printf REPAIRED_SHARE > /tmp/tarit-share/index.html' > /usr/libexec/tarit/post-fork; chmod 0755 /usr/libexec/tarit/post-fork; printf inherited-token > /run/tarit/cached-token; printf UNREPAIRED_SHARE > /tmp/tarit-share/index.html; busybox httpd -f -p 18080 -h /tmp/tarit-share >/tmp/tarit-share.log 2>&1 & echo mixed-ingress-ready" \
    'mixed-ingress-ready'
  SHARE_JSON=$(api -H 'Content-Type: application/json' \
    -d "{\"vm_id\":\"$RECOVERY_ID\",\"guest_port\":18080,\"visibility\":\"public\"}" \
    "$BASE_URL/v1/shares")
  SHARE_SLUG=$(printf '%s' "$SHARE_JSON" | json_field slug)
  api -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$RECOVERY_ID/hibernate" | grep -q '"status":"hibernated"'
  [ -z "$(vmm_pids_for_id "$RECOVERY_ID")" ] || { echo "FAIL: mixed-ingress fixture VMM survived hibernate"; exit 1; }

  MIXED_PIDS=()
  for index in $(seq 1 4); do
    (expect_exec "$RECOVERY_ID" \
      "if [ -e /run/tarit/cached-token ]; then echo UNREPAIRED_EXEC; exit 97; fi; echo REPAIRED_EXEC_$index" \
      "REPAIRED_EXEC_$index" >"$DIR/mixed-exec-$index.log" 2>&1) &
    MIXED_PIDS+=("$!")
  done
  (api -H 'Content-Type: application/json' -d '{"cols":80,"rows":24}' \
    "$BASE_URL/v1/vms/$RECOVERY_ID/pty/sessions" >"$DIR/mixed-pty.json") &
  MIXED_PIDS+=("$!")
  (python3 "$ROOT/orch/tests/ssh_pty_test.py" \
    "$DIR/ssh-client-ed25519" "$SSH_PORT" "$RECOVERY_ID" 127.0.0.1 \
    'if [ -e /run/tarit/cached-token ]; then echo UNREPAIRED_SSH; exit 97; fi; echo REPAIRED_SSH' \
    REPAIRED_SSH >"$DIR/mixed-ssh.log" 2>&1) &
  MIXED_PIDS+=("$!")
  (curl -fsS --max-time 90 -H "Host: $SHARE_SLUG.shares.e2e.test" \
    "http://127.0.0.1:$SHARE_PORT/" >"$DIR/mixed-share.out") &
  MIXED_PIDS+=("$!")

  MIXED_FAILED=0
  for mixed_pid in "${MIXED_PIDS[@]}"; do
    wait "$mixed_pid" || MIXED_FAILED=1
  done
  if [ "$MIXED_FAILED" -ne 0 ]; then
    echo "FAIL: one or more concurrent repair-barrier ingress requests failed" >&2
    grep -aiE 'ssh|gateway|pty|share|activation|repair|request failed' "$DIR/taritd.log" | tail -120 >&2 || true
    exit 1
  fi
  grep -q '"pty_id"' "$DIR/mixed-pty.json"
  grep -q REPAIRED_SSH "$DIR/mixed-ssh.log"
  ! grep -q UNREPAIRED_SSH "$DIR/mixed-ssh.log"
  grep -qx REPAIRED_SHARE "$DIR/mixed-share.out"
  api "$BASE_URL/v1/vms/$RECOVERY_ID" | grep -q '"status":"running"'
  [ "$(vmm_pids_for_id "$RECOVERY_ID" | sed '/^$/d' | wc -l)" -eq 1 ]
  expect_exec "$RECOVERY_ID" \
    'test ! -e /run/tarit/cached-token && grep -q "^repaired:" /run/tarit/mixed-userspace-token && test -e /run/tarit/mixed-hook-started' \
    ''
else
  echo "== SSH gateway is an authenticated activation source =="
  api -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$RECOVERY_ID/hibernate" | grep -q '"status":"hibernated"'
  [ -z "$(vmm_pids_for_id "$RECOVERY_ID")" ] || { echo "FAIL: SSH fixture VMM survived hibernate"; exit 1; }
  if ! SSH_OUTPUT=$(python3 "$ROOT/orch/tests/ssh_pty_test.py" \
    "$DIR/ssh-client-ed25519" "$SSH_PORT" "$RECOVERY_ID" 127.0.0.1 2>&1); then
    echo "FAIL: SSH activation probe failed:" >&2
    printf '%s\n' "$SSH_OUTPUT" >&2
    grep -aiE 'ssh|gateway|auth|pty|activation|request failed' "$DIR/taritd.log" | tail -80 >&2 || true
    exit 1
  fi
  printf '%s\n' "$SSH_OUTPUT" | grep -q 'SSH_GW_PASS'
  api "$BASE_URL/v1/vms/$RECOVERY_ID" | grep -q '"status":"running"'
  [ "$(vmm_pids_for_id "$RECOVERY_ID" | sed '/^$/d' | wc -l)" -eq 1 ]
  expect_exec "$RECOVERY_ID" "printf 'post-ssh-agent-ok\\n'" 'post-ssh-agent-ok'
fi

grep -q 'adopted VMM live integrity sidecar without rereading RAM' "$DIR/taritd.log" || {
  echo "FAIL: live fork did not use the precomputed RAM integrity path" >&2
  exit 1
}

if [ "${TARIT_TEST_SHARE:-0}" = "1" ]; then
  echo "PASS: live fork + hibernate + HTTP/PTy/SSH/share activation + integrity/crash recovery fail-closed"
else
  echo "PASS: live fork + hibernate + HTTP/PTy/SSH activation + integrity/crash recovery fail-closed"
fi
