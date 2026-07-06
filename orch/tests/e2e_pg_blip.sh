#!/usr/bin/env bash
# tests/e2e_pg_blip.sh - Postgres outage/partition durability (self-contained,
# no cloud API needed). Runs a 2-node taritd cluster against the RDS, then
# severs the DB with an nft drop rule to simulate a Postgres failover/outage.
# Asserts the nodes never crash during the outage and that cluster health
# recovers once the DB is reachable again. Run as root (nft needs CAP_NET_ADMIN).
set -uo pipefail
ROOT="${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
TARIT="${TARIT:-$ROOT/target/release/taritd}"
VMM_BIN="${TARIT_VMM_BIN:-$ROOT/../vmm/target/debug/vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
: "${TARIT_DATABASE_URL:?source cp-rds.env first}"
CA="${TARIT_RDS_CA_FILE:-$TARITD_HOME/rds-global-bundle.pem}"
export PGSSLMODE=require PGSSLROOTCERT="$CA" PGCONNECT_TIMEOUT=6
OUTAGE="${OUTAGE:-45}"; RECOVER="${RECOVER:-75}"
RID="blip-$(date +%H%M%S)"
DIR=/tmp/pgblip-$$
API=blipkey; PEER=blip-peer-secret-long-enough-000
H=("$RID-n1" "$RID-n2"); P=(19070 19071); PIDS=()
NFT_T=blip_partition
mkdir -p "$DIR"
RDS_HOST="$(printf '%s' "$TARIT_DATABASE_URL" | sed -E 's#.*@([^:/]+).*#\1#')"
RDS_IP="$(getent hosts "$RDS_HOST" | awk '{print $1; exit}')"

start(){ local i=$1 nd="$DIR/n$i"; mkdir -p "$nd/sockets"
  ( export TARIT_API_KEY=$API TARIT_PEER_SECRET=$PEER \
      TARIT_DATABASE_URL="$TARIT_DATABASE_URL" TARIT_RDS_CA_FILE="$CA" \
      TARIT_LISTEN=127.0.0.1:${P[$i]} TARIT_RPC_ADDR=http://127.0.0.1:${P[$i]} \
      TARIT_HOST_ID=${H[$i]} TARIT_SOCKET_DIR="$nd/sockets" TARIT_DB="$nd/i.sqlite" \
      TARIT_VMM_BIN="$VMM_BIN" TARIT_KERNEL=/tmp/vmlinux.microvm \
      TARIT_ROOTFS=/tmp/vsock-rootfs.ext4 TARIT_CONFIG="$nd/none.toml" TARIT_WARM_POOL=0 \
      TARIT_MAX_VMS=2 RUST_LOG=taritd=info
    exec "$TARIT" >"$nd/log" 2>&1 ) &
  PIDS[$i]=$!; }
unblock(){ nft delete table ip $NFT_T 2>/dev/null || true; }
cleanup(){ unblock; for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill "$p" 2>/dev/null; done; sleep 1
  for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null; done
  psql "$TARIT_DATABASE_URL" -qAtc "delete from fleet_hosts where host_id like '$RID-%'; delete from fleet_leader where leader_id like '$RID-%';" >/dev/null 2>&1 || true
  rm -rf "$DIR"; }
trap cleanup EXIT
alive(){ local n=0; for p in "${PIDS[@]}"; do kill -0 "$p" 2>/dev/null && n=$((n+1)); done; echo $n; }
health(){ curl -sS --max-time 5 -H "X-API-Key: $API" "http://127.0.0.1:${P[0]}/v1/cluster" 2>/dev/null \
  | python3 -c 'import sys,json
try: print(json.load(sys.stdin).get("healthy_nodes",-1))
except Exception: print("ERR")'; }

echo "RDS host=$RDS_HOST ip=${RDS_IP:-<unresolved>}"
[ -n "$RDS_IP" ] || { echo "RESULT: PG_BLIP_FAIL (could not resolve RDS ip)"; exit 1; }
for p in $(pgrep -f 'target/release/taritd' 2>/dev/null); do kill "$p" 2>/dev/null; done; sleep 2
for i in 0 1; do start $i; done
for _ in $(seq 1 30); do [ "$(health)" = "2" ] && break; sleep 1; done
BASE="$(health)"
echo "$(date +%H:%M:%S) baseline healthy=$BASE procs_alive=$(alive)"
[ "$BASE" = "2" ] || { echo "RESULT: PG_BLIP_FAIL (cluster did not form)"; exit 1; }

echo "$(date +%H:%M:%S) >>> severing Postgres ($RDS_IP:5432) for ${OUTAGE}s"
nft add table ip $NFT_T
nft add chain ip $NFT_T out '{ type filter hook output priority 0 ; policy accept ; }'
nft add rule ip $NFT_T out ip daddr $RDS_IP tcp dport 5432 drop

CRASHED=0; end=$((SECONDS+OUTAGE))
while (( SECONDS < end )); do a="$(alive)"; [ "$a" != "2" ] && CRASHED=1
  echo "$(date +%H:%M:%S) [outage] health=$(health) procs_alive=$a"; sleep 4; done

echo "$(date +%H:%M:%S) >>> restoring Postgres connectivity"
unblock
REC=0; end=$((SECONDS+RECOVER))
while (( SECONDS < end )); do h="$(health)"; a="$(alive)"; [ "$a" != "2" ] && CRASHED=1
  echo "$(date +%H:%M:%S) [recover] health=$h procs_alive=$a"
  [ "$h" = "2" ] && { REC=1; break; }; sleep 4; done

echo "SUMMARY: crashed_during_outage=$CRASHED recovered=$REC final_health=$(health) final_procs=$(alive)"
if [ "$CRASHED" = "0" ] && [ "$REC" = "1" ]; then echo "RESULT: PG_BLIP_PASS (survived DB outage, recovered)"; exit 0
else echo "RESULT: PG_BLIP_FAIL"; exit 1; fi
