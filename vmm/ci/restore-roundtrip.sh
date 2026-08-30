#!/usr/bin/env bash
# ci/restore-roundtrip.sh — validate faithful snapshot→restore→resume on KVM.
#
# Boots a real guest via the VMM API, snapshots it, stops it, restores it, and
# checks that the restored VM comes back *running* (vCPU state re-applied, guest
# still making progress on the serial console) rather than a paused memory image.
#
# Run on the c8i KVM host (needs sudo for /dev/kvm):
#   sudo bash /tmp/restore-roundtrip.sh
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${ROOTFS:-/tmp/debian-rootfs.ext4}"
SOCK=/tmp/vmm-restore.sock
LOG=/tmp/vmm-restore-server.log
rm -f "$SOCK" "$LOG"

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
RA=$(api '{"op":"snapshot","diff":false}')
echo "  $RA"
SNAP=$(echo "$RA" | python3 -c "import sys,json;print(json.loads(sys.stdin.read()).get('path',''))" 2>/dev/null)
echo "  snap=$SNAP"

echo "=== stop ==="
api '{"op":"stop"}'
sleep 1

MARK=$(wc -l < "$LOG")
echo "=== restore (expect: running) ==="
api '{"op":"restore","snapshot_path":"'"$SNAP"'"}'
echo "  (running 5s post-restore)"
sleep 5

echo "=== snapshot B (liveness probe — should re-capture a live vCPU) ==="
api '{"op":"snapshot","diff":false}'
sleep 1

echo "=== stop ==="
api '{"op":"stop"}'
sleep 1
kill "$SERVE_PID" 2>/dev/null || true
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
