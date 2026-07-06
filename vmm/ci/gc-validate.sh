#!/usr/bin/env bash
# ci/gc-validate.sh — validate snapshot/overlay GC on KVM: VMM scratch is cleaned
# on stop while an explicit snapshot output is preserved. Run as root. c8i only.
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/debug/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
AGENT=$HOME/tarit/vmm/guest/agent/vmm-agent
BAKE=$HOME/tarit/vmm/guest/agent/bake-agent.sh
ROOTFS=/tmp/gc-rootfs.ext4
SOCK=/tmp/vmm-gc.sock
LOG=/tmp/vmm-gc.log
rm -f "$SOCK" "$LOG" /tmp/vmm-live.snap /tmp/.vmm-suspend-*.snap

make -C $HOME/tarit/vmm/guest/agent >/dev/null 2>&1 || true
cp -f /tmp/vsock-rootfs.ext4 "$ROOTFS"; e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$BAKE" "$ROOTFS" "$AGENT" >/dev/null

api() {
  python3 - "$SOCK" "$1" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(60)
s.connect(sys.argv[1]); b = sys.argv[2].encode()
s.sendall(struct.pack('>I', len(b)) + b)
rl = struct.unpack('>I', s.recv(4))[0]; d = b''
while len(d) < rl:
    c = s.recv(rl - len(d))
    if not c: break
    d += c
sys.stdout.write(d.decode())
PY
}

CMD="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
RUST_LOG=warn "$VMM" serve --socket "$SOCK" >"$LOG" 2>&1 & SP=$!
sleep 1
api "{\"op\":\"create\",\"config\":{\"kernel\":{\"path\":\"$KERNEL\",\"cmdline\":\"$CMD\",\"initramfs\":null},\"memory\":{\"size_mib\":512},\"vcpus\":{\"count\":1},\"volumes\":[{\"path\":\"$ROOTFS\",\"read_only\":false}],\"net\":[]}}" >/dev/null
echo "  (25s boot)"; sleep 25

SNAP=$(api '{"op":"snapshot","diff":false}' | python3 -c 'import sys,json;print(json.load(sys.stdin).get("path",""))')
echo "user snapshot: $SNAP"; ls -la "$SNAP" 2>&1
api '{"op":"suspend"}' >/dev/null; sleep 1
echo "suspend scratch present:"; ls -la /tmp/.vmm-suspend-*.snap 2>&1 | head
echo "=== stop ==="; api '{"op":"stop"}' >/dev/null; kill "$SP" 2>/dev/null || true; sleep 1

echo ""
echo "=== after stop ==="
SNAP_OK=0; [ -f "$SNAP" ] && SNAP_OK=1
SUSP_LEFT=$(ls /tmp/.vmm-suspend-*.snap 2>/dev/null | wc -l)
LIVE_LEFT=0; [ -f /tmp/vmm-live.snap ] && LIVE_LEFT=1
echo "user snapshot preserved: $SNAP_OK (path=$SNAP)"
echo "suspend scratch remaining: $SUSP_LEFT (want 0)"
echo "live snap remaining: $LIVE_LEFT (want 0)"

echo ""
echo "=== gc sweep (orphans) ==="
"$VMM" gc --dir /tmp --max-age 0 2>&1 | tail -5 || true
# the user snapshot lives at /tmp/vmm-<pid>-<ts>.snap; gc scratch must NOT delete it
[ -f "$SNAP" ] && SNAP_OK2=1 || SNAP_OK2=0
echo "user snapshot still present after gc: $SNAP_OK2"
rm -f "$SNAP"

echo ""
if [ "$SNAP_OK" = 1 ] && [ "$SUSP_LEFT" = 0 ] && [ "$LIVE_LEFT" = 0 ] && [ "$SNAP_OK2" = 1 ]; then
  echo "RESULT: GC_PASS"; exit 0
else
  echo "RESULT: GC_FAIL"; exit 1
fi
