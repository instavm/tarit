#!/usr/bin/env bash
# Host-safe regression test for the egress recovery VMM socket path budget.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
HARNESS="$SCRIPT_DIR/e2e_egress_recovery.sh"

"$HARNESS" --check-socket-path

if output=$(EGRESS_RECOVERY_RUN_ROOT="/root/.taritd/egress-recovery-runs/$(printf 'x%.0s' {1..80})" \
  "$HARNESS" --check-socket-path 2>&1); then
  echo "FAIL: overlong egress recovery VMM socket path was accepted" >&2
  exit 1
fi

if ! grep -F "VMM socket path exceeds Linux sun_path limit" <<<"$output" >/dev/null; then
  echo "FAIL: overlong egress recovery VMM socket path was not rejected" >&2
  exit 1
fi
