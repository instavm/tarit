#!/usr/bin/env bash
# Quick 3-node bringup diagnostic against the RDS. Starts 3 taritd nodes,
# waits, prints each node's /health and the tail of its log, then tears down.
set -uo pipefail
ROOT="${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
TARIT="${TARIT:-$ROOT/target/release/taritd}"
VMM_BIN="${TARIT_VMM_BIN:-$ROOT/../vmm/target/debug/vmm}"
: "${TARIT_DATABASE_URL:?source cp-rds.env first}"
RID="diag-$$"
DIR=/tmp/ha-diag-$$
API=diagkey; PEER=diagpeersecretlongenough123456
H=("$RID-n1" "$RID-n2" "$RID-n3"); P=(19090 19091 19092); PIDS=()
mkdir -p "$DIR"
start(){ local i=$1 nd="$DIR/n$i"; mkdir -p "$nd/sockets"
  ( export TARIT_API_KEY=$API TARIT_PEER_SECRET=$PEER \
      TARIT_DATABASE_URL="$TARIT_DATABASE_URL" TARIT_RDS_CA_FILE="${TARIT_RDS_CA_FILE:-}" \
      TARIT_LISTEN=127.0.0.1:${P[$i]} TARIT_RPC_ADDR=http://127.0.0.1:${P[$i]} \
      TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
      TARIT_HOST_ID=${H[$i]} TARIT_SOCKET_DIR="$nd/sockets" TARIT_DB="$nd/i.sqlite" \
      TARIT_VMM_BIN="$VMM_BIN" TARIT_KERNEL=/tmp/vmlinux.microvm \
      TARIT_ROOTFS=/tmp/vsock-rootfs.ext4 TARIT_CONFIG="$nd/none.toml" TARIT_WARM_POOL=0 \
      TARIT_MAX_VMS=2 TARIT_AUTOSCALE=1 RUST_LOG=taritd=debug,tarit_fleet=debug
    exec "$TARIT" >"$nd/log" 2>&1 ) &
  PIDS[$i]=$!; }
for i in 0 1 2; do start $i; done
sleep 14
for i in 0 1 2; do
  echo "=== node $i (${H[$i]}) port ${P[$i]} pid=${PIDS[$i]} alive=$(kill -0 ${PIDS[$i]} 2>/dev/null && echo yes || echo no) ==="
  echo -n "  /health: "; curl -sS --max-time 4 http://127.0.0.1:${P[$i]}/health 2>&1 | head -c 200; echo
  echo "  log tail:"; tail -6 "$DIR/n$i/log" 2>/dev/null | sed 's/^/    /'
done
echo "=== fleet_hosts rows for this run ==="
psql "$TARIT_DATABASE_URL" -qAtc "select host_id, healthy, last_heartbeat from fleet_hosts where host_id like '$RID-%' order by host_id" 2>&1 | sed 's/^/  /'
echo "=== /v1/cluster from node 0 (X-API-Key: $API) ==="
curl -sS --max-time 6 -H "X-API-Key: $API" http://127.0.0.1:${P[0]}/v1/cluster 2>&1 | python3 -m json.tool 2>&1 | head -40
for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done; sleep 1; for p in "${PIDS[@]}"; do kill -9 "$p" 2>/dev/null; done
psql "$TARIT_DATABASE_URL" -qAtc "delete from fleet_hosts where host_id like '$RID-%'; delete from fleet_leader where leader_id like '$RID-%';" >/dev/null 2>&1 || true
rm -rf "$DIR"
