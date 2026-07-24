#!/usr/bin/env bash
# tests/e2e_image_pipeline.sh — build a golden rootfs from an OCI image via the
# orchestrator, then boot a VM from it and run node -v. Run as root on c8i.
set -uo pipefail

ORCH_ROOT="${ORCH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
TARIT="${TARIT:-$ORCH_ROOT/target/debug/taritd}"
export TARIT_API_KEY="img-key"
export TARIT_LISTEN="127.0.0.1:8080"
export TARIT_VMM_BIN="$VMM_ROOT/target/debug/vmm"
export TARIT_KERNEL="${TARIT_KERNEL:-/tmp/vmlinux.microvm}"
export TARIT_ROOTFS="${TARIT_ROOTFS:-/tmp/vsock-rootfs.ext4}"
export TARIT_ROOTFS_READONLY="0"
export TARIT_ENABLE_NET="0"
export TARIT_MAX_VMS="6"
export TARIT_SOCKET_DIR="${TARIT_SOCKET_DIR:-$TARITD_HOME/sockets}"
export TARIT_DB="${TARIT_DB:-$TARITD_HOME/fleet.db}"
export TARIT_IMAGES_DIR="${TARIT_IMAGES_DIR:-$TARITD_HOME/images}"
export TARIT_VMM_AGENT="$VMM_ROOT/guest/agent/vmm-agent"
export TARIT_BASE_URL="http://127.0.0.1:8080"
export TARIT_CONFIG="/tmp/img-empty.toml"
export RUST_LOG="info"
: > /tmp/img-empty.toml
PASS=1
mkdir -p "$TARIT_SOCKET_DIR" "$TARIT_IMAGES_DIR"; rm -f "$TARIT_DB"
rm -f "$TARIT_IMAGES_DIR"/node20*.ext4 2>/dev/null || true
make -C "$VMM_ROOT/guest/agent" >/dev/null 2>&1 || true
for p in $(pgrep -f 'target/debug/taritd' 2>/dev/null) $(pgrep -f 'vmm serve' 2>/dev/null); do kill "$p" 2>/dev/null || true; done
sleep 1

echo "=== check OCI tooling ==="
for t in umoci skopeo; do command -v "$t" >/dev/null 2>&1 && echo "  $t: $(command -v $t)" || echo "  $t: MISSING"; done

"$TARIT" serve >/tmp/taritd-img.log 2>&1 & SP=$!
sleep 4
cleanup() { [ -n "${VM_ID:-}" ] && "$TARIT" vm delete "$VM_ID" >/dev/null 2>&1 || true; kill "$SP" 2>/dev/null || true; sleep 1; }
trap cleanup EXIT

echo "=== taritd image build --oci node:20-slim --name node20 (slow: pull+convert) ==="
"$TARIT" image build --oci node:20-slim --name node20 2>&1 | tail -8 || { echo "FAIL: image build"; PASS=0; }

echo "=== taritd image ls ==="
"$TARIT" image ls 2>&1 | tail -6
"$TARIT" image ls 2>&1 | grep -q node20 || { echo "FAIL: node20 not registered"; PASS=0; }

echo "=== create VM from image node20 ==="
VM_ID=$("$TARIT" --json vm create --image node20 --vcpus 1 --memory-mib 512 2>/dev/null | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
echo "  VM_ID=$VM_ID"
[ -n "$VM_ID" ] || { echo "FAIL: create from image"; tail -20 /tmp/taritd-img.log; PASS=0; }
echo "  (20s boot)"; sleep 20

echo "=== taritd exec node -v ==="
OUT=$("$TARIT" exec "$VM_ID" "node -v" 2>&1)
echo "  $OUT"
echo "$OUT" | grep -qE 'v20\.' || { echo "FAIL: node -v did not return v20.x"; PASS=0; }

echo ""
if [ "$PASS" = 1 ]; then echo "RESULT: IMAGE_PIPELINE_PASS"; exit 0; else echo "RESULT: IMAGE_PIPELINE_FAIL"; exit 1; fi
