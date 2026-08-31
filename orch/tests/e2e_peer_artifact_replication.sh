#!/usr/bin/env bash
# Real two-node c8i acceptance gate for an Ubuntu OCI snapshot artifact:
# snapshot on node A, mTLS-stream and verify on node B, lose node A, then
# restore and execute the guest through node B. Run as root; use a btrfs
# TMPDIR so replicated immutable images and snapshot artifacts use one isolated
# copy-on-write test filesystem.
set -Eeuo pipefail
umask 077

ORCH_ROOT="${ORCH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD="${TARITD_BIN:-$ORCH_ROOT/target/release/taritd}"
VMM="${VMM_BIN:-$VMM_ROOT/target/release/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
AGENT="${AGENT:-$VMM_ROOT/guest/agent/vmm-agent}"
SECRET="0123456789abcdef0123456789abcdef-artifact-e2e"
API_KEY="peer-artifact-e2e-key"
DIR=$(mktemp -d "${TMPDIR:-/tmp}/tarit-peer-artifact.XXXXXX")
chmod 700 "$DIR"
SOCKET_PATH_PROBE="$DIR/node-a/sockets/00000000-0000-0000-0000-000000000000.sock"
if [ "${#SOCKET_PATH_PROBE}" -ge 108 ]; then
  echo "FAIL: TMPDIR produces a Unix socket path of ${#SOCKET_PATH_PROBE} bytes; require <108" >&2
  find "$DIR" -depth -delete 2>/dev/null || true
  exit 1
fi
DB_SUFFIX=$(python3 -c 'import secrets; print(secrets.token_hex(6))')
DB_NAME="tarit_artifact_$DB_SUFFIX"
DB_ROLE="tarit_artifact_role_$DB_SUFFIX"
DB_PASSWORD=$(python3 -c 'import secrets; print(secrets.token_hex(24))')
DATABASE_URL="postgresql://$DB_ROLE:$DB_PASSWORD@127.0.0.1:5432/$DB_NAME?sslmode=disable"
A_PID=""
B_PID=""
C_PID=""
SOURCE_VM=""
RESTORED_VM=""
CROSS_NODE_FORK_VM=""
REQUESTED_CROSS_NODE_FORK_VM=""
B_FORK_PAUSE_MS=0
B_FORK_PAUSE_PHASE=""
if [ "${TARIT_TEST_CROSS_NODE_FORK_DEATH:-0}" = 1 ]; then
  B_FORK_PAUSE_MS=30000
  B_FORK_PAUSE_PHASE="${TARIT_TEST_CROSS_NODE_FORK_DEATH_PHASE:-after_child}"
  case "$B_FORK_PAUSE_PHASE" in
    after_claim|after_snapshot|after_localize|after_bind|after_child|after_commit) ;;
    *)
      echo "FAIL: unsupported cross-node fork death phase: $B_FORK_PAUSE_PHASE" >&2
      exit 1
      ;;
  esac
fi
VOLUME_VM=""
VOLUME_ID=""
LAST_PID=""
FIRST_FORK_CURL_PID=""
NFS_UNIT=""
NFS_WAS_ACTIVE=0
NFS_EXPORTED=0
NFS_EXPORT_CONFIG=""
NFS_EXPORT="$DIR/nfs-export"

port() {
  python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
}

A_CONTROL=$(port)
A_PEER=$(port)
B_CONTROL=$(port)
B_PEER=$(port)
C_CONTROL=$(port)
C_PEER=$(port)

cleanup() {
  local status=$?
  if [ -n "$FIRST_FORK_CURL_PID" ] && kill -0 "$FIRST_FORK_CURL_PID" 2>/dev/null; then
    kill -TERM "$FIRST_FORK_CURL_PID" 2>/dev/null || true
    wait "$FIRST_FORK_CURL_PID" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ]; then
    echo "FAIL: preserving diagnostics before peer artifact cleanup (status $status)" >&2
    tail -120 "$DIR"/*.log 2>/dev/null || true
    find "$DIR" -maxdepth 3 -type f -name '*.json' -size -64k \
      -exec sh -c 'echo "== $1 ==" >&2; cat "$1" >&2' _ {} \; 2>/dev/null || true
  fi
  for endpoint_and_vm in \
    "http://127.0.0.1:$B_CONTROL $VOLUME_VM" \
    "http://127.0.0.1:$B_CONTROL $CROSS_NODE_FORK_VM" \
    "http://127.0.0.1:$B_CONTROL $REQUESTED_CROSS_NODE_FORK_VM" \
    "http://127.0.0.1:$B_CONTROL $RESTORED_VM" \
    "http://127.0.0.1:$A_CONTROL $SOURCE_VM"; do
    read -r endpoint vm_id <<<"$endpoint_and_vm"
    if [ -n "$vm_id" ]; then
      curl -fsS --max-time 5 -X DELETE -H "X-API-Key: $API_KEY" \
        "$endpoint/v1/vms/$vm_id" >/dev/null 2>&1 || true
    fi
  done
  if [ -n "$VOLUME_ID" ]; then
    curl -fsS --max-time 5 -X DELETE -H "X-API-Key: $API_KEY" \
      "http://127.0.0.1:$B_CONTROL/v1/volumes/$VOLUME_ID" >/dev/null 2>&1 || true
  fi
  for pid in "$A_PID" "$B_PID" "$C_PID"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  while IFS= read -r mounted_target; do
    [[ "$mounted_target" == "$DIR/"*"/nfs-mounts/"* ]] || continue
    timeout --signal=TERM --kill-after=2s 10s umount -- "$mounted_target" 2>/dev/null ||
      umount -l -- "$mounted_target" 2>/dev/null || status=1
  done < <(findmnt -rn -t nfs4 -o TARGET || true)
  if [ "$NFS_EXPORTED" -eq 1 ]; then
    exportfs -u "127.0.0.1:$NFS_EXPORT" 2>/dev/null || status=1
  fi
  [ -z "$NFS_EXPORT_CONFIG" ] || rm -f -- "$NFS_EXPORT_CONFIG"
  if [ -n "$NFS_UNIT" ]; then
    exportfs -ra 2>/dev/null || status=1
    if [ "$NFS_WAS_ACTIVE" -eq 0 ]; then
      systemctl stop "$NFS_UNIT" >/dev/null 2>&1 || status=1
    fi
  fi
  sudo -u postgres psql -v ON_ERROR_STOP=1 -d postgres -qAtc \
    "select pg_terminate_backend(pid) from pg_stat_activity where datname='$DB_NAME' and pid <> pg_backend_pid()" >/dev/null 2>&1 || true
  sudo -u postgres dropdb --if-exists "$DB_NAME" >/dev/null 2>&1 || true
  sudo -u postgres dropuser --if-exists "$DB_ROLE" >/dev/null 2>&1 || true
  if [ "$status" -ne 0 ] && [ "${TARIT_E2E_KEEP_FAILED:-0}" = 1 ]; then
    echo "FAIL: retained diagnostic directory: $DIR" >&2
    return "$status"
  fi
  find "$DIR" -depth -delete 2>/dev/null || true
  return "$status"
}
trap cleanup EXIT
trap 'status=$?; echo "FAIL: peer artifact gate exited $status" >&2; tail -120 "$DIR"/*.log 2>/dev/null || true' ERR

for required in curl exportfs findmnt mount.nfs4 openssl python3 psql createdb sqlite3 skopeo systemctl timeout umoci e2fsck; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done
test -x "$TARITD" || { echo "FAIL: taritd not executable: $TARITD" >&2; exit 1; }
test -x "$VMM" || { echo "FAIL: vmm not executable: $VMM" >&2; exit 1; }
test -x "$AGENT" || { echo "FAIL: guest agent not executable: $AGENT" >&2; exit 1; }
test -r "$KERNEL" || { echo "FAIL: kernel not readable: $KERNEL" >&2; exit 1; }
test "$(stat -f -c %T "$DIR")" = btrfs || {
  echo "FAIL: TMPDIR must reside on btrfs for exact CoW artifact validation" >&2
  exit 1
}

for candidate in nfs-server.service nfs-kernel-server.service; do
  if systemctl cat "$candidate" >/dev/null 2>&1; then
    NFS_UNIT="$candidate"
    break
  fi
done
[ -n "$NFS_UNIT" ] || { echo "FAIL: no NFS server systemd unit" >&2; exit 1; }
systemctl is-active --quiet "$NFS_UNIT" && NFS_WAS_ACTIVE=1
mkdir -m 700 "$NFS_EXPORT"
NFS_EXPORT_CONFIG="/etc/exports.d/tarit-peer-volume-test-$$.exports"
systemctl start "$NFS_UNIT"
install -d -m 755 /etc/exports.d
printf '%s 127.0.0.1(rw,sync,no_subtree_check,no_root_squash,fsid=0,insecure)\n' \
  "$NFS_EXPORT" >"$NFS_EXPORT_CONFIG"
chmod 600 "$NFS_EXPORT_CONFIG"
exportfs -ra
NFS_EXPORTED=1

sudo -u postgres psql -v ON_ERROR_STOP=1 -d postgres -qAtc \
  "create role $DB_ROLE login password '$DB_PASSWORD'"
sudo -u postgres createdb -O "$DB_ROLE" "$DB_NAME"

openssl req -x509 -newkey rsa:2048 -nodes -days 2 -sha256 \
  -subj '/CN=Tarit peer artifact test CA' \
  -keyout "$DIR/ca.key" -out "$DIR/ca.pem" >/dev/null 2>&1
chmod 600 "$DIR/ca.key"

make_leaf() {
  local name=$1
  openssl req -newkey rsa:2048 -nodes -sha256 -subj "/CN=$name" \
    -keyout "$DIR/$name.key" -out "$DIR/$name.csr" >/dev/null 2>&1
  chmod 600 "$DIR/$name.key"
  printf '%s\n' 'subjectAltName=DNS:localhost' 'extendedKeyUsage=serverAuth,clientAuth' >"$DIR/$name.ext"
  openssl x509 -req -days 2 -sha256 -in "$DIR/$name.csr" \
    -CA "$DIR/ca.pem" -CAkey "$DIR/ca.key" -CAcreateserial \
    -extfile "$DIR/$name.ext" -out "$DIR/$name.pem" >/dev/null 2>&1
}
make_leaf node-a
make_leaf node-b
make_leaf node-c

start_node() {
  local name=$1 zone=$2 control=$3 peer=$4 log=$5
  local node_kernel=$KERNEL
  local fork_pause_ms=0
  local fork_pause_phase=""
  # Peers deliberately start with a different readable kernel. Artifact
  # localization must fetch the authenticated source kernel rather than rely
  # on a shared host path or manual pre-provisioning.
  if [ "$name" != node-a ]; then
    node_kernel=/bin/true
  fi
  if [ "$name" = node-b ]; then
    fork_pause_ms=$B_FORK_PAUSE_MS
    fork_pause_phase=$B_FORK_PAUSE_PHASE
  fi
  install -d -m 0700 "$DIR/$name/sockets" "$DIR/$name/images"
  install -d -m 0700 "$DIR/$name/nfs-mounts"
  env \
    TARIT_API_KEY="$API_KEY" \
    TARIT_HOST_ID="$name" \
    TARIT_REGION=test-region \
    TARIT_ZONE="$zone" \
    TARIT_LISTEN="127.0.0.1:$control" \
    TARIT_PEER_LISTEN="127.0.0.1:$peer" \
    TARIT_RPC_ADDR="https://localhost:$peer" \
    TARIT_PEER_SECRET="$SECRET" \
    TARIT_PEER_TLS_CERT="$DIR/$name.pem" \
    TARIT_PEER_TLS_KEY="$DIR/$name.key" \
    TARIT_PEER_TLS_CLIENT_CA="$DIR/ca.pem" \
    TARIT_DATABASE_URL="$DATABASE_URL" \
    TARIT_ARTIFACT_MIN_REPLICAS=2 \
    TARIT_ARTIFACT_MIN_FAILURE_DOMAINS=2 \
    TARIT_VMM_BIN="$VMM" \
    TARIT_KERNEL="$node_kernel" \
    TARIT_ROOTFS=/bin/true \
    TARIT_ROOTFS_READONLY=0 \
    TARIT_VMM_AGENT="$AGENT" \
    TARIT_SOCKET_DIR="$DIR/$name/sockets" \
    TARIT_IMAGES_DIR="$DIR/$name/images" \
    TARIT_SHARED_BLOCK_PROVIDER=nfs_v4_1_block \
    TARIT_SHARED_BLOCK_ENDPOINT=127.0.0.1 \
    TARIT_SHARED_BLOCK_EXPORT=/ \
    TARIT_SHARED_BLOCK_MOUNT_ROOT="$DIR/$name/nfs-mounts" \
    TARIT_SHARED_BLOCK_MAX_BYTES=1073741824 \
    TARIT_SHARED_BLOCK_TIMEOUT_MS=30000 \
    TARIT_DB="$DIR/$name/store.db" \
    TARIT_NET_STATE="$DIR/$name/net-state.json" \
    TARIT_CONFIG="$DIR/missing.toml" \
    TARIT_ENABLE_NET=0 \
    TARIT_MAX_VMS=2 \
    TARIT_MAX_VCPUS=2 \
    TARIT_MAX_MEMORY_MIB=2048 \
    TARIT_WARM_POOL=0 \
    TARIT_ARTIFACT_GC_INTERVAL_SECS=1 \
    TARIT_ARTIFACT_GC_MIN_AGE_SECS=1 \
    TARIT_REAP_ON_SHUTDOWN=false \
    TARIT_TEST_FORK_PAUSE_PHASE="$fork_pause_phase" \
    TARIT_TEST_FORK_PAUSE_MS="$fork_pause_ms" \
    TARIT_TEST_FORK_PAUSE_AFTER_CHILD_MS="$fork_pause_ms" \
    RUST_LOG=info \
    "$TARITD" serve >"$log" 2>&1 &
  LAST_PID=$!
}

wait_health() {
  local url=$1 pid=$2
  for _ in $(seq 1 200); do
    kill -0 "$pid" 2>/dev/null || return 1
    curl -fsS --max-time 1 "$url/health" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}

api_json() {
  local method=$1 url=$2 body=${3:-}
  if [ -n "$body" ]; then
    curl -fsS --max-time 180 -X "$method" -H "X-API-Key: $API_KEY" \
      -H 'Content-Type: application/json' -d "$body" "$url"
  else
    curl -fsS --max-time 180 -X "$method" -H "X-API-Key: $API_KEY" "$url"
  fi
}

wait_exec_ubuntu() {
  local base=$1
  local vm_id=$2
  local response="$DIR/exec-$vm_id.json"
  for _ in $(seq 1 90); do
    code=$(curl -sS --max-time 5 -o "$response" -w '%{http_code}' \
      -H "X-API-Key: $API_KEY" -H 'Content-Type: application/json' \
      -d "{\"vm_id\":\"$vm_id\",\"command\":\"grep -E '^(ID|VERSION_ID)=' /etc/os-release\",\"timeout_ms\":5000}" \
      "$base/v1/execute" || true)
    if [ "$code" = 200 ] && python3 - "$response" <<'PY'
import json, sys
row=json.load(open(sys.argv[1]))
out=row.get("stdout", "")
assert "ID=ubuntu" in out and 'VERSION_ID="24.04"' in out, row
PY
    then
      return 0
    fi
    sleep 1
  done
  return 1
}

assert_nested_virtualization_hidden() {
  local base=$1
  local vm_id=$2
  local response="$DIR/nested-virt-$vm_id.json"
  api_json POST "$base/v1/execute" \
    "{\"vm_id\":\"$vm_id\",\"command\":\"if grep -Eq '(^|[[:space:]])(vmx|svm)([[:space:]]|$)' /proc/cpuinfo; then echo nested-cpu-flag-visible >&2; exit 71; fi; if [ -e /dev/kvm ]; then echo guest-kvm-device-visible >&2; exit 72; fi; echo nested-virtualization-hidden\",\"timeout_ms\":5000}" \
    >"$response"
  python3 - "$response" <<'PY'
import json, sys
row=json.load(open(sys.argv[1]))
assert row.get("status") == "completed", row
assert row.get("exit_code") == 0, row
assert row.get("stdout", "").strip() == "nested-virtualization-hidden", row
PY
}

echo '== start two exact-release Postgres+mTLS peers =='
start_node node-a zone-a "$A_CONTROL" "$A_PEER" "$DIR/node-a.log"
A_PID=$LAST_PID
start_node node-b zone-b "$B_CONTROL" "$B_PEER" "$DIR/node-b.log"
B_PID=$LAST_PID
wait_health "http://127.0.0.1:$A_CONTROL" "$A_PID"
wait_health "http://127.0.0.1:$B_CONTROL" "$B_PID"

echo '== build and boot a real Ubuntu 24.04 OCI image on node A =='
env \
  TARIT_API_KEY="$API_KEY" TARIT_LISTEN="127.0.0.1:$A_CONTROL" \
  TARIT_BASE_URL="http://127.0.0.1:$A_CONTROL" TARIT_VMM_BIN="$VMM" \
  TARIT_KERNEL="$KERNEL" TARIT_ROOTFS=/bin/true TARIT_VMM_AGENT="$AGENT" \
  TARIT_SOCKET_DIR="$DIR/node-a/sockets" TARIT_IMAGES_DIR="$DIR/node-a/images" \
  TARIT_DB="$DIR/node-a/store.db" TARIT_CONFIG="$DIR/missing.toml" \
  "$TARITD" image build --oci ubuntu:24.04 --name ubuntu2404 >"$DIR/image-build.log" 2>&1
env TARIT_DB="$DIR/node-a/store.db" TARIT_IMAGES_DIR="$DIR/node-a/images" \
  TARIT_VMM_BIN="$VMM" TARIT_VMM_AGENT="$AGENT" TARIT_CONFIG="$DIR/missing.toml" \
  "$TARITD" --json image ls >"$DIR/image.json"
IMAGE_SOURCE_DIGEST=$(python3 -c 'import json,sys; print(next(row for row in json.load(open(sys.argv[1])) if row["name"] == "ubuntu2404")["source_digest"])' "$DIR/image.json")
IMAGE_ROOTFS_DIGEST=$(python3 -c 'import json,sys; print(next(row for row in json.load(open(sys.argv[1])) if row["name"] == "ubuntu2404")["rootfs_digest"])' "$DIR/image.json")

echo '== prove node B has no prelocalized image or matching kernel =='
test "$(sqlite3 "$DIR/node-b/store.db" 'select count(*) from images')" = 0
test "$(sha256sum /bin/true | cut -d' ' -f1)" != \
  "$(sha256sum "$KERNEL" | cut -d' ' -f1)"

SOURCE_VM=$(api_json POST "http://127.0.0.1:$A_CONTROL/v1/vms" \
  '{"image":"ubuntu2404","vcpus":1,"memory_mib":256}' | \
  python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
wait_exec_ubuntu "http://127.0.0.1:$A_CONTROL" "$SOURCE_VM"
assert_nested_virtualization_hidden "http://127.0.0.1:$A_CONTROL" "$SOURCE_VM"

echo '== atomic live fork snapshots on A and starts the isolated child on B =='
api_json POST "http://127.0.0.1:$A_CONTROL/v1/execute" \
  "{\"vm_id\":\"$SOURCE_VM\",\"command\":\"printf cross-node-live-fork > /root/tarit-cross-node-fork-proof; sync; echo fork-source-ready\",\"timeout_ms\":30000}" \
  >"$DIR/fork-source-proof.json"
python3 - "$DIR/fork-source-proof.json" <<'PY'
import json,sys
row=json.load(open(sys.argv[1]))
assert row.get("exit_code") == 0 and "fork-source-ready" in row.get("stdout", ""), row
PY
REQUESTED_CROSS_NODE_FORK_VM=$(python3 -c 'import uuid; print(uuid.uuid4())')
FORK_REQUEST_BODY="{\"id\":\"$REQUESTED_CROSS_NODE_FORK_VM\"}"
if [ "${TARIT_TEST_CROSS_NODE_FORK_DEATH:-0}" = 1 ]; then
  curl -sS --max-time 180 -o "$DIR/cross-node-first-fork.json" -w '%{http_code}' \
    -X POST -H "X-API-Key: $API_KEY" -H 'Content-Type: application/json' \
    -d "$FORK_REQUEST_BODY" \
    "http://127.0.0.1:$B_CONTROL/v1/vms/$SOURCE_VM/fork" \
    >"$DIR/cross-node-first-fork-status" &
  FIRST_FORK_CURL_PID=$!
  FORK_PAUSED=0
  for _ in $(seq 1 1800); do
    if grep -F 'test fork paused at phase' "$DIR/node-b.log" | \
      grep -Fq "\"$B_FORK_PAUSE_PHASE\""; then
      FORK_PAUSED=1
      break
    fi
    sleep 0.1
  done
  [ "$FORK_PAUSED" = 1 ] || {
    echo "FAIL: cross-node fork did not reach $B_FORK_PAUSE_PHASE" >&2
    exit 1
  }
  operation_state=$(PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
    "select status from fleet_vm_fork_operations where child_vm_id='$REQUESTED_CROSS_NODE_FORK_VM'")
  child_state=$(PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
    "select coalesce((select status from fleet_vms where id='$REQUESTED_CROSS_NODE_FORK_VM'), '')")
  case "$B_FORK_PAUSE_PHASE" in
    after_claim|after_snapshot|after_localize|after_bind)
      [ "$operation_state:$child_state" = preparing: ]
      ;;
    after_child)
      [ "$operation_state:$child_state" = preparing:running ]
      ;;
    after_commit)
      [ "$operation_state:$child_state" = committed:running ]
      ;;
  esac
  kill -KILL "$B_PID"
  wait "$B_PID" 2>/dev/null || true
  wait "$FIRST_FORK_CURL_PID" 2>/dev/null || true
  FIRST_FORK_CURL_PID=""
  B_PID=""
  B_FORK_PAUSE_MS=0
  B_FORK_PAUSE_PHASE=""
  start_node node-b zone-b "$B_CONTROL" "$B_PEER" "$DIR/node-b.log"
  B_PID=$LAST_PID
  wait_health "http://127.0.0.1:$B_CONTROL" "$B_PID"
fi

CROSS_NODE_FORK_STATUS=$(curl -sS --max-time 180 \
  -o "$DIR/cross-node-fork-response.json" -w '%{http_code}' \
  -X POST -H "X-API-Key: $API_KEY" -H 'Content-Type: application/json' \
  -d "$FORK_REQUEST_BODY" \
  "http://127.0.0.1:$B_CONTROL/v1/vms/$SOURCE_VM/fork")
if [ "${TARIT_TEST_CROSS_NODE_FORK_DEATH:-0}" = 1 ] && \
   [[ "${TARIT_TEST_CROSS_NODE_FORK_DEATH_PHASE:-after_child}" =~ ^after_(child|commit)$ ]]; then
  [ "$CROSS_NODE_FORK_STATUS" = 200 ]
else
  [ "$CROSS_NODE_FORK_STATUS" = 201 ]
fi
CROSS_NODE_FORK_RESPONSE=$(cat "$DIR/cross-node-fork-response.json")
CROSS_NODE_FORK_VM=$(printf '%s' "$CROSS_NODE_FORK_RESPONSE" | python3 -c '
import json,sys
row=json.load(sys.stdin)
assert row["source_vm_id"] == sys.argv[1], row
assert row["vm"]["status"] == "running" and row["vm"]["startup_path"] == "snapshot_restore", row
print(row["vm"]["id"])
' "$SOURCE_VM")
[ "$CROSS_NODE_FORK_VM" = "$REQUESTED_CROSS_NODE_FORK_VM" ]
RETRY_CROSS_NODE_FORK_STATUS=$(curl -sS --max-time 30 \
  -o "$DIR/cross-node-fork-retry.json" -w '%{http_code}' \
  -X POST -H "X-API-Key: $API_KEY" -H 'Content-Type: application/json' \
  -d "$FORK_REQUEST_BODY" \
  "http://127.0.0.1:$B_CONTROL/v1/vms/$SOURCE_VM/fork")
[ "$RETRY_CROSS_NODE_FORK_STATUS" = 200 ]
python3 - "$DIR/cross-node-fork-retry.json" "$SOURCE_VM" "$CROSS_NODE_FORK_VM" <<'PY'
import json,sys
row=json.load(open(sys.argv[1]))
assert row.get("source_vm_id") == sys.argv[2], row
assert row.get("vm", {}).get("id") == sys.argv[3], row
PY
WRONG_CROSS_NODE_SOURCE=$(python3 -c 'import uuid; print(uuid.uuid4())')
WRONG_CROSS_NODE_STATUS=$(curl -sS --max-time 30 \
  -o "$DIR/cross-node-fork-wrong-source.json" -w '%{http_code}' \
  -X POST -H "X-API-Key: $API_KEY" -H 'Content-Type: application/json' \
  -d "$FORK_REQUEST_BODY" \
  "http://127.0.0.1:$B_CONTROL/v1/vms/$WRONG_CROSS_NODE_SOURCE/fork")
[ "$WRONG_CROSS_NODE_STATUS" = 409 ]
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select source_vm_id || ':' || source_host_id || ':' || target_host_id || ':' || status
     from fleet_vm_fork_operations where child_vm_id='$CROSS_NODE_FORK_VM'" | \
  grep -qx "$SOURCE_VM:node-a:node-b:committed"
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select count(*) from fleet_vm_fork_operations f join fleet_vms v
     on v.id=f.child_vm_id
    where f.child_vm_id='$CROSS_NODE_FORK_VM'
      and date_trunc('microseconds', f.child_created_at)=date_trunc('microseconds', v.created_at)" | \
  grep -qx 1
wait_exec_ubuntu "http://127.0.0.1:$B_CONTROL" "$CROSS_NODE_FORK_VM"
assert_nested_virtualization_hidden "http://127.0.0.1:$B_CONTROL" "$CROSS_NODE_FORK_VM"
api_json POST "http://127.0.0.1:$B_CONTROL/v1/execute" \
  "{\"vm_id\":\"$CROSS_NODE_FORK_VM\",\"command\":\"cat /root/tarit-cross-node-fork-proof\",\"timeout_ms\":5000}" \
  | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("exit_code") == 0 and row.get("stdout", "").strip() == "cross-node-live-fork", row'
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select host_id from fleet_vms where id='$CROSS_NODE_FORK_VM'" | grep -qx node-b
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select host_id from fleet_vms where id='$SOURCE_VM'" | grep -qx node-a
FORK_ARTIFACT_ID=$(sqlite3 "$DIR/node-b/store.db" \
  "select snapshot_id from snapshots where ephemeral_owner_vm_id='$CROSS_NODE_FORK_VM'")
test -n "$FORK_ARTIFACT_ID"
FORK_SOURCE_RAM=$(sqlite3 "$DIR/node-a/store.db" \
  "select path from snapshots where snapshot_id='$FORK_ARTIFACT_ID'")
FORK_SOURCE_OVERLAY=$(sqlite3 "$DIR/node-a/store.db" \
  "select coalesce(overlay_path,'') from snapshots where snapshot_id='$FORK_ARTIFACT_ID'")
FORK_TARGET_RAM=$(sqlite3 "$DIR/node-b/store.db" \
  "select path from snapshots where snapshot_id='$FORK_ARTIFACT_ID'")
FORK_TARGET_OVERLAY=$(sqlite3 "$DIR/node-b/store.db" \
  "select coalesce(overlay_path,'') from snapshots where snapshot_id='$FORK_ARTIFACT_ID'")
api_json DELETE "http://127.0.0.1:$B_CONTROL/v1/vms/$CROSS_NODE_FORK_VM" >/dev/null
CROSS_NODE_FORK_VM=""
FORK_GC_CONVERGED=0
for _ in $(seq 1 60); do
  FORK_GLOBAL=$(PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
    "select (select count(*) from fleet_artifacts where artifact_id='$FORK_ARTIFACT_ID') +
            (select count(*) from fleet_snapshots where snapshot_id='$FORK_ARTIFACT_ID')")
  FORK_SOURCE_LOCAL=$(sqlite3 "$DIR/node-a/store.db" \
    "select (select count(*) from artifacts where artifact_id='$FORK_ARTIFACT_ID') +
            (select count(*) from snapshots where snapshot_id='$FORK_ARTIFACT_ID')")
  FORK_TARGET_LOCAL=$(sqlite3 "$DIR/node-b/store.db" \
    "select (select count(*) from artifacts where artifact_id='$FORK_ARTIFACT_ID') +
            (select count(*) from snapshots where snapshot_id='$FORK_ARTIFACT_ID')")
  if [ "$FORK_GLOBAL:$FORK_SOURCE_LOCAL:$FORK_TARGET_LOCAL" = '0:0:0' ]; then
    FORK_GC_CONVERGED=1
    break
  fi
  sleep 1
done
test "$FORK_GC_CONVERGED" = 1
for deleted in "$FORK_SOURCE_RAM" "$FORK_SOURCE_RAM.integrity" "$FORK_SOURCE_OVERLAY" \
  "$FORK_TARGET_RAM" "$FORK_TARGET_RAM.integrity" "$FORK_TARGET_OVERLAY"; do
  [ -z "$deleted" ] || test ! -e "$deleted"
done

echo '== snapshot on A; replication policy must initially be degraded =='
ARTIFACT_ID=$(api_json POST "http://127.0.0.1:$A_CONTROL/v1/vms/$SOURCE_VM/snapshot" \
  '{"diff":false}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["snapshot_id"])')
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select replication_state || ':' || count(*) from fleet_artifacts a join fleet_artifact_replicas r using (artifact_id) where a.artifact_id='$ARTIFACT_ID' group by replication_state" | \
  grep -qx 'degraded:1'

echo '== branch creation triggers authenticated cross-zone localization =='
BRANCH_ID=$(python3 -c 'import uuid; print(uuid.uuid4())')
BRANCH_BODY="{\"branch_id\":\"$BRANCH_ID\",\"name\":\"ubuntu-peer-main\",\"head_artifact_id\":\"$ARTIFACT_ID\",\"source_vm_id\":\"$SOURCE_VM\"}"

echo '== tampered boot metadata fails closed before replica publication =='
python3 - "$DIR/node-a/store.db" "$ARTIFACT_ID" "$DIR/original-cmdline" <<'PY'
import sqlite3, sys
db_path, artifact_id, output = sys.argv[1:]
with sqlite3.connect(db_path, timeout=30) as db:
    row=db.execute("select cmdline from snapshots where snapshot_id=?", (artifact_id,)).fetchone()
    assert row and row[0], row
    open(output, "w").write(row[0])
    db.execute("update snapshots set cmdline=? where snapshot_id=?", (row[0] + " init=/bin/sh", artifact_id))
PY
TAMPER_STATUS=$(curl -sS --max-time 180 -o "$DIR/tamper-response.json" -w '%{http_code}' \
  -H "X-API-Key: $API_KEY" -H 'Content-Type: application/json' -d "$BRANCH_BODY" \
  "http://127.0.0.1:$A_CONTROL/v1/branches")
test "$TAMPER_STATUS" = 503
python3 - "$DIR/tamper-response.json" "$DIR" <<'PY'
import json, sys
row=json.load(open(sys.argv[1]))
text=json.dumps(row)
assert sys.argv[2] not in text and "init=/bin/sh" not in text and "localhost:" not in text, row
PY
test "$(sqlite3 "$DIR/node-b/store.db" "select count(*) from artifacts where artifact_id='$ARTIFACT_ID'")" = 0
test -z "$(find "$DIR/node-b/sockets/snapshots" -maxdepth 1 -type f \( -name '.replica-stage-*' -o -name 'replica-*' \) -print 2>/dev/null)"
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select count(*) from fleet_artifact_replicas where artifact_id='$ARTIFACT_ID'" | grep -qx 1
python3 - "$DIR/node-a/store.db" "$ARTIFACT_ID" "$DIR/original-cmdline" <<'PY'
import sqlite3, sys
db_path, artifact_id, source = sys.argv[1:]
with sqlite3.connect(db_path, timeout=30) as db:
    db.execute("update snapshots set cmdline=? where snapshot_id=?", (open(source).read(), artifact_id))
PY

BRANCH_STATUS=$(curl -sS --max-time 180 -o "$DIR/branch-response.json" -w '%{http_code}' \
  -H "X-API-Key: $API_KEY" -H 'Content-Type: application/json' -d "$BRANCH_BODY" \
  "http://127.0.0.1:$A_CONTROL/v1/branches")
if [ "$BRANCH_STATUS" != 201 ]; then
  echo "FAIL: clean branch localization returned HTTP $BRANCH_STATUS" >&2
  cat "$DIR/branch-response.json" >&2
  grep -E 'artifact|boot metadata|kernel|localiz' "$DIR/node-a.log" "$DIR/node-b.log" | tail -80 >&2 || true
  exit 1
fi
BRANCH=$(cat "$DIR/branch-response.json")
python3 - "$BRANCH_ID" "$ARTIFACT_ID" "$BRANCH" <<'PY'
import json, sys
row=json.loads(sys.argv[3])
assert row["branch_id"] == sys.argv[1] and row["head_artifact_id"] == sys.argv[2], row
assert not ({"owner_key","host_id","storage_locator","failure_domain"} & set(row)), row
PY
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select a.replication_state || ':' || count(*) || ':' || count(distinct r.failure_domain) from fleet_artifacts a join fleet_artifact_replicas r using (artifact_id) where a.artifact_id='$ARTIFACT_ID' and r.status='available' and r.verified_at is not null group by a.replication_state" | \
  grep -qx 'ready:2:2'
sqlite3 "$DIR/node-b/store.db" \
  "select count(*) from images where source_digest='$IMAGE_SOURCE_DIGEST' and rootfs_digest='$IMAGE_ROOTFS_DIGEST' and rootfs_path like '%/peer-boot-inputs/rootfs-%'" | grep -qx 1
sqlite3 "$DIR/node-b/store.db" \
  "select count(*) from snapshots where snapshot_id='$ARTIFACT_ID' and kernel_path like '%/peer-boot-inputs/kernel-%' and rootfs_path like '%/peer-boot-inputs/rootfs-%'" | grep -qx 1
LOCALIZED_KERNEL=$(sqlite3 "$DIR/node-b/store.db" \
  "select kernel_path from snapshots where snapshot_id='$ARTIFACT_ID'")
LOCALIZED_ROOTFS=$(sqlite3 "$DIR/node-b/store.db" \
  "select rootfs_path from snapshots where snapshot_id='$ARTIFACT_ID'")
test "$(stat -c %a "$LOCALIZED_KERNEL")" = 444
test "$(stat -c %a "$LOCALIZED_ROOTFS")" = 444
test "sha256:$(sha256sum "$LOCALIZED_KERNEL" | cut -d' ' -f1)" = \
  "sha256:$(sha256sum "$KERNEL" | cut -d' ' -f1)"
test "sha256:$(sha256sum "$LOCALIZED_ROOTFS" | cut -d' ' -f1)" = "$IMAGE_ROOTFS_DIGEST"
sqlite3 "$DIR/node-b/store.db" \
  "select count(*) from artifacts where artifact_id='$ARTIFACT_ID' and status='available' and replication_state='ready'" | grep -qx 1

echo '== non-owner HTTP hibernate replicates before true scale-to-zero; HTTP resume routes back to owner =='
api_json POST "http://127.0.0.1:$B_CONTROL/v1/vms/$SOURCE_VM/hibernate" '{}' >"$DIR/hibernate.json"
python3 - "$DIR/hibernate.json" "$SOURCE_VM" <<'PY'
import json, sys
row=json.load(open(sys.argv[1]))
assert row["id"] == sys.argv[2] and row["status"] == "hibernated", row
assert not ({"host_id","socket_path","pid","kernel_path","rootfs_path"} & set(row)), row
PY
sqlite3 "$DIR/node-a/store.db" \
  "select status || ':' || coalesce(pid,'') || ':' || coalesce(socket_path,'') from vms where id='$SOURCE_VM'" | \
  grep -qx 'hibernated::'
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select count(*) from fleet_artifacts where owner_key=(select owner_key from fleet_vms where id='$SOURCE_VM') and source_vm_id='$SOURCE_VM' and replication_state='ready'" | \
  grep -qx 2
api_json POST "http://127.0.0.1:$B_CONTROL/v1/vms/$SOURCE_VM/resume" '{}' >"$DIR/resume.json"
python3 - "$DIR/resume.json" "$SOURCE_VM" <<'PY'
import json, sys
row=json.load(open(sys.argv[1]))
assert row["id"] == sys.argv[2] and row["status"] == "running", row
assert row["startup_path"] == "snapshot_restore", row
PY
wait_exec_ubuntu "http://127.0.0.1:$B_CONTROL" "$SOURCE_VM"
assert_nested_virtualization_hidden "http://127.0.0.1:$B_CONTROL" "$SOURCE_VM"

echo '== hibernate again, change policy while scaled to zero, lose owner, and wake the same VM ID on B =='
HIBERNATE_STATUS=$(curl -sS --max-time 180 -o "$DIR/hibernate-owner-loss.json" -w '%{http_code}' \
  -X POST -H "X-API-Key: $API_KEY" -H 'Content-Type: application/json' -d '{}' \
  "http://127.0.0.1:$B_CONTROL/v1/vms/$SOURCE_VM/hibernate")
if [ "$HIBERNATE_STATUS" != 200 ]; then
  echo "FAIL: second hibernate returned HTTP $HIBERNATE_STATUS" >&2
  cat "$DIR/hibernate-owner-loss.json" >&2
  tail -n 120 "$DIR/node-a.log" "$DIR/node-b.log" >&2 || true
  exit 1
fi

echo '== create a shared volume VM on A and hibernate it for stale-owner recovery on B =='
VOLUME_RESPONSE=$(api_json POST "http://127.0.0.1:$A_CONTROL/v1/volumes" \
  '{"name":"peer-failover-workspace","size_bytes":67108864,"provider":"nfs_v4_1_block"}')
VOLUME_ID=$(printf '%s' "$VOLUME_RESPONSE" | python3 -c '
import json,sys
row=json.load(sys.stdin)
assert row["status"] == "available" and row["provider"] == "nfs_v4_1_block", row
assert not ({"owner_key","host_id","last_error","private_path"} & set(row)), row
print(row["id"])
')
VOLUME_VM=$(api_json POST "http://127.0.0.1:$A_CONTROL/v1/vms" \
  "{\"image\":\"ubuntu2404\",\"vcpus\":1,\"memory_mib\":256,\"volumes\":[{\"volume_id\":\"$VOLUME_ID\",\"mode\":\"read_write\"}]}" | \
  python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
wait_exec_ubuntu "http://127.0.0.1:$A_CONTROL" "$VOLUME_VM"
api_json POST "http://127.0.0.1:$A_CONTROL/v1/execute" \
  "{\"vm_id\":\"$VOLUME_VM\",\"command\":\"export PATH=/usr/sbin:/usr/bin:/sbin:/bin; test -b /dev/vdb; mkfs.ext4 -q /dev/vdb; mkdir -p /mnt/work; mount /dev/vdb /mnt/work; printf peer-shared-volume-proof > /mnt/work/proof; sync; cat /mnt/work/proof\",\"timeout_ms\":30000}" | \
  python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("exit_code") == 0 and row.get("stdout", "").strip() == "peer-shared-volume-proof", row'
api_json POST "http://127.0.0.1:$B_CONTROL/v1/vms/$VOLUME_VM/hibernate" '{}' | \
  python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("status") == "hibernated", row'
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select count(*) from fleet_vm_volume_attachments where vm_id='$VOLUME_VM' and volume_id='$VOLUME_ID' and mode='read_write'" | grep -qx 1
[ -f "$NFS_EXPORT/.tarit-block-volumes/$VOLUME_ID.block" ]
if findmnt -rn -t nfs4 -o TARGET | grep -Fq "$DIR/"; then
  echo 'FAIL: shared provider left a live NFS mount before owner loss' >&2
  exit 1
fi
EGRESS_STATUS=$(curl -sS --max-time 180 -o "$DIR/egress-hibernated.json" -w '%{http_code}' \
  -X PUT -H "X-API-Key: $API_KEY" -H 'Content-Type: application/json' \
  -d '{"expected_revision":1,"allowlist":["203.0.113.11/32:443/tcp"],"allow_existing":true}' \
  "http://127.0.0.1:$B_CONTROL/v1/vms/$SOURCE_VM/egress-policy")
if [ "$EGRESS_STATUS" != 200 ]; then
  echo "FAIL: hibernated egress update returned HTTP $EGRESS_STATUS" >&2
  cat "$DIR/egress-hibernated.json" >&2
  tail -n 120 "$DIR/node-a.log" "$DIR/node-b.log" >&2 || true
  exit 1
fi
python3 - "$DIR/egress-hibernated.json" <<'PY'
import json, sys
row=json.load(open(sys.argv[1]))
assert row["revision"] == 2 and row["allow_existing"] is True, row
assert row["allowlist"] == ["203.0.113.11:443/tcp"], row
PY
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select count(*) from fleet_hibernations where vm_id='$SOURCE_VM' and policy_revision=2 and allow_existing=true" | \
  grep -qx 1
kill -TERM "$A_PID"
wait "$A_PID"
A_PID=""
for _ in $(seq 1 30); do
  OWNER_AGE=$(PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
    "select extract(epoch from now()-last_heartbeat)::int from fleet_hosts where host_id='node-a'")
  [ "${OWNER_AGE:-0}" -ge 15 ] && break
  sleep 1
done
test "${OWNER_AGE:-0}" -ge 15
wait_exec_ubuntu "http://127.0.0.1:$B_CONTROL" "$SOURCE_VM"
assert_nested_virtualization_hidden "http://127.0.0.1:$B_CONTROL" "$SOURCE_VM"
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select host_id || ':' || status from fleet_vms where id='$SOURCE_VM'" | grep -qx 'node-b:running'
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select count(*) from fleet_hibernations where vm_id='$SOURCE_VM'" | grep -qx 0
sqlite3 "$DIR/node-b/store.db" \
  "select revision || ':' || allow_existing || ':' || allowlist_json from vm_egress_policies where vm_id='$SOURCE_VM'" | \
  grep -Fqx '2:1:["203.0.113.11:443/tcp"]'

echo '== HTTP exec recovers the shared-volume VM on B with its durable attachment =='
api_json POST "http://127.0.0.1:$B_CONTROL/v1/execute" \
  "{\"vm_id\":\"$VOLUME_VM\",\"command\":\"export PATH=/usr/sbin:/usr/bin:/sbin:/bin; mkdir -p /mnt/work; mountpoint -q /mnt/work || mount /dev/vdb /mnt/work; cat /mnt/work/proof\",\"timeout_ms\":30000}" | \
  python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("exit_code") == 0 and row.get("stdout", "").strip() == "peer-shared-volume-proof", row'
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select host_id || ':' || status from fleet_vms where id='$VOLUME_VM'" | grep -qx 'node-b:running'
sqlite3 "$DIR/node-b/store.db" \
  "select count(*) from vm_volume_attachments where vm_id='$VOLUME_VM' and volume_id='$VOLUME_ID'" | grep -qx 1
api_json DELETE "http://127.0.0.1:$B_CONTROL/v1/vms/$VOLUME_VM" >/dev/null
VOLUME_VM=""
api_json DELETE "http://127.0.0.1:$B_CONTROL/v1/volumes/$VOLUME_ID" >/dev/null
[ ! -e "$NFS_EXPORT/.tarit-block-volumes/$VOLUME_ID.block" ]
VOLUME_ID=""
api_json DELETE "http://127.0.0.1:$B_CONTROL/v1/vms/$SOURCE_VM" >/dev/null
SOURCE_VM=""

echo '== restore the independent branch solely from B replica =='
for _ in $(seq 1 30); do
  REPLICATION_STATE=$(PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
    "select replication_state from fleet_artifacts where artifact_id='$ARTIFACT_ID'")
  [ "$REPLICATION_STATE" = degraded ] && break
  sleep 1
done
test "$REPLICATION_STATE" = degraded
RESTORED_VM=$(api_json POST "http://127.0.0.1:$B_CONTROL/v1/branches/$BRANCH_ID/restore" '{}' | \
  python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["startup_path"] == "snapshot_restore", row; print(row["id"])')
wait_exec_ubuntu "http://127.0.0.1:$B_CONTROL" "$RESTORED_VM"
assert_nested_virtualization_hidden "http://127.0.0.1:$B_CONTROL" "$RESTORED_VM"
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select replication_state from fleet_artifacts where artifact_id='$ARTIFACT_ID'" | grep -qx degraded

echo '== start node C and continuously repair the degraded replica set =='
start_node node-c zone-c "$C_CONTROL" "$C_PEER" "$DIR/node-c.log"
C_PID=$LAST_PID
wait_health "http://127.0.0.1:$C_CONTROL" "$C_PID"
test "$(sqlite3 "$DIR/node-c/store.db" 'select count(*) from images')" = 0
REPAIR_STATE=""
for _ in $(seq 1 60); do
  REPAIR_STATE=$(PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
    "select a.replication_state || ':' || count(*) || ':' || count(distinct r.failure_domain)
       from fleet_artifacts a
       join fleet_artifact_replicas r using (artifact_id)
       join fleet_hosts h on h.host_id=r.host_id
      where a.artifact_id='$ARTIFACT_ID' and r.status='available' and r.verified_at is not null
        and h.healthy=true and h.last_heartbeat >= now()-interval '15 seconds'
      group by a.replication_state")
  [ "$REPAIR_STATE" = 'ready:2:2' ] && break
  sleep 1
done
test "$REPAIR_STATE" = 'ready:2:2'
sqlite3 "$DIR/node-c/store.db" \
  "select count(*) from artifacts where artifact_id='$ARTIFACT_ID' and status='available'" | grep -qx 1
sqlite3 "$DIR/node-c/store.db" \
  "select count(*) from images where source_digest='$IMAGE_SOURCE_DIGEST' and rootfs_digest='$IMAGE_ROOTFS_DIGEST' and rootfs_path like '%/peer-boot-inputs/rootfs-%'" | grep -qx 1
sqlite3 "$DIR/node-c/store.db" \
  "select count(*) from snapshots where snapshot_id='$ARTIFACT_ID' and kernel_path like '%/peer-boot-inputs/kernel-%' and rootfs_path like '%/peer-boot-inputs/rootfs-%'" | grep -qx 1
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select count(*) from fleet_artifact_repair_leases where artifact_id='$ARTIFACT_ID'" | grep -qx 0

echo '== private placement and storage metadata never appears on public APIs =='
api_json GET "http://127.0.0.1:$B_CONTROL/v1/branches/$BRANCH_ID" >"$DIR/public-branch.json"
python3 - "$DIR/public-branch.json" <<'PY'
import json, sys
text=open(sys.argv[1]).read()
row=json.loads(text)
assert not ({"owner_key","host_id","storage_locator","failure_domain"} & set(row)), row
assert "/" not in row["name"], row
PY

echo '== last branch deletion preserves a live lazy restore, then physical replicas converge =='
B_REPLICA_RAM=$(sqlite3 "$DIR/node-b/store.db" \
  "select path from snapshots where snapshot_id='$ARTIFACT_ID'")
B_REPLICA_OVERLAY=$(sqlite3 "$DIR/node-b/store.db" \
  "select coalesce(overlay_path,'') from snapshots where snapshot_id='$ARTIFACT_ID'")
C_REPLICA_RAM=$(sqlite3 "$DIR/node-c/store.db" \
  "select path from snapshots where snapshot_id='$ARTIFACT_ID'")
C_REPLICA_OVERLAY=$(sqlite3 "$DIR/node-c/store.db" \
  "select coalesce(overlay_path,'') from snapshots where snapshot_id='$ARTIFACT_ID'")
api_json DELETE "http://127.0.0.1:$B_CONTROL/v1/branches/$BRANCH_ID" >/dev/null
PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
  "select reference_count from fleet_artifacts where artifact_id='$ARTIFACT_ID'" | grep -qx 1
wait_exec_ubuntu "http://127.0.0.1:$B_CONTROL" "$RESTORED_VM"
api_json DELETE "http://127.0.0.1:$B_CONTROL/v1/vms/$RESTORED_VM" >/dev/null
RESTORED_VM=""
GC_CONVERGED=0
for _ in $(seq 1 60); do
  GLOBAL_COUNT=$(PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
    "select count(*) from fleet_artifacts where artifact_id='$ARTIFACT_ID'")
  B_LOCAL=$(sqlite3 "$DIR/node-b/store.db" \
    "select (select count(*) from artifacts where artifact_id='$ARTIFACT_ID') + (select count(*) from snapshots where snapshot_id='$ARTIFACT_ID')")
  C_LOCAL=$(sqlite3 "$DIR/node-c/store.db" \
    "select (select count(*) from artifacts where artifact_id='$ARTIFACT_ID') + (select count(*) from snapshots where snapshot_id='$ARTIFACT_ID')")
  if [ "$GLOBAL_COUNT:$B_LOCAL:$C_LOCAL" = '0:0:0' ]; then
    GC_CONVERGED=1
    break
  fi
  sleep 1
done
test "$GC_CONVERGED" = 1
for deleted in \
  "$B_REPLICA_RAM" "$B_REPLICA_RAM.integrity" "$B_REPLICA_OVERLAY" \
  "$C_REPLICA_RAM" "$C_REPLICA_RAM.integrity" "$C_REPLICA_OVERLAY"; do
  [ -z "$deleted" ] || test ! -e "$deleted"
done

echo 'PASS: atomic live fork started its isolated child across nodes; boot metadata failed closed; Ubuntu OCI artifacts used peer mTLS; hibernate reached zero; stale owner was fenced; HTTP exec woke the same VM ID on B with durable egress and its shared-volume attachment; cross-node volume deletion removed the backing object; branch restored; node C repaired degradation; last-branch deletion preserved the live lazy restore and physical replica GC converged'
