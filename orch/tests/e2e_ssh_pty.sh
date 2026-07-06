#!/usr/bin/env bash
# tests/e2e_ssh_pty.sh — end-to-end SSH gateway + WebSocket PTY via taritd on a
# Linux+KVM host (the c8i validation box).
#
# Boots taritd with the SSH gateway enabled, pointing at the Tarit `vmm` binary
# and a PTY-agent-baked rootfs. Then, over the orchestrator REST API: registers
# an SSH key, creates a VM, opens a WebSocket PTY, and SSHes into the VM through
# the gateway. Both interactive paths bridge to the guest vsock PTY (no in-guest
# sshd). Run as root (needs /dev/kvm + loop mount).
#
#   sudo bash tests/e2e_ssh_pty.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ORCH_ROOT="${ORCH_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
TARIT_BIN="${TARIT_BIN:-$ORCH_ROOT/target/debug/taritd}"
VMM_BIN="${VMM_BIN:-$VMM_ROOT/target/debug/vmm}"
AGENT="$VMM_ROOT/guest/agent/vmm-agent"
BAKE="$VMM_ROOT/guest/agent/bake-agent.sh"
BASE_ROOTFS="${BASE_ROOTFS:-/tmp/vsock-rootfs.ext4}"
ROOTFS=/tmp/taritd-pty-rootfs.ext4

export TARIT_API_KEY="e2e-test-key"
export TARIT_LISTEN="127.0.0.1:8080"
export TARIT_VMM_BIN="$VMM_BIN"
export TARIT_KERNEL="/tmp/vmlinux.microvm"
export TARIT_ROOTFS="$ROOTFS"
export TARIT_ROOTFS_READONLY="0"
export TARIT_ENABLE_NET="0"
export TARIT_MAX_VMS="4"
export TARIT_SOCKET_DIR="${TARIT_SOCKET_DIR:-$TARITD_HOME/sockets}"
export TARIT_DB="${TARIT_DB:-$TARITD_HOME/fleet.db}"
export TARIT_SSH_GATEWAY="1"
export TARIT_SSH_GATEWAY_ADDR="127.0.0.1:2222"
export TARIT_SSH_GATEWAY_HOST_KEY="${TARIT_SSH_GATEWAY_HOST_KEY:-$TARITD_HOME/ssh_host_ed25519}"
export RUST_LOG="info,russh=warn"

API="http://127.0.0.1:8080"
H="X-API-Key: $TARIT_API_KEY"
KEYFILE="${KEYFILE:-$TARITD_HOME/e2e_id_ed25519}"
TARIT_LOG=/tmp/taritd-e2e.log
PASS_SSH=0
PASS_WS=0

mkdir -p "$TARIT_SOCKET_DIR"
rm -f "$TARIT_DB" "$TARIT_LOG"

# Kill any stale taritd / vmm serve from prior runs so ports/sockets are free.
for p in $(pgrep -f 'target/debug/taritd' 2>/dev/null) $(pgrep -f 'vmm serve' 2>/dev/null); do
  kill "$p" 2>/dev/null || true
done
sleep 1

echo "=== bake PTY-agent rootfs ==="
make -C "$VMM_ROOT/guest/agent" >/dev/null 2>&1 || true
[ -x "$AGENT" ] || { echo "FAIL: could not build vmm-agent"; exit 1; }
cp -f "$BASE_ROOTFS" "$ROOTFS"
e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$BAKE" "$ROOTFS" "$AGENT"

echo "=== ensure python websockets is available ==="
python3 -c 'import websockets' 2>/dev/null || {
  apt-get install -y -q python3-websockets >/dev/null 2>&1 || \
  (apt-get update -q >/dev/null 2>&1 && apt-get install -y -q python3-websockets >/dev/null 2>&1) || true
}
python3 -c 'import websockets; print("websockets", websockets.__version__)' 2>&1 || echo "WARN: websockets unavailable"

echo "=== generate e2e ssh keypair ==="
rm -f "$KEYFILE" "$KEYFILE.pub"
ssh-keygen -t ed25519 -N '' -f "$KEYFILE" -q
PUB=$(cat "$KEYFILE.pub")

echo "=== start taritd ==="
"$TARIT_BIN" >"$TARIT_LOG" 2>&1 &
TARIT_PID=$!
sleep 4
if ! kill -0 "$TARIT_PID" 2>/dev/null; then
  echo "taritd failed to start:"; tail -30 "$TARIT_LOG"; exit 1
fi

cleanup() {
  echo "=== cleanup ==="
  [ -n "${VM_ID:-}" ] && curl -s -X DELETE -H "$H" "$API/v1/vms/$VM_ID" >/dev/null 2>&1 || true
  kill "$TARIT_PID" 2>/dev/null || true
  sleep 1
}
trap cleanup EXIT

echo "=== register ssh key (POST /v1/ssh-keys) ==="
curl -s -H "$H" -H 'Content-Type: application/json' \
  -d "{\"public_key\":\"$PUB\"}" "$API/v1/ssh-keys"; echo

echo "=== create VM (POST /v1/vms) ==="
VM_JSON=$(curl -s -H "$H" -H 'Content-Type: application/json' \
  -d '{"memory_mib":512,"vcpus":1}' "$API/v1/vms")
echo "$VM_JSON"
VM_ID=$(echo "$VM_JSON" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("id",""))')
if [ -z "$VM_ID" ]; then echo "no vm id; taritd log:"; tail -30 "$TARIT_LOG"; exit 1; fi
echo "VM_ID=$VM_ID"
echo "  waiting 25s for guest boot + agent..."
sleep 25

echo ""
echo "=== TEST 1: SSH gateway interactive (real PTY) ==="
SSH_OUT=$(python3 "$SCRIPT_DIR/ssh_pty_test.py" "$KEYFILE" 2222 "$VM_ID" 127.0.0.1 2>&1)
echo "$SSH_OUT"
if echo "$SSH_OUT" | grep -q SSH_GW_PASS; then echo "SSH_GW_PASS"; PASS_SSH=1; else echo "SSH_GW_FAIL"; fi

echo ""
echo "=== TEST 2: WebSocket PTY (POST pty session + WS connect) ==="
PTY_JSON=$(curl -s -H "$H" -H 'Content-Type: application/json' \
  -d '{"cols":80,"rows":24}' "$API/v1/vms/$VM_ID/pty/sessions")
echo "$PTY_JSON"
PTY_ID=$(echo "$PTY_JSON" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("pty_id",""))')
PTY_TOKEN=$(echo "$PTY_JSON" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("connect_token",""))')
if [ -n "$PTY_ID" ]; then
  # Authenticate the WebSocket upgrade with the per-session connect token, not
  # the long-lived API key (R-008): the account credential never appears in a URL.
  WS_URL="ws://127.0.0.1:8080/v1/vms/$VM_ID/pty/$PTY_ID/connect?token=$PTY_TOKEN"
  WS_OUT=$(python3 "$SCRIPT_DIR/ws_pty_client.py" "$WS_URL" 2>&1)
  echo "$WS_OUT"
  if echo "$WS_OUT" | grep -q WS_PTY_PASS; then echo "WS_PTY_PASS"; PASS_WS=1; else echo "WS_PTY_FAIL"; fi
else
  echo "WS_PTY_FAIL (no pty_id)"
fi

echo ""
echo "=== gateway/taritd evidence ==="
grep -iE "ssh gateway|pty|attach|listening" "$TARIT_LOG" | tail -12

echo ""
echo "RESULT: SSH_GW=$PASS_SSH WS_PTY=$PASS_WS"
if [ "$PASS_SSH" = "1" ] && [ "$PASS_WS" = "1" ]; then echo "E2E_PASS"; exit 0; else echo "E2E_FAIL"; exit 1; fi
