#!/usr/bin/env bash
# Comprehensive 3-node taritd cluster end-to-end test. Intended to run on the
# Linux/KVM c8i host from the repository root after `cargo build --release -p taritd`.
set -Eeuo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

CURL_CONNECT_TIMEOUT="${CURL_CONNECT_TIMEOUT:-2}"
CURL_MAX_TIME="${CURL_MAX_TIME:-35}"
API_KEY="${TARIT_API_KEY:-$(python3 - <<'PY'
import secrets
print('e2e-api-' + secrets.token_hex(24))
PY
)}"
PEER_SECRET="${TARIT_PEER_SECRET:-$(python3 - <<'PY'
import secrets
print('e2e-peer-' + secrets.token_hex(32))
PY
)}"
DATABASE_URL="${TARIT_DATABASE_URL:-postgresql://taritd:taritd@127.0.0.1:5432/taritd?sslmode=disable}"
VMM_BIN="${TARIT_VMM_BIN:-$ROOT/../vmm/target/release/vmm}"
KERNEL="${TARIT_KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${TARIT_ROOTFS:-/tmp/vsock-rootfs.ext4}"
BASE_PORT="${TARIT_E2E_BASE_PORT:-18080}"
MAX_VMS="${TARIT_MAX_VMS:-2}"
ADMISSION_TIMEOUT_MS="${TARIT_ADMISSION_TIMEOUT_MS:-750}"
RUN_ID="e2e-$(date +%H%M%S)-$$"
# Keep this path short: Unix socket paths are limited to ~108 bytes.
RUN_DIR="$ROOT/e/$RUN_ID"
REPORT_ROWS=()
PASS_COUNT=0
FAIL_COUNT=0
PIDS=()
CREATED_VMS=()
HOST_IDS=("$RUN_ID-n1" "$RUN_ID-n2" "$RUN_ID-n3")
URLS=("http://127.0.0.1:${BASE_PORT}" "http://127.0.0.1:$((BASE_PORT+1))" "http://127.0.0.1:$((BASE_PORT+2))")
LAST_CODE=""
LAST_BODY=""
CREATED_ID=""
CREATED_BODY=""

log() { printf '%s\n' "$*"; }

json_get() {
  local body="$1" expr="$2"
  JSON_BODY="$body" JSON_EXPR="$expr" python3 - <<'PY'
import json, os, sys
body = os.environ.get('JSON_BODY', '')
expr = os.environ['JSON_EXPR']
data = json.loads(body)
try:
    safe = {"__builtins__": {}, "data": data, "len": len, "any": any, "all": all, "str": str, "int": int, "bool": bool}
    value = eval(expr, safe, {})
except Exception as e:
    raise SystemExit(f"json expr failed: {expr}: {e}\nbody={body}")
if isinstance(value, bool):
    print('true' if value else 'false')
elif value is None:
    print('')
elif isinstance(value, (dict, list)):
    print(json.dumps(value, separators=(',', ':')))
else:
    print(value)
PY
}

json_assert() {
  local body="$1" expr="$2"
  [[ "$(json_get "$body" "$expr")" == "true" ]]
}

request() {
  local method="$1" url="$2" body="${3-}" auth="${4-api}" max_time="${5:-$CURL_MAX_TIME}"
  local args=(-sS --connect-timeout "$CURL_CONNECT_TIMEOUT" --max-time "$max_time" -X "$method" -H 'Content-Type: application/json')
  [[ "$auth" == "api" ]] && args+=(-H "X-API-Key: $API_KEY")
  [[ "$auth" == "peer" ]] && args+=(-H "X-Peer-Secret: $PEER_SECRET")
  if [[ -n "$body" ]]; then
    args+=(-d "$body")
  fi
  local resp code marker=$'\n__HTTP_STATUS__:'
  set +e
  resp=$(curl "${args[@]}" -w "${marker}%{http_code}" "$url" 2>&1)
  local rc=$?
  set -e
  if [[ $rc -ne 0 ]]; then
    LAST_CODE="000"
    LAST_BODY="$resp"
    return 0
  fi
  code="${resp##*__HTTP_STATUS__:}"
  LAST_CODE="$code"
  LAST_BODY="${resp%$marker$code}"
}

api() { request "$2" "${URLS[$1]}$3" "${4-}" api "${5:-$CURL_MAX_TIME}"; }
noauth() { request "$2" "${URLS[$1]}$3" "${4-}" none "${5:-$CURL_MAX_TIME}"; }
peer() { request "$2" "${URLS[$1]}$3" "${4-}" peer "${5:-$CURL_MAX_TIME}"; }

expect_code() {
  local expected="$1"
  [[ "$LAST_CODE" == "$expected" ]] || { echo "expected HTTP $expected, got $LAST_CODE body=$LAST_BODY" >&2; return 1; }
}

expect_code_in() {
  local allowed=" $1 "
  [[ "$allowed" == *" $LAST_CODE "* ]] || { echo "expected HTTP in [$1], got $LAST_CODE body=$LAST_BODY" >&2; return 1; }
}

uuid() { python3 - <<'PY'
import uuid
print(uuid.uuid4())
PY
}

record_vm() {
  local id="$1"
  CREATED_VMS+=("$id")
}

node_index_for_host() {
  local host="$1"
  for i in 0 1 2; do
    if [[ "${HOST_IDS[$i]}" == "$host" ]]; then
      echo "$i"
      return 0
    fi
  done
  return 1
}

other_node_index() {
  local host="$1"
  for i in 0 1 2; do
    if [[ "${HOST_IDS[$i]}" != "$host" ]]; then
      echo "$i"
      return 0
    fi
  done
}

wait_for_http() {
  local idx="$1" deadline=$((SECONDS + 45))
  while (( SECONDS < deadline )); do
    noauth "$idx" GET /health '' 3
    [[ "$LAST_CODE" == "200" ]] && json_assert "$LAST_BODY" 'data["status"] == "ok"' && return 0
    sleep 1
  done
  echo "node $idx did not become healthy; last=$LAST_CODE $LAST_BODY" >&2
  return 1
}

wait_cluster() {
  local deadline=$((SECONDS + 60))
  while (( SECONDS < deadline )); do
    api 0 GET /v1/cluster '' 5
    if [[ "$LAST_CODE" == "200" ]] && json_assert "$LAST_BODY" 'data["healthy_nodes"] >= 3 and all(any(n["host_id"] == h and n.get("up") for n in data["nodes"]) for h in ["'"${HOST_IDS[0]}"'", "'"${HOST_IDS[1]}"'", "'"${HOST_IDS[2]}"'"])'; then
      return 0
    fi
    sleep 1
  done
  echo "cluster never reached 3 healthy nodes; last=$LAST_CODE $LAST_BODY" >&2
  return 1
}

wait_status() {
  local idx="$1" id="$2" want="$3" deadline=$((SECONDS + ${4:-75}))
  while (( SECONDS < deadline )); do
    api "$idx" GET "/v1/vms/$id/status" '' 10
    if [[ "$LAST_CODE" == "200" ]] && json_assert "$LAST_BODY" 'data.get("state") == "'"$want"'"'; then
      return 0
    fi
    sleep 1
  done
  echo "vm $id did not reach live state $want via node $idx; last=$LAST_CODE $LAST_BODY" >&2
  return 1
}

wait_running_ready() {
  local idx="$1" id="$2" deadline=$((SECONDS + ${3:-90}))
  while (( SECONDS < deadline )); do
    api "$idx" GET "/v1/vms/$id/status" '' 10
    if [[ "$LAST_CODE" == "200" ]] && json_assert "$LAST_BODY" 'data.get("state") == "running" and bool(data.get("vcpu_alive", False)) and int(data.get("uptime_ms", 0)) > 0'; then
      return 0
    fi
    sleep 1
  done
  echo "vm $id was not running/ready; last=$LAST_CODE $LAST_BODY" >&2
  return 1
}

exec_expect() {
  local idx="$1" vm_id="$2" command="$3" expected="$4" timeout_ms="${5:-30000}"
  local exec_id status deadline=$((SECONDS + 120)) last=""
  while (( SECONDS < deadline )); do
    api "$idx" POST /v1/execute_async "$(printf '{"vm_id":"%s","command":"%s","timeout_ms":%s}' "$vm_id" "$command" "$timeout_ms")" 15
    expect_code 202
    exec_id="$(json_get "$LAST_BODY" 'data["id"]')"
    local poll_deadline=$((SECONDS + 45))
    status=""
    while (( SECONDS < poll_deadline )); do
      api "$idx" GET "/v1/executions/$exec_id" '' 10
      expect_code 200
      status="$(json_get "$LAST_BODY" 'data["status"]')"
      if [[ "$status" == "completed" || "$status" == "failed" ]]; then
        break
      fi
      sleep 1
    done
    last="exec=$exec_id status=$status body=$LAST_BODY"
    if [[ "$status" == "completed" ]] && json_assert "$LAST_BODY" 'data.get("exit_code") == 0 and "'"$expected"'" in (data.get("stdout") or "")'; then
      return 0
    fi
    sleep 2
  done
  echo "expected exec stdout '$expected'; last $last" >&2
  return 1
}

create_vm() {
  local idx="$1" body="$2"
  api "$idx" POST /v1/vms "$body" 80
  expect_code 201
  CREATED_ID="$(json_get "$LAST_BODY" 'data["id"]')"
  CREATED_BODY="$LAST_BODY"
  record_vm "$CREATED_ID"
}

stop_vm_best_effort() {
  local id="$1"
  for idx in 0 1 2; do
    api "$idx" DELETE "/v1/vms/$id" '' 15 || true
    [[ "$LAST_CODE" == "204" || "$LAST_CODE" == "404" || "$LAST_CODE" == "409" ]] && return 0
  done
  return 0
}

postgres_cleanup() {
  command -v psql >/dev/null 2>&1 || return 0
  PGPASSWORD="${PGPASSWORD:-}" psql "$DATABASE_URL" -v ON_ERROR_STOP=0 -qAt <<SQL >/dev/null 2>&1 || true
DELETE FROM fleet_vms WHERE host_id LIKE '$RUN_ID-%';
DELETE FROM fleet_hosts WHERE host_id LIKE '$RUN_ID-%';
SQL
}

cleanup() {
  set +e
  for id in "${CREATED_VMS[@]:-}"; do
    stop_vm_best_effort "$id" >/dev/null 2>&1 || true
  done
  for pid in "${PIDS[@]:-}"; do
    if kill -0 "$pid" >/dev/null 2>&1; then
      kill "$pid" >/dev/null 2>&1 || true
      for _ in $(seq 1 20); do
        kill -0 "$pid" >/dev/null 2>&1 || break
        sleep 0.2
      done
      kill -9 "$pid" >/dev/null 2>&1 || true
    fi
  done
  postgres_cleanup
}
trap cleanup EXIT

run_case() {
  local name="$1"
  shift
  local detail=""
  set +e
  detail=$("$@" 2>&1)
  local rc=$?
  set -e
  if [[ $rc -eq 0 ]]; then
    printf 'PASS %-36s\n' "$name"
    REPORT_ROWS+=("PASS|$name|")
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    printf 'FAIL %-36s %s\n' "$name" "$detail"
    REPORT_ROWS+=("FAIL|$name|${detail//$'\n'/; }")
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

start_cluster() {
  [[ -x ./target/release/taritd ]] || cargo build --release -p taritd
  [[ -x "$VMM_BIN" ]] || { echo "missing VMM binary: $VMM_BIN" >&2; return 1; }
  [[ -r "$KERNEL" ]] || { echo "missing kernel: $KERNEL" >&2; return 1; }
  [[ -r "$ROOTFS" ]] || { echo "missing rootfs: $ROOTFS" >&2; return 1; }
  set +e
  if command -v sudo >/dev/null 2>&1; then
    sudo e2fsck -fy "$ROOTFS" >/dev/null
  else
    e2fsck -fy "$ROOTFS" >/dev/null
  fi
  local fsck_rc=$?
  set -e
  (( fsck_rc <= 3 )) || { echo "e2fsck failed with $fsck_rc" >&2; return 1; }
  mkdir -p "$RUN_DIR"
  postgres_cleanup
  for i in 0 1 2; do
    local node_dir="$RUN_DIR/node$((i+1))"
    mkdir -p "$node_dir/sockets"
    (
      export TARIT_API_KEY="$API_KEY"
      export TARIT_PEER_SECRET="$PEER_SECRET"
      export TARIT_DATABASE_URL="$DATABASE_URL"
      export TARIT_LISTEN="127.0.0.1:$((BASE_PORT+i))"
      export TARIT_RPC_ADDR="${URLS[$i]}"
      export TARIT_HOST_ID="${HOST_IDS[$i]}"
      export TARIT_SOCKET_DIR="$node_dir/sockets"
      export TARIT_DB="$node_dir/taritd.sqlite"
      export TARIT_VMM_BIN="$VMM_BIN"
      export TARIT_KERNEL="$KERNEL"
      export TARIT_ROOTFS="$ROOTFS"
      export TARIT_ROOTFS_READONLY="${TARIT_ROOTFS_READONLY:-1}"
      export TARIT_CONFIG="$node_dir/no-config.toml"
      export TARIT_WARM_POOL="${TARIT_WARM_POOL:-0}"
      export TARIT_MAX_VMS="$MAX_VMS"
      export TARIT_MAX_VCPUS="${TARIT_MAX_VCPUS:-64}"
      export TARIT_MAX_MEMORY_MIB="${TARIT_MAX_MEMORY_MIB:-65536}"
      export TARIT_ADMISSION_TIMEOUT_MS="$ADMISSION_TIMEOUT_MS"
      export RUST_LOG="${RUST_LOG:-taritd=info,tower_http=warn}"
      exec ./target/release/taritd >"$node_dir/taritd.log" 2>&1
    ) &
    PIDS+=("$!")
  done
  for i in 0 1 2; do wait_for_http "$i"; done
  wait_cluster
}

case_infra_auth() {
  noauth 0 GET /health
  expect_code 200
  json_assert "$LAST_BODY" 'data["status"] == "ok"'
  noauth 0 GET /v1/cluster
  expect_code 401
  api 0 GET /v1/cluster
  expect_code 200
  json_assert "$LAST_BODY" 'data["healthy_nodes"] >= 3 and all(any(n["host_id"] == h and n.get("up") for n in data["nodes"]) for h in ["'"${HOST_IDS[0]}"'", "'"${HOST_IDS[1]}"'", "'"${HOST_IDS[2]}"'"])'
}

case_lifecycle_state_machine() {
  local vm_id full_snap diff_snap
  create_vm 0 '{"memory_mib":256,"vcpus":1}'
  vm_id="$CREATED_ID"
  wait_running_ready 0 "$vm_id"
  api 0 GET /v1/vms
  expect_code 200
  json_assert "$LAST_BODY" 'any(vm["id"] == "'"$vm_id"'" for vm in data)'
  api 0 GET "/v1/vms/$vm_id"
  expect_code 200
  json_assert "$LAST_BODY" 'data["status"] == "running"'
  exec_expect 0 "$vm_id" 'echo lifecycle-ok' 'lifecycle-ok'
  api 0 POST "/v1/vms/$vm_id/pause" '{}'
  expect_code 200
  json_assert "$LAST_BODY" 'data["status"] == "paused"'
  wait_status 0 "$vm_id" paused
  api 0 POST "/v1/vms/$vm_id/resume" '{}'
  expect_code 200
  json_assert "$LAST_BODY" 'data["status"] == "running"'
  wait_running_ready 0 "$vm_id"
  exec_expect 0 "$vm_id" 'echo resumed-ok' 'resumed-ok'
  api 0 POST "/v1/vms/$vm_id/snapshot" '{"diff":false}' 60
  expect_code 200
  json_assert "$LAST_BODY" 'bool(data.get("path")) and bool(data.get("host_id"))'
  full_snap="$(json_get "$LAST_BODY" 'data["path"]')"
  echo "$full_snap" >"$RUN_DIR/full_snapshot_path"
  echo "$(json_get "$LAST_BODY" 'data["host_id"]')" >"$RUN_DIR/full_snapshot_host"
  api 0 POST "/v1/vms/$vm_id/snapshot" '{"diff":true}' 60
  expect_code 200
  json_assert "$LAST_BODY" 'bool(data.get("path"))'
  diff_snap="$(json_get "$LAST_BODY" 'data["path"]')"
  [[ -n "$diff_snap" ]]
  api 0 DELETE "/v1/vms/$vm_id" '' 20
  expect_code 204
  api 0 GET "/v1/vms/$vm_id/status" '' 10
  expect_code_in "404 409"
  api 0 GET "/v1/vms/$vm_id" '' 10
  expect_code 200
  json_assert "$LAST_BODY" 'data["status"] == "stopped"'
}

case_restore_from_snapshot() {
  local snap host restored
  snap="$(cat "$RUN_DIR/full_snapshot_path")"
  host="$(cat "$RUN_DIR/full_snapshot_host")"
  api 1 POST /v1/restore "$(printf '{"snapshot_path":"%s","host_id":"%s"}' "$snap" "$host")" 70
  expect_code 201
  restored="$(json_get "$LAST_BODY" 'data["id"]')"
  record_vm "$restored"
  json_assert "$LAST_BODY" 'data["status"] == "running"'
  wait_running_ready 1 "$restored"
  exec_expect 1 "$restored" 'echo restore-ok' 'restore-ok'
  api 1 DELETE "/v1/vms/$restored" '' 20
  expect_code 204
}

case_invalid_transitions_errors() {
  local missing stopped dup exec_id status
  missing="$(uuid)"
  api 0 GET "/v1/vms/$missing"
  expect_code 404
  api 0 GET "/v1/vms/$missing/status"
  expect_code 404
  api 0 POST "/v1/vms/$missing/pause" '{}'
  expect_code 404
  api 0 POST "/v1/vms/$missing/resume" '{}'
  expect_code 404
  api 0 POST "/v1/vms/$missing/snapshot" '{"diff":false}'
  expect_code 404
  api 0 POST /v1/execute_async "$(printf '{"vm_id":"%s","command":"echo nope","timeout_ms":1000}' "$missing")"
  expect_code 404
  api 0 DELETE "/v1/vms/$missing" '' 10
  expect_code 404
  api 0 PATCH "/v1/egress/vm/$missing" '{"allowlist":[],"allow_existing":false}'
  expect_code 404

  create_vm 0 '{"memory_mib":256,"vcpus":1}'
  stopped="$CREATED_ID"
  wait_running_ready 0 "$stopped"
  api 0 DELETE "/v1/vms/$stopped" '' 20
  expect_code 204
  api 0 POST "/v1/vms/$stopped/pause" '{}' 10
  expect_code_in "404 409"
  api 0 POST "/v1/vms/$stopped/snapshot" '{"diff":false}' 10
  expect_code_in "404 409"
  api 0 POST /v1/execute_async "$(printf '{"vm_id":"%s","command":"echo stopped","timeout_ms":1000}' "$stopped")" 10
  if [[ "$LAST_CODE" == "202" ]]; then
    exec_id="$(json_get "$LAST_BODY" 'data["id"]')"
    local deadline=$((SECONDS + 20))
    status=""
    while (( SECONDS < deadline )); do
      api 0 GET "/v1/executions/$exec_id" '' 5
      expect_code 200
      status="$(json_get "$LAST_BODY" 'data["status"]')"
      [[ "$status" == "failed" || "$status" == "completed" ]] && break
      sleep 1
    done
    [[ "$status" == "failed" ]] || { echo "stopped exec expected failed, got $status body=$LAST_BODY" >&2; return 1; }
  else
    expect_code_in "404 409"
  fi

  request POST "${URLS[0]}/v1/vms" '{' api 10
  expect_code_in "400 422"
  request POST "${URLS[0]}/v1/execute_async" '{"command":"missing vm"}' api 10
  expect_code_in "400 422"

  dup="$(uuid)"
  api 0 POST /v1/vms "$(printf '{"id":"%s","memory_mib":256,"vcpus":1}' "$dup")" 80
  expect_code 201
  record_vm "$dup"
  wait_running_ready 0 "$dup"
  api 1 POST /v1/vms "$(printf '{"id":"%s","memory_mib":256,"vcpus":1}' "$dup")" 20
  expect_code 409
  api 0 PATCH "/v1/egress/vm/$dup" '{"allowlist":["10.0.0.0/8:443/tcp"],"allow_existing":true}' 20
  expect_code 200
  json_assert "$LAST_BODY" '"rules_applied" in data'
  api 0 DELETE "/v1/vms/$dup" '' 20
  expect_code 204
}

case_capacity_backpressure() {
  local ids=() id
  for n in $(seq 1 $((MAX_VMS * 3))); do
    create_vm 0 '{"memory_mib":256,"vcpus":1}'
    id="$CREATED_ID"
    ids+=("$id")
    api 0 GET "/v1/vms/$id/status" '' 20
    expect_code 200
  done
  api 0 POST /v1/vms '{"memory_mib":256,"vcpus":1}' 15
  # A full cluster returns 429 + Retry-After (overload backpressure); older
  # builds returned 409. Accept either so the test tracks the shipped semantics.
  expect_code_in "409 429"
  for id in "${ids[@]}"; do
    api 0 DELETE "/v1/vms/$id" '' 20
    expect_code 204
  done
  sleep 10
  wait_cluster
}

case_cross_node_routing() {
  local filler1 filler2 target owner owner_idx route_idx snap_owner restored
  create_vm 0 '{"memory_mib":256,"vcpus":1}'
  filler1="$CREATED_ID"
  create_vm 0 '{"memory_mib":256,"vcpus":1}'
  filler2="$CREATED_ID"
  wait_running_ready 0 "$filler1"
  wait_running_ready 0 "$filler2"
  create_vm 0 '{"memory_mib":256,"vcpus":1}'
  target="$CREATED_ID"
  owner="$(json_get "$CREATED_BODY" 'data["host_id"]')"
  [[ "$owner" != "${HOST_IDS[0]}" ]] || { echo "target landed on entry node, expected peer" >&2; return 1; }
  owner_idx="$(node_index_for_host "$owner")"
  route_idx="$(other_node_index "$owner")"
  api "$route_idx" GET "/v1/vms/$target"
  expect_code 200
  json_assert "$LAST_BODY" 'data["id"] == "'"$target"'" and data["host_id"] == "'"$owner"'"'
  wait_running_ready "$route_idx" "$target"
  exec_expect "$route_idx" "$target" 'echo routed-exec' 'routed-exec'
  api "$route_idx" POST "/v1/vms/$target/pause" '{}'
  expect_code 200
  wait_status "$route_idx" "$target" paused
  api "$route_idx" POST "/v1/vms/$target/resume" '{}'
  expect_code 200
  wait_running_ready "$route_idx" "$target"
  api "$route_idx" POST "/v1/vms/$target/snapshot" '{"diff":false}' 60
  expect_code 200
  json_assert "$LAST_BODY" 'bool(data.get("path")) and data.get("host_id") == "'"$owner"'"'
  api "$route_idx" PATCH "/v1/egress/vm/$target" '{"allowlist":["192.168.0.0/16:443/tcp"],"allow_existing":true}' 20
  expect_code 200
  json_assert "$LAST_BODY" '"rules_applied" in data'

  api "$owner_idx" POST "/v1/vms/$target/snapshot" '{"diff":false}' 60
  expect_code 200
  snap_owner="$(json_get "$LAST_BODY" 'data["path"]')"
  api "$route_idx" POST /v1/restore "$(printf '{"snapshot_path":"%s","host_id":"%s"}' "$snap_owner" "$owner")" 70
  expect_code 201
  restored="$(json_get "$LAST_BODY" 'data["id"]')"
  record_vm "$restored"
  json_assert "$LAST_BODY" 'data["host_id"] == "'"$owner"'" and data["status"] == "running"'
  wait_running_ready "$route_idx" "$restored"
  exec_expect "$route_idx" "$restored" 'echo routed-restore' 'routed-restore'
  api "$route_idx" DELETE "/v1/vms/$restored" '' 20
  expect_code 204
  api "$route_idx" DELETE "/v1/vms/$target" '' 20
  expect_code 204
  api 0 DELETE "/v1/vms/$filler1" '' 20
  expect_code 204
  api 0 DELETE "/v1/vms/$filler2" '' 20
  expect_code 204
}

case_peer_security() {
  local missing
  missing="$(uuid)"
  noauth 0 GET "/internal/v1/vms/$missing"
  expect_code 401
  peer 0 GET "/internal/v1/vms/$missing"
  expect_code 404
}

main() {
  log "== starting 3-node taritd cluster run_id=$RUN_ID base_port=$BASE_PORT max_vms=$MAX_VMS =="
  start_cluster
  run_case "infra/auth" case_infra_auth
  run_case "create+lifecycle state machine" case_lifecycle_state_machine
  run_case "restore from full snapshot" case_restore_from_snapshot
  run_case "invalid transitions/errors" case_invalid_transitions_errors
  run_case "capacity/backpressure" case_capacity_backpressure
  run_case "cross-node routing" case_cross_node_routing
  run_case "peer security" case_peer_security

  printf '\n%-6s | %-36s | %s\n' STATUS CASE DETAIL
  printf '%s\n' '-------|--------------------------------------|----------------'
  for row in "${REPORT_ROWS[@]}"; do
    IFS='|' read -r st name detail <<<"$row"
    printf '%-6s | %-36s | %s\n' "$st" "$name" "$detail"
  done
  printf '\nSUMMARY: %d passed, %d failed\n' "$PASS_COUNT" "$FAIL_COUNT"
  [[ "$FAIL_COUNT" -eq 0 ]]
}

main "$@"
