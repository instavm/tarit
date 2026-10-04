#!/usr/bin/env bash
# tests/e2e_cpu_refill.sh — validate CPU-isolated warm-pool refill: the refill
# (warm) vmm serve children run in a dedicated cgroup with a LOW cpu.weight.
# Run as root on c8i.
set -uo pipefail

ORCH_ROOT="${ORCH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
TARIT="${TARIT:-$ORCH_ROOT/target/debug/taritd}"
BASE_ROOTFS="${BASE_ROOTFS:-${TARIT_ROOTFS:-/tmp/vsock-rootfs.ext4}}"
ROOTFS=/tmp/refill-base.ext4
CONFIG="${CONFIG:-$TARITD_HOME/refill.toml}"
REFILL_CG=/sys/fs/cgroup/taritd-refill
TARGET=2

export TARIT_API_KEY="refill-key"
export TARIT_LISTEN="127.0.0.1:8080"
export TARIT_VMM_BIN="$VMM_ROOT/target/debug/vmm"
export TARIT_KERNEL="${TARIT_KERNEL:-/tmp/vmlinux.microvm}"
export TARIT_ROOTFS="$ROOTFS"
export TARIT_ROOTFS_READONLY="1"
export TARIT_ENABLE_NET="0"
export TARIT_MAX_VMS="8"
export TARIT_SOCKET_DIR="${TARIT_SOCKET_DIR:-$TARITD_HOME/sockets}"
export TARIT_DB="${TARIT_DB:-$TARITD_HOME/fleet.db}"
export TARIT_CONFIG="$CONFIG"
export TARIT_BASE_URL="http://127.0.0.1:8080"
export TARIT_REFILL_CGROUP="$REFILL_CG"
export TARIT_REFILL_CPU_WEIGHT="10"
export RUST_LOG="info"
PASS=1
mkdir -p "$TARIT_SOCKET_DIR"; rm -f "$TARIT_DB"
for p in $(pgrep -f 'target/debug/taritd' 2>/dev/null) $(pgrep -f 'vmm serve' 2>/dev/null); do kill "$p" 2>/dev/null || true; done
sleep 1
# cgroup v2: cpu controller for children of root; clear any stale refill cgroup.
echo "+cpu +memory +pids" > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || true
rmdir "$REFILL_CG" 2>/dev/null || true

make -C "$VMM_ROOT/guest/agent" >/dev/null 2>&1 || true
cp -f "$BASE_ROOTFS" "$ROOTFS"; e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$VMM_ROOT/guest/agent/bake-agent.sh" "$ROOTFS" "$VMM_ROOT/guest/agent/vmm-agent" >/dev/null
e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true

cat > "$CONFIG" <<TOML
[warm_pool]
enabled = true
replenish_concurrency = 2

[[warm_pool.class]]
vcpus = 1
memory_mib = 512
target = $TARGET
restore = true
rootfs = "$ROOTFS"
TOML

"$TARIT" serve >/tmp/taritd-refill.log 2>&1 & SP=$!
sleep 4
cleanup() { kill "$SP" 2>/dev/null || true; sleep 1; rmdir "$REFILL_CG" 2>/dev/null || true; }
trap cleanup EXIT

echo "=== wait for warm pool (restore) to fill target=$TARGET ==="
for i in $(seq 1 90); do
  d=$("$TARIT" metrics 2>/dev/null | awk -F' ' '/taritd_warm_pool_depth\{class="1vcpu_512mib"\}/{print $2}')
  [ "${d:-0}" -ge "$TARGET" ] && { echo "  filled: depth=$d"; break; }
  sleep 2
done

echo "=== inspect refill cgroup ==="
WEIGHT=$(cat "$REFILL_CG/cpu.weight" 2>&1); echo "  $REFILL_CG/cpu.weight = $WEIGHT (want 10)"
REFILL_PROCS=$(cat "$REFILL_CG/cgroup.procs" 2>/dev/null | tr '\n' ' ')
echo "  refill cgroup.procs = [$REFILL_PROCS]"
VMM_PIDS=$(pgrep -f 'vmm serve' 2>/dev/null | tr '\n' ' ')
echo "  all vmm serve pids   = [$VMM_PIDS]"
TARIT_PID=$SP
echo "  taritd pid ($TARIT_PID) in refill cgroup? $(grep -qw "$TARIT_PID" "$REFILL_CG/cgroup.procs" 2>/dev/null && echo yes || echo no) (want no)"

N_REFILL=$(echo "$REFILL_PROCS" | wc -w)

echo ""
echo "=== verdict ==="
[ "$WEIGHT" = "10" ] || { echo "FAIL: refill cgroup cpu.weight != 10"; PASS=0; }
[ "$N_REFILL" -ge "$TARGET" ] || { echo "FAIL: fewer than $TARGET refill pids in the cgroup (got $N_REFILL)"; PASS=0; }
grep -qw "$TARIT_PID" "$REFILL_CG/cgroup.procs" 2>/dev/null && { echo "FAIL: taritd itself is in the refill cgroup"; PASS=0; }
echo "=== claim restored warm VMs and execute in each guest ==="
for index in 1 2; do
  VM_ID=$("$TARIT" --json vm create --vcpus 1 --memory-mib 512 2>/dev/null |
    python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')
  if [ -z "$VM_ID" ] ||
      ! "$TARIT" exec "$VM_ID" "bash -c 'echo refill-runtime-$index-ok'" 2>&1 |
        grep -q "refill-runtime-$index-ok"; then
    echo "FAIL: restored warm VM $index did not execute"
    PASS=0
  fi
  [ -z "$VM_ID" ] || "$TARIT" vm delete "$VM_ID" >/dev/null 2>&1 || true
done
if [ "$PASS" = 1 ]; then echo "RESULT: CPU_REFILL_PASS"; exit 0; else echo "RESULT: CPU_REFILL_FAIL"; exit 1; fi
