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
RUN_ROOT="$TARITD_HOME/egress-recovery-runs"
LOCK_FILE=/run/lock/taritd-egress-recovery.lock
RUN_DIR=
ROOTFS="$RUN_DIR/rootfs.ext4"
SERVER_LOG="$RUN_DIR/taritd.log"
SERVER_PID=
SERVER_START_TICKS=
VM_A=
VM_B=
VM_C=
TAP_A=
TAP_B=
TAP_C=
IPV6_FORWARDING=
IPV6_FORWARDING_CHANGED=0
IPV4_FORWARDING=
IPV4_FORWARDING_CHANGED=0

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

for command in curl flock ip nft ps python3 ss stat sysctl timeout; do
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

umask 077
mkdir -p "$RUN_ROOT"
mkdir -p /run/lock
touch "$LOCK_FILE"
chown root:root "$LOCK_FILE"
chmod 0600 "$LOCK_FILE"
[ "$(stat -c '%u:%a' "$LOCK_FILE")" = "0:600" ] ||
  fail "global lock is not root-owned mode 0600: $LOCK_FILE"
exec 9>"$LOCK_FILE"
flock -n 9 || fail "another egress recovery run already holds $LOCK_FILE"
RUN_DIR="$RUN_ROOT/run-$(date +%s)-$$-$RANDOM"
mkdir "$RUN_DIR" || fail "could not create run directory $RUN_DIR"
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

server_pid_is_current() {
  [ -n "$SERVER_PID" ] &&
    [ -n "$SERVER_START_TICKS" ] &&
    kill -0 "$SERVER_PID" 2>/dev/null &&
    [ -r "/proc/$SERVER_PID/stat" ] &&
    [ "$(awk '{print $22}' "/proc/$SERVER_PID/stat")" = "$SERVER_START_TICKS" ]
}

server_listener_is_current() {
  server_pid_is_current &&
    ss -lntp "sport = :8080" 2>/dev/null |
      grep -F "127.0.0.1:8080" |
      grep -F "pid=$SERVER_PID," >/dev/null
}

start_taritd() {
  "$TARIT" serve >>"$SERVER_LOG" 2>&1 &
  SERVER_PID=$!
  SERVER_START_TICKS=$(awk '{print $22}' "/proc/$SERVER_PID/stat") ||
    fail "could not read launched taritd start time"
  for _ in $(seq 1 40); do
    server_listener_is_current &&
      curl -fsS "$TARIT_BASE_URL/health" >/dev/null 2>&1 &&
      return
    sleep 1
  done
  cat "$SERVER_LOG" >&2
  fail "launched taritd did not remain healthy"
}

stop_taritd() {
  [ -n "$SERVER_PID" ] || return
  if server_pid_is_current; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  SERVER_PID=
  SERVER_START_TICKS=
}

delete_vm() {
  local vm_id=$1
  [ -n "$vm_id" ] || return
  server_pid_is_current || return 1
  "$TARIT" vm delete "$vm_id" >/dev/null
}

cleanup_run_vmm_processes() {
  [ -n "$RUN_DIR" ] || return
  ps -eo pid=,args= |
    awk -v sockets="$RUN_DIR/sockets/" 'index($0, sockets) { print $1 }' |
    while IFS= read -r pid; do
      [ -n "$pid" ] && [ "$pid" != "$$" ] && kill "$pid" 2>/dev/null || true
    done
}

cleanup_tap_policy() {
  local tap=$1 slot chain handle
  [ -n "$tap" ] || return
  slot=${tap#insta}
  [ "$slot" != "$tap" ] && [ "$slot" -ge 0 ] 2>/dev/null || return
  for chain in post vm_egress vm_input; do
    while IFS= read -r handle; do
      [ -n "$handle" ] &&
        nft delete rule ip taritd_nat "$chain" handle "$handle" >/dev/null 2>&1 || true
    done < <(
      nft -a list chain ip taritd_nat "$chain" 2>/dev/null |
        awk -v tap="$tap" '
          index($0, "comment \"taritd") &&
          index($0, "tap=" tap "\"") &&
          match($0, /# handle [0-9]+$/) {
            print substr($0, RSTART + 9, RLENGTH - 9)
          }'
    )
  done
  nft delete table netdev "taritd_ingress_$slot" >/dev/null 2>&1 || true
  ip link del "$tap" >/dev/null 2>&1 || true
}

cleanup() {
  delete_vm "$VM_C" || true
  delete_vm "$VM_B" || true
  delete_vm "$VM_A" || true
  cleanup_run_vmm_processes
  cleanup_tap_policy "$TAP_C"
  cleanup_tap_policy "$TAP_B"
  cleanup_tap_policy "$TAP_A"
  stop_taritd
  if [ "$IPV6_FORWARDING_CHANGED" = 1 ]; then
    sysctl -qw "net.ipv6.conf.all.forwarding=$IPV6_FORWARDING" || true
  fi
  if [ "$IPV4_FORWARDING_CHANGED" = 1 ]; then
    sysctl -qw "net.ipv4.ip_forward=$IPV4_FORWARDING" || true
  fi
  [ -n "$RUN_DIR" ] && rm -rf -- "$RUN_DIR"
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

wait_for_guest_listener() {
  local vm_id=$1 protocol=$2 port=$3 pid_file=$4 socket_check
  case "$protocol" in
    tcp) socket_check='ss -ltn' ;;
    udp) socket_check='ss -lun' ;;
    *) fail "unsupported listener protocol: $protocol" ;;
  esac
  for _ in $(seq 1 20); do
    run_guest "$vm_id" "pid=\$(cat $pid_file 2>/dev/null) && [ -n \"\$pid\" ] && kill -0 \"\$pid\" && $socket_check | grep -Eq ':$port([[:space:]]|$)'"
    [ "$GUEST_STATUS" = 0 ] && return
    sleep 1
  done
  fail "$protocol listener on guest $vm_id port $port was not ready"
}

wait_for_host_ipv6_address() {
  local tap=$1 address=$2
  for _ in $(seq 1 20); do
    ip -6 addr show dev "$tap" scope global |
      grep -F "$address/" |
      grep -vq tentative &&
      return
    sleep 1
  done
  fail "IPv6 address $address on $tap did not complete DAD"
}

wait_for_guest_ipv6_address() {
  local vm_id=$1 address=$2
  for _ in $(seq 1 20); do
    run_guest "$vm_id" "ip -6 addr show dev eth0 scope global | grep -F '$address/' | grep -vq tentative"
    [ "$GUEST_STATUS" = 0 ] && return
    sleep 1
  done
  fail "guest $vm_id IPv6 address $address did not complete DAD"
}

require_rule() {
  local text=$1 expected=$2
  grep -F -- "$expected" <<<"$text" >/dev/null ||
    fail "missing rule: $expected"
}

normalize_nft_counter() {
  local rule=$1
  if [[ "$rule" =~ ^(.*)\ counter\ packets\ ([0-9]+)\ bytes\ ([0-9]+)\ drop$ ]]; then
    printf '%s counter drop\n' "${BASH_REMATCH[1]}"
  else
    printf '%s\n' "$rule"
  fi
}

assert_closed_tarit_chain_for_tap() {
  local listing=$1 chain=$2 tap=$3 vm_id=$4 guest=$5 uplink=$6 line rule comment
  while IFS= read -r line; do
    [[ "$line" == *"type filter"* || "$line" == "table "* || "$line" == "chain "* ||
      "$line" == "}" ]] && continue
    [[ "$line" == *" accept"* && "$line" != *'iifname "'* ]] &&
      fail "broad accept lacks an exact TAP matcher in $chain: $line"
    [[ "$line" == *"iifname \"$tap\""* ]] || continue
    rule=${line%% comment \"*}
    comment=${line#* comment \"}
    comment=${comment%%\"*}
    rule=$(normalize_nft_counter "$rule")
    case "$chain:$rule:$comment" in
      "vm_egress:iifname \"$tap\" ip saddr != $guest counter drop:taritd-guard slot=${tap#insta} vm=$vm_id tap=$tap"|\
      "vm_egress:iifname \"$tap\" ip saddr $guest ip daddr 172.16.0.0/16 drop:taritd-guard slot=${tap#insta} vm=$vm_id tap=$tap"|\
      "vm_egress:iifname \"$tap\" ip saddr $guest oifname != \"$uplink\" drop:taritd-guard slot=${tap#insta} vm=$vm_id tap=$tap"|\
      "vm_egress:iifname \"$tap\" ip saddr $guest ct state established,related accept:taritd-egress slot=${tap#insta} vm=$vm_id tap=$tap"|\
      "vm_egress:iifname \"$tap\" ip saddr $guest ip daddr $EGRESS_TEST_IP/32 tcp dport $EGRESS_TEST_PORT accept:taritd-egress slot=${tap#insta} vm=$vm_id tap=$tap"|\
      "vm_egress:iifname \"$tap\" ip saddr $guest drop:taritd-egress slot=${tap#insta} vm=$vm_id tap=$tap"|\
      "vm_input:iifname \"$tap\" ip saddr != $guest counter drop:taritd-input slot=${tap#insta} vm=$vm_id tap=$tap"|\
      "vm_input:iifname \"$tap\" ct state established,related accept:taritd-input slot=${tap#insta} vm=$vm_id tap=$tap"|\
      "vm_input:iifname \"$tap\" ip drop:taritd-input slot=${tap#insta} vm=$vm_id tap=$tap")
        ;;
      *)
        fail "unmanaged or unrecognized Tarit rule shape in $chain for $tap: $line"
        ;;
    esac
  done <<<"$listing"
}

assert_egress_rule_order() {
  local listing=$1 tap=$2 guest=$3 vm_id=$4 source lateral uplink stateful allow deny
  source=$(grep -nF \
    "iifname \"$tap\" ip saddr != $guest counter" \
    <<<"$listing" | head -1 | cut -d: -f1)
  lateral=$(grep -nF \
    "iifname \"$tap\" ip saddr $guest ip daddr 172.16.0.0/16 drop" \
    <<<"$listing" | head -1 | cut -d: -f1)
  uplink=$(grep -nF \
    "iifname \"$tap\" ip saddr $guest oifname !=" \
    <<<"$listing" | head -1 | cut -d: -f1)
  stateful=$(grep -nF \
    "iifname \"$tap\" ip saddr $guest ct state established,related accept comment \"taritd-egress slot=${tap#insta} vm=$vm_id tap=$tap\"" \
    <<<"$listing" | head -1 | cut -d: -f1)
  allow=$(grep -nF \
    "iifname \"$tap\" ip saddr $guest ip daddr $EGRESS_TEST_IP/32 tcp dport $EGRESS_TEST_PORT accept comment \"taritd-egress slot=${tap#insta} vm=$vm_id tap=$tap\"" \
    <<<"$listing" | head -1 | cut -d: -f1)
  deny=$(grep -nF \
    "iifname \"$tap\" ip saddr $guest drop comment \"taritd-egress slot=${tap#insta} vm=$vm_id tap=$tap\"" \
    <<<"$listing" | tail -1 | cut -d: -f1)
  [ -n "$source" ] && [ -n "$lateral" ] && [ -n "$uplink" ] &&
    [ -n "$stateful" ] && [ -n "$allow" ] && [ -n "$deny" ] ||
    fail "missing guard, stateful-return, allow, or final default-deny for $tap"
  [ "$source" -lt "$stateful" ] && [ "$lateral" -lt "$stateful" ] &&
    [ "$uplink" -lt "$stateful" ] && [ "$stateful" -lt "$allow" ] &&
    [ "$allow" -lt "$deny" ] ||
    fail "default-open or misordered egress policy for $tap"
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
  forward=$(nft -a list chain ip taritd_nat vm_egress)
  assert_closed_tarit_chain_for_tap "$forward" vm_egress "$tap" "$vm_id" "$guest" "$uplink"
  require_rule "$forward" "iifname \"$tap\" ip saddr != $guest counter"
  require_rule "$forward" "iifname \"$tap\" ip saddr $guest ip daddr 172.16.0.0/16 drop"
  require_rule "$forward" "iifname \"$tap\" ip saddr $guest oifname != \"$uplink\" drop"
  assert_egress_rule_order "$forward" "$tap" "$guest" "$vm_id"
  input=$(nft -a list chain ip taritd_nat vm_input)
  assert_closed_tarit_chain_for_tap "$input" vm_input "$tap" "$vm_id" "$guest" "$uplink"
  require_rule "$input" "iifname \"$tap\" ip saddr != $guest counter"
  require_rule "$input" "iifname \"$tap\" ct state established,related accept"
  require_rule "$input" "iifname \"$tap\" ip drop"
  nat=$(nft list chain ip taritd_nat post)
  require_rule "$nat" "iifname \"$tap\" ip saddr $guest oifname \"$uplink\" masquerade"
  nft -a list table ip taritd_nat | grep -F "vm=$vm_id tap=$tap" >/dev/null ||
    fail "recovered allocation tags are absent for $vm_id"
}

forged_source_drop_packets() {
  local tap=$1 guest=$2 packets
  packets=$(nft -a list chain ip taritd_nat vm_egress |
    awk -v tap="$tap" -v guest="$guest" '
      $0 ~ ("iifname \"" tap "\"") && $0 ~ ("ip saddr != " guest) {
        for (i = 1; i <= NF; i++) {
          if ($(i - 1) == "counter" && $i == "packets") {
            print $(i + 1)
            exit
          }
        }
      }')
  [ -n "$packets" ] && [ "$packets" -ge 0 ] 2>/dev/null ||
    fail "source-guard counter is unavailable for $tap"
  printf '%s\n' "$packets"
}

mkdir -p "$TARIT_SOCKET_DIR"
: >"$TARIT_CONFIG"
cp -f "$BASE_ROOTFS" "$ROOTFS"
IPV4_FORWARDING=$(sysctl -n net.ipv4.ip_forward)
IPV4_FORWARDING_CHANGED=1

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
  expect_guest_success "$vm_id" 'command -v sh ip ping nc ss sysctl timeout'
done

# Keep listeners on guest B alive while guest A attempts lateral TCP and UDP.
expect_guest_success "$VM_B" 'rm -f /run/tarit-tcp.pid /run/tarit-udp.pid /run/tarit-udp; (while :; do nc -l -p 31337 >/dev/null 2>&1; done) & tcp_pid=$!; echo "$tcp_pid" >/run/tarit-tcp.pid; nc -u -l -p 31338 >/run/tarit-udp 2>&1 & udp_pid=$!; echo "$udp_pid" >/run/tarit-udp.pid; kill -0 "$tcp_pid" && kill -0 "$udp_pid"'
wait_for_guest_listener "$VM_B" tcp 31337 /run/tarit-tcp.pid
wait_for_guest_listener "$VM_B" udp 31338 /run/tarit-udp.pid
expect_guest_denial "$VM_A" "timeout 3 nc -z -w 2 $GUEST_B 31337"
expect_guest_success "$VM_A" "printf denied | timeout 3 nc -u -w 2 $GUEST_B 31338 || true"
wait_for_guest_listener "$VM_B" tcp 31337 /run/tarit-tcp.pid
wait_for_guest_listener "$VM_B" udp 31338 /run/tarit-udp.pid
expect_guest_success "$VM_B" '[ ! -s /run/tarit-udp ]'
expect_guest_denial "$VM_A" "ping -c 1 -W 2 $GUEST_B"

# The source guard counter proves a forged packet is dropped in the host
# forwarding path even though the forged source cannot have a return route.
expect_guest_success "$VM_A" 'ip addr add 172.16.255.250/32 dev eth0'
FORGED_DROPS_BEFORE=$(forged_source_drop_packets "$TAP_A" "$GUEST_A")
expect_guest_denial "$VM_A" "ping -I 172.16.255.250 -c 1 -W 2 $EGRESS_TEST_IP"
FORGED_DROPS_AFTER=$(forged_source_drop_packets "$TAP_A" "$GUEST_A")
[ "$FORGED_DROPS_AFTER" -gt "$FORGED_DROPS_BEFORE" ] ||
  fail "forged-source packet did not increment the host source-guard counter"
expect_guest_success "$VM_A" 'ip addr del 172.16.255.250/32 dev eth0'

# Configure distinct routed IPv6 prefixes for both guests while host forwarding
# is forced on. Without the netdev ingress guard, guest A can route through the
# host to guest B.
IPV6_FORWARDING=$(sysctl -n net.ipv6.conf.all.forwarding)
IPV6_FORWARDING_CHANGED=1
sysctl -qw net.ipv6.conf.all.forwarding=1 ||
  fail "could not force host IPv6 forwarding"
IPV6_PREFIX_A=2001:db8:7:1::/64
IPV6_PREFIX_B=2001:db8:7:2::/64
IPV6_HOST_A=2001:db8:7:1::1
IPV6_GUEST_A=2001:db8:7:1::2
IPV6_HOST_B=2001:db8:7:2::1
IPV6_GUEST_B=2001:db8:7:2::2
for tap in "$TAP_A" "$TAP_B"; do
  sysctl -qw "net.ipv6.conf.$tap.disable_ipv6=0" ||
    fail "could not enable IPv6 on $tap for the forwarding-path test"
  sysctl -qw "net.ipv6.conf.$tap.forwarding=1" ||
    fail "could not enable IPv6 forwarding on $tap"
  sysctl -qw "net.ipv6.conf.$tap.accept_ra=0" ||
    fail "could not disable IPv6 router advertisements on $tap"
done
ip -6 addr replace "$IPV6_HOST_A/64" dev "$TAP_A" ||
  fail "could not configure $IPV6_HOST_A on $TAP_A"
ip -6 addr replace "$IPV6_HOST_B/64" dev "$TAP_B" ||
  fail "could not configure $IPV6_HOST_B on $TAP_B"
wait_for_host_ipv6_address "$TAP_A" "$IPV6_HOST_A"
wait_for_host_ipv6_address "$TAP_B" "$IPV6_HOST_B"

for vm_id in "$VM_A" "$VM_B"; do
  expect_guest_success "$vm_id" 'sysctl -qw net.ipv6.conf.all.disable_ipv6=0'
  expect_guest_success "$vm_id" 'sysctl -qw net.ipv6.conf.eth0.disable_ipv6=0'
  expect_guest_success "$vm_id" 'sysctl -qw net.ipv6.conf.eth0.accept_ra=0'
done
expect_guest_success "$VM_A" "ip -6 addr replace $IPV6_GUEST_A/64 dev eth0"
expect_guest_success "$VM_B" "ip -6 addr replace $IPV6_GUEST_B/64 dev eth0"
wait_for_guest_ipv6_address "$VM_A" "$IPV6_GUEST_A"
wait_for_guest_ipv6_address "$VM_B" "$IPV6_GUEST_B"
expect_guest_success "$VM_A" "ip -6 route replace $IPV6_PREFIX_B via $IPV6_HOST_A dev eth0"
expect_guest_success "$VM_B" "ip -6 route replace $IPV6_PREFIX_A via $IPV6_HOST_B dev eth0"
expect_guest_success "$VM_A" "ip -6 route get $IPV6_GUEST_B | grep -F 'via $IPV6_HOST_A dev eth0'"
expect_guest_success "$VM_B" "ip -6 route get $IPV6_GUEST_A | grep -F 'via $IPV6_HOST_B dev eth0'"
ip -6 route get "$IPV6_GUEST_B" from "$IPV6_HOST_A" |
  grep -F "dev $TAP_B" >/dev/null ||
  fail "host lacks a routed IPv6 path from $TAP_A to $TAP_B"
nft list table netdev "taritd_ingress_${TAP_A#insta}" |
  grep -F "policy drop" >/dev/null ||
  fail "missing IPv6 netdev default-deny on $TAP_A"
nft list table netdev "taritd_ingress_${TAP_B#insta}" |
  grep -F "policy drop" >/dev/null ||
  fail "missing IPv6 netdev default-deny on $TAP_B"
expect_guest_denial "$VM_A" "ping -6 -I eth0 -c 1 -W 3 $IPV6_GUEST_B"

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

RESTART_SENTINEL=198.18.0.1
nft add rule ip taritd_nat vm_egress \
  iifname "$TAP_A" ip saddr "$GUEST_A" ip daddr "$RESTART_SENTINEL" drop \
  comment "taritd-egress slot=${TAP_A#insta} vm=$VM_A tap=$TAP_A"

stop_taritd
start_taritd
assert_recovered_policy "$VM_A" "$TAP_A" "$GUEST_A" "$UPLINK"
assert_recovered_policy "$VM_B" "$TAP_B" "$GUEST_B" "$UPLINK"
! nft -a list chain ip taritd_nat vm_egress |
  grep -F "ip daddr $RESTART_SENTINEL" >/dev/null ||
  fail "restart left the prior-owner sentinel policy installed"
expect_guest_success "$VM_A" "timeout 8 nc -z -w 5 $EGRESS_TEST_IP $EGRESS_TEST_PORT"
expect_guest_success "$VM_B" "timeout 8 nc -z -w 5 $EGRESS_TEST_IP $EGRESS_TEST_PORT"

delete_vm "$VM_A"
OLD_VM_A=$VM_A
VM_A=
! ip link show "$TAP_A" >/dev/null 2>&1 || fail "deleted TAP leaked: $TAP_A"
! nft list table netdev "taritd_ingress_${TAP_A#insta}" >/dev/null 2>&1 ||
  fail "deleted ingress table leaked: $TAP_A"
! nft -a list table ip taritd_nat | grep -F "tap=$TAP_A" >/dev/null ||
  fail "deleted nft rules leaked: $TAP_A"

VM_C=$(create_vm)
TAP_C=$(tap_for_vm "$VM_C")
[ "$TAP_C" = "$TAP_A" ] || fail "slot was not reused: expected $TAP_A, got $TAP_C"
GUEST_C=$(guest_ip_for_tap "$TAP_C")
curl -fsS -X PATCH \
  -H "X-API-Key: $TARIT_API_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"allowlist\":[\"$EGRESS_TEST_IP/32:$EGRESS_TEST_PORT/tcp\"],\"allow_existing\":true}" \
  "$TARIT_BASE_URL/v1/egress/vm/$VM_C" >/dev/null
assert_recovered_policy "$VM_C" "$TAP_C" "$GUEST_C" "$UPLINK"
! nft -a list table ip taritd_nat | grep -F "vm=$OLD_VM_A tap=$TAP_C" >/dev/null ||
  fail "slot reuse retained prior-owner policy for $OLD_VM_A"

echo "RESULT: EGRESS_RECOVERY_PASS"
