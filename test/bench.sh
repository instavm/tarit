#!/usr/bin/env bash
# test/bench.sh — ComputeSDK-style Time-To-Interactive benchmark.
#
# TTI is measured per iteration from create() to the first `node -v` that returns
# exit 0 (the ComputeSDK metric). Runs the warm-pool harness in sequential,
# staggered, and burst modes, and writes a results table.
#
#   sudo test/bench.sh                 # warm-pool TTI, defaults from BENCHMARK-RESULTS.md
#   sudo MODE=cold N=100 test/bench.sh   # warm handout, pool refilled by cold boot
#   sudo MODE=direct N=100 test/bench.sh # actual cold create-to-exec path
#
# Bare metal gives the headline numbers; a nested-KVM guest works too but pays a
# ~10x KVM-exit tax (the runner warns about this).
set -uo pipefail
HERE="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
. "$HERE/lib/preflight.sh"

require_kvm
require_root
require_tools
detect_virt
info "building release binaries (vmm, taritd, tarit-bench)…"
[ -x "$REPO_ROOT/vmm/target/release/vmm" ] || ( cd "$REPO_ROOT/vmm" && cargo build --release -p vmm --features boot ) || die "vmm build failed"
{ [ -x "$REPO_ROOT/orch/target/release/taritd" ] && [ -x "$REPO_ROOT/orch/target/release/tarit-bench" ]; } || ( cd "$REPO_ROOT/orch" && cargo build --release -p taritd -p tarit-bench ) || die "orch build failed"
export TARIT_VMM_BIN="$REPO_ROOT/vmm/target/release/vmm"

# The bench command is `node -v`, so the rootfs must contain node. Build a node
# rootfs with the agent baked in if one is not already present.
: "${TARIT_KERNEL:=/tmp/vmlinux.microvm}"
: "${TARIT_ROOTFS:=/tmp/bench-node-rootfs.ext4}"
[ -f "$TARIT_KERNEL" ] || die "guest kernel not found at $TARIT_KERNEL. Build with: sudo make guest"
if [ ! -f "$TARIT_ROOTFS" ]; then
  info "building a node rootfs at $TARIT_ROOTFS (node:20-slim + agent)…"
  agent="$REPO_ROOT/vmm/guest/agent/vmm-agent"
  [ -x "$agent" ] || make -C "$REPO_ROOT/vmm/guest/agent" >/dev/null
  "$TARIT_VMM_BIN" pull docker://node:20-slim --output "$TARIT_ROOTFS" --agent "$agent" || die "node rootfs pull failed"
  e2fsck -fy "$TARIT_ROOTFS" >/dev/null 2>&1 || true
fi
export TARIT_KERNEL TARIT_ROOTFS

info "running ComputeSDK-style warm-pool TTI benchmark…"
TARIT_VMM_BIN="$TARIT_VMM_BIN" TARIT_KERNEL="$TARIT_KERNEL" TARIT_ROOTFS="$TARIT_ROOTFS" \
  bash "$REPO_ROOT/orch/scripts/bench-warmpool.sh"
