#!/usr/bin/env bash
# tests/e2e_multitenant.sh — validate multi-tenant auth, RBAC, tenant isolation,
# and per-tenant VM quota over the HTTP API. Run as root on c8i.
set -uo pipefail

ORCH_ROOT="${ORCH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
TARIT="${TARIT:-$ORCH_ROOT/target/debug/taritd}"
BASE_ROOTFS="${BASE_ROOTFS:-/tmp/vsock-rootfs.ext4}"
ROOTFS=/tmp/taritd-pty-rootfs.ext4
BASE=http://127.0.0.1:8080

export TARIT_API_KEY="admin-key"                      # legacy => default/admin/unlimited
export TARIT_API_KEYS="key-a:tenantA:user:1,key-b:tenantB:user:5"
export TARIT_LISTEN="127.0.0.1:8080"
export TARIT_VMM_BIN="$VMM_ROOT/target/debug/vmm"
export TARIT_KERNEL="/tmp/vmlinux.microvm"
export TARIT_ROOTFS="$ROOTFS"
export TARIT_ROOTFS_READONLY="0"
export TARIT_ENABLE_NET="0"
export TARIT_MAX_VMS="8"
export TARIT_SOCKET_DIR="${TARIT_SOCKET_DIR:-$TARITD_HOME/sockets}"
export TARIT_DB="${TARIT_DB:-$TARITD_HOME/fleet.db}"
export RUST_LOG="info"
LOG=/tmp/taritd-mt.log
PASS=1
mkdir -p "$TARIT_SOCKET_DIR"; rm -f "$TARIT_DB" "$LOG"
for p in $(pgrep -f 'target/debug/taritd' 2>/dev/null) $(pgrep -f 'vmm serve' 2>/dev/null); do kill "$p" 2>/dev/null || true; done
sleep 1

make -C "$VMM_ROOT/guest/agent" >/dev/null 2>&1 || true
cp -f "$BASE_ROOTFS" "$ROOTFS"; e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$VMM_ROOT/guest/agent/bake-agent.sh" "$ROOTFS" "$VMM_ROOT/guest/agent/vmm-agent" >/dev/null

"$TARIT" serve >"$LOG" 2>&1 & SP=$!
sleep 4
cleanup() { kill "$SP" 2>/dev/null || true; sleep 1; }
trap cleanup EXIT

code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
create() { curl -s -H "X-API-Key: $1" -H 'Content-Type: application/json' -d '{"memory_mib":512,"vcpus":1}' "$BASE/v1/vms"; }
ids() { python3 -c 'import sys,json
d=json.load(sys.stdin)
d=d if isinstance(d,list) else d.get("vms",[])
print(" ".join(x.get("id","") for x in d))'; }

echo "=== 1) bad key -> 401 ==="
C=$(code -H 'X-API-Key: nope' "$BASE/v1/vms"); echo "  GET /v1/vms bad key: $C"; [ "$C" = 401 ] || { echo FAIL; PASS=0; }

echo "=== 2) RBAC: user cannot GET /v1/cluster (admin only) ==="
C=$(code -H 'X-API-Key: key-a' "$BASE/v1/cluster"); echo "  user -> /v1/cluster: $C"; [ "$C" = 403 ] || { echo FAIL; PASS=0; }
C=$(code -H 'X-API-Key: admin-key' "$BASE/v1/cluster"); echo "  admin -> /v1/cluster: $C"; [ "$C" = 200 ] || { echo FAIL; PASS=0; }

echo "=== 3) create one VM per tenant ==="
A_VM=$(create key-a | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
B_VM=$(create key-b | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
echo "  tenantA VM=$A_VM  tenantB VM=$B_VM"
[ -n "$A_VM" ] && [ -n "$B_VM" ] || { echo "FAIL: create"; PASS=0; }

echo "=== 4) tenant isolation: each key sees only its own VMs ==="
LA=$(curl -s -H 'X-API-Key: key-a' "$BASE/v1/vms" | ids)
LB=$(curl -s -H 'X-API-Key: key-b' "$BASE/v1/vms" | ids)
echo "  key-a sees: $LA"; echo "  key-b sees: $LB"
echo "$LA" | grep -q "$A_VM" || { echo "FAIL: A can't see its VM"; PASS=0; }
echo "$LA" | grep -q "$B_VM" && { echo "FAIL: A sees B's VM"; PASS=0; }
echo "$LB" | grep -q "$B_VM" || { echo "FAIL: B can't see its VM"; PASS=0; }
echo "$LB" | grep -q "$A_VM" && { echo "FAIL: B sees A's VM"; PASS=0; }

echo "=== 5) per-tenant quota: tenantA max_vms=1, 2nd create -> 403 ==="
C=$(code -H 'X-API-Key: key-a' -H 'Content-Type: application/json' -d '{"memory_mib":512,"vcpus":1}' "$BASE/v1/vms")
echo "  tenantA 2nd create: $C"; [ "$C" = 403 ] || { echo "FAIL: quota not enforced (got $C)"; PASS=0; }
echo "  tenantB 2nd create (quota 5):"
C=$(code -H 'X-API-Key: key-b' -H 'Content-Type: application/json' -d '{"memory_mib":512,"vcpus":1}' "$BASE/v1/vms")
echo "    $C"; [ "$C" = 200 ] || [ "$C" = 201 ] || { echo "FAIL: tenantB within quota rejected ($C)"; PASS=0; }

echo ""
if [ "$PASS" = 1 ]; then echo "RESULT: MULTITENANT_PASS"; exit 0; else echo "RESULT: MULTITENANT_FAIL"; exit 1; fi
