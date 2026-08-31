#!/usr/bin/env bash
# Real-KVM model-based lifecycle gate. The Python model checks API, SQLite,
# process, jail, artifact, persistence, and cleanup invariants after each step.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD="${TARITD_BIN:-$ROOT/orch/target/release/taritd}"
VMM="${TARIT_VMM_BIN:-$ROOT/vmm/target/release/vmm}"
KERNEL="${TARIT_KERNEL:?set TARIT_KERNEL to a KVM guest kernel}"
ROOTFS="${TARIT_ROOTFS:?set TARIT_ROOTFS to an Ubuntu ext4 rootfs containing vmm-agent}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"
CLONE_WORKLOAD_BIN="${TARIT_TEST_CLONE_WORKLOAD_BIN:-}"
GUEST_AGENT_BIN="${TARIT_TEST_GUEST_AGENT_BIN:-}"
DRIVER="${TARIT_LIFECYCLE_DRIVER:-$ROOT/orch/tests/lifecycle_state_machine.py}"
PORT="${TARIT_LIFECYCLE_STATE_PORT:-}"
KEY="lifecycle-state-machine-e2e-key"
JAIL_UID_BASE="${TARIT_LIFECYCLE_JAIL_UID_BASE:-300000}"
JAIL_GID_BASE="${TARIT_LIFECYCLE_JAIL_GID_BASE:-310000}"
MAX_VMS="${TARIT_LIFECYCLE_MAX_VMS:-4}"
[[ "$MAX_VMS" =~ ^[1-9][0-9]*$ ]] || {
  echo "FAIL: TARIT_LIFECYCLE_MAX_VMS must be a positive integer" >&2
  exit 1
}
JAIL_ID_COUNT="$MAX_VMS"

for required in curl python3 setsid sqlite3; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done
[ "$(id -u)" -eq 0 ] || { echo "FAIL: lifecycle state-machine gate must run as root" >&2; exit 1; }
test -x "$TARITD" || { echo "FAIL: taritd not executable: $TARITD" >&2; exit 1; }
test -x "$VMM" || { echo "FAIL: vmm not executable: $VMM" >&2; exit 1; }
test -r "$KERNEL" || { echo "FAIL: kernel not readable: $KERNEL" >&2; exit 1; }
test -r "$ROOTFS" || { echo "FAIL: guest rootfs not readable: $ROOTFS" >&2; exit 1; }
test -r "$DRIVER" || { echo "FAIL: lifecycle driver not readable: $DRIVER" >&2; exit 1; }
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

DIR=$(mktemp -d "$SOCKET_ROOT/tarit-sm.XXXXXX")
chmod 700 "$DIR"
mkdir -m 700 "$DIR/sockets" "$DIR/runtime" "$DIR/jails"
ROOTFS_MOUNT=""
BASE_URL="http://127.0.0.1:$PORT"
TARITD_PID=""
TARITD_PGID=""

SOCKET_PATH_PROBE="$DIR/jails/00000000-0000-0000-0000-000000000000/root/run/vmm.sock"
if [ "${#SOCKET_PATH_PROBE}" -ge 108 ]; then
  find "$DIR" -depth -delete 2>/dev/null || true
  echo "FAIL: TARIT_TEST_SOCKET_ROOT produces a ${#SOCKET_PATH_PROBE}-byte jailed Unix socket path; require <108" >&2
  exit 1
fi
printf x >"$DIR/.reflink-source"
if ! cp --reflink=always "$DIR/.reflink-source" "$DIR/.reflink-clone" 2>/dev/null; then
  find "$DIR" -depth -delete 2>/dev/null || true
  echo "FAIL: TARIT_TEST_SOCKET_ROOT must be on a reflink-capable filesystem" >&2
  exit 1
fi
rm -f -- "$DIR/.reflink-source" "$DIR/.reflink-clone"

# Stage the immutable guest image once on the same reflink-capable filesystem
# as the jail roots. Production OCI images and jail assets share this storage
# domain; leaving the test input on another filesystem would force one sparse
# extent copy per VM and test fixture capacity instead of the CoW fast path.
STAGED_ROOTFS="$DIR/ubuntu.ext4"
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
    echo "FAIL: lifecycle state-machine gate exited $status" >&2
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
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

TARIT_API_KEY="$KEY" \
TARIT_LISTEN="127.0.0.1:$PORT" \
TARIT_RPC_ADDR="$BASE_URL" \
TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
TARIT_HOST_ID=lifecycle-state-c8i \
TARIT_VMM_BIN="$VMM" \
TARIT_KERNEL="$KERNEL" \
TARIT_ROOTFS="$ROOTFS" \
TARIT_ROOTFS_READONLY=0 \
TARIT_ENABLE_NET=0 \
TARIT_SOCKET_DIR="$DIR/sockets" \
TARIT_DB="$DIR/fleet.db" \
TARIT_CONFIG="$DIR/missing.toml" \
TARIT_WARM_POOL=0 \
TARIT_MAX_VMS="$MAX_VMS" \
TARIT_MAX_VCPUS="$MAX_VMS" \
TARIT_MAX_MEMORY_MIB="$((MAX_VMS * 256))" \
TARIT_ADMISSION_TIMEOUT_MS=1000 \
TARIT_VM_JAIL_BASE="$DIR/jails" \
TARIT_VM_JAIL_UID_BASE="$JAIL_UID_BASE" \
TARIT_VM_JAIL_GID_BASE="$JAIL_GID_BASE" \
TARIT_VM_JAIL_ID_COUNT="$JAIL_ID_COUNT" \
TARIT_VM_JAIL_SECCOMP=1 \
TARIT_VM_JAIL_PID_NAMESPACE=1 \
TARIT_VM_JAIL_NETWORK_NAMESPACE=1 \
TARIT_REAP_ON_SHUTDOWN=true \
TARIT_PRODUCTION=0 \
RUST_LOG=taritd=info,vmm_core=info \
TMPDIR="$DIR/runtime" \
setsid "$TARITD" serve >"$DIR/taritd.log" 2>&1 &
TARITD_PID=$!
TARITD_PGID=$TARITD_PID

for _ in $(seq 1 100); do
  curl -fsS --max-time 1 "$BASE_URL/health" >/dev/null 2>&1 && break
  kill -0 "$TARITD_PID" 2>/dev/null || { tail -160 "$DIR/taritd.log"; exit 1; }
  sleep 0.2
done
curl -fsS "$BASE_URL/health" >/dev/null

read -r -a DRIVER_ARGS <<<"${TARIT_LIFECYCLE_DRIVER_ARGS:-}"
python3 "$DRIVER" \
  --base-url "$BASE_URL" \
  --api-key "$KEY" \
  --database "$DIR/fleet.db" \
  --vmm "$VMM" \
  --jail-uid-base "$JAIL_UID_BASE" \
  --jail-uid-count "$JAIL_ID_COUNT" \
  --max-vms "$MAX_VMS" \
  --max-snapshots "${TARIT_LIFECYCLE_MAX_SNAPSHOTS:-8}" \
  --seeds "${TARIT_LIFECYCLE_SEEDS:-7,202609,424242}" \
  --steps "${TARIT_LIFECYCLE_STEPS:-20}" \
  "${DRIVER_ARGS[@]}"

if pgrep -f -- "${VMM} serve .*${DIR}" >/dev/null; then
  echo "FAIL: lifecycle state-machine left a VMM process" >&2
  exit 1
fi
[ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]
grep -Eq '\b(vmx|svm)\b' /proc/cpuinfo
