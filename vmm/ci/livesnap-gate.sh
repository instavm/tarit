#!/usr/bin/env bash
# ci/livesnap-gate.sh — live-snapshot / restore / suspend memory-consistency CI
# gate. Runs deterministic, SHA256-verified consistency checks for full-snapshot
# restore, diff-chain (incremental) restore, and suspend/resume. Exits non-zero
# if any regresses. Run as root on a Linux+KVM host (c8i):
#
#   sudo bash ci/livesnap-gate.sh
#
# This is the fast, reliable gate. The exhaustive membench A/B/C/D soak lives in
# membench/ and is run separately (it needs the hardened, fresh-VM-per-test
# harness; the in-repo harness is not yet hardened for an unattended gate).
set -uo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
FAIL=0
run() {
  echo ""; echo "########## $1 ##########"
  if bash "$DIR/$2"; then echo ">>> $1: OK"; else echo ">>> $1: FAILED"; FAIL=1; fi
}
run "full-snapshot restore consistency" full-restore-check.sh
run "diff-chain restore consistency"    diff-restore-check.sh
run "suspend/resume RAM consistency"    suspend-validate.sh
echo ""
if [ "$FAIL" = 0 ]; then echo "LIVESNAP_GATE_PASS"; exit 0; else echo "LIVESNAP_GATE_FAIL"; exit 1; fi
