#!/usr/bin/env bash
# Two-process Postgres-backed acceptance gate for peer mTLS and boot fencing.
set -Eeuo pipefail

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD="${TARITD_BIN:-$ROOT/orch/target/release/taritd}"
SECRET="0123456789abcdef0123456789abcdef-peer-e2e"
DIR=$(mktemp -d "${TMPDIR:-/tmp}/tarit-peer-mtls.XXXXXX")
chmod 700 "$DIR"
DB_SUFFIX=$(python3 -c 'import secrets; print(secrets.token_hex(6))')
DB_NAME="tarit_peer_$DB_SUFFIX"
DB_ROLE="tarit_peer_role_$DB_SUFFIX"
DB_PASSWORD=$(python3 -c 'import secrets; print(secrets.token_hex(24))')
DATABASE_URL="postgresql://$DB_ROLE:$DB_PASSWORD@127.0.0.1:5432/$DB_NAME?sslmode=disable"
A_PID=""
B_PID=""
LAST_PID=""

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

cleanup() {
  for pid in "$A_PID" "$B_PID"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  sudo -u postgres psql -v ON_ERROR_STOP=1 -d postgres -qAtc \
    "select pg_terminate_backend(pid) from pg_stat_activity where datname='$DB_NAME' and pid <> pg_backend_pid()" >/dev/null 2>&1 || true
  sudo -u postgres dropdb --if-exists "$DB_NAME" >/dev/null 2>&1 || true
  sudo -u postgres dropuser --if-exists "$DB_ROLE" >/dev/null 2>&1 || true
  find "$DIR" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT
on_error() {
  local exit_status=$?
  trap - ERR
  echo "FAIL: peer mTLS gate exited $exit_status" >&2
  tail -100 "$DIR"/*.log 2>/dev/null || true
  exit "$exit_status"
}
trap on_error ERR

for required in curl openssl python3 psql createdb; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done

sudo -u postgres psql -v ON_ERROR_STOP=1 -d postgres -qAtc \
  "create role $DB_ROLE login password '$DB_PASSWORD'"
sudo -u postgres createdb -O "$DB_ROLE" "$DB_NAME"

make_ca() {
  local name=$1
  openssl req -x509 -newkey rsa:2048 -nodes -days 2 -sha256 \
    -subj "/CN=Tarit $name test CA" \
    -keyout "$DIR/$name-ca.key" -out "$DIR/$name-ca.pem" >/dev/null 2>&1
  chmod 600 "$DIR/$name-ca.key"
}

make_leaf() {
  local name=$1 ca=$2
  openssl req -newkey rsa:2048 -nodes -sha256 -subj "/CN=$name" \
    -keyout "$DIR/$name.key" -out "$DIR/$name.csr" >/dev/null 2>&1
  chmod 600 "$DIR/$name.key"
  printf '%s\n' 'subjectAltName=DNS:localhost' 'extendedKeyUsage=serverAuth,clientAuth' >"$DIR/$name.ext"
  openssl x509 -req -days 2 -sha256 -in "$DIR/$name.csr" \
    -CA "$DIR/$ca-ca.pem" -CAkey "$DIR/$ca-ca.key" -CAcreateserial \
    -extfile "$DIR/$name.ext" -out "$DIR/$name.pem" >/dev/null 2>&1
}

make_ca old
make_ca new
make_ca rogue
make_leaf node-a old
make_leaf node-b old
make_leaf node-new new
make_leaf node-b-new new
make_leaf node-rogue rogue
cat "$DIR/old-ca.pem" "$DIR/new-ca.pem" >"$DIR/overlap-ca.pem"

start_node() {
  local name=$1 control=$2 peer=$3 cert=$4 key=$5 ca=$6 log=$7
  install -d -m 0700 "$DIR/$name" "$DIR/$name/sockets" "$DIR/$name/images"
  env \
    TARIT_API_KEY="peer-e2e-api-key-$name" \
    TARIT_HOST_ID="$name" \
    TARIT_LISTEN="127.0.0.1:$control" \
    TARIT_PEER_LISTEN="127.0.0.1:$peer" \
    TARIT_RPC_ADDR="https://localhost:$peer" \
    TARIT_PEER_SECRET="$SECRET" \
    TARIT_PEER_TLS_CERT="$cert" \
    TARIT_PEER_TLS_KEY="$key" \
    TARIT_PEER_TLS_CLIENT_CA="$ca" \
    TARIT_DATABASE_URL="$DATABASE_URL" \
    TARIT_VMM_BIN=/bin/false \
    TARIT_KERNEL=/bin/true \
    TARIT_ROOTFS=/bin/true \
    TARIT_SOCKET_DIR="$DIR/$name/sockets" \
    TARIT_IMAGES_DIR="$DIR/$name/images" \
    TARIT_DB="$DIR/$name/fleet.db" \
    TARIT_NET_STATE="$DIR/$name/net-state.json" \
    TARIT_CONFIG="$DIR/missing.toml" \
    TARIT_WARM_POOL=0 \
    TARIT_REAP_ON_SHUTDOWN=false \
    "$TARITD" serve >"$log" 2>&1 &
  LAST_PID=$!
}

wait_health() {
  local url=$1 pid=$2
  for _ in $(seq 1 100); do
    kill -0 "$pid" 2>/dev/null || { echo "node exited before health" >&2; return 1; }
    curl -fsS --max-time 1 "$url/health" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}

session() {
  local host=$1
  PGPASSWORD="$DB_PASSWORD" psql "$DATABASE_URL" -qAtc \
    "select boot_session_id::text from fleet_hosts where host_id='$host'"
}

echo "== start two Postgres-backed mTLS peers =="
start_node node-a "$A_CONTROL" "$A_PEER" "$DIR/node-a.pem" "$DIR/node-a.key" "$DIR/overlap-ca.pem" "$DIR/node-a.log"
A_PID=$LAST_PID
start_node node-b "$B_CONTROL" "$B_PEER" "$DIR/node-b.pem" "$DIR/node-b.key" "$DIR/overlap-ca.pem" "$DIR/node-b.log"
B_PID=$LAST_PID
wait_health "http://127.0.0.1:$A_CONTROL" "$A_PID"
wait_health "http://127.0.0.1:$B_CONTROL" "$B_PID"
A_SESSION=$(session node-a)
B_SESSION=$(session node-b)
test -n "$A_SESSION" && test -n "$B_SESSION"

echo "== mTLS rejects absent/untrusted certificates =="
if curl -fsS --max-time 5 --cacert "$DIR/old-ca.pem" "https://localhost:$B_PEER/internal/v1/vms" >/dev/null 2>&1; then
  echo "FAIL: peer listener accepted a client without a certificate" >&2
  exit 1
fi
if curl -fsS --max-time 5 --cert "$DIR/node-rogue.pem" --key "$DIR/node-rogue.key" \
  --cacert "$DIR/old-ca.pem" "https://localhost:$B_PEER/internal/v1/vms" >/dev/null 2>&1; then
  echo "FAIL: peer listener accepted an untrusted client certificate" >&2
  exit 1
fi

PROBE="$ROOT/orch/tests/peer_mtls_probe.py"
VM_ID=00000000-0000-0000-0000-000000000001
TARGET_URL="https://localhost:$B_PEER/internal/v1/vms/$VM_ID/status"
python3 "$PROBE" "$TARGET_URL" "$DIR/node-a.pem" "$DIR/node-a.key" "$DIR/old-ca.pem" \
  "$SECRET" node-a "$A_SESSION" node-b "$B_SESSION" 404
python3 "$PROBE" "$TARGET_URL" "$DIR/node-new.pem" "$DIR/node-new.key" "$DIR/old-ca.pem" \
  "$SECRET" node-a "$A_SESSION" node-b "$B_SESSION" 401

echo "== target and source boot sessions fence stale processes =="
python3 "$PROBE" "$TARGET_URL" "$DIR/node-a.pem" "$DIR/node-a.key" "$DIR/old-ca.pem" \
  "$SECRET" node-a "$A_SESSION" node-b 00000000-0000-0000-0000-000000000099 401
kill -TERM "$A_PID"
wait "$A_PID" || true
start_node node-a "$A_CONTROL" "$A_PEER" "$DIR/node-a.pem" "$DIR/node-a.key" "$DIR/overlap-ca.pem" "$DIR/node-a.log"
A_PID=$LAST_PID
wait_health "http://127.0.0.1:$A_CONTROL" "$A_PID"
NEW_A_SESSION=$(session node-a)
test "$NEW_A_SESSION" != "$A_SESSION"
python3 "$PROBE" "$TARGET_URL" "$DIR/node-a.pem" "$DIR/node-a.key" "$DIR/old-ca.pem" \
  "$SECRET" node-a "$A_SESSION" node-b "$B_SESSION" 401
python3 "$PROBE" "$TARGET_URL" "$DIR/node-a.pem" "$DIR/node-a.key" "$DIR/old-ca.pem" \
  "$SECRET" node-a "$NEW_A_SESSION" node-b "$B_SESSION" 404

echo "== CA overlap accepts old/new clients, rotation fences old CA =="
for identity in node-a node-new; do
  code=$(curl -sS --max-time 5 --cert "$DIR/$identity.pem" --key "$DIR/$identity.key" \
    --cacert "$DIR/old-ca.pem" -o /dev/null -w '%{http_code}' \
    "https://localhost:$B_PEER/internal/v1/vms")
  test "$code" = 401
done

kill -TERM "$A_PID"
wait "$A_PID" || true
start_node node-a "$A_CONTROL" "$A_PEER" "$DIR/node-new.pem" "$DIR/node-new.key" "$DIR/overlap-ca.pem" "$DIR/node-a.log"
A_PID=$LAST_PID
wait_health "http://127.0.0.1:$A_CONTROL" "$A_PID"
ROTATED_A_SESSION=$(session node-a)
test "$ROTATED_A_SESSION" != "$NEW_A_SESSION"
python3 "$PROBE" "$TARGET_URL" "$DIR/node-new.pem" "$DIR/node-new.key" "$DIR/old-ca.pem" \
  "$SECRET" node-a "$ROTATED_A_SESSION" node-b "$B_SESSION" 404
python3 "$PROBE" "$TARGET_URL" "$DIR/node-a.pem" "$DIR/node-a.key" "$DIR/old-ca.pem" \
  "$SECRET" node-a "$ROTATED_A_SESSION" node-b "$B_SESSION" 401

kill -TERM "$B_PID"
wait "$B_PID" || true
start_node node-b "$B_CONTROL" "$B_PEER" "$DIR/node-b-new.pem" "$DIR/node-b-new.key" "$DIR/overlap-ca.pem" "$DIR/node-b.log"
B_PID=$LAST_PID
wait_health "http://127.0.0.1:$B_CONTROL" "$B_PID"
OVERLAP_B_SESSION=$(session node-b)
test "$OVERLAP_B_SESSION" != "$B_SESSION"
python3 "$PROBE" "$TARGET_URL" "$DIR/node-new.pem" "$DIR/node-new.key" "$DIR/new-ca.pem" \
  "$SECRET" node-a "$ROTATED_A_SESSION" node-b "$OVERLAP_B_SESSION" 404

echo "== remove old CA only after both peers present new leaves =="
kill -TERM "$A_PID"
wait "$A_PID" || true
start_node node-a "$A_CONTROL" "$A_PEER" "$DIR/node-new.pem" "$DIR/node-new.key" "$DIR/new-ca.pem" "$DIR/node-a.log"
A_PID=$LAST_PID
wait_health "http://127.0.0.1:$A_CONTROL" "$A_PID"
NEW_ONLY_A_SESSION=$(session node-a)
test "$NEW_ONLY_A_SESSION" != "$ROTATED_A_SESSION"

kill -TERM "$B_PID"
wait "$B_PID" || true
start_node node-b "$B_CONTROL" "$B_PEER" "$DIR/node-b-new.pem" "$DIR/node-b-new.key" "$DIR/new-ca.pem" "$DIR/node-b.log"
B_PID=$LAST_PID
wait_health "http://127.0.0.1:$B_CONTROL" "$B_PID"
NEW_ONLY_B_SESSION=$(session node-b)
test "$NEW_ONLY_B_SESSION" != "$OVERLAP_B_SESSION"
if curl -fsS --max-time 5 --cert "$DIR/node-a.pem" --key "$DIR/node-a.key" \
  --cacert "$DIR/new-ca.pem" "https://localhost:$B_PEER/internal/v1/vms" >/dev/null 2>&1; then
  echo "FAIL: rotated listener accepted an old-CA client" >&2
  exit 1
fi
code=$(curl -sS --max-time 5 --cert "$DIR/node-new.pem" --key "$DIR/node-new.key" \
  --cacert "$DIR/new-ca.pem" -o /dev/null -w '%{http_code}' \
  "https://localhost:$B_PEER/internal/v1/vms")
test "$code" = 401
python3 "$PROBE" "$TARGET_URL" "$DIR/node-new.pem" "$DIR/node-new.key" "$DIR/new-ca.pem" \
  "$SECRET" node-a "$NEW_ONLY_A_SESSION" node-b "$NEW_ONLY_B_SESSION" 404
A_TARGET_URL="https://localhost:$A_PEER/internal/v1/vms/$VM_ID/status"
python3 "$PROBE" "$A_TARGET_URL" "$DIR/node-b-new.pem" "$DIR/node-b-new.key" "$DIR/new-ca.pem" \
  "$SECRET" node-b "$NEW_ONLY_B_SESSION" node-a "$NEW_ONLY_A_SESSION" 404

echo "== rollback restores overlap before either old leaf =="
kill -TERM "$A_PID"
wait "$A_PID" || true
start_node node-a "$A_CONTROL" "$A_PEER" "$DIR/node-new.pem" "$DIR/node-new.key" "$DIR/overlap-ca.pem" "$DIR/node-a.log"
A_PID=$LAST_PID
wait_health "http://127.0.0.1:$A_CONTROL" "$A_PID"
ROLLBACK_OVERLAP_A_SESSION=$(session node-a)

kill -TERM "$B_PID"
wait "$B_PID" || true
start_node node-b "$B_CONTROL" "$B_PEER" "$DIR/node-b-new.pem" "$DIR/node-b-new.key" "$DIR/overlap-ca.pem" "$DIR/node-b.log"
B_PID=$LAST_PID
wait_health "http://127.0.0.1:$B_CONTROL" "$B_PID"
ROLLBACK_OVERLAP_B_SESSION=$(session node-b)
python3 "$PROBE" "$TARGET_URL" "$DIR/node-new.pem" "$DIR/node-new.key" "$DIR/new-ca.pem" \
  "$SECRET" node-a "$ROLLBACK_OVERLAP_A_SESSION" node-b "$ROLLBACK_OVERLAP_B_SESSION" 404

kill -TERM "$A_PID"
wait "$A_PID" || true
start_node node-a "$A_CONTROL" "$A_PEER" "$DIR/node-a.pem" "$DIR/node-a.key" "$DIR/overlap-ca.pem" "$DIR/node-a.log"
A_PID=$LAST_PID
wait_health "http://127.0.0.1:$A_CONTROL" "$A_PID"
ROLLBACK_A_SESSION=$(session node-a)

kill -TERM "$B_PID"
wait "$B_PID" || true
start_node node-b "$B_CONTROL" "$B_PEER" "$DIR/node-b.pem" "$DIR/node-b.key" "$DIR/overlap-ca.pem" "$DIR/node-b.log"
B_PID=$LAST_PID
wait_health "http://127.0.0.1:$B_CONTROL" "$B_PID"
ROLLBACK_B_SESSION=$(session node-b)
python3 "$PROBE" "$TARGET_URL" "$DIR/node-a.pem" "$DIR/node-a.key" "$DIR/old-ca.pem" \
  "$SECRET" node-a "$ROLLBACK_A_SESSION" node-b "$ROLLBACK_B_SESSION" 404

echo "== complete rollback removes the new CA =="
kill -TERM "$A_PID"
wait "$A_PID" || true
start_node node-a "$A_CONTROL" "$A_PEER" "$DIR/node-a.pem" "$DIR/node-a.key" "$DIR/old-ca.pem" "$DIR/node-a.log"
A_PID=$LAST_PID
wait_health "http://127.0.0.1:$A_CONTROL" "$A_PID"
OLD_ONLY_A_SESSION=$(session node-a)

kill -TERM "$B_PID"
wait "$B_PID" || true
start_node node-b "$B_CONTROL" "$B_PEER" "$DIR/node-b.pem" "$DIR/node-b.key" "$DIR/old-ca.pem" "$DIR/node-b.log"
B_PID=$LAST_PID
wait_health "http://127.0.0.1:$B_CONTROL" "$B_PID"
OLD_ONLY_B_SESSION=$(session node-b)
if curl -fsS --max-time 5 --cert "$DIR/node-new.pem" --key "$DIR/node-new.key" \
  --cacert "$DIR/old-ca.pem" "https://localhost:$B_PEER/internal/v1/vms" >/dev/null 2>&1; then
  echo "FAIL: rolled-back listener accepted a new-CA client" >&2
  exit 1
fi
python3 "$PROBE" "$TARGET_URL" "$DIR/node-a.pem" "$DIR/node-a.key" "$DIR/old-ca.pem" \
  "$SECRET" node-a "$OLD_ONLY_A_SESSION" node-b "$OLD_ONLY_B_SESSION" 404
python3 "$PROBE" "$A_TARGET_URL" "$DIR/node-b.pem" "$DIR/node-b.key" "$DIR/old-ca.pem" \
  "$SECRET" node-b "$OLD_ONLY_B_SESSION" node-a "$OLD_ONLY_A_SESSION" 404

echo "PASS: dedicated peer mTLS + Postgres boot-session fencing + CA rotation and rollback"
