#!/usr/bin/env bash
# Linux/KVM port-share end-to-end gate.
#
# This intentionally uses real guest networking, two taritd nodes, and the
# production share listeners. Run it on a Linux KVM host:
#
#   sudo -E bash tests/e2e_shares.sh
#
# Inputs are deliberately environment-configurable. No credential is embedded:
# per-run API, peer, and share-token keys are generated in memory.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ORCH_ROOT="${ORCH_ROOT:-$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)}"
REPO_ROOT="${REPO_ROOT:-$(CDPATH='' cd -- "$ORCH_ROOT/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$REPO_ROOT/vmm}"

# shellcheck source=../../test/lib/preflight.sh
. "$REPO_ROOT/test/lib/preflight.sh"

usage() {
  cat <<'USAGE'
Usage: sudo -E bash tests/e2e_shares.sh [--preflight]

Required guest asset:
  TARIT_SHARE_ROOTFS (or TARIT_ROOTFS) must be an agent-enabled Node.js rootfs.

Useful overrides:
  TARITD_BIN, TARIT_VMM_BIN (or VMM_BIN), TARIT_KERNEL, TARIT_SHARE_ROOTFS
  TARIT_DATABASE_URL                 Existing PostgreSQL fleet database.
  TARIT_E2E_BASE_PORT                First of five local listener ports.
  TARIT_E2E_EDGE_SLUG                Fixed Caddy test hostname label (default: tarit-e2e-edge).
  TARIT_CADDY_BIN                    Caddy executable (default: caddy).
  TARIT_E2E_GUEST_PORT               Guest test-server port (default 43127).
  TARIT_E2E_RUN_ROOT                 Per-run artifact root (default: orch/e).
  TARIT_E2E_KEEP_ARTIFACTS=1         Keep the per-run directory after cleanup.

When TARIT_DATABASE_URL is unset, the harness starts an isolated local
PostgreSQL instance using initdb and pg_ctl. It never uses Docker. The Linux
host must provide Caddy, curl, Python 3 (with sqlite3), GNU coreutils
(sha256sum, timeout, mktemp, stat, cmp, chown, and chgrp), iproute2, nftables, procps, util-linux
(flock and, for local PostgreSQL, runuser), and PostgreSQL's psql. Local
PostgreSQL mode additionally needs initdb and pg_ctl. The guest rootfs must
provide Node.js. Caddy is mandatory: this gate does not fall back to plaintext
share traffic.
USAGE
}

case "${1:-}" in
  "")
    ;;
  --preflight)
    PREFLIGHT_ONLY=1
    ;;
  --help|-h)
    usage
    exit 0
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

PREFLIGHT_ONLY="${PREFLIGHT_ONLY:-0}"

log() {
  printf '%s\n' "$*"
}

warn() {
  printf 'WARN: %s\n' "$*" >&2
}

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  return 1
}

skip() {
  printf 'SKIP: %s\n' "$*" >&2
  exit 77
}

require_command() {
  local command_name="$1"
  local hint="$2"
  command -v "$command_name" >/dev/null 2>&1 ||
    skip "required command '$command_name' is unavailable; $hint"
}

acquire_host_network_lock() {
  NETWORK_LOCK_PATH="${TARIT_E2E_NETWORK_LOCK_PATH:-/run/lock/tarit-e2e-shares.lock}"
  case "$NETWORK_LOCK_PATH" in
    /run/lock/*)
      ;;
    *)
      fail "TARIT_E2E_NETWORK_LOCK_PATH must be below /run/lock"
      ;;
  esac
  exec 9>"$NETWORK_LOCK_PATH"
  flock -n 9 ||
    skip "another Tarit share E2E run owns the host-network lock"
  private_path "$NETWORK_LOCK_PATH"
}

canonical_path() {
  readlink -f -- "$1"
}

private_path() {
  local path="$1"
  local mode=""

  [[ -e "$path" && ! -L "$path" ]] ||
    fail "expected private artifact '$path' is missing or is a symlink"
  mode="$(stat -c '%a' -- "$path")" ||
    fail "could not read permissions for '$path'"
  [[ "$mode" =~ ^[0-7]{3,4}$ ]] ||
    fail "unexpected permissions for '$path'"
  (( (8#$mode & 8#077) == 0 )) ||
    fail "artifact '$path' is accessible to group or other users"
}

subprocess_timeout_seconds() {
  local fallback="$1"
  local value="${TARIT_E2E_ACTIVE_WAIT_TIMEOUT_SECS:-$fallback}"

  [[ "$value" =~ ^[1-9][0-9]*$ ]] ||
    fail "subprocess timeout must be a positive integer"
  printf '%s\n' "$value"
}

allocate_port() {
  python3 - <<'PY'
import socket

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
}

allocate_ports() {
  local count="$1"
  python3 - "$count" <<'PY'
import socket
import sys

sockets = []
try:
    for _ in range(int(sys.argv[1])):
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.bind(("127.0.0.1", 0))
        sockets.append(sock)
    for sock in sockets:
        print(sock.getsockname()[1])
finally:
    for sock in sockets:
        sock.close()
PY
}

allocate_listener_ports() {
  if [[ -n "${TARIT_E2E_BASE_PORT:-}" ]]; then
    [[ "${TARIT_E2E_BASE_PORT}" =~ ^[0-9]+$ ]] ||
      fail "TARIT_E2E_BASE_PORT must be an integer"
    CONTROL_PORT_A="$TARIT_E2E_BASE_PORT"
    CONTROL_PORT_B="$((TARIT_E2E_BASE_PORT + 1))"
    SHARE_PORT_A="$((TARIT_E2E_BASE_PORT + 2))"
    SHARE_PORT_B="$((TARIT_E2E_BASE_PORT + 3))"
    CADDY_PORT="$((TARIT_E2E_BASE_PORT + 4))"
  else
    local listener_ports=()
    mapfile -t listener_ports < <(allocate_ports 5)
    CONTROL_PORT_A="${listener_ports[0]}"
    CONTROL_PORT_B="${listener_ports[1]}"
    SHARE_PORT_A="${listener_ports[2]}"
    SHARE_PORT_B="${listener_ports[3]}"
    CADDY_PORT="${listener_ports[4]}"
  fi

  python3 - "$CONTROL_PORT_A" "$CONTROL_PORT_B" "$SHARE_PORT_A" "$SHARE_PORT_B" "$CADDY_PORT" <<'PY'
import sys

ports = [int(port) for port in sys.argv[1:]]
if len(set(ports)) != len(ports) or any(port < 1024 or port > 65535 for port in ports):
    raise SystemExit("listener ports must be distinct unprivileged TCP ports")
PY
}

find_pg_binary() {
  local name="$1"
  local override="$2"
  local bindir=""

  if [[ -n "$override" ]]; then
    [[ -x "$override" ]] || return 1
    printf '%s\n' "$override"
    return 0
  fi
  if command -v "$name" >/dev/null 2>&1; then
    command -v "$name"
    return 0
  fi
  if command -v pg_config >/dev/null 2>&1; then
    bindir="$(pg_config --bindir 2>/dev/null || true)"
    if [[ -n "$bindir" && -x "$bindir/$name" ]]; then
      printf '%s\n' "$bindir/$name"
      return 0
    fi
  fi
  return 1
}

pid_matches_binary() {
  local pid="$1"
  local expected="$2"
  local actual=""

  [[ -n "$pid" ]] && kill -0 "$pid" >/dev/null 2>&1 || return 1
  [[ -r "/proc/$pid/exe" ]] || return 1
  actual="$(readlink -f -- "/proc/$pid/exe" 2>/dev/null || true)"
  [[ "$actual" == "$expected" ]]
}

process_environment_has() {
  local pid="$1"
  local expected="$2"
  local entry=""
  local environment="/proc/$pid/environ"

  [[ -r "$environment" ]] || return 1
  while IFS= read -r -d '' entry; do
    [[ "$entry" == "$expected" ]] && return 0
  done <"$environment"
  return 1
}

pid_belongs_to_this_run() {
  local pid="$1"

  [[ -n "${RUN_ID:-}" && -n "${RUN_DIR:-}" && -f "${RUN_MARKER:-}" ]] ||
    return 1
  process_environment_has "$pid" "TARIT_E2E_SHARES_RUN_ID=$RUN_ID" &&
    process_environment_has "$pid" "TARIT_E2E_SHARES_RUN_DIR=$RUN_DIR"
}

pid_matches_owned_binary() {
  local pid="$1"
  local expected="$2"

  pid_matches_binary "$pid" "$expected" &&
    pid_belongs_to_this_run "$pid"
}

is_tarit_or_vmm_process() {
  local pid="$1"
  local actual=""
  local name=""

  [[ -r "/proc/$pid/exe" ]] || return 1
  actual="$(readlink -f -- "/proc/$pid/exe" 2>/dev/null || true)"
  name="${actual##*/}"
  [[ "$name" == "taritd" || "$name" == "vmm" ]] ||
    [[ -n "${TARITD_BIN_REAL:-}" && "$actual" == "$TARITD_BIN_REAL" ]] ||
    [[ -n "${VMM_BIN_REAL:-}" && "$actual" == "$VMM_BIN_REAL" ]]
}

unrelated_tarit_or_vmm_processes_present() {
  local proc=""
  local pid=""
  local actual=""

  for proc in /proc/[0-9]*; do
    pid="${proc##*/}"
    [[ "$pid" =~ ^[0-9]+$ && "$pid" != "$$" ]] || continue
    is_tarit_or_vmm_process "$pid" || continue
    pid_belongs_to_this_run "$pid" && continue
    actual="$(readlink -f -- "/proc/$pid/exe" 2>/dev/null || true)"
    warn "unrelated Tarit process is present (PID $pid, executable $actual)"
    return 0
  done
  return 1
}

any_tarit_or_vmm_processes_present() {
  local proc=""
  local pid=""

  for proc in /proc/[0-9]*; do
    pid="${proc##*/}"
    [[ "$pid" =~ ^[0-9]+$ && "$pid" != "$$" ]] || continue
    is_tarit_or_vmm_process "$pid" && return 0
  done
  return 1
}

pid_is_gone() {
  local pid="$1"
  ! kill -0 "$pid" >/dev/null 2>&1
}

wait_until() {
  local description="$1"
  local timeout_seconds="$2"
  shift 2
  local probe_timeout="${TARIT_E2E_WAIT_CALL_TIMEOUT_SECS:-3}"
  local deadline=$((SECONDS + timeout_seconds))
  local TARIT_E2E_ACTIVE_WAIT_TIMEOUT_SECS=""

  [[ "$timeout_seconds" =~ ^[1-9][0-9]*$ ]] ||
    fail "wait timeout for $description must be a positive integer"
  if ! [[ "$probe_timeout" =~ ^[1-9][0-9]*$ ]] ||
    ! (( probe_timeout < timeout_seconds )); then
    fail "per-call wait timeout must be shorter than the $timeout_seconds-second $description deadline"
  fi
  TARIT_E2E_ACTIVE_WAIT_TIMEOUT_SECS="$probe_timeout"

  while (( SECONDS < deadline )); do
    if "$@"; then
      return 0
    fi
    sleep 0.2
  done
  fail "timed out waiting for $description after ${timeout_seconds}s"
}

wait_for_pid_exit() {
  local pid="$1"
  local timeout_seconds="$2"
  local deadline=$((SECONDS + timeout_seconds))
  local probe_timeout=1
  local process_state=""

  (( probe_timeout < timeout_seconds )) ||
    fail "PID wait timeout must exceed its per-call process probe timeout"

  while (( SECONDS < deadline )); do
    if pid_is_gone "$pid"; then
      wait "$pid" 2>/dev/null || true
      return 0
    fi
    process_state="$("$TIMEOUT_BIN" "${probe_timeout}s" ps -o stat= -p "$pid" 2>/dev/null || true)"
    process_state="${process_state//[[:space:]]/}"
    if [[ "$process_state" == Z* ]]; then
      wait "$pid" 2>/dev/null || true
      return 0
    fi
    sleep 0.2
  done
  return 1
}

terminate_expected_pid() {
  local pid="$1"
  local binary="$2"
  local label="$3"

  [[ -n "$pid" ]] || return 0
  if pid_is_gone "$pid"; then
    wait "$pid" 2>/dev/null || true
    return 0
  fi
  pid_matches_owned_binary "$pid" "$binary" ||
    fail "refusing to terminate $label PID $pid because it is not this run's expected $binary"

  kill -TERM "$pid"
  if ! wait_for_pid_exit "$pid" 30; then
    warn "$label PID $pid did not exit after SIGTERM; sending SIGKILL to that tracked PID"
    pid_matches_owned_binary "$pid" "$binary" ||
      fail "refusing to SIGKILL $label PID $pid because it is not this run's expected $binary"
    kill -KILL "$pid"
    wait_for_pid_exit "$pid" 10 ||
      fail "$label PID $pid did not exit after SIGKILL"
  fi
}

safe_remove_run_dir() {
  [[ "${TARIT_E2E_KEEP_ARTIFACTS:-0}" == "1" ]] && {
    log "Keeping artifacts at $RUN_DIR"
    return 0
  }
  [[ -n "${RUN_DIR:-}" && -f "${RUN_MARKER:-}" ]] ||
    return 0
  case "$RUN_DIR" in
    "$RUN_ROOT"/shares.*)
      rm -rf -- "$RUN_DIR"
      ;;
    *)
      warn "refusing to remove unexpected run directory '$RUN_DIR'"
      ;;
  esac
}

run_as_pg_user() {
  if [[ "$PG_OS_USER" == "$(id -un)" ]]; then
    "$@"
  else
    runuser -u "$PG_OS_USER" -- "$@"
  fi
}

grant_postgres_run_access() {
  [[ "$PG_OS_USER" == "$(id -un)" ]] && return 0
  RUN_DIR_ORIGINAL_GID="$(stat -c '%g' -- "$RUN_DIR")" ||
    fail "could not read run-directory owner before PostgreSQL setup"
  PG_OS_GID="$(id -g "$PG_OS_USER")" ||
    fail "could not determine PostgreSQL OS group"
  chgrp "$PG_OS_GID" "$RUN_DIR" ||
    fail "could not grant the isolated PostgreSQL user traversal to the run directory"
  chmod 0710 "$RUN_DIR" ||
    fail "could not set private PostgreSQL traversal permissions"
  [[ "$(stat -c '%a' -- "$RUN_DIR")" == "710" &&
    "$(stat -c '%g' -- "$RUN_DIR")" == "$PG_OS_GID" ]] ||
    fail "run directory did not retain owner-only data permissions for PostgreSQL setup"
  RUN_DIR_PG_TRAVERSE_GRANTED=1
}

restore_run_dir_permissions() {
  [[ "${RUN_DIR_PG_TRAVERSE_GRANTED:-0}" == "1" ]] || return 0
  chgrp "$RUN_DIR_ORIGINAL_GID" "$RUN_DIR" ||
    return 1
  chmod 0700 "$RUN_DIR" ||
    return 1
  private_path "$RUN_DIR" || return 1
  RUN_DIR_PG_TRAVERSE_GRANTED=0
}

cleanup_database_rows() {
  [[ "$DATABASE_MODE" == "external" ]] || return 0
  [[ -n "${PSQL_BIN:-}" && -n "${DATABASE_URL:-}" ]] || return 0

  "$TIMEOUT_BIN" 15s "$PSQL_BIN" "$DATABASE_URL" --no-psqlrc -q \
    -v ON_ERROR_STOP=0 \
    -v owner_key="$OWNER_KEY" \
    -v host_prefix="$HOST_PREFIX%" <<'SQL' >/dev/null 2>&1 || true
DELETE FROM fleet_shares WHERE owner_key = :'owner_key';
DELETE FROM fleet_vms WHERE host_id LIKE :'host_prefix';
DELETE FROM fleet_hosts WHERE host_id LIKE :'host_prefix';
DELETE FROM fleet_leader WHERE leader_id LIKE :'host_prefix';
SQL
}

stop_local_postgres() {
  [[ "$DATABASE_MODE" == "local" ]] || return 0
  if ! [[ -n "${PG_PID:-}" && "${PG_PID}" =~ ^[0-9]+$ ]]; then
    DATABASE_MODE="stopped"
    return 0
  fi
  if pid_is_gone "$PG_PID"; then
    DATABASE_MODE="stopped"
    return 0
  fi
  local cmdline=""
  cmdline="$(tr '\0' ' ' <"/proc/$PG_PID/cmdline" 2>/dev/null || true)"
  [[ "$cmdline" == *postgres* && "$cmdline" == *"$PG_DATA_DIR"* ]] ||
    return 1

  kill -TERM "$PG_PID"
  if ! wait_for_pid_exit "$PG_PID" 30; then
    warn "isolated PostgreSQL PID $PG_PID did not exit after SIGTERM; sending SIGINT to that tracked PID"
    kill -INT "$PG_PID"
    wait_for_pid_exit "$PG_PID" 15 || return 1
  fi
  DATABASE_MODE="stopped"
  return 0
}

record_local_postgres_pid() {
  PG_PID="$(head -n 1 "$PG_DATA_DIR/postmaster.pid" 2>/dev/null || true)"
  [[ -n "$PG_PID" && "$PG_PID" =~ ^[0-9]+$ ]]
}

tarit_network_artifacts_present() {
  ip -o link show | grep -Eq '^[0-9]+: insta[0-9]+(:|@)'
}

capture_host_networking() {
  if nft list table ip taritd_nat >/dev/null 2>&1; then
    fail "refusing to capture a pre-existing taritd_nat nft table"
  fi
  ORIGINAL_IP_FORWARD="$(sysctl -n net.ipv4.ip_forward 2>/dev/null)" ||
    fail "could not read net.ipv4.ip_forward before guest-network setup"
  [[ "$ORIGINAL_IP_FORWARD" =~ ^[01]$ ]] ||
    fail "unexpected net.ipv4.ip_forward value '$ORIGINAL_IP_FORWARD'"
  NETWORK_SNAPSHOT_CAPTURED=1
  NETWORK_START_ATTEMPTED=0
  NFT_TABLE_CREATED_BY_RUN=0
  IP_FORWARD_CHANGED_BY_RUN=0
}

record_owned_host_networking() {
  [[ "${NETWORK_SNAPSHOT_CAPTURED:-0}" == "1" ]] ||
    fail "host-network state was not captured before node startup"
  nft list table ip taritd_nat >/dev/null 2>&1 ||
    fail "taritd did not create its expected nft table"
  local current_ip_forward=""
  current_ip_forward="$(sysctl -n net.ipv4.ip_forward 2>/dev/null)" ||
    fail "could not read net.ipv4.ip_forward after guest-network setup"
  [[ "$current_ip_forward" == "1" ]] ||
    fail "taritd guest networking did not enable IPv4 forwarding"
  [[ "$ORIGINAL_IP_FORWARD" == "$current_ip_forward" ]] ||
    IP_FORWARD_CHANGED_BY_RUN=1
  NFT_TABLE_BASELINE="$(mktemp "$RUN_DIR/nft-baseline.XXXXXX")" ||
    fail "could not create a private nft baseline artifact"
  nft list table ip taritd_nat >"$NFT_TABLE_BASELINE" ||
    fail "could not capture the owned taritd nft table baseline"
  private_path "$NFT_TABLE_BASELINE"
  NFT_TABLE_CREATED_BY_RUN=1
}

nft_table_matches_owned_baseline() {
  local current=""
  current="$(mktemp "$RUN_DIR/nft-current.XXXXXX")" ||
    return 1
  if ! nft list table ip taritd_nat >"$current"; then
    rm -f -- "$current"
    return 1
  fi
  if cmp -s "$NFT_TABLE_BASELINE" "$current"; then
    rm -f -- "$current"
    return 0
  fi
  rm -f -- "$current"
  return 1
}

restore_host_networking() {
  [[ "${NETWORK_SNAPSHOT_CAPTURED:-0}" == "1" ]] || return 0
  if any_tarit_or_vmm_processes_present; then
    warn "refusing to alter host networking while a Tarit process still exists"
    return 1
  fi
  if tarit_network_artifacts_present; then
    warn "refusing to remove taritd_nat while guest-network interfaces still exist"
    return 1
  fi
  if [[ "${NETWORK_START_ATTEMPTED:-0}" == "1" &&
    "${NFT_TABLE_CREATED_BY_RUN:-0}" != "1" ]]; then
    if nft list table ip taritd_nat >/dev/null 2>&1 ||
      [[ "$(sysctl -n net.ipv4.ip_forward 2>/dev/null)" != "$ORIGINAL_IP_FORWARD" ]]; then
      warn "refusing to alter host networking because startup ownership was not fully recorded"
      return 1
    fi
  fi
  if [[ "${NFT_TABLE_CREATED_BY_RUN:-0}" == "1" ]]; then
    nft list table ip taritd_nat >/dev/null 2>&1 ||
      return 1
    if ! nft_table_matches_owned_baseline; then
      warn "refusing to delete a taritd_nat table that differs from this run's baseline"
      return 1
    fi
    nft delete table ip taritd_nat >/dev/null 2>&1 ||
      return 1
  fi
  if [[ "${IP_FORWARD_CHANGED_BY_RUN:-0}" == "1" ]]; then
    [[ "$(sysctl -n net.ipv4.ip_forward 2>/dev/null)" == "1" ]] ||
      return 1
    sysctl -qw "net.ipv4.ip_forward=$ORIGINAL_IP_FORWARD" ||
      return 1
  fi
  NETWORK_SNAPSHOT_CAPTURED=0
  return 0
}

delete_known_vms_best_effort() {
  local vm_id
  [[ -n "${CONTROL_URL_A:-}" ]] || return 0
  for vm_id in "${VM_IDS[@]:-}"; do
    curl --noproxy '*' --silent --show-error --connect-timeout 2 --max-time 15 \
      -X DELETE \
      -H "X-API-Key: $API_KEY" \
      "$CONTROL_URL_A/v1/vms/$vm_id" >/dev/null 2>&1 || true
  done
}

stop_tracked_vmm_processes() {
  local pid
  for pid in "${VMM_PIDS[@]:-}"; do
    if pid_matches_owned_binary "$pid" "$VMM_BIN_REAL"; then
      terminate_expected_pid "$pid" "$VMM_BIN_REAL" "VMM child" || true
    fi
  done
}

stop_caddy() {
  [[ -n "${CADDY_PID:-}" ]] || return 0
  terminate_expected_pid "$CADDY_PID" "$CADDY_BIN_REAL" "Caddy edge"
  CADDY_PID=""
}

cleanup() {
  local status=$?
  local cleanup_failed=0
  trap - EXIT INT TERM HUP
  set +e
  stop_caddy || cleanup_failed=1
  delete_known_vms_best_effort
  [[ -n "${NODE_A_PID:-}" ]] &&
    terminate_expected_pid "$NODE_A_PID" "$TARITD_BIN_REAL" "node A" || true
  [[ -n "${NODE_B_PID:-}" ]] &&
    terminate_expected_pid "$NODE_B_PID" "$TARITD_BIN_REAL" "node B" || true
  stop_tracked_vmm_processes
  cleanup_database_rows
  restore_host_networking || cleanup_failed=1
  stop_local_postgres || cleanup_failed=1
  if [[ "$DATABASE_MODE" == "stopped" ]]; then
    restore_run_dir_permissions || cleanup_failed=1
  fi
  if [[ "$cleanup_failed" -eq 0 ]]; then
    safe_remove_run_dir
  else
    warn "cleanup could not safely release every owned resource; preserving $RUN_DIR"
    [[ "$status" -eq 0 ]] && status=1
  fi
  exit "$status"
}

preflight() {
  [[ "$(uname -s)" == "Linux" ]] ||
    skip "this gate boots real KVM guests and requires Linux (found $(uname -s))"
  [[ "$(id -u)" == "0" ]] ||
    skip "guest networking requires root; rerun with sudo -E"
  [[ -e /dev/kvm && -r /dev/kvm && -w /dev/kvm ]] ||
    skip "/dev/kvm is unavailable or inaccessible; use a bare-metal or nested-KVM Linux host"

  require_command curl "install curl"
  require_command python3 "install Python 3"
  require_command sha256sum "install coreutils"
  require_command ip "install iproute2"
  require_command nft "install nftables"
  require_command ps "install procps"
  require_command readlink "install coreutils"
  require_command grep "install grep"
  require_command sysctl "install procps"
  require_command flock "install util-linux"
  require_command "$TIMEOUT_BIN" "install GNU coreutils timeout"
  require_command mktemp "install coreutils"
  require_command stat "install coreutils"
  require_command cmp "install coreutils"
  require_command awk "install awk"
  require_command find "install findutils"
  if ! command -v "$CADDY_BIN" >/dev/null 2>&1; then
    fail "Caddy is required for the TLS edge gate but '$CADDY_BIN' is unavailable; install Caddy or set TARIT_CADDY_BIN"
    return 1
  fi

  [[ -x "$TARITD_BIN" ]] ||
    skip "taritd binary not found at TARITD_BIN=$TARITD_BIN; build orch with cargo build --release -p taritd or set TARITD_BIN"
  [[ -x "$VMM_BIN" ]] ||
    skip "VMM binary not found at TARIT_VMM_BIN=$VMM_BIN; build vmm with its KVM boot feature or set TARIT_VMM_BIN"
  [[ -r "$KERNEL" ]] ||
    skip "guest kernel not found at TARIT_KERNEL=$KERNEL; build guest assets or set TARIT_KERNEL"
  [[ -r "$ROOTFS" ]] ||
    skip "Node.js guest rootfs not found at TARIT_SHARE_ROOTFS=$ROOTFS; set it to an agent-enabled Node.js rootfs"
  TARITD_BIN_REAL="$(canonical_path "$TARITD_BIN")"
  VMM_BIN_REAL="$(canonical_path "$VMM_BIN")"
  CADDY_BIN_REAL="$(canonical_path "$(command -v "$CADDY_BIN")")"

  acquire_host_network_lock
  if unrelated_tarit_or_vmm_processes_present; then
    fail "refusing to start with unrelated taritd or VMM processes"
    return 1
  fi
  if tarit_network_artifacts_present; then
    fail "refusing to start with existing Tarit guest-network interfaces"
    return 1
  fi
  if nft list table ip taritd_nat >/dev/null 2>&1; then
    fail "refusing to start with a pre-existing taritd_nat nft table"
    return 1
  fi

  if [[ -z "$REQUESTED_DATABASE_URL" ]]; then
    INITDB_BIN="$(find_pg_binary initdb "${TARIT_E2E_INITDB:-}" || true)"
    PG_CTL_BIN="$(find_pg_binary pg_ctl "${TARIT_E2E_PG_CTL:-}" || true)"
    PSQL_BIN="$(find_pg_binary psql "${TARIT_E2E_PSQL:-}" || true)"
    [[ -n "$INITDB_BIN" && -n "$PG_CTL_BIN" && -n "$PSQL_BIN" ]] ||
      skip "no TARIT_DATABASE_URL and local PostgreSQL tools are unavailable; install initdb/pg_ctl/psql or set TARIT_DATABASE_URL"
    if [[ -n "${TARIT_E2E_POSTGRES_OS_USER:-}" ]]; then
      PG_OS_USER="$TARIT_E2E_POSTGRES_OS_USER"
    elif [[ -n "${SUDO_USER:-}" && "${SUDO_USER}" != "root" ]]; then
      PG_OS_USER="$SUDO_USER"
    elif id postgres >/dev/null 2>&1; then
      PG_OS_USER="postgres"
    else
      skip "local PostgreSQL needs TARIT_E2E_POSTGRES_OS_USER (or SUDO_USER); alternatively set TARIT_DATABASE_URL"
    fi
    id "$PG_OS_USER" >/dev/null 2>&1 ||
      skip "local PostgreSQL OS user '$PG_OS_USER' does not exist; set TARIT_E2E_POSTGRES_OS_USER or TARIT_DATABASE_URL"
    require_command runuser "install util-linux or set TARIT_DATABASE_URL"
    require_command chown "install coreutils or set TARIT_DATABASE_URL"
    require_command chgrp "install coreutils or set TARIT_DATABASE_URL"
  else
    PSQL_BIN="$(find_pg_binary psql "${TARIT_E2E_PSQL:-}" || true)"
    [[ -n "$PSQL_BIN" ]] ||
      skip "TARIT_DATABASE_URL is set but psql is unavailable for the isolated-run cleanup"
  fi

  detect_virt
  log "Preflight: Linux, KVM, guest assets, network tools, and database mode are available."
}

TARITD_BIN="${TARITD_BIN:-${TARIT_BIN:-$ORCH_ROOT/target/release/taritd}}"
VMM_BIN="${TARIT_VMM_BIN:-${VMM_BIN:-$VMM_ROOT/target/release/vmm}}"
CADDY_BIN="${TARIT_CADDY_BIN:-caddy}"
TIMEOUT_BIN="${TARIT_E2E_TIMEOUT_BIN:-timeout}"
KERNEL="${TARIT_KERNEL:-$REPO_ROOT/guest-assets/vmlinux}"
ROOTFS="${TARIT_SHARE_ROOTFS:-${TARIT_ROOTFS:-$REPO_ROOT/guest-assets/share-node-rootfs.ext4}}"
GUEST_PORT="${TARIT_E2E_GUEST_PORT:-43127}"
SHARE_DOMAIN="${TARIT_SHARE_DOMAIN:-shares.e2e.test}"
EDGE_SLUG="${TARIT_E2E_EDGE_SLUG:-tarit-e2e-edge}"
RUN_ROOT="${TARIT_E2E_RUN_ROOT:-$ORCH_ROOT/e}"
REQUESTED_DATABASE_URL="${TARIT_DATABASE_URL:-}"
RUN_ID="shares-$(date -u +%Y%m%dT%H%M%S)-$$"
RUN_DIR=""
RUN_MARKER=""
TARITD_BIN_REAL=""
VMM_BIN_REAL=""
CADDY_BIN_REAL=""

if ! [[ "$GUEST_PORT" =~ ^[0-9]+$ ]] || (( GUEST_PORT < 1 || GUEST_PORT > 65535 )); then
  fail "TARIT_E2E_GUEST_PORT must be in 1..=65535"
fi
if ! [[ "$EDGE_SLUG" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]]; then
  fail "TARIT_E2E_EDGE_SLUG must be a lowercase DNS label"
fi
if ! [[ "$SHARE_DOMAIN" =~ ^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$ ]]; then
  fail "TARIT_SHARE_DOMAIN must be a lowercase DNS name for the Caddy test edge"
fi

preflight
if [[ "$PREFLIGHT_ONLY" == "1" ]]; then
  exit 0
fi

mkdir -p -- "$RUN_ROOT"
RUN_DIR="$(mktemp -d "$RUN_ROOT/shares.XXXXXX")" ||
  fail "could not create a private per-run artifact directory"
RUN_MARKER="$RUN_DIR/.tarit-e2e-shares-run"
: >"$RUN_MARKER"
private_path "$RUN_DIR"
private_path "$RUN_MARKER"
export TARIT_E2E_SHARES_RUN_ID="$RUN_ID"
export TARIT_E2E_SHARES_RUN_DIR="$RUN_DIR"

CONTROL_URL_A=""
CONTROL_URL_B=""
NODE_A_PID=""
NODE_B_PID=""
CADDY_PID=""
VM_IDS=()
VMM_PIDS=()
CREATED_VM_ID=""
DATABASE_MODE=""
DATABASE_URL=""
PG_DATA_DIR=""
PG_PORT=""
PG_PID=""
ORIGINAL_IP_FORWARD=""
NETWORK_SNAPSHOT_CAPTURED=0
NETWORK_START_ATTEMPTED=0
NFT_TABLE_CREATED_BY_RUN=0
NFT_TABLE_BASELINE=""
IP_FORWARD_CHANGED_BY_RUN=0
RUN_DIR_PG_TRAVERSE_GRANTED=0
RUN_DIR_ORIGINAL_GID=""
PG_OS_GID=""
VMM_LAUNCHER=""
RACE_VMM_ARM=""
RACE_VMM_READY=""
RACE_VMM_RELEASE=""
RACE_VMM_PID=""
PROPOSED_VM_ID=""
EDGE_HOST="$EDGE_SLUG.$SHARE_DOMAIN"
CADDY_CONFIG=""
CADDY_LOG=""
CADDY_HOME=""
CADDY_XDG_DATA_HOME=""
CADDY_XDG_CONFIG_HOME=""
CADDY_XDG_CACHE_HOME=""
CADDY_STORAGE_ROOT=""
CADDY_CA_CERT=""
CADDY_CA_KEY=""
HOST_PREFIX="share-e2e-$RUN_ID"
NODE_A_HOST="$HOST_PREFIX-a"
NODE_B_HOST="$HOST_PREFIX-b"
OWNER_KEY="$HOST_PREFIX-owner"

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

NODE_A_DIR="$(mktemp -d "$RUN_DIR/node-a.XXXXXX")"
NODE_B_DIR="$(mktemp -d "$RUN_DIR/node-b.XXXXXX")"
mkdir -p -- "$NODE_A_DIR/sockets" "$NODE_B_DIR/sockets"
private_path "$NODE_A_DIR"
private_path "$NODE_B_DIR"
private_path "$NODE_A_DIR/sockets"
private_path "$NODE_B_DIR/sockets"
NODE_A_LOG="$(mktemp "$NODE_A_DIR/taritd.log.XXXXXX")"
NODE_B_LOG="$(mktemp "$NODE_B_DIR/taritd.log.XXXXXX")"
private_path "$NODE_A_LOG"
private_path "$NODE_B_LOG"

readarray -t GENERATED_SECRETS < <("$TIMEOUT_BIN" 5s python3 - <<'PY'
import base64
import secrets

print(secrets.token_urlsafe(32))
print(secrets.token_urlsafe(40))
print(base64.urlsafe_b64encode(secrets.token_bytes(32)).rstrip(b"=").decode())
PY
)
API_KEY="${GENERATED_SECRETS[0]}"
PEER_SECRET="${GENERATED_SECRETS[1]}"
SHARE_TOKEN_KEY="${GENERATED_SECRETS[2]}"
unset GENERATED_SECRETS

LAST_BODY="$(mktemp "$RUN_DIR/last-body.XXXXXX")"
LAST_HEADERS="$(mktemp "$RUN_DIR/last-headers.XXXXXX")"
REQUEST_BODY_FILE="$(mktemp "$RUN_DIR/request-body.XXXXXX")"
private_path "$LAST_BODY"
private_path "$LAST_HEADERS"
private_path "$REQUEST_BODY_FILE"
LAST_STATUS=""
LAST_CURL_STATUS=""

http_request() {
  local method="$1"
  local url="$2"
  local body_path="$3"
  shift 3
  local max_time="${TARIT_E2E_HTTP_TIMEOUT_SECS:-60}"
  if [[ -n "${TARIT_E2E_ACTIVE_WAIT_TIMEOUT_SECS:-}" ]]; then
    max_time="$TARIT_E2E_ACTIVE_WAIT_TIMEOUT_SECS"
  fi
  [[ "$max_time" =~ ^[1-9][0-9]*$ ]] ||
    fail "HTTP timeout must be a positive integer"
  local -a args=(
    curl
    --noproxy '*'
    --silent
    --show-error
    --connect-timeout "${TARIT_E2E_CONNECT_TIMEOUT_SECS:-3}"
    --max-time "$max_time"
    -X "$method"
    -D "$LAST_HEADERS"
    -o "$LAST_BODY"
    -w '%{http_code}'
  )

  [[ -n "$body_path" ]] && args+=(--data-binary "@$body_path")
  args+=("$@" "$url")
  : >"$LAST_BODY"
  : >"$LAST_HEADERS"
  set +e
  LAST_CURL_STATUS="$("${args[@]}")"
  local curl_status=$?
  set -e
  if [[ "$curl_status" -ne 0 ]]; then
    LAST_STATUS="000"
  else
    LAST_STATUS="$LAST_CURL_STATUS"
  fi
  return 0
}

expect_status() {
  local expected="$1"
  local description="$2"
  [[ "$LAST_STATUS" == "$expected" ]] ||
    fail "$description: expected HTTP $expected, received $LAST_STATUS"
}

json_get() {
  local file="$1"
  local path="$2"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  JSON_FILE="$file" JSON_PATH="$path" "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - <<'PY'
import json
import os
import sys

value = json.load(open(os.environ["JSON_FILE"], encoding="utf-8"))
for part in os.environ["JSON_PATH"].split("."):
    if not isinstance(value, dict) or part not in value:
        raise SystemExit(f"missing JSON path {os.environ['JSON_PATH']}")
    value = value[part]
if value is None:
    print("")
elif isinstance(value, bool):
    print("true" if value else "false")
elif isinstance(value, (dict, list)):
    print(json.dumps(value, separators=(",", ":")))
else:
    print(value)
PY
}

json_assert_eq() {
  local file="$1"
  local path="$2"
  local expected="$3"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  JSON_FILE="$file" JSON_PATH="$path" JSON_EXPECTED="$expected" "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - <<'PY'
import json
import os
import sys

value = json.load(open(os.environ["JSON_FILE"], encoding="utf-8"))
for part in os.environ["JSON_PATH"].split("."):
    if not isinstance(value, dict) or part not in value:
        raise SystemExit(f"FAIL: missing JSON path {os.environ['JSON_PATH']}")
    value = value[part]
if value is None:
    actual = ""
elif isinstance(value, bool):
    actual = "true" if value else "false"
else:
    actual = str(value)
if actual != os.environ["JSON_EXPECTED"]:
    raise SystemExit(f"FAIL: JSON path {os.environ['JSON_PATH']} did not match its expected value")
PY
}

json_assert_exact_error() {
  local file="$1"
  local expected="$2"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  JSON_FILE="$file" JSON_EXPECTED="$expected" "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - <<'PY'
import json
import os

value = json.load(open(os.environ["JSON_FILE"], encoding="utf-8"))
if value != {"error": os.environ["JSON_EXPECTED"]}:
    raise SystemExit("FAIL: error response was not the expected stable JSON object")
PY
}

json_assert_forwarding_boundary() {
  local file="$1"
  local expected_host="$2"
  local expected_authorization="$3"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  JSON_FILE="$file" EXPECTED_HOST="$expected_host" EXPECTED_AUTHORIZATION="$expected_authorization" \
    "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - <<'PY'
import json
import os

data = json.load(open(os.environ["JSON_FILE"], encoding="utf-8"))
headers = data.get("headers")
counts = data.get("header_counts")
if not isinstance(headers, dict) or not isinstance(counts, dict):
    raise SystemExit("FAIL: guest did not return parsed request headers and counts")

host = os.environ["EXPECTED_HOST"]
expected = {
    "authorization": os.environ["EXPECTED_AUTHORIZATION"],
    "x-forwarded-host": host,
    "x-forwarded-proto": "https",
    "forwarded": f"host={host};proto=https",
}
for name, value in expected.items():
    if headers.get(name) != value or counts.get(name) != 1:
        raise SystemExit(f"FAIL: forwarding boundary did not produce exactly one expected {name} value")

for name in (
    "x-api-key",
    "x-tarit-share-token",
    "x-peer-secret",
    "x-real-ip",
    "x-forwarded-for",
):
    if name in headers or counts.get(name, 0) != 0:
        raise SystemExit(f"FAIL: sensitive header {name} reached the guest")
PY
}

json_assert_int_at_least() {
  local file="$1"
  local path="$2"
  local minimum="$3"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  JSON_FILE="$file" JSON_PATH="$path" JSON_MINIMUM="$minimum" "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - <<'PY'
import json
import os

value = json.load(open(os.environ["JSON_FILE"], encoding="utf-8"))
for part in os.environ["JSON_PATH"].split("."):
    if not isinstance(value, dict) or part not in value:
        raise SystemExit(f"FAIL: missing JSON path {os.environ['JSON_PATH']}")
    value = value[part]
if not isinstance(value, int) or value < int(os.environ["JSON_MINIMUM"]):
    raise SystemExit(f"FAIL: JSON path {os.environ['JSON_PATH']} was below its required minimum")
PY
}

write_json_body() {
  local payload="$1"
  printf '%s' "$payload" >"$REQUEST_BODY_FILE"
  printf '%s\n' "$REQUEST_BODY_FILE"
}

control_request() {
  local node="$1"
  local method="$2"
  local path="$3"
  local body_path="$4"
  shift 4
  local base_url=""
  case "$node" in
    a) base_url="$CONTROL_URL_A" ;;
    b) base_url="$CONTROL_URL_B" ;;
    *) fail "unknown control node '$node'" ;;
  esac
  http_request "$method" "$base_url$path" "$body_path" \
    -H "X-API-Key: $API_KEY" "$@"
}

api_json() {
  local node="$1"
  local method="$2"
  local path="$3"
  local payload="$4"
  local request_path=""
  request_path="$(write_json_body "$payload")"
  control_request "$node" "$method" "$path" "$request_path" \
    -H 'Content-Type: application/json'
}

api_empty() {
  local node="$1"
  local method="$2"
  local path="$3"
  control_request "$node" "$method" "$path" ""
}

share_host() {
  printf '%s\n' "$EDGE_HOST"
}

share_request() {
  local method="$1"
  local path="$2"
  local token="$3"
  shift 3
  local host=""
  host="$(share_host)"
  local -a headers=(
    --proto '=https'
    --cacert "$CADDY_CA_CERT"
    --resolve "$host:$CADDY_PORT:127.0.0.1"
    -H "Host: $host"
  )
  [[ -n "$token" ]] && headers+=(-H "X-Tarit-Share-Token: $token")
  headers+=("$@")
  http_request "$method" "https://$host:$CADDY_PORT$path" "" "${headers[@]}"
}

wait_for_health() {
  local node="$1"
  local url=""
  case "$node" in
    a) url="$CONTROL_URL_A" ;;
    b) url="$CONTROL_URL_B" ;;
    *) return 1 ;;
  esac
  http_request GET "$url/health" ""
  [[ "$LAST_STATUS" == "200" ]] && json_assert_eq "$LAST_BODY" status ok
}

wait_for_cluster() {
  api_empty a GET /v1/cluster
  [[ "$LAST_STATUS" == "200" ]] || return 1
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  CLUSTER_FILE="$LAST_BODY" NODE_A_HOST="$NODE_A_HOST" NODE_B_HOST="$NODE_B_HOST" \
    "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - <<'PY'
import json
import os

data = json.load(open(os.environ["CLUSTER_FILE"], encoding="utf-8"))
hosts = {entry.get("host_id") for entry in data.get("nodes", []) if entry.get("up")}
if data.get("healthy_nodes", 0) < 2:
    raise SystemExit(1)
if {os.environ["NODE_A_HOST"], os.environ["NODE_B_HOST"]} - hosts:
    raise SystemExit(1)
PY
}

wait_for_vm_running() {
  local vm_id="$1"
  api_empty b GET "/v1/vms/$vm_id/status"
  [[ "$LAST_STATUS" == "200" ]] || return 1
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  VM_STATUS_FILE="$LAST_BODY" "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - <<'PY'
import json
import os

data = json.load(open(os.environ["VM_STATUS_FILE"], encoding="utf-8"))
if data.get("state") != "running" or not data.get("vcpu_alive"):
    raise SystemExit(1)
PY
}

exec_payload() {
  local vm_id="$1"
  local command="$2"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - "$vm_id" "$command" <<'PY'
import json
import sys

print(json.dumps({
    "vm_id": sys.argv[1],
    "command": sys.argv[2],
    "timeout_ms": 60000,
}, separators=(",", ":")))
PY
}

guest_command_is_ready() {
  local vm_id="$1"
  api_json b POST /v1/execute "$(exec_payload "$vm_id" 'node --version')"
  [[ "$LAST_STATUS" == "200" ]] || return 1
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  JSON_FILE="$LAST_BODY" "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - <<'PY'
import json
import os

data = json.load(open(os.environ["JSON_FILE"], encoding="utf-8"))
if data.get("status") != "completed" or data.get("exit_code") != 0:
    raise SystemExit(1)
if not str(data.get("stdout", "")).startswith("v"):
    raise SystemExit(1)
PY
}

exec_guest_or_fail() {
  local vm_id="$1"
  local command="$2"
  local description="$3"
  api_json b POST /v1/execute "$(exec_payload "$vm_id" "$command")"
  expect_status 200 "$description request"
  json_assert_eq "$LAST_BODY" status completed
  json_assert_eq "$LAST_BODY" exit_code 0
}

create_vm_payload() {
  local requested_id="${1:-}"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - "$requested_id" <<'PY'
import json
import sys

payload = {"memory_mib": 256, "vcpus": 1}
if sys.argv[1]:
    payload["id"] = sys.argv[1]
print(json.dumps(payload, separators=(",", ":")))
PY
}

write_vmm_launcher() {
  VMM_LAUNCHER="$(mktemp "$RUN_DIR/vmm-launcher.XXXXXX")" ||
    fail "could not create a private VMM launcher"
  RACE_VMM_ARM="$(mktemp "$RUN_DIR/vmm-race-arm.XXXXXX")" ||
    fail "could not allocate a VMM-race arm path"
  RACE_VMM_READY="$(mktemp "$RUN_DIR/vmm-race-ready.XXXXXX")" ||
    fail "could not allocate a VMM-race ready path"
  RACE_VMM_RELEASE="$(mktemp "$RUN_DIR/vmm-race-release.XXXXXX")" ||
    fail "could not allocate a VMM-race release path"
  rm -f -- "$RACE_VMM_ARM" "$RACE_VMM_READY" "$RACE_VMM_RELEASE"
  cat >"$VMM_LAUNCHER" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail

if [[ -n "${TARIT_E2E_VMM_RACE_ARM:-}" && -f "$TARIT_E2E_VMM_RACE_ARM" ]]; then
  : "${TARIT_E2E_VMM_RACE_READY:?missing deterministic race-ready path}"
  : "${TARIT_E2E_VMM_RACE_RELEASE:?missing deterministic race-release path}"
  printf '%s\n' "$$" >"$TARIT_E2E_VMM_RACE_READY"
  deadline=$((SECONDS + ${TARIT_E2E_VMM_RACE_WAIT_SECS:-20}))
  while [[ ! -f "$TARIT_E2E_VMM_RACE_RELEASE" ]]; do
    (( SECONDS < deadline )) || exit 70
    sleep 0.05
  done
fi

exec "${TARIT_E2E_VMM_REAL:?missing real VMM path}" "$@"
SH
  chmod 0700 "$VMM_LAUNCHER"
  private_path "$VMM_LAUNCHER"
}

verify_real_kvm_vmm() {
  local vm_id="$1"
  local reported_pid="$2"
  local resolved_pid=""
  local fd=""
  local target=""
  local kvm_fds=0

  api_empty a GET "/v1/vms/$vm_id"
  expect_status 200 "resolve exact VMM PID after VM creation"
  resolved_pid="$(json_get "$LAST_BODY" pid)"
  [[ "$resolved_pid" == "$reported_pid" ]] ||
    fail "VM record did not resolve to the VMM PID returned by creation"
  pid_matches_owned_binary "$resolved_pid" "$VMM_BIN_REAL" ||
    fail "resolved VMM PID is not this run's expected VMM executable"
  [[ -d "/proc/$resolved_pid/fd" ]] ||
    fail "resolved VMM PID does not expose /proc/$resolved_pid/fd"
  for fd in "/proc/$resolved_pid/fd/"*; do
    [[ -e "$fd" ]] || continue
    target="$(readlink -f -- "$fd" 2>/dev/null || true)"
    [[ "$target" == "/dev/kvm" ]] && ((kvm_fds += 1))
  done
  (( kvm_fds > 0 )) ||
    fail "resolved VMM PID has no open /dev/kvm descriptor"
}

create_vm_on_node_a() {
  api_json a POST /v1/vms "$(create_vm_payload)"
  expect_status 201 "create KVM VM on node A"
  local vm_id=""
  local vmm_pid=""
  vm_id="$(json_get "$LAST_BODY" id)"
  vmm_pid="$(json_get "$LAST_BODY" pid)"
  [[ "$vm_id" =~ ^[0-9a-f-]{36}$ ]] ||
    fail "VM creation did not return a UUID"
  [[ "$vmm_pid" =~ ^[0-9]+$ ]] ||
    fail "VM creation did not return its tracked VMM PID"
  verify_real_kvm_vmm "$vm_id" "$vmm_pid"
  VM_IDS+=("$vm_id")
  VMM_PIDS+=("$vmm_pid")
  CREATED_VM_ID="$vm_id"
}

create_share_payload() {
  local vm_id="$1"
  local visibility="$2"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - "$vm_id" "$GUEST_PORT" "$visibility" <<'PY'
import json
import sys
print(json.dumps({
    "vm_id": sys.argv[1],
    "guest_port": int(sys.argv[2]),
    "visibility": sys.argv[3],
}, separators=(",", ":")))
PY
}

patch_share_payload() {
  local vm_id="$1"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - "$vm_id" <<'PY'
import json
import sys
print(json.dumps({"vm_id": sys.argv[1]}, separators=(",", ":")))
PY
}

patch_visibility_payload() {
  local visibility="$1"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - "$visibility" <<'PY'
import json
import sys
print(json.dumps({"visibility": sys.argv[1]}, separators=(",", ":")))
PY
}

stream_digest() {
  local byte_count="$1"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 20)"
  "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - "$byte_count" <<'PY'
import hashlib
import sys

remaining = int(sys.argv[1])
chunk = b"Z" * min(65536, remaining)
digest = hashlib.sha256()
while remaining:
    part = chunk if remaining >= len(chunk) else chunk[:remaining]
    digest.update(part)
    remaining -= len(part)
print(digest.hexdigest())
PY
}

run_stream_sha_gate() {
  local byte_count="${TARIT_E2E_STREAM_BYTES:-33554432}"
  local expected=""
  local host=""
  local status=""
  local headers_file=""
  local digest_file=""
  expected="$(stream_digest "$byte_count")"
  host="$(share_host)"
  headers_file="$(mktemp "$RUN_DIR/stream-headers.XXXXXX")"
  digest_file="$(mktemp "$RUN_DIR/stream-sha.XXXXXX")"
  private_path "$headers_file"
  private_path "$digest_file"

  if ! curl --noproxy '*' --silent --show-error --no-buffer \
    --proto '=https' \
    --cacert "$CADDY_CA_CERT" \
    --connect-timeout "${TARIT_E2E_CONNECT_TIMEOUT_SECS:-3}" \
    --max-time "${TARIT_E2E_STREAM_TIMEOUT_SECS:-90}" \
    --limit-rate "${TARIT_E2E_STREAM_RATE_LIMIT:-4M}" \
    --resolve "$host:$CADDY_PORT:127.0.0.1" \
    -H "Host: $host" \
    -D "$headers_file" \
    "https://$host:$CADDY_PORT/stream?bytes=$byte_count&chunk=65536" |
    sha256sum | awk '{print $1}' >"$digest_file"; then
    fail "32 MiB share response did not stream through the non-owner node"
  fi
  status="$(awk '/^HTTP\// { code=$2 } END { print code }' "$headers_file")"
  [[ "$status" == "200" ]] ||
    fail "streaming response returned HTTP $status instead of 200"
  [[ "$(cat "$digest_file")" == "$expected" ]] ||
    fail "streaming SHA-256 differed from the deterministic 32 MiB guest response"
}

run_large_upload_gate() {
  local byte_count="${TARIT_E2E_UPLOAD_BYTES:-33554432}"
  local expected=""
  local host=""
  local status_file=""
  expected="$(stream_digest "$byte_count")"
  host="$(share_host)"
  status_file="$(mktemp "$RUN_DIR/upload-status.XXXXXX")"
  private_path "$status_file"
  : >"$LAST_BODY"
  : >"$LAST_HEADERS"

  if ! "$TIMEOUT_BIN" "${TARIT_E2E_UPLOAD_TIMEOUT_SECS:-90}s" python3 - "$byte_count" <<'PY' |
import sys

remaining = int(sys.argv[1])
chunk = b"Z" * min(65536, remaining)
out = sys.stdout.buffer
while remaining:
    part = chunk if remaining >= len(chunk) else chunk[:remaining]
    out.write(part)
    remaining -= len(part)
PY
    curl --noproxy '*' --silent --show-error \
      --proto '=https' \
      --cacert "$CADDY_CA_CERT" \
      --connect-timeout "${TARIT_E2E_CONNECT_TIMEOUT_SECS:-3}" \
      --max-time "${TARIT_E2E_UPLOAD_TIMEOUT_SECS:-90}" \
      -X POST \
      --resolve "$host:$CADDY_PORT:127.0.0.1" \
      -H "Host: $host" \
      -H 'Content-Type: application/octet-stream' \
      --data-binary @- \
      -D "$LAST_HEADERS" \
      -o "$LAST_BODY" \
      -w '%{http_code}' \
      "https://$host:$CADDY_PORT/upload" >"$status_file"; then
    fail "large upload did not stream through the non-owner node"
  fi
  LAST_STATUS="$(cat "$status_file")"
  expect_status 200 "large streaming upload"
  json_assert_eq "$LAST_BODY" bytes "$byte_count"
  json_assert_eq "$LAST_BODY" sha256 "$expected"
}

assert_delayed_first_chunk() {
  SHARE_HOST="$(share_host)" SHARE_PORT="$CADDY_PORT" SHARE_CA_CERT="$CADDY_CA_CERT" \
    "$TIMEOUT_BIN" 15s python3 - <<'PY'
import http.client
import os
import socket
import ssl
import time

class ResolvedHttpsConnection(http.client.HTTPSConnection):
    def connect(self):
        raw = socket.create_connection(("127.0.0.1", self.port), self.timeout)
        self.sock = self._context.wrap_socket(raw, server_hostname=self.host)

context = ssl.create_default_context(cafile=os.environ["SHARE_CA_CERT"])
connection = ResolvedHttpsConnection(
    os.environ["SHARE_HOST"],
    int(os.environ["SHARE_PORT"]),
    timeout=10,
    context=context,
)
connection.putrequest("GET", "/delayed", skip_host=True)
connection.putheader("Host", os.environ["SHARE_HOST"])
connection.endheaders()
response = connection.getresponse()
if response.status != 200:
    raise SystemExit(f"FAIL: delayed response returned HTTP {response.status}")
started = time.monotonic()
first = response.read(1)
elapsed = time.monotonic() - started
rest = response.read()
connection.close()
if first + rest != b"delayed-first-chunk":
    raise SystemExit("FAIL: delayed response body changed")
if elapsed < 0.25 or elapsed > 8:
    raise SystemExit("FAIL: delayed first response chunk did not preserve timing")
PY
}

assert_malformed_hosts() {
  SHARE_DOMAIN="$SHARE_DOMAIN" SHARE_PORT="$SHARE_PORT_B" "$TIMEOUT_BIN" 10s python3 - <<'PY'
import os
import socket

port = int(os.environ["SHARE_PORT"])
domain = os.environ["SHARE_DOMAIN"]

def status_for(host):
    sock = socket.create_connection(("127.0.0.1", port), timeout=5)
    request = (
        "GET / HTTP/1.1\r\n"
        f"Host: {host}\r\n"
        "Connection: close\r\n\r\n"
    ).encode()
    sock.sendall(request)
    data = b""
    while b"\r\n" not in data:
        chunk = sock.recv(1024)
        if not chunk:
            break
        data += chunk
    sock.close()
    line = data.split(b"\r\n", 1)[0].split()
    return int(line[1]) if len(line) >= 2 else 0

for host in ("unrelated.example.test", f"-bad.{domain}"):
    if status_for(host) != 404:
        raise SystemExit(f"FAIL: malformed or non-share host {host!r} was not rejected with 404")
PY
}

assert_listener_isolation() {
  local host=""
  host="$(share_host)"

  http_request GET "$CONTROL_URL_B/" "" -H "Host: $host"
  expect_status 404 "control listener must not dispatch share host traffic"

  http_request GET "http://127.0.0.1:$SHARE_PORT_B/health" "" -H 'Host: unrelated.example.test'
  expect_status 404 "share listener must not expose the control health route"

  http_request GET "http://127.0.0.1:$SHARE_PORT_B/internal/v1/shares/$SHARE_ID" "" \
    -H "Host: $host"
  expect_status 404 "share listener must reject internal peer paths"
}

assert_caddy_internal_route_blocked() {
  share_request GET "/internal/v1/shares/$SHARE_ID" ""
  expect_status 404 "Caddy must block public internal peer paths"
  [[ "$(cat "$LAST_BODY")" == "edge-internal-route-blocked" ]] ||
    fail "internal route was not rejected by the Caddy edge"
}

assert_peer_rejections() {
  local forged_nonce=""
  forged_nonce="$("$TIMEOUT_BIN" 5s python3 - <<'PY'
import uuid
print(uuid.uuid4())
PY
)"

  http_request GET "$CONTROL_URL_A/internal/v1/vms/$VM1" ""
  expect_status 401 "missing peer secret must be rejected"

  http_request GET "$CONTROL_URL_A/internal/v1/vms/$VM1" "" \
    -H 'X-Peer-Secret: forged-peer-secret'
  expect_status 401 "forged peer secret must be rejected"

  http_request GET "$CONTROL_URL_A/internal/v1/vms/$VM1" "" \
    -H "X-Peer-Secret: $PEER_SECRET"
  expect_status 401 "peer calls without a signed identity must be rejected"

  http_request GET "$CONTROL_URL_A/internal/v1/vms/$VM1" "" \
    -H "X-Peer-Secret: $PEER_SECRET" \
    -H "X-Tarit-Tenant: $OWNER_KEY" \
    -H 'X-Tarit-Role: user' \
    -H 'X-Tarit-Api-Key-Id: forged' \
    -H "X-Tarit-Identity-Timestamp: $(date +%s)" \
    -H "X-Tarit-Identity-Nonce: $forged_nonce" \
    -H 'X-Tarit-Identity-Signature: invalid'
  expect_status 401 "forged signed peer identity must be rejected"

  http_request GET "$CONTROL_URL_A/internal/v1/shares/$SHARE_ID" ""
  expect_status 503 "unauthenticated internal share path must not disclose a share"

  http_request GET "$CONTROL_URL_A/internal/v1/shares/$SHARE_ID" "" \
    -H "X-Peer-Secret: $PEER_SECRET"
  expect_status 503 "unsigned internal share path must fail closed"
}

metric_value() {
  local file="$1"
  local name="$2"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  METRICS_FILE="$file" METRIC_NAME="$name" "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - <<'PY'
import os

values = []
for line in open(os.environ["METRICS_FILE"], encoding="utf-8"):
    line = line.strip()
    if not line or line.startswith("#"):
        continue
    parts = line.split()
    if len(parts) == 2 and parts[0] == os.environ["METRIC_NAME"]:
        values.append(parts[1])
if len(values) != 1:
    raise SystemExit("metric must have exactly one unlabelled sample")
print(values[0])
PY
}

share_gauges_are_zero_if_exposed() {
  local file="$1"
  local timeout_seconds=""
  timeout_seconds="$(subprocess_timeout_seconds 10)"
  METRICS_FILE="$file" "$TIMEOUT_BIN" "${timeout_seconds}s" python3 - <<'PY'
import os

samples = {}
for line in open(os.environ["METRICS_FILE"], encoding="utf-8"):
    line = line.strip()
    if not line or line.startswith("#"):
        continue
    parts = line.split()
    if len(parts) == 2 and parts[0] in {
        "taritd_share_active_http",
        "taritd_share_active_websockets",
    }:
        samples.setdefault(parts[0], []).append(parts[1])

if samples:
    for name in ("taritd_share_active_http", "taritd_share_active_websockets"):
        if samples.get(name) != ["0"]:
            raise SystemExit(f"{name} did not return to zero exactly once")
PY
}

share_gauges_are_zero() {
  local node=""
  for node in a b; do
    control_request "$node" GET /metrics ""
    [[ "$LAST_STATUS" == "200" ]] || return 1
    share_gauges_are_zero_if_exposed "$LAST_BODY" || return 1
  done
}

assert_metric_secrecy() {
  local metrics_file="$1"
  local node="$2"
  local value=""

  for value in \
    "$SHARE_SLUG" \
    "$SHARE_ID" \
    "$API_KEY" \
    "$PEER_SECRET" \
    "$OWNER_KEY" \
    "$VM1" \
    "$VM2" \
    "$TOKEN_EXPIRING" \
    "$TOKEN_BEFORE_RETARGET" \
    "$TOKEN_AFTER_RETARGET" \
    "$TOKEN_TARGET_UNAVAILABLE"; do
    [[ -n "$value" ]] || continue
    grep -Fq -- "$value" "$metrics_file" &&
      fail "share metrics on node $node leaked a confidential share, tenant, token, or VM identifier"
  done
}

assert_metrics_for_node() {
  local node="$1"
  local metrics_file="$2"
  local request_series=""
  request_series="$(grep -c '^taritd_share_requests_total{' "$metrics_file")"
  [[ "$request_series" == "18" ]] ||
    fail "share request metrics on node $node must expose exactly 18 bounded visibility/status series"
  assert_metric_secrecy "$metrics_file" "$node"
  share_gauges_are_zero_if_exposed "$metrics_file"
}

assert_metrics() {
  local node=""
  local metrics_file=""
  local bytes_in=""
  local bytes_out=""

  for node in a b; do
    control_request "$node" GET /metrics ""
    expect_status 200 "share metrics endpoint on node $node"
    metrics_file="$(mktemp "$RUN_DIR/share-metrics-$node.XXXXXX")"
    cp -- "$LAST_BODY" "$metrics_file"
    private_path "$metrics_file"
    assert_metrics_for_node "$node" "$metrics_file"
    if [[ "$node" != "b" ]]; then
      continue
    fi

    bytes_in="$(metric_value "$metrics_file" taritd_share_bytes_in_total)"
    bytes_out="$(metric_value "$metrics_file" taritd_share_bytes_out_total)"
  [[ "$bytes_in" =~ ^[0-9]+$ && "$bytes_out" =~ ^[0-9]+$ ]] ||
    fail "share byte metrics must be numeric"
  (( bytes_in >= TARIT_E2E_UPLOAD_BYTES_EFFECTIVE )) ||
    fail "share input byte metric did not observe the large upload"
  (( bytes_out >= TARIT_E2E_STREAM_BYTES_EFFECTIVE )) ||
    fail "share output byte metric did not observe the 32 MiB stream"
  done
}

node_shutdown_started() {
  "$TIMEOUT_BIN" 1s grep -q 'shutdown signal received; draining HTTP listeners' "$NODE_A_LOG"
}

assert_no_vmm_sockets() {
  local socket_dir="$1"
  [[ -d "$socket_dir" ]] || return 0
  ! find "$socket_dir" -type s -print -quit | grep -q .
}

start_node() {
  local node="$1"
  local host_id=""
  local control_port=""
  local share_port=""
  local node_dir=""
  local node_log=""
  local max_vms=""

  case "$node" in
    a)
      host_id="$NODE_A_HOST"
      control_port="$CONTROL_PORT_A"
      share_port="$SHARE_PORT_A"
      node_dir="$NODE_A_DIR"
      node_log="$NODE_A_LOG"
      max_vms="${TARIT_E2E_NODE_A_MAX_VMS:-2}"
      ;;
    b)
      host_id="$NODE_B_HOST"
      control_port="$CONTROL_PORT_B"
      share_port="$SHARE_PORT_B"
      node_dir="$NODE_B_DIR"
      node_log="$NODE_B_LOG"
      max_vms="${TARIT_E2E_NODE_B_MAX_VMS:-1}"
      ;;
    *)
      fail "unknown node '$node'"
      ;;
  esac

  (
    unset TARIT_API_KEY TARIT_API_KEYS TARIT_CONFIG TARIT_DB TARIT_SOCKET_DIR
    export TARIT_API_KEYS="$API_KEY:$OWNER_KEY:admin:0"
    export TARIT_PEER_SECRET="$PEER_SECRET"
    export TARIT_DATABASE_URL="$DATABASE_URL"
    export TARIT_HOST_ID="$host_id"
    export TARIT_LISTEN="127.0.0.1:$control_port"
    export TARIT_SHARE_LISTEN="127.0.0.1:$share_port"
    export TARIT_SHARE_DOMAIN="$SHARE_DOMAIN"
    export TARIT_SHARE_TOKEN_KEY="$SHARE_TOKEN_KEY"
    export TARIT_SHARE_TOKEN_TTL_SECS="${TARIT_E2E_TOKEN_TTL_SECS:-3}"
    export TARIT_SHARE_CONNECT_TIMEOUT_MS="${TARIT_E2E_CONNECT_TIMEOUT_MS:-5000}"
    export TARIT_SHARE_IDLE_TIMEOUT_SECS="${TARIT_E2E_IDLE_TIMEOUT_SECS:-45}"
    export TARIT_RPC_ADDR="http://127.0.0.1:$control_port"
    export TARIT_VMM_BIN="$VMM_LAUNCHER"
    export TARIT_E2E_VMM_REAL="$VMM_BIN_REAL"
    export TARIT_E2E_VMM_RACE_ARM="$RACE_VMM_ARM"
    export TARIT_E2E_VMM_RACE_READY="$RACE_VMM_READY"
    export TARIT_E2E_VMM_RACE_RELEASE="$RACE_VMM_RELEASE"
    export TARIT_KERNEL="$KERNEL"
    export TARIT_ROOTFS="$ROOTFS"
    export TARIT_ROOTFS_READONLY=1
    export TARIT_ENABLE_NET=1
    export TARIT_MAX_VMS="$max_vms"
    export TARIT_MAX_VCPUS="${TARIT_E2E_MAX_VCPUS:-8}"
    export TARIT_MAX_MEMORY_MIB="${TARIT_E2E_MAX_MEMORY_MIB:-4096}"
    export TARIT_WARM_POOL=0
    export TARIT_REAP_ON_SHUTDOWN=1
    export TARIT_SOCKET_DIR="$node_dir/sockets"
    export TARIT_DB="$node_dir/taritd.sqlite"
    export TARIT_NET_STATE="$node_dir/net-state.json"
    export TARIT_IMAGES_DIR="$node_dir/images"
    export TARIT_CONFIG="$node_dir/absent-config.toml"
    export RUST_LOG="${RUST_LOG:-taritd=info,tower_http=warn}"
    exec "$TARITD_BIN_REAL" serve
  ) >"$node_log" 2>&1 &

  if [[ "$node" == "a" ]]; then
    NODE_A_PID="$!"
  else
    NODE_B_PID="$!"
  fi
}

start_local_postgres() {
  DATABASE_MODE="local"
  PG_PORT="$(allocate_port)"
  [[ -z "${TARIT_E2E_POSTGRES_DIR:-}" ]] ||
    fail "TARIT_E2E_POSTGRES_DIR is unsupported: the harness creates a private mktemp data directory"
  grant_postgres_run_access
  PG_DATA_DIR="$(mktemp -d "$RUN_DIR/postgres.XXXXXX")" ||
    fail "could not create a private local PostgreSQL data directory"
  chown "$PG_OS_USER" "$PG_DATA_DIR"
  chmod 0700 "$PG_DATA_DIR"
  [[ "$(stat -c '%a' -- "$PG_DATA_DIR")" == "700" ]] ||
    fail "local PostgreSQL data directory is not owner-private"
  run_as_pg_user "$INITDB_BIN" -D "$PG_DATA_DIR" \
    --auth=trust --no-locale --encoding=UTF8 --username=tarit_e2e \
    >"$PG_DATA_DIR/initdb.log" 2>&1 ||
    fail "isolated PostgreSQL initialization failed; inspect $PG_DATA_DIR/initdb.log"
  run_as_pg_user "$PG_CTL_BIN" -D "$PG_DATA_DIR" \
    -l "$PG_DATA_DIR/postgres.log" \
    -o "-h 127.0.0.1 -p $PG_PORT" \
    -w -t 30 start >/dev/null || {
    record_local_postgres_pid || true
    fail "isolated PostgreSQL did not start; inspect $PG_DATA_DIR/postgres.log"
  }
  record_local_postgres_pid ||
    fail "isolated PostgreSQL did not publish a valid postmaster PID"
  DATABASE_URL="postgresql://tarit_e2e@127.0.0.1:$PG_PORT/postgres?sslmode=disable"
  "$TIMEOUT_BIN" 15s "$PSQL_BIN" "$DATABASE_URL" --no-psqlrc -qAtc 'SELECT 1' >/dev/null ||
    fail "isolated PostgreSQL did not accept a connection"
}

configure_database() {
  if [[ -n "$REQUESTED_DATABASE_URL" ]]; then
    DATABASE_MODE="external"
    DATABASE_URL="$REQUESTED_DATABASE_URL"
    "$TIMEOUT_BIN" 15s "$PSQL_BIN" "$DATABASE_URL" --no-psqlrc -qAtc 'SELECT 1' >/dev/null ||
      fail "TARIT_DATABASE_URL is not reachable"
    cleanup_database_rows
  else
    start_local_postgres
  fi
}

set_deterministic_share_slug() {
  local conflict_count=""
  local applied_slug=""

  conflict_count="$("$TIMEOUT_BIN" 12s "$PSQL_BIN" "$DATABASE_URL" --no-psqlrc -qAt \
    -v ON_ERROR_STOP=1 \
    -v slug="$EDGE_SLUG" \
    -v share_id="$SHARE_ID" \
    -c "SELECT count(*) FROM fleet_shares WHERE slug = :'slug' AND id <> :'share_id';")" ||
    fail "could not check deterministic Caddy hostname availability"
  [[ "$conflict_count" == "0" ]] ||
    fail "deterministic Caddy hostname is already owned by a different share"
  applied_slug="$("$TIMEOUT_BIN" 12s "$PSQL_BIN" "$DATABASE_URL" --no-psqlrc -qAt \
    -v ON_ERROR_STOP=1 \
    -v slug="$EDGE_SLUG" \
    -v share_id="$SHARE_ID" \
    -v owner_key="$OWNER_KEY" \
    -c "UPDATE fleet_shares SET slug = :'slug' WHERE id = :'share_id' AND owner_key = :'owner_key' RETURNING slug;")" ||
    fail "could not set the deterministic Caddy hostname"
  [[ "$applied_slug" == "$EDGE_SLUG" ]] ||
    fail "deterministic Caddy hostname update did not affect exactly this run's share"
  SHARE_SLUG="$EDGE_SLUG"
}

write_caddy_config() {
  CADDY_HOME="$(mktemp -d "$RUN_DIR/caddy-home.XXXXXX")"
  CADDY_XDG_DATA_HOME="$(mktemp -d "$RUN_DIR/caddy-data.XXXXXX")"
  CADDY_XDG_CONFIG_HOME="$(mktemp -d "$RUN_DIR/caddy-config.XXXXXX")"
  CADDY_XDG_CACHE_HOME="$(mktemp -d "$RUN_DIR/caddy-cache.XXXXXX")"
  CADDY_STORAGE_ROOT="$(mktemp -d "$RUN_DIR/caddy-storage.XXXXXX")"
  CADDY_CONFIG="$(mktemp "$RUN_DIR/Caddyfile.XXXXXX")"
  CADDY_LOG="$(mktemp "$RUN_DIR/caddy.log.XXXXXX")"
  private_path "$CADDY_HOME"
  private_path "$CADDY_XDG_DATA_HOME"
  private_path "$CADDY_XDG_CONFIG_HOME"
  private_path "$CADDY_XDG_CACHE_HOME"
  private_path "$CADDY_STORAGE_ROOT"
  private_path "$CADDY_CONFIG"
  private_path "$CADDY_LOG"
  CADDY_CA_CERT="$CADDY_STORAGE_ROOT/pki/authorities/local/root.crt"
  CADDY_CA_KEY="$CADDY_STORAGE_ROOT/pki/authorities/local/root.key"
  cat >"$CADDY_CONFIG" <<CADDY
{
  auto_https off
  admin off
  skip_install_trust
  storage file_system "$CADDY_STORAGE_ROOT"
}

https://$EDGE_HOST:$CADDY_PORT {
  tls internal

  @internal path /internal/v1 /internal/v1/*
  handle @internal {
    respond "edge-internal-route-blocked" 404
  }

  handle {
    reverse_proxy 127.0.0.1:$SHARE_PORT_B {
      header_up -X-API-Key
      header_up -Proxy-Authorization
      header_up -X-Peer-Secret
      header_up -Forwarded
      header_up -X-Forwarded-For
      header_up -X-Forwarded-Host
      header_up -X-Forwarded-Proto
      header_up -X-Real-IP
      header_up Host {host}
      header_up X-Forwarded-For {remote_host}
      header_up X-Forwarded-Host {host}
      header_up X-Forwarded-Proto {scheme}
      header_up Forwarded "for={remote_host};host={host};proto={scheme}"
    }
  }
}
CADDY
  private_path "$CADDY_CONFIG"
}

caddy_ca_is_ready() {
  [[ -r "$CADDY_CA_CERT" && -r "$CADDY_CA_KEY" ]]
}

caddy_edge_is_ready() {
  share_request GET /__caddy-edge-ready ""
  [[ "$LAST_STATUS" == "404" ]]
}

start_caddy_edge() {
  write_caddy_config
  "$TIMEOUT_BIN" 15s "$CADDY_BIN_REAL" validate \
    --config "$CADDY_CONFIG" --adapter caddyfile >"$CADDY_LOG" 2>&1 ||
    fail "Caddy rejected the generated TLS edge configuration; inspect $CADDY_LOG"
  (
    export HOME="$CADDY_HOME"
    export XDG_DATA_HOME="$CADDY_XDG_DATA_HOME"
    export XDG_CONFIG_HOME="$CADDY_XDG_CONFIG_HOME"
    export XDG_CACHE_HOME="$CADDY_XDG_CACHE_HOME"
    exec "$CADDY_BIN_REAL" run --config "$CADDY_CONFIG" --adapter caddyfile
  ) >>"$CADDY_LOG" 2>&1 &
  CADDY_PID="$!"
  wait_until "Caddy internal CA material" 15 caddy_ca_is_ready
  private_path "$CADDY_CA_KEY"
  wait_until "Caddy TLS edge with verified CA" 15 caddy_edge_is_ready
}

write_guest_server() {
  GUEST_SERVER_SOURCE="$(mktemp "$RUN_DIR/guest-share-server.XXXXXX")"
  cat >"$GUEST_SERVER_SOURCE" <<'NODE'
const crypto = require("crypto");
const http = require("http");

const port = Number(process.argv[2]);
const instance = process.argv[3];
const stats = {
  stream_drains: 0,
  upload_pauses: 0,
  ws_pings: 0,
  ws_pongs: 0,
  ws_abrupt_disconnects: 0,
};

function sendJson(response, status, body) {
  const encoded = Buffer.from(JSON.stringify(body));
  response.writeHead(status, {
    "content-type": "application/json",
    "content-length": String(encoded.length),
  });
  response.end(encoded);
}

function sendFrame(socket, opcode, payload) {
  const body = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
  let header;
  if (body.length < 126) {
    header = Buffer.from([0x80 | opcode, body.length]);
  } else if (body.length < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x80 | opcode;
    header[1] = 126;
    header.writeUInt16BE(body.length, 2);
  } else {
    throw new Error("test WebSocket frame unexpectedly large");
  }
  socket.write(Buffer.concat([header, body]));
}

function headerCounts(request) {
  const counts = {};
  for (let index = 0; index < request.rawHeaders.length; index += 2) {
    const name = request.rawHeaders[index].toLowerCase();
    counts[name] = (counts[name] || 0) + 1;
  }
  return counts;
}

function websocketAccept(key) {
  return crypto
    .createHash("sha1")
    .update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`)
    .digest("base64");
}

function attachWebSocket(request, socket, head) {
  const key = request.headers["sec-websocket-key"];
  if (typeof key !== "string") {
    socket.destroy();
    return;
  }
  socket.write(
    "HTTP/1.1 101 Switching Protocols\r\n" +
      "Upgrade: websocket\r\n" +
      "Connection: Upgrade\r\n" +
      `Sec-WebSocket-Accept: ${websocketAccept(key)}\r\n\r\n`,
  );
  let buffered = head;
  let graceful = false;

  function consume() {
    while (buffered.length >= 2) {
      const first = buffered[0];
      const second = buffered[1];
      const opcode = first & 0x0f;
      const masked = (second & 0x80) !== 0;
      let length = second & 0x7f;
      let offset = 2;
      if (!masked) {
        socket.destroy();
        return;
      }
      if (length === 126) {
        if (buffered.length < 4) return;
        length = buffered.readUInt16BE(2);
        offset = 4;
      } else if (length === 127) {
        socket.destroy();
        return;
      }
      if (buffered.length < offset + 4 + length) return;
      const mask = buffered.subarray(offset, offset + 4);
      const payload = Buffer.from(buffered.subarray(offset + 4, offset + 4 + length));
      for (let index = 0; index < payload.length; index += 1) {
        payload[index] ^= mask[index % 4];
      }
      buffered = buffered.subarray(offset + 4 + length);

      if (opcode === 0x1 || opcode === 0x2) {
        sendFrame(socket, opcode, payload);
      } else if (opcode === 0x9) {
        stats.ws_pings += 1;
        sendFrame(socket, 0x0a, payload);
        sendFrame(socket, 0x1, `server-saw-ping:${payload.toString("hex")}`);
      } else if (opcode === 0x0a) {
        stats.ws_pongs += 1;
        sendFrame(socket, 0x1, `server-saw-pong:${payload.toString("hex")}`);
      } else if (opcode === 0x8) {
        graceful = true;
        sendFrame(socket, 0x8, payload);
        socket.end();
      } else {
        socket.destroy();
      }
    }
  }

  socket.on("data", (chunk) => {
    buffered = Buffer.concat([buffered, chunk]);
    consume();
  });
  socket.on("close", () => {
    if (!graceful) stats.ws_abrupt_disconnects += 1;
  });
  socket.on("error", () => {});
  sendFrame(socket, 0x9, Buffer.from("gateway-ping"));
  consume();
}

const server = http.createServer((request, response) => {
  const url = new URL(request.url, "http://guest.invalid");
  if (url.pathname === "/") {
    sendJson(response, 200, { instance, method: request.method, url: request.url });
    return;
  }
  if (url.pathname === "/nested/one/two") {
    sendJson(response, 200, { instance, method: request.method, url: request.url });
    return;
  }
  if (url.pathname === "/inspect") {
    sendJson(response, 200, {
      instance,
      method: request.method,
      url: request.url,
      headers: request.headers,
      header_counts: headerCounts(request),
    });
    return;
  }
  if (url.pathname === "/stats") {
    sendJson(response, 200, { instance, ...stats });
    return;
  }
  if (url.pathname === "/delayed") {
    response.writeHead(200, { "content-type": "text/plain" });
    response.flushHeaders();
    setTimeout(() => response.end("delayed-first-chunk"), 350);
    return;
  }
  if (url.pathname === "/stream") {
    const bytes = Number(url.searchParams.get("bytes") || "0");
    const chunkSize = Number(url.searchParams.get("chunk") || "65536");
    if (!Number.isSafeInteger(bytes) || bytes < 1 || !Number.isSafeInteger(chunkSize) || chunkSize < 1) {
      sendJson(response, 400, { error: "invalid stream shape" });
      return;
    }
    response.writeHead(200, {
      "content-type": "application/octet-stream",
      "content-length": String(bytes),
    });
    void (async () => {
      let remaining = bytes;
      while (remaining > 0) {
        const chunk = Buffer.alloc(Math.min(chunkSize, remaining), "Z");
        remaining -= chunk.length;
        if (!response.write(chunk)) {
          stats.stream_drains += 1;
          await new Promise((resolve) => response.once("drain", resolve));
        }
      }
      response.end();
    })().catch(() => response.destroy());
    return;
  }
  if (url.pathname === "/upload") {
    const digest = crypto.createHash("sha256");
    let bytes = 0;
    let nextPause = 1024 * 1024;
    request.on("data", (chunk) => {
      bytes += chunk.length;
      digest.update(chunk);
      if (bytes >= nextPause) {
        nextPause += 1024 * 1024;
        stats.upload_pauses += 1;
        request.pause();
        setTimeout(() => request.resume(), 5);
      }
    });
    request.on("end", () => {
      sendJson(response, 200, { instance, bytes, sha256: digest.digest("hex") });
    });
    request.on("error", () => response.destroy());
    return;
  }
  sendJson(response, 404, { error: "not found" });
});

server.on("upgrade", (request, socket, head) => {
  const url = new URL(request.url, "http://guest.invalid");
  if (url.pathname !== "/ws") {
    socket.write("HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
    socket.destroy();
    return;
  }
  attachWebSocket(request, socket, head);
});

server.listen(port, "0.0.0.0");
NODE

  private_path "$GUEST_SERVER_SOURCE"
  GUEST_SERVER_B64="$("$TIMEOUT_BIN" 10s python3 - "$GUEST_SERVER_SOURCE" <<'PY'
import base64
import gzip
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_bytes()
print(base64.b64encode(gzip.compress(source, mtime=0)).decode("ascii"))
PY
)"
}

start_guest_server() {
  local vm_id="$1"
  local command=""
  command="mkdir -p /run/tarit-e2e && node -e \"require('fs').writeFileSync('/run/tarit-e2e/server.js',require('zlib').gunzipSync(Buffer.from('$GUEST_SERVER_B64','base64')))\" && (node /run/tarit-e2e/server.js '$GUEST_PORT' '$vm_id' >/run/tarit-e2e/server.log 2>&1 & echo share-server-started)"
  (( ${#command} < 3900 )) ||
    fail "compressed guest server command exceeds the guest exec line limit"
  exec_guest_or_fail "$vm_id" "$command" "start guest HTTP/WebSocket server"
}

write_websocket_client() {
  WS_CLIENT="$(mktemp "$RUN_DIR/ws-client.XXXXXX")"
  cat >"$WS_CLIENT" <<'PY'
#!/usr/bin/env python3
import base64
import os
import socket
import ssl
import struct
import sys


def read_exact(sock, count):
    data = bytearray()
    while len(data) < count:
        part = sock.recv(count - len(data))
        if not part:
            raise RuntimeError("unexpected WebSocket EOF")
        data.extend(part)
    return bytes(data)


def send_frame(sock, opcode, payload):
    if isinstance(payload, str):
        payload = payload.encode()
    mask = os.urandom(4)
    size = len(payload)
    if size < 126:
        header = bytes([0x80 | opcode, 0x80 | size])
    elif size < 65536:
        header = bytes([0x80 | opcode, 0x80 | 126]) + struct.pack("!H", size)
    else:
        raise RuntimeError("test frame unexpectedly large")
    masked = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
    sock.sendall(header + mask + masked)


def receive_frame(sock):
    first, second = read_exact(sock, 2)
    opcode = first & 0x0F
    masked = bool(second & 0x80)
    size = second & 0x7F
    if size == 126:
        size = struct.unpack("!H", read_exact(sock, 2))[0]
    elif size == 127:
        size = struct.unpack("!Q", read_exact(sock, 8))[0]
    payload = read_exact(sock, size)
    if masked:
        mask = read_exact(sock, 4)
        payload = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
    return opcode, payload


def receive_until(sock, expected_text=None, expected_opcode=None, expected_payload=None):
    for _ in range(32):
        opcode, payload = receive_frame(sock)
        if opcode == 0x9:
            send_frame(sock, 0xA, payload)
            continue
        if expected_text is not None and opcode == 0x1 and payload.decode() == expected_text:
            return
        if expected_opcode is not None and opcode == expected_opcode and payload == expected_payload:
            return
        if opcode == 0x8:
            raise RuntimeError("peer closed before expected WebSocket frame")
    raise RuntimeError("expected WebSocket frame was not observed")


def connect(port, host, ca_cert):
    raw = socket.create_connection(("127.0.0.1", port), timeout=10)
    context = ssl.create_default_context(cafile=ca_cert)
    sock = context.wrap_socket(raw, server_hostname=host)
    sock.settimeout(10)
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        "GET /ws HTTP/1.1\r\n"
        f"Host: {host}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n\r\n"
    ).encode()
    sock.sendall(request)
    response = b""
    while not response.endswith(b"\r\n\r\n"):
        part = sock.recv(1)
        if not part:
            raise RuntimeError("WebSocket handshake EOF")
        response += part
    if not response.startswith(b"HTTP/1.1 101 "):
        raise RuntimeError(f"WebSocket handshake failed: {response[:80]!r}")
    return sock


def exercise(port, host, ca_cert, abrupt):
    sock = connect(port, host, ca_cert)
    receive_until(sock, expected_text="server-saw-pong:676174657761792d70696e67")

    text = "text-through-non-owner"
    send_frame(sock, 0x1, text)
    receive_until(sock, expected_opcode=0x1, expected_payload=text.encode())

    binary = bytes(range(256))
    send_frame(sock, 0x2, binary)
    receive_until(sock, expected_opcode=0x2, expected_payload=binary)

    ping = b"client-ping"
    send_frame(sock, 0x9, ping)
    receive_until(sock, expected_opcode=0xA, expected_payload=ping)
    receive_until(sock, expected_text="server-saw-ping:636c69656e742d70696e67")

    if abrupt:
        sock.shutdown(socket.SHUT_RDWR)
        sock.close()
        print("WS_ABRUPT_PASS")
        return

    close_payload = struct.pack("!H", 1000) + b"normal-close"
    send_frame(sock, 0x8, close_payload)
    opcode, payload = receive_frame(sock)
    if opcode != 0x8:
        raise RuntimeError("graceful WebSocket close was not preserved")
    sock.close()
    print("WS_GRACEFUL_PASS")


if __name__ == "__main__":
    if len(sys.argv) != 5 or sys.argv[4] not in {"graceful", "abrupt"}:
        raise SystemExit("usage: ws_client.py PORT HOST CA_CERT graceful|abrupt")
    exercise(int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4] == "abrupt")
PY
  chmod 0700 "$WS_CLIENT"
  private_path "$WS_CLIENT"
}

run_websocket_gate() {
  local host=""
  local graceful_output=""
  local abrupt_output=""
  host="$(share_host)"
  graceful_output="$(mktemp "$RUN_DIR/ws-graceful.XXXXXX")"
  abrupt_output="$(mktemp "$RUN_DIR/ws-abrupt.XXXXXX")"
  private_path "$graceful_output"
  private_path "$abrupt_output"
  "$TIMEOUT_BIN" 20s python3 "$WS_CLIENT" "$CADDY_PORT" "$host" "$CADDY_CA_CERT" graceful >"$graceful_output"
  grep -qx 'WS_GRACEFUL_PASS' "$graceful_output" ||
    fail "WebSocket text/binary/ping/pong/graceful-close gate failed"

  "$TIMEOUT_BIN" 20s python3 "$WS_CLIENT" "$CADDY_PORT" "$host" "$CADDY_CA_CERT" abrupt >"$abrupt_output"
  grep -qx 'WS_ABRUPT_PASS' "$abrupt_output" ||
    fail "WebSocket abrupt-disconnect gate failed"
  wait_until "share HTTP and WebSocket gauge cleanup" 20 share_gauges_are_zero
}

race_vmm_barrier_reached() {
  [[ -s "$RACE_VMM_READY" ]]
}

assert_rejected_vm_has_no_state() {
  local vm_id="$1"
  local vmm_pid="$2"
  local fleet_count=""

  VM_ID="$vm_id" NODE_DB="$NODE_A_DIR/taritd.sqlite" "$TIMEOUT_BIN" 10s python3 - <<'PY'
import os
import sqlite3

db_path = os.environ["NODE_DB"]
vm_id = os.environ["VM_ID"]
connection = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
try:
    count = connection.execute("SELECT count(*) FROM vms WHERE id = ?", (vm_id,)).fetchone()[0]
finally:
    connection.close()
if count != 0:
    raise SystemExit("FAIL: rejected shutdown create persisted a local VM record")
PY
  fleet_count="$("$TIMEOUT_BIN" 12s "$PSQL_BIN" "$DATABASE_URL" --no-psqlrc -qAt \
    -v ON_ERROR_STOP=1 \
    -v vm_id="$vm_id" \
    -c "SELECT count(*) FROM fleet_vms WHERE id = :'vm_id';")" ||
    fail "could not query authoritative fleet state after shutdown rejection"
  [[ "$fleet_count" == "0" ]] ||
    fail "rejected shutdown create persisted an authoritative fleet VM record"
  pid_is_gone "$vmm_pid" ||
    fail "rejected shutdown create left its resolved VMM PID running"
  [[ ! -e "$NODE_A_DIR/sockets/$vm_id.sock" ]] ||
    fail "rejected shutdown create left a VMM socket"
  VM_ID="$vm_id" NET_STATE="$NODE_A_DIR/net-state.json" "$TIMEOUT_BIN" 10s python3 - <<'PY'
import json
import os
from pathlib import Path

path = Path(os.environ["NET_STATE"])
if not path.is_file():
    raise SystemExit("FAIL: missing network state while checking shutdown cleanup")
data = json.loads(path.read_text(encoding="utf-8"))
if any(entry.get("vm_id") == os.environ["VM_ID"] for entry in data.get("allocations", [])):
    raise SystemExit("FAIL: rejected shutdown create retained a network allocation")
PY
  if nft list table ip taritd_nat 2>/dev/null | grep -Fq -- "$vm_id"; then
    fail "rejected shutdown create retained an nft network allocation"
  fi
}

stop_node_a_for_shutdown_gate() {
  [[ -n "$NODE_A_PID" ]] || fail "node A PID was not tracked"
  local tracked_pid="$NODE_A_PID"
  local request_body=""
  local request_headers=""
  local request_status=""
  local request_stderr=""
  local proposed_id_file=""
  local status=""

  pid_matches_owned_binary "$tracked_pid" "$TARITD_BIN_REAL" ||
    fail "node A PID changed before shutdown test"
  PROPOSED_VM_ID="$("$TIMEOUT_BIN" 5s python3 - <<'PY'
import uuid
print(uuid.uuid4())
PY
)"
  proposed_id_file="$(mktemp "$RUN_DIR/shutdown-race-vm-id.XXXXXX")"
  printf '%s\n' "$PROPOSED_VM_ID" >"$proposed_id_file"
  private_path "$proposed_id_file"
  request_body="$(write_json_body "$(create_vm_payload "$PROPOSED_VM_ID")")"
  request_headers="$(mktemp "$RUN_DIR/shutdown-race-headers.XXXXXX")"
  request_status="$(mktemp "$RUN_DIR/shutdown-race-status.XXXXXX")"
  request_stderr="$(mktemp "$RUN_DIR/shutdown-race-curl-stderr.XXXXXX")"
  SHUTDOWN_RACE_BODY="$(mktemp "$RUN_DIR/shutdown-race-body.XXXXXX")"
  private_path "$request_headers"
  private_path "$request_status"
  private_path "$request_stderr"
  private_path "$SHUTDOWN_RACE_BODY"
  : >"$RACE_VMM_ARM"
  private_path "$RACE_VMM_ARM"

  curl --noproxy '*' --silent --show-error \
    --connect-timeout 3 --max-time 20 \
    -X POST \
    -H "X-API-Key: $API_KEY" \
    -H 'Content-Type: application/json' \
    --data-binary "@$request_body" \
    -D "$request_headers" \
    -o "$SHUTDOWN_RACE_BODY" \
    -w '%{http_code}' \
    "$CONTROL_URL_A/v1/vms" >"$request_status" 2>"$request_stderr" &
  local request_pid="$!"
  wait_until "deterministic in-flight VM create barrier" 15 race_vmm_barrier_reached
  private_path "$RACE_VMM_READY"
  RACE_VMM_PID="$(cat "$RACE_VMM_READY")"
  [[ "$RACE_VMM_PID" =~ ^[0-9]+$ ]] ||
    fail "deterministic in-flight create did not publish a VMM PID"
  pid_belongs_to_this_run "$RACE_VMM_PID" ||
    fail "deterministic in-flight create barrier PID is not owned by this run"

  kill -TERM "$tracked_pid"
  wait_until "node A shutdown admission closure" 15 node_shutdown_started
  : >"$RACE_VMM_RELEASE"
  private_path "$RACE_VMM_RELEASE"
  wait_for_pid_exit "$request_pid" 25 ||
    fail "in-flight shutdown create did not return before its bounded curl deadline"
  status="$(cat "$request_status")"
  [[ -n "$status" && "$status" != "000" ]] ||
    fail "in-flight shutdown create did not receive an API response (HTTP 000 is not a pass)"
  [[ "$status" == "429" ]] ||
    fail "in-flight shutdown create must receive HTTP 429 after admission closes"
  json_assert_exact_error "$SHUTDOWN_RACE_BODY" "taritd is shutting down"
  wait_for_pid_exit "$RACE_VMM_PID" 20 ||
    fail "rejected in-flight create did not reap its VMM process"

  wait_for_pid_exit "$tracked_pid" 35 ||
    fail "node A did not complete coordinated shutdown"
  NODE_A_PID=""
  grep -q 'shutdown drain summary: reaped local VMs' "$NODE_A_LOG" ||
    fail "node A did not report its VM reaping shutdown sweep"
  assert_no_vmm_sockets "$NODE_A_DIR/sockets" ||
    fail "node A left a VMM socket after coordinated shutdown"
  assert_rejected_vm_has_no_state "$PROPOSED_VM_ID" "$RACE_VMM_PID"
  VM_IDS=()
  VMM_PIDS=()
}

stop_node_b_after_gate() {
  [[ -n "$NODE_B_PID" ]] || fail "node B PID was not tracked"
  local tracked_pid="$NODE_B_PID"
  terminate_expected_pid "$tracked_pid" "$TARITD_BIN_REAL" "node B"
  NODE_B_PID=""
  grep -q 'shutdown drain summary: reaped local VMs' "$NODE_B_LOG" ||
    fail "node B did not report its coordinated shutdown sweep"
}

main() {
  allocate_listener_ports
  CONTROL_URL_A="http://127.0.0.1:$CONTROL_PORT_A"
  CONTROL_URL_B="http://127.0.0.1:$CONTROL_PORT_B"
  TARIT_E2E_STREAM_BYTES_EFFECTIVE="${TARIT_E2E_STREAM_BYTES:-33554432}"
  TARIT_E2E_UPLOAD_BYTES_EFFECTIVE="${TARIT_E2E_UPLOAD_BYTES:-33554432}"
  if ! [[ "$TARIT_E2E_STREAM_BYTES_EFFECTIVE" =~ ^[0-9]+$ ]] ||
    (( TARIT_E2E_STREAM_BYTES_EFFECTIVE < 33554432 )); then
    fail "TARIT_E2E_STREAM_BYTES must be at least 33554432"
  fi
  if ! [[ "$TARIT_E2E_UPLOAD_BYTES_EFFECTIVE" =~ ^[0-9]+$ ]] ||
    (( TARIT_E2E_UPLOAD_BYTES_EFFECTIVE < 1048576 )); then
    fail "TARIT_E2E_UPLOAD_BYTES must be at least 1048576"
  fi

  log "== setting up isolated PostgreSQL fleet state =="
  configure_database

  log "== starting node A and node B with independent control/share listeners =="
  capture_host_networking
  write_vmm_launcher
  NETWORK_START_ATTEMPTED=1
  start_node a
  wait_until "node A health" 45 wait_for_health a
  record_owned_host_networking
  start_node b
  wait_until "node B health" 45 wait_for_health b
  wait_until "two-node fleet membership" 45 wait_for_cluster

  write_guest_server
  write_websocket_client

  log "== booting two real networked KVM guests on node A =="
  create_vm_on_node_a
  VM1="$CREATED_VM_ID"
  wait_until "VM1 running through non-owner node B" 90 wait_for_vm_running "$VM1"
  wait_until "Node.js guest agent on VM1" 90 guest_command_is_ready "$VM1"
  start_guest_server "$VM1"

  create_vm_on_node_a
  VM2="$CREATED_VM_ID"
  wait_until "VM2 running through non-owner node B" 90 wait_for_vm_running "$VM2"
  wait_until "Node.js guest agent on VM2" 90 guest_command_is_ready "$VM2"
  start_guest_server "$VM2"

  log "== creating public share through non-owner node B =="
  api_json b POST /v1/shares "$(create_share_payload "$VM1" public)"
  expect_status 201 "create public share through node B"
  SHARE_ID="$(json_get "$LAST_BODY" id)"
  SHARE_SLUG="$(json_get "$LAST_BODY" slug)"
  json_assert_eq "$LAST_BODY" vm_id "$VM1"
  json_assert_eq "$LAST_BODY" guest_port "$GUEST_PORT"
  json_assert_eq "$LAST_BODY" visibility public
  [[ "$SHARE_ID" =~ ^[0-9a-f-]{36}$ && "$SHARE_SLUG" =~ ^[a-z0-9-]+$ ]] ||
    fail "share creation did not return a valid id and hostname label"
  set_deterministic_share_slug
  api_empty b GET "/v1/shares/$SHARE_ID"
  expect_status 200 "read deterministic share hostname through node B"
  json_assert_eq "$LAST_BODY" slug "$EDGE_SLUG"
  start_caddy_edge

  api_empty b GET "/v1/vms/$VM1"
  expect_status 200 "non-owner VM lookup"
  json_assert_eq "$LAST_BODY" host_id "$NODE_A_HOST"

  assert_listener_isolation
  assert_caddy_internal_route_blocked
  assert_malformed_hosts
  assert_peer_rejections

  log "== Caddy TLS public HTTP, root/nested path, and trusted forwarding gate =="
  share_request GET / ""
  expect_status 200 "public root request"
  json_assert_eq "$LAST_BODY" instance "$VM1"
  json_assert_eq "$LAST_BODY" url /

  share_request GET '/nested/one/two?one=1&two=two' ""
  expect_status 200 "nested share path"
  json_assert_eq "$LAST_BODY" instance "$VM1"
  json_assert_eq "$LAST_BODY" url '/nested/one/two?one=1&two=two'

  APP_AUTHORIZATION='Bearer tarit-e2e-application-authorization'
  share_request PATCH '/inspect?query=preserved&repeat=a&repeat=b' 'client-token-must-not-reach-guest' \
    -H "Authorization: $APP_AUTHORIZATION" \
    -H "X-API-Key: $API_KEY" \
    -H "X-Peer-Secret: $PEER_SECRET" \
    -H 'X-Forwarded-Proto: http' \
    -H 'X-Forwarded-Proto: attacker' \
    -H 'Forwarded: for=attacker.example;proto=http' \
    -H 'X-Forwarded-For: attacker.example' \
    -H 'X-Real-IP: attacker.example'
  expect_status 200 "header preservation request"
  json_assert_eq "$LAST_BODY" method PATCH
  json_assert_eq "$LAST_BODY" url '/inspect?query=preserved&repeat=a&repeat=b'
  json_assert_forwarding_boundary "$LAST_BODY" "$EDGE_HOST" "$APP_AUTHORIZATION"

  assert_delayed_first_chunk

  log "== streaming response and upload backpressure gate =="
  run_stream_sha_gate
  share_request GET /stats ""
  expect_status 200 "streaming backpressure statistics"
  json_assert_int_at_least "$LAST_BODY" stream_drains 1

  run_large_upload_gate
  share_request GET /stats ""
  expect_status 200 "upload backpressure statistics"
  json_assert_int_at_least "$LAST_BODY" upload_pauses 1

  log "== WebSocket text, binary, ping/pong, close, and abrupt-disconnect gate =="
  run_websocket_gate

  log "== private share token and malformed-token gate =="
  api_json b PATCH "/v1/shares/$SHARE_ID" "$(patch_visibility_payload private)"
  expect_status 200 "make share private"
  json_assert_eq "$LAST_BODY" visibility private
  PRIVATE_VERSION="$(json_get "$LAST_BODY" token_version)"

  share_request GET / ""
  expect_status 401 "anonymous private-share request"

  share_request GET / 'not-a-valid-share-token'
  expect_status 401 "malformed private-share token"

  share_request GET / "" \
    -H 'X-Tarit-Share-Token: first' \
    -H 'X-Tarit-Share-Token: second'
  expect_status 401 "duplicate private-share token headers"

  share_request GET '/?token=not-accepted-in-query' ""
  expect_status 401 "query-string private-share token"

  api_empty b POST "/v1/shares/$SHARE_ID/tokens"
  expect_status 200 "issue expiring private-share token"
  TOKEN_EXPIRING="$(json_get "$LAST_BODY" token)"
  [[ -n "$TOKEN_EXPIRING" ]] || fail "token issuance returned an empty token"
  share_request GET / "$TOKEN_EXPIRING"
  expect_status 200 "valid private-share token"
  json_assert_eq "$LAST_BODY" instance "$VM1"

  token_has_expired() {
    share_request GET / "$TOKEN_EXPIRING"
    [[ "$LAST_STATUS" == "401" ]]
  }
  wait_until "short-lived share token expiry" 12 token_has_expired

  api_empty b POST "/v1/shares/$SHARE_ID/tokens"
  expect_status 200 "issue token before target rotation"
  TOKEN_BEFORE_RETARGET="$(json_get "$LAST_BODY" token)"

  log "== retarget and token-version rotation gate =="
  api_json b PATCH "/v1/shares/$SHARE_ID" "$(patch_share_payload "$VM2")"
  expect_status 200 "retarget share to VM2 through node B"
  json_assert_eq "$LAST_BODY" vm_id "$VM2"
  json_assert_eq "$LAST_BODY" visibility private
  RETARGET_VERSION="$(json_get "$LAST_BODY" token_version)"
  (( RETARGET_VERSION == PRIVATE_VERSION + 1 )) ||
    fail "retarget must increment the private-share token version exactly once"

  share_request GET / "$TOKEN_BEFORE_RETARGET"
  expect_status 401 "pre-retarget token after token-version rotation"

  api_empty b POST "/v1/shares/$SHARE_ID/tokens"
  expect_status 200 "issue token after retarget"
  TOKEN_AFTER_RETARGET="$(json_get "$LAST_BODY" token)"
  share_request GET / "$TOKEN_AFTER_RETARGET"
  expect_status 200 "retargeted private-share request"
  json_assert_eq "$LAST_BODY" instance "$VM2"

  log "== stopped target and revoke gate =="
  api_empty a DELETE "/v1/vms/$VM2"
  expect_status 204 "stop retargeted VM"

  api_empty b POST "/v1/shares/$SHARE_ID/tokens"
  expect_status 200 "issue token for stopped target"
  TOKEN_TARGET_UNAVAILABLE="$(json_get "$LAST_BODY" token)"
  share_request GET / "$TOKEN_TARGET_UNAVAILABLE"
  expect_status 503 "stopped share target"
  json_assert_exact_error "$LAST_BODY" "share unavailable"

  api_empty b DELETE "/v1/shares/$SHARE_ID"
  expect_status 204 "revoke retargeted share"
  share_request GET / "$TOKEN_TARGET_UNAVAILABLE"
  expect_status 404 "revoked share"

  log "== metrics secrecy, bounded cardinality, and gauge cleanup gate =="
  wait_until "final share gauge cleanup" 20 share_gauges_are_zero
  assert_metrics

  log "== coordinated shutdown and post-shutdown admission gate =="
  stop_node_a_for_shutdown_gate
  stop_caddy
  stop_node_b_after_gate
  restore_host_networking ||
    fail "guest-network host state could not be restored safely"
  stop_local_postgres ||
    fail "isolated PostgreSQL did not complete PID-specific shutdown"
  restore_run_dir_permissions ||
    fail "per-run artifact directory could not be restored to private permissions"

  log "SHARES_PASS"
}

main "$@"
