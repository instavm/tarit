#!/usr/bin/env bash
# Real-KVM qualification for application state repair across clone and resume.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"

for required in gcc mktemp; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done

DIR=$(mktemp -d "$SOCKET_ROOT/tarit-clone-workload-build.XXXXXX")
cleanup() {
  find "$DIR" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

gcc -std=c11 -O2 -Wall -Wextra -Werror -pedantic -static \
  "$ROOT/orch/tests/clone_repair_workload.c" -o "$DIR/clone-repair-workload"

TARIT_TEST_CLONE_WORKLOAD_BIN="$DIR/clone-repair-workload" \
  bash "$ROOT/orch/tests/e2e_live_fork_hibernate.sh"
