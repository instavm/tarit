#!/usr/bin/env bash
# Start one disposable tenant-owned Ubuntu VM for real SDK integration tests.
# Run as root on a KVM worker; SIGINT/SIGTERM performs exact fixture cleanup.
set -Eeuo pipefail
umask 077

[ "$(id -u)" -eq 0 ] || { echo "serve_kvm_fixture.sh must run as root" >&2; exit 1; }

TARITD_BIN=${TARITD_BIN:?set TARITD_BIN}
VMM_BIN=${VMM_BIN:?set VMM_BIN}
KERNEL=${KERNEL:?set KERNEL}
ROOTFS_SOURCE=${ROOTFS_SOURCE:?set ROOTFS_SOURCE}
AGENT=${AGENT:?set AGENT}
SOCKET_ROOT=${SOCKET_ROOT:-/tmp}
PORT=${PORT:-18082}
VM_ID=${VM_ID:-11111111-1111-4111-8111-111111111111}
TENANT_KEY=${TENANT_KEY:-sdk-tenant-key}
FOREIGN_KEY=${FOREIGN_KEY:-sdk-foreign-key}
LOCK=${LOCK:-/run/lock/tarit-september-global.lock}

exec 9<"$LOCK"
flock 9

if ss -ltn "sport = :$PORT" | grep -q LISTEN; then
  echo "SDK fixture port $PORT is already in use" >&2
  exit 1
fi

DIR=$(mktemp -d "$SOCKET_ROOT/tarit-sdk.XXXXXX")
ROOTFS="$DIR/ubuntu.ext4"
MOUNT_DIR="$DIR/mount"
TARITD_PID=""
TARITD_PGID=""

cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [ -n "$TARITD_PGID" ] && kill -0 -- "-$TARITD_PGID" 2>/dev/null; then
    kill -TERM -- "-$TARITD_PGID" 2>/dev/null || true
    for _ in $(seq 1 80); do
      kill -0 -- "-$TARITD_PGID" 2>/dev/null || break
      sleep .1
    done
    kill -KILL -- "-$TARITD_PGID" 2>/dev/null || true
  fi
  [ -z "$TARITD_PID" ] || wait "$TARITD_PID" 2>/dev/null || true
  umount "$MOUNT_DIR" 2>/dev/null || true
  if [ "$status" -ne 0 ]; then
    echo "SDK fixture failed with status $status" >&2
    tail -200 "$DIR/taritd.log" 2>/dev/null || true
  fi
  find "$DIR" -depth -delete 2>/dev/null || true
  btrfs filesystem sync "$SOCKET_ROOT" >/dev/null 2>&1 || true
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

cp --sparse=always "$ROOTFS_SOURCE" "$ROOTFS"
mkdir "$MOUNT_DIR"
mount -o loop "$ROOTFS" "$MOUNT_DIR"
install -m 0755 "$AGENT" "$MOUNT_DIR/usr/sbin/vmm-agent"
umount "$MOUNT_DIR"
install -d -m 0700 "$DIR/sockets" "$DIR/jails"

TARIT_API_KEYS="$TENANT_KEY:tenant-a:user:4,$FOREIGN_KEY:tenant-b:user:4" \
TARIT_LISTEN="127.0.0.1:$PORT" \
TARIT_RPC_ADDR="http://127.0.0.1:$PORT" \
TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
TARIT_HOST_ID=sdk-c8i \
TARIT_VMM_BIN="$VMM_BIN" \
TARIT_KERNEL="$KERNEL" \
TARIT_ROOTFS="$ROOTFS" \
TARIT_ROOTFS_READONLY=0 \
TARIT_ENABLE_NET=0 \
TARIT_SOCKET_DIR="$DIR/sockets" \
TARIT_DB="$DIR/fleet.db" \
TARIT_CONFIG="$DIR/missing.toml" \
TARIT_WARM_POOL=0 \
TARIT_MAX_VMS=4 \
TARIT_MAX_VCPUS=4 \
TARIT_MAX_MEMORY_MIB=1024 \
TARIT_VM_JAIL_BASE="$DIR/jails" \
TARIT_VM_JAIL_UID_BASE=320000 \
TARIT_VM_JAIL_GID_BASE=330000 \
TARIT_VM_JAIL_ID_COUNT=4 \
TARIT_VM_JAIL_SECCOMP=1 \
TARIT_VM_JAIL_PID_NAMESPACE=1 \
TARIT_VM_JAIL_NETWORK_NAMESPACE=1 \
TARIT_REAP_ON_SHUTDOWN=true \
TARIT_PRODUCTION=0 \
RUST_LOG=taritd=info,vmm_core=info \
TMPDIR="$DIR" \
setsid "$TARITD_BIN" serve >"$DIR/taritd.log" 2>&1 &
TARITD_PID=$!
TARITD_PGID=$TARITD_PID

for _ in $(seq 1 150); do
  curl -fsS --max-time 1 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
  kill -0 "$TARITD_PID" 2>/dev/null || { tail -200 "$DIR/taritd.log"; exit 1; }
  sleep .2
done
curl -fsS "http://127.0.0.1:$PORT/health" >/dev/null

response=$(curl -fsS -X POST \
  -H "X-API-Key: $TENANT_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"id\":\"$VM_ID\",\"memory_mib\":256,\"vcpus\":1}" \
  "http://127.0.0.1:$PORT/v1/vms")
python3 -c 'import json,sys; record=json.loads(sys.argv[1]); assert record["status"] == "running", record' "$response"

echo "SDK_SERVER_READY vm_id=$VM_ID port=$PORT"
while kill -0 "$TARITD_PID" 2>/dev/null; do
  sleep 1
done
tail -200 "$DIR/taritd.log" >&2
exit 1
