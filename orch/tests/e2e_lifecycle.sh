#!/usr/bin/env bash
# tests/e2e_lifecycle.sh — validate graceful drain/reaper + 429 backpressure.
# Part 1: SIGTERM to taritd reaps the per-VM `vmm serve` child (no orphan) and
# cleans the socket. Part 2: under a tight capacity + short admission timeout, a
# create beyond capacity returns 429 + Retry-After. Run as root on c8i.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ORCH_ROOT="${ORCH_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
TARIT="${TARIT:-$ORCH_ROOT/target/debug/taritd}"
BASE_ROOTFS="${BASE_ROOTFS:-/tmp/vsock-rootfs.ext4}"
ROOTFS=/tmp/taritd-pty-rootfs.ext4

export TARIT_API_KEY="life-e2e-key"
export TARIT_LISTEN="127.0.0.1:8080"
export TARIT_VMM_BIN="$VMM_ROOT/target/debug/vmm"
export TARIT_KERNEL="/tmp/vmlinux.microvm"
export TARIT_ROOTFS="$ROOTFS"
export TARIT_ROOTFS_READONLY="0"
export TARIT_ENABLE_NET="0"
export TARIT_SOCKET_DIR="${TARIT_SOCKET_DIR:-$TARITD_HOME/sockets}"
export TARIT_DB="${TARIT_DB:-$TARITD_HOME/fleet.db}"
export TARIT_BASE_URL="http://127.0.0.1:8080"
export TARIT_REAP_ON_SHUTDOWN="true"
export RUST_LOG="info"

py() { python3 -c "import sys,json;print(json.load(sys.stdin).get('$1',''))"; }
REAP=0; BP=0
mkdir -p "$TARIT_SOCKET_DIR"; rm -f "$TARIT_DB"
for p in $(pgrep -f 'target/debug/taritd' 2>/dev/null) $(pgrep -f 'vmm serve' 2>/dev/null); do kill "$p" 2>/dev/null || true; done
sleep 1

echo "=== bake rootfs ==="
make -C "$VMM_ROOT/guest/agent" >/dev/null 2>&1 || true
cp -f "$BASE_ROOTFS" "$ROOTFS"; e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$VMM_ROOT/guest/agent/bake-agent.sh" "$ROOTFS" "$VMM_ROOT/guest/agent/vmm-agent" >/dev/null

echo ""
echo "=== PART 1: reaper on SIGTERM ==="
export TARIT_MAX_VMS=4
"$TARIT" serve >/tmp/taritd-life1.log 2>&1 & IP=$!
sleep 4
VM=$("$TARIT" --json vm create --vcpus 1 --memory-mib 512)
CHILD=$(echo "$VM" | py pid); SOCK=$(echo "$VM" | py socket_path)
echo "  child vmm serve pid=$CHILD socket=$SOCK"
sleep 3
echo "  sending SIGTERM to taritd ($IP)"
kill -TERM "$IP"
for i in $(seq 1 30); do kill -0 "$IP" 2>/dev/null || break; sleep 0.5; done
sleep 1
if [ -n "$CHILD" ] && kill -0 "$CHILD" 2>/dev/null; then
  echo "  FAIL: vmm serve child $CHILD orphaned after taritd shutdown"
else
  echo "  ok: vmm serve child reaped"; REAP=1
fi
if [ -n "$SOCK" ] && [ -S "$SOCK" ]; then echo "  FAIL: socket $SOCK remains"; REAP=0; else echo "  ok: socket cleaned"; fi
for p in $(pgrep -f 'target/debug/taritd' 2>/dev/null) $(pgrep -f 'vmm serve' 2>/dev/null); do kill "$p" 2>/dev/null || true; done
sleep 1; rm -f "$TARIT_DB"

echo ""
echo "=== PART 2: 429 + Retry-After under overload ==="
export TARIT_MAX_VMS=1 TARIT_ADMISSION_TIMEOUT_MS=1000
"$TARIT" serve >/tmp/taritd-life2.log 2>&1 & IP2=$!
sleep 4
echo "  create VM 1 (takes the only slot)"
"$TARIT" --json vm create --vcpus 1 --memory-mib 512 | py id
echo "  create VM 2 (expect 429)"
CODE=$(curl -s -o /tmp/life-body -w '%{http_code}' -D /tmp/life-hdr \
  -H "X-API-Key: $TARIT_API_KEY" -H 'Content-Type: application/json' \
  -d '{"memory_mib":512,"vcpus":1}' http://127.0.0.1:8080/v1/vms)
echo "  2nd create HTTP status: $CODE"
grep -i '^retry-after' /tmp/life-hdr || echo "  (no Retry-After header)"
if [ "$CODE" = 429 ] && grep -qi '^retry-after' /tmp/life-hdr; then BP=1; fi
kill "$IP2" 2>/dev/null || true
for i in $(seq 1 20); do kill -0 "$IP2" 2>/dev/null || break; sleep 0.5; done
for p in $(pgrep -f 'target/debug/taritd' 2>/dev/null) $(pgrep -f 'vmm serve' 2>/dev/null); do kill "$p" 2>/dev/null || true; done

echo ""
echo "RESULT: REAP=$REAP BACKPRESSURE=$BP"
if [ "$REAP" = 1 ] && [ "$BP" = 1 ]; then echo "LIFECYCLE_PASS"; exit 0; else echo "LIFECYCLE_FAIL"; exit 1; fi
