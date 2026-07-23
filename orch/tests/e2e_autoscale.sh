#!/usr/bin/env bash
# tests/e2e_autoscale.sh - Autoscaler actuation. Runs a single cluster node with
# the autoscaler enabled and a scale-out threshold high enough that the cluster
# always reads as low on capacity, then asserts the leader invokes the provider
# command with a scale_out decision. Proves the decision -> provider actuation
# path end to end without any cloud API. Needs the fleet DB env sourced.
set -uo pipefail
ROOT="${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
TARIT="${TARIT:-$ROOT/target/release/taritd}"
VMM_BIN="${TARIT_VMM_BIN:-$ROOT/../vmm/target/debug/vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
CA="${TARIT_RDS_CA_FILE:-$TARITD_HOME/rds-global-bundle.pem}"
: "${TARIT_DATABASE_URL:?source cp-rds.env first}"
export PGSSLMODE=require PGSSLROOTCERT="$CA" PGCONNECT_TIMEOUT=6
RID="as-$(date +%H%M%S)"
DIR=/tmp/autoscale-$$
DECISIONS="$DIR/decisions.log"
mkdir -p "$DIR/sockets"
# The provider_cmd is a shell snippet run as: sh -c "<cmd>" provider "<json>".
# So the decision JSON is $1 of the snippet; append it to the capture file.
PROVIDER_CMD="printf '%s\n' \"\$1\" >> $DECISIONS"
: > "$DECISIONS"

for p in $(pgrep -f 'target/release/taritd' 2>/dev/null); do kill "$p" 2>/dev/null; done; sleep 2
( export TARIT_API_KEY=askey TARIT_PEER_SECRET=autoscale-peer-secret-long-enough-000 \
    TARIT_DATABASE_URL="$TARIT_DATABASE_URL" TARIT_RDS_CA_FILE="$CA" \
    TARIT_LISTEN=127.0.0.1:19050 TARIT_RPC_ADDR=http://127.0.0.1:19050 TARIT_HOST_ID=$RID-n1 \
    TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
    TARIT_SOCKET_DIR="$DIR/sockets" TARIT_DB="$DIR/i.sqlite" TARIT_CONFIG="$DIR/none.toml" \
    TARIT_VMM_BIN="$VMM_BIN" TARIT_KERNEL=/tmp/vmlinux.microvm \
    TARIT_ROOTFS=/tmp/vsock-rootfs.ext4 TARIT_WARM_POOL=0 TARIT_MAX_VMS=2 \
    TARIT_AUTOSCALE=1 TARIT_AUTOSCALE_MIN=1 TARIT_AUTOSCALE_MAX=10 \
    TARIT_AUTOSCALE_OUT_FREE_VCPUS=1000000 TARIT_AUTOSCALE_PROVIDER_CMD="$PROVIDER_CMD" \
    RUST_LOG=taritd=info
  exec "$TARIT" >"$DIR/log" 2>&1 ) &
PID=$!
cleanup(){ kill "$PID" 2>/dev/null; sleep 1; kill -9 "$PID" 2>/dev/null
  psql "$TARIT_DATABASE_URL" -qAtc "delete from fleet_hosts where host_id like '$RID-%'; delete from fleet_leader where leader_id like '$RID-%';" >/dev/null 2>&1 || true
  rm -rf "$DIR"; }
trap cleanup EXIT

for _ in $(seq 1 20); do
  curl -sf -H "X-API-Key: askey" http://127.0.0.1:19050/health >/dev/null 2>&1 && break; sleep 1
done
echo "node up, waiting for autoscaler tick + actuation (up to 25s)"
for _ in $(seq 1 25); do [ -s "$DECISIONS" ] && break; sleep 1; done

echo "=== captured provider decisions ==="; cat "$DECISIONS" 2>/dev/null
if [ -s "$DECISIONS" ] && grep -q '"action":"scale_out"' "$DECISIONS" \
   && grep -q '"target_nodes":' "$DECISIONS" && grep -q '"cloud":"onprem"' "$DECISIONS"; then
  echo "RESULT: AUTOSCALE_PASS (leader invoked provider with a scale_out decision)"; exit 0
else
  echo "--- taritd log tail ---"; tail -8 "$DIR/log" 2>/dev/null
  echo "RESULT: AUTOSCALE_FAIL"; exit 1
fi
