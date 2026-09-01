#!/usr/bin/env bash
# Real-KVM acceptance gate for persistent block volumes across scale-to-zero.
set -Eeuo pipefail

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD="${TARITD_BIN:-$ROOT/orch/target/release/taritd}"
VMM="${TARIT_VMM_BIN:-$ROOT/vmm/target/release/vmm}"
KERNEL="${TARIT_KERNEL:?set TARIT_KERNEL to a KVM guest kernel}"
ROOTFS="${TARIT_ROOTFS:?set TARIT_ROOTFS to an agent-enabled OCI rootfs}"
EXPECTED_OS_ID="${TARIT_EXPECT_OS_ID:-ubuntu}"
VOLUME_PROVIDER="${TARIT_VOLUME_PROVIDER:-local_block}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"
GUEST_AGENT_BIN="${TARIT_TEST_GUEST_AGENT_BIN:-}"
KEY="persistent-volume-e2e-key"
PORT="${PERSISTENT_VOLUME_E2E_PORT:-}"

for required in cp curl findmnt python3 setsid pgrep stat timeout; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done
case "$VOLUME_PROVIDER" in
  local_block|nfs_v4_1_block) ;;
  *) echo "FAIL: unsupported TARIT_VOLUME_PROVIDER: $VOLUME_PROVIDER" >&2; exit 1 ;;
esac
[ "$(id -u)" -eq 0 ] || { echo "FAIL: this jail/KVM gate must run as root" >&2; exit 1; }
[ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ] || {
  echo "FAIL: worker /dev/kvm is unavailable" >&2
  exit 1
}
grep -Eq '\b(vmx|svm)\b' /proc/cpuinfo || {
  echo "FAIL: worker nested-virtualization feature is unavailable" >&2
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

DIR=$(mktemp -d "$SOCKET_ROOT/tarit-volume-hibernate.XXXXXX")
chmod 700 "$DIR"
mkdir -m 700 "$DIR/sockets" "$DIR/runtime" "$DIR/images" "$DIR/jails"
BASE_URL="http://127.0.0.1:$PORT"
ROOTFS_MOUNT=""
NFS_UNIT=""
NFS_WAS_ACTIVE=0
NFS_EXPORTED=0
NFS_EXPORT_CONFIG=""
NFS_EXPORT=""
NFS_MOUNTS=""

if [ "$VOLUME_PROVIDER" = nfs_v4_1_block ]; then
  for required in exportfs mount.nfs4 systemctl; do
    command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
  done
  for candidate in nfs-server.service nfs-kernel-server.service; do
    if systemctl cat "$candidate" >/dev/null 2>&1; then
      NFS_UNIT="$candidate"
      break
    fi
  done
  [ -n "$NFS_UNIT" ] || { echo "FAIL: no NFS server systemd unit" >&2; exit 1; }
  systemctl is-active --quiet "$NFS_UNIT" && NFS_WAS_ACTIVE=1
  NFS_EXPORT="$DIR/nfs-export"
  NFS_MOUNTS="$DIR/nfs-mounts"
  mkdir -m 700 "$NFS_EXPORT" "$NFS_MOUNTS"
  NFS_EXPORT_CONFIG="/etc/exports.d/tarit-volume-test-$$.exports"
  systemctl start "$NFS_UNIT"
  install -d -m 755 /etc/exports.d
  printf '%s 127.0.0.1(rw,sync,no_subtree_check,no_root_squash,fsid=0,insecure)\n' \
    "$NFS_EXPORT" >"$NFS_EXPORT_CONFIG"
  chmod 600 "$NFS_EXPORT_CONFIG"
  exportfs -ra
  NFS_EXPORTED=1
  export TARIT_SHARED_BLOCK_PROVIDER=nfs_v4_1_block
  export TARIT_SHARED_BLOCK_ENDPOINT=127.0.0.1
  export TARIT_SHARED_BLOCK_EXPORT=/
  export TARIT_SHARED_BLOCK_MOUNT_ROOT="$NFS_MOUNTS"
  export TARIT_SHARED_BLOCK_MAX_BYTES=$((1024 * 1024 * 1024))
  export TARIT_SHARED_BLOCK_TIMEOUT_MS=30000
fi

# Atomic live disk snapshots deliberately fail closed when the backing
# filesystem cannot clone extents. Detect that prerequisite before booting.
printf x >"$DIR/.reflink-source"
if ! cp --reflink=always "$DIR/.reflink-source" "$DIR/.reflink-clone" 2>/dev/null; then
  find "$DIR" -depth -delete 2>/dev/null || true
  echo "FAIL: TARIT_TEST_SOCKET_ROOT must be on a reflink-capable filesystem" >&2
  exit 1
fi
rm -f -- "$DIR/.reflink-source" "$DIR/.reflink-clone"

# Keep the immutable OCI-derived rootfs in the same CoW storage domain as the
# jail and volume artifacts. Otherwise every jail construction can expand the
# sparse source image across filesystems and turn this into a capacity test.
STAGED_ROOTFS="$DIR/rootfs.ext4"
cp --reflink=auto --sparse=always -- "$ROOTFS" "$STAGED_ROOTFS"
cmp -s -- "$ROOTFS" "$STAGED_ROOTFS" || {
  echo "FAIL: staged OCI rootfs differs from the source" >&2
  exit 1
}
if [ -n "$GUEST_AGENT_BIN" ]; then
  for required in e2fsck install mount mountpoint umount; do
    command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
  done
  test -x "$GUEST_AGENT_BIN" || {
    echo "FAIL: guest agent not executable: $GUEST_AGENT_BIN" >&2
    exit 1
  }
  ROOTFS_MOUNT="$DIR/rootfs-mount"
  mkdir -m 700 "$ROOTFS_MOUNT"
  mount -o loop,rw "$STAGED_ROOTFS" "$ROOTFS_MOUNT"
  install -D -m 0755 "$GUEST_AGENT_BIN" "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
  sync -f "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
  umount "$ROOTFS_MOUNT"
  ROOTFS_MOUNT=""
  e2fsck -pf "$STAGED_ROOTFS" >/dev/null
fi
chmod 0444 "$STAGED_ROOTFS"
ROOTFS="$STAGED_ROOTFS"

cleanup() {
  local status=$?
  if [ -n "$ROOTFS_MOUNT" ] && mountpoint -q "$ROOTFS_MOUNT"; then
    umount "$ROOTFS_MOUNT" || status=1
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
  if [ -n "$NFS_MOUNTS" ]; then
    while IFS= read -r mounted_target; do
      [[ "$mounted_target" == "$NFS_MOUNTS/"* ]] || continue
      if ! timeout --signal=TERM --kill-after=2s 10s umount -- "$mounted_target" 2>/dev/null &&
         ! umount -l -- "$mounted_target" 2>/dev/null; then
        echo "FAIL: could not detach test NFS mount $mounted_target" >&2
        status=1
      fi
    done < <(findmnt -rn -t nfs4 -o TARGET || true)
  fi
  if [ "$NFS_EXPORTED" -eq 1 ]; then
    if ! exportfs -u "127.0.0.1:$NFS_EXPORT" 2>/dev/null; then
      echo "FAIL: could not remove test NFS export $NFS_EXPORT" >&2
      status=1
    fi
  fi
  [ -z "$NFS_EXPORT_CONFIG" ] || rm -f -- "$NFS_EXPORT_CONFIG"
  if [ -n "$NFS_UNIT" ]; then
    if ! exportfs -ra 2>/dev/null; then
      echo "FAIL: could not reload NFS exports after test" >&2
      status=1
    fi
    if [ "$NFS_WAS_ACTIVE" -eq 0 ]; then
      if ! systemctl stop "$NFS_UNIT" >/dev/null 2>&1; then
        echo "FAIL: could not restore inactive state for $NFS_UNIT" >&2
        status=1
      fi
    fi
  fi
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
  return "$status"
}
on_error() {
  local status=$?
  echo "FAIL: persistent-volume gate exited $status" >&2
  [ ! -f "$DIR/taritd.log" ] || tail -160 "$DIR/taritd.log" >&2
}
trap cleanup EXIT
trap on_error ERR
trap 'exit 130' INT
trap 'exit 143' TERM

api() { curl -fsS --max-time 120 -H "X-API-Key: $KEY" "$@"; }
json_field() { python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"; }
now_ms() { python3 -c 'import time; print(time.monotonic_ns() // 1000000)'; }
exec_request() {
  local vm_id=$1 command=$2
  api -H 'Content-Type: application/json' \
    -d "$(python3 -c 'import json,sys; print(json.dumps({"vm_id":sys.argv[1],"command":sys.argv[2],"timeout_ms":90000}))' "$vm_id" "$command")" \
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
  local pid
  pid=$(python3 - "$DIR/fleet.db" "$id" <<'PY'
import sqlite3,sys
with sqlite3.connect(sys.argv[1]) as db:
    row=db.execute("select pid from vms where id=?", (sys.argv[2],)).fetchone()
if row and row[0] is not None:
    print(row[0])
PY
)
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    printf '%s\n' "$pid"
  fi
}
wait_for_no_vmm() {
  local id=$1
  for _ in $(seq 1 100); do
    [ -z "$(vmm_pids_for_id "$id")" ] && return 0
    sleep 0.1
  done
  return 1
}
assert_no_provider_mounts() {
  [ "$VOLUME_PROVIDER" != nfs_v4_1_block ] && return 0
  if findmnt -rn -t nfs4 -o TARGET | grep -Fxq -- "$NFS_MOUNTS" || \
     findmnt -rn -t nfs4 -o TARGET | grep -Fq -- "$NFS_MOUNTS/"; then
    echo "FAIL: shared provider left a live NFS mount" >&2
    findmnt -rn -t nfs4 -o SOURCE,TARGET >&2 || true
    return 1
  fi
}

TARIT_API_KEY="$KEY" \
TARIT_LISTEN="127.0.0.1:$PORT" \
TARIT_RPC_ADDR="$BASE_URL" \
TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
TARIT_HOST_ID=volume-e2e-c8i \
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
TARIT_MAX_VMS=2 \
TARIT_MAX_VCPUS=2 \
TARIT_MAX_MEMORY_MIB=768 \
TARIT_VM_JAIL_BASE="$DIR/jails" \
TARIT_VM_JAIL_UID_BASE=260000 \
TARIT_VM_JAIL_GID_BASE=270000 \
TARIT_VM_JAIL_ID_COUNT=2 \
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
  curl -fsS --max-time 1 "$BASE_URL/health" >/dev/null 2>&1 && break
  kill -0 "$TARITD_PID" 2>/dev/null || { tail -120 "$DIR/taritd.log"; exit 1; }
  sleep 0.2
done
curl -fsS "$BASE_URL/health" >/dev/null

VOLUME_JSON=$(api -H 'Content-Type: application/json' \
  -d "{\"name\":\"persistent-workspace\",\"size_bytes\":67108864,\"provider\":\"$VOLUME_PROVIDER\"}" \
  "$BASE_URL/v1/volumes")
VOLUME_ID=$(printf '%s' "$VOLUME_JSON" | json_field id)
printf '%s' "$VOLUME_JSON" | python3 -c '
import json,sys
v=json.load(sys.stdin)
assert v["status"] == "available" and v["storage_class"] == "block", v
for private in ("owner_key", "host_id", "last_error", "private_path"):
    assert private not in v, (private, v)
'
if [ "$VOLUME_PROVIDER" = local_block ]; then
  VOLUME_PATH="$DIR/images/volumes/$VOLUME_ID.block"
else
  VOLUME_PATH="$NFS_EXPORT/.tarit-block-volumes/$VOLUME_ID.block"
fi
[ -f "$VOLUME_PATH" ] || { echo "FAIL: physical volume was not created" >&2; exit 1; }
[ "$(stat -c '%a' "$VOLUME_PATH")" = 600 ] || { echo "FAIL: volume mode is not 0600" >&2; exit 1; }
assert_no_provider_mounts

VM_JSON=$(api -H 'Content-Type: application/json' \
  -d "{\"vcpus\":1,\"memory_mib\":256,\"volumes\":[{\"volume_id\":\"$VOLUME_ID\",\"mode\":\"read_write\"}]}" \
  "$BASE_URL/v1/vms")
VM_ID=$(printf '%s' "$VM_JSON" | json_field id)
expect_exec "$VM_ID" "grep '^ID=$EXPECTED_OS_ID$' /etc/os-release" "ID=$EXPECTED_OS_ID"
expect_exec "$VM_ID" "test ! -e /dev/kvm && ! grep -Eq '\\b(vmx|svm)\\b' /proc/cpuinfo && echo GUEST_VIRT_HIDDEN" GUEST_VIRT_HIDDEN
expect_exec "$VM_ID" "test ! -e '$NFS_EXPORT_CONFIG' && ! grep -q nfs /proc/mounts && echo SHARED_STORAGE_HIDDEN" SHARED_STORAGE_HIDDEN
expect_exec "$VM_ID" "test -b /dev/vdb || { ls -l /dev /dev/vd* 2>&1; exit 1; }; echo BLOCK_DEVICE_READY" BLOCK_DEVICE_READY
if [ "$EXPECTED_OS_ID" = alpine ]; then
  GUEST_VOLUME_MODE=raw
  expect_exec "$VM_ID" "printf 'tarit-volume-proof' | dd of=/dev/vdb bs=1 seek=4096 conv=fsync && dd if=/dev/vdb bs=1 skip=4096 count=18" tarit-volume-proof
else
  GUEST_VOLUME_MODE=ext4
  expect_exec "$VM_ID" "test -x /sbin/mkfs.ext4 && echo BLOCK_FORMATTER_READY" BLOCK_FORMATTER_READY
  expect_exec "$VM_ID" "export PATH=/usr/sbin:/usr/bin:/sbin:/bin; mkfs.ext4 -q /dev/vdb && echo BLOCK_FORMATTED" BLOCK_FORMATTED
  expect_exec "$VM_ID" "export PATH=/usr/sbin:/usr/bin:/sbin:/bin; mkdir -p /mnt/persist && mount /dev/vdb /mnt/persist && printf 'tarit-volume-proof' > /mnt/persist/proof && sync && cat /mnt/persist/proof" tarit-volume-proof
fi
assert_no_provider_mounts

VMM_PIDS=$(vmm_pids_for_id "$VM_ID")
[ "$(printf '%s\n' "$VMM_PIDS" | sed '/^$/d' | wc -l)" -eq 1 ] || {
  echo "FAIL: expected one jailed VMM, got: $VMM_PIDS" >&2; exit 1;
}
VMM_PID=$(printf '%s\n' "$VMM_PIDS" | sed -n '1p')
VMM_EUID=$(ps -o euid= -p "$VMM_PID" | tr -d ' ')
case "$VMM_EUID" in 260000|260001) ;; *) echo "FAIL: VMM euid is $VMM_EUID" >&2; exit 1 ;; esac
JAIL_PATH=$(python3 - "$DIR/fleet.db" "$VM_ID" <<'PY'
import sqlite3,sys
with sqlite3.connect(sys.argv[1]) as db:
    row=db.execute("select runtime_jail_path from vms where id=?", (sys.argv[2],)).fetchone()
assert row and row[0], row
print(row[0])
PY
)
case "$JAIL_PATH" in "$DIR/jails"/*) ;; *) echo "FAIL: jail escaped test root: $JAIL_PATH" >&2; exit 1 ;; esac
[ ! -e "$JAIL_PATH/dev/kvm" ] || { echo "FAIL: /dev/kvm was staged inside VMM jail" >&2; exit 1; }

DELETE_ATTACHED_BODY="$DIR/delete-attached.json"
DELETE_ATTACHED_STATUS=$(curl -sS --max-time 20 -o "$DELETE_ATTACHED_BODY" -w '%{http_code}' \
  -X DELETE -H "X-API-Key: $KEY" "$BASE_URL/v1/volumes/$VOLUME_ID")
[ "$DELETE_ATTACHED_STATUS" = 409 ] || {
  echo "FAIL: attached-volume deletion returned HTTP $DELETE_ATTACHED_STATUS: $(cat "$DELETE_ATTACHED_BODY")" >&2
  exit 1
}
[ -f "$VOLUME_PATH" ] || { echo "FAIL: rejected deletion removed physical volume" >&2; exit 1; }

FORK_CHILD_ID=$(python3 -c 'import uuid; print(uuid.uuid4())')
FORK_ATTACHED_BODY="$DIR/fork-attached.json"
FORK_ATTACHED_STATUS=$(curl -sS --max-time 20 -o "$FORK_ATTACHED_BODY" -w '%{http_code}' \
  -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  -d "{\"id\":\"$FORK_CHILD_ID\"}" "$BASE_URL/v1/vms/$VM_ID/fork")
[ "$FORK_ATTACHED_STATUS" = 409 ] || {
  echo "FAIL: attached-volume fork returned HTTP $FORK_ATTACHED_STATUS: $(cat "$FORK_ATTACHED_BODY")" >&2
  exit 1
}
python3 - "$DIR/fleet.db" "$FORK_CHILD_ID" <<'PY'
import sqlite3
import sys

with sqlite3.connect(sys.argv[1]) as db:
    child = db.execute("select count(*) from vms where id=?", (sys.argv[2],)).fetchone()[0]
    operation = db.execute(
        "select count(*) from vm_fork_operations where child_vm_id=?", (sys.argv[2],)
    ).fetchone()[0]
    snapshot = db.execute(
        "select count(*) from snapshots where snapshot_id=? or ephemeral_owner_vm_id=?",
        (sys.argv[2], sys.argv[2]),
    ).fetchone()[0]
assert (child, operation, snapshot) == (0, 0, 0), (child, operation, snapshot)
PY
expect_exec "$VM_ID" "test -b /dev/vdb && echo FORK_REJECTION_SOURCE_INTACT" FORK_REJECTION_SOURCE_INTACT
[ -f "$VOLUME_PATH" ] || { echo "FAIL: rejected fork removed physical volume" >&2; exit 1; }

api -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$VM_ID/hibernate" | grep -q '"status":"hibernated"'
wait_for_no_vmm "$VM_ID" || { echo "FAIL: VMM survived hibernate" >&2; exit 1; }
[ -f "$VOLUME_PATH" ] || { echo "FAIL: hibernate removed persistent volume" >&2; exit 1; }
assert_no_provider_mounts

RESUME_START=$(now_ms)
if [ "$GUEST_VOLUME_MODE" = raw ]; then
  expect_exec "$VM_ID" "dd if=/dev/vdb bs=1 skip=4096 count=18" tarit-volume-proof
else
  expect_exec "$VM_ID" "export PATH=/usr/sbin:/usr/bin:/sbin:/bin; mkdir -p /mnt/persist && (mountpoint -q /mnt/persist || mount /dev/vdb /mnt/persist) && cat /mnt/persist/proof" tarit-volume-proof
fi
RESUME_END=$(now_ms)
expect_exec "$VM_ID" "test ! -e /dev/kvm && ! grep -Eq '\\b(vmx|svm)\\b' /proc/cpuinfo && echo GUEST_VIRT_HIDDEN" GUEST_VIRT_HIDDEN
VMM_PIDS=$(vmm_pids_for_id "$VM_ID")
[ "$(printf '%s\n' "$VMM_PIDS" | sed '/^$/d' | wc -l)" -eq 1 ] || {
  echo "FAIL: expected one resumed VMM, got: $VMM_PIDS" >&2; exit 1;
}
assert_no_provider_mounts

api -X DELETE "$BASE_URL/v1/vms/$VM_ID" >/dev/null
wait_for_no_vmm "$VM_ID" || { echo "FAIL: VMM survived VM deletion" >&2; exit 1; }
DELETE_VOLUME_STATUS=$(curl -sS --max-time 20 -o /dev/null -w '%{http_code}' \
  -X DELETE -H "X-API-Key: $KEY" "$BASE_URL/v1/volumes/$VOLUME_ID")
[ "$DELETE_VOLUME_STATUS" = 204 ] || { echo "FAIL: final volume delete returned HTTP $DELETE_VOLUME_STATUS" >&2; exit 1; }
[ ! -e "$VOLUME_PATH" ] || { echo "FAIL: physical volume survived deletion" >&2; exit 1; }
assert_no_provider_mounts

[ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo "FAIL: worker KVM was damaged" >&2; exit 1; }
grep -Eq '\b(vmx|svm)\b' /proc/cpuinfo || { echo "FAIL: worker VMX/SVM disappeared" >&2; exit 1; }
if command -v systemctl >/dev/null && systemctl list-unit-files postgresql.service >/dev/null 2>&1; then
  systemctl is-active --quiet postgresql || { echo "FAIL: PostgreSQL is unhealthy" >&2; exit 1; }
fi

echo "VOLUME_E2E_PASS provider=$VOLUME_PROVIDER guest_mode=$GUEST_VOLUME_MODE vm_id=$VM_ID volume_id=$VOLUME_ID http_resume_ms=$((RESUME_END-RESUME_START)) credentials_in_guest=0"
