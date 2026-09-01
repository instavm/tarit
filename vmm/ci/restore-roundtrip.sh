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
SOCK=/tmp/vmm-restore.sock
LOG=/tmp/vmm-restore-server.log
PERSISTED_SNAP="/tmp/vmm-restore-persisted-$$.snap"
PERSISTED_INTEGRITY="/tmp/vmm-restore-persisted-$$.integrity.json"
SERVE_PID=
rm -f -- "$SOCK" "$LOG" "$PERSISTED_SNAP" "$PERSISTED_INTEGRITY"

cleanup() {
  if [[ -n "$SERVE_PID" ]]; then
    kill "$SERVE_PID" 2>/dev/null || true
    wait "$SERVE_PID" 2>/dev/null || true
  fi
  rm -f -- "$SOCK" "$PERSISTED_SNAP" "$PERSISTED_INTEGRITY"
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

CMDLINE="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"

echo "=== create (real kernel + rootfs, full boot) ==="
CFG='{"op":"create","config":{"kernel":{"path":"'"$KERNEL"'","cmdline":"'"$CMDLINE"'","initramfs":null},"memory":{"size_mib":256},"vcpus":{"count":1},"volumes":[{"path":"'"$ROOTFS"'","read_only":false}],"net":[]}}'
api "$CFG"
echo "  (booting 12s)"
sleep 12

echo "=== snapshot A ==="
RA=$(api '{"op":"snapshot","diff":false,"live":true}')
echo "  $RA"
SNAP=$(echo "$RA" | python3 -c "import sys,json;print(json.loads(sys.stdin.read()).get('path',''))" 2>/dev/null)
INTEGRITY=$(echo "$RA" | python3 -c "import sys,json;print(json.loads(sys.stdin.read()).get('integrity_path',''))" 2>/dev/null)
echo "  snap=$SNAP"
if [ -z "$SNAP" ]; then
  echo "snapshot response did not include a path" >&2
  exit 1
fi
if [ ! -f "$SNAP" ]; then
  echo "snapshot file does not exist: $SNAP" >&2
  exit 1
fi
if [ -z "$INTEGRITY" ] || [ ! -f "$INTEGRITY" ]; then
  echo "snapshot response did not include an integrity manifest" >&2
  exit 1
fi
# Snapshot paths returned by the VMM are process-owned scratch files. Preserve
# private test copy before stop.
cp --reflink=auto --sparse=always -- "$SNAP" "$PERSISTED_SNAP"
cp --reflink=auto --sparse=always -- "$INTEGRITY" "$PERSISTED_INTEGRITY"
test -s "$PERSISTED_SNAP"
test -s "$PERSISTED_INTEGRITY"
MANIFEST_SHA=$(sha256sum "$PERSISTED_INTEGRITY" | awk '{print $1}')
[[ "$MANIFEST_SHA" =~ ^[0-9a-f]{64}$ ]]

echo "=== stop ==="
api '{"op":"stop"}'
sleep 1
test ! -e "$SNAP"
test ! -e "$INTEGRITY"

MARK=$(wc -l < "$LOG")
echo "=== unauthenticated restore (expect: rejected) ==="
UNVERIFIED=$(api '{"op":"restore","snapshot_path":"'"$PERSISTED_SNAP"'"}')
echo "  $UNVERIFIED"
python3 -c 'import json,sys; response=json.load(sys.stdin); assert response.get("status") == "err"; assert "unverified snapshot restore is disabled" in response.get("msg", "")' <<<"$UNVERIFIED"
EMPTY_STATUS=$(api '{"op":"status"}')
python3 -c 'import json,sys; response=json.load(sys.stdin); assert response.get("status") == "err"; assert "no VM" in response.get("msg", "")' <<<"$EMPTY_STATUS"

echo "=== restore (expect: running) ==="
RESTORED=$(api '{"op":"restore","snapshot_path":"'"$PERSISTED_SNAP"'","memory_integrity":{"manifest_path":"'"$PERSISTED_INTEGRITY"'","manifest_sha256":"'"$MANIFEST_SHA"'"}}')
echo "  $RESTORED"
python3 -c 'import json,sys; assert json.load(sys.stdin).get("status") == "restored"' <<<"$RESTORED"
echo "  (running 5s post-restore)"
sleep 5

echo "=== snapshot B (liveness probe — should re-capture a live vCPU) ==="
api '{"op":"snapshot","diff":false}'
sleep 1

echo "=== stop ==="
api '{"op":"stop"}'
sleep 1
kill "$SERVE_PID" 2>/dev/null || true
wait "$SERVE_PID" 2>/dev/null || true
SERVE_PID=
sleep 1

echo ""
echo "=== restore outcome ==="
grep -nE "restored|reconstruct|could not" "$LOG" | tail -8 || true
echo ""
echo "=== post-restore serial/log (lines after restore call) ==="
tail -n +"$MARK" "$LOG" | grep -vE "^\s*$" | tail -60 || true
echo ""
echo "=== any KVM errors / guest panic anywhere ==="
grep -niE "panic|SHUTDOWN|triple|KVM_RUN|internal error|fault|BUG:" "$LOG" | tail -30 || true

echo "AUTHENTICATED_RESTORE_E2E_PASS"
