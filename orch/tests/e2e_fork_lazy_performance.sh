#!/usr/bin/env bash
# Real-KVM 100-sample lazy live-fork performance and size-independence gate.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD="${TARITD_BIN:-$ROOT/orch/target/release/taritd}"
VMM="${TARIT_VMM_BIN:-$ROOT/vmm/target/release/vmm}"
AGENT="${TARIT_TEST_GUEST_AGENT_BIN:-$ROOT/vmm/guest/agent/vmm-agent}"
KERNEL="${TARIT_KERNEL:?set TARIT_KERNEL}"
ROOTFS="${TARIT_ROOTFS:?set TARIT_ROOTFS to an OCI-derived ext4 image}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-/t}"
ITERATIONS="${TARIT_FORK_PERF_ITERATIONS:-100}"
SMALL_MEMORY_MIB="${TARIT_FORK_PERF_SMALL_MEMORY_MIB:-256}"
LARGE_MEMORY_MIB="${TARIT_FORK_PERF_LARGE_MEMORY_MIB:-4096}"
MAX_P99_TOTAL_US="${TARIT_FORK_PERF_MAX_P99_TOTAL_US:-8000000}"
MAX_P99_DOWNTIME_US="${TARIT_FORK_PERF_MAX_P99_DOWNTIME_US:-50000}"
MAX_SIZE_RATIO="${TARIT_FORK_PERF_MAX_SIZE_RATIO:-1.25}"
KEY=fork-lazy-performance-key

for path in "$TARITD" "$VMM" "$AGENT" "$KERNEL" "$ROOTFS"; do
  test -f "$path" || { echo "FAIL: missing test input: $path" >&2; exit 1; }
done
test -x "$TARITD" && test -x "$VMM" && test -x "$AGENT"
test -c /dev/kvm && test -r /dev/kvm && test -w /dev/kvm
[[ "$ITERATIONS" =~ ^[0-9]+$ ]] && (( ITERATIONS >= 100 ))
[[ "$SMALL_MEMORY_MIB" =~ ^[0-9]+$ ]]
[[ "$LARGE_MEMORY_MIB" =~ ^[0-9]+$ ]]
(( SMALL_MEMORY_MIB >= 128 && LARGE_MEMORY_MIB > SMALL_MEMORY_MIB ))

DIR=$(mktemp -d "$SOCKET_ROOT/tarit-fork-performance.XXXXXX")
chmod 0700 "$DIR"
mkdir -m 0700 "$DIR/sockets" "$DIR/images" "$DIR/jails" "$DIR/runtime" "$DIR/results"
STAGED_ROOTFS="$DIR/rootfs.ext4"
ROOTFS_MOUNT="$DIR/rootfs-mount"
TARITD_PID=
TARITD_PGID=

cleanup() {
  local status=$?
  if mountpoint -q "$ROOTFS_MOUNT" 2>/dev/null; then
    umount "$ROOTFS_MOUNT" || status=1
  fi
  if [[ -n "$TARITD_PGID" ]] && kill -0 -- "-$TARITD_PGID" 2>/dev/null; then
    kill -TERM -- "-$TARITD_PGID" 2>/dev/null || true
    for _ in $(seq 1 100); do
      kill -0 -- "-$TARITD_PGID" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL -- "-$TARITD_PGID" 2>/dev/null || true
  fi
  [[ -z "$TARITD_PID" ]] || wait "$TARITD_PID" 2>/dev/null || true
  if [[ "${TARIT_TEST_KEEP_DIR:-0}" == 1 ]] ||
     { [[ "$status" -ne 0 ]] && [[ "${TARIT_TEST_KEEP_FAILED:-0}" == 1 ]]; }; then
    echo "retained fork performance artifacts at $DIR" >&2
  else
    find "$DIR" -depth -delete 2>/dev/null || true
  fi
  return "$status"
}
trap cleanup EXIT

cp --reflink=auto --sparse=always -- "$ROOTFS" "$STAGED_ROOTFS"
mkdir -m 0700 "$ROOTFS_MOUNT"
mount -o loop,rw "$STAGED_ROOTFS" "$ROOTFS_MOUNT"
install -D -m 0755 "$AGENT" "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
sync -f "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
umount "$ROOTFS_MOUNT"
e2fsck -pf "$STAGED_ROOTFS" >/dev/null
chmod 0444 "$STAGED_ROOTFS"

PORT=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
BASE_URL="http://127.0.0.1:$PORT"
TARIT_API_KEY="$KEY" \
TARIT_LISTEN="127.0.0.1:$PORT" \
TARIT_RPC_ADDR="$BASE_URL" \
TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
TARIT_HOST_ID=fork-performance-c8i \
TARIT_VMM_BIN="$VMM" \
TARIT_KERNEL="$KERNEL" \
TARIT_ROOTFS="$STAGED_ROOTFS" \
TARIT_ENABLE_NET=0 \
TARIT_SOCKET_DIR="$DIR/sockets" \
TARIT_IMAGES_DIR="$DIR/images" \
TARIT_DB="$DIR/fleet.db" \
TARIT_NET_STATE="$DIR/net-state.json" \
TARIT_WARM_POOL=0 \
TARIT_MAX_VMS=2 \
TARIT_MAX_VCPUS=2 \
TARIT_MAX_MEMORY_MIB=$((LARGE_MEMORY_MIB * 2)) \
TARIT_VM_JAIL_BASE="$DIR/jails" \
TARIT_VM_JAIL_UID_BASE=280000 \
TARIT_VM_JAIL_GID_BASE=290000 \
TARIT_VM_JAIL_ID_COUNT=2 \
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

for _ in $(seq 1 150); do
  curl -fsS --max-time 1 "$BASE_URL/health" >/dev/null 2>&1 && break
  kill -0 "$TARITD_PID"
  sleep 0.2
done
curl -fsS --max-time 2 "$BASE_URL/health" >/dev/null

REPORT="$DIR/results/fork-lazy-performance.json"
python3 "$ROOT/orch/tests/fork_lazy_performance.py" \
  --base-url "$BASE_URL" \
  --api-key "$KEY" \
  --report "$REPORT" \
  --iterations "$ITERATIONS" \
  --small-memory-mib "$SMALL_MEMORY_MIB" \
  --large-memory-mib "$LARGE_MEMORY_MIB" \
  --storage-path "$SOCKET_ROOT" \
  --reclaim-every 10 \
  --min-free-bytes 3221225472 \
  --max-p99-total-us "$MAX_P99_TOTAL_US" \
  --max-p99-downtime-us "$MAX_P99_DOWNTIME_US" \
  --max-large-small-p99-ratio "$MAX_SIZE_RATIO"

python3 - "$DIR/fleet.db" "$ITERATIONS" <<'PY'
import sqlite3
import sys
with sqlite3.connect(sys.argv[1]) as db:
    vms = db.execute("select count(*) from vms").fetchone()[0]
    snapshots = db.execute("select count(*) from snapshots").fetchone()[0]
    operations = db.execute(
        "select count(*) from vm_fork_operations where status='committed'"
    ).fetchone()[0]
expected_operations = int(sys.argv[2]) * 2
assert (vms, snapshots, operations) == (0, 0, expected_operations), (
    vms, snapshots, operations, expected_operations
)
PY

install -D -m 0600 "$REPORT" "${TARIT_FORK_PERF_REPORT_OUT:-$SOCKET_ROOT/fork-lazy-performance.json}"
echo "FORK_LAZY_PERFORMANCE_E2E_PASS iterations_per_size=$ITERATIONS small_mib=$SMALL_MEMORY_MIB large_mib=$LARGE_MEMORY_MIB"
