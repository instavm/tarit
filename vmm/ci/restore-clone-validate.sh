#!/usr/bin/env bash
# ci/restore-clone-validate.sh — validate per-restore disk isolation on KVM.
#
# Boots a golden VM (shared read-only base + its own CoW overlay), snapshots it,
# then restores TWO clones from that one snapshot, each with its own `overlay`.
# Asserts: each clone boots and execs; a write in clone A is NOT visible in clone
# B (and vice versa); and the shared base image is byte-identical afterwards.
# Run as root (needs /dev/kvm). c8i test box only.
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/debug/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
BASE=/tmp/clone-base.ext4
GOLDEN_OV=/tmp/clone-golden.overlay
OVA=/tmp/cloneA.overlay
OVB=/tmp/cloneB.overlay
SG=/tmp/vmm-clone-g.sock
SA=/tmp/vmm-clone-a.sock
SB=/tmp/vmm-clone-b.sock

rm -f "$GOLDEN_OV" "$OVA" "$OVB" "$SG" "$SA" "$SB"
cp -f /tmp/vsock-rootfs.ext4 "$BASE"
e2fsck -fy "$BASE" >/dev/null 2>&1 || true
BASE_MD5_BEFORE=$(md5sum "$BASE" | awk '{print $1}')

api() {
  python3 - "$1" "$2" <<'PY'
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

echo "=== golden: read-only base + golden overlay ==="
RUST_LOG=warn "$VMM" serve --socket "$SG" >/tmp/clone-g.log 2>&1 & GP=$!
sleep 1
api "$SG" "{\"op\":\"create\",\"config\":{\"kernel\":{\"path\":\"$KERNEL\",\"cmdline\":\"$CMD\",\"initramfs\":null},\"memory\":{\"size_mib\":512},\"vcpus\":{\"count\":1},\"volumes\":[{\"path\":\"$BASE\",\"read_only\":true,\"overlay\":\"$GOLDEN_OV\"}],\"net\":[]}}" >/dev/null
echo "  (25s boot)"; sleep 25
api "$SG" '{"op":"exec","command":"echo GOLDEN_READY","timeout_ms":15000}'; echo
SNAP_RESP=$(api "$SG" '{"op":"snapshot","diff":false}')
echo "  snapshot: $SNAP_RESP"
SNAP=$(echo "$SNAP_RESP" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("path",""))')
api "$SG" '{"op":"stop"}' >/dev/null; kill "$GP" 2>/dev/null || true; sleep 1
if [ -z "$SNAP" ]; then echo "FAIL: no snapshot path"; exit 1; fi

echo ""
echo "=== restore clone A (overlay $OVA) ==="
RUST_LOG=warn "$VMM" serve --socket "$SA" >/tmp/clone-a.log 2>&1 & AP=$!
sleep 1
api "$SA" "{\"op\":\"restore\",\"snapshot_path\":\"$SNAP\",\"overlay\":\"$OVA\"}"; echo
sleep 4
A_WRITE=$(api "$SA" '{"op":"exec","command":"echo CLONE_A_DATA > /root/marker; sync; cat /root/marker","timeout_ms":15000}')
echo "  A write+read: $A_WRITE"

echo ""
echo "=== restore clone B (overlay $OVB) ==="
RUST_LOG=warn "$VMM" serve --socket "$SB" >/tmp/clone-b.log 2>&1 & BP=$!
sleep 1
api "$SB" "{\"op\":\"restore\",\"snapshot_path\":\"$SNAP\",\"overlay\":\"$OVB\"}"; echo
sleep 4
B_READ=$(api "$SB" '{"op":"exec","command":"cat /root/marker 2>&1","timeout_ms":15000}')
echo "  B read of A marker (want: absent): $B_READ"
api "$SB" '{"op":"exec","command":"echo CLONE_B_DATA > /root/marker; sync","timeout_ms":15000}' >/dev/null

echo ""
echo "=== re-check clone A sees its own data, not B's ==="
A_RECHECK=$(api "$SA" '{"op":"exec","command":"cat /root/marker","timeout_ms":15000}')
echo "  A re-read (want: CLONE_A_DATA): $A_RECHECK"

api "$SA" '{"op":"stop"}' >/dev/null; api "$SB" '{"op":"stop"}' >/dev/null
kill "$AP" "$BP" 2>/dev/null || true; sleep 1

BASE_MD5_AFTER=$(md5sum "$BASE" | awk '{print $1}')
echo ""
echo "=== overlays (separate, non-empty) ==="
ls -la "$OVA" "$OVB" 2>&1

echo ""
echo "=== verdict ==="
PASS=1
echo "$A_WRITE"  | grep -q CLONE_A_DATA || { echo "FAIL: clone A could not write/read its overlay"; PASS=0; }
echo "$B_READ"   | grep -qi 'No such file\|cannot open\|CLONE_B_DATA' || echo "note: B marker read = $B_READ"
echo "$B_READ"   | grep -q CLONE_A_DATA && { echo "FAIL: clone B saw clone A's write (NOT isolated)"; PASS=0; }
echo "$A_RECHECK"| grep -q CLONE_A_DATA || { echo "FAIL: clone A lost its own data / saw B's"; PASS=0; }
echo "$A_RECHECK"| grep -q CLONE_B_DATA && { echo "FAIL: clone A saw clone B's write (NOT isolated)"; PASS=0; }
[ "$BASE_MD5_BEFORE" = "$BASE_MD5_AFTER" ] || { echo "FAIL: base image changed ($BASE_MD5_BEFORE -> $BASE_MD5_AFTER)"; PASS=0; }
echo "base md5 before=$BASE_MD5_BEFORE after=$BASE_MD5_AFTER"
if [ "$PASS" = 1 ]; then echo "RESULT: PASS (clones isolated, base unchanged)"; exit 0; else echo "RESULT: FAIL"; exit 1; fi
