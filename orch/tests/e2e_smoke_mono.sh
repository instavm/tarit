#!/usr/bin/env bash
# Monorepo smoke test: prove taritd <-> vmm wire compatibility through the shared
# tarit-proto crate, and that the PTY framing still works after the de-dup.
# Single-host (no fleet DB needed). Run as root on c8i.
set -uo pipefail
ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD=$ROOT/orch/target/release/taritd
VMM=$ROOT/vmm/target/debug/vmm
KEY=smoke-key
PORT=18099
DIR=/tmp/tarit-smoke-$$
ROOTFS=/tmp/tarit-smoke-rootfs.ext4
mkdir -p "$DIR/sockets"
PASS=1; note(){ printf '%s\n' "$*"; }; fail(){ printf 'FAIL %s\n' "$*"; PASS=0; }

cp -f /tmp/vsock-rootfs.ext4 "$ROOTFS"
make -C "$ROOT/vmm/guest/agent" >/dev/null 2>&1 || true
sh "$ROOT/vmm/guest/agent/bake-agent.sh" "$ROOTFS" "$ROOT/vmm/guest/agent/vmm-agent" >/dev/null 2>&1 || true
e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true

for p in $(pgrep -f 'release/taritd' 2>/dev/null); do kill "$p" 2>/dev/null; done; sleep 1
TARIT_API_KEY=$KEY TARIT_LISTEN=127.0.0.1:$PORT TARIT_RPC_ADDR=http://127.0.0.1:$PORT \
TARIT_VMM_BIN=$VMM TARIT_KERNEL=/tmp/vmlinux.microvm TARIT_ROOTFS=$ROOTFS TARIT_ROOTFS_READONLY=1 \
TARIT_SOCKET_DIR=$DIR/sockets TARIT_DB=$DIR/db.sqlite TARIT_CONFIG=$DIR/none.toml \
TARIT_WARM_POOL=0 TARIT_SSH_GATEWAY=1 TARIT_SSH_GATEWAY_ADDR=127.0.0.1:2299 \
TARIT_SSH_GATEWAY_HOST_KEY=$DIR/hostkey RUST_LOG=taritd=info \
"$TARITD" serve >"$DIR/log" 2>&1 &
PID=$!
cleanup(){ kill "$PID" 2>/dev/null; sleep 1; kill -9 "$PID" 2>/dev/null
  for p in $(pgrep -f 'vmm serve' 2>/dev/null); do kill "$p" 2>/dev/null; done
  rm -rf "$DIR" "$ROOTFS" /tmp/tarit_smoke_key* ; }
trap cleanup EXIT
api(){ curl -sS --max-time 30 -H "X-API-Key: $KEY" "$@"; }
for _ in $(seq 1 30); do curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break; sleep 1; done

# 1) create + exec: proves taritd<->vmm wire compat via tarit-proto
VM=$(api -H 'content-type: application/json' -d '{"vcpus":1,"memory_mib":256}' "http://127.0.0.1:$PORT/v1/vms" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
note "created vm=$VM"
OUT=$(api -H 'content-type: application/json' -d "{\"vm_id\":\"$VM\",\"command\":\"echo proto-wire-ok\",\"timeout_ms\":5000}" "http://127.0.0.1:$PORT/v1/execute")
echo "$OUT" | grep -q 'proto-wire-ok' && note "exec ok: wire compat confirmed" || fail "exec did not return expected output: $OUT"

# 2) PTY via the SSH gateway: proves the shared raw framing works end to end
ssh-keygen -t ed25519 -N '' -f /tmp/tarit_smoke_key -q
api -X POST -H 'content-type: application/json' -d "{\"public_key\":\"$(cat /tmp/tarit_smoke_key.pub)\"}" "http://127.0.0.1:$PORT/v1/ssh-keys" >/dev/null
PTYOUT=$(printf 'echo pty-frame-ok\nexit\n' | timeout 30 ssh -tt -i /tmp/tarit_smoke_key -p 2299 \
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=10 \
  "$VM"@127.0.0.1 2>/dev/null | tr -d '\r')
echo "$PTYOUT" | grep -q 'pty-frame-ok' && note "pty ok: shared framing works" || fail "pty did not echo expected: [$PTYOUT]"

api -X DELETE "http://127.0.0.1:$PORT/v1/vms/$VM" >/dev/null
echo ""
[ "$PASS" = 1 ] && { echo "RESULT: TARIT_SMOKE_PASS"; exit 0; } || { echo "RESULT: TARIT_SMOKE_FAIL"; echo "--- log tail ---"; tail -20 "$DIR/log"; exit 1; }
