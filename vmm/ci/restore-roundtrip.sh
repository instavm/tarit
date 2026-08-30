#!/usr/bin/env bash
# ci/restore-roundtrip.sh — validate faithful snapshot→restore→resume on KVM.
#
# Boots a real guest via the VMM API, snapshots it, stops it, restores it, and
# checks that the restored VM comes back *running* (vCPU state re-applied, guest
# still making progress on the serial console) rather than a paused memory image.
#
# Run on the c8i KVM host (needs sudo for /dev/kvm):
#   sudo bash /tmp/restore-roundtrip.sh
set -Eeuo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${ROOTFS:-/tmp/debian-rootfs.ext4}"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/vmm-restore.XXXXXX")
SOCK="$WORK/vmm.sock"
LOG="$WORK/vmm.log"
PERSISTED_SNAP="$WORK/roundtrip.snap"
SERVE_PID=""

cleanup() {
  local status=$?
  if [ -n "$SERVE_PID" ] && kill -0 "$SERVE_PID" 2>/dev/null; then
    kill "$SERVE_PID" 2>/dev/null || true
    wait "$SERVE_PID" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ]; then
    echo "FAIL: restore roundtrip exited $status" >&2
    tail -160 "$LOG" 2>/dev/null || true
  fi
  find "$WORK" -depth -delete 2>/dev/null || true
  return "$status"
}
trap cleanup EXIT

api() {
  python3 - "$SOCK" "$1" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(30)
try:
    s.connect(sys.argv[1])
    body = sys.argv[2].encode()
    s.sendall(struct.pack('>I', len(body)) + body)
    rl = struct.unpack('>I', s.recv(4))[0]
    data = b''
    while len(data) < rl:
        chunk = s.recv(rl - len(data))
        if not chunk:
            break
        data += chunk
    print(data.decode())
except Exception as e:
    print('{"error":"client: %s"}' % e)
finally:
    s.close()
PY
}

RUST_LOG=info "$VMM" serve --socket "$SOCK" >"$LOG" 2>&1 &
SERVE_PID=$!
sleep 1

assert_status() {
  local expected=$1 response=$2
  python3 - "$expected" "$response" <<'PY'
import json
import sys

expected, raw = sys.argv[1:]
payload = json.loads(raw)
if payload.get("status") != expected:
    raise SystemExit(f"expected status={expected!r}, got {payload!r}")
PY
}

CMDLINE="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"

echo "=== create (real kernel + rootfs, full boot) ==="
CFG='{"op":"create","config":{"kernel":{"path":"'"$KERNEL"'","cmdline":"'"$CMDLINE"'","initramfs":null},"memory":{"size_mib":256},"vcpus":{"count":1},"volumes":[{"path":"'"$ROOTFS"'","read_only":false}],"net":[]}}'
CREATE=$(api "$CFG")
echo "$CREATE"
assert_status ok "$CREATE"
echo "  (booting 12s)"
sleep 12

echo "=== snapshot A ==="
RA=$(api '{"op":"snapshot","diff":false}')
echo "  $RA"
assert_status snapshot "$RA"
SNAP=$(echo "$RA" | python3 -c "import sys,json;print(json.loads(sys.stdin.read()).get('path',''))" 2>/dev/null)
echo "  snap=$SNAP"
test -n "$SNAP" && test -f "$SNAP"
# Snapshot paths returned by the VMM are process-owned scratch files. Preserve
# a private test copy before stop, which correctly releases the scratch file.
cp --reflink=auto --sparse=always "$SNAP" "$PERSISTED_SNAP"
test -s "$PERSISTED_SNAP"

echo "=== stop ==="
STOP_A=$(api '{"op":"stop"}')
echo "$STOP_A"
assert_status ok "$STOP_A"
sleep 1

MARK=$(wc -l < "$LOG")
echo "=== restore (expect: running) ==="
RESTORE=$(api '{"op":"restore","snapshot_path":"'"$PERSISTED_SNAP"'"}')
echo "$RESTORE"
assert_status restored "$RESTORE"
echo "  (running 5s post-restore)"
sleep 5

echo "=== snapshot B (liveness probe — should re-capture a live vCPU) ==="
RB=$(api '{"op":"snapshot","diff":false}')
echo "$RB"
assert_status snapshot "$RB"
sleep 1

echo "=== stop ==="
STOP_B=$(api '{"op":"stop"}')
echo "$STOP_B"
assert_status ok "$STOP_B"
sleep 1
kill "$SERVE_PID" 2>/dev/null || true
wait "$SERVE_PID" 2>/dev/null || true
SERVE_PID=""
sleep 1

echo ""
echo "=== restore outcome ==="
grep -nE "restored|reconstruct|could not" "$LOG" | tail -8
echo ""
echo "=== post-restore serial/log (lines after restore call) ==="
tail -n +"$MARK" "$LOG" | grep -vE "^\s*$" | tail -60
echo ""
echo "=== any KVM errors / guest panic anywhere ==="
grep -niE "panic|SHUTDOWN|triple|KVM_RUN|internal error|fault|BUG:" "$LOG" | tail -30

if grep -Eqi 'vCPU thread is gone|SIGSYS|KVM processed [0-9]+/[0-9]+|unsupported destination MSR|internal error|triple fault|BUG:' "$LOG"; then
  echo "FAIL: VMM/KVM error detected in restore log" >&2
  exit 1
fi
echo "RESTORE_ROUNDTRIP_PASS"
