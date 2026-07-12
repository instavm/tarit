# Task 9 evidence

2026-07-12

## Current focused verification (macOS)

- `bash -n orch/tests/e2e_shares.sh && bash -n orch/tests/test_e2e_shares_harness.sh` — PASS.
- `(cd orch/tests && shellcheck -x --shell=bash e2e_shares.sh test_e2e_shares_harness.sh)` — PASS.
- `bash orch/tests/test_e2e_shares_harness.sh` — exit 0 with `SUMMARY: 7 passed, 1 skipped, 0 failed` and `E2E_SHARES_HARNESS_HELPERS_PASS_WITH_SKIPS`.
  The skipped case is `test_sudo_local_postgres_uses_a_private_pg_owned_run_dir`:
  `SKIP: local PostgreSQL ownership helper requires Linux users`. It did not pass
  or execute on macOS. The seven passing cases include the external-PostgreSQL
  no-`chown` check.

## Earlier workspace checks (not rerun in this focused follow-up)

- `(cd orch && cargo fmt --all -- --check)` — PASS.
- `(cd orch && cargo clippy --workspace --all-targets -- -D warnings)` — PASS.
- `(cd orch && cargo test --workspace)` — PASS: 177 unit tests passed; all workspace doctest suites passed.
- `git diff --check` — PASS.

## Prior Ubuntu c8i failure

- Before the run-directory ownership fix, `sudo -E bash orch/tests/e2e_shares.sh`
  failed during isolated `initdb` with
  `could not access directory .../shares.<id>/postgres.<id>: Permission denied`.
  The preserved tree showed `RUN_DIR` as `0700 root:root` and `PG_DIR` as
  `ubuntu:root`; the selected `PG_OS_USER` was `ubuntu`, so it could not
  traverse the parent directory.

## Linux root/runuser verification still required

- The local regression test is intentionally skipped on macOS. Run
  `sudo -E bash orch/tests/test_e2e_shares_harness.sh` on Linux from a non-root
  sudo caller to execute it. That case models the inaccessible root-owned
  `RUN_DIR`/PG data directory state, verifies the helper changes it, verifies
  root-owned `0600` secret access stays denied to both group and other users,
  and checks PostgreSQL can traverse both directories.
- The full KVM/Caddy gate was not run locally. The controller must run
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
