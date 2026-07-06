#!/usr/bin/env bash
# tests/e2e_cli.sh — exercise the whole orchestrator flow through the `taritd`
# CLI (no curl): serve, vm create, ssh-key add, exec, metrics, interactive pty,
# and ssh through the gateway. Run as root on a Linux+KVM host (c8i).
#
#   sudo bash tests/e2e_cli.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ORCH_ROOT="${ORCH_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
TARIT="${TARIT:-$ORCH_ROOT/target/debug/taritd}"
BASE_ROOTFS="${BASE_ROOTFS:-/tmp/vsock-rootfs.ext4}"
ROOTFS=/tmp/taritd-pty-rootfs.ext4

export TARIT_API_KEY="cli-e2e-key"
export TARIT_LISTEN="127.0.0.1:8080"
export TARIT_VMM_BIN="$VMM_ROOT/target/debug/vmm"
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
export TARIT_BASE_URL="http://127.0.0.1:8080"
export RUST_LOG="info,russh=warn"

KEYFILE="${KEYFILE:-$TARITD_HOME/cli_id_ed25519}"
LOG=/tmp/taritd-cli-e2e.log
PASS=1
note() { echo "  -> $1"; }

mkdir -p "$TARIT_SOCKET_DIR"
rm -f "$TARIT_DB" "$LOG" "$KEYFILE" "$KEYFILE.pub"
for p in $(pgrep -f 'target/debug/taritd' 2>/dev/null) $(pgrep -f 'vmm serve' 2>/dev/null); do kill "$p" 2>/dev/null || true; done
sleep 1

echo "=== bake PTY rootfs ==="
make -C "$VMM_ROOT/guest/agent" >/dev/null 2>&1 || true
[ -x "$VMM_ROOT/guest/agent/vmm-agent" ] || { echo "FAIL: could not build vmm-agent"; exit 1; }
cp -f "$BASE_ROOTFS" "$ROOTFS"; e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$VMM_ROOT/guest/agent/bake-agent.sh" "$ROOTFS" "$VMM_ROOT/guest/agent/vmm-agent" >/dev/null
python3 -c 'import websockets' 2>/dev/null || apt-get install -y -q python3-websockets >/dev/null 2>&1 || true
ssh-keygen -t ed25519 -N '' -f "$KEYFILE" -q

echo "=== taritd serve (daemon) ==="
"$TARIT" serve >"$LOG" 2>&1 &
SERVE_PID=$!
sleep 4
cleanup() { [ -n "${VM_ID:-}" ] && "$TARIT" vm delete "$VM_ID" >/dev/null 2>&1 || true; kill "$SERVE_PID" 2>/dev/null || true; }
trap cleanup EXIT

echo "=== taritd health ==="
"$TARIT" health || PASS=0

echo "=== taritd ssh-key add / list ==="
"$TARIT" ssh-key add "$KEYFILE.pub" || PASS=0
"$TARIT" ssh-key list

echo "=== taritd vm create ==="
VM_ID=$("$TARIT" vm create --vcpus 1 --memory-mib 512 | awk 'NR==1{print $1}')
note "VM_ID=$VM_ID"
[ -n "$VM_ID" ] || { echo "FAIL: no vm id"; tail -20 "$LOG"; exit 1; }
echo "  waiting 25s for guest boot..."; sleep 25

echo "=== taritd vm list ==="
"$TARIT" vm list

echo "=== taritd exec ==="
EXEC_OUT=$("$TARIT" exec "$VM_ID" "echo CLI_EXEC_OK; uname -s")
echo "$EXEC_OUT"
echo "$EXEC_OUT" | grep -q CLI_EXEC_OK || { echo "FAIL: exec"; PASS=0; }

echo "=== taritd metrics (expect running VM + RSS) ==="
MET=$("$TARIT" metrics)
echo "$MET" | grep -E 'taritd_vms\{status="running"\} [1-9]|taritd_vm_memory_rss_bytes' | head -3
echo "$MET" | grep -Eq 'taritd_vms\{status="running"\} [1-9]' || { echo "FAIL: metrics running count"; PASS=0; }
echo "$MET" | grep -q 'taritd_vm_memory_rss_bytes' || note "note: no per-vm rss line"

echo "=== taritd pty (interactive, driven via real pty) ==="
PTY_OUT=$(python3 "$SCRIPT_DIR/pty_drive.py" CLI_PTY_OK -- "$TARIT" pty "$VM_ID" 2>&1)
echo "$PTY_OUT" | tail -6
echo "$PTY_OUT" | grep -q 'CLI_PTY_OK' || { echo "FAIL: taritd pty"; PASS=0; }

echo "=== taritd ssh (gateway, driven via real pty) ==="
SSH_OUT=$(python3 "$SCRIPT_DIR/pty_drive.py" CLI_SSH_OK -- "$TARIT" ssh "$VM_ID" -i "$KEYFILE" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -o LogLevel=ERROR 2>&1)
echo "$SSH_OUT" | tail -6
echo "$SSH_OUT" | grep -q 'CLI_SSH_OK' || { echo "FAIL: taritd ssh"; PASS=0; }

echo ""
if [ "$PASS" = 1 ]; then echo "RESULT: CLI_E2E_PASS"; exit 0; else echo "RESULT: CLI_E2E_FAIL"; exit 1; fi
