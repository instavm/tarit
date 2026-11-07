#!/usr/bin/env bash
# Checkpoint loops through taritd: snapshot, restore, change, snapshot again,
# hibernate and wake, with several loops at once, verifying every restored
# checkpoint byte for byte (see checkpoint_e2e.py). Requires Linux, KVM, root,
# and prepared guest assets.
set -euo pipefail

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD="${TARITD_BIN:-$ROOT/orch/target/release/taritd}"
VMM="${TARIT_VMM_BIN:-$ROOT/vmm/target/release/vmm}"
KERNEL="${TARIT_KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${TARIT_ROOTFS:-/tmp/vsock-rootfs.ext4}"
KEY="checkpoint-e2e-key"
PARALLEL="${CHECKPOINT_E2E_PARALLEL:-2}"

for required in curl python3 setsid ps; do
  command -v "$required" >/dev/null || {
    echo "FAIL: required command '$required' is missing" >&2
    exit 1
  }
done
PORT=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
DIR=$(mktemp -d "${TMPDIR:-/tmp}/tarit-checkpoint-e2e.XXXXXX")
BASE_URL="http://127.0.0.1:$PORT"
mkdir -p "$DIR/sockets"

cleanup() {
  if [ -n "${TARITD_PGID:-}" ] && kill -0 -- "-$TARITD_PGID" 2>/dev/null; then
    kill -TERM -- "-$TARITD_PGID" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 -- "-$TARITD_PGID" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL -- "-$TARITD_PGID" 2>/dev/null || true
  fi
  [ -z "${TARITD_PID:-}" ] || wait "$TARITD_PID" 2>/dev/null || true
  rm -rf -- "$DIR"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Each loop holds at most four 2-vCPU VMs at once: the source, two restores
# it keeps working with, and one restore that is checked and deleted.
TARIT_API_KEY="$KEY" \
TARIT_LISTEN="127.0.0.1:$PORT" \
TARIT_RPC_ADDR="$BASE_URL" \
TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
TARIT_VMM_BIN="$VMM" \
TARIT_KERNEL="$KERNEL" \
TARIT_ROOTFS="$ROOTFS" \
TARIT_ROOTFS_READONLY=0 \
TARIT_ENABLE_NET=0 \
TARIT_SOCKET_DIR="$DIR/sockets" \
TARIT_DB="$DIR/fleet.db" \
TARIT_CONFIG="$DIR/none.toml" \
TARIT_WARM_POOL=0 \
TARIT_MAX_VMS=$((PARALLEL * 4 + 2)) \
TARIT_MAX_VCPUS=$((PARALLEL * 8 + 4)) \
TARIT_MAX_MEMORY_MIB=$((PARALLEL * 4 * 512 + 1024)) \
TARIT_REAP_ON_SHUTDOWN=true \
TARIT_PRODUCTION=0 \
RUST_LOG=taritd=info \
setsid "$TARITD" serve >"$DIR/taritd.log" 2>&1 &
TARITD_PID=$!
TARITD_PGID=$TARITD_PID

ready=0
for _ in $(seq 1 60); do
  if curl -fsS --max-time 1 "$BASE_URL/health" >/dev/null 2>&1; then
    ready=1
    break
  fi
  kill -0 "$TARITD_PID" 2>/dev/null || break
  sleep 0.25
done
if [ "$ready" -ne 1 ]; then
  echo "FAIL: taritd did not become healthy"
  tail -80 "$DIR/taritd.log"
  exit 1
fi

if ! TARIT_BASE_URL="$BASE_URL" TARIT_API_KEY="$KEY" CHECKPOINT_E2E_PARALLEL="$PARALLEL" \
  python3 "$ROOT/orch/tests/checkpoint_e2e.py"; then
  echo "FAIL: checkpoint loops failed; taritd log tail:"
  tail -120 "$DIR/taritd.log"
  exit 1
fi
echo "RESULT: CHECKPOINT_PASS loops=$PARALLEL"
