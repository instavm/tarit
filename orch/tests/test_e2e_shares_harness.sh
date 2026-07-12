#!/usr/bin/env bash
# Deterministic helper coverage for the share E2E harness; does not require KVM.
set -Eeuo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SYSTEM_STAT="$(command -v stat)"
SYSTEM_CHOWN="$(command -v chown)"
export SYSTEM_STAT SYSTEM_CHOWN
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

test_sudo_local_postgres_uses_a_private_pg_owned_run_dir() {
  local workspace_mode=""
  local run_dir_mode=""
  local run_dir_uid=""
  local pg_data_mode=""
  local secret_mode=""
  local secret_uid=""

  if [[ "$(uname -s)" != "Linux" ]]; then
    printf 'SKIP: local PostgreSQL ownership helper requires Linux users\n'
    return 77
  fi
  if [[ "$(id -u)" != "0" || -z "${SUDO_USER:-}" || "$SUDO_USER" == "root" ]]; then
    printf 'SKIP: local PostgreSQL ownership helper requires sudo -E from a non-root user\n'
    return 77
  fi
  if ! command -v runuser >/dev/null 2>&1 || ! id "$SUDO_USER" >/dev/null 2>&1; then
    printf 'SKIP: local PostgreSQL ownership helper requires runuser and SUDO_USER\n'
    return 77
  fi

  PG_OS_USER="$SUDO_USER"
  RUN_DIR="$(mktemp -d "$WORK_DIR/local-pg-run.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  chown root:root "$RUN_DIR"
  workspace_mode="$("$SYSTEM_STAT" -c '%a' -- "$WORK_DIR")"
  [[ "$workspace_mode" == "711" ]] ||
    fail_test "workspace parent above RUN_DIR was not traversable"
  printf 'root-only-secret\n' >"$RUN_DIR/root-secret"
  chmod 0600 "$RUN_DIR/root-secret"
  chown root:root "$RUN_DIR/root-secret"
  PG_DATA_DIR="$RUN_DIR/postgres"
  mkdir "$PG_DATA_DIR"
  chown "$PG_OS_USER" "$PG_DATA_DIR"
  chmod 0700 "$PG_DATA_DIR"

  [[ "$("$SYSTEM_STAT" -c '%a' -- "$RUN_DIR")" == "700" &&
    "$("$SYSTEM_STAT" -c '%u' -- "$RUN_DIR")" == "0" ]] ||
    fail_test "modeled pre-fix run directory was not root-owned and owner-private"
  assert_nonzero "modeled pre-fix PostgreSQL traversal of root-owned RUN_DIR" \
    runuser -u "$PG_OS_USER" -- test -x "$RUN_DIR"
  assert_nonzero "modeled pre-fix PostgreSQL traversal of PG_DIR" \
    runuser -u "$PG_OS_USER" -- test -x "$PG_DATA_DIR"

  export E2E_SHARES_HARNESS_REAL_STAT=1
  grant_postgres_run_access

  run_dir_mode="$("$SYSTEM_STAT" -c '%a' -- "$RUN_DIR")"
  run_dir_uid="$("$SYSTEM_STAT" -c '%u' -- "$RUN_DIR")"
  [[ "$run_dir_mode" == "700" ]] ||
    fail_test "PostgreSQL run directory mode was not owner-private"
  [[ "$run_dir_uid" == "$(id -u "$PG_OS_USER")" ]] ||
    fail_test "PostgreSQL user did not own the private run directory"
  secret_mode="$("$SYSTEM_STAT" -c '%a' -- "$RUN_DIR/root-secret")"
  secret_uid="$("$SYSTEM_STAT" -c '%u' -- "$RUN_DIR/root-secret")"
  [[ "$secret_mode" == "600" && "$secret_uid" == "0" ]] ||
    fail_test "root-owned secret did not deny group and other reads"

  pg_data_mode="$("$SYSTEM_STAT" -c '%a' -- "$PG_DATA_DIR")"
  [[ "$pg_data_mode" == "700" ]] ||
    fail_test "PostgreSQL data directory mode was not owner-private"
  runuser -u "$PG_OS_USER" -- test -x "$RUN_DIR" ||
    fail_test "PostgreSQL user could not traverse its run directory"
  runuser -u "$PG_OS_USER" -- test -x "$PG_DATA_DIR" ||
    fail_test "PostgreSQL user could not traverse its data directory"
  assert_nonzero "other user reading root-only secret" \
    runuser -u "$PG_OS_USER" -- cat "$RUN_DIR/root-secret"
  assert_nonzero "root-group user reading root-only secret" \
    runuser -u "$PG_OS_USER" -g root -- cat "$RUN_DIR/root-secret"

  restore_run_dir_permissions
  restore_run_dir_permissions
  [[ "$("$SYSTEM_STAT" -c '%a' -- "$RUN_DIR")" == "700" ]] ||
    fail_test "cleanup changed the private run directory mode"
  unset E2E_SHARES_HARNESS_REAL_STAT
}

test_external_postgres_does_not_chown_run_dir() {
  RUN_DIR="$(mktemp -d "$WORK_DIR/external-pg-run.XXXXXX")"
  export FAKE_CHOWN_CALLS="$WORK_DIR/external-chown-args"
  : >"$FAKE_CHOWN_CALLS"
  export FAKE_PSQL_ARGS="$WORK_DIR/external-psql-args"
  export FAKE_PSQL_CALLS="$WORK_DIR/external-psql-calls"
  export REQUESTED_DATABASE_URL='postgresql://tarit@database.example.test:5432/tarit?sslmode=require'
  export PSQL_BIN="$BIN_DIR/psql"
  export OWNER_KEY=external-owner
  export HOST_PREFIX=external-host

  configure_database >/dev/null

  [[ ! -s "$FAKE_CHOWN_CALLS" ]] ||
    fail_test "external PostgreSQL mode changed ownership"
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
run_test test_external_postgres_does_not_chown_run_dir
run_test test_sudo_local_postgres_uses_a_private_pg_owned_run_dir

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
