# Task 9 evidence

2026-07-12

- `bash -n orch/tests/e2e_shares.sh && bash -n orch/tests/test_e2e_shares_harness.sh` — PASS.
- `bash orch/tests/test_e2e_shares_harness.sh` — PASS: `E2E_SHARES_HARNESS_HELPERS_PASS`. The deterministic helper cases cover an `ip` probe failure, zero-tap distinction, missing gauge, SQL cleanup failure plus verification query, immutable lock path, secret-free `psql`/`curl` child arguments, and the sudo-caller/local-PostgreSQL ownership regression (skipped on macOS).
- `(cd orch/tests && shellcheck -x --shell=bash e2e_shares.sh test_e2e_shares_harness.sh)` — PASS.
- `(cd orch && cargo fmt --all -- --check)` — PASS.
- `(cd orch && cargo clippy --workspace --all-targets -- -D warnings)` — PASS.
- `(cd orch && cargo test --workspace)` — PASS: 177 unit tests passed; all workspace doctest suites passed.
- `git diff --check` — PASS.

## Ubuntu c8i RED

- `sudo -E bash orch/tests/e2e_shares.sh` failed during isolated `initdb` with
  `could not access directory .../shares.<id>/postgres.<id>: Permission denied`.
  The preserved tree showed `RUN_DIR` as `0700 root:root` and `PG_DIR` as
  `ubuntu:root`; the selected `PG_OS_USER` was `ubuntu`, so it could not
  traverse the parent directory.

## Remote prerequisite

The real KVM/Caddy gate was not run locally. The controller must run
`sudo -E bash orch/tests/e2e_shares.sh` on an idle Linux host with root access,
`/dev/kvm`, nftables privileges, Caddy, SQLite CLI, and either reachable
PostgreSQL or local PostgreSQL tools.

## Workspace-test follow-up

- The default parallel `cargo test --workspace` passed once, but two later
  reruns separately exposed existing timing-sensitive failures in unmodified
  `share_gateway::tests::websocket_idle_timeout_closes_an_inactive_bridge` and
  `autoscale::tests::shutdown_terminates_and_reaps_a_running_provider_child`.
  Each passed in isolation.
- `cargo test --workspace -- --test-threads=1` — PASS: 177 unit tests passed;
  all workspace doctest suites passed.
