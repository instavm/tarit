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
# Usage:  ./scripts/bench-warmpool.sh            # restore-based pool, 200 VMs, n=100
#         MODE=cold TARGET=100 N=50 ./scripts/bench-warmpool.sh
set -uo pipefail

REPO="${REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_BIN="${TARIT_VMM_BIN:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${TARIT_KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${TARIT_ROOTFS:-/tmp/bench-node-rootfs.ext4}"
TARGET="${TARGET:-200}"          # warm pool size
N="${N:-100}"                    # iterations (sequential) / concurrency (burst, staggered)
MEM="${MEM:-512}"                # MiB per VM
VCPUS="${VCPUS:-1}"
CMD="${CMD:-node -v}"
MODE="${MODE:-restore}"          # restore = clone from one golden snapshot; cold = boot each
PORT="${PORT:-8080}"
DB=/tmp/taritd-bench.db
CFG=/tmp/taritd-bench.toml
LOG=/tmp/taritd-bench.log

echo "== host =="; uname -srm; echo "vCPUs: $(nproc)"
if [ -e /dev/kvm ]; then echo "KVM: present"; else echo "KVM: MISSING (need a KVM host)"; exit 1; fi
lscpu | grep -qi hypervisor && echo "virt: nested (KVM-exit tax applies)" || echo "virt: bare metal"
echo

command -v cargo >/dev/null || { echo "cargo required"; exit 1; }
[ -x "$VMM_BIN" ] || { echo "vmm binary not found at $VMM_BIN (build vmm first)"; exit 1; }
[ -f "$KERNEL" ] || { echo "kernel not found at $KERNEL"; exit 1; }
[ -f "$ROOTFS" ] || { echo "rootfs not found at $ROOTFS"; exit 1; }

echo "building taritd + tarit-bench..."
if [ ! -x "$REPO/target/release/taritd" ] || [ ! -x "$REPO/target/release/tarit-bench" ]; then
  ( cd "$REPO" && cargo build --release -p taritd -p tarit-bench ) || exit 1
fi
TARIT="$REPO/target/release/taritd"; BENCH="$REPO/target/release/tarit-bench"

# A shared read-only rootfs must be journal-clean or the guest panics on mount.
command -v e2fsck >/dev/null && sudo e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true

RESTORE_LINE=""; [ "$MODE" = "restore" ] && RESTORE_LINE="restore = true"
cat > "$CFG" <<EOF
[warm_pool]
enabled = true
cpu_overcommit = 8.0
replenish_concurrency = 100

[[warm_pool.class]]
vcpus = $VCPUS
memory_mib = $MEM
target = $TARGET
$RESTORE_LINE
rootfs = "$ROOTFS"
EOF

# Sweep any previous run (kill by pattern -- these names never match this script).
cleanup() {
  for p in $(pgrep -f "release/taritd" 2>/dev/null); do kill "$p" 2>/dev/null || true; done
  sleep 1
  for p in $(pgrep -f "vmm serve" 2>/dev/null); do kill "$p" 2>/dev/null || true; done
  sleep 1
  for p in $(pgrep -f "vmm serve" 2>/dev/null); do kill -9 "$p" 2>/dev/null || true; done
  rm -f "$DB" "$DB-wal" "$DB-shm" 2>/dev/null || true
}
trap cleanup EXIT
cleanup

TARIT_API_KEY=test-key TARIT_VMM_BIN="$VMM_BIN" TARIT_KERNEL="$KERNEL" \
TARIT_ROOTFS="$ROOTFS" TARIT_ROOTFS_READONLY=1 TARIT_CONFIG="$CFG" TARIT_DB="$DB" \
TARIT_MAX_VMS=$((TARGET + 60)) TARIT_MAX_VCPUS=2048 TARIT_MAX_MEMORY_MIB=300000 \
TARIT_ADMISSION_TIMEOUT_MS=180000 RUST_LOG=taritd=info \
  setsid "$TARIT" > "$LOG" 2>&1 < /dev/null &
sleep 3

echo "filling warm pool to $TARGET ($MODE)..."
for _ in $(seq 1 240); do
  c=$(pgrep -c -f "vmm serve" 2>/dev/null || echo 0)
  [ "$c" -ge "$((TARGET - 1))" ] && break
  sleep 1
done
echo "warm VMs ready: $(pgrep -c -f 'vmm serve' 2>/dev/null || echo 0)"; echo

wait_warm() { for _ in $(seq 1 180); do c=$(pgrep -c -f "vmm serve" 2>/dev/null || echo 0); [ "$c" -ge "$((TARGET-1))" ] && break; sleep 1; done; }
run() { TARIT_API_KEY=test-key "$BENCH" "$@" --memory-mib "$MEM" --vcpus "$VCPUS" \
        --command "$CMD" --timeout-ms 60000 --url "http://127.0.0.1:$PORT" 2>&1 | tail -1; }

echo "== sequential n=$N ==";        wait_warm; run sequential --iterations "$N"
echo "== staggered n=$N (20ms) ==";  wait_warm; run staggered  --concurrency "$N" --stagger-delay-ms 20
echo "== burst n=$N ==";             wait_warm; run burst      --concurrency "$N"
echo; echo "done (taritd + warm VMs are torn down on exit)."
