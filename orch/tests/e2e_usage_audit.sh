#!/usr/bin/env bash
# tests/e2e_usage_audit.sh - validate that per-API-key usage stats and the audit
# trail reach the primary store (Postgres). Boots one cluster-mode taritd node
# against the taritd-cp RDS, exercises a VM lifecycle, and asserts:
#   - usage_events has vm_runtime rows (seconds > 0) and an exec row for the key
#   - audit_events has create/exec/delete/snapshot rows for the key
#   - GET /v1/usage and GET /v1/audit return the same, scoped to the key
# Run as root on c8i with the fleet env sourced (set -a; . ~/.taritd/cp-rds.env).
set -uo pipefail
ROOT="${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
TARIT="${TARIT:-$ROOT/target/release/taritd}"
VMM_ROOT="${VMM_ROOT:-$ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
: "${TARIT_DATABASE_URL:?source cp-rds.env first}"
CA="${TARIT_RDS_CA_FILE:-$TARITD_HOME/rds-global-bundle.pem}"
export PGSSLMODE=require PGSSLROOTCERT="$CA" PGCONNECT_TIMEOUT=6

KEY="usage-test-$(date +%s)"
KEY_ID="$(printf '%s' "$KEY" | sha256sum | cut -d' ' -f1)"
PORT=18090
DIR=/tmp/usage-$$
ROOTFS=/tmp/usage-rootfs.ext4
BASE=/tmp/vsock-rootfs.ext4
mkdir -p "$DIR/sockets"
PASS=1
note(){ printf '%s\n' "$*"; }
fail(){ printf 'FAIL %s\n' "$*"; PASS=0; }

# fresh agent-baked rootfs
cp -f "$BASE" "$ROOTFS"
make -C "$VMM_ROOT/guest/agent" >/dev/null 2>&1 || true
sh "$VMM_ROOT/guest/agent/bake-agent.sh" "$ROOTFS" "$VMM_ROOT/guest/agent/vmm-agent" >/dev/null 2>&1 || true
e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true

for p in $(pgrep -f 'target/release/taritd' 2>/dev/null); do kill "$p" 2>/dev/null; done; sleep 2
# clean this key's rows from prior runs
psql "$TARIT_DATABASE_URL" -qAtc "delete from usage_events where api_key_id='$KEY_ID'; delete from audit_events where api_key_id='$KEY_ID';" >/dev/null 2>&1 || true

TARIT_API_KEY="$KEY" TARIT_PEER_SECRET="usage-peer-secret-long-enough-000" \
TARIT_DATABASE_URL="$TARIT_DATABASE_URL" TARIT_RDS_CA_FILE="$CA" \
TARIT_LISTEN="127.0.0.1:$PORT" TARIT_RPC_ADDR="http://127.0.0.1:$PORT" TARIT_HOST_ID="usage-n1" \
TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
TARIT_SOCKET_DIR="$DIR/sockets" TARIT_DB="$DIR/i.sqlite" TARIT_CONFIG="$DIR/none.toml" \
TARIT_VMM_BIN="$VMM_ROOT/target/debug/vmm" TARIT_KERNEL=/tmp/vmlinux.microvm TARIT_ROOTFS="$ROOTFS" \
TARIT_ROOTFS_READONLY=1 TARIT_WARM_POOL=0 TARIT_MAX_VMS=4 \
TARIT_USAGE_METER_INTERVAL_SECS=3 TARIT_USAGE_FLUSH_INTERVAL_SECS=2 \
RUST_LOG=taritd=info "$TARIT" serve >"$DIR/log" 2>&1 &
PID=$!
cleanup(){ kill "$PID" 2>/dev/null; sleep 1; kill -9 "$PID" 2>/dev/null
  for p in $(pgrep -f 'vmm serve' 2>/dev/null); do kill "$p" 2>/dev/null; done
  psql "$TARIT_DATABASE_URL" -qAtc "delete from usage_events where api_key_id='$KEY_ID'; delete from audit_events where api_key_id='$KEY_ID'; delete from fleet_hosts where host_id='usage-n1'; delete from fleet_vms where api_key_id='$KEY_ID';" >/dev/null 2>&1 || true
  rm -rf "$DIR" "$ROOTFS"; }
trap cleanup EXIT

api(){ curl -sS --max-time 30 -H "X-API-Key: $KEY" "$@"; }
for _ in $(seq 1 30); do curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break; sleep 1; done
note "node up; api_key_id=$KEY_ID"

VM=$(api -H 'content-type: application/json' -d '{"vcpus":1,"memory_mib":256}' "http://127.0.0.1:$PORT/v1/vms" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
note "created vm=$VM; letting it run ~9s for runtime metering"
sleep 9
api -H 'content-type: application/json' -d "{\"vm_id\":\"$VM\",\"command\":\"echo usage-ok\",\"timeout_ms\":5000}" "http://127.0.0.1:$PORT/v1/execute" >/dev/null
api -H 'content-type: application/json' -d '{"diff":false}' "http://127.0.0.1:$PORT/v1/vms/$VM/snapshot" >/dev/null
sleep 5   # let the flusher push to Postgres

RUNTIME=$(psql "$TARIT_DATABASE_URL" -qAtc "select coalesce(round(sum(seconds)::numeric,1),0) from usage_events where api_key_id='$KEY_ID' and kind='vm_runtime'")
EXECN=$(psql "$TARIT_DATABASE_URL" -qAtc "select count(*) from usage_events where api_key_id='$KEY_ID' and kind='exec'")
note "usage_events: vm_runtime_seconds=$RUNTIME exec_rows=$EXECN"
awk "BEGIN{exit !($RUNTIME+0 >= 5)}" || fail "expected >=5 vm_runtime seconds, got $RUNTIME"
[ "${EXECN:-0}" -ge 1 ] || fail "expected an exec usage row, got $EXECN"

for act in create exec snapshot; do
  n=$(psql "$TARIT_DATABASE_URL" -qAtc "select count(*) from audit_events where api_key_id='$KEY_ID' and action='$act'")
  [ "${n:-0}" -ge 1 ] && note "audit $act: $n" || fail "expected audit '$act' row"
done

USAGE_API=$(api "http://127.0.0.1:$PORT/v1/usage")
note "GET /v1/usage: $USAGE_API"
echo "$USAGE_API" | python3 -c 'import sys,json;d=json.load(sys.stdin);
assert any(r["vm_runtime_seconds"]>=5 for r in d), d' 2>/dev/null || fail "/v1/usage did not report runtime seconds"
AUDIT_API=$(api "http://127.0.0.1:$PORT/v1/audit?limit=50")
echo "$AUDIT_API" | python3 -c 'import sys,json;d=json.load(sys.stdin);
acts={r["action"] for r in d};
assert {"create","exec","snapshot"} <= acts, acts' 2>/dev/null || fail "/v1/audit missing actions"

api -X DELETE "http://127.0.0.1:$PORT/v1/vms/$VM" >/dev/null; sleep 4
DEL=$(psql "$TARIT_DATABASE_URL" -qAtc "select count(*) from audit_events where api_key_id='$KEY_ID' and action='delete'")
[ "${DEL:-0}" -ge 1 ] && note "audit delete: $DEL" || fail "expected audit 'delete' row"

echo ""
[ "$PASS" = 1 ] && { echo "RESULT: USAGE_AUDIT_PASS"; exit 0; } || { echo "RESULT: USAGE_AUDIT_FAIL"; echo "--- log tail ---"; tail -15 "$DIR/log"; exit 1; }
