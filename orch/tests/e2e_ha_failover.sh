#!/usr/bin/env bash
# tests/e2e_ha_failover.sh - HA/durability failover test for a 3-node taritd
# cluster backed by a shared Postgres (RDS). Validates:
#   1. cluster formation (3 healthy nodes agree via the fleet store)
#   2. leader election (one node holds the fleet_leader lease)
#   3. node-down detection (kill a node -> healthy_nodes drops)
#   4. leader failover (kill the leader -> the lease moves to a survivor)
#   5. rejoin (restart the node -> healthy_nodes recovers)
# No KVM VMs are created; this exercises the coordination plane only.
#
# Requires: TARIT_DATABASE_URL + TARIT_RDS_CA_FILE (source cp-rds.env first),
# release taritd built, psql present. Run as the login user on c8i.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
TARIT="${TARIT:-$ROOT/target/release/taritd}"
BASE_PORT="${BASE_PORT:-19080}"
RUN_ID="ha-$(date +%H%M%S)-$$"
API_KEY="ha-api-$(python3 -c 'import secrets;print(secrets.token_hex(16))')"
PEER_SECRET="ha-peer-$(python3 -c 'import secrets;print(secrets.token_hex(24))')"
RUN_DIR="$ROOT/e/$RUN_ID"
: "${TARIT_DATABASE_URL:?source cp-rds.env first}"
export PGSSLMODE=require PGSSLROOTCERT="${TARIT_RDS_CA_FILE:-}" PGCONNECT_TIMEOUT=6
HOSTS=("$RUN_ID-n1" "$RUN_ID-n2" "$RUN_ID-n3")
PORTS=("$BASE_PORT" "$((BASE_PORT+1))" "$((BASE_PORT+2))")
PIDS=(); PASS=0; FAIL=0

log(){ printf '%s\n' "$*"; }
ok(){ printf 'PASS %s\n' "$*"; PASS=$((PASS+1)); }
bad(){ printf 'FAIL %s\n' "$*"; FAIL=$((FAIL+1)); }
cluster_json(){ curl -sS --max-time 8 -H "X-API-Key: $API_KEY" "http://127.0.0.1:${PORTS[$1]}/v1/cluster" 2>/dev/null; }
healthy_count(){ cluster_json "$1" | python3 -c 'import sys,json;
try: print(json.load(sys.stdin).get("healthy_nodes",-1))
except Exception: print(-1)'; }
leader_id(){ psql "$TARIT_DATABASE_URL" -qAtc "select leader_id from fleet_leader where id=1 and expires_at>now()" 2>/dev/null | tr -d '[:space:]'; }

start_node(){
  local i="$1" nd="$RUN_DIR/node$((i+1))"; mkdir -p "$nd/sockets"
  ( export TARIT_API_KEY="$API_KEY" TARIT_PEER_SECRET="$PEER_SECRET" \
      TARIT_DATABASE_URL="$TARIT_DATABASE_URL" TARIT_RDS_CA_FILE="${TARIT_RDS_CA_FILE:-}" \
      TARIT_LISTEN="127.0.0.1:${PORTS[$i]}" TARIT_RPC_ADDR="http://127.0.0.1:${PORTS[$i]}" \
      TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
      TARIT_HOST_ID="${HOSTS[$i]}" TARIT_SOCKET_DIR="$nd/sockets" TARIT_DB="$nd/taritd.sqlite" \
      TARIT_VMM_BIN="${TARIT_VMM_BIN:-$ROOT/../vmm/target/debug/vmm}" \
      TARIT_KERNEL="${TARIT_KERNEL:-/tmp/vmlinux.microvm}" TARIT_ROOTFS="${TARIT_ROOTFS:-/tmp/vsock-rootfs.ext4}" \
      TARIT_CONFIG="$nd/none.toml" TARIT_WARM_POOL=0 TARIT_MAX_VMS=2 \
      TARIT_AUTOSCALE=1 TARIT_AUTOSCALE_MIN=1 TARIT_AUTOSCALE_MAX=100 \
      RUST_LOG="taritd=info,tower_http=warn"
    exec "$TARIT" >"$nd/taritd.log" 2>&1 ) &
  PIDS[$i]=$!
}

pg_cleanup(){ psql "$TARIT_DATABASE_URL" -qAtc "delete from fleet_vms where host_id like '$RUN_ID-%'; delete from fleet_hosts where host_id like '$RUN_ID-%'; delete from fleet_leader where leader_id like '$RUN_ID-%';" >/dev/null 2>&1 || true; }
cleanup(){ for i in 1 2 3; do cp "$RUN_DIR/node$i/taritd.log" "/tmp/ha-node$i.log" 2>/dev/null; done; for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill "$p" 2>/dev/null; done; sleep 1; for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null; done; pg_cleanup; rm -rf "$RUN_DIR"; }
trap cleanup EXIT

wait_healthy(){ # node_idx, want_count, timeout
  local idx="$1" want="$2" dl=$((SECONDS+${3:-40}))
  while (( SECONDS < dl )); do [ "$(healthy_count "$idx")" = "$want" ] && return 0; sleep 2; done; return 1; }

log "== HA failover run_id=$RUN_ID base_port=$BASE_PORT =="
# Clear any stray taritd processes that could occupy our ports.
for p in $(pgrep -f 'target/release/taritd' 2>/dev/null); do kill "$p" 2>/dev/null; done
sleep 2
mkdir -p "$RUN_DIR"; pg_cleanup
[ -x "$TARIT" ] || { echo "missing $TARIT"; exit 1; }
for i in 0 1 2; do start_node "$i"; done

# 1) cluster formation
if wait_healthy 0 3 60; then ok "cluster formed: 3 healthy nodes"; else bad "cluster never reached 3 healthy (got $(healthy_count 0))"; fi

# 2) leader election (autoscaler lease)
LEADER=""; dl=$((SECONDS+40))
while (( SECONDS < dl )); do L="$(leader_id)"; case " ${HOSTS[*]} " in *" $L "*) LEADER="$L"; break;; esac; sleep 2; done
if [ -n "$LEADER" ]; then ok "leader elected: $LEADER"; else bad "no leader in fleet_leader within 40s"; fi

# find the leader's node index
LIDX=-1; for i in 0 1 2; do [ "${HOSTS[$i]}" = "$LEADER" ] && LIDX=$i; done
SURV=(); for i in 0 1 2; do [ "$i" != "$LIDX" ] && SURV+=("$i"); done

# 3+4) kill the leader -> node-down detection + leader failover
if [ "$LIDX" -ge 0 ]; then
  log "killing leader node $LIDX (${HOSTS[$LIDX]}) pid=${PIDS[$LIDX]}"
  kill "${PIDS[$LIDX]}" 2>/dev/null; PIDS[$LIDX]=""
  if wait_healthy "${SURV[0]}" 2 40; then ok "node-down detected: healthy_nodes dropped to 2"; else bad "healthy_nodes did not drop to 2 (got $(healthy_count "${SURV[0]}"))"; fi
  NEW=""; dl=$((SECONDS+60))
  while (( SECONDS < dl )); do L="$(leader_id)"; if [ -n "$L" ] && [ "$L" != "$LEADER" ]; then case " ${HOSTS[${SURV[0]}]} ${HOSTS[${SURV[1]}]} " in *" $L "*) NEW="$L"; break;; esac; fi; sleep 3; done
  if [ -n "$NEW" ]; then ok "leader failover: lease moved $LEADER -> $NEW"; else bad "leader lease did not move to a survivor within 60s (still '$(leader_id)')"; fi
else
  bad "could not identify leader node index (skipping kill)"
fi

# 5) rejoin
if [ "$LIDX" -ge 0 ]; then
  log "restarting node $LIDX"; start_node "$LIDX"
  if wait_healthy "${SURV[0]}" 3 60; then ok "rejoin: healthy_nodes recovered to 3"; else bad "healthy_nodes did not recover to 3 (got $(healthy_count "${SURV[0]}"))"; fi
fi

log ""; log "SUMMARY: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] && { log "RESULT: HA_FAILOVER_PASS"; exit 0; } || { log "RESULT: HA_FAILOVER_FAIL"; exit 1; }
