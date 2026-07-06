#!/usr/bin/env bash
# ci/diff-restore-check.sh — focused test of diff (incremental) snapshot restore
# memory consistency. Writes random data before a full snapshot and more between
# the full and a diff snapshot, restores from the DIFF tip, and checks both
# regions' SHA256 survive. Fast reproducer for diff-chain-tip restore. Root/KVM.
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/debug/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
AGENT=$HOME/tarit/vmm/guest/agent/vmm-agent
BAKE=$HOME/tarit/vmm/guest/agent/bake-agent.sh
ROOTFS=/tmp/diffchk-rootfs.ext4
S1=/tmp/vmm-diff1.sock
S2=/tmp/vmm-diff2.sock
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
snap() { api "$1" "{\"op\":\"snapshot\",\"diff\":$2}" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("path",""))'; }

CMD="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
RUST_LOG=warn "$VMM" serve --socket "$S1" >/tmp/diff1.log 2>&1 & P1=$!
sleep 1
api "$S1" "{\"op\":\"create\",\"config\":{\"kernel\":{\"path\":\"$KERNEL\",\"cmdline\":\"$CMD\",\"initramfs\":null},\"memory\":{\"size_mib\":512},\"vcpus\":{\"count\":1},\"volumes\":[{\"path\":\"$ROOTFS\",\"read_only\":false}],\"net\":[]}}" >/dev/null
echo "  (25s boot)"; sleep 25

# region F: written BEFORE the full snapshot
guest 'dd if=/dev/urandom of=/dev/shm/F bs=1M count=48 2>/dev/null; sync' "$S1" >/dev/null
SHA_F=$(guest 'sha256sum /dev/shm/F | cut -d" " -f1' "$S1")
SNAP_FULL=$(snap "$S1" false)
echo "full snapshot: $SNAP_FULL  SHA_F=$SHA_F"

# region G: written AFTER the full, BEFORE the diff (must be captured by the diff)
guest 'dd if=/dev/urandom of=/dev/shm/G bs=1M count=32 2>/dev/null; sync' "$S1" >/dev/null
SHA_G=$(guest 'sha256sum /dev/shm/G | cut -d" " -f1' "$S1")
SNAP_DIFF=$(snap "$S1" true)
echo "diff snapshot: $SNAP_DIFF  SHA_G=$SHA_G"

api "$S1" '{"op":"stop"}' >/dev/null; kill "$P1" 2>/dev/null || true; sleep 1

echo "=== restore from DIFF tip into a fresh serve ==="
RUST_LOG=warn "$VMM" serve --socket "$S2" >/tmp/diff2.log 2>&1 & P2=$!
sleep 1
api "$S2" "{\"op\":\"restore\",\"snapshot_path\":\"$SNAP_DIFF\"}"; echo
sleep 5
POST=$(guest 'echo ALIVE; sha256sum /dev/shm/F /dev/shm/G 2>&1' "$S2")
echo "post-restore: $POST"
RF=$(echo "$POST" | awk '/\/dev\/shm\/F/{print $1}')
RG=$(echo "$POST" | awk '/\/dev\/shm\/G/{print $1}')

api "$S2" '{"op":"stop"}' >/dev/null; kill "$P2" 2>/dev/null || true; sleep 1

echo ""
echo "=== verdict ==="
PASS=1
echo "$POST" | grep -q ALIVE || { echo "FAIL: guest not alive after diff restore"; PASS=0; }
[ "$RF" = "$SHA_F" ] || { echo "FAIL: region F changed ($SHA_F -> $RF)"; PASS=0; }
[ "$RG" = "$SHA_G" ] || { echo "FAIL: region G (diff-captured) changed ($SHA_G -> $RG)"; PASS=0; }
if [ "$PASS" = 1 ]; then echo "RESULT: DIFF_RESTORE_PASS"; exit 0; else echo "RESULT: DIFF_RESTORE_FAIL"; exit 1; fi
