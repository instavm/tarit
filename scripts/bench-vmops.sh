#!/usr/bin/env bash
# Benchmark single-VM operations against taritd; clean up resources from this run.
# Cleanup is best effort when the API is unavailable. An operation that finishes
# after a client timeout can require manual cleanup using the printed VM id.
# Snapshot reconciliation covers artifacts listed by this API node.
# Usage: API=http://127.0.0.1:8080 ./scripts/bench-vmops.sh
set -euo pipefail
API="${API:-http://127.0.0.1:8080}"
H="X-API-Key: ${TARIT_API_KEY:-test-key}"
CT="Content-Type: application/json"
MEM="${MEM:-512}"; VCPUS="${VCPUS:-1}"; ITERS="${ITERS:-30}"
[[ "$ITERS" =~ ^[1-9][0-9]*$ ]] || { echo "ITERS must be a positive integer" >&2; exit 1; }
TMP=$(mktemp -d)
VMS=(); SNAPSHOTS=()
SOURCE=""; SNAPSHOT_STARTED=0; CLEANED=0

pct() { sort -n | awk '{a[NR]=$1} END{ if(NR==0){print "no data";exit}
  printf "p50=%.2fms p95=%.2fms max=%.2fms n=%d\n", a[int((NR-1)*0.5)+1]*1000, a[int((NR-1)*0.95)+1]*1000, a[NR]*1000, NR}'; }

# Keep response bodies (including error bodies) separate from HTTP status/timing.
request() {
  local result
  HTTP=000
  result=$(curl -sS --connect-timeout 10 --max-time "${REQUEST_TIMEOUT:-180}" -o "$TMP/response" \
    -w '%{http_code} %{time_total}' -X "$1" "$API$2" -H "$H" -H "$CT" \
    --data "${3:-}") || return 1
  HTTP=${result%% *}; ELAPSED=${result#* }
}

expect_success() {
  request "$@" || return 1
  [[ "$HTTP" =~ ^2 ]] || { echo "$1 $2 failed with HTTP $HTTP" >&2; return 1; }
}

response_id() {
  python3 -c 'import json,sys,uuid; print(uuid.UUID(json.load(sys.stdin)[sys.argv[1]]))' "$1" < "$TMP/response"
}

forget_vm() {
  local i
  for i in "${!VMS[@]}"; do
    if [[ "${VMS[$i]}" == "$1" ]]; then unset 'VMS[i]'; fi
  done
}

forget_snapshot() {
  local i
  for i in "${!SNAPSHOTS[@]}"; do
    if [[ "${SNAPSHOTS[$i]}" == "$1" ]]; then unset 'SNAPSHOTS[i]'; fi
  done
}

remember_snapshot() {
  local id
  for id in "${SNAPSHOTS[@]-}"; do [[ "$id" == "$1" ]] && return 0; done
  SNAPSHOTS+=("$1")
}

remove_resource() {
  request DELETE "$1" || return 1
  [[ "$HTTP" =~ ^2 || "$HTTP" == 404 ]] || {
    echo "cleanup failed: DELETE $1 returned HTTP $HTTP" >&2; return 1;
  }
}

cleanup() {
  [[ "$CLEANED" == 0 ]] || return 0
  local failed=0 id REQUEST_TIMEOUT=10
  # A failed snapshot response may still have published an artifact. Only
  # reconcile snapshots of the source VM created by this invocation.
  if [[ "$SNAPSHOT_STARTED" == 1 && -n "$SOURCE" ]]; then
    if expect_success GET /v1/snapshots && python3 -c '
import json,sys,uuid
for item in json.load(sys.stdin):
    if item.get("source_vm_id") == sys.argv[1]:
        print(uuid.UUID(item["snapshot_id"]))
' "$SOURCE" < "$TMP/response" > "$TMP/snapshot-ids"; then
      while IFS= read -r id; do remember_snapshot "$id"; done < "$TMP/snapshot-ids"
    else
      echo "could not reconcile snapshots for benchmark VM $SOURCE" >&2
      failed=1
    fi
  fi
  # Delete restored VMs before the snapshots they pin. Never adopt an ID from
  # an error response or a conflicting create into our cleanup set.
  for id in "${VMS[@]-}"; do
    [[ -n "$id" ]] || continue
    remove_resource "/v1/vms/$id" || failed=1
  done
  for id in "${SNAPSHOTS[@]-}"; do
    [[ -n "$id" ]] || continue
    remove_resource "/v1/snapshots/$id" || failed=1
  done
  for id in "$TMP"/*; do [[ ! -e "$id" ]] || rm "$id"; done
  rmdir "$TMP"
  CLEANED=1
  return "$failed"
}
trap 'status=$?; trap - EXIT; cleanup || status=1; exit "$status"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

create_vm() {
  local endpoint=$1 body=$2 actual
  CREATED=$(python3 -c 'import uuid; print(uuid.uuid4())')
  # Confirm absence before considering an ambiguous response ours to clean up.
  request GET "/v1/vms/$CREATED"
  [[ "$HTTP" == 404 ]] || { echo "refusing to use occupied/unverified VM id $CREATED" >&2; return 1; }
  VMS+=("$CREATED")
  if ! request POST "$endpoint" "{\"id\":\"$CREATED\",$body}"; then
    echo "ambiguous create/restore for VM $CREATED; attempting cleanup" >&2
    return 1
  fi
  if [[ "$HTTP" =~ ^4 ]]; then forget_vm "$CREATED"; fi
  [[ "$HTTP" =~ ^2 ]] || { echo "POST $endpoint failed with HTTP $HTTP" >&2; return 1; }
  actual=$(response_id id)
  [[ "$actual" == "$CREATED" ]] || { echo "server returned an unexpected VM id" >&2; return 1; }
}

create_vm /v1/vms "\"memory_mib\":$MEM,\"vcpus\":$VCPUS"
SOURCE=$CREATED
echo "created vm=$SOURCE  (mem=${MEM}MiB vcpus=$VCPUS iters=$ITERS)"

# These loops run in the current shell so EXIT always sees newly created IDs.
for op in status pause resume; do
  : > "$TMP/timings"
  for ((i=0; i<ITERS; i++)); do
    case "$op" in
      status) expect_success GET "/v1/vms/$SOURCE/status" ;;
      pause) expect_success POST "/v1/vms/$SOURCE/pause" ;;
      resume)
        expect_success POST "/v1/vms/$SOURCE/pause"
        expect_success POST "/v1/vms/$SOURCE/resume" ;;
    esac
    printf '%s\n' "$ELAPSED" >> "$TMP/timings"
    if [[ "$op" == pause ]]; then expect_success POST "/v1/vms/$SOURCE/resume"; fi
  done
  printf '%-9s: ' "$op"; pct < "$TMP/timings"
done

SNAPSHOT_STARTED=1
expect_success POST "/v1/vms/$SOURCE/snapshot" '{"diff":false}'
SNAP=$(response_id snapshot_id)
remember_snapshot "$SNAP"
: > "$TMP/timings"
for ((i=0; i<ITERS; i++)); do
  expect_success POST "/v1/vms/$SOURCE/snapshot" '{"diff":false}'
  id=$(response_id snapshot_id)
  remember_snapshot "$id"
  printf '%s\n' "$ELAPSED" >> "$TMP/timings"
  remove_resource "/v1/snapshots/$id"
  forget_snapshot "$id"
done
printf 'snapshot : '; pct < "$TMP/timings"
echo "  snapshot_id=$SNAP"

: > "$TMP/timings"
for ((i=0; i<ITERS; i++)); do
  create_vm /v1/restore "\"snapshot_id\":\"$SNAP\""
  printf '%s\n' "$ELAPSED" >> "$TMP/timings"
  remove_resource "/v1/vms/$CREATED"
  forget_vm "$CREATED"
done
printf 'restore  : '; pct < "$TMP/timings"
cleanup
trap - EXIT
echo "done."
