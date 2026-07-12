#!/usr/bin/env bash
# Validate fail-closed egress guards with two real guests on a Linux KVM host.
set -euo pipefail

[ "$(id -u)" -eq 0 ] || { echo "FAIL: run as root"; exit 1; }
[ "$(uname -s)" = Linux ] || { echo "FAIL: run on Linux"; exit 1; }
: "${BASE_ROOTFS:?set BASE_ROOTFS to a bootable guest rootfs}"
: "${TARIT_KERNEL:?set TARIT_KERNEL to the guest kernel}"
: "${EGRESS_TEST_IP:?set EGRESS_TEST_IP to a reachable external IPv4 TCP endpoint}"
: "${EGRESS_TEST_PORT:?set EGRESS_TEST_PORT to its reachable TCP port}"

ORCH_ROOT="${ORCH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
TARIT="${TARIT:-$ORCH_ROOT/target/debug/taritd}"
RUN_DIR="$TARITD_HOME/egress-recovery"
ROOTFS="$RUN_DIR/rootfs.ext4"
SERVER_LOG="$RUN_DIR/taritd.log"
SERVER_PID=
VM_A=
VM_B=
VM_C=
IPV6_FORWARDING=
IPV6_FORWARDING_CHANGED=0

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

for command in curl ip nft python3 sysctl timeout; do
  command -v "$command" >/dev/null || fail "required host command is missing: $command"
done
[ -x "$TARIT" ] || fail "taritd binary is not executable: $TARIT"
[ -x "$VMM_ROOT/target/debug/vmm" ] || fail "VMM binary is not executable"
[ -r "$BASE_ROOTFS" ] || fail "BASE_ROOTFS is not readable: $BASE_ROOTFS"
[ -r "$TARIT_KERNEL" ] || fail "TARIT_KERNEL is not readable: $TARIT_KERNEL"
python3 - "$EGRESS_TEST_IP" "$EGRESS_TEST_PORT" <<'PY'
import ipaddress
import sys

ipaddress.IPv4Address(sys.argv[1])
port = int(sys.argv[2])
if not 1 <= port <= 65535:
    raise ValueError("port out of range")
PY

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

start_taritd() {
  "$TARIT" serve >>"$SERVER_LOG" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 40); do
    curl -fsS "$TARIT_BASE_URL/health" >/dev/null 2>&1 && return
    sleep 1
  done
  cat "$SERVER_LOG" >&2
  fail "taritd did not become healthy"
}

stop_taritd() {
  [ -n "$SERVER_PID" ] || return
  kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=
}

delete_vm() {
  local vm_id=$1
  [ -n "$vm_id" ] || return
  [ -n "$SERVER_PID" ] || return
  "$TARIT" vm delete "$vm_id" >/dev/null
}

cleanup() {
  delete_vm "$VM_C" || true
  delete_vm "$VM_B" || true
  delete_vm "$VM_A" || true
  stop_taritd
  if [ "$IPV6_FORWARDING_CHANGED" = 1 ]; then
    sysctl -qw "net.ipv6.conf.all.forwarding=$IPV6_FORWARDING" || true
  fi
  rm -rf "$RUN_DIR"
}
trap cleanup EXIT

create_vm() {
  "$TARIT" --json vm create --vcpus 1 --memory-mib 256 |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])'
}

tap_for_vm() {
  local vm_id=$1 tap
  for _ in $(seq 1 20); do
    tap=$(nft -a list chain ip taritd_nat post |
      sed -n "s/.*vm=$vm_id tap=\\([^\" ]*\\).*/\\1/p" | head -1)
    [ -n "$tap" ] && { printf '%s\n' "$tap"; return; }
    sleep 1
  done
  fail "no TAP allocation for $vm_id"
}

guest_ip_for_tap() {
  local tap=$1 slot base
  slot=${tap#insta}
  [ "$slot" != "$tap" ] && [ "$slot" -ge 0 ] 2>/dev/null ||
    fail "invalid TAP name: $tap"
  base=$((slot * 4))
  printf '172.16.%s.%s\n' "$((base / 256))" "$((base % 256 + 2))"
}

run_guest() {
  local vm_id=$1 command=$2 reply
  reply=$("$TARIT" --json exec "$vm_id" "$command") ||
    fail "guest command could not be executed for $vm_id: $command"
  GUEST_STATUS=$(printf '%s' "$reply" | python3 -c '
import json, sys
record = json.load(sys.stdin)
status = record.get("exit_code")
if status is None:
    raise SystemExit("missing guest exit code: " + json.dumps(record))
print(status)
')
}

expect_guest_success() {
  run_guest "$1" "$2"
  [ "$GUEST_STATUS" = 0 ] ||
    fail "guest command unexpectedly failed (status $GUEST_STATUS): $2"
}

expect_guest_denial() {
  run_guest "$1" "$2"
  [ "$GUEST_STATUS" != 0 ] ||
    fail "guest command unexpectedly succeeded: $2"
}

require_rule() {
  local text=$1 expected=$2
  grep -F -- "$expected" <<<"$text" >/dev/null ||
    fail "missing rule: $expected"
}

assert_recovered_policy() {
  local vm_id=$1 tap=$2 guest=$3 slot=${2#insta} netdev forward input nat
  local uplink=$4
  [ "$(sysctl -n "net.ipv6.conf.$tap.disable_ipv6")" = 1 ] ||
    fail "recovered TAP permits IPv6: $tap"
  netdev=$(nft list table netdev "taritd_ingress_$slot")
  require_rule "$netdev" "policy drop"
  require_rule "$netdev" "ether type arp accept"
  require_rule "$netdev" "ether type ip accept"
  forward=$(nft list chain ip taritd_nat vm_egress)
  require_rule "$forward" "iifname \"$tap\" ip saddr != $guest drop"
  require_rule "$forward" "iifname \"$tap\" ip saddr $guest ip daddr 172.16.0.0/16 drop"
  require_rule "$forward" "iifname \"$tap\" ip saddr $guest oifname != \"$uplink\" drop"
  input=$(nft list chain ip taritd_nat vm_input)
  require_rule "$input" "iifname \"$tap\" ip saddr != $guest drop"
  require_rule "$input" "iifname \"$tap\" ct state established,related accept"
  require_rule "$input" "iifname \"$tap\" ip drop"
  nat=$(nft list chain ip taritd_nat post)
  require_rule "$nat" "iifname \"$tap\" ip saddr $guest oifname \"$uplink\" masquerade"
  nft -a list table ip taritd_nat | grep -F "vm=$vm_id tap=$tap" >/dev/null ||
    fail "recovered allocation tags are absent for $vm_id"
}

mkdir -p "$TARIT_SOCKET_DIR"
rm -rf "$RUN_DIR"
mkdir -p "$TARIT_SOCKET_DIR"
: >"$TARIT_CONFIG"
cp -f "$BASE_ROOTFS" "$ROOTFS"

start_taritd
VM_A=$(create_vm)
VM_B=$(create_vm)
TAP_A=$(tap_for_vm "$VM_A")
TAP_B=$(tap_for_vm "$VM_B")
GUEST_A=$(guest_ip_for_tap "$TAP_A")
GUEST_B=$(guest_ip_for_tap "$TAP_B")
UPLINK=$(ip route get "$EGRESS_TEST_IP" |
  awk '/ dev / {for (i=1; i<=NF; i++) if ($i == "dev") {print $(i+1); exit}}')
UPLINK_IP=$(ip route get "$EGRESS_TEST_IP" |
  awk '/ src / {for (i=1; i<=NF; i++) if ($i == "src") {print $(i+1); exit}}')
[ -n "$UPLINK" ] && [ -n "$UPLINK_IP" ] || fail "could not determine default uplink and source IP"

# These guest utilities are required for the behavioral checks; do not treat a
# missing tool as a pass because that would skip the actual isolation gate.
for vm_id in "$VM_A" "$VM_B"; do
  expect_guest_success "$vm_id" 'command -v sh ip ping nc timeout'
done

# Keep listeners on guest B alive while guest A attempts lateral TCP and UDP.
expect_guest_success "$VM_B" 'rm -f /run/tarit-udp; nc -l -p 31337 >/dev/null 2>&1 &'
expect_guest_success "$VM_B" 'nc -u -l -p 31338 >/run/tarit-udp 2>&1 &'
sleep 1
expect_guest_denial "$VM_A" "timeout 3 nc -z -w 2 $GUEST_B 31337"
expect_guest_success "$VM_A" "printf denied | timeout 3 nc -u -w 2 $GUEST_B 31338 || true"
sleep 1
expect_guest_success "$VM_B" '[ ! -s /run/tarit-udp ]'
expect_guest_denial "$VM_A" "ping -c 1 -W 2 $GUEST_B"

# The source guard must reject a forged address before forwarding it.
expect_guest_success "$VM_A" 'ip addr add 172.16.255.250/32 dev eth0'
expect_guest_denial "$VM_A" "ping -I 172.16.255.250 -c 1 -W 2 $EGRESS_TEST_IP"
expect_guest_success "$VM_A" 'ip addr del 172.16.255.250/32 dev eth0'

# IPv6 remains disabled even if the host is explicitly configured to forward it.
IPV6_FORWARDING=$(sysctl -n net.ipv6.conf.all.forwarding)
IPV6_FORWARDING_CHANGED=1
sysctl -qw net.ipv6.conf.all.forwarding=1
expect_guest_denial "$VM_A" 'ip -6 addr add 2001:db8::7/64 dev eth0'

# A guest cannot initiate host-local traffic through its TAP.
expect_guest_denial "$VM_A" "ping -c 1 -W 2 $UPLINK_IP"

# Permit one external TCP destination and prove a connection plus its return
# traffic. EGRESS_TEST_IP is an externally supplied real endpoint, not localhost.
for vm_id in "$VM_A" "$VM_B"; do
  curl -fsS -X PATCH \
    -H "X-API-Key: $TARIT_API_KEY" \
    -H 'Content-Type: application/json' \
    -d "{\"allowlist\":[\"$EGRESS_TEST_IP/32:$EGRESS_TEST_PORT/tcp\"],\"allow_existing\":true}" \
    "$TARIT_BASE_URL/v1/egress/vm/$vm_id" >/dev/null
  expect_guest_success "$vm_id" "timeout 8 nc -z -w 5 $EGRESS_TEST_IP $EGRESS_TEST_PORT"
done

stop_taritd
start_taritd
assert_recovered_policy "$VM_A" "$TAP_A" "$GUEST_A" "$UPLINK"
assert_recovered_policy "$VM_B" "$TAP_B" "$GUEST_B" "$UPLINK"
expect_guest_success "$VM_A" "timeout 8 nc -z -w 5 $EGRESS_TEST_IP $EGRESS_TEST_PORT"
expect_guest_success "$VM_B" "timeout 8 nc -z -w 5 $EGRESS_TEST_IP $EGRESS_TEST_PORT"

delete_vm "$VM_A"
VM_A=
! ip link show "$TAP_A" >/dev/null 2>&1 || fail "deleted TAP leaked: $TAP_A"
! nft list table netdev "taritd_ingress_${TAP_A#insta}" >/dev/null 2>&1 ||
  fail "deleted ingress table leaked: $TAP_A"
! nft -a list table ip taritd_nat | grep -F "tap=$TAP_A" >/dev/null ||
  fail "deleted nft rules leaked: $TAP_A"

VM_C=$(create_vm)
TAP_C=$(tap_for_vm "$VM_C")
[ "$TAP_C" = "$TAP_A" ] || fail "slot was not reused: expected $TAP_A, got $TAP_C"

echo "RESULT: EGRESS_RECOVERY_PASS"
