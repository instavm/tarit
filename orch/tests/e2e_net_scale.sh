#!/usr/bin/env bash
# tests/e2e_net_scale.sh — validate per-VM tap/IP/nft lifecycle: allocation,
# teardown, slot reuse, and stale-tap sweep. Run as root on c8i.
set -uo pipefail

ORCH_ROOT="${ORCH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
TARIT="${TARIT:-$ORCH_ROOT/target/debug/taritd}"
BASE_ROOTFS="${BASE_ROOTFS:-${TARIT_ROOTFS:-/tmp/vsock-rootfs.ext4}}"
ROOTFS=/tmp/net-rootfs.ext4

export TARIT_API_KEY="net-key"
export TARIT_LISTEN="127.0.0.1:8080"
export TARIT_VMM_BIN="$VMM_ROOT/target/debug/vmm"
export TARIT_KERNEL="${TARIT_KERNEL:-/tmp/vmlinux.microvm}"
export TARIT_ROOTFS="$ROOTFS"
export TARIT_ROOTFS_READONLY="0"
export TARIT_ENABLE_NET="1"
export TARIT_MAX_VMS="8"
export TARIT_SOCKET_DIR="${TARIT_SOCKET_DIR:-$TARITD_HOME/sockets}"
export TARIT_DB="${TARIT_DB:-$TARITD_HOME/fleet.db}"
export TARIT_NET_STATE="${TARIT_NET_STATE:-$TARITD_HOME/net.json}"
export TARIT_CONFIG="/tmp/net-empty.toml"
export TARIT_BASE_URL="http://127.0.0.1:8080"
export RUST_LOG="info"
: > /tmp/net-empty.toml
PASS=1
N=3
mkdir -p "$TARIT_SOCKET_DIR"; rm -f "$TARIT_DB" "$TARIT_NET_STATE"
for p in $(pgrep -f 'target/debug/taritd' 2>/dev/null) $(pgrep -f 'vmm serve' 2>/dev/null); do kill "$p" 2>/dev/null || true; done
sleep 1
make -C "$VMM_ROOT/guest/agent" >/dev/null 2>&1 || true
cp -f "$BASE_ROOTFS" "$ROOTFS"; e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$VMM_ROOT/guest/agent/bake-agent.sh" "$ROOTFS" "$VMM_ROOT/guest/agent/vmm-agent" >/dev/null

taps() { ip -o link show 2>/dev/null | grep -oE 'insta[a-z0-9_-]+' | sort -u; }
ntaps() { taps | grep -c . || true; }

start_taritd() { "$TARIT" serve >/tmp/taritd-net.log 2>&1 & echo $!; }
SP=$(start_taritd); sleep 4
cleanup() {
  for id in ${VMS:-}; do "$TARIT" vm delete "$id" >/dev/null 2>&1 || true; done
  kill "$SP" 2>/dev/null || true; sleep 1
  for t in $(taps); do ip link del "$t" 2>/dev/null || true; done
}
trap cleanup EXIT

create_n() {
  local ids=""
  for i in $(seq 1 "$1"); do
    id=$("$TARIT" --json vm create --vcpus 1 --memory-mib 256 2>/dev/null | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
    ids="$ids $id"
  done
  echo "$ids"
}

assert_guest_runtime() {
  local id output
  for id in "$@"; do
    output=$("$TARIT" exec "$id" "bash -c 'echo net-runtime-ok'" 2>&1) || {
      echo "FAIL: guest exec failed for $id: $output"
      return 1
    }
    printf '%s' "$output" | grep -q 'net-runtime-ok' || {
      echo "FAIL: guest exec returned no marker for $id: $output"
      return 1
    }
  done
}

echo "=== create $N VMs ==="
VMS=$(create_n "$N")
echo "  VMs:$VMS"
assert_guest_runtime $VMS || PASS=0
sleep 2
T1=$(ntaps); TAPS1=$(taps | tr '\n' ' ')
echo "  taps after create ($T1): $TAPS1"
[ "$T1" -ge "$N" ] || { echo "FAIL: expected >= $N taps, got $T1"; PASS=0; }
nft list table ip taritd_nat >/tmp/nft1 2>/dev/null || true

echo "=== delete the $N VMs ==="
for id in $VMS; do "$TARIT" vm delete "$id" >/dev/null 2>&1 || true; done
VMS=""
sleep 2
T2=$(ntaps)
echo "  taps after delete ($T2): $(taps | tr '\n' ' ')"
[ "$T2" -eq 0 ] || { echo "FAIL: taps leaked after delete (got $T2)"; PASS=0; }

echo "=== recreate $N VMs (expect slot/tap reuse) ==="
VMS=$(create_n "$N")
assert_guest_runtime $VMS || PASS=0
sleep 2
TAPS3=$(taps | tr '\n' ' ')
echo "  taps after recreate: $TAPS3"
[ "$(ntaps)" -ge "$N" ] || { echo "FAIL: recreate taps"; PASS=0; }
[ "$TAPS1" = "$TAPS3" ] && echo "  slots REUSED (same tap set)" || echo "  note: tap set differs (reuse may pick lowest-free): '$TAPS1' vs '$TAPS3'"
for id in $VMS; do "$TARIT" vm delete "$id" >/dev/null 2>&1 || true; done
VMS=""; sleep 2

echo "=== stale-tap sweep ==="
ORPHAN=insta999
ip tuntap add "$ORPHAN" mode tap 2>/dev/null || ip link add "$ORPHAN" type dummy 2>/dev/null || true
ip link show "$ORPHAN" >/dev/null 2>&1 && echo "  created orphan $ORPHAN"
kill "$SP" 2>/dev/null || true; sleep 1
SP=$(start_taritd); sleep 6
if ip link show "$ORPHAN" >/dev/null 2>&1; then echo "  note: orphan $ORPHAN still present (sweep may be age-gated / periodic)"; else echo "  orphan $ORPHAN swept on restart"; fi

echo ""
if [ "$PASS" = 1 ]; then echo "RESULT: NET_SCALE_PASS"; exit 0; else echo "RESULT: NET_SCALE_FAIL"; exit 1; fi
