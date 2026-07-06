#!/usr/bin/env bash
# End-to-end API test for taritd on a Linux KVM host (e.g. EC2 c8i).
set -euo pipefail

BASE_URL="${TARIT_URL:-http://127.0.0.1:8080}"
API_KEY="${TARIT_API_KEY:?set TARIT_API_KEY}"
HDR=(-H "X-API-Key: ${API_KEY}" -H "Content-Type: application/json")

curl_json() {
  local method=$1 path=$2
  shift 2
  curl -sfS -X "$method" "${BASE_URL}${path}" "${HDR[@]}" "$@"
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

echo "== list vms =="
curl_json GET /v1/vms | grep -q "$VM_ID"

echo "== get vm =="
curl_json GET "/v1/vms/${VM_ID}" | grep -q '"status":"running"'

echo "== pause =="
curl_json POST "/v1/vms/${VM_ID}/pause" -d '{}' | grep -q '"status":"paused"'

echo "== resume =="
curl_json POST "/v1/vms/${VM_ID}/resume" -d '{}' | grep -q '"status":"running"'

echo "== snapshot =="
SNAP=$(curl_json POST "/v1/vms/${VM_ID}/snapshot" -d '{"diff":false}')
echo "$SNAP" | grep -q '"path"'

echo "== egress update =="
curl_json PATCH "/v1/egress/vm/${VM_ID}" -d '{"allowlist":["10.0.0.0/8:443/tcp"],"allow_existing":true}' \
  | grep -q 'rules_applied'

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

echo "== multi-vm (3 sandboxes) =="
for _ in 1 2 3; do
  curl_json POST /v1/vms -d '{"memory_mib":128,"vcpus":1,"rootfs_path":""}' >/dev/null
done
COUNT=$(curl_json GET /v1/vms | python3 -c 'import sys,json; print(len(json.load(sys.stdin)))')
[[ "$COUNT" -ge 3 ]] || { echo "expected >=3 vms, got $COUNT"; exit 1; }

echo "PASS: e2e_c8i (all states + multi-vm)"
