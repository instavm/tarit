#!/usr/bin/env bash
#
# bench-vmops.sh -- single-VM operation latency via the live taritd REST API:
# status, pause (suspend), resume, snapshot (full), restore. These are KVM-exit
# and memory-op bound, so they are markedly faster on bare metal than nested.
#
# Start taritd first (e.g. via scripts/bench-warmpool.sh, or your own launch),
# then run this against it. Reference numbers are in BENCHMARK-RESULTS.md.
#
# Usage:  API=http://127.0.0.1:8080 ./scripts/bench-vmops.sh
set -euo pipefail
API="${API:-http://127.0.0.1:8080}"; H="X-API-Key: ${TARIT_API_KEY:-test-key}"; CT="Content-Type: application/json"
MEM="${MEM:-512}"; VCPUS="${VCPUS:-1}"; ITERS="${ITERS:-30}"

pct() { sort -n | awk '{a[NR]=$1} END{ if(NR==0){print "no data";exit}
  printf "p50=%.2fms p95=%.2fms max=%.2fms n=%d\n", a[int((NR-1)*0.5)+1]*1000, a[int((NR-1)*0.95)+1]*1000, a[NR]*1000, NR}'; }

VM=$(curl -fsS -X POST "$API/v1/vms" -H "$H" -H "$CT" -d "{\"memory_mib\":$MEM,\"vcpus\":$VCPUS}" \
     | python3 -c 'import sys,json,uuid; print(uuid.UUID(json.load(sys.stdin)["id"]))')
[ -z "$VM" ] && { echo "create failed (is taritd up at $API?)"; exit 1; }
echo "created vm=$VM  (mem=${MEM}MiB vcpus=$VCPUS iters=$ITERS)"

echo -n "status   : "; for _ in $(seq 1 $ITERS); do curl -fsS -o /dev/null -w "%{time_total}\n" "$API/v1/vms/$VM/status" -H "$H"; done | pct
echo -n "pause    : "; for _ in $(seq 1 $ITERS); do curl -fsS -o /dev/null -w "%{time_total}\n" -X POST "$API/v1/vms/$VM/pause" -H "$H"; curl -fsS -o /dev/null -X POST "$API/v1/vms/$VM/resume" -H "$H"; done | pct
echo -n "resume   : "; for _ in $(seq 1 $ITERS); do curl -fsS -o /dev/null -X POST "$API/v1/vms/$VM/pause" -H "$H"; curl -fsS -o /dev/null -w "%{time_total}\n" -X POST "$API/v1/vms/$VM/resume" -H "$H"; done | pct

RESP=$(curl -fsS -X POST "$API/v1/vms/$VM/snapshot" -H "$H" -H "$CT" -d '{"diff":false}')
SNAP=$(printf '%s' "$RESP" | python3 -c 'import sys,json,uuid; print(uuid.UUID(json.load(sys.stdin)["snapshot_id"]))')
echo -n "snapshot : "; for _ in $(seq 1 $ITERS); do curl -fsS -o /dev/null -w "%{time_total}\n" -X POST "$API/v1/vms/$VM/snapshot" -H "$H" -H "$CT" -d '{"diff":false}'; done | pct
echo "  snapshot_id=$SNAP"

echo -n "restore  : "; for _ in $(seq 1 $ITERS); do curl -fsS -o /dev/null -w "%{time_total}\n" -X POST "$API/v1/restore" -H "$H" -H "$CT" -d "{\"snapshot_id\":\"$SNAP\"}"; done | pct
echo "done."
