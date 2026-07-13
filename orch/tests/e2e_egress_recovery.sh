#!/usr/bin/env bash
# Validate fail-closed egress guards with two real guests on a Linux KVM host.
set -euo pipefail

ORCH_ROOT="${ORCH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARIT="${TARIT:-$ORCH_ROOT/target/debug/taritd}"
TRUSTED_RUN_BASE=/run/taritd
RUN_ROOT="${EGRESS_RECOVERY_RUN_ROOT:-$TRUSTED_RUN_BASE/egress-recovery}"
LOCK_FILE=/run/lock/taritd-egress-recovery.lock
RUN_DIR=
RUN_DIR_CREATED=0
ROOTFS=
SERVER_LOG=
SERVER_PID=
SERVER_START_TICKS=
SERVER_STOP_TIMEOUT_SECONDS=20
LINUX_UNIX_SOCKET_PATH_LIMIT=108
MAX_VMM_ID=00000000-0000-0000-0000-000000000000
VM_A=
VM_B=
VM_C=
TAP_A=
TAP_B=
TAP_C=
IPV6_FORWARDING=
IPV4_FORWARDING=
VM_IDS=()
VM_TAPS=()
VMM_PIDS=()
VMM_START_TICKS=()
VMM_EXIT_CONFIRMED=()

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

assert_vmm_socket_path_fits() {
  local run_dir=$1 socket_path socket_path_bytes
  socket_path="$run_dir/sockets/$MAX_VMM_ID.sock"
  socket_path_bytes=$(LC_ALL=C printf '%s' "$socket_path" | LC_ALL=C wc -c) ||
    fail "could not measure VMM socket path length"
  [ "$socket_path_bytes" -lt "$LINUX_UNIX_SOCKET_PATH_LIMIT" ] ||
    fail "VMM socket path exceeds Linux sun_path limit (${socket_path_bytes} >= $LINUX_UNIX_SOCKET_PATH_LIMIT): $socket_path"
}

run_root_path_is_valid() {
  local path=$1
  case "$path" in
    "$TRUSTED_RUN_BASE"/*)
      case "$path" in
        */ | *'//'* | *'/./'* | *'/../'* | */. | */..) return 1 ;;
      esac
      return 0
      ;;
  esac
  return 1
}

run_root_path_has_control_characters() {
  [[ $1 == *[[:cntrl:]]* ]]
}

trusted_directory() {
  local path=$1 file_type owner mode
  [ -d "$path" ] && [ ! -L "$path" ] || return 1
  IFS=: read -r file_type owner mode < <(stat -c '%F:%u:%a' -- "$path") || return 1
  [ "$file_type" = directory ] &&
    [ "$owner" = 0 ] &&
    (( (8#$mode & 8#022) == 0 ))
}

private_root_directory() {
  trusted_directory "$1" &&
    [ "$(stat -c '%u:%a' -- "$1")" = "0:700" ]
}

bootstrap_trusted_run_base() {
  trusted_directory /run ||
    fail "/run is not a root-owned non-writable directory"

  if [ -e "$TRUSTED_RUN_BASE" ] || [ -L "$TRUSTED_RUN_BASE" ]; then
    trusted_directory "$TRUSTED_RUN_BASE" ||
      fail "trusted run base is not a root-owned non-writable directory: $TRUSTED_RUN_BASE"
  else
    mkdir -m 0700 -- "$TRUSTED_RUN_BASE" ||
      fail "could not create trusted run base $TRUSTED_RUN_BASE"
  fi

  chown root:root -- "$TRUSTED_RUN_BASE" ||
    fail "could not make trusted run base root-owned: $TRUSTED_RUN_BASE"
  chmod 0700 -- "$TRUSTED_RUN_BASE" ||
    fail "could not restrict trusted run base permissions: $TRUSTED_RUN_BASE"
  private_root_directory "$TRUSTED_RUN_BASE" ||
    fail "trusted run base is not a root-owned mode 0700 directory: $TRUSTED_RUN_BASE"
}

run_root_ancestry_is_trusted() {
  local run_root=$1 relative_path current part
  local -a components

  run_root_path_is_valid "$run_root" || return 1
  trusted_directory / &&
    trusted_directory /run &&
    private_root_directory "$TRUSTED_RUN_BASE" || return 1

  relative_path=${run_root#"$TRUSTED_RUN_BASE"/}
  IFS=/ read -r -a components <<<"$relative_path"
  current=$TRUSTED_RUN_BASE
  for ((part = 0; part < ${#components[@]} - 1; part++)); do
    current+="/${components[$part]}"
    trusted_directory "$current" || return 1
  done
}

validate_run_root_path() {
  run_root_path_has_control_characters "$1" &&
    fail "run root must not contain control characters"
  run_root_path_is_valid "$1" ||
    fail "run root must be a normalized child of $TRUSTED_RUN_BASE: $1"
}

validate_run_root_ancestry() {
  run_root_ancestry_is_trusted "$1" ||
    fail "run root has untrusted ancestry: $1"
}

prepare_run_root() {
  validate_run_root_path "$RUN_ROOT"
  bootstrap_trusted_run_base
  validate_run_root_ancestry "$RUN_ROOT"
  if [ -e "$RUN_ROOT" ] || [ -L "$RUN_ROOT" ]; then
    trusted_directory "$RUN_ROOT" ||
      fail "run root is not a root-owned non-writable directory: $RUN_ROOT"
  else
    mkdir -m 0700 -- "$RUN_ROOT" || fail "could not create run root $RUN_ROOT"
    trusted_directory "$RUN_ROOT" ||
      fail "created run root is not a root-owned non-writable directory: $RUN_ROOT"
  fi
}

run_dir_is_owned() {
  [ "$RUN_DIR_CREATED" -eq 1 ] &&
    run_root_ancestry_is_trusted "$RUN_ROOT" &&
    trusted_directory "$RUN_ROOT" &&
    [ -d "$RUN_DIR" ] &&
    [ ! -L "$RUN_DIR" ] &&
    [ "$(stat -c '%u:%a' "$RUN_DIR")" = "0:700" ]
}

recorded_process_stat() {
  awk '{print $3, $22}' "/proc/$1/stat" 2>/dev/null
}

recorded_process_is_current() {
  local pid=$1 start_ticks=$2 state current_ticks
  [ -n "$pid" ] && [ -n "$start_ticks" ] &&
    kill -0 "$pid" 2>/dev/null &&
    read -r state current_ticks < <(recorded_process_stat "$pid") &&
    [ "$current_ticks" = "$start_ticks" ]
}

recorded_process_exit_confirmed() {
  local pid=$1 start_ticks=$2 state current_ticks
  [ -n "$pid" ] && [ -n "$start_ticks" ] || return 2
  [ -e "/proc/$pid" ] || return 0
  if [ ! -r "/proc/$pid/stat" ]; then
    [ -e "/proc/$pid" ] && return 2
    return 0
  fi
  if ! read -r state current_ticks < <(recorded_process_stat "$pid"); then
    [ -e "/proc/$pid" ] && return 2
    return 0
  fi
  case "$state:$current_ticks" in
    [A-Za-z]:[0-9]*) ;;
    *) return 2 ;;
  esac
  [ "$current_ticks" != "$start_ticks" ] && return 0
  [ "$state" = Z ]
}

check_server_exit_race() {
  local child_pid status attempt
  [ -r /proc/self/stat ] || {
    echo "SKIP: server exit race check requires Linux procfs" >&2
    return 0
  }

  SERVER_EXIT_RACE_CHILD_PID=
  SERVER_EXIT_RACE_CHECK_PID=
  SERVER_EXIT_RACE_DIR=$(mktemp -d "$SCRIPT_DIR/.egress-recovery-exit-race.XXXXXX") ||
    return 1
  SERVER_EXIT_RACE_READY_FILE="$SERVER_EXIT_RACE_DIR/stat-read-ready"
  SERVER_EXIT_RACE_RELEASE_FILE="$SERVER_EXIT_RACE_DIR/stat-read-release"
  trap check_server_exit_race_cleanup EXIT
  trap 'exit 1' INT TERM

  sleep 30 &
  SERVER_EXIT_RACE_CHILD_PID=$!
  child_pid=$SERVER_EXIT_RACE_CHILD_PID
  SERVER_START_TICKS=$(command awk '{print $22}' "/proc/$SERVER_EXIT_RACE_CHILD_PID/stat") ||
    return 1

  awk() {
    if [ "$1" = "{print \$3, \$22}" ] &&
      [ "${2:-}" = "/proc/$SERVER_EXIT_RACE_CHILD_PID/stat" ]; then
      : > "$SERVER_EXIT_RACE_READY_FILE"
      while [ ! -e "$SERVER_EXIT_RACE_RELEASE_FILE" ]; do
        sleep 0.01
      done
      [ ! -e "/proc/$SERVER_EXIT_RACE_CHILD_PID/stat" ] || return 1
    fi
    command awk "$@"
  }

  recorded_process_exit_confirmed "$SERVER_EXIT_RACE_CHILD_PID" "$SERVER_START_TICKS" &
  SERVER_EXIT_RACE_CHECK_PID=$!
  for ((attempt = 0; attempt < 500; attempt++)); do
    [ -e "$SERVER_EXIT_RACE_READY_FILE" ] && break
    sleep 0.01
  done
  if [ ! -e "$SERVER_EXIT_RACE_READY_FILE" ]; then
    echo "FAIL: process-exit race stat read did not start" >&2
    return 1
  fi

  kill -TERM -- "$SERVER_EXIT_RACE_CHILD_PID"
  wait "$SERVER_EXIT_RACE_CHILD_PID" 2>/dev/null || true
  SERVER_EXIT_RACE_CHILD_PID=
  : > "$SERVER_EXIT_RACE_RELEASE_FILE"
  if wait "$SERVER_EXIT_RACE_CHECK_PID"; then
    status=0
  else
    status=$?
  fi
  if [ "$status" -ne 0 ] || [ -e "/proc/$child_pid" ]; then
    echo "FAIL: disappeared recorded child was not confirmed as exited" >&2
    return 1
  fi

  SERVER_EXIT_RACE_CHECK_PID=
  check_server_exit_race_cleanup
  trap - EXIT INT TERM
}

check_server_exit_race_cleanup() {
  if [ -n "${SERVER_EXIT_RACE_CHILD_PID:-}" ]; then
    kill -TERM -- "$SERVER_EXIT_RACE_CHILD_PID" 2>/dev/null || true
    wait "$SERVER_EXIT_RACE_CHILD_PID" 2>/dev/null || true
    SERVER_EXIT_RACE_CHILD_PID=
  fi
  if [ -n "${SERVER_EXIT_RACE_RELEASE_FILE:-}" ]; then
    : > "$SERVER_EXIT_RACE_RELEASE_FILE"
  fi
  if [ -n "${SERVER_EXIT_RACE_CHECK_PID:-}" ]; then
    wait "$SERVER_EXIT_RACE_CHECK_PID" 2>/dev/null || true
    SERVER_EXIT_RACE_CHECK_PID=
  fi
  if [ -n "${SERVER_EXIT_RACE_DIR:-}" ]; then
    rm -rf -- "$SERVER_EXIT_RACE_DIR"
    SERVER_EXIT_RACE_DIR=
  fi
}

case "${1:-}" in
  --check-socket-path)
    assert_vmm_socket_path_fits "$RUN_ROOT/run-2147483647-4194304-32767"
    exit 0
    ;;
  --check-run-root)
    [ "$#" -eq 2 ] || fail "usage: $0 --check-run-root PATH"
    validate_run_root_path "$2"
    exit 0
    ;;
  --check-server-exit-race)
    check_server_exit_race
    exit $?
    ;;
esac

[ "$(id -u)" -eq 0 ] || { echo "FAIL: run as root"; exit 1; }
[ "$(uname -s)" = Linux ] || { echo "FAIL: run on Linux"; exit 1; }
: "${BASE_ROOTFS:?set BASE_ROOTFS to a bootable guest rootfs}"
: "${TARIT_KERNEL:?set TARIT_KERNEL to the guest kernel}"
: "${EGRESS_TEST_IP:?set EGRESS_TEST_IP to a reachable external IPv4 TCP endpoint}"
: "${EGRESS_TEST_PORT:?set EGRESS_TEST_PORT to its reachable TCP port}"

for command in curl flock ip nft python3 readlink ss stat sysctl timeout wc; do
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

IPV4_FORWARDING=$(sysctl -n net.ipv4.ip_forward) ||
  fail "could not capture original IPv4 forwarding value"
IPV6_FORWARDING=$(sysctl -n net.ipv6.conf.all.forwarding) ||
  fail "could not capture original IPv6 forwarding value"

umask 077
RUN_DIR="$RUN_ROOT/run-$(date +%s)-$$-$RANDOM"
assert_vmm_socket_path_fits "$RUN_DIR"
prepare_run_root
mkdir -p /run/lock
touch "$LOCK_FILE"
chown root:root "$LOCK_FILE"
chmod 0600 "$LOCK_FILE"
[ "$(stat -c '%u:%a' "$LOCK_FILE")" = "0:600" ] ||
  fail "global lock is not root-owned mode 0600: $LOCK_FILE"
exec 9>"$LOCK_FILE"
flock -n 9 || fail "another egress recovery run already holds $LOCK_FILE"
mkdir -m 0700 "$RUN_DIR" || fail "could not create run directory $RUN_DIR"
RUN_DIR_CREATED=1
run_dir_is_owned || fail "run directory is not a root-owned mode 0700 directory: $RUN_DIR"
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
  recorded_process_is_current "$SERVER_PID" "$SERVER_START_TICKS"
}

server_listener_is_current() {
  server_pid_is_current &&
    ss -lntp "sport = :8080" 2>/dev/null |
      grep -F "127.0.0.1:8080" |
      grep -F "pid=$SERVER_PID," >/dev/null
}

server_exit_confirmed() {
  recorded_process_exit_confirmed "$SERVER_PID" "$SERVER_START_TICKS"
}

wait_for_server_exit() {
  local deadline=$((SECONDS + SERVER_STOP_TIMEOUT_SECONDS)) status
  while ((SECONDS < deadline)); do
    if server_exit_confirmed; then
      status=0
    else
      status=$?
    fi
    [ "$status" -eq 0 ] && return 0
    [ "$status" -eq 2 ] && return 2
    sleep 1
  done
  if server_exit_confirmed; then
    status=0
  else
    status=$?
  fi
  [ "$status" -eq 0 ] && return 0
  [ "$status" -eq 2 ] && return 2
  return 1
}

reap_confirmed_server() {
  local wait_status
  if [ -e "/proc/$SERVER_PID" ] && ! server_pid_is_current &&
    [ -e "/proc/$SERVER_PID" ]; then
    echo "WARN: could not verify recorded taritd PID $SERVER_PID identity before reap" >&2
    return 1
  fi
  if wait "$SERVER_PID" 2>/dev/null; then
    return 0
  fi
  wait_status=$?
  if [ "$wait_status" -eq 127 ]; then
    echo "WARN: could not reap recorded taritd PID $SERVER_PID" >&2
    return 1
  fi
  return 0
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
  local status cleanup_failed=0
  [ -n "$SERVER_PID" ] || return 0

  if server_exit_confirmed; then
    status=0
  else
    status=$?
  fi
  case "$status" in
    0) ;;
    1)
      if server_pid_is_current; then
        if ! kill -TERM -- "$SERVER_PID" 2>/dev/null; then
          if server_exit_confirmed; then
            status=0
          else
            echo "WARN: could not TERM recorded taritd PID $SERVER_PID" >&2
            cleanup_failed=1
          fi
        fi
      elif server_exit_confirmed; then
        status=0
      else
        echo "WARN: could not verify recorded taritd PID $SERVER_PID identity before TERM" >&2
        cleanup_failed=1
      fi

      if wait_for_server_exit; then
        status=0
      else
        status=$?
      fi
      if [ "$status" -eq 1 ]; then
        if server_pid_is_current; then
          if ! kill -KILL -- "$SERVER_PID" 2>/dev/null; then
            if server_exit_confirmed; then
              status=0
            else
              echo "WARN: could not KILL recorded taritd PID $SERVER_PID after TERM deadline" >&2
              cleanup_failed=1
            fi
          fi
        elif server_exit_confirmed; then
          status=0
        else
          echo "WARN: could not verify recorded taritd PID $SERVER_PID identity before KILL" >&2
          cleanup_failed=1
        fi
        if wait_for_server_exit; then
          status=0
        else
          status=$?
        fi
      fi
      if [ "$status" -ne 0 ]; then
        if [ "$status" -eq 2 ]; then
          echo "WARN: could not observe recorded taritd PID $SERVER_PID exit" >&2
        else
          echo "WARN: recorded taritd PID $SERVER_PID did not exit after TERM/KILL deadline" >&2
        fi
        cleanup_failed=1
      fi
      ;;
    *)
      echo "WARN: could not observe recorded taritd PID $SERVER_PID before shutdown" >&2
      cleanup_failed=1
      ;;
  esac

  if server_exit_confirmed; then
    reap_confirmed_server || cleanup_failed=1
  else
    echo "WARN: recorded taritd PID $SERVER_PID exit was not confirmed before reap" >&2
    cleanup_failed=1
  fi
  SERVER_PID=
  SERVER_START_TICKS=
  return "$cleanup_failed"
}

delete_vm() {
  local vm_id=$1
  [ -n "$vm_id" ] || return
  server_listener_is_current || {
    echo "WARN: skipping VM delete because recorded taritd listener ownership changed" >&2
    return 1
  }
  "$TARIT" vm delete "$vm_id" >/dev/null
}

cleanup_run_vmm_processes() {
  local index pid start_ticks status cleanup_failed=0
  for index in "${!VMM_PIDS[@]}"; do
    pid=${VMM_PIDS[$index]:-}
    start_ticks=${VMM_START_TICKS[$index]:-}
    if [ -z "$pid" ] || [ -z "$start_ticks" ]; then
      echo "WARN: incomplete recorded VMM identity at index $index" >&2
      cleanup_failed=1
      continue
    fi
    if recorded_process_exit_confirmed "$pid" "$start_ticks"; then
      status=0
    else
      status=$?
    fi
    if [ "$status" -eq 0 ]; then
      VMM_EXIT_CONFIRMED[$index]=1
      continue
    fi
    if [ "$status" -eq 2 ]; then
      echo "WARN: could not observe recorded VMM $pid exit" >&2
      cleanup_failed=1
      continue
    fi
    if ! recorded_process_is_current "$pid" "$start_ticks"; then
      if recorded_process_exit_confirmed "$pid" "$start_ticks"; then
        VMM_EXIT_CONFIRMED[$index]=1
      else
        echo "WARN: could not verify recorded VMM $pid identity before TERM" >&2
        cleanup_failed=1
      fi
      continue
    fi
    if ! kill -TERM "$pid" 2>/dev/null; then
      if recorded_process_exit_confirmed "$pid" "$start_ticks"; then
        VMM_EXIT_CONFIRMED[$index]=1
      else
        echo "WARN: could not TERM recorded VMM $pid" >&2
        cleanup_failed=1
      fi
      continue
    fi
    for _ in $(seq 1 20); do
      if recorded_process_exit_confirmed "$pid" "$start_ticks"; then
        status=0
        break
      fi
      status=$?
      [ "$status" -eq 2 ] && break
      sleep 1
    done
    if recorded_process_exit_confirmed "$pid" "$start_ticks"; then
      status=0
    else
      status=$?
    fi
    if [ "$status" -eq 1 ]; then
      echo "WARN: recorded VMM $pid did not exit after TERM" >&2
      cleanup_failed=1
    elif [ "$status" -eq 2 ]; then
      echo "WARN: could not observe recorded VMM $pid exit" >&2
      cleanup_failed=1
    else
      VMM_EXIT_CONFIRMED[$index]=1
    fi
  done
  return "$cleanup_failed"
}

tap_is_present() {
  local tap=$1 links
  links=$(ip -j link show) || return 2
  python3 -c '
import json
import sys

tap = sys.argv[1]
try:
    links = json.load(sys.stdin)
except (TypeError, ValueError):
    raise SystemExit(2)
raise SystemExit(0 if any(link.get("ifname") == tap for link in links) else 1)
' "$tap" <<<"$links"
}

ingress_table_exists() {
  local slot=$1 table tables
  table="taritd_ingress_$slot"
  tables=$(nft list tables netdev) || return 2
  awk -v table="$table" '$0 == "table netdev " table { found = 1 } END { exit !found }' <<<"$tables"
}

assert_managed_ingress_table() {
  local vm_id=$1 tap=$2 slot=${2#insta} listing
  [ "$slot" != "$tap" ] && [ "$slot" -ge 0 ] 2>/dev/null || return 1
  listing=$(nft -a list table netdev "taritd_ingress_$slot") || return 1
  python3 -c '
import re
import sys

slot, vm_id, tap = sys.argv[1:]
text = "\n".join(
    re.sub(r"\s+# handle [0-9]+\s*$", "", line)
    for line in sys.stdin.read().splitlines()
)
tokens = []
token = []
quoted = False
escaped = False
for char in text:
    if quoted:
        token.append(char)
        if escaped:
            escaped = False
        elif char == "\\":
            escaped = True
        elif char == "\"":
            quoted = False
            tokens.append("".join(token))
            token = []
    elif char == "\"":
        if token:
            tokens.append("".join(token))
            token = []
        token.append(char)
        quoted = True
    elif char in "{};":
        if token:
            tokens.append("".join(token))
            token = []
        tokens.append(char)
    elif char.isspace():
        if token:
            tokens.append("".join(token))
            token = []
    else:
        token.append(char)
if quoted:
    raise SystemExit(1)
if token:
    tokens.append("".join(token))

comment = f"\"taritd-ingress slot={slot} vm={vm_id} tap={tap}\""
expected = [
    ["table", "netdev", f"taritd_ingress_{slot}", "{"],
    ["chain", "ingress", "{"],
    ["type", "filter", "hook", "ingress", "device", f"\"{tap}\"", "priority", "filter", ";", "policy", "drop", ";"],
    ["ether", "type", "arp", "accept", "comment", comment],
    ["ether", "type", "ip", "accept", "comment", comment],
]
index = 0
for rule_index, rule in enumerate(expected):
    if tokens[index:index + len(rule)] != rule:
        raise SystemExit(1)
    index += len(rule)
    if rule_index in (3, 4) and index < len(tokens) and tokens[index] == ";":
        index += 1
raise SystemExit(0 if tokens[index:] == ["}", "}"] else 1)
' "$slot" "$vm_id" "$tap" <<<"$listing"
}

guest_ip_for_slot() {
  local slot=$1 base b2 b3
  [ "$slot" -ge 0 ] 2>/dev/null && [ "$slot" -lt 16384 ] || return 1
  base=$((slot * 4))
  b2=$((base >> 8))
  b3=$((base & 255))
  printf '172.16.%s.%s\n' "$b2" "$((b3 + 2))"
}

cleanup_expected_uplink() {
  ip route get "$EGRESS_TEST_IP" 2>/dev/null |
    awk '/ dev / { for (i = 1; i <= NF; i++) if ($i == "dev") { print $(i + 1); exit } }'
}

recognized_managed_cleanup_rule() {
  local chain=$1 line=$2 tap=$3 tag=$4 guest=$5 uplink=$6 rule
  [[ "$line" =~ \#\ handle\ [0-9]+$ ]] || return 1
  rule=${line%% \# handle *}
  rule=$(normalize_nft_counter "$rule")
  case "$chain" in
    post)
      [[ "$rule" == "iifname \"$tap\" ip saddr $guest oifname \"$uplink\" masquerade comment \"taritd $tag\"" ]]
      ;;
    vm_egress)
      [[ "$rule" == "iifname \"$tap\" ip saddr != $guest counter drop comment \"taritd-guard $tag\"" ]] ||
        [[ "$rule" == "iifname \"$tap\" ip saddr $guest ip daddr 172.16.0.0/16 drop comment \"taritd-guard $tag\"" ]] ||
        [[ "$rule" == "iifname \"$tap\" ip saddr $guest oifname != \"$uplink\" drop comment \"taritd-guard $tag\"" ]] ||
        [[ "$rule" == "iifname \"$tap\" ip saddr $guest ct state established,related accept comment \"taritd-egress $tag\"" ]] ||
        [[ "$rule" == "iifname \"$tap\" ip saddr $guest ip daddr $EGRESS_TEST_IP/32 tcp dport $EGRESS_TEST_PORT accept comment \"taritd-egress $tag\"" ]] ||
        [[ "$rule" == "iifname \"$tap\" ip saddr $guest drop comment \"taritd-egress $tag\"" ]] ||
        [[ "$rule" == "iifname \"$tap\" drop comment \"taritd-recovery-quarantine $tag\"" ]] ||
        [[ "$rule" == "iifname \"$tap\" drop comment \"taritd-egress-update-quarantine $tag\"" ]]
      ;;
    vm_input)
      [[ "$rule" == "iifname \"$tap\" ip saddr != $guest counter drop comment \"taritd-input $tag\"" ]] ||
        [[ "$rule" == "iifname \"$tap\" ct state established,related accept comment \"taritd-input $tag\"" ]] ||
        [[ "$rule" == "iifname \"$tap\" drop comment \"taritd-input $tag\"" ]] ||
        [[ "$rule" == "iifname \"$tap\" drop comment \"taritd-recovery-quarantine $tag\"" ]] ||
        [[ "$rule" == "iifname \"$tap\" drop comment \"taritd-egress-update-quarantine $tag\"" ]]
      ;;
    *) return 1 ;;
  esac
}

cleanup_tap_policy() {
  local vm_id=$1 tap=$2 slot chain handle line tag listing cleanup_failed=0 present_status guest uplink table tables
  [ -n "$vm_id" ] && [ -n "$tap" ] || return
  slot=${tap#insta}
  [ "$slot" != "$tap" ] && [ "$slot" -ge 0 ] 2>/dev/null || return
  tag="slot=$slot vm=$vm_id tap=$tap"
  guest=$(guest_ip_for_slot "$slot") || {
    echo "WARN: retained policy for $tap because its derived guest IP is invalid" >&2
    return 1
  }
  uplink=$(cleanup_expected_uplink) || {
    echo "WARN: retained policy for $tap because the expected uplink could not be derived" >&2
    return 1
  }
  [ -n "$uplink" ] || {
    echo "WARN: retained policy for $tap because the expected uplink is empty" >&2
    return 1
  }
  if tap_is_present "$tap"; then
    if ! ip link set "$tap" down >/dev/null 2>&1 ||
      ! ip link del "$tap" >/dev/null 2>&1; then
      echo "WARN: retained policy for $tap because fallback TAP containment/deletion failed" >&2
      return 1
    fi
    if tap_is_present "$tap"; then
      echo "WARN: retained policy for $tap because fallback TAP deletion was not confirmed" >&2
      return 1
    fi
  else
    present_status=$?
    if [ "$present_status" -ne 1 ]; then
      echo "WARN: retained policy for $tap because fallback TAP presence could not be checked" >&2
      return 1
    fi
  fi
  listing=$(nft -a list table ip taritd_nat) || {
    echo "WARN: retained policy for $tap because taritd_nat could not be listed" >&2
    return 1
  }
  while IFS=$'\t' read -r chain handle line; do
    [ -n "$handle" ] || continue
    if ! recognized_managed_cleanup_rule "$chain" "$line" "$tap" "$tag" "$guest" "$uplink"; then
      echo "WARN: retained unrecognized exact tagged $chain policy for $tap" >&2
      cleanup_failed=1
      continue
    fi
    nft delete rule ip taritd_nat "$chain" handle "$handle" >/dev/null 2>&1 || {
      echo "WARN: could not remove exact $chain policy for $tap" >&2
      cleanup_failed=1
    }
  done < <(awk -v tag="$tag" '
        /^[[:space:]]*chain[[:space:]]+[^[:space:]]+[[:space:]]*\{/ {
          chain = $2
        }
        index($0, tag) && chain != "" && match($0, /# handle [0-9]+$/) {
          print chain "\t" substr($0, RSTART + 9, RLENGTH - 9) "\t" $0
        }' <<<"$listing")
  listing=$(nft -a list table ip taritd_nat) || {
    echo "WARN: retained policy for $tap because taritd_nat could not be re-listed" >&2
    cleanup_failed=1
  }
  if grep -F -- "$tag" <<<"$listing" >/dev/null; then
    echo "WARN: retained exact tagged taritd_nat policy for $tap" >&2
    cleanup_failed=1
  fi

  tables=$(nft list tables netdev) || {
    echo "WARN: retained ingress policy for $tap because netdev tables could not be listed" >&2
    return 1
  }
  while IFS= read -r table; do
    [ -n "$table" ] || continue
    listing=$(nft -a list table netdev "$table") || {
      echo "WARN: retained ingress policy for $tap because netdev table $table could not be listed" >&2
      cleanup_failed=1
      continue
    }
    if ! grep -F -- "$tag" <<<"$listing" >/dev/null; then
      continue
    fi
    if [ "$table" != "taritd_ingress_$slot" ] ||
      ! assert_managed_ingress_table "$vm_id" "$tap"; then
      echo "WARN: retained netdev policy for $tap because its full managed shape did not validate" >&2
      cleanup_failed=1
      continue
    fi
    nft delete table netdev "$table" >/dev/null 2>&1 || {
      echo "WARN: could not remove exact ingress policy for $tap" >&2
      cleanup_failed=1
    }
  done < <(awk '$1 == "table" && $2 == "netdev" { print $3 }' <<<"$tables")
  tables=$(nft list tables netdev) || {
    echo "WARN: retained ingress policy for $tap because netdev tables could not be re-listed" >&2
    return 1
  }
  while IFS= read -r table; do
    listing=$(nft -a list table netdev "$table") || {
      cleanup_failed=1
      continue
    }
    if grep -F -- "$tag" <<<"$listing" >/dev/null; then
      echo "WARN: retained exact tagged netdev policy for $tap in $table" >&2
      cleanup_failed=1
    fi
  done < <(awk '$1 == "table" && $2 == "netdev" { print $3 }' <<<"$tables")
  return "$cleanup_failed"
}

tap_for_recorded_vm_from_listing() {
  local vm_id=$1 listing=$2 uplink=$3 chain line slot tap tag guest
  [ -n "$vm_id" ] && [ -n "$uplink" ] || return 1
  while IFS=$'\t' read -r chain line; do
    [ "$chain" = post ] || continue
    [[ "$line" =~ comment\ \"taritd(-egress|-guard|-input|-recovery-quarantine|-egress-update-quarantine)?\ slot=([0-9]+)\ vm=$vm_id\ tap=(insta[0-9]+)\"\ \#\ handle\ [0-9]+$ ]] ||
      continue
    slot=${BASH_REMATCH[2]}
    tap=${BASH_REMATCH[3]}
    [ "$tap" = "insta$slot" ] || continue
    tag="slot=$slot vm=$vm_id tap=$tap"
    guest=$(guest_ip_for_slot "$slot") || continue
    recognized_managed_cleanup_rule "$chain" "$line" "$tap" "$tag" "$guest" "$uplink" ||
      continue
    printf '%s\n' "$tap"
    return 0
  done < <(awk '
    /^[[:space:]]*chain[[:space:]]+[^[:space:]]+[[:space:]]*\{/ {
      chain = $2
    }
    chain != "" {
      print chain "\t" $0
    }' <<<"$listing")
  return 1
}

tap_for_recorded_vm() {
  local vm_id=$1 listing uplink
  listing=$(nft -a list table ip taritd_nat 2>/dev/null) || return 1
  uplink=$(cleanup_expected_uplink) || return 1
  tap_for_recorded_vm_from_listing "$vm_id" "$listing" "$uplink"
}

assert_fallback_tap_parser_rejects_non_allocation_rules() {
  local vm_id=00000000-0000-0000-0000-000000000000 candidate
  for candidate in \
    "chain post {
    iifname \"insta7\" ip saddr 172.16.0.30 oifname \"eth0\" masquerade comment \"taritd operator note vm=$vm_id tap=insta7\" # handle 7
  }" \
    "chain vm_egress {
    iifname \"insta7\" ip saddr 172.16.0.30 drop comment \"taritd-egress slot=7 vm=$vm_id tap=insta7\" # handle 8
  }" \
    "chain vm_input {
    iifname \"insta7\" drop comment \"taritd-recovery-quarantine slot=7 vm=$vm_id tap=insta7\" # handle 9
  }"; do
    if tap_for_recorded_vm_from_listing "$vm_id" "$candidate" eth0 >/dev/null; then
      fail "fallback TAP parser accepted a non-allocation rule"
    fi
  done
}

cleanup_recorded_tap_policies() {
  local index vm_id tap cleanup_failed=0 exited
  for index in "${!VM_IDS[@]}"; do
    vm_id=${VM_IDS[$index]:-}
    tap=${VM_TAPS[$index]:-}
    exited=${VMM_EXIT_CONFIRMED[$index]:-0}
    if [ "$exited" != 1 ]; then
      echo "WARN: retained TAP policy for VM $vm_id because exact VMM exit was not confirmed" >&2
      cleanup_failed=1
      continue
    fi
    [ -n "$tap" ] || tap=$(tap_for_recorded_vm "$vm_id")
    [ -n "$tap" ] && cleanup_tap_policy "$vm_id" "$tap" || cleanup_failed=1
  done
  return "$cleanup_failed"
}

delete_recorded_vms() {
  local index
  for ((index=${#VM_IDS[@]} - 1; index >= 0; index--)); do
    delete_vm "${VM_IDS[$index]}" || true
  done
}

cleanup() {
  local original_status=$? cleanup_failed=0 final_status
  delete_recorded_vms || true
  cleanup_run_vmm_processes || cleanup_failed=1
  cleanup_recorded_tap_policies || cleanup_failed=1
  stop_taritd || cleanup_failed=1
  if [ -n "$IPV6_FORWARDING" ]; then
    sysctl -qw "net.ipv6.conf.all.forwarding=$IPV6_FORWARDING" || {
      echo "WARN: could not restore IPv6 forwarding" >&2
      cleanup_failed=1
    }
  fi
  if [ -n "$IPV4_FORWARDING" ]; then
    sysctl -qw "net.ipv4.ip_forward=$IPV4_FORWARDING" || {
      echo "WARN: could not restore IPv4 forwarding" >&2
      cleanup_failed=1
    }
  fi
  if [ "$cleanup_failed" -ne 0 ]; then
    echo "FAIL: fail-closed fallback cleanup retained unmanaged resources" >&2
    final_status=1
  else
    if [ "$RUN_DIR_CREATED" -eq 1 ]; then
      if run_dir_is_owned; then
        if ! rm -rf -- "$RUN_DIR" || [ -e "$RUN_DIR" ]; then
          echo "FAIL: could not remove run directory: $RUN_DIR" >&2
          final_status=1
        fi
      else
        echo "FAIL: retained run directory with unexpected ownership or permissions: $RUN_DIR" >&2
        final_status=1
      fi
    fi
    [ "${final_status:-}" = 1 ] || final_status=$original_status
  fi
  trap - EXIT
  exit "$final_status"
}
trap cleanup EXIT

create_vm() {
  "$TARIT" --json vm create --vcpus 1 --memory-mib 256 |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])'
}

vmm_pid_for_vm() {
  local vm_id=$1 socket inode proc pid fd
  socket="$TARIT_SOCKET_DIR/$vm_id.sock"
  for _ in $(seq 1 20); do
    inode=$(awk -v socket="$socket" '$8 == socket { print $7; exit }' /proc/net/unix)
    if [ -n "$inode" ]; then
      for proc in /proc/[0-9]*; do
        pid=${proc#/proc/}
        for fd in "$proc"/fd/*; do
          [ "$(readlink "$fd" 2>/dev/null)" = "socket:[$inode]" ] && {
            printf '%s\n' "$pid"
            return
          }
        done
      done
    fi
    sleep 1
  done
  fail "could not record VMM PID for VM $vm_id"
}

record_vm_process() {
  local vm_id=$1 pid start_ticks
  pid=$(vmm_pid_for_vm "$vm_id")
  [ -r "/proc/$pid/stat" ] || fail "could not read recorded VMM $pid start time"
  start_ticks=$(awk '{print $22}' "/proc/$pid/stat") ||
    fail "could not read recorded VMM $pid start time"
  case "$pid:$start_ticks" in
    *[!0-9:]*|:) fail "invalid recorded VMM identity for VM $vm_id" ;;
  esac
  VMM_PIDS+=("$pid")
  VMM_START_TICKS+=("$start_ticks")
  VMM_EXIT_CONFIRMED+=(0)
  VM_TAPS+=("")
}

record_vm_tap() {
  local vm_id=$1 tap=$2 index
  for index in "${!VM_IDS[@]}"; do
    if [ "${VM_IDS[$index]}" = "$vm_id" ]; then
      VM_TAPS[$index]=$tap
      return
    fi
  done
  fail "cannot record TAP for unknown VM $vm_id"
}

tap_for_vm() {
  local vm_id=$1 tap listing uplink
  for _ in $(seq 1 20); do
    listing=$(nft -a list table ip taritd_nat 2>/dev/null) || {
      sleep 1
      continue
    }
    uplink=$(cleanup_expected_uplink) || {
      sleep 1
      continue
    }
    tap=$(tap_for_recorded_vm_from_listing "$vm_id" "$listing" "$uplink" || true)
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
    tcp) socket_check='ss -lntp' ;;
    udp) socket_check='ss -lunp' ;;
    *) fail "unsupported listener protocol: $protocol" ;;
  esac
  for _ in $(seq 1 20); do
    run_guest "$vm_id" "set -- \$(cat '$pid_file' 2>/dev/null); [ \"\$#\" = 2 ] && pid=\$1 && start_ticks=\$2 && case \"\$pid:\$start_ticks\" in *[!0-9:]*|:) exit 1;; esac && [ -r \"/proc/\$pid/stat\" ] && [ \"\$(awk '{print \$22}' \"/proc/\$pid/stat\")\" = \"\$start_ticks\" ] && $socket_check \"sport = :$port\" | grep -F \"pid=\$pid,\" >/dev/null"
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

normalize_nft_rule() {
  python3 -c '
import sys

text = sys.stdin.read().strip()
out = []
pending_space = False
quoted = False
escaped = False
for char in text:
    if quoted:
        out.append(char)
        if escaped:
            escaped = False
        elif char == "\\":
            escaped = True
        elif char == "\"":
            quoted = False
    elif char == "\"":
        if pending_space and out:
            out.append(" ")
        pending_space = False
        out.append(char)
        quoted = True
    elif char.isspace():
        pending_space = True
    else:
        if pending_space and out:
            out.append(" ")
        pending_space = False
        out.append(char)
print("".join(out))
' <<<"$1"
}

normalize_nft_counter() {
  local rule
  rule=$(normalize_nft_rule "$1")
  if [[ "$rule" =~ ^(.*)\ counter\ packets\ ([0-9]+)\ bytes\ ([0-9]+)\ (drop)$ ]]; then
    printf '%s counter drop\n' "${BASH_REMATCH[1]}"
  else
    printf '%s\n' "$rule"
  fi
}

assert_known_tarit_tap() {
  local line=$1 tap
  for tap in "$TAP_A" "$TAP_B" "$TAP_C"; do
    [ -n "$tap" ] && [[ "$line" == *"iifname \"$tap\""* ]] && return
  done
  fail "closed Tarit chain has an untracked TAP rule: $line"
}

assert_closed_tarit_chain_for_tap() {
  local listing=$1 chain=$2 tap=$3 vm_id=$4 guest=$5 uplink=$6 line rule comment
  while IFS= read -r line; do
    line=$(normalize_nft_rule "$line")
    [[ "$line" == "table "* || "$line" == "chain "* || "$line" == "}" ||
      "$line" =~ ^type\ filter\ hook\ (forward|input)\ priority\ (filter|0)\;\ policy\ accept\;$ ]] &&
      continue
    rule=${line%% comment \"*}
    [[ " $rule " == *" jump "* || " $rule " == *" goto "* ||
      " $rule " == *" vmap "* || " $rule " == *" map "* ]] &&
      fail "closed Tarit chain has an indirect verdict: $line"
    [[ "$rule" == *" accept"* && ! "$rule" =~ ^iifname\ \"insta[0-9]+\"\  ]] &&
      fail "accept lacks an exact TAP matcher in $chain: $line"
    [[ "$rule" =~ ^iifname\ \"insta[0-9]+\"\  ]] ||
      fail "closed Tarit chain has a globally effective unknown rule: $line"
    [[ "$line" == *'comment "taritd'* ]] ||
      fail "closed Tarit chain has an untagged rule: $line"
    assert_known_tarit_tap "$rule"
    [[ "$rule" == *"iifname \"$tap\""* ]] || continue
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
      "vm_input:iifname \"$tap\" drop:taritd-input slot=${tap#insta} vm=$vm_id tap=$tap")
        ;;
      *)
        fail "unmanaged or unrecognized Tarit rule shape in $chain for $tap: $line"
        ;;
    esac
  done <<<"$listing"
}

count_rule() {
  grep -cF -- "$2" <<<"$1" || true
}

assert_egress_rule_order() {
  local listing=$1 tap=$2 guest=$3 vm_id=$4 source lateral uplink stateful allow deny
  listing=$(while IFS= read -r line; do normalize_nft_rule "$line"; done <<<"$listing")
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
  [ "$(count_rule "$listing" "iifname \"$tap\" ip saddr != $guest counter")" = 1 ] &&
    [ "$(count_rule "$listing" "iifname \"$tap\" ip saddr $guest ip daddr 172.16.0.0/16 drop")" = 1 ] &&
    [ "$(count_rule "$listing" "iifname \"$tap\" ip saddr $guest oifname !=")" = 1 ] &&
    [ "$(count_rule "$listing" "iifname \"$tap\" ip saddr $guest ct state established,related accept comment \"taritd-egress slot=${tap#insta} vm=$vm_id tap=$tap\"")" = 1 ] &&
    [ "$(count_rule "$listing" "iifname \"$tap\" ip saddr $guest ip daddr $EGRESS_TEST_IP/32 tcp dport $EGRESS_TEST_PORT accept comment \"taritd-egress slot=${tap#insta} vm=$vm_id tap=$tap\"")" = 1 ] &&
    [ "$(count_rule "$listing" "iifname \"$tap\" ip saddr $guest drop comment \"taritd-egress slot=${tap#insta} vm=$vm_id tap=$tap\"")" = 1 ] ||
    fail "duplicate required egress policy rule for $tap"
  [ "$source" -lt "$stateful" ] && [ "$lateral" -lt "$stateful" ] &&
    [ "$uplink" -lt "$stateful" ] && [ "$stateful" -lt "$allow" ] &&
    [ "$allow" -lt "$deny" ] ||
    fail "default-open or misordered egress policy for $tap"
}

assert_input_rule_order() {
  local listing=$1 tap=$2 guest=$3 vm_id=$4 source stateful deny
  listing=$(while IFS= read -r line; do normalize_nft_rule "$line"; done <<<"$listing")
  source=$(grep -nF "iifname \"$tap\" ip saddr != $guest counter" <<<"$listing" |
    head -1 | cut -d: -f1)
  stateful=$(grep -nF "iifname \"$tap\" ct state established,related accept comment \"taritd-input slot=${tap#insta} vm=$vm_id tap=$tap\"" <<<"$listing" |
    head -1 | cut -d: -f1)
  deny=$(grep -nF "iifname \"$tap\" drop comment \"taritd-input slot=${tap#insta} vm=$vm_id tap=$tap\"" <<<"$listing" |
    tail -1 | cut -d: -f1)
  [ -n "$source" ] && [ -n "$stateful" ] && [ -n "$deny" ] ||
    fail "missing input source guard, return accept, or default deny for $tap"
  [ "$(count_rule "$listing" "iifname \"$tap\" ip saddr != $guest counter")" = 1 ] &&
    [ "$(count_rule "$listing" "iifname \"$tap\" ct state established,related accept comment \"taritd-input slot=${tap#insta} vm=$vm_id tap=$tap\"")" = 1 ] &&
    [ "$(count_rule "$listing" "iifname \"$tap\" drop comment \"taritd-input slot=${tap#insta} vm=$vm_id tap=$tap\"")" = 1 ] ||
    fail "duplicate required input policy rule for $tap"
  [ "$source" -lt "$stateful" ] && [ "$stateful" -lt "$deny" ] ||
    fail "misordered input policy for $tap"
}

assert_recovered_policy() {
  local vm_id=$1 tap=$2 guest=$3 slot=${2#insta} forward input nat
  local uplink=$4
  [ "$(sysctl -n "net.ipv6.conf.$tap.disable_ipv6")" = 1 ] ||
    fail "recovered TAP permits IPv6: $tap"
  assert_managed_ingress_table "$vm_id" "$tap" ||
    fail "recovered TAP ingress policy is not the exact managed policy: $tap"
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
  require_rule "$input" "iifname \"$tap\" drop"
  assert_input_rule_order "$input" "$tap" "$guest" "$vm_id"
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
assert_fallback_tap_parser_rejects_non_allocation_rules

cp -f "$BASE_ROOTFS" "$ROOTFS"

start_taritd
VM_A=$(create_vm)
VM_IDS+=("$VM_A")
record_vm_process "$VM_A"
VM_B=$(create_vm)
VM_IDS+=("$VM_B")
record_vm_process "$VM_B"
TAP_A=$(tap_for_vm "$VM_A")
TAP_B=$(tap_for_vm "$VM_B")
record_vm_tap "$VM_A" "$TAP_A"
record_vm_tap "$VM_B" "$TAP_B"
GUEST_A=$(guest_ip_for_tap "$TAP_A")
GUEST_B=$(guest_ip_for_tap "$TAP_B")
UPLINK=$(ip route get 8.8.8.8 |
  awk '/ dev / {for (i=1; i<=NF; i++) if ($i == "dev") {print $(i+1); exit}}')
UPLINK_IP=$(ip route get 8.8.8.8 |
  awk '/ src / {for (i=1; i<=NF; i++) if ($i == "src") {print $(i+1); exit}}')
[ -n "$UPLINK" ] && [ -n "$UPLINK_IP" ] || fail "could not determine default uplink and source IP"
TEST_UPLINK=$(ip route get "$EGRESS_TEST_IP" |
  awk '/ dev / {for (i=1; i<=NF; i++) if ($i == "dev") {print $(i+1); exit}}')
[ "$TEST_UPLINK" = "$UPLINK" ] ||
  fail "external endpoint $EGRESS_TEST_IP routes via $TEST_UPLINK, expected production uplink $UPLINK"

# These guest utilities are required for the behavioral checks; do not treat a
# missing tool as a pass because that would skip the actual isolation gate.
for vm_id in "$VM_A" "$VM_B"; do
  expect_guest_success "$vm_id" 'command -v sh ip ping nc ss sysctl timeout'
done

# Keep listeners on guest B alive while guest A attempts lateral TCP and UDP.
expect_guest_success "$VM_B" 'rm -f /run/tarit-tcp.pid /run/tarit-udp.pid /run/tarit-udp; nc -l -p 31337 >/dev/null 2>&1 & tcp_pid=$!; echo "$tcp_pid $(awk '"'"'{print $22}'"'"' /proc/$tcp_pid/stat)" >/run/tarit-tcp.pid; nc -u -l -p 31338 >/run/tarit-udp 2>&1 & udp_pid=$!; echo "$udp_pid $(awk '"'"'{print $22}'"'"' /proc/$udp_pid/stat)" >/run/tarit-udp.pid; kill -0 "$tcp_pid" && kill -0 "$udp_pid"'
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
assert_managed_ingress_table "$VM_A" "$TAP_A" ||
  fail "missing exact IPv6 netdev default-deny policy on $TAP_A"
assert_managed_ingress_table "$VM_B" "$TAP_B" ||
  fail "missing exact IPv6 netdev default-deny policy on $TAP_B"
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

STALE_VM_A=$(cat /proc/sys/kernel/random/uuid) ||
  fail "could not create a distinct prior-owner restart sentinel identity"
[ "$STALE_VM_A" != "$VM_A" ] ||
  fail "restart sentinel identity unexpectedly matches the current VM"
nft add rule ip taritd_nat vm_egress \
  iifname "$TAP_A" ip saddr "$GUEST_A" drop \
  comment "taritd-egress slot=${TAP_A#insta} vm=$STALE_VM_A tap=$TAP_A"

stop_taritd
start_taritd
assert_recovered_policy "$VM_A" "$TAP_A" "$GUEST_A" "$UPLINK"
assert_recovered_policy "$VM_B" "$TAP_B" "$GUEST_B" "$UPLINK"
! nft -a list chain ip taritd_nat vm_egress |
  grep -F "vm=$STALE_VM_A tap=$TAP_A" >/dev/null ||
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
VM_IDS+=("$VM_C")
record_vm_process "$VM_C"
TAP_C=$(tap_for_vm "$VM_C")
record_vm_tap "$VM_C" "$TAP_C"
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
