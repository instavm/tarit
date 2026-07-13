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

if output=$(LC_ALL=C.UTF-8 EGRESS_RECOVERY_RUN_ROOT="/run/taritd/$(printf 'é%.0s' {1..15})" \
  "$HARNESS" --check-socket-path 2>&1); then
  echo "FAIL: multibyte overlong egress recovery VMM socket path was accepted" >&2
  exit 1
fi

if ! grep -F "VMM socket path exceeds Linux sun_path limit" <<<"$output" >/dev/null; then
  echo "FAIL: multibyte overlong egress recovery VMM socket path was not rejected" >&2
  exit 1
fi

"$HARNESS" --check-run-root /run/taritd/egress-recovery-override

for run_root in /run /etc /run/taritd/../outside /run/taritd//double-slash; do
  if output=$("$HARNESS" --check-run-root "$run_root" 2>&1); then
    echo "FAIL: unsafe egress recovery run root was accepted: $run_root" >&2
    exit 1
  fi
  if ! grep -F "run root must be a normalized child of /run/taritd" <<<"$output" >/dev/null; then
    echo "FAIL: unsafe run root was not rejected as untrusted: $run_root" >&2
    exit 1
  fi
done

if grep -E 'mkdir -p .*RUN_ROOT|chown .*RUN_ROOT|chmod .*RUN_ROOT' "$HARNESS" >/dev/null; then
  echo "FAIL: run root is mutated before its trust can be established" >&2
  exit 1
fi
