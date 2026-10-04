#!/usr/bin/env bash
# test/e2e.sh — single-node end-to-end suite for the whole platform (both
# binaries: taritd driving vmm). Runs the self-contained feature tests back to
# back on one Linux + KVM host and aggregates pass/fail.
#
# Cluster / HA / Postgres tests live in test/cluster.sh.
#
#   sudo test/e2e.sh                 # run the whole suite
#   sudo test/e2e.sh smoke cli       # run a subset by short name
#
# Env: TARIT_KERNEL, TARIT_ROOTFS (default /tmp/vmlinux.microvm, /tmp/vsock-rootfs.ext4;
# build them with `sudo make guest`), PER_TEST_TIMEOUT (default 360s).
set -uo pipefail
HERE="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
. "$HERE/lib/preflight.sh"

require_kvm
require_root
require_tools
detect_virt
build_binaries
require_fixtures

# short name -> orch/tests script (the feature building blocks)
declare -A SUITE=(
  [smoke]=e2e_smoke_mono.sh
  [cli]=e2e_cli.sh
  [ssh-pty]=e2e_ssh_pty.sh
  [image]=e2e_image_pipeline.sh
  [warmpool]=e2e_warmpool_restore.sh
  [multitenant]=e2e_multitenant.sh
  [net]=e2e_net_scale.sh
  [lifecycle]=e2e_lifecycle.sh
  [suspend]=e2e_suspend_resume.sh
  [snapshot-disk]=e2e_snapshot_disk.sh
  [cpu-refill]=e2e_cpu_refill.sh
)
ORDER=(smoke cli ssh-pty image warmpool multitenant net lifecycle suspend snapshot-disk cpu-refill)
[ "$#" -gt 0 ] && ORDER=("$@")

declare -A RESULT; PASS=0; FAIL=0
for name in "${ORDER[@]}"; do
  script="${SUITE[$name]:-}"
  [ -n "$script" ] || { warn "unknown suite '$name' (known: ${!SUITE[*]})"; continue; }
  path="$REPO_ROOT/orch/tests/$script"
  [ -f "$path" ] || { RESULT[$name]=MISSING; FAIL=$((FAIL+1)); continue; }
  echo; info "── $name ($script) ──"
  # Each suite owns and tears down the process group it launches. Never sweep
  # unrelated taritd or VMM processes from a shared KVM host.
  if timeout "${PER_TEST_TIMEOUT:-360}" bash "$path"; then RESULT[$name]=PASS; PASS=$((PASS+1)); else RESULT[$name]=FAIL; FAIL=$((FAIL+1)); fi
done

echo; echo "================ test/e2e summary ================"
for name in "${ORDER[@]}"; do printf "  %-14s %s\n" "$name" "${RESULT[$name]:-SKIP}"; done
echo "  ------------------------------------------"
echo "  PASS=$PASS FAIL=$FAIL"
if [ "$FAIL" -eq 0 ]; then
  echo "  RESULT: E2E_PASS"
  exit 0
fi
echo "  RESULT: E2E_FAIL"
exit 1
