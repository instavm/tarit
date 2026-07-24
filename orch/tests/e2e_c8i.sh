#!/usr/bin/env bash
# End-to-end API test for taritd on a Linux KVM host (e.g. EC2 c8i).
set -euo pipefail

BASE_URL="${TARIT_URL:-http://127.0.0.1:8080}"
API_KEY="${TARIT_API_KEY:?set TARIT_API_KEY}"
HDR=(-H "X-API-Key: ${API_KEY}" -H "Content-Type: application/json")
: "${EGRESS_TEST_IP:=1.1.1.1}"
: "${EGRESS_TEST_PORT:=443}"
GUEST_CURL="curl --insecure --fail --silent --show-error --connect-timeout 3 --max-time 8 --output /dev/null https://${EGRESS_TEST_IP}:${EGRESS_TEST_PORT}/"

curl_json() {
  local method=$1 path=$2
  shift 2
  curl -sfS -X "$method" "${BASE_URL}${path}" "${HDR[@]}" "$@"
}

exec_json() {
  local vm_id=$1 command=$2
  curl_json POST /v1/execute \
    -d "$(python3 -c 'import json,sys; print(json.dumps({"vm_id":sys.argv[1],"command":sys.argv[2],"timeout_ms":30000}))' "$vm_id" "$command")"
}

expect_exec_success() {
  local vm_id=$1 command=$2 expected=${3:-}
  local reply
  reply=$(exec_json "$vm_id" "$command")
  printf '%s' "$reply" | python3 -c '
import json, sys
expected = sys.argv[1]
result = json.load(sys.stdin)
assert result["exit_code"] == 0, result
assert not expected or expected in result.get("stdout", ""), result
' "$expected"
}

expect_exec_failure() {
  local vm_id=$1 command=$2
  exec_json "$vm_id" "$command" |
    python3 -c 'import json,sys; result=json.load(sys.stdin); assert result["exit_code"] != 0, result'
}

echo "== health (no auth) =="
curl -sfS "${BASE_URL}/health" | grep -q '"status":"ok"'

echo "== unauthorized without key =="
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "${BASE_URL}/v1/vms" -H 'Content-Type: application/json' -d '{}')
[[ "$code" == "401" ]]

echo "== create vm =="
VM_JSON=$(curl_json POST /v1/vms -d '{"memory_mib":256,"vcpus":1}')
VM_ID=$(echo "$VM_JSON" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
echo "vm_id=$VM_ID"
echo "$VM_JSON" | grep -q '"status":"running"'
expect_exec_success "$VM_ID" 'bash -c "echo create-runtime-ok"' create-runtime-ok

echo "== list vms =="
curl_json GET /v1/vms | grep -q "$VM_ID"

echo "== get vm =="
curl_json GET "/v1/vms/${VM_ID}" | grep -q '"status":"running"'

echo "== pause =="
curl_json POST "/v1/vms/${VM_ID}/pause" -d '{}' | grep -q '"status":"paused"'

echo "== resume =="
curl_json POST "/v1/vms/${VM_ID}/resume" -d '{}' | grep -q '"status":"running"'
expect_exec_success "$VM_ID" 'bash -c "echo resume-runtime-ok"' resume-runtime-ok

echo "== snapshot =="
SNAP=$(curl_json POST "/v1/vms/${VM_ID}/snapshot" -d '{"diff":false}')
echo "$SNAP" | grep -q '"path"'
SNAP_PATH=$(printf '%s' "$SNAP" | python3 -c 'import sys,json; print(json.load(sys.stdin)["path"])')

echo "== egress update =="
expect_exec_failure "$VM_ID" "$GUEST_CURL"
curl_json PATCH "/v1/egress/vm/${VM_ID}" \
  -d "{\"allowlist\":[\"${EGRESS_TEST_IP}/32:${EGRESS_TEST_PORT}/tcp\"],\"allow_existing\":false}" \
  | grep -q 'rules_applied'
expect_exec_success "$VM_ID" "$GUEST_CURL"
curl_json PATCH "/v1/egress/vm/${VM_ID}" -d '{"allowlist":[],"allow_existing":false}' |
  grep -q 'rules_applied'
expect_exec_failure "$VM_ID" "$GUEST_CURL"

echo "== execute async =="
EXEC_JSON=$(curl_json POST /v1/execute_async -d "{\"vm_id\":\"${VM_ID}\",\"command\":\"echo hello\",\"timeout_ms\":30000}")
EXEC_ID=$(echo "$EXEC_JSON" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
echo "exec_id=$EXEC_ID"

for _ in $(seq 1 60); do
  STATUS=$(curl_json GET "/v1/executions/${EXEC_ID}" | python3 -c 'import sys,json; print(json.load(sys.stdin)["status"])')
  [[ "$STATUS" == "completed" || "$STATUS" == "failed" ]] && break
  sleep 1
done
echo "execution status=$STATUS"
[[ "$STATUS" == "completed" ]]
curl_json GET "/v1/executions/${EXEC_ID}" | grep -qE 'hello|"exit_code":0'

echo "== delete vm =="
code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "${BASE_URL}/v1/vms/${VM_ID}" -H "X-API-Key: ${API_KEY}")
[[ "$code" == "204" ]]

curl_json GET "/v1/vms/${VM_ID}" | grep -q '"status":"stopped"' || true

echo "== restore and execute =="
RESTORE_JSON=$(curl_json POST /v1/restore \
  -d "$(python3 -c 'import json,sys; print(json.dumps({"snapshot_path":sys.argv[1]}))' "$SNAP_PATH")")
RESTORE_ID=$(printf '%s' "$RESTORE_JSON" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
expect_exec_success "$RESTORE_ID" 'bash -c "echo restore-runtime-ok"' restore-runtime-ok
code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "${BASE_URL}/v1/vms/${RESTORE_ID}" -H "X-API-Key: ${API_KEY}")
[[ "$code" == "204" ]]

echo "== multi-vm (3 sandboxes) =="
for index in 1 2 3; do
  MULTI_JSON=$(curl_json POST /v1/vms -d '{"memory_mib":256,"vcpus":1}')
  MULTI_ID=$(printf '%s' "$MULTI_JSON" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
  expect_exec_success "$MULTI_ID" "bash -c \"echo multi-runtime-${index}-ok\"" "multi-runtime-${index}-ok"
done
COUNT=$(curl_json GET /v1/vms | python3 -c 'import sys,json; print(len(json.load(sys.stdin)))')
[[ "$COUNT" -ge 3 ]] || { echo "expected >=3 vms, got $COUNT"; exit 1; }

echo "PASS: e2e_c8i (all states + multi-vm)"
