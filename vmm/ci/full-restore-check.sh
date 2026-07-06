#!/usr/bin/env bash
# ci/full-restore-check.sh — full snapshot -> restore-into-fresh-process memory
# consistency. Writes random data, full-snapshots, stops, restores into a fresh
# serve, and checks the data's SHA256 survives. Root/KVM. c8i only.
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/debug/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
AGENT=$HOME/tarit/vmm/guest/agent/vmm-agent
BAKE=$HOME/tarit/vmm/guest/agent/bake-agent.sh
ROOTFS=/tmp/fullchk-rootfs.ext4
S1=/tmp/vmm-full1.sock
S2=/tmp/vmm-full2.sock
rm -f "$S1" "$S2"

make -C $HOME/tarit/vmm/guest/agent >/dev/null 2>&1 || true
cp -f /tmp/vsock-rootfs.ext4 "$ROOTFS"; e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$BAKE" "$ROOTFS" "$AGENT" >/dev/null

api() { python3 - "$1" "$2" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(90)
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
guest() { local j; j=$(python3 -c 'import json,sys;print(json.dumps({"op":"exec","command":sys.argv[1],"timeout_ms":40000}))' "$1"); api "$2" "$j" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("stdout","").strip())'; }

CMD="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
RUST_LOG=warn "$VMM" serve --socket "$S1" >/tmp/full1.log 2>&1 & P1=$!
sleep 1
api "$S1" "{\"op\":\"create\",\"config\":{\"kernel\":{\"path\":\"$KERNEL\",\"cmdline\":\"$CMD\",\"initramfs\":null},\"memory\":{\"size_mib\":512},\"vcpus\":{\"count\":1},\"volumes\":[{\"path\":\"$ROOTFS\",\"read_only\":false}],\"net\":[]}}" >/dev/null
echo "  (25s boot)"; sleep 25

guest 'dd if=/dev/urandom of=/dev/shm/M bs=1M count=64 2>/dev/null; sync' "$S1" >/dev/null
SHA=$(guest 'sha256sum /dev/shm/M | cut -d" " -f1' "$S1")
SNAP=$(api "$S1" '{"op":"snapshot","diff":false}' | python3 -c 'import sys,json;print(json.load(sys.stdin).get("path",""))')
echo "full snapshot: $SNAP  SHA=$SHA"
api "$S1" '{"op":"stop"}' >/dev/null; kill "$P1" 2>/dev/null || true; sleep 1

echo "=== restore into a fresh serve ==="
RUST_LOG=warn "$VMM" serve --socket "$S2" >/tmp/full2.log 2>&1 & P2=$!
sleep 1
api "$S2" "{\"op\":\"restore\",\"snapshot_path\":\"$SNAP\"}"; echo
sleep 5
POST=$(guest 'echo ALIVE; sha256sum /dev/shm/M 2>&1' "$S2")
echo "post-restore: $POST"
RM=$(echo "$POST" | awk '/\/dev\/shm\/M/{print $1}')
api "$S2" '{"op":"stop"}' >/dev/null; kill "$P2" 2>/dev/null || true; sleep 1

echo ""
echo "=== verdict ==="
PASS=1
echo "$POST" | grep -q ALIVE || { echo "FAIL: guest not alive after restore"; PASS=0; }
[ "$RM" = "$SHA" ] || { echo "FAIL: RAM changed across full snapshot/restore ($SHA -> $RM)"; PASS=0; }
if [ "$PASS" = 1 ]; then echo "RESULT: FULL_RESTORE_PASS"; exit 0; else echo "RESULT: FULL_RESTORE_FAIL"; exit 1; fi
