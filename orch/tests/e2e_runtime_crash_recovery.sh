#!/usr/bin/env bash
# Real-KVM crash gate for taritd re-adoption and unexpected VMM exit cleanup.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD="${TARITD_BIN:-$ROOT/orch/target/release/taritd}"
VMM="${TARIT_VMM_BIN:-$ROOT/vmm/target/release/vmm}"
KERNEL="${TARIT_KERNEL:?set TARIT_KERNEL to a KVM guest kernel}"
ROOTFS="${TARIT_ROOTFS:?set TARIT_ROOTFS to an agent-enabled OCI rootfs}"
GUEST_AGENT="${TARIT_TEST_GUEST_AGENT_BIN:-}"
EXPECTED_OS_ID="${TARIT_EXPECT_OS_ID:-ubuntu}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"
KEY="runtime-crash-recovery-e2e-key"
PORT="${RUNTIME_CRASH_E2E_PORT:-}"

for required in cmp cp curl findmnt python3 setsid sqlite3; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done
[ "$(id -u)" -eq 0 ] || { echo "FAIL: runtime crash gate must run as root" >&2; exit 1; }
[ -x "$TARITD" ] || { echo "FAIL: taritd is not executable: $TARITD" >&2; exit 1; }
[ -x "$VMM" ] || { echo "FAIL: VMM is not executable: $VMM" >&2; exit 1; }
[ -r "$KERNEL" ] || { echo "FAIL: kernel is not readable: $KERNEL" >&2; exit 1; }
[ -r "$ROOTFS" ] || { echo "FAIL: rootfs is not readable: $ROOTFS" >&2; exit 1; }
[[ "$EXPECTED_OS_ID" =~ ^[a-z0-9._-]+$ ]] || {
  echo "FAIL: TARIT_EXPECT_OS_ID contains unsupported characters" >&2
  exit 1
}
[ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ] || {
  echo "FAIL: worker /dev/kvm is unavailable" >&2
  exit 1
}
grep -Eq '\b(vmx|svm)\b' /proc/cpuinfo || {
  echo "FAIL: worker nested virtualization is unavailable" >&2
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

DIR=$(mktemp -d "$SOCKET_ROOT/tarit-runtime-crash.XXXXXX")
chmod 700 "$DIR"
mkdir -m 700 "$DIR/sockets" "$DIR/runtime" "$DIR/images" "$DIR/jails"
BASE_URL="http://127.0.0.1:$PORT"
TARITD_PID=""
TARITD_PGID=""
ROOTFS_MOUNT=""

cleanup() {
  local status=$?
  if [ -n "$ROOTFS_MOUNT" ] && mountpoint -q "$ROOTFS_MOUNT"; then
    umount "$ROOTFS_MOUNT" || true
  fi
  if [ -n "$TARITD_PGID" ] && kill -0 -- "-$TARITD_PGID" 2>/dev/null; then
    kill -TERM -- "-$TARITD_PGID" 2>/dev/null || true
    for _ in $(seq 1 80); do
      kill -0 -- "-$TARITD_PGID" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL -- "-$TARITD_PGID" 2>/dev/null || true
  fi
  [ -z "$TARITD_PID" ] || wait "$TARITD_PID" 2>/dev/null || true
  if [ "$status" -ne 0 ]; then
    echo "FAIL: runtime crash gate exited $status" >&2
    tail -240 "$DIR/taritd.log" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ] && [ "${TARIT_E2E_KEEP_FAILED:-0}" = 1 ]; then
    echo "FAIL: retained diagnostic directory: $DIR" >&2
  else
    find "$DIR" -depth -delete 2>/dev/null || true
  fi
  mount_target=$(findmnt -n -o TARGET -T "$SOCKET_ROOT" 2>/dev/null || true)
  if [ -n "$mount_target" ] && command -v fstrim >/dev/null 2>&1; then
    sync -f "$SOCKET_ROOT" >/dev/null 2>&1 || true
    fstrim "$mount_target" >/dev/null 2>&1 || true
  fi
  return "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
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
  echo "FAIL: staged OCI rootfs differs from its source" >&2
  exit 1
}
if [ -n "$GUEST_AGENT" ]; then
  for required in e2fsck install mount mountpoint umount; do
    command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
  done
  test -x "$GUEST_AGENT" || {
    echo "FAIL: guest agent is not executable: $GUEST_AGENT" >&2
    exit 1
  }
  ROOTFS_MOUNT="$DIR/rootfs-mount"
  mkdir -m 700 "$ROOTFS_MOUNT"
  mount -o loop,rw "$STAGED_ROOTFS" "$ROOTFS_MOUNT"
  install -D -m 0755 "$GUEST_AGENT" "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
  sync -f "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
  umount "$ROOTFS_MOUNT"
  ROOTFS_MOUNT=""
  e2fsck -pf "$STAGED_ROOTFS" >/dev/null
fi
chmod 0444 "$STAGED_ROOTFS"
ROOTFS="$STAGED_ROOTFS"

api() { curl -fsS --max-time 90 -H "X-API-Key: $KEY" "$@"; }
json_field() { python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"; }
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
row=json.load(sys.stdin); expected=sys.argv[1]
assert row["exit_code"] == 0, row
assert expected in row.get("stdout", ""), row
' "$expected"
}
vmm_pid_for_id() {
  local id=$1
  sqlite3 "$DIR/fleet.db" "select coalesce(pid, '') from vms where id='$id';"
}
control_runtime_pids() {
  local id=$1
  local jail_root
  jail_root=$(sqlite3 "$DIR/fleet.db" "select coalesce(runtime_jail_path, '') from vms where id='$id';")
  python3 - "$jail_root" <<'PY'
import os
import sys
from pathlib import Path

jail_root = os.fsencode(sys.argv[1])
matches = []
for proc in Path("/proc").iterdir():
    if not proc.name.isdigit():
        continue
    try:
        argv = (proc / "cmdline").read_bytes().split(b"\0")
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        continue
    socket_matches = [
        pair[1] for pair in zip(argv, argv[1:]) if pair[0] == b"--socket"
    ]
    jail_matches = [
        pair[1]
        for pair in zip(argv, argv[1:])
        if pair[0] in (b"--jail", b"--jail-root")
    ]
    if socket_matches == [b"/run/vmm.sock"] and jail_matches == [jail_root]:
        matches.append(int(proc.name))
print("\n".join(str(pid) for pid in sorted(matches)))
PY
}
wait_pid_gone() {
  local pid=$1
  for _ in $(seq 1 100); do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep 0.1
  done
  return 1
}
wait_vm_status() {
  local id=$1 expected=$2
  for _ in $(seq 1 100); do
    if api "$BASE_URL/v1/vms/$id" 2>/dev/null | python3 -c '
import json,sys
assert json.load(sys.stdin)["status"] == sys.argv[1]
' "$expected" 2>/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}
start_taritd() {
  TARIT_API_KEY="$KEY" \
  TARIT_LISTEN="127.0.0.1:$PORT" \
  TARIT_RPC_ADDR="$BASE_URL" \
  TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
  TARIT_HOST_ID=runtime-crash-c8i \
  TARIT_VMM_BIN="$VMM" \
  TARIT_KERNEL="$KERNEL" \
  TARIT_ROOTFS="$ROOTFS" \
  TARIT_ROOTFS_READONLY=0 \
  TARIT_ENABLE_NET=0 \
  TARIT_SOCKET_DIR="$DIR/sockets" \
  TARIT_IMAGES_DIR="$DIR/images" \
  TARIT_DB="$DIR/fleet.db" \
  TARIT_CONFIG="$DIR/none.toml" \
  TARIT_WARM_POOL=0 \
  TARIT_MAX_VMS=1 \
  TARIT_MAX_VCPUS=1 \
  TARIT_MAX_MEMORY_MIB=256 \
  TARIT_ADMISSION_TIMEOUT_MS=1000 \
  TARIT_VM_JAIL_BASE="$DIR/jails" \
  TARIT_VM_JAIL_UID_BASE=300000 \
  TARIT_VM_JAIL_GID_BASE=310000 \
  TARIT_VM_JAIL_ID_COUNT=1 \
  TARIT_VM_JAIL_SECCOMP=1 \
  TARIT_VM_JAIL_PID_NAMESPACE=1 \
  TARIT_VM_JAIL_NETWORK_NAMESPACE=1 \
  TARIT_REAP_ON_SHUTDOWN=true \
  TARIT_PRODUCTION=0 \
  RUST_LOG=taritd=info,vmm_core=info \
  TMPDIR="$DIR/runtime" \
  setsid "$TARITD" serve >>"$DIR/taritd.log" 2>&1 &
  TARITD_PID=$!
  TARITD_PGID=$TARITD_PID
  for _ in $(seq 1 100); do
    curl -fsS --max-time 1 "$BASE_URL/health" >/dev/null 2>&1 && return 0
    kill -0 "$TARITD_PID" 2>/dev/null || { tail -160 "$DIR/taritd.log" >&2; return 1; }
    sleep 0.2
  done
  echo "FAIL: taritd did not become healthy" >&2
  return 1
}

start_taritd
VM_JSON=$(api -H 'Content-Type: application/json' -d '{"vcpus":1,"memory_mib":256}' "$BASE_URL/v1/vms")
VM_ID=$(printf '%s' "$VM_JSON" | json_field id)
expect_exec "$VM_ID" "grep '^ID=$EXPECTED_OS_ID$' /etc/os-release" "ID=$EXPECTED_OS_ID"
expect_exec "$VM_ID" "test ! -e /dev/kvm && ! grep -Eq '\\b(vmx|svm)\\b' /proc/cpuinfo && echo GUEST_VIRT_HIDDEN" GUEST_VIRT_HIDDEN
expect_exec "$VM_ID" "printf 'crash-recovery-proof' > /root/tarit-crash-proof; sync; cat /root/tarit-crash-proof" crash-recovery-proof
ORIGINAL_VMM_PID=$(vmm_pid_for_id "$VM_ID")
[ -n "$ORIGINAL_VMM_PID" ] && kill -0 "$ORIGINAL_VMM_PID"
ORIGINAL_START_TIME=$(awk '{print $22}' "/proc/$ORIGINAL_VMM_PID/stat")
ORIGINAL_CONTROL_PIDS=$(control_runtime_pids "$VM_ID")
printf '%s\n' "$ORIGINAL_CONTROL_PIDS" | grep -Fxq "$ORIGINAL_VMM_PID" || {
  echo "FAIL: durable VMM PID is absent from its control runtime" >&2
  exit 1
}
[ "$(ps -o euid= -p "$ORIGINAL_VMM_PID" | tr -d ' ')" = 300000 ] || {
  echo "FAIL: original VMM is not using its allocated jail UID" >&2
  exit 1
}

echo "== SIGKILL taritd and re-adopt the exact surviving VMM =="
OLD_TARITD_PID=$TARITD_PID
kill -KILL "$OLD_TARITD_PID"
set +e
wait "$OLD_TARITD_PID" 2>/dev/null
set -e
TARITD_PID=""
TARITD_PGID=""
kill -0 "$ORIGINAL_VMM_PID" || { echo "FAIL: VMM died with taritd" >&2; exit 1; }
start_taritd
READOPTED_PID=$(vmm_pid_for_id "$VM_ID")
[ "$READOPTED_PID" = "$ORIGINAL_VMM_PID" ] || {
  echo "FAIL: restart did not re-adopt the exact VMM PID" >&2
  exit 1
}
[ "$(awk '{print $22}' "/proc/$READOPTED_PID/stat")" = "$ORIGINAL_START_TIME" ] || {
  echo "FAIL: re-adoption accepted a reused PID" >&2
  exit 1
}
[ "$(control_runtime_pids "$VM_ID")" = "$ORIGINAL_CONTROL_PIDS" ] || {
  echo "FAIL: taritd restart changed the VM control-runtime PID set" >&2
  exit 1
}
expect_exec "$VM_ID" 'cat /root/tarit-crash-proof' crash-recovery-proof

echo "== hibernate and HTTP-resume the re-adopted runtime =="
api -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$VM_ID/hibernate" | grep -q '"status":"hibernated"'
wait_pid_gone "$READOPTED_PID" || { echo "FAIL: re-adopted VMM survived hibernate" >&2; exit 1; }
[ -z "$(vmm_pid_for_id "$VM_ID")" ] || { echo "FAIL: hibernated VM retained a PID" >&2; exit 1; }

if [ "${TARIT_E2E_UFFD_HANDLER_FAILURE:-0}" = 1 ]; then
  echo "== force lazy-restore handler failure and preserve retryable state =="
  exec_request "$VM_ID" 'cat /root/tarit-crash-proof' >"$DIR/uffd-failure.response"
  python3 - "$DIR/uffd-failure.response" <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as response:
    row=json.load(response)
assert row["status"] == "failed", row
assert row["exit_code"] is None, row
assert row["stdout"] is None and row["stderr"] is None, row
PY
  wait_vm_status "$VM_ID" hibernated || {
    echo "FAIL: forced UFFD failure did not preserve hibernated state" >&2
    api "$BASE_URL/v1/vms/$VM_ID" >&2 || true
    exit 1
  }
  [ -z "$(vmm_pid_for_id "$VM_ID")" ] || {
    echo "FAIL: forced UFFD failure retained a VMM PID" >&2
    exit 1
  }

  kill -TERM -- "-$TARITD_PGID"
  wait "$TARITD_PID"
  TARITD_PID=""
  TARITD_PGID=""
  unset TARIT_TEST_UFFD_HANDLER_FAILURE
  unset TARIT_TEST_UFFD_MAPPING_EVENT
  start_taritd
fi

expect_exec "$VM_ID" 'cat /root/tarit-crash-proof' crash-recovery-proof
RESUMED_PID=$(vmm_pid_for_id "$VM_ID")
[ -n "$RESUMED_PID" ] && kill -0 "$RESUMED_PID"
[ "$RESUMED_PID" != "$READOPTED_PID" ] || { echo "FAIL: resume reused the stopped PID" >&2; exit 1; }

echo "== SIGKILL VMM and reconcile terminal error plus released capacity =="
kill -KILL "$RESUMED_PID"
wait_pid_gone "$RESUMED_PID" || { echo "FAIL: killed VMM remained alive" >&2; exit 1; }
wait_vm_status "$VM_ID" error || {
  echo "FAIL: killed VMM did not converge to error" >&2
  api "$BASE_URL/v1/vms/$VM_ID" >&2 || true
  exit 1
}
python3 - "$DIR/fleet.db" "$VM_ID" <<'PY'
import sqlite3,sys
with sqlite3.connect(sys.argv[1]) as db:
    row=db.execute(
        "select status,pid,runtime_jail_path,runtime_overlay_path,socket_path "
        "from vms where id=?", (sys.argv[2],)
    ).fetchone()
assert row == ("error", None, None, None, None), row
PY
REPLACEMENT_JSON=$(api -H 'Content-Type: application/json' -d '{"vcpus":1,"memory_mib":256}' "$BASE_URL/v1/vms")
REPLACEMENT_ID=$(printf '%s' "$REPLACEMENT_JSON" | json_field id)
expect_exec "$REPLACEMENT_ID" 'printf replacement-capacity-ok' replacement-capacity-ok
REPLACEMENT_PID=$(vmm_pid_for_id "$REPLACEMENT_ID")
[ -n "$REPLACEMENT_PID" ] && kill -0 "$REPLACEMENT_PID"
IFS='|' read -r REPLACEMENT_JAIL REPLACEMENT_OVERLAY REPLACEMENT_SOCKET <<EOF
$(sqlite3 -separator '|' "$DIR/fleet.db" \
  "select coalesce(runtime_jail_path, ''),coalesce(runtime_overlay_path, ''),coalesce(socket_path, '') from vms where id='$REPLACEMENT_ID';")
EOF
[ -n "$REPLACEMENT_JAIL" ] && [ -d "$REPLACEMENT_JAIL" ] || {
  echo "FAIL: replacement VM has no live jail path" >&2
  exit 1
}
[ -n "$REPLACEMENT_OVERLAY" ] && [ -f "$REPLACEMENT_OVERLAY" ] || {
  echo "FAIL: replacement VM has no live overlay path" >&2
  exit 1
}
[ -n "$REPLACEMENT_SOCKET" ] && [ -S "$REPLACEMENT_SOCKET" ] || {
  echo "FAIL: replacement VM has no live control socket" >&2
  exit 1
}

echo "== SIGSTOP VMM and verify API teardown remains bounded through SIGKILL =="
kill -STOP "$REPLACEMENT_PID"
TEARDOWN_START_NS=$(python3 -c 'import time; print(time.monotonic_ns())')
api -X DELETE "$BASE_URL/v1/vms/$REPLACEMENT_ID" >/dev/null
TEARDOWN_END_NS=$(python3 -c 'import time; print(time.monotonic_ns())')
TEARDOWN_MS=$(((TEARDOWN_END_NS - TEARDOWN_START_NS) / 1000000))
((TEARDOWN_MS <= 10000)) || {
  echo "FAIL: forced VMM teardown exceeded 10 seconds: ${TEARDOWN_MS}ms" >&2
  exit 1
}
wait_pid_gone "$REPLACEMENT_PID" || {
  echo "FAIL: forced teardown retained VMM PID $REPLACEMENT_PID" >&2
  exit 1
}
[ ! -e "$REPLACEMENT_SOCKET" ] || { echo "FAIL: forced teardown retained socket" >&2; exit 1; }
[ ! -e "$REPLACEMENT_OVERLAY" ] || { echo "FAIL: forced teardown retained overlay" >&2; exit 1; }
[ ! -e "$REPLACEMENT_JAIL" ] || { echo "FAIL: forced teardown retained jail" >&2; exit 1; }
[ -z "$(vmm_pid_for_id "$REPLACEMENT_ID")" ] || {
  echo "FAIL: forced teardown retained durable PID ownership" >&2
  exit 1
}
[ -z "$(control_runtime_pids "$REPLACEMENT_ID")" ] || {
  echo "FAIL: forced teardown retained a control-runtime process" >&2
  exit 1
}

FINAL_JSON=$(api -H 'Content-Type: application/json' -d '{"vcpus":1,"memory_mib":256}' "$BASE_URL/v1/vms")
FINAL_ID=$(printf '%s' "$FINAL_JSON" | json_field id)
expect_exec "$FINAL_ID" 'printf post-teardown-capacity-ok' post-teardown-capacity-ok
api -X DELETE "$BASE_URL/v1/vms/$FINAL_ID" >/dev/null
api -X DELETE "$BASE_URL/v1/vms/$VM_ID" >/dev/null

[ -z "$(vmm_pid_for_id "$REPLACEMENT_ID")" ] || { echo "FAIL: replacement VMM leaked" >&2; exit 1; }
[ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]
grep -Eq '\b(vmx|svm)\b' /proc/cpuinfo
echo "RUNTIME_CRASH_RECOVERY_PASS readopted_pid=$READOPTED_PID resumed_pid=$RESUMED_PID forced_teardown_ms=$TEARDOWN_MS"
