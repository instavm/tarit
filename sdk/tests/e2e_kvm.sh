#!/usr/bin/env bash
# Exercise both published SDK surfaces against one disposable real-KVM server.
set -Eeuo pipefail
umask 077

ROOT=${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}
FIXTURE=${TARIT_SDK_FIXTURE:-$ROOT/sdk/tests/serve_kvm_fixture.sh}
PYTHON_E2E=${TARIT_SDK_PYTHON_E2E:-$ROOT/sdk/tests/python/e2e.py}
TYPESCRIPT_E2E=${TARIT_SDK_TYPESCRIPT_E2E:-$ROOT/sdk/tests/typescript/e2e.ts}
TYPESCRIPT_DIR=${TARIT_SDK_TYPESCRIPT_DIR:-$ROOT/sdk/typescript}
TSX=${TARIT_SDK_TSX:-$TYPESCRIPT_DIR/node_modules/.bin/tsx}
PYTHON_BIN=${TARIT_SDK_PYTHON_BIN:-python3}
SOCKET_ROOT=${SOCKET_ROOT:-/tmp}
LOCK=${LOCK:-/run/lock/tarit-september-global.lock}
LOCK_WAIT_SECONDS=${TARIT_SDK_LOCK_WAIT_SECONDS:-43200}
PORT=${PORT:-18082}
VM_ID=${VM_ID:-11111111-1111-4111-8111-111111111111}
PYTHON_CHILD_ID=${PYTHON_CHILD_ID:-22222222-2222-4222-8222-222222222222}
TYPESCRIPT_CHILD_ID=${TYPESCRIPT_CHILD_ID:-33333333-3333-4333-8333-333333333333}
TENANT_KEY=${TENANT_KEY:-sdk-tenant-key}
FOREIGN_KEY=${FOREIGN_KEY:-sdk-foreign-key}
KEEP_FAILED=${TARIT_E2E_KEEP_FAILED:-0}

[ "$(id -u)" -eq 0 ] || { echo "e2e_kvm.sh must run as root" >&2; exit 1; }
for required in curl flock grep; do
  command -v "$required" >/dev/null || { echo "missing $required" >&2; exit 1; }
done
if [[ "$PYTHON_BIN" == */* ]]; then
  [ -x "$PYTHON_BIN" ] || { echo "Python executable is missing: $PYTHON_BIN" >&2; exit 1; }
else
  command -v "$PYTHON_BIN" >/dev/null || { echo "missing $PYTHON_BIN" >&2; exit 1; }
fi
for required in TARITD_BIN VMM_BIN KERNEL ROOTFS_SOURCE AGENT; do
  [ -n "${!required:-}" ] || { echo "set $required" >&2; exit 1; }
done
for required in "$FIXTURE" "$TSX"; do
  [ -x "$required" ] || { echo "required executable is missing: $required" >&2; exit 1; }
done
for required in "$PYTHON_E2E" "$TYPESCRIPT_E2E"; do
  [ -r "$required" ] || { echo "required test is unreadable: $required" >&2; exit 1; }
done
[[ "$KEEP_FAILED" =~ ^[01]$ ]] || { echo "TARIT_E2E_KEEP_FAILED must be 0 or 1" >&2; exit 1; }
[[ "$LOCK_WAIT_SECONDS" =~ ^[1-9][0-9]*$ ]] || {
  echo "TARIT_SDK_LOCK_WAIT_SECONDS must be a positive integer" >&2
  exit 1
}
"$PYTHON_BIN" -c 'import httpx, websockets' >/dev/null
install -d -m 0755 "$(dirname "$LOCK")"
touch "$LOCK"
chmod 0600 "$LOCK"
exec 9<"$LOCK"
flock -w "$LOCK_WAIT_SECONDS" 9 || {
  echo "timed out waiting for the SDK KVM host lock" >&2
  exit 1
}

RUN_DIR=$(mktemp -d "$SOCKET_ROOT/tarit-sdk-e2e.XXXXXX")
FIXTURE_LOCK=$RUN_DIR/fixture.lock
FIXTURE_LOG=$RUN_DIR/fixture.log
PYTHON_LOG=$RUN_DIR/python.log
TYPESCRIPT_LOG=$RUN_DIR/typescript.log
FIXTURE_PID=""
touch "$FIXTURE_LOCK"

cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [ -n "$FIXTURE_PID" ] && kill -0 "$FIXTURE_PID" 2>/dev/null; then
    kill -TERM "$FIXTURE_PID" 2>/dev/null || true
    for _ in $(seq 1 100); do
      kill -0 "$FIXTURE_PID" 2>/dev/null || break
      sleep .1
    done
    kill -KILL "$FIXTURE_PID" 2>/dev/null || true
  fi
  [ -z "$FIXTURE_PID" ] || wait "$FIXTURE_PID" 2>/dev/null || true
  if [ "$status" -ne 0 ]; then
    echo "SDK KVM integration failed; fixture log follows" >&2
    tail -240 "$FIXTURE_LOG" >&2 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ] && [ "$KEEP_FAILED" -eq 1 ]; then
    echo "SDK KVM diagnostics retained at $RUN_DIR" >&2
  else
    find "$RUN_DIR" -depth -delete 2>/dev/null || true
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

TARITD_BIN="$TARITD_BIN" \
VMM_BIN="$VMM_BIN" \
KERNEL="$KERNEL" \
ROOTFS_SOURCE="$ROOTFS_SOURCE" \
AGENT="$AGENT" \
SOCKET_ROOT="$SOCKET_ROOT" \
PORT="$PORT" \
VM_ID="$VM_ID" \
TENANT_KEY="$TENANT_KEY" \
FOREIGN_KEY="$FOREIGN_KEY" \
LOCK="$FIXTURE_LOCK" \
"$FIXTURE" >"$FIXTURE_LOG" 2>&1 &
FIXTURE_PID=$!

for _ in $(seq 1 300); do
  grep -q '^SDK_SERVER_READY ' "$FIXTURE_LOG" 2>/dev/null && break
  kill -0 "$FIXTURE_PID" 2>/dev/null || {
    echo "SDK fixture exited before readiness" >&2
    exit 1
  }
  sleep .2
done
grep -q "^SDK_SERVER_READY vm_id=$VM_ID port=$PORT$" "$FIXTURE_LOG" || {
  echo "SDK fixture did not become ready" >&2
  exit 1
}

export TARIT_SDK_BASE_URL="http://127.0.0.1:$PORT"
export TARIT_SDK_TENANT_KEY="$TENANT_KEY"
export TARIT_SDK_FOREIGN_KEY="$FOREIGN_KEY"
export TARIT_SDK_VM_ID="$VM_ID"
export TARIT_SDK_PYTHON_CHILD_ID="$PYTHON_CHILD_ID"
export TARIT_SDK_TYPESCRIPT_CHILD_ID="$TYPESCRIPT_CHILD_ID"

PYTHONPATH="$ROOT/sdk/python${PYTHONPATH:+:$PYTHONPATH}" \
  "$PYTHON_BIN" "$PYTHON_E2E" | tee "$PYTHON_LOG"
grep -q '^PYTHON_SDK_E2E_PASS ' "$PYTHON_LOG"

(cd "$TYPESCRIPT_DIR" && "$TSX" "$TYPESCRIPT_E2E") | tee "$TYPESCRIPT_LOG"
grep -q '^TYPESCRIPT_SDK_E2E_PASS ' "$TYPESCRIPT_LOG"

tenant_count=$(curl -fsS -H "X-API-Key: $TENANT_KEY" \
  "$TARIT_SDK_BASE_URL/v1/vms" | "$PYTHON_BIN" -c 'import json,sys; print(len(json.load(sys.stdin)))')
foreign_count=$(curl -fsS -H "X-API-Key: $FOREIGN_KEY" \
  "$TARIT_SDK_BASE_URL/v1/vms" | "$PYTHON_BIN" -c 'import json,sys; print(len(json.load(sys.stdin)))')
[ "$tenant_count" -eq 3 ] || { echo "expected 3 tenant VMs, got $tenant_count" >&2; exit 1; }
[ "$foreign_count" -eq 0 ] || { echo "foreign tenant listed $foreign_count VMs" >&2; exit 1; }

echo "SDK_KVM_E2E_PASS clients=python,typescript fork_replay=pass tenant_denials=8 tenant_vm_count=3 foreign_vm_count=0"
