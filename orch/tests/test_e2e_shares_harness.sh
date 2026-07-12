#!/usr/bin/env bash
# Deterministic helper coverage for the share E2E harness; does not require KVM.
set -Eeuo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SYSTEM_STAT="$(command -v stat)"
SYSTEM_CHOWN="$(command -v chown)"
SYSTEM_CHGRP="$(command -v chgrp)"
export SYSTEM_STAT SYSTEM_CHOWN SYSTEM_CHGRP
WORK_DIR="$(mktemp -d "$SCRIPT_DIR/.e2e-shares-harness-test.XXXXXX")"
BIN_DIR="$WORK_DIR/bin"
mkdir -p -- "$BIN_DIR"
chmod 0711 "$WORK_DIR"

cleanup_test_artifacts() {
  rm -rf -- "$WORK_DIR"
}
trap cleanup_test_artifacts EXIT

cat >"$BIN_DIR/timeout" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
shift
exec "$@"
SH
chmod 0700 "$BIN_DIR/timeout"

cat >"$BIN_DIR/ip" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
if [[ "${FAKE_IP_MODE:-ok}" == "failure" ]]; then
  printf 'simulated ip probe failure\n' >&2
  exit 42
fi
printf '1: lo: <LOOPBACK> mtu 65536\n'
SH
chmod 0700 "$BIN_DIR/ip"

cat >"$BIN_DIR/stat" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
if [[ "${E2E_SHARES_HARNESS_REAL_STAT:-0}" == "1" ]]; then
  exec "$SYSTEM_STAT" "$@"
fi
if [[ "$1" == "-c" && "$2" == "%a" ]]; then
  printf '600\n'
  exit 0
fi
if [[ "$1" == "-c" && "$2" == "%g" ]]; then
  id -g
  exit 0
fi
exec /usr/bin/stat "$@"
SH
chmod 0700 "$BIN_DIR/stat"

cat >"$BIN_DIR/chown" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
if [[ -n "${FAKE_CHOWN_CALLS:-}" ]]; then
  printf '%s\0' "$@" >>"$FAKE_CHOWN_CALLS"
fi
exec "$SYSTEM_CHOWN" "$@"
SH
chmod 0700 "$BIN_DIR/chown"

cat >"$BIN_DIR/chgrp" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
if [[ -n "${FAKE_CHGRP_CALLS:-}" ]]; then
  printf '%s\0' "$@" >>"$FAKE_CHGRP_CALLS"
fi
exec "$SYSTEM_CHGRP" "$@"
SH
chmod 0700 "$BIN_DIR/chgrp"

cat >"$BIN_DIR/psql" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
printf '%s\0' "$@" >"$FAKE_PSQL_ARGS"
cat >/dev/null
if [[ "${FAKE_PSQL_MODE:-ok}" == "failure" ]]; then
  printf '%s\n' "failed" >>"$FAKE_PSQL_CALLS"
  exit 1
fi
printf '%s\n' "ok" >>"$FAKE_PSQL_CALLS"
printf '0\n'
SH
chmod 0700 "$BIN_DIR/psql"

cat >"$BIN_DIR/curl" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
printf '%s\0' "$@" >"$FAKE_CURL_ARGS"
[[ "$1" == "--config" && -r "$2" ]] || exit 64
cp -- "$2" "$FAKE_CURL_CONFIG"
printf '200'
SH
chmod 0700 "$BIN_DIR/curl"

export PATH="$BIN_DIR:$PATH"
export TARIT_E2E_TIMEOUT_BIN="$BIN_DIR/timeout"
export TARIT_E2E_SHARES_HELPERS_ONLY=1
# shellcheck source=./e2e_shares.sh
source "$SCRIPT_DIR/e2e_shares.sh"

fail_test() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

assert_nonzero() {
  local description="$1"
  shift
  if "$@"; then
    fail_test "$description unexpectedly succeeded"
  fi
}

test_probe_failure_fails_closed() {
  export FAKE_IP_MODE=failure
  assert_nonzero "failed ip probe" probe_tarit_network_artifacts
  [[ "${HOST_NETWORK_PROBE_STATUS:-}" == "42" ]] ||
    fail_test "failed ip probe status was not preserved"
  [[ "${HOST_NETWORK_PROBE_OUTPUT:-}" == *"simulated ip probe failure"* ]] ||
    fail_test "failed ip probe output was not captured"
  unset FAKE_IP_MODE
}

test_zero_taps_is_distinct_from_probe_failure() {
  if probe_tarit_network_artifacts; then
    fail_test "zero matching tap interfaces were reported as present"
  else
    [[ "$?" == "1" ]] ||
      fail_test "zero matching tap interfaces were not distinguished from probe failure"
  fi
  [[ "${HOST_NETWORK_PROBE_STATUS:-}" == "0" ]] ||
    fail_test "zero matching tap interfaces did not retain a successful command status"
}

test_metric_absence_is_failure() {
  local metrics="$WORK_DIR/metrics-missing"
  cat >"$metrics" <<'METRICS'
# TYPE taritd_share_active_http gauge
taritd_share_active_http 0
METRICS
  assert_nonzero "missing WebSocket gauge" share_gauges_are_zero "$metrics"
}

test_cleanup_sql_failure_is_reported_and_verified() {
  local pgpass="$WORK_DIR/pgpass"
  : >"$pgpass"
  chmod 0600 "$pgpass"
  export FAKE_PSQL_ARGS="$WORK_DIR/psql-args"
  export FAKE_PSQL_CALLS="$WORK_DIR/psql-calls"
  export FAKE_PSQL_MODE=failure
  export DATABASE_MODE=external
  export PSQL_BIN="$BIN_DIR/psql"
  export PGPASSFILE="$pgpass"
  export DATABASE_HOST=127.0.0.1
  export DATABASE_PORT=5432
  export DATABASE_NAME=tarit
  export DATABASE_USER=tarit
  export OWNER_KEY=helper-owner
  export HOST_PREFIX=helper-host
  assert_nonzero "SQL cleanup failure" cleanup_database_rows
  [[ "$(wc -l <"$FAKE_PSQL_CALLS" | tr -d '[:space:]')" == "2" ]] ||
    fail_test "cleanup did not run its verification query after SQL failure"
  unset FAKE_PSQL_MODE
}

test_lock_path_is_immutable() {
  [[ "$(NETWORK_LOCK_PATH="$WORK_DIR/attacker.lock" host_network_lock_path)" == "/run/lock/tarit-e2e-shares.lock" ]] ||
    fail_test "host-network lock path accepted an override"
}

test_secret_free_child_commands() {
  export FAKE_PSQL_ARGS="$WORK_DIR/psql-secret-args"
  export FAKE_PSQL_CALLS="$WORK_DIR/psql-secret-calls"
  export RUN_DIR="$WORK_DIR"
  DATABASE_URL='postgresql://tarit:database-password@database.example.test:5432/tarit?sslmode=require'
  configure_external_postgres_connection
  export PSQL_BIN="$BIN_DIR/psql"
  grep -Fq 'database-password' "$PGPASSFILE" ||
    fail_test "private PostgreSQL password file did not retain the database password"
  psql_execute <<<'SELECT 1;' >/dev/null
  if tr '\0' '\n' <"$FAKE_PSQL_ARGS" | grep -Fq -- 'database-password'; then
    fail_test "psql child argv exposed the database password"
  fi
  if tr '\0' '\n' <"$FAKE_PSQL_ARGS" | grep -Fq -- "$DATABASE_URL"; then
    fail_test "psql child argv exposed the database URL"
  fi

  LAST_BODY="$WORK_DIR/last-body"
  LAST_HEADERS="$WORK_DIR/last-headers"
  : >"$LAST_BODY"
  : >"$LAST_HEADERS"
  export FAKE_CURL_ARGS="$WORK_DIR/curl-args"
  export FAKE_CURL_CONFIG="$WORK_DIR/curl-config"
  http_request GET 'https://edge.example.test/' '' \
    -H 'X-API-Key: api-secret' \
    -H 'X-Tarit-Share-Token: share-token-secret'
  if tr '\0' '\n' <"$FAKE_CURL_ARGS" | grep -Eq 'api-secret|share-token-secret'; then
    fail_test "curl child argv exposed an API or share token"
  fi
  grep -Fq 'X-API-Key: api-secret' "$FAKE_CURL_CONFIG" ||
    fail_test "curl config did not carry the API key"
  grep -Fq 'X-Tarit-Share-Token: share-token-secret' "$FAKE_CURL_CONFIG" ||
    fail_test "curl config did not carry the share token"
}

test_fixed_run_root_rejects_environment_override_before_artifacts() {
  local rejected_root="$WORK_DIR/rejected-run-root"
  local output=""

  export TARIT_E2E_RUN_ROOT="$rejected_root"
  RUN_ROOT="$rejected_root"
  if output="$(prepare_fixed_run_root 2>&1)"; then
    fail_test "TARIT_E2E_RUN_ROOT override was accepted"
  fi
  [[ "$output" == *"TARIT_E2E_RUN_ROOT is unsupported"* ]] ||
    fail_test "TARIT_E2E_RUN_ROOT override was not rejected explicitly"
  [[ "${FIXED_RUN_ROOT:-}" == "/var/tmp/tarit-e2e-shares" ]] ||
    fail_test "run root is not the fixed /var/tmp/tarit-e2e-shares path"
  [[ ! -e "$rejected_root" && ! -L "$rejected_root" ]] ||
    fail_test "override rejection created a run-root artifact"
  unset TARIT_E2E_RUN_ROOT
}

test_run_directory_creation_requires_fixed_root_preparation() {
  local output=""

  RUN_ID="helper-unprepared-$$"
  RUN_ROOT="$WORK_DIR/unprepared-run-root"
  RUN_DIR=""
  RUN_MARKER=""
  RUN_ROOT_READY=0
  if output="$(create_run_directory 2>&1)"; then
    fail_test "run directory was created before fixed-root preparation"
  fi
  [[ "$output" == *"fixed run root is not prepared"* ]] ||
    fail_test "run-directory creation did not fail for an unprepared fixed root"
  [[ -z "$RUN_DIR" && -z "$RUN_MARKER" ]] ||
    fail_test "unprepared run-directory creation recorded artifacts"
}

test_cleanup_stops_postgres_before_other_cleanup() {
  local calls="$WORK_DIR/cleanup-order"
  local expected_calls="stop-local-postgres,stop-caddy,delete-known-vms,stop-vmms,cleanup-database,restore-network,restore-paths,remove-run-dir,"

  : >"$calls"
  record_cleanup_call() {
    printf '%s\n' "$1" >>"$calls"
  }
  stop_local_postgres() { record_cleanup_call stop-local-postgres; }
  stop_caddy() { record_cleanup_call stop-caddy; }
  delete_known_vms_best_effort() { record_cleanup_call delete-known-vms; }
  stop_tracked_vmm_processes() { record_cleanup_call stop-vmms; }
  cleanup_database_rows() { record_cleanup_call cleanup-database; }
  restore_host_networking() { record_cleanup_call restore-network; }
  restore_run_dir_permissions() { record_cleanup_call restore-paths; }
  safe_remove_run_dir() { record_cleanup_call remove-run-dir; }

  NODE_A_PID=""
  NODE_B_PID=""
  RUN_DIR="$WORK_DIR/cleanup-order-run"
  if ! (cleanup); then
    fail_test "cleanup unexpectedly failed while all teardown helpers succeeded"
  fi
  [[ "$(head -n 1 "$calls")" == "stop-local-postgres" ]] ||
    fail_test "cleanup did not stop PostgreSQL before other teardown"
  [[ "$(tr '\n' ',' <"$calls")" == "$expected_calls" ]] ||
    fail_test "cleanup helper ordering changed unexpectedly"
}

test_cleanup_preserves_paths_when_postgres_stop_fails() {
  local calls="$WORK_DIR/cleanup-stop-failure"

  : >"$calls"
  record_cleanup_call() {
    printf '%s\n' "$1" >>"$calls"
  }
  stop_local_postgres() { record_cleanup_call stop-local-postgres; return 1; }
  stop_caddy() { record_cleanup_call stop-caddy; }
  delete_known_vms_best_effort() { record_cleanup_call delete-known-vms; }
  stop_tracked_vmm_processes() { record_cleanup_call stop-vmms; }
  cleanup_database_rows() { record_cleanup_call cleanup-database; }
  restore_host_networking() { record_cleanup_call restore-network; }
  restore_run_dir_permissions() { record_cleanup_call restore-paths; }
  safe_remove_run_dir() { record_cleanup_call remove-run-dir; }

  NODE_A_PID=""
  NODE_B_PID=""
  RUN_DIR="$WORK_DIR/cleanup-stop-failure-run"
  if (cleanup); then
    fail_test "cleanup succeeded after PostgreSQL stop failure"
  fi
  if grep -Fxq restore-paths "$calls"; then
    fail_test "cleanup restored run-path permissions after PostgreSQL stop failure"
  fi
  if grep -Fxq remove-run-dir "$calls"; then
    fail_test "cleanup removed artifacts after PostgreSQL stop failure"
  fi
}

test_restore_refuses_unconfirmed_postgres_stop() {
  local output=""

  RUN_DIR_PG_TRAVERSE_GRANTED=1
  LOCAL_POSTGRES_STOP_CONFIRMED=0
  if output="$(restore_run_dir_permissions 2>&1)"; then
    fail_test "run-path restoration was allowed before PostgreSQL stop confirmation"
  fi
  [[ "$output" == *"refusing to restore run-path permissions before PostgreSQL stop is confirmed"* ]] ||
    fail_test "run-path restoration did not reject an unconfirmed PostgreSQL stop"
}

require_linux_root_sudo_user() {
  if [[ "$(uname -s)" != "Linux" ]]; then
    printf 'SKIP: fixed-root helpers require Linux users\n'
    return 77
  fi
  if [[ "$(id -u)" != "0" || -z "${SUDO_USER:-}" || "$SUDO_USER" == "root" ]]; then
    printf 'SKIP: fixed-root helpers require sudo -E from a non-root user\n'
    return 77
  fi
  if ! command -v runuser >/dev/null 2>&1 || ! id "$SUDO_USER" >/dev/null 2>&1; then
    printf 'SKIP: fixed-root helpers require runuser and SUDO_USER\n'
    return 77
  fi
  export E2E_SHARES_HARNESS_REAL_STAT=1
}

setup_fixed_run_paths() {
  RUN_ID="helper-fixed-$$-$RANDOM"
  RUN_ROOT="$FIXED_RUN_ROOT"
  RUN_DIR=""
  RUN_MARKER=""
  RUN_ROOT_READY=0
  RUN_DIR_READY=0
  RUN_DIR_PG_TRAVERSE_GRANTED=0
  LOCAL_POSTGRES_STOP_CONFIRMED=1
  DATABASE_MODE=""
  unset TARIT_E2E_RUN_ROOT
  prepare_fixed_run_root || return 1
  create_run_directory
}

teardown_fixed_run_paths() {
  local status=$?
  local cleanup_status=0

  trap - EXIT
  if [[ -n "${RUN_DIR:-}" && -d "$RUN_DIR" && ! -L "$RUN_DIR" ]]; then
    LOCAL_POSTGRES_STOP_CONFIRMED=1
    restore_run_dir_permissions >/dev/null 2>&1 || cleanup_status=1
    safe_remove_run_dir >/dev/null 2>&1 || cleanup_status=1
  fi
  [[ "$status" -ne 0 ]] && return "$status"
  return "$cleanup_status"
}

test_sudo_local_postgres_gets_fixed_traverse_only_run_paths() {
  local run_root_mode=""
  local run_root_uid=""
  local run_root_gid=""
  local run_dir_mode=""
  local run_dir_uid=""
  local run_dir_gid=""
  local pg_data_mode=""
  local pg_data_uid=""
  local pg_data_gid=""
  local pg_gid=""
  local original_run_root=""
  local original_run_dir=""

  require_linux_root_sudo_user || return $?

  PG_OS_USER="$SUDO_USER"
  pg_gid="$(id -g "$PG_OS_USER")"
  setup_fixed_run_paths || return 1
  trap 'teardown_fixed_run_paths' EXIT
  original_run_root="$("$SYSTEM_STAT" -c '%u:%g:%a' -- "$RUN_ROOT")"
  original_run_dir="$("$SYSTEM_STAT" -c '%u:%g:%a' -- "$RUN_DIR")"
  printf 'root-only-root-secret\n' >"$RUN_ROOT/root-secret"
  chmod 0600 "$RUN_ROOT/root-secret"
  chown root:root "$RUN_ROOT/root-secret"
  printf 'root-only-secret\n' >"$RUN_DIR/root-secret"
  chmod 0600 "$RUN_DIR/root-secret"
  chown root:root "$RUN_DIR/root-secret"
  PG_DATA_DIR="$RUN_DIR/postgres"
  mkdir "$PG_DATA_DIR"
  chown "$PG_OS_USER:$pg_gid" "$PG_DATA_DIR"
  chmod 0700 "$PG_DATA_DIR"

  [[ "$("$SYSTEM_STAT" -c '%a' -- "$RUN_ROOT")" == "700" &&
    "$("$SYSTEM_STAT" -c '%u' -- "$RUN_ROOT")" == "0" &&
    "$("$SYSTEM_STAT" -c '%a' -- "$RUN_DIR")" == "700" &&
    "$("$SYSTEM_STAT" -c '%u' -- "$RUN_DIR")" == "0" ]] ||
    fail_test "modeled pre-fix run root and run directory were not root-owned and owner-private"
  assert_nonzero "modeled pre-fix PostgreSQL traversal through blocked RUN_ROOT to PG_DIR" \
    runuser -u "$PG_OS_USER" -- test -x "$PG_DATA_DIR"

  export E2E_SHARES_HARNESS_REAL_STAT=1
  DATABASE_MODE=local
  grant_postgres_run_access

  run_root_mode="$("$SYSTEM_STAT" -c '%a' -- "$RUN_ROOT")"
  run_root_uid="$("$SYSTEM_STAT" -c '%u' -- "$RUN_ROOT")"
  run_root_gid="$("$SYSTEM_STAT" -c '%g' -- "$RUN_ROOT")"
  [[ "$run_root_mode" == "710" && "$run_root_uid" == "0" &&
    "$run_root_gid" == "$pg_gid" ]] ||
    fail_test "PostgreSQL run root was not root-owned with group traverse-only access"
  [[ "${PG_PRIMARY_GID:-}" == "$pg_gid" ]] ||
    fail_test "PostgreSQL primary group was not recorded"
  run_dir_mode="$("$SYSTEM_STAT" -c '%a' -- "$RUN_DIR")"
  run_dir_uid="$("$SYSTEM_STAT" -c '%u' -- "$RUN_DIR")"
  run_dir_gid="$("$SYSTEM_STAT" -c '%g' -- "$RUN_DIR")"
  [[ "$run_dir_mode" == "710" && "$run_dir_uid" == "0" &&
    "$run_dir_gid" == "$pg_gid" ]] ||
    fail_test "PostgreSQL run directory was not root-owned with group traverse-only access"

  pg_data_mode="$("$SYSTEM_STAT" -c '%a' -- "$PG_DATA_DIR")"
  pg_data_uid="$("$SYSTEM_STAT" -c '%u' -- "$PG_DATA_DIR")"
  pg_data_gid="$("$SYSTEM_STAT" -c '%g' -- "$PG_DATA_DIR")"
  [[ "$pg_data_mode" == "700" && "$pg_data_uid" == "$(id -u "$PG_OS_USER")" &&
    "$pg_data_gid" == "$pg_gid" ]] ||
    fail_test "PostgreSQL data directory was not PostgreSQL-owned and owner-private"
  runuser -u "$PG_OS_USER" -- test -x "$PG_DATA_DIR" ||
    fail_test "PostgreSQL user could not traverse the exact PostgreSQL data directory"
  assert_nonzero "PostgreSQL user listing root-only RUN_ROOT artifacts" \
    runuser -u "$PG_OS_USER" -- ls "$RUN_ROOT"
  assert_nonzero "PostgreSQL user listing root-only RUN_DIR artifacts" \
    runuser -u "$PG_OS_USER" -- ls "$RUN_DIR"
  assert_nonzero "PostgreSQL user creating a RUN_DIR artifact" \
    runuser -u "$PG_OS_USER" -- touch "$RUN_DIR/postgres-must-not-create"
  assert_nonzero "PostgreSQL user deleting a root-owned RUN_DIR artifact" \
    runuser -u "$PG_OS_USER" -- rm "$RUN_DIR/root-secret"
  assert_nonzero "PostgreSQL user reading root-only RUN_ROOT secret" \
    runuser -u "$PG_OS_USER" -- cat "$RUN_ROOT/root-secret"
  assert_nonzero "PostgreSQL user reading root-only RUN_DIR secret" \
    runuser -u "$PG_OS_USER" -- cat "$RUN_DIR/root-secret"

  restore_run_dir_permissions
  restore_run_dir_permissions
  [[ "$("$SYSTEM_STAT" -c '%u:%g:%a' -- "$RUN_ROOT")" == "$original_run_root" ]] ||
    fail_test "cleanup did not restore the exact run-root metadata"
  [[ "$("$SYSTEM_STAT" -c '%u:%g:%a' -- "$RUN_DIR")" == "$original_run_dir" ]] ||
    fail_test "cleanup did not restore the exact run-directory metadata idempotently"
  unset E2E_SHARES_HARNESS_REAL_STAT
}

test_fixed_root_cleanup_rejects_swapped_run_dir() {
  local target=""

  require_linux_root_sudo_user || return $?
  setup_fixed_run_paths || return 1
  trap 'teardown_fixed_run_paths' EXIT
  target="$(mktemp -d "$RUN_ROOT/cleanup-target.XXXXXX")"
  printf 'must-survive\n' >"$target/sentinel"
  rm -rf -- "$RUN_DIR"
  ln -s "$target" "$RUN_DIR"

  assert_nonzero "swapped per-run directory cleanup" safe_remove_run_dir
  [[ -L "$RUN_DIR" ]] ||
    fail_test "cleanup did not preserve the rejected swapped path for inspection"
  [[ -f "$target/sentinel" ]] ||
    fail_test "cleanup followed the swapped path"

  rm -f -- "$RUN_DIR"
  rm -rf -- "$target"
  RUN_DIR=""
}

test_external_postgres_keeps_fixed_run_paths_root_private() {
  local original_run_root=""
  local original_run_dir=""

  require_linux_root_sudo_user || return $?
  setup_fixed_run_paths || return 1
  trap 'teardown_fixed_run_paths' EXIT
  original_run_root="$("$SYSTEM_STAT" -c '%u:%g:%a' -- "$RUN_ROOT")"
  original_run_dir="$("$SYSTEM_STAT" -c '%u:%g:%a' -- "$RUN_DIR")"
  export FAKE_CHOWN_CALLS="$WORK_DIR/external-chown-args"
  : >"$FAKE_CHOWN_CALLS"
  export FAKE_CHGRP_CALLS="$WORK_DIR/external-chgrp-args"
  : >"$FAKE_CHGRP_CALLS"
  export FAKE_PSQL_ARGS="$WORK_DIR/external-psql-args"
  export FAKE_PSQL_CALLS="$WORK_DIR/external-psql-calls"
  export REQUESTED_DATABASE_URL='postgresql://tarit@database.example.test:5432/tarit?sslmode=require'
  export PSQL_BIN="$BIN_DIR/psql"
  export OWNER_KEY=external-owner
  export HOST_PREFIX=external-host

  configure_database >/dev/null

  [[ ! -s "$FAKE_CHOWN_CALLS" ]] ||
    fail_test "external PostgreSQL mode changed ownership"
  [[ ! -s "$FAKE_CHGRP_CALLS" ]] ||
    fail_test "external PostgreSQL mode changed group ownership"
  [[ "$("$SYSTEM_STAT" -c '%u:%g:%a' -- "$RUN_ROOT")" == "$original_run_root" &&
    "$("$SYSTEM_STAT" -c '%u:%g:%a' -- "$RUN_DIR")" == "$original_run_dir" ]] ||
    fail_test "external PostgreSQL mode changed fixed run-path metadata"
}

test_external_postgres_rejects_nonprivate_fixed_run_dir() {
  local original_run_dir=""
  local output=""

  require_linux_root_sudo_user || return $?
  setup_fixed_run_paths || return 1
  trap 'teardown_fixed_run_paths' EXIT
  original_run_dir="$("$SYSTEM_STAT" -c '%u:%g:%a' -- "$RUN_DIR")"
  chgrp "$(id -g "$SUDO_USER")" "$RUN_DIR"
  chmod 0710 "$RUN_DIR"
  export FAKE_CHOWN_CALLS="$WORK_DIR/external-invalid-chown-args"
  : >"$FAKE_CHOWN_CALLS"
  export FAKE_CHGRP_CALLS="$WORK_DIR/external-invalid-chgrp-args"
  : >"$FAKE_CHGRP_CALLS"
  export FAKE_PSQL_ARGS="$WORK_DIR/external-invalid-psql-args"
  export FAKE_PSQL_CALLS="$WORK_DIR/external-invalid-psql-calls"
  export REQUESTED_DATABASE_URL='postgresql://tarit@database.example.test:5432/tarit?sslmode=require'
  export PSQL_BIN="$BIN_DIR/psql"
  export OWNER_KEY=external-invalid-owner
  export HOST_PREFIX=external-invalid-host

  if output="$(configure_database 2>&1)"; then
    fail_test "external PostgreSQL accepted non-private fixed run paths"
  fi
  [[ "$output" == *"external PostgreSQL requires root:root 0700 fixed run paths"* ]] ||
    fail_test "external PostgreSQL did not reject non-private fixed run paths explicitly"
  [[ ! -s "$FAKE_CHOWN_CALLS" && ! -s "$FAKE_CHGRP_CALLS" ]] ||
    fail_test "external PostgreSQL repaired non-private paths instead of refusing them"
  [[ "$("$SYSTEM_STAT" -c '%u:%g:%a' -- "$RUN_DIR")" != "$original_run_dir" ]] ||
    fail_test "external invalid-path test did not model altered metadata"
  chown root:root "$RUN_DIR"
  chmod 0700 "$RUN_DIR"
}

PASS_COUNT=0
SKIP_COUNT=0
FAIL_COUNT=0

run_test() {
  local name="$1"
  local status=0

  if ( "$name" ); then
    printf 'PASS: %s\n' "$name"
    PASS_COUNT=$((PASS_COUNT + 1))
    return 0
  else
    status=$?
  fi
  if [[ "$status" == "77" ]]; then
    printf 'SKIP: %s\n' "$name"
    SKIP_COUNT=$((SKIP_COUNT + 1))
    return 0
  fi
  printf 'FAIL: %s (exit %d)\n' "$name" "$status" >&2
  FAIL_COUNT=$((FAIL_COUNT + 1))
  return 0
}

run_test test_probe_failure_fails_closed
run_test test_zero_taps_is_distinct_from_probe_failure
run_test test_metric_absence_is_failure
run_test test_cleanup_sql_failure_is_reported_and_verified
run_test test_lock_path_is_immutable
run_test test_secret_free_child_commands
run_test test_fixed_run_root_rejects_environment_override_before_artifacts
run_test test_run_directory_creation_requires_fixed_root_preparation
run_test test_cleanup_stops_postgres_before_other_cleanup
run_test test_cleanup_preserves_paths_when_postgres_stop_fails
run_test test_restore_refuses_unconfirmed_postgres_stop
run_test test_sudo_local_postgres_gets_fixed_traverse_only_run_paths
run_test test_fixed_root_cleanup_rejects_swapped_run_dir
run_test test_external_postgres_keeps_fixed_run_paths_root_private
run_test test_external_postgres_rejects_nonprivate_fixed_run_dir

printf 'SUMMARY: %d passed, %d skipped, %d failed\n' \
  "$PASS_COUNT" "$SKIP_COUNT" "$FAIL_COUNT"
if [[ "$FAIL_COUNT" -ne 0 ]]; then
  printf 'E2E_SHARES_HARNESS_HELPERS_FAIL\n' >&2
  exit 1
fi
if [[ "$SKIP_COUNT" -ne 0 ]]; then
  printf 'E2E_SHARES_HARNESS_HELPERS_PASS_WITH_SKIPS\n'
  exit 0
fi
printf 'E2E_SHARES_HARNESS_HELPERS_PASS\n'
