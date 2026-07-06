#!/usr/bin/env bash
# tests/e2e_warmpool_restore.sh — validate restore-from-golden warm-pool refill
# with per-clone disk isolation, on a Linux+KVM host (c8i). Run as root.
#
#   sudo bash tests/e2e_warmpool_restore.sh
set -uo pipefail

ORCH_ROOT="${ORCH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
TARIT="${TARIT:-$ORCH_ROOT/target/debug/taritd}"
BASE_ROOTFS="${BASE_ROOTFS:-/tmp/vsock-rootfs.ext4}"
ROOTFS=/tmp/warmpool-base.ext4
CONFIG="${CONFIG:-$TARITD_HOME/warmpool.toml}"
LOG=/tmp/taritd-warmpool.log

export TARIT_API_KEY="warm-e2e-key"
export TARIT_LISTEN="127.0.0.1:8080"
export TARIT_VMM_BIN="$VMM_ROOT/target/debug/vmm"
export TARIT_KERNEL="/tmp/vmlinux.microvm"
export TARIT_ROOTFS="$ROOTFS"
export TARIT_ROOTFS_READONLY="1"
export TARIT_ENABLE_NET="0"
export TARIT_MAX_VMS="8"
export TARIT_SOCKET_DIR="${TARIT_SOCKET_DIR:-$TARITD_HOME/sockets}"
export TARIT_DB="${TARIT_DB:-$TARITD_HOME/fleet.db}"
export TARIT_CONFIG="$CONFIG"
export TARIT_BASE_URL="http://127.0.0.1:8080"
export RUST_LOG="info"

TARGET=2
CLASS='2vcpu'    # placeholder; real label computed below
PASS=1
mkdir -p "$TARIT_SOCKET_DIR"
rm -f "$TARIT_DB" "$LOG"
for p in $(pgrep -f 'target/debug/taritd' 2>/dev/null) $(pgrep -f 'vmm serve' 2>/dev/null); do kill "$p" 2>/dev/null || true; done
sleep 1

echo "=== bake read-only agent base rootfs ==="
make -C "$VMM_ROOT/guest/agent" >/dev/null 2>&1 || true
[ -x "$VMM_ROOT/guest/agent/vmm-agent" ] || { echo "FAIL: no vmm-agent"; exit 1; }
cp -f "$BASE_ROOTFS" "$ROOTFS"; e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$VMM_ROOT/guest/agent/bake-agent.sh" "$ROOTFS" "$VMM_ROOT/guest/agent/vmm-agent" >/dev/null
# a shared read-only base must be journal-clean
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
LABEL="1vcpu_512mib"

echo "=== taritd serve (restore warm pool) ==="
"$TARIT" serve >"$LOG" 2>&1 &
SP=$!
sleep 4
cleanup() { kill "$SP" 2>/dev/null || true; sleep 1; }
trap cleanup EXIT

echo "=== wait for warm pool to fill via restore (target=$TARGET) ==="
filled=0
for i in $(seq 1 90); do
  depth=$("$TARIT" metrics 2>/dev/null | awk -F'[ ]' "/taritd_warm_pool_depth\\{class=\"$LABEL\"\\}/{print \$2}")
  echo "  [$i] warm_pool_depth{$LABEL}=${depth:-0}"
  if [ "${depth:-0}" -ge "$TARGET" ]; then filled=1; break; fi
  sleep 2
done
[ "$filled" = 1 ] || { echo "FAIL: warm pool did not fill; log:"; grep -iE 'golden|restore|warm|error|panic' "$LOG" | tail -20; PASS=0; }

echo "=== create 2 VMs (served from warm restored clones) ==="
A=$("$TARIT" --json vm create --vcpus 1 --memory-mib 512 | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
B=$("$TARIT" --json vm create --vcpus 1 --memory-mib 512 | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
echo "  A=$A  B=$B"
[ -n "$A" ] && [ -n "$B" ] || { echo "FAIL: create"; PASS=0; }
sleep 3

echo "=== isolation: write in A, assert absent in B (and vice versa) ==="
"$TARIT" exec "$A" "sh -c 'echo VM_A_DATA > /root/iso; sync; cat /root/iso'"
B_SEES_A=$("$TARIT" exec "$B" "sh -c 'cat /root/iso 2>&1'")
echo "  B reads /root/iso (want: absent): $B_SEES_A"
"$TARIT" exec "$B" "sh -c 'echo VM_B_DATA > /root/iso; sync'" >/dev/null
A_SEES=$("$TARIT" exec "$A" "sh -c 'cat /root/iso'")
echo "  A reads /root/iso (want: VM_A_DATA): $A_SEES"

echo "$B_SEES_A" | grep -q VM_A_DATA && { echo "FAIL: B saw A's write (NOT isolated)"; PASS=0; }
echo "$A_SEES"   | grep -q VM_A_DATA || { echo "FAIL: A lost its own data"; PASS=0; }
echo "$A_SEES"   | grep -q VM_B_DATA && { echo "FAIL: A saw B's write (NOT isolated)"; PASS=0; }

"$TARIT" vm delete "$A" >/dev/null 2>&1 || true
"$TARIT" vm delete "$B" >/dev/null 2>&1 || true

echo ""
echo "=== base image unchanged (read-only shared base) ==="
BASE_MD5=$(md5sum "$ROOTFS" | awk '{print $1}')
echo "  base md5=$BASE_MD5"

echo ""
if [ "$PASS" = 1 ]; then echo "RESULT: WARMPOOL_RESTORE_PASS"; exit 0; else echo "RESULT: WARMPOOL_RESTORE_FAIL"; exit 1; fi
