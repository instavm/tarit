#!/usr/bin/env bash
# Real cosign + OCI + KVM admission gate. Run as root on the isolated c8i
# runner with a known key-signed image and two distinct public keys.
set -euo pipefail
umask 077

ORCH_ROOT="${ORCH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
TARIT="${TARIT:-$ORCH_ROOT/target/release/taritd}"
TARIT_VMM_BIN="${TARIT_VMM_BIN:?set TARIT_VMM_BIN}"
TARIT_VMM_AGENT="${TARIT_VMM_AGENT:?set TARIT_VMM_AGENT}"
TARIT_KERNEL="${TARIT_KERNEL:?set TARIT_KERNEL}"
TARIT_ROOTFS="${TARIT_ROOTFS:?set TARIT_ROOTFS}"
TARIT_COSIGN_KEY="${TARIT_COSIGN_KEY:?set TARIT_COSIGN_KEY}"
TARIT_UNTRUSTED_COSIGN_KEY="${TARIT_UNTRUSTED_COSIGN_KEY:?set TARIT_UNTRUSTED_COSIGN_KEY}"
SIGNED_IMAGE_REF="${SIGNED_IMAGE_REF:?set SIGNED_IMAGE_REF}"
UNSIGNED_IMAGE_REF="${UNSIGNED_IMAGE_REF:-ubuntu:24.04}"
RUN_ROOT="${IMAGE_PROVENANCE_RUN_ROOT:-/run/taritd/image-provenance}"
LISTEN="${IMAGE_PROVENANCE_LISTEN:-127.0.0.1:18084}"
BASE_URL="http://$LISTEN"
API_KEY="image-provenance-key"

SERVER_PID=""
VM_ID=""
cleanup() {
  if [ -n "$VM_ID" ]; then
    TARIT_BASE_URL="$BASE_URL" TARIT_API_KEY="$API_KEY" \
      "$TARIT" vm delete "$VM_ID" >/dev/null 2>&1 || true
  fi
  if [ -n "$SERVER_PID" ]; then
    kill -TERM "$SERVER_PID" >/dev/null 2>&1 || true
    wait "$SERVER_PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

test "$(id -u)" = 0 || { echo "run as root" >&2; exit 1; }
for tool in cosign skopeo umoci e2fsck curl python3; do
  command -v "$tool" >/dev/null || { echo "missing $tool" >&2; exit 1; }
done
test -x "$TARIT" && test -x "$TARIT_VMM_BIN"
test -f "$TARIT_VMM_AGENT" && test -f "$TARIT_KERNEL" && test -f "$TARIT_ROOTFS"
test -f "$TARIT_COSIGN_KEY" && test -f "$TARIT_UNTRUSTED_COSIGN_KEY"
test "$(sha256sum "$TARIT_COSIGN_KEY" | cut -d' ' -f1)" != \
  "$(sha256sum "$TARIT_UNTRUSTED_COSIGN_KEY" | cut -d' ' -f1)"

install -d -m 0700 "$RUN_ROOT" "$RUN_ROOT/images" "$RUN_ROOT/sockets"
rm -f "$RUN_ROOT/fleet.db" "$RUN_ROOT/fleet.db-shm" "$RUN_ROOT/fleet.db-wal"
rm -f "$RUN_ROOT/images/signed__latest.ext4" "$RUN_ROOT/images/unsigned__latest.ext4"

common_image_env=(
  "PATH=$PATH"
  "TARIT_VMM_BIN=$TARIT_VMM_BIN"
  "TARIT_VMM_AGENT=$TARIT_VMM_AGENT"
  "TARIT_DB=$RUN_ROOT/fleet.db"
  "TARIT_IMAGES_DIR=$RUN_ROOT/images"
  "TARIT_IMAGE_REQUIRE_SIGNATURE=1"
)

echo "=== reject an unsigned image under the trusted-key policy ==="
if env "${common_image_env[@]}" TARIT_IMAGE_COSIGN_KEY="$TARIT_COSIGN_KEY" \
  "$TARIT" image build --oci "$UNSIGNED_IMAGE_REF" --name unsigned >"$RUN_ROOT/unsigned.log" 2>&1; then
  echo "unsigned image was admitted" >&2
  exit 1
fi
test ! -e "$RUN_ROOT/images/unsigned__latest.ext4"
grep -E "cosign verification failed|no matching signatures" "$RUN_ROOT/unsigned.log" >/dev/null

echo "=== admit exactly one signed manifest digest ==="
env "${common_image_env[@]}" TARIT_IMAGE_COSIGN_KEY="$TARIT_COSIGN_KEY" \
  "$TARIT" image build --oci "$SIGNED_IMAGE_REF" --name signed
env "${common_image_env[@]}" TARIT_IMAGE_COSIGN_KEY="$TARIT_COSIGN_KEY" \
  "$TARIT" --json image ls | python3 -c '
import json, re, sys
row = next(item for item in json.load(sys.stdin) if item["name"] == "signed")
sha = re.compile(r"^sha256:[0-9a-f]{64}$")
for field in ("source_digest", "rootfs_digest", "agent_digest", "provenance_key_digest"):
    assert sha.fullmatch(row.get(field) or ""), field
assert row["source_ref"].endswith(row["source_digest"])
assert row["provenance_verified_at"]
'

start_server() {
  local key=$1 log=$2
  install -d -m 0700 "$RUN_ROOT/sockets" "$RUN_ROOT/sockets/overlays" "$RUN_ROOT/sockets/snapshots"
  env PATH="$PATH" TARIT_API_KEY="$API_KEY" TARIT_LISTEN="$LISTEN" \
    TARIT_VMM_BIN="$TARIT_VMM_BIN" TARIT_VMM_AGENT="$TARIT_VMM_AGENT" \
    TARIT_KERNEL="$TARIT_KERNEL" TARIT_ROOTFS="$TARIT_ROOTFS" \
    TARIT_SOCKET_DIR="$RUN_ROOT/sockets" TARIT_DB="$RUN_ROOT/fleet.db" \
    TARIT_IMAGES_DIR="$RUN_ROOT/images" TARIT_IMAGE_REQUIRE_SIGNATURE=1 \
    TARIT_IMAGE_COSIGN_KEY="$key" TARIT_ENABLE_NET=0 \
    "$TARIT" serve >"$log" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 100); do
    curl -fsS "$BASE_URL/health" >/dev/null 2>&1 && return
    kill -0 "$SERVER_PID" 2>/dev/null || { tail -50 "$log" >&2; exit 1; }
    sleep 0.1
  done
  echo "taritd did not become ready" >&2
  exit 1
}

stop_server() {
  kill -TERM "$SERVER_PID"
  wait "$SERVER_PID"
  SERVER_PID=""
}

echo "=== a provenance-policy rotation fences the old admission ==="
start_server "$TARIT_UNTRUSTED_COSIGN_KEY" "$RUN_ROOT/untrusted-server.log"
code=$(curl -sS -o "$RUN_ROOT/untrusted-response.json" -w '%{http_code}' \
  -H "X-API-Key: $API_KEY" -H 'Content-Type: application/json' \
  --data '{"image":"signed","memory_mib":256,"vcpus":1}' "$BASE_URL/v1/vms")
test "$code" = 422
grep -F "currently trusted provenance key" "$RUN_ROOT/untrusted-response.json" >/dev/null
stop_server

echo "=== rolling back the trusted key readmits the same pinned digest ==="
env "${common_image_env[@]}" TARIT_IMAGE_COSIGN_KEY="$TARIT_COSIGN_KEY" \
  "$TARIT" image verify signed | grep -F 'verified signed:latest sha256:' >/dev/null

echo "RESULT: IMAGE_PROVENANCE_PASS"
