#!/usr/bin/env bash
# Soak test: runs boot/snapshot/restore cycles continuously.
#
# Two modes:
#   --mode=fast  Runs the single boot+snapshot+restore E2E (~50ms-1s on c8i).
#                Enables tens of thousands of cycles in ~3h. Default for
#                leak hunting and steady-state regression.
#   --mode=full  Runs the full workspace test suite (~minutes per cycle).
#                Default for correctness coverage on long runs.
#
# Usage:
#   ./ci/soak.sh                        # full, 3h, 5s delay
#   ./ci/soak.sh --mode=fast 1800       # fast, 30min, default delay
#   ./ci/soak.sh --mode=fast 3600 0     # fast, 1h, no delay
#
# On first failure: dump dmesg, the failing test's stderr, and the cycle
# index; stop. Set CONTINUE_ON_FAIL=1 to keep going (historical behavior).

set -euo pipefail

MODE="full"
case "${1:-}" in
  --mode=fast) MODE="fast"; shift ;;
  --mode=full) MODE="full"; shift ;;
esac

DURATION=${1:-10800}
DELAY=${2:-5}
START=$(date +%s)
CYCLE=0
RESULTS=docs/soak-results.md
CONTINUE_ON_FAIL=${CONTINUE_ON_FAIL:-0}

mkdir -p docs

if [ "$MODE" = "fast" ]; then
    CMD=(cargo test --release -p vmm-integration --features kvm
         --test lifecycle_e2e e2e_boot_snapshot_restore --
         --include-ignored --nocapture)
else
    CMD=(cargo test --release --workspace
         --features vmm-core/kvm --features vmm-core/boot
         --features vmm-memory-backend/kvm --features vmm-integration/kvm
         -- --include-ignored)
fi

cat > "$RESULTS" << EOF
# VMM Soak Test Results

Started: $(date -u '+%Y-%m-%d %H:%M:%S UTC')
Mode: $MODE
Duration target: ${DURATION}s
Cycle delay: ${DELAY}s
Command: ${CMD[*]}

## Summary

| Metric | Value |
|---|---|
| Cycles | 0 |
| Total tests | 0 |
| Failures | 0 |
| First failure | — |
| Max RSS | 0 |
| Max FDs | 0 |

## Cycle Results

| Cycle | Time | Pass | Fail | RSS_KB | FDs | Slab_KB |
|---|---|---|---|---|---|---|
EOF

TOTAL_PASS=0
TOTAL_FAIL=0
MAX_RSS=0
MAX_FDS=0
FIRST_FAILURE=""

# Self PID — for fd-count tracking we measure the process running the
# tests (this script + cargo + cargo_test_runner). Tracking our own
# /proc/self/fd is the cheapest proxy for "are we leaking fds?"
SELF_PID=$$

while true; do
    ELAPSED=$(($(date +%s) - START))
    if [ "$ELAPSED" -ge "$DURATION" ]; then
        echo "Duration reached — stopping"
        break
    fi

    CYCLE=$((CYCLE + 1))
    TIME=$(date -u '+%H:%M:%S')
    LOG="docs/soak-cycle-${CYCLE}.log"
    PASS=0
    FAIL=0

    if "${CMD[@]}" > "$LOG" 2>&1; then
        if grep -q "test result: ok" "$LOG"; then
            PASS=1
            rm -f "$LOG"
        else
            FAIL=1
        fi
    else
        FAIL=1
    fi

    TOTAL_PASS=$((TOTAL_PASS + PASS))
    TOTAL_FAIL=$((TOTAL_FAIL + FAIL))

    # Resource snapshots — RSS of any vmm/cargo process, FD count of
    # this script's fd table (proxy for leaks since cargo respawns
    # the test binary each cycle), and slab cache size (kernel-side
    # leaks like UFFD pages or KVM dirty bitmaps show up here).
    RSS=$(ps -eo rss,comm 2>/dev/null | awk '/cargo|vmm|comprehensive/ {sum += $1} END {print sum+0}')
    FDS=$(ls /proc/$SELF_PID/fd 2>/dev/null | wc -l)
    SLAB_KB=$(awk '/^Slab:/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)

    [ "$RSS" -gt "$MAX_RSS" ] && MAX_RSS=$RSS
    [ "$FDS" -gt "$MAX_FDS" ] && MAX_FDS=$FDS

    echo "| $CYCLE | $TIME | $PASS | $FAIL | $RSS | $FDS | $SLAB_KB |" >> "$RESULTS"

    sed -i.bak "s/| Cycles | .*/| Cycles | $CYCLE |/" "$RESULTS"
    sed -i.bak "s/| Total tests | .*/| Total tests | $TOTAL_PASS |/" "$RESULTS"
    sed -i.bak "s/| Failures | .*/| Failures | $TOTAL_FAIL |/" "$RESULTS"
    sed -i.bak "s/| Max RSS | .*/| Max RSS | $MAX_RSS |/" "$RESULTS"
    sed -i.bak "s/| Max FDs | .*/| Max FDs | $MAX_FDS |/" "$RESULTS"
    rm -f "${RESULTS}.bak"

    if [ "$FAIL" -gt 0 ]; then
        if [ -z "$FIRST_FAILURE" ]; then
            FIRST_FAILURE="cycle $CYCLE @ $TIME"
            sed -i.bak "s|| First failure | .*|| First failure | $FIRST_FAILURE ||" "$RESULTS"
            rm -f "${RESULTS}.bak"
        fi
        echo "" >> "$RESULTS"
        echo "### Cycle $CYCLE failure" >> "$RESULTS"
        echo '```' >> "$RESULTS"
        tail -50 "$LOG" >> "$RESULTS"
        echo '```' >> "$RESULTS"
        echo '#### dmesg (last 30 lines)' >> "$RESULTS"
        echo '```' >> "$RESULTS"
        dmesg 2>/dev/null | tail -30 >> "$RESULTS" || true
        echo '```' >> "$RESULTS"

        if [ "$CONTINUE_ON_FAIL" != "1" ]; then
            echo ""
            echo "=== SOAK FAILED at cycle $CYCLE ==="
            echo "Log: $LOG"
            echo "Results: $RESULTS"
            exit 1
        fi
    fi

    echo "Cycle $CYCLE: $PASS pass, $FAIL fail (elapsed ${ELAPSED}s rss=${RSS}KB fds=${FDS})"
    [ "$DELAY" -gt 0 ] && sleep "$DELAY"
done

echo ""
echo "=== Soak complete: $CYCLE cycles, $TOTAL_PASS passed, $TOTAL_FAIL failed ==="
echo "Max RSS: $MAX_RSS KB, Max FDs: $MAX_FDS"
echo "Results: $RESULTS"
