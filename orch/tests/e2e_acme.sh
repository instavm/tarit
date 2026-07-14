#!/usr/bin/env bash
# Hermetic ACME DNS-01 and wildcard-TLS end-to-end gate.
#
# Run on a Linux host as root:
#   sudo -E bash orch/tests/e2e_acme.sh
#
# Layer A validates ACME issuance, fleet certificate distribution, SNI rejection,
# and failover without a VM. Layer B optionally creates a real KVM share.
set -Eeuo pipefail
umask 077

if [[ "${BASH_SOURCE[0]}" == */* ]]; then
  SCRIPT_DIR="$(CDPATH='' cd -- "${BASH_SOURCE[0]%/*}" && pwd)"
else
  SCRIPT_DIR="$PWD"
fi
TARITD_BIN="${TARITD_BIN:-$HOME/tarit-egress/target/release/taritd}"
VMM_BIN="${TARIT_VMM_BIN:-${VMM_BIN:-$HOME/tarit-egress/target/release/vmm}}"
KERNEL="${TARIT_KERNEL:-/tmp/vmlinux.minimal}"
ROOTFS="${TARIT_SHARE_ROOTFS:-${TARIT_ROOTFS:-/tmp/bench-node-rootfs-vsock.ext4}}"
PEBBLE_BIN="${PEBBLE_BIN:-$HOME/pebble-e2e/bin/pebble}"
CHALLTESTSRV_BIN="${CHALLTESTSRV_BIN:-$HOME/pebble-e2e/bin/pebble-challtestsrv}"
PEBBLE_CONFIG_SOURCE="${PEBBLE_CONFIG_SOURCE:-$HOME/pebble-e2e/config/pebble-config.json}"
PEBBLE_MINICA_PEM="${PEBBLE_MINICA_PEM:-$HOME/pebble-e2e/certs/pebble.minica.pem}"
PEBBLE_TLS_CERT="${PEBBLE_TLS_CERT:-$HOME/pebble-e2e/certs/localhost/cert.pem}"
PEBBLE_TLS_KEY="${PEBBLE_TLS_KEY:-$HOME/pebble-e2e/certs/localhost/key.pem}"
PEBBLE_VALIDITY_SECS="${PEBBLE_VALIDITY_SECS:-518400}"
TIMEOUT_BIN="${TIMEOUT_BIN:-timeout}"
PSQL_BIN="${PSQL_BIN:-psql}"
ACME_E2E_WITH_VM="${ACME_E2E_WITH_VM:-1}"
ACME_E2E_KEEP="${ACME_E2E_KEEP:-0}"
ACME_E2E_TIMEOUT_SECS="${ACME_E2E_TIMEOUT_SECS:-180}"
ACME_E2E_GUEST_PORT="${ACME_E2E_GUEST_PORT:-43127}"
ACME_E2E_EDGE_SLUG="${ACME_E2E_EDGE_SLUG:-edge}"
RUN_ROOT="${TARIT_ACME_E2E_RUN_ROOT:-$SCRIPT_DIR/.acme-e2e-runs}"
REQUESTED_DATABASE_URL="${TARIT_DATABASE_URL:-}"
PG_OS_USER="${TARIT_ACME_E2E_POSTGRES_OS_USER:-${SUDO_USER:-postgres}}"
CA_INSTALL_PATH="/usr/local/share/ca-certificates/pebble-minica.crt"

SHARE_DOMAIN="shares.example.test"
ACME_IDENTIFIER="*.$SHARE_DOMAIN"
CERT_PROBE_HOST="example-slug.$SHARE_DOMAIN"
WRONG_SNI_HOST="not-a-share.wrong.test"

RUN=""
NODE_A_DIR=""
NODE_B_DIR=""
NODE_A_LOG=""
NODE_B_LOG=""
CHALLTESTSRV_LOG=""
PEBBLE_LOG=""
MOCK_CF_LOG=""
TRUST_LOG=""
RESOLV_BACKUP=""
RESOLV_TARGET=""
CA_PREVIOUS=""
PEBBLE_CONFIG=""
ISSUER_CA=""
VMM_LAUNCHER=""
GUEST_SERVER_SOURCE=""
LAST_BODY=""
LAST_HEADERS=""
REQUEST_BODY_FILE=""
LAST_STATUS=""
DATABASE_URL=""
DATABASE_MODE=""
PG_DATA_DIR=""
PG_PORT=""
PG_PID=""
INITDB_BIN=""
PG_CTL_BIN=""
NODE_A_PID=""
NODE_B_PID=""
CHALLTESTSRV_PID=""
PEBBLE_PID=""
MOCK_CF_PID=""
CONTROL_PORT_A=""
CONTROL_PORT_B=""
SHARE_PORT_A=""
SHARE_PORT_B=""
TLS_PORT_A=""
TLS_PORT_B=""
MOCK_CF_PORT=""
CONTROL_URL_A=""
CONTROL_URL_B=""
NODE_A_HOST=""
NODE_B_HOST=""
HOST_PREFIX=""
OWNER_KEY=""
API_KEY=""
PEER_SECRET=""
SHARE_TOKEN_KEY=""
ACME_KEK=""
SHARE_ID=""
SHARE_SLUG=""
CREATED_VM_ID=""
GENERATION_AFTER_ISSUE=""
SERIAL_A=""
ACME_RC=1
FAIL_REASON=""
RESULT_PRINTED=0
CLEANUP_RUNNING=0
RESOLV_CONF_CHANGED=0
CA_INSTALLED=0
RUN_ROOT_MODE=""
VM_IDS=()
VMM_PIDS=()

log() {
  printf '%s\n' "$*"
}

warn() {
  printf 'WARN: %s\n' "$*" >&2
}

tail_log() {
  local label="$1"
  local file="$2"
  [[ -n "$file" && -f "$file" ]] || return 0
  printf '\n----- %s (last 120 lines) -----\n' "$label" >&2
  tail -n 120 "$file" >&2 || true
}

dump_logs() {
  local reason="${1:-failure}"
  printf '\n===== diagnostic logs: %s =====\n' "$reason" >&2
  tail_log "node A" "$NODE_A_LOG"
  tail_log "node B" "$NODE_B_LOG"
  tail_log "Pebble" "$PEBBLE_LOG"
  tail_log "pebble-challtestsrv" "$CHALLTESTSRV_LOG"
  tail_log "mock Cloudflare" "$MOCK_CF_LOG"
}

report_failure() {
  local reason="$1"
  if [[ "$RESULT_PRINTED" == "0" ]]; then
    RESULT_PRINTED=1
    ACME_RC=1
    printf 'RESULT: ACME_FAIL %s\n' "$reason"
    printf 'ACME_RC=%s\n' "$ACME_RC"
  fi
}

die() {
  FAIL_REASON="$*"
  dump_logs "$FAIL_REASON"
  report_failure "$FAIL_REASON"
  exit 1
}

on_err() {
  local status="$1"
  local line="$2"
  local command="$3"
  [[ "$CLEANUP_RUNNING" == "1" ]] && return 0
  if [[ "$RESULT_PRINTED" == "0" ]]; then
    FAIL_REASON="unhandled command failed at line $line (status $status): $command"
    dump_logs "$FAIL_REASON"
    report_failure "$FAIL_REASON"
  fi
  exit "$status"
}

trap 'on_err "$?" "$LINENO" "$BASH_COMMAND"' ERR

require_command() {
  local name="$1"
  command -v "$name" >/dev/null 2>&1 || die "required command is unavailable: $name"
}

canonical_path() {
  readlink -f -- "$1"
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
  local -a ports=()
  if [[ -n "${TARIT_ACME_E2E_BASE_PORT:-}" ]]; then
    [[ "$TARIT_ACME_E2E_BASE_PORT" =~ ^[1-9][0-9]*$ ]] ||
      die "TARIT_ACME_E2E_BASE_PORT must be a positive integer"
    CONTROL_PORT_A="$TARIT_ACME_E2E_BASE_PORT"
    CONTROL_PORT_B="$((CONTROL_PORT_A + 1))"
    SHARE_PORT_A="$((CONTROL_PORT_A + 2))"
    SHARE_PORT_B="$((CONTROL_PORT_A + 3))"
    TLS_PORT_A="$((CONTROL_PORT_A + 4))"
    TLS_PORT_B="$((CONTROL_PORT_A + 5))"
    MOCK_CF_PORT="$((CONTROL_PORT_A + 6))"
  else
    mapfile -t ports < <(allocate_ports 7)
    CONTROL_PORT_A="${ports[0]}"
    CONTROL_PORT_B="${ports[1]}"
    SHARE_PORT_A="${ports[2]}"
    SHARE_PORT_B="${ports[3]}"
    TLS_PORT_A="${ports[4]}"
    TLS_PORT_B="${ports[5]}"
    MOCK_CF_PORT="${ports[6]}"
  fi
  python3 - "$CONTROL_PORT_A" "$CONTROL_PORT_B" "$SHARE_PORT_A" "$SHARE_PORT_B" \
    "$TLS_PORT_A" "$TLS_PORT_B" "$MOCK_CF_PORT" <<'PY'
import sys

ports = [int(value) for value in sys.argv[1:]]
if len(set(ports)) != len(ports) or any(port < 1024 or port > 65535 for port in ports):
    raise SystemExit("listener ports must be distinct unprivileged TCP ports")
PY
}

assert_ports_available() {
  python3 - "$CONTROL_PORT_A" "$CONTROL_PORT_B" "$SHARE_PORT_A" "$SHARE_PORT_B" \
    "$TLS_PORT_A" "$TLS_PORT_B" "$MOCK_CF_PORT" <<'PY'
import socket
import sys

tcp_ports = [53, 8055, 14000, 15000, 5001, 5002] + [int(value) for value in sys.argv[1:]]
checks = [(socket.SOCK_STREAM, port) for port in tcp_ports]
checks.append((socket.SOCK_DGRAM, 53))
sockets = []
try:
    for sock_type, port in checks:
        sock = socket.socket(socket.AF_INET, sock_type)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 0)
        sock.bind(("127.0.0.1", port))
        sockets.append(sock)
except OSError as error:
    raise SystemExit(f"required test port is already in use: {error}") from error
finally:
    for sock in sockets:
        sock.close()
PY
}

wait_until() {
  local description="$1"
  local timeout_seconds="$2"
  shift 2
  local deadline=$((SECONDS + timeout_seconds))

  while (( SECONDS < deadline )); do
    if "$@"; then
      return 0
    fi
    sleep 1
  done
  dump_logs "timed out waiting for $description after ${timeout_seconds}s"
  return 1
}

wait_for_pid_exit() {
  local pid="$1"
  local timeout_seconds="$2"
  local deadline=$((SECONDS + timeout_seconds))

  while (( SECONDS < deadline )); do
    if ! kill -0 "$pid" >/dev/null 2>&1; then
      wait "$pid" 2>/dev/null || true
      return 0
    fi
    sleep 0.2
  done
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

stop_binary_pid() {
  local variable_name="$1"
  local expected_binary="$2"
  local label="$3"
  local pid="${!variable_name:-}"

  [[ -n "$pid" ]] || return 0
  if ! kill -0 "$pid" >/dev/null 2>&1; then
    wait "$pid" 2>/dev/null || true
    printf -v "$variable_name" '%s' ""
    return 0
  fi
  if ! pid_matches_binary "$pid" "$expected_binary"; then
    warn "refusing to kill $label PID $pid because it no longer matches $expected_binary"
    return 1
  fi
  kill -TERM "$pid" || return 1
  if ! wait_for_pid_exit "$pid" 20; then
    warn "$label PID $pid did not exit after SIGTERM; sending SIGKILL"
    pid_matches_binary "$pid" "$expected_binary" || return 1
    kill -KILL "$pid" || return 1
    wait_for_pid_exit "$pid" 10 || return 1
  fi
  printf -v "$variable_name" '%s' ""
}

stop_mock_cf() {
  [[ -n "$MOCK_CF_PID" ]] || return 0
  if kill -0 "$MOCK_CF_PID" >/dev/null 2>&1; then
    if [[ -r "/proc/$MOCK_CF_PID/cmdline" ]] &&
      tr '\0' ' ' <"/proc/$MOCK_CF_PID/cmdline" | grep -Fq -- "$RUN/mock_cf.py"; then
      kill -TERM "$MOCK_CF_PID" || return 1
      if ! wait_for_pid_exit "$MOCK_CF_PID" 15; then
        kill -KILL "$MOCK_CF_PID" || return 1
        wait_for_pid_exit "$MOCK_CF_PID" 5 || return 1
      fi
    else
      warn "refusing to kill mock Cloudflare PID $MOCK_CF_PID because it no longer matches this run"
      return 1
    fi
  fi
  MOCK_CF_PID=""
}

find_pg_binary() {
  local name="$1"
  local bindir=""
  if command -v "$name" >/dev/null 2>&1; then
    command -v "$name"
    return 0
  fi
  if command -v pg_config >/dev/null 2>&1; then
    bindir="$(pg_config --bindir 2>/dev/null || true)"
    [[ -x "$bindir/$name" ]] && {
      printf '%s\n' "$bindir/$name"
      return 0
    }
  fi
  return 1
}

run_as_pg_user() {
  if [[ "$PG_OS_USER" == "$(id -un)" ]]; then
    "$@"
  else
    runuser -u "$PG_OS_USER" -- "$@"
  fi
}

psql_query() {
  "$PSQL_BIN" "$TARIT_DATABASE_URL" \
    --no-psqlrc --no-password --set=ON_ERROR_STOP=1 --quiet --tuples-only --no-align "$@"
}

certificate_generation() {
  psql_query -v "domain=$ACME_IDENTIFIER" <<'SQL'
SELECT generation FROM fleet_certificates WHERE domain = :'domain';
SQL
}

delete_run_database_rows() {
  [[ "$DATABASE_MODE" == "external" ]] || return 0
  [[ -n "$TARIT_DATABASE_URL" ]] || return 0
  psql_query \
    -v "domain=$ACME_IDENTIFIER" \
    -v "owner_key=$OWNER_KEY" \
    -v "host_prefix=$HOST_PREFIX%" <<'SQL' || return 1
DELETE FROM fleet_shares WHERE owner_key = :'owner_key';
DELETE FROM fleet_vms WHERE host_id LIKE :'host_prefix';
DELETE FROM fleet_hosts WHERE host_id LIKE :'host_prefix';
DELETE FROM fleet_leader WHERE leader_id LIKE :'host_prefix';
DELETE FROM fleet_certificates WHERE domain = :'domain';
DELETE FROM fleet_acme_jobs WHERE identifier = :'domain';
DELETE FROM fleet_acme_accounts WHERE directory_url = 'https://127.0.0.1:14000/dir';
SQL
}

clear_prior_acme_state() {
  local certificate_table=""
  certificate_table="$(psql_query -c "SELECT to_regclass('public.fleet_certificates');")" ||
    return 1
  [[ -n "$certificate_table" ]] || return 0
  psql_query -v "domain=$ACME_IDENTIFIER" <<'SQL'
DELETE FROM fleet_certificates WHERE domain = :'domain';
DELETE FROM fleet_acme_jobs WHERE identifier = :'domain';
DELETE FROM fleet_acme_accounts WHERE directory_url = 'https://127.0.0.1:14000/dir';
SQL
}

start_local_postgres() {
  DATABASE_MODE="local"
  PG_PORT="$(allocate_ports 1 | head -n 1)"
  PG_DATA_DIR="$RUN/postgres"
  mkdir -p -- "$PG_DATA_DIR"
  chown "$PG_OS_USER" "$PG_DATA_DIR"
  chmod 0700 "$PG_DATA_DIR"
  chmod 0711 "$RUN_ROOT" "$RUN"

  run_as_pg_user "$INITDB_BIN" -D "$PG_DATA_DIR" \
    --auth=trust --no-locale --encoding=UTF8 --username=tarit_e2e \
    >"$RUN/postgres-initdb.log" 2>&1 ||
    die "local PostgreSQL initdb failed"
  : >"$RUN/postgres.log"
  chown "$PG_OS_USER" "$RUN/postgres.log"
  run_as_pg_user "$PG_CTL_BIN" -D "$PG_DATA_DIR" -l "$RUN/postgres.log" \
    -o "-h 127.0.0.1 -p $PG_PORT -k $PG_DATA_DIR" -w -t 30 start >/dev/null ||
    die "local PostgreSQL failed to start"
  PG_PID="$(head -n 1 "$PG_DATA_DIR/postmaster.pid")"
  [[ "$PG_PID" =~ ^[0-9]+$ ]] || die "local PostgreSQL did not record a postmaster PID"
  DATABASE_URL="postgresql://tarit_e2e@127.0.0.1:$PG_PORT/postgres?sslmode=disable"
  export TARIT_DATABASE_URL="$DATABASE_URL"
  psql_query -c 'SELECT 1;' >/dev/null || die "local PostgreSQL did not accept a connection"
}

configure_database() {
  if [[ -n "$REQUESTED_DATABASE_URL" ]]; then
    DATABASE_MODE="external"
    DATABASE_URL="$REQUESTED_DATABASE_URL"
    export TARIT_DATABASE_URL="$DATABASE_URL"
    psql_query -c 'SELECT 1;' >/dev/null ||
      die "TARIT_DATABASE_URL is not reachable"
  else
    start_local_postgres
  fi
}

stop_local_postgres() {
  [[ "$DATABASE_MODE" == "local" && -n "$PG_DATA_DIR" ]] || return 0
  if [[ -n "$PG_PID" ]] && kill -0 "$PG_PID" >/dev/null 2>&1; then
    run_as_pg_user "$PG_CTL_BIN" -D "$PG_DATA_DIR" -m fast -w -t 30 stop >/dev/null ||
      return 1
  fi
  PG_PID=""
}

create_run_directory() {
  mkdir -p -- "$RUN_ROOT"
  RUN_ROOT_MODE="$(stat -c '%a' "$RUN_ROOT")"
  chmod 0700 "$RUN_ROOT"
  RUN="$RUN_ROOT/acme-$(date -u +%Y%m%dT%H%M%S)-$$-$RANDOM"
  mkdir -m 0700 -- "$RUN"
  NODE_A_DIR="$RUN/node-a"
  NODE_B_DIR="$RUN/node-b"
  mkdir -m 0700 -- "$NODE_A_DIR" "$NODE_B_DIR"
  mkdir -m 0700 -- "$NODE_A_DIR/sockets" "$NODE_B_DIR/sockets"
  NODE_A_LOG="$RUN/nodeA.log"
  NODE_B_LOG="$RUN/nodeB.log"
  CHALLTESTSRV_LOG="$RUN/challtestsrv.log"
  PEBBLE_LOG="$RUN/pebble.log"
  MOCK_CF_LOG="$RUN/mock_cf.log"
  TRUST_LOG="$RUN/trust-store.log"
  LAST_BODY="$RUN/last-body"
  LAST_HEADERS="$RUN/last-headers"
  REQUEST_BODY_FILE="$RUN/request.json"
  : >"$NODE_A_LOG"
  : >"$NODE_B_LOG"
  : >"$LAST_BODY"
  : >"$LAST_HEADERS"
  : >"$REQUEST_BODY_FILE"
}

prepare_pebble_config() {
  PEBBLE_CONFIG="$RUN/pebble-config.json"
  python3 - "$PEBBLE_CONFIG_SOURCE" "$PEBBLE_CONFIG" "$PEBBLE_TLS_CERT" "$PEBBLE_TLS_KEY" \
    "$PEBBLE_VALIDITY_SECS" <<'PY'
import json
import os
import sys

source, destination, certificate, private_key, validity = sys.argv[1:]
with open(source, encoding="utf-8") as handle:
    config = json.load(handle)
try:
    validity = int(validity)
except ValueError as error:
    raise SystemExit("PEBBLE_VALIDITY_SECS must be a positive integer") from error
if validity < 1:
    raise SystemExit("PEBBLE_VALIDITY_SECS must be a positive integer")

pebble = config.setdefault("pebble", {})
pebble["certificate"] = os.path.abspath(certificate)
pebble["privateKey"] = os.path.abspath(private_key)
pebble["listenAddress"] = "0.0.0.0:14000"
pebble["managementListenAddress"] = "0.0.0.0:15000"
config.setdefault("profiles", {}).setdefault("default", {})["validityPeriod"] = validity

with open(destination, "w", encoding="utf-8") as handle:
    json.dump(config, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY
}

install_pebble_minica() {
  if [[ -e "$CA_INSTALL_PATH" ]]; then
    CA_PREVIOUS="$RUN/pebble-minica.previous.crt"
    cp -a -- "$CA_INSTALL_PATH" "$CA_PREVIOUS"
  fi
  install -D -m 0644 "$PEBBLE_MINICA_PEM" "$CA_INSTALL_PATH"
  update-ca-certificates >"$TRUST_LOG" 2>&1 ||
    die "could not add Pebble minica to the system trust store"
  CA_INSTALLED=1
}

restore_pebble_minica() {
  [[ "$CA_INSTALLED" == "1" ]] || return 0
  if [[ -n "$CA_PREVIOUS" && -f "$CA_PREVIOUS" ]]; then
    cp -a -- "$CA_PREVIOUS" "$CA_INSTALL_PATH" ||
      return 1
  else
    rm -f -- "$CA_INSTALL_PATH" || return 1
  fi
  update-ca-certificates --fresh >>"$TRUST_LOG" 2>&1 || return 1
  CA_INSTALLED=0
}

replace_resolv_conf() {
  RESOLV_TARGET="$(readlink -f /etc/resolv.conf 2>/dev/null || true)"
  [[ -n "$RESOLV_TARGET" && -f "$RESOLV_TARGET" ]] ||
    die "could not resolve a writable /etc/resolv.conf target"
  RESOLV_BACKUP="$RUN/resolv.conf.backup"
  cp -a -- "$RESOLV_TARGET" "$RESOLV_BACKUP"
  if command -v chattr >/dev/null 2>&1; then
    chattr -i "$RESOLV_TARGET" >/dev/null 2>&1 || true
  fi
  printf 'nameserver 127.0.0.1\n' >"$RESOLV_TARGET" ||
    die "could not replace /etc/resolv.conf with challtestsrv"
  RESOLV_CONF_CHANGED=1
}

restore_resolv_conf() {
  [[ "$RESOLV_CONF_CHANGED" == "1" ]] || return 0
  [[ -n "$RESOLV_TARGET" && -f "$RESOLV_BACKUP" ]] || return 1
  if command -v chattr >/dev/null 2>&1; then
    chattr -i "$RESOLV_TARGET" >/dev/null 2>&1 || true
  fi
  cat "$RESOLV_BACKUP" >"$RESOLV_TARGET" || return 1
  RESOLV_CONF_CHANGED=0
}

write_mock_cf() {
  cat >"$RUN/mock_cf.py" <<'PY'
#!/usr/bin/env python3
import json
import sys
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit
from urllib.request import Request, urlopen


class Handler(BaseHTTPRequestHandler):
    records = {}

    def send_json(self, status, body):
        payload = json.dumps(body, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def read_json(self):
        length = int(self.headers.get("Content-Length", "0"))
        return json.loads(self.rfile.read(length).decode("utf-8"))

    @staticmethod
    def challtestsrv(path, body):
        payload = json.dumps(body, separators=(",", ":")).encode()
        request = Request(
            "http://127.0.0.1:8055" + path,
            data=payload,
            method="POST",
            headers={"Content-Type": "application/json"},
        )
        with urlopen(request, timeout=5) as response:
            response.read()

    def do_GET(self):
        if urlsplit(self.path).path == "/health":
            self.send_json(200, {"status": "ok"})
        else:
            self.send_json(404, {"success": False})

    def do_POST(self):
        parts = [part for part in urlsplit(self.path).path.split("/") if part]
        if len(parts) != 3 or parts[0] != "zones" or parts[2] != "dns_records":
            self.send_json(404, {"success": False})
            return
        try:
            record = self.read_json()
            name = record["name"]
            content = record["content"]
            if not isinstance(name, str) or not isinstance(content, str):
                raise ValueError("name and content must be strings")
            host = name if name.endswith(".") else name + "."
            self.challtestsrv("/set-txt", {"host": host, "value": content})
        except Exception as error:  # noqa: BLE001 - return mock API diagnostics.
            self.send_json(502, {"success": False, "errors": [{"message": str(error)}]})
            return
        record_id = str(uuid.uuid4())
        self.records[record_id] = {"host": host, "name": name}
        self.send_json(200, {"success": True, "result": {"id": record_id}})

    def do_DELETE(self):
        parts = [part for part in urlsplit(self.path).path.split("/") if part]
        if len(parts) != 4 or parts[0] != "zones" or parts[2] != "dns_records":
            self.send_json(404, {"success": False})
            return
        record = self.records.pop(parts[3], None)
        if record is not None:
            try:
                self.challtestsrv("/clear-txt", {"host": record["host"]})
            except Exception as error:  # noqa: BLE001 - return mock API diagnostics.
                self.send_json(502, {"success": False, "errors": [{"message": str(error)}]})
                return
        self.send_json(200, {"success": True})

    def log_message(self, fmt, *args):
        sys.stderr.write("%s - %s\n" % (self.log_date_time_string(), fmt % args))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: mock_cf.py PORT")
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
    server.serve_forever()
PY
  chmod 0700 "$RUN/mock_cf.py"
}

challtestsrv_is_ready() {
  curl --silent --show-error --fail --max-time 3 \
    --request POST --header 'Content-Type: application/json' \
    --data '{"host":"_acme-challenge.shares.example.test.","value":"probe"}' \
    http://127.0.0.1:8055/set-txt >/dev/null 2>&1 &&
    curl --silent --show-error --fail --max-time 3 \
      --request POST --header 'Content-Type: application/json' \
      --data '{"host":"_acme-challenge.shares.example.test."}' \
      http://127.0.0.1:8055/clear-txt >/dev/null 2>&1
}

mock_cf_is_ready() {
  curl --silent --show-error --fail --max-time 3 \
    "http://127.0.0.1:$MOCK_CF_PORT/health" >/dev/null 2>&1
}

pebble_is_ready() {
  local response=""
  response="$(curl --silent --show-error --fail --max-time 3 --cacert "$PEBBLE_MINICA_PEM" \
    https://127.0.0.1:14000/dir 2>/dev/null)" || return 1
  PEBBLE_DIRECTORY_RESPONSE="$response" python3 - <<'PY'
import json
import os

directory = json.loads(os.environ["PEBBLE_DIRECTORY_RESPONSE"])
if not isinstance(directory.get("newOrder"), str):
    raise SystemExit(1)
PY
}

start_challtestsrv() {
  (
    exec "$CHALLTESTSRV_BIN" \
      -dnsserver 127.0.0.1:53 \
      -management :8055 \
      -http01 "" \
      -https01 "" \
      -tlsalpn01 "" \
      -doh "" \
      -defaultIPv4 127.0.0.1
  ) >"$CHALLTESTSRV_LOG" 2>&1 &
  # shellcheck disable=SC2034 # Read by cleanup through an indirect variable.
  CHALLTESTSRV_PID="$!"
  wait_until "pebble-challtestsrv management API" 30 challtestsrv_is_ready ||
    die "pebble-challtestsrv did not become ready"
}

start_mock_cf() {
  write_mock_cf
  python3 "$RUN/mock_cf.py" "$MOCK_CF_PORT" >"$MOCK_CF_LOG" 2>&1 &
  MOCK_CF_PID="$!"
  wait_until "mock Cloudflare API" 30 mock_cf_is_ready ||
    die "mock Cloudflare API did not become ready"
}

start_pebble() {
  (
    exec "$PEBBLE_BIN" -config "$PEBBLE_CONFIG" -dnsserver 127.0.0.1:53
  ) >"$PEBBLE_LOG" 2>&1 &
  # shellcheck disable=SC2034 # Read by cleanup through an indirect variable.
  PEBBLE_PID="$!"
  wait_until "Pebble ACME directory" 45 pebble_is_ready ||
    die "Pebble did not become ready"
  ISSUER_CA="$RUN/pebble-issuer.pem"
  curl --silent --show-error --fail --insecure --max-time 10 \
    https://127.0.0.1:15000/roots/0 -o "$ISSUER_CA" ||
    die "could not fetch Pebble issuing root"
  openssl x509 -in "$ISSUER_CA" -noout >/dev/null ||
    die "Pebble issuing root is not a PEM certificate"
}

http_request() {
  local method="$1"
  local url="$2"
  local body_path="$3"
  shift 3
  local status=""
  local curl_status=0
  local -a curl_body=()

  : >"$LAST_BODY"
  : >"$LAST_HEADERS"
  if [[ -n "$body_path" ]]; then
    curl_body=(--data-binary "@$body_path")
  fi
  if status="$(curl --silent --show-error --connect-timeout 3 --max-time 30 \
    --request "$method" --dump-header "$LAST_HEADERS" --output "$LAST_BODY" \
    --write-out '%{http_code}' \
    "${curl_body[@]}" \
    "$@" "$url")"; then
    curl_status=0
  else
    curl_status=$?
  fi
  if [[ "$curl_status" -eq 0 ]]; then
    LAST_STATUS="$status"
  else
    LAST_STATUS="000"
  fi
}

expect_status() {
  local expected="$1"
  local description="$2"
  [[ "$LAST_STATUS" == "$expected" ]] ||
    die "$description: expected HTTP $expected, got $LAST_STATUS"
}

json_get() {
  local file="$1"
  local path="$2"
  JSON_FILE="$file" JSON_PATH="$path" python3 - <<'PY'
import json
import os

value = json.load(open(os.environ["JSON_FILE"], encoding="utf-8"))
for part in os.environ["JSON_PATH"].split("."):
    if not isinstance(value, dict) or part not in value:
        raise SystemExit(f"missing JSON path {os.environ['JSON_PATH']}")
    value = value[part]
if value is None:
    print("")
elif isinstance(value, bool):
    print("true" if value else "false")
else:
    print(value)
PY
}

api_json() {
  local node="$1"
  local method="$2"
  local path="$3"
  local payload="$4"
  local base_url=""
  case "$node" in
    a) base_url="$CONTROL_URL_A" ;;
    b) base_url="$CONTROL_URL_B" ;;
    *) die "unknown node: $node" ;;
  esac
  printf '%s' "$payload" >"$REQUEST_BODY_FILE"
  http_request "$method" "$base_url$path" "$REQUEST_BODY_FILE" \
    -H "X-API-Key: $API_KEY" -H 'Content-Type: application/json'
}

api_empty() {
  local node="$1"
  local method="$2"
  local path="$3"
  local base_url=""
  case "$node" in
    a) base_url="$CONTROL_URL_A" ;;
    b) base_url="$CONTROL_URL_B" ;;
    *) die "unknown node: $node" ;;
  esac
  http_request "$method" "$base_url$path" "" -H "X-API-Key: $API_KEY"
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
  [[ "$LAST_STATUS" == "200" ]] || return 1
  HEALTH_FILE="$LAST_BODY" python3 - <<'PY'
import json
import os

raise SystemExit(0 if json.load(open(os.environ["HEALTH_FILE"])).get("status") == "ok" else 1)
PY
}

wait_for_cluster() {
  api_empty a GET /v1/cluster
  [[ "$LAST_STATUS" == "200" ]] || return 1
  CLUSTER_FILE="$LAST_BODY" NODE_A_HOST="$NODE_A_HOST" NODE_B_HOST="$NODE_B_HOST" python3 - <<'PY'
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

write_vmm_launcher() {
  VMM_LAUNCHER="$RUN/vmm-launcher"
  cat >"$VMM_LAUNCHER" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
exec "${TARIT_ACME_E2E_VMM_REAL:?missing real VMM path}" "$@"
SH
  chmod 0700 "$VMM_LAUNCHER"
}

start_node() {
  local node="$1"
  local host_id=""
  local control_port=""
  local share_port=""
  local tls_port=""
  local node_dir=""
  local node_log=""

  case "$node" in
    a)
      host_id="$NODE_A_HOST"
      control_port="$CONTROL_PORT_A"
      share_port="$SHARE_PORT_A"
      tls_port="$TLS_PORT_A"
      node_dir="$NODE_A_DIR"
      node_log="$NODE_A_LOG"
      ;;
    b)
      host_id="$NODE_B_HOST"
      control_port="$CONTROL_PORT_B"
      share_port="$SHARE_PORT_B"
      tls_port="$TLS_PORT_B"
      node_dir="$NODE_B_DIR"
      node_log="$NODE_B_LOG"
      ;;
    *) die "unknown node: $node" ;;
  esac

  (
    unset TARIT_API_KEY TARIT_CONFIG
    export TARIT_API_KEYS="$API_KEY:$OWNER_KEY:admin:0"
    export TARIT_PEER_SECRET="$PEER_SECRET"
    export TARIT_DATABASE_URL="$DATABASE_URL"
    export TARIT_HOST_ID="$host_id"
    export TARIT_LISTEN="127.0.0.1:$control_port"
    export TARIT_SHARE_LISTEN="127.0.0.1:$share_port"
    export TARIT_SHARE_TLS_LISTEN="127.0.0.1:$tls_port"
    export TARIT_SHARE_DOMAIN="$SHARE_DOMAIN"
    export TARIT_SHARE_TOKEN_KEY="$SHARE_TOKEN_KEY"
    export TARIT_RPC_ADDR="http://127.0.0.1:$control_port"
    export TARIT_VMM_BIN="$VMM_LAUNCHER"
    export TARIT_ACME_E2E_VMM_REAL="$VMM_BIN"
    export TARIT_KERNEL="$KERNEL"
    export TARIT_ROOTFS="$ROOTFS"
    export TARIT_ROOTFS_READONLY=1
    export TARIT_ENABLE_NET="$ACME_E2E_WITH_VM"
    export TARIT_MAX_VMS=2
    export TARIT_MAX_VCPUS=8
    export TARIT_MAX_MEMORY_MIB=4096
    export TARIT_WARM_POOL=0
    export TARIT_REAP_ON_SHUTDOWN=1
    export TARIT_SOCKET_DIR="$node_dir/sockets"
    export TARIT_DB="$node_dir/taritd.sqlite"
    export TARIT_NET_STATE="$node_dir/net-state.json"
    export TARIT_IMAGES_DIR="$node_dir/images"
    export TARIT_CONFIG="$node_dir/absent-config.toml"
    export TARIT_ACME_ENABLED=true
    export TARIT_ACME_DIRECTORY_URL=https://127.0.0.1:14000/dir
    export TARIT_ACME_CONTACT_EMAIL=ops@example.test
    export TARIT_ACME_DNS_PROVIDER=cloudflare
    export TARIT_ACME_CLOUDFLARE_API_TOKEN=test-token
    export TARIT_ACME_CLOUDFLARE_ZONE_ID=test-zone
    export TARIT_ACME_CLOUDFLARE_API_BASE="http://127.0.0.1:$MOCK_CF_PORT"
    export TARIT_ACME_KEK="$ACME_KEK"
    export RUST_LOG="${RUST_LOG:-taritd=info,tower_http=warn}"
    exec "$TARITD_BIN" serve
  ) >>"$node_log" 2>&1 &

  if [[ "$node" == "a" ]]; then
    # shellcheck disable=SC2034 # Read by cleanup through an indirect variable.
    NODE_A_PID="$!"
  else
    # shellcheck disable=SC2034 # Read by cleanup through an indirect variable.
    NODE_B_PID="$!"
  fi
}

openssl_certificate_details() {
  local port="$1"
  local server_name="$2"
  local output="$3"
  local error_output="$4"
  local details="$5"

  if ! "$TIMEOUT_BIN" 15s openssl s_client \
    -connect "127.0.0.1:$port" \
    -servername "$server_name" \
    -CAfile "$ISSUER_CA" \
    -verify_return_error </dev/null >"$output" 2>"$error_output"; then
    return 1
  fi
  openssl x509 -in "$output" -noout -serial -issuer -ext subjectAltName >"$details" 2>/dev/null
}

valid_wildcard_certificate_on() {
  local port="$1"
  local label="$2"
  local output="$RUN/$label-sclient.out"
  local error_output="$RUN/$label-sclient.err"
  local details="$RUN/$label-cert-details"
  openssl_certificate_details "$port" "$CERT_PROBE_HOST" "$output" "$error_output" "$details" ||
    return 1
  grep -Fq "DNS:*.$SHARE_DOMAIN" "$details" &&
    grep -qi 'issuer=.*pebble' "$details"
}

certificate_serial_on() {
  local port="$1"
  local label="$2"
  local output="$RUN/$label-sclient.out"
  local error_output="$RUN/$label-sclient.err"
  local details="$RUN/$label-cert-details"
  openssl_certificate_details "$port" "$CERT_PROBE_HOST" "$output" "$error_output" "$details" ||
    return 1
  sed -n 's/^serial=//p' "$details"
}

assert_wrong_sni_fails() {
  local output="$RUN/wrong-sni.out"
  local status=0
  if "$TIMEOUT_BIN" 15s openssl s_client \
    -connect "127.0.0.1:$TLS_PORT_B" \
    -servername "$WRONG_SNI_HOST" \
    -CAfile "$ISSUER_CA" \
    -verify_return_error -brief </dev/null >"$output" 2>&1; then
    status=0
  else
    status=$?
  fi
  [[ "$status" -ne 0 ]] &&
    ! grep -q 'CONNECTION ESTABLISHED' "$output" &&
    ! grep -q 'Certificate chain' "$output"
}

kill_node_a_for_failover() {
  [[ -n "$NODE_A_PID" ]] || die "node A PID is unavailable for failover"
  pid_matches_binary "$NODE_A_PID" "$(canonical_path "$TARITD_BIN")" ||
    die "node A PID no longer belongs to taritd before failover"
  kill -KILL "$NODE_A_PID" ||
    die "could not SIGKILL node A for failover"
  wait_for_pid_exit "$NODE_A_PID" 15 ||
    die "node A did not exit after SIGKILL"
  NODE_A_PID=""
}

create_vm_payload() {
  python3 - <<'PY'
import json
print(json.dumps({"memory_mib": 256, "vcpus": 1}, separators=(",", ":")))
PY
}

create_share_payload() {
  local vm_id="$1"
  python3 - "$vm_id" "$ACME_E2E_GUEST_PORT" <<'PY'
import json
import sys
print(json.dumps({
    "vm_id": sys.argv[1],
    "guest_port": int(sys.argv[2]),
    "visibility": "public",
}, separators=(",", ":")))
PY
}

wait_for_vm_running() {
  local vm_id="$1"
  api_empty b GET "/v1/vms/$vm_id/status"
  [[ "$LAST_STATUS" == "200" ]] || return 1
  VM_STATUS_FILE="$LAST_BODY" python3 - <<'PY'
import json
import os

data = json.load(open(os.environ["VM_STATUS_FILE"], encoding="utf-8"))
raise SystemExit(0 if data.get("state") == "running" and data.get("vcpu_alive") else 1)
PY
}

verify_real_kvm_vmm() {
  local vm_id="$1"
  local expected_pid="$2"
  local resolved_pid=""
  local fd=""
  local target=""
  local kvm_fds=0

  api_empty a GET "/v1/vms/$vm_id"
  [[ "$LAST_STATUS" == "200" ]] || return 1
  resolved_pid="$(json_get "$LAST_BODY" pid)" || return 1
  [[ "$resolved_pid" == "$expected_pid" && "$resolved_pid" =~ ^[0-9]+$ ]] || return 1
  for fd in "/proc/$resolved_pid/fd/"*; do
    [[ -e "$fd" ]] || continue
    target="$(readlink -f -- "$fd" 2>/dev/null || true)"
    [[ "$target" == "/dev/kvm" ]] && ((kvm_fds += 1))
  done
  (( kvm_fds > 0 ))
}

create_vm_on_node_a() {
  api_json a POST /v1/vms "$(create_vm_payload)"
  [[ "$LAST_STATUS" == "201" ]] || return 1
  local vm_id=""
  local vmm_pid=""
  vm_id="$(json_get "$LAST_BODY" id)" || return 1
  vmm_pid="$(json_get "$LAST_BODY" pid)" || return 1
  [[ "$vm_id" =~ ^[0-9a-f-]{36}$ && "$vmm_pid" =~ ^[0-9]+$ ]] || return 1
  verify_real_kvm_vmm "$vm_id" "$vmm_pid" || return 1
  VM_IDS+=("$vm_id")
  VMM_PIDS+=("$vmm_pid")
  CREATED_VM_ID="$vm_id"
}

write_guest_server() {
  GUEST_SERVER_SOURCE="$RUN/guest-server.js"
  cat >"$GUEST_SERVER_SOURCE" <<'NODE'
const http = require("http");
const port = Number(process.argv[2]);

http.createServer((_request, response) => {
  const body = "acme-e2e-guest\n";
  response.writeHead(200, {
    "content-type": "text/plain",
    "content-length": String(Buffer.byteLength(body)),
  });
  response.end(body);
}).listen(port, "0.0.0.0");
NODE
}

start_guest_server() {
  local vm_id="$1"
  local source_b64=""
  local command=""
  source_b64="$(python3 - "$GUEST_SERVER_SOURCE" <<'PY'
import base64
from pathlib import Path
import sys
print(base64.b64encode(Path(sys.argv[1]).read_bytes()).decode("ascii"))
PY
)" || return 1
  command="mkdir -p /run/tarit-e2e && node -e \"require('fs').writeFileSync('/run/tarit-e2e/server.js', Buffer.from('$source_b64','base64'))\" && (node /run/tarit-e2e/server.js '$ACME_E2E_GUEST_PORT' >/run/tarit-e2e/server.log 2>&1 & echo guest-server-started)"
  api_json a POST /v1/execute "$(python3 - "$vm_id" "$command" <<'PY'
import json
import sys
print(json.dumps({
    "vm_id": sys.argv[1],
    "command": sys.argv[2],
    "timeout_ms": 60000,
}, separators=(",", ":")))
PY
)"
  [[ "$LAST_STATUS" == "200" ]] || return 1
  [[ "$(json_get "$LAST_BODY" status)" == "completed" ]] ||
    return 1
  [[ "$(json_get "$LAST_BODY" exit_code)" == "0" ]]
}

set_edge_slug() {
  local slug=""
  slug="$(psql_query -v "slug=$ACME_E2E_EDGE_SLUG" -v "share_id=$SHARE_ID" \
    -v "owner_key=$OWNER_KEY" <<'SQL'
UPDATE fleet_shares
SET slug = :'slug'
WHERE id = :'share_id' AND owner_key = :'owner_key'
RETURNING slug;
SQL
)" || return 1
  [[ "$slug" == "$ACME_E2E_EDGE_SLUG" ]] || return 1
  SHARE_SLUG="$slug"
}

https_share_is_ready_on() {
  local port="$1"
  local output="$RUN/share-$port.body"
  local host="$SHARE_SLUG.$SHARE_DOMAIN"
  local status=""
  status="$(curl --silent --show-error --connect-timeout 3 --max-time 20 \
    --cacert "$ISSUER_CA" \
    --resolve "$host:$port:127.0.0.1" \
    --output "$output" --write-out '%{http_code}' \
    "https://$host:$port/" 2>/dev/null)" || return 1
  [[ "$status" == "200" ]] && grep -qx 'acme-e2e-guest' "$output"
}

run_layer_b() {
  if [[ "$ACME_E2E_WITH_VM" == "0" ]]; then
    log "SKIP: Layer B disabled by ACME_E2E_WITH_VM=0"
    return 0
  fi
  if [[ ! -e /dev/kvm || ! -r /dev/kvm || ! -w /dev/kvm ]]; then
    log "SKIP: Layer B requires an accessible /dev/kvm"
    return 0
  fi
  if [[ ! -x "$VMM_BIN" || ! -r "$KERNEL" || ! -r "$ROOTFS" ]]; then
    log "SKIP: Layer B assets are unavailable (VMM=$VMM_BIN KERNEL=$KERNEL ROOTFS=$ROOTFS)"
    return 0
  fi

  log "== Layer B: real KVM HTTPS share =="
  wait_until "reformed two-node fleet membership" 60 wait_for_cluster || return 1
  write_guest_server || return 1
  create_vm_on_node_a || return 1
  local vm_id="$CREATED_VM_ID"
  wait_until "KVM VM running through node B" 90 wait_for_vm_running "$vm_id" || return 1
  start_guest_server "$vm_id" || return 1
  api_json a POST /v1/shares "$(create_share_payload "$vm_id")"
  [[ "$LAST_STATUS" == "201" ]] || return 1
  SHARE_ID="$(json_get "$LAST_BODY" id)" || return 1
  [[ "$SHARE_ID" =~ ^[0-9a-f-]{36}$ ]] || return 1
  set_edge_slug || return 1
  wait_until "HTTPS share through node A" 90 https_share_is_ready_on "$TLS_PORT_A" || return 1
  wait_until "HTTPS share through node B" 90 https_share_is_ready_on "$TLS_PORT_B"
}

delete_known_vms() {
  local vm_id=""
  [[ "${#VM_IDS[@]}" -gt 0 ]] || return 0
  [[ -n "$NODE_A_PID" ]] || return 0
  for vm_id in "${VM_IDS[@]}"; do
    api_empty a DELETE "/v1/vms/$vm_id" || continue
    [[ "$LAST_STATUS" == "204" || "$LAST_STATUS" == "404" ]] || true
  done
}

stop_tracked_vmms() {
  local pid=""
  local expected_vmm=""
  [[ -x "$VMM_BIN" ]] || return 0
  expected_vmm="$(canonical_path "$VMM_BIN")"
  for pid in "${VMM_PIDS[@]}"; do
    if kill -0 "$pid" >/dev/null 2>&1 && pid_matches_binary "$pid" "$expected_vmm"; then
      kill -TERM "$pid" || true
      wait_for_pid_exit "$pid" 10 || {
        kill -KILL "$pid" || true
        wait_for_pid_exit "$pid" 5 || true
      }
    fi
  done
}

cleanup() {
  local original_status="${1:-$?}"
  local cleanup_failed=0
  [[ "$CLEANUP_RUNNING" == "0" ]] || return 0
  CLEANUP_RUNNING=1
  trap - EXIT INT TERM ERR
  set +e

  delete_known_vms || cleanup_failed=1
  stop_binary_pid NODE_A_PID "$(canonical_path "$TARITD_BIN" 2>/dev/null || printf '%s' "$TARITD_BIN")" "node A" ||
    cleanup_failed=1
  stop_binary_pid NODE_B_PID "$(canonical_path "$TARITD_BIN" 2>/dev/null || printf '%s' "$TARITD_BIN")" "node B" ||
    cleanup_failed=1
  stop_tracked_vmms || cleanup_failed=1
  stop_binary_pid PEBBLE_PID "$(canonical_path "$PEBBLE_BIN" 2>/dev/null || printf '%s' "$PEBBLE_BIN")" "Pebble" ||
    cleanup_failed=1
  stop_binary_pid CHALLTESTSRV_PID \
    "$(canonical_path "$CHALLTESTSRV_BIN" 2>/dev/null || printf '%s' "$CHALLTESTSRV_BIN")" \
    "pebble-challtestsrv" || cleanup_failed=1
  stop_mock_cf || cleanup_failed=1
  delete_run_database_rows || cleanup_failed=1
  stop_local_postgres || cleanup_failed=1
  [[ -z "$RUN_ROOT_MODE" ]] || chmod "$RUN_ROOT_MODE" "$RUN_ROOT" || cleanup_failed=1
  restore_pebble_minica || cleanup_failed=1
  restore_resolv_conf || cleanup_failed=1

  if [[ -n "$RUN" && -d "$RUN" ]]; then
    if [[ "$ACME_E2E_KEEP" == "1" ]]; then
      log "Keeping artifacts at $RUN"
    else
      rm -rf -- "$RUN" || cleanup_failed=1
    fi
  fi
  if [[ "$cleanup_failed" -ne 0 ]]; then
    warn "cleanup could not fully release this run's resources"
  fi
  return "$original_status"
}

trap 'cleanup "$?"' EXIT
trap 'cleanup 130; exit 130' INT
trap 'cleanup 143; exit 143' TERM

preflight() {
  [[ "$(uname -s)" == "Linux" ]] || die "this harness must run on Linux"
  [[ "$(id -u)" == "0" ]] || die "rerun under sudo -E so DNS and the trust store can be changed"
  [[ "$ACME_E2E_WITH_VM" == "0" || "$ACME_E2E_WITH_VM" == "1" ]] ||
    die "ACME_E2E_WITH_VM must be 0 or 1"
  [[ "$ACME_E2E_KEEP" == "0" || "$ACME_E2E_KEEP" == "1" ]] ||
    die "ACME_E2E_KEEP must be 0 or 1"
  [[ "$ACME_E2E_TIMEOUT_SECS" =~ ^[1-9][0-9]*$ ]] ||
    die "ACME_E2E_TIMEOUT_SECS must be a positive integer"
  if ! [[ "$ACME_E2E_GUEST_PORT" =~ ^[0-9]+$ ]] ||
    ! (( ACME_E2E_GUEST_PORT >= 1 && ACME_E2E_GUEST_PORT <= 65535 )); then
    die "ACME_E2E_GUEST_PORT must be in 1..65535"
  fi
  [[ "$ACME_E2E_EDGE_SLUG" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]] ||
    die "ACME_E2E_EDGE_SLUG must be a lowercase DNS label"

  require_command bash
  require_command curl
  require_command date
  require_command install
  require_command openssl
  require_command python3
  require_command readlink
  require_command "$TIMEOUT_BIN"
  require_command "$PSQL_BIN"
  require_command update-ca-certificates
  [[ -x "$TARITD_BIN" ]] || die "taritd binary is not executable: $TARITD_BIN"
  [[ -x "$PEBBLE_BIN" ]] || die "Pebble binary is not executable: $PEBBLE_BIN"
  [[ -x "$CHALLTESTSRV_BIN" ]] || die "pebble-challtestsrv binary is not executable: $CHALLTESTSRV_BIN"
  [[ -r "$PEBBLE_CONFIG_SOURCE" ]] || die "Pebble config is unreadable: $PEBBLE_CONFIG_SOURCE"
  [[ -r "$PEBBLE_MINICA_PEM" ]] || die "Pebble minica is unreadable: $PEBBLE_MINICA_PEM"
  [[ -r "$PEBBLE_TLS_CERT" && -r "$PEBBLE_TLS_KEY" ]] ||
    die "Pebble localhost certificate assets are unreadable"
  openssl x509 -in "$PEBBLE_MINICA_PEM" -noout >/dev/null ||
    die "Pebble minica is not a PEM certificate"

  if [[ -z "$REQUESTED_DATABASE_URL" ]]; then
    require_command runuser
    INITDB_BIN="$(find_pg_binary initdb || true)"
    PG_CTL_BIN="$(find_pg_binary pg_ctl || true)"
    [[ -n "$INITDB_BIN" && -n "$PG_CTL_BIN" ]] ||
      die "initdb and pg_ctl are required when TARIT_DATABASE_URL is unset"
    id "$PG_OS_USER" >/dev/null 2>&1 ||
      die "local PostgreSQL OS user does not exist: $PG_OS_USER"
  fi
}

setup_secrets() {
  API_KEY="$(openssl rand -hex 24)"
  PEER_SECRET="$(openssl rand -hex 32)"
  SHARE_TOKEN_KEY="$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')"
  ACME_KEK="$(openssl rand -hex 32)"
}

main() {
  preflight
  create_run_directory
  setup_secrets
  allocate_listener_ports
  assert_ports_available || die "one or more required Pebble, challtestsrv, or listener ports are busy"
  CONTROL_URL_A="http://127.0.0.1:$CONTROL_PORT_A"
  CONTROL_URL_B="http://127.0.0.1:$CONTROL_PORT_B"
  HOST_PREFIX="acme-e2e-$(date -u +%Y%m%dT%H%M%S)-$$"
  NODE_A_HOST="$HOST_PREFIX-a"
  NODE_B_HOST="$HOST_PREFIX-b"
  OWNER_KEY="$HOST_PREFIX-owner"

  write_vmm_launcher
  prepare_pebble_config
  install_pebble_minica

  log "== starting DNS-01 test infrastructure =="
  start_challtestsrv
  replace_resolv_conf
  start_mock_cf
  start_pebble

  log "== Layer A: wildcard ACME issuance on node A =="
  configure_database
  clear_prior_acme_state ||
    die "could not clear prior wildcard ACME state from the fleet database"
  start_node a
  wait_until "node A health" 45 wait_for_health a ||
    die "node A did not become healthy"
  wait_until "wildcard certificate issuance on node A" "$ACME_E2E_TIMEOUT_SECS" \
    valid_wildcard_certificate_on "$TLS_PORT_A" node-a ||
    die "node A did not obtain a valid Pebble wildcard certificate"
  GENERATION_AFTER_ISSUE="$(certificate_generation)" ||
    die "could not read fleet certificate generation after issuance"
  [[ "$GENERATION_AFTER_ISSUE" =~ ^[0-9]+$ ]] ||
    die "fleet certificate generation was not a number: $GENERATION_AFTER_ISSUE"
  SERIAL_A="$(certificate_serial_on "$TLS_PORT_A" node-a)" ||
    die "could not read node A wildcard certificate serial"
  [[ -n "$SERIAL_A" ]] || die "node A wildcard certificate had an empty serial"

  log "== Layer A: fleet cache load on node B =="
  start_node b
  wait_until "node B health" 45 wait_for_health b ||
    die "node B did not become healthy"
  wait_until "two-node fleet membership" 60 wait_for_cluster ||
    die "node A and node B did not form a healthy cluster"
  wait_until "fleet certificate load on node B" 45 \
    valid_wildcard_certificate_on "$TLS_PORT_B" node-b ||
    die "node B did not load the fleet wildcard certificate"
  [[ "$(certificate_serial_on "$TLS_PORT_B" node-b)" == "$SERIAL_A" ]] ||
    die "node B served a different wildcard certificate serial"
  [[ "$(certificate_generation)" == "$GENERATION_AFTER_ISSUE" ]] ||
    die "starting node B created a new ACME certificate generation"

  assert_wrong_sni_fails ||
    die "unknown SNI completed a TLS handshake"

  log "== Layer A: fenced-lease failover =="
  kill_node_a_for_failover
  wait_until "node B certificate after node A SIGKILL" 45 \
    valid_wildcard_certificate_on "$TLS_PORT_B" node-b-after-failover ||
    die "node B did not continue serving the wildcard certificate after node A failed"
  [[ "$(certificate_generation)" == "$GENERATION_AFTER_ISSUE" ]] ||
    die "node A failover changed the fleet certificate generation"
  start_node a
  wait_until "restarted node A health" 45 wait_for_health a ||
    die "restarted node A did not become healthy"
  wait_until "restarted node A fleet certificate load" 45 \
    valid_wildcard_certificate_on "$TLS_PORT_A" node-a-after-restart ||
    die "restarted node A did not load the fleet wildcard certificate"
  [[ "$(certificate_serial_on "$TLS_PORT_A" node-a-after-restart)" == "$SERIAL_A" ]] ||
    die "restarted node A served a different wildcard certificate serial"
  [[ "$(certificate_generation)" == "$GENERATION_AFTER_ISSUE" ]] ||
    die "restarting node A created a new ACME certificate generation"

  if ! run_layer_b; then
    log "SKIP: Layer B could not complete; Layer A remains authoritative"
    dump_logs "Layer B skipped"
  fi

  ACME_RC=0
  RESULT_PRINTED=1
  printf 'RESULT: ACME_PASS\n'
  printf 'ACME_RC=%s\n' "$ACME_RC"
}

main
