#!/usr/bin/env bash
#
# bench-warmpool.sh -- reproduce the taritd warm-pool Time-To-Interactive numbers.
#
# TTI is measured per iteration from create() to the first `node -v` that returns
# exit 0 (the ComputeSDK metric). Runs sequential, staggered and burst at n=100.
#
# Reference run: AWS c8i.metal-48xl (192 vCPU / 384 GiB, bare metal), us-east-1,
# Ubuntu 24.04 + KVM. See BENCHMARK-RESULTS.md for the machine and the numbers.
#
# Requires a Linux + KVM host. Bare metal gives the headline numbers; a nested
# KVM guest (e.g. c8i.xlarge) works too but pays a ~10x KVM-exit tax.
#
# Prereqs (see BENCHMARK-RESULTS.md "Reproducing"):
#   - vmm built:  $TARIT_VMM_BIN            (default ~/tarit/vmm/target/release/vmm)
#   - a guest kernel: $TARIT_KERNEL             (default /tmp/vmlinux.microvm)
#   - a node rootfs:  $TARIT_ROOTFS             (ext4 with node + the vmm-agent)
#
# Usage:  ./scripts/bench-warmpool.sh              # snapshot-refilled warm pool
#         MODE=cold TARGET=100 N=100 ./scripts/bench-warmpool.sh
#         MODE=direct N=100 ./scripts/bench-warmpool.sh # real cold create path
set -euo pipefail

REPO="${REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_BIN="${TARIT_VMM_BIN:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${TARIT_KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${TARIT_ROOTFS:-/tmp/bench-node-rootfs.ext4}"
TARGET="${TARGET:-200}"          # warm pool size
N="${N:-100}"                    # iterations (sequential) / concurrency (burst, staggered)
MEM="${MEM:-512}"                # MiB per VM
VCPUS="${VCPUS:-1}"
CMD="${CMD:-node -v}"
MODE="${MODE:-restore}"          # restore/cold = warm pool refill source; direct = no warm pool
BENCHMARK_MODE="${BENCHMARK_MODE:-all}"
RESULTS_DIR="${RESULTS_DIR:-$REPO/bench-results}"
MIN_SUCCESS_PERCENT="${MIN_SUCCESS_PERCENT:-100}"
PORT="${PORT:-}"

for numeric in TARGET N MEM VCPUS; do
  value="${!numeric}"
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || {
    echo "$numeric must be a positive integer" >&2
    exit 2
  }
done
case "$BENCHMARK_MODE" in
  all|sequential|staggered|burst) ;;
  *) echo "BENCHMARK_MODE must be all, sequential, staggered, or burst" >&2; exit 2 ;;
esac

echo "== host =="; uname -srm; echo "vCPUs: $(nproc)"
if [ -e /dev/kvm ]; then echo "KVM: present"; else echo "KVM: MISSING (need a KVM host)"; exit 1; fi
lscpu | grep -qi hypervisor && echo "virt: nested (KVM-exit tax applies)" || echo "virt: bare metal"
echo

command -v cargo >/dev/null || { echo "cargo required"; exit 1; }
for required in curl python3 setsid ps cp; do
  command -v "$required" >/dev/null || { echo "$required required"; exit 1; }
done
[ -x "$VMM_BIN" ] || { echo "vmm binary not found at $VMM_BIN (build vmm first)"; exit 1; }
[ -f "$KERNEL" ] || { echo "kernel not found at $KERNEL"; exit 1; }
[ -f "$ROOTFS" ] || { echo "rootfs not found at $ROOTFS"; exit 1; }

if [ -z "$PORT" ]; then
  PORT=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
fi
[[ "$PORT" =~ ^[0-9]+$ ]] && [ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || {
  echo "PORT must be an integer between 1 and 65535" >&2
  exit 2
}

RUN_DIR=$(mktemp -d "${TMPDIR:-/tmp}/taritd-bench.XXXXXX")
DB="$RUN_DIR/fleet.db"
CFG="$RUN_DIR/taritd.toml"
LOG="$RUN_DIR/taritd.log"
SOCKET_DIR="$RUN_DIR/sockets"
RUN_ROOTFS="$RUN_DIR/rootfs.ext4"
mkdir -p "$SOCKET_DIR"
TARITD_PID=
TARITD_PGID=

cleanup() {
  if [ -n "$TARITD_PGID" ] && kill -0 -- "-$TARITD_PGID" 2>/dev/null; then
    kill -TERM -- "-$TARITD_PGID" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 -- "-$TARITD_PGID" 2>/dev/null || break
      sleep 0.1
    done
    if kill -0 -- "-$TARITD_PGID" 2>/dev/null; then
      kill -KILL -- "-$TARITD_PGID" 2>/dev/null || true
    fi
  elif [ -n "$TARITD_PID" ] && kill -0 "$TARITD_PID" 2>/dev/null; then
    kill -TERM "$TARITD_PID" 2>/dev/null || true
  fi
  if [ -n "$TARITD_PID" ]; then
    wait "$TARITD_PID" 2>/dev/null || true
  fi
  rm -rf -- "$RUN_DIR"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

echo "building taritd + tarit-bench..."
if [ ! -x "$REPO/target/release/taritd" ] || [ ! -x "$REPO/target/release/tarit-bench" ]; then
  ( cd "$REPO" && cargo build --release -p taritd -p tarit-bench ) || exit 1
fi
TARIT="$REPO/target/release/taritd"; BENCH="$REPO/target/release/tarit-bench"

# Never mutate a caller-owned or concurrently used rootfs. Reflink when the
# backing filesystem supports it and run any journal repair on the private copy.
cp --reflink=auto -- "$ROOTFS" "$RUN_ROOTFS"
command -v e2fsck >/dev/null && sudo e2fsck -fy "$RUN_ROOTFS" >/dev/null 2>&1 || true

case "$MODE" in
  restore) ENABLED=true; RESTORE_LINE="restore = true"; STARTUP_PATH=warm ;;
  cold)    ENABLED=true; RESTORE_LINE=""; STARTUP_PATH=warm ;;
  direct)  ENABLED=false; RESTORE_LINE=""; STARTUP_PATH=cold ;;
  *) echo "MODE must be restore, cold, or direct" >&2; exit 2 ;;
esac
cat > "$CFG" <<EOF
[warm_pool]
enabled = $ENABLED
cpu_overcommit = 8.0
replenish_concurrency = 100

[[warm_pool.class]]
vcpus = $VCPUS
memory_mib = $MEM
target = $TARGET
$RESTORE_LINE
rootfs = "$RUN_ROOTFS"
EOF

TARIT_API_KEY=test-key TARIT_VMM_BIN="$VMM_BIN" TARIT_KERNEL="$KERNEL" \
TARIT_ROOTFS="$RUN_ROOTFS" TARIT_ROOTFS_READONLY="${TARIT_ROOTFS_READONLY:-0}" \
TARIT_CONFIG="$CFG" TARIT_DB="$DB" TARIT_SOCKET_DIR="$SOCKET_DIR" \
TARIT_LISTEN="127.0.0.1:$PORT" TARIT_RPC_ADDR="http://127.0.0.1:$PORT" \
TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
TARIT_MAX_VMS=$((TARGET + N + 16)) TARIT_MAX_VCPUS=$((TARGET + N + 16)) \
TARIT_MAX_MEMORY_MIB=$(((TARGET + N + 16) * MEM)) \
TARIT_ADMISSION_TIMEOUT_MS=180000 RUST_LOG=taritd=info \
  setsid "$TARIT" serve > "$LOG" 2>&1 < /dev/null &
TARITD_PID=$!
TARITD_PGID=$TARITD_PID

ready=false
for _ in $(seq 1 120); do
  if curl -fsS --max-time 1 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
    ready=true
    break
  fi
  kill -0 "$TARITD_PID" 2>/dev/null || break
  sleep 0.25
done
if [ "$ready" != true ]; then
  echo "taritd did not become healthy; log follows" >&2
  tail -80 "$LOG" >&2 || true
  exit 1
fi
actual_pgid=$(ps -o pgid= -p "$TARITD_PID" | tr -d ' ')
if [ "$actual_pgid" != "$TARITD_PGID" ]; then
  echo "taritd did not start in its own process group" >&2
  TARITD_PGID=
  exit 1
fi

warm_depth() {
  curl -fsS --max-time 2 -H 'X-API-Key: test-key' "http://127.0.0.1:$PORT/metrics" |
    awk -v class="${VCPUS}vcpu_${MEM}mib" '
      $1 == "taritd_warm_pool_depth{class=\"" class "\"}" { print int($2); found=1 }
      END { if (!found) exit 1 }
    '
}
wait_warm() {
  [ "$MODE" = direct ] && return
  local depth=0
  for _ in $(seq 1 240); do
    depth=$(warm_depth 2>/dev/null || echo 0)
    [ "$depth" -ge "$TARGET" ] && return
    kill -0 "$TARITD_PID" 2>/dev/null || break
    sleep 1
  done
  echo "warm pool failed to reach target $TARGET (reported depth $depth)" >&2
  tail -80 "$LOG" >&2 || true
  return 1
}

GATE_ARGS=()
[ -n "${MAX_MEDIAN_MS:-}" ] && GATE_ARGS+=(--max-median-ms "$MAX_MEDIAN_MS")
[ -n "${MAX_P95_MS:-}" ] && GATE_ARGS+=(--max-p95-ms "$MAX_P95_MS")
[ -n "${MAX_P99_MS:-}" ] && GATE_ARGS+=(--max-p99-ms "$MAX_P99_MS")
GATE_ARGS+=(--min-success-percent "$MIN_SUCCESS_PERCENT")

run() { TARIT_API_KEY=test-key "$BENCH" "$@" --memory-mib "$MEM" --vcpus "$VCPUS" \
        --startup-path "$STARTUP_PATH" "${GATE_ARGS[@]}" --command "$CMD" \
        --results-dir "$RESULTS_DIR" --timeout-ms 60000 \
        --url "http://127.0.0.1:$PORT"; }

if [ "$MODE" != direct ]; then
  echo "filling warm pool to $TARGET ($MODE refill)..."
  wait_warm
  echo "warm pool ready: depth=$(warm_depth)"; echo
else
  echo "warm pool disabled: measuring the cold create path"; echo
fi

if [ "$BENCHMARK_MODE" = all ] || [ "$BENCHMARK_MODE" = sequential ]; then
  echo "== sequential n=$N =="; wait_warm; run sequential --iterations "$N"
fi
if [ "$BENCHMARK_MODE" = all ] || [ "$BENCHMARK_MODE" = staggered ]; then
  echo "== staggered n=$N (20ms) =="; wait_warm
  run staggered --concurrency "$N" --stagger-delay-ms 20
fi
if [ "$BENCHMARK_MODE" = all ] || [ "$BENCHMARK_MODE" = burst ]; then
  echo "== burst n=$N =="; wait_warm; run burst --concurrency "$N"
fi
echo; echo "done (taritd + warm VMs are torn down on exit)."
