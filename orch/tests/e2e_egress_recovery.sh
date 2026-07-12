#!/usr/bin/env bash
# Validate fail-closed egress guard reconciliation after a taritd restart.
# Run as root on a Linux KVM host such as c8i with BASE_ROOTFS and TARIT_KERNEL set.
set -euo pipefail

[ "$(id -u)" -eq 0 ] || { echo "FAIL: run as root"; exit 1; }
[ "$(uname -s)" = Linux ] || { echo "FAIL: run on Linux"; exit 1; }
: "${BASE_ROOTFS:?set BASE_ROOTFS to a bootable guest rootfs}"
: "${TARIT_KERNEL:?set TARIT_KERNEL to the guest kernel}"

ORCH_ROOT="${ORCH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
TARIT="${TARIT:-$ORCH_ROOT/target/debug/taritd}"
RUN_DIR="$TARITD_HOME/egress-recovery"
ROOTFS="$RUN_DIR/rootfs.ext4"
SERVER_LOG="$RUN_DIR/taritd.log"

export TARIT_API_KEY="egress-recovery-key"
export TARIT_LISTEN="127.0.0.1:8080"
export TARIT_VMM_BIN="$VMM_ROOT/target/debug/vmm"
export TARIT_KERNEL
export TARIT_ROOTFS="$ROOTFS"
export TARIT_ROOTFS_READONLY="0"
export TARIT_ENABLE_NET="1"
export TARIT_MAX_VMS="2"
export TARIT_SOCKET_DIR="$RUN_DIR/sockets"
export TARIT_DB="$RUN_DIR/fleet.db"
export TARIT_NET_STATE="$RUN_DIR/net.json"
export TARIT_CONFIG="$RUN_DIR/empty.toml"
export TARIT_BASE_URL="http://127.0.0.1:8080"

mkdir -p "$TARIT_SOCKET_DIR"
: > "$TARIT_CONFIG"
rm -f "$TARIT_DB" "$TARIT_NET_STATE" "$SERVER_LOG"
cp -f "$BASE_ROOTFS" "$ROOTFS"

SERVER_PID=
VM_ID=
start_taritd() {
  "$TARIT" serve >>"$SERVER_LOG" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 40); do
    curl -fsS "$TARIT_BASE_URL/health" >/dev/null 2>&1 && return
    sleep 1
  done
  echo "FAIL: taritd did not become healthy"
  cat "$SERVER_LOG"
  return 1
}
stop_taritd() {
  [ -n "$SERVER_PID" ] || return
  kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=
}
cleanup() {
  if [ -n "$VM_ID" ] && [ -n "$SERVER_PID" ]; then
    "$TARIT" vm delete "$VM_ID" >/dev/null 2>&1 || true
  fi
  stop_taritd
}
trap cleanup EXIT

create_vm() {
  "$TARIT" --json vm create --vcpus 1 --memory-mib 256 |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])'
}
require_rule() {
  local text=$1 expected=$2
  grep -F -- "$expected" <<<"$text" >/dev/null ||
    { echo "FAIL: missing rule: $expected"; exit 1; }
}

start_taritd
VM_ID=$(create_vm)
sleep 2

NAT_RULES=$(nft list chain ip taritd_nat post)
TAP=$(sed -n "s/.*vm=$VM_ID tap=\\([^\" ]*\\).*/\\1/p" <<<"$NAT_RULES" | head -1)
[ -n "$TAP" ] || { echo "FAIL: no NAT allocation for $VM_ID"; exit 1; }
SLOT=${TAP#insta}
[ "$SLOT" != "$TAP" ] && [ "$SLOT" -ge 0 ] 2>/dev/null ||
  { echo "FAIL: invalid recovered TAP $TAP"; exit 1; }
BASE=$((SLOT * 4))
GUEST="172.16.$((BASE / 256)).$((BASE % 256 + 2))"
UPLINK=$(ip route get 8.8.8.8 | awk '/ dev / {for (i=1; i<=NF; i++) if ($i == "dev") {print $(i+1); exit}}')
[ -n "$UPLINK" ] || { echo "FAIL: no default uplink"; exit 1; }

stop_taritd
start_taritd

[ "$(sysctl -n "net.ipv6.conf.$TAP.disable_ipv6")" = 1 ] ||
  { echo "FAIL: recovered TAP permits IPv6"; exit 1; }
NETDEV=$(nft list table netdev "taritd_ingress_$SLOT")
require_rule "$NETDEV" "policy drop"
require_rule "$NETDEV" "ether type arp accept"
require_rule "$NETDEV" "ether type ip accept"

FORWARD=$(nft list chain ip taritd_nat vm_egress)
require_rule "$FORWARD" "iifname \"$TAP\" ip saddr != $GUEST drop"
require_rule "$FORWARD" "iifname \"$TAP\" ip saddr $GUEST ip daddr 172.16.0.0/16 drop"
require_rule "$FORWARD" "iifname \"$TAP\" ip saddr $GUEST oifname != \"$UPLINK\" drop"
INPUT=$(nft list chain ip taritd_nat vm_input)
require_rule "$INPUT" "iifname \"$TAP\" ip saddr != $GUEST drop"
require_rule "$INPUT" "iifname \"$TAP\" ct state established,related accept"
require_rule "$INPUT" "iifname \"$TAP\" ip drop"
NAT_RULES=$(nft list chain ip taritd_nat post)
require_rule "$NAT_RULES" "iifname \"$TAP\" ip saddr $GUEST oifname \"$UPLINK\" masquerade"

"$TARIT" vm delete "$VM_ID"
VM_ID=
sleep 2
! ip link show "$TAP" >/dev/null 2>&1 ||
  { echo "FAIL: recovered TAP leaked after delete"; exit 1; }
! nft list table netdev "taritd_ingress_$SLOT" >/dev/null 2>&1 ||
  { echo "FAIL: recovered netdev guard leaked after delete"; exit 1; }
! nft -a list table ip taritd_nat | grep -F "tap=$TAP" >/dev/null ||
  { echo "FAIL: recovered nft rules leaked after delete"; exit 1; }

VM_ID=$(create_vm)
sleep 2
REUSED=$(nft list chain ip taritd_nat post)
require_rule "$REUSED" "tap=$TAP"
echo "RESULT: EGRESS_RECOVERY_PASS"
