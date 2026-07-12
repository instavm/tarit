# Task 9 evidence

2026-07-12

## Current focused verification (macOS)

- `bash -n orch/tests/e2e_shares.sh && bash -n orch/tests/test_e2e_shares_harness.sh` — PASS.
- `(cd orch/tests && shellcheck -x --shell=bash e2e_shares.sh test_e2e_shares_harness.sh)` — PASS.
- `bash orch/tests/test_e2e_shares_harness.sh` — exit 0 with `SUMMARY: 11 passed, 4 skipped, 0 failed` and `E2E_SHARES_HARNESS_HELPERS_PASS_WITH_SKIPS`.
  The skipped Linux/root cases are
  `test_sudo_local_postgres_gets_fixed_traverse_only_run_paths`,
  `test_fixed_root_cleanup_rejects_swapped_run_dir`,
  `test_external_postgres_keeps_fixed_run_paths_root_private`, and
  `test_external_postgres_rejects_nonprivate_fixed_run_dir`. They did not pass
  or execute on macOS. The eleven passing cases include fixed-root override
  refusal, pre-artifact validation, and cleanup stop ordering/failure
  preservation.

## Earlier workspace checks (not rerun in this focused follow-up)

- `(cd orch && cargo fmt --all -- --check)` — PASS.
- `(cd orch && cargo clippy --workspace --all-targets -- -D warnings)` — PASS.
- `(cd orch && cargo test --workspace)` — PASS: 177 unit tests passed; all workspace doctest suites passed.
- `git diff --check` — PASS.

## First Ubuntu c8i RED run

- Before the run-directory ownership fix, `sudo -E bash orch/tests/e2e_shares.sh`
  failed during isolated `initdb` with
  `could not access directory .../shares.<id>/postgres.<id>: Permission denied`.
  The preserved tree showed `RUN_DIR` as `0700 root:root` and `PG_DIR` as
  `ubuntu:root`; the selected `PG_OS_USER` was `ubuntu`, so it could not
  traverse the parent directory.

## Second Ubuntu c8i RED run (`namei` evidence)

- The first fix made `RUN_DIR` and `PG_DIR` `0700 ubuntu`, but isolated
  `initdb` still failed with permission denied. `namei -l` on the exact data
  directory path proved that the blocked ancestor was
  `/home/ubuntu/tarit/orch/e`: it remained `0700 root`, so `ubuntu` could not
  traverse the path even though it owned both descendant directories.
- These are the two recorded c8i RED runs: first the blocked per-run directory,
  then the blocked `orch/e` ancestor after the first attempted fix.

## Reviewer rejection and replacement design

- Review rejected the configurable-root remediation because checks were followed
  by name-based `chgrp`, `chmod`, `chown`, and recursive removal on mutable
  paths; custom-root validation occurred after artifacts; external mode still
  depended on the traversal machinery; and cleanup restored permissions even
  when PostgreSQL had not stopped.
- The replacement refuses every `TARIT_E2E_RUN_ROOT` setting and uses only
  `/var/tmp/tarit-e2e-shares`, a direct child of root-owned sticky `/var/tmp`.
  Under the existing fixed global flock it creates or verifies that root as
  canonical, non-symlink, non-mountpoint, root:root `0700`, with no symlink
  ancestors. Its root ownership and the sticky parent prevent the PostgreSQL
  user from swapping it.
- External PostgreSQL keeps both the fixed root and its marked direct
  `shares.*` child root:root `0700` and refuses altered metadata. Local
  PostgreSQL changes only those two directories to root:`PG_PRIMARY_GID`
  `0710` before `initdb`; the data directory is PostgreSQL-owned `0700`.
  Runtime checks and the Linux helper coverage prove exact traversal while
  list/create/delete/read of root artifacts are denied.
- Cleanup now stops and positively awaits tracked PostgreSQL before restoring
  exact fixed-path metadata. A failed or unconfirmed stop preserves modes and
  artifacts. Removal revalidates the root-owned, non-symlink marked child and
  does not traverse symlinks; the empty fixed root remains root:root `0700`.

## Linux root/runuser verification still required

- The local regression test is intentionally skipped on macOS. Run
  `sudo -E bash orch/tests/test_e2e_shares_harness.sh` on Linux from a non-root
  sudo caller to execute the four fixed-root cases. They model the blocked
  root-owned ancestor, prove pre-fix PG-directory traversal is denied, then
  prove exact `PG_DIR` traversal while list/create/delete/read of root
  artifacts remain denied. They also verify root metadata restoration,
  external-mode immutability, and symlink/swap cleanup rejection.
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
