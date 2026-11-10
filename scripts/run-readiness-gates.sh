#!/usr/bin/env bash
# Run the single-node readiness gates on this host and keep the evidence:
# the manual KVM validation gates, the whole test/e2e.sh suite,
# and a bounded mixed-image lifecycle soak. Needs root (TAP networking, the
# jailer, OCI unpack) and release builds of both binaries:
#
#   cargo build --release -p vmm --features boot          # in vmm/
#   cargo build --release -p taritd -p tarit-bench        # in orch/
#   sudo TMPDIR=/var/tmp scripts/run-readiness-gates.sh [OUT_DIR]
#
# Each gate's log and a summary.tsv (gate, result, seconds) land in OUT_DIR,
# owned by the user who ran sudo. Gates run one after another and a failing
# gate does not stop the rest; the exit status is 1 if any failed.
#
# Env: KERNEL, ROOTFS (default guest-assets/), GATES (space-separated subset
# of: e2e kvm-suite cold-create warm-create perf soak), SOAK_CASES (newline-
# separated name|kernel|rootfs; default: the guest rootfs and every image in
# IMAGES_DIR), IMAGES_DIR, VMM_TEST_OVERLAY_DIR (a reflink filesystem enables
# the live-snapshot tests).
set -uo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$REPO/target/readiness/$(date -u +%Y%m%dT%H%M%SZ)}
KERNEL=${KERNEL:-$REPO/guest-assets/vmlinux}
ROOTFS=${ROOTFS:-$REPO/guest-assets/rootfs.ext4}
GATES=${GATES:-e2e kvm-suite cold-create warm-create perf soak}
VMM_BIN=$REPO/vmm/target/release/vmm
TARITD_BIN=$REPO/orch/target/release/taritd
OWNER=${SUDO_UID:-$(id -u)}:${SUDO_GID:-$(id -g)}
INVOKER_HOME=$(getent passwd "${SUDO_USER:-$(id -un)}" | cut -d: -f6)

die() { echo "run-readiness-gates: $*" >&2; exit 2; }
[ "$(id -u)" = 0 ] || die "run as root (sudo)"
[ -c /dev/kvm ] || die "/dev/kvm is missing"
[ -x "$VMM_BIN" ] || die "build the VMM first: $VMM_BIN"
[ -x "$TARITD_BIN" ] || die "build taritd first: $TARITD_BIN"
[ -r "$KERNEL" ] && [ -r "$ROOTFS" ] || die "guest kernel/rootfs not found ($KERNEL, $ROOTFS)"

mkdir -p "$OUT"
SUMMARY=$OUT/summary.tsv
printf 'gate\tresult\tseconds\n' > "$SUMMARY"
{
  echo "host: $(hostname) $(uname -r) $(nproc) CPUs $(awk '/MemTotal/ {print int($2/1024)" MiB"}' /proc/meminfo)"
  echo "commit: $(git -C "$REPO" rev-parse HEAD 2>/dev/null)$(git -C "$REPO" diff --quiet 2>/dev/null || echo ' (with local changes)')"
  echo "started: $(date -u +%FT%TZ)  load: $(cut -d' ' -f1-3 /proc/loadavg)"
} > "$OUT/host.txt"
failed=0

# Root runs cargo with the invoking user's toolchain, building into its own
# target directory so it leaves no root-owned files in the user's.
export PATH=$INVOKER_HOME/.cargo/bin:$PATH
export RUSTUP_HOME=${RUSTUP_HOME:-$INVOKER_HOME/.rustup}
export CARGO_HOME=${CARGO_HOME:-$INVOKER_HOME/.cargo}
export TARIT_KERNEL=$KERNEL TARIT_ROOTFS=$ROOTFS TARIT_VMM_BIN=$VMM_BIN TARITD_BIN=$TARITD_BIN

gate() {
  local name=$1; shift
  echo "== $name"
  local start=$SECONDS
  if "$@" > "$OUT/$name.log" 2>&1; then
    printf '%s\tPASS\t%s\n' "$name" $((SECONDS - start)) >> "$SUMMARY"
    echo "   PASS ($((SECONDS - start)) s)"
  else
    printf '%s\tFAIL\t%s\n' "$name" $((SECONDS - start)) >> "$SUMMARY"
    echo "   FAIL ($((SECONDS - start)) s), see $OUT/$name.log"
    failed=1
  fi
}

wanted() { case " $GATES " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

wanted e2e && gate e2e bash "$REPO/test/e2e.sh"

wanted kvm-suite && gate kvm-suite env \
  VMM_TEST_KERNEL="$KERNEL" VMM_TEST_ROOTFS="$ROOTFS" \
  CARGO_TARGET_DIR="$OUT/../cargo-target" \
  bash -c "cd '$REPO/vmm' && cargo test --release --locked --no-fail-fast --workspace \
    --features vmm-core/kvm --features vmm-core/boot \
    --features vmm-memory-backend/kvm --features vmm-integration/kvm \
    -- --include-ignored --test-threads=1"

wanted cold-create && gate cold-create env \
  MODE=direct BENCHMARK_MODE=sequential TARGET=1 N=100 MEM=256 CMD=true \
  MAX_P99_MS=5000 MIN_SUCCESS_PERCENT=100 RESULTS_DIR="$OUT/cold-create" \
  bash "$REPO/orch/scripts/bench-warmpool.sh"

wanted warm-create && gate warm-create env \
  MODE=restore BENCHMARK_MODE=sequential TARGET=32 N=100 MEM=256 CMD=true \
  MAX_P99_MS=1000 MIN_SUCCESS_PERCENT=100 RESULTS_DIR="$OUT/warm-create" \
  bash "$REPO/orch/scripts/bench-warmpool.sh"

wanted perf && gate perf env \
  VMM="$VMM_BIN" KERNEL="$KERNEL" ROOTFS="$ROOTFS" VMM_PERF_STRICT=1 \
  bash "$REPO/vmm/ci/perf-gates.sh"

if wanted soak; then
  if [ -z "${SOAK_CASES:-}" ]; then
    SOAK_CASES="guest|$KERNEL|$ROOTFS"
    if [ -n "${IMAGES_DIR:-}" ]; then
      for image in "$IMAGES_DIR"/*.ext4; do
        [ -f "$image" ] && SOAK_CASES+=$'\n'"$(basename "$image" .ext4)|$KERNEL|$image"
      done
    fi
  fi
  gate soak env TARIT_SOAK_CASES="$SOAK_CASES" \
    bash "$REPO/orch/tests/e2e_mixed_oci_soak.sh"
fi

echo "finished: $(date -u +%FT%TZ)  load: $(cut -d' ' -f1-3 /proc/loadavg)" >> "$OUT/host.txt"
chown -R "$OWNER" "$OUT"
echo
column -t "$SUMMARY"
echo "evidence: $OUT"
exit $failed
