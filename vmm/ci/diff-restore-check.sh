#!/usr/bin/env bash
# ci/diff-restore-check.sh — focused test of diff (incremental) snapshot restore
# memory consistency. Writes random data before a full snapshot and more between
# the full and a diff snapshot, restores from the DIFF tip, and checks both
# regions' SHA256 survive. Fast reproducer for diff-chain-tip restore. Root/KVM.
set -uo pipefail

REPO_VMM="$(cd "$(dirname "$0")/.." && pwd)"
VMM="${VMM:-$REPO_VMM/target/debug/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
AGENT="${AGENT:-$REPO_VMM/guest/agent/vmm-agent}"
BAKE="$REPO_VMM/guest/agent/bake-agent.sh"
ROOTFS=/tmp/diffchk-rootfs.ext4
S1=/tmp/vmm-diff1.sock
S2=/tmp/vmm-diff2.sock
rm -f "$S1" "$S2"

make -C "$REPO_VMM/guest/agent" >/dev/null 2>&1 || true
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

# Mount a tmpfs in the guest explicitly: the payload must live in guest RAM (a
# write to the disk image would not prove anything about memory consistency),
# and this image does not reliably provide one at a fixed path.
guest_ram_setup() {  # $1 = socket
  local n i
  for i in $(seq 1 5); do
    n=$(guest "mkdir -p /run/ramcheck && mount -t tmpfs -o size=256m tmpfs /run/ramcheck 2>/dev/null; grep -c ' /run/ramcheck tmpfs ' /proc/mounts" "$1")
    [ "$n" = "1" ] && return 0
    sleep 2
  done
  echo "FAIL: could not mount the guest tmpfs at /run/ramcheck"; exit 1
}

# Snapshot files are scratch owned by the serve process that made them and are
# deleted when it exits. `vmm snapshot` transfers that ownership to the caller,
# so the artifact survives at its original path — which matters for diff
# snapshots, whose header records the absolute path of their parent.
take_snapshot() {  # $1 = socket, $2 = "--diff" or ""
  "$VMM" --socket "$1" snapshot ${2:-} \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["path"])'
}

# Terminate a serve process and wait for it, so a failed run cannot leave a VM
# holding host RAM and starve the checks that follow it in the gate.
stop_serve() {  # $1 = pid
  [ -n "${1:-}" ] || return 0
  kill "$1" 2>/dev/null || return 0
  for _ in $(seq 1 20); do kill -0 "$1" 2>/dev/null || return 0; sleep 0.5; done
  kill -9 "$1" 2>/dev/null || true
  wait "$1" 2>/dev/null || true
}

trap 'stop_serve "${P1:-}"; stop_serve "${P2:-}"' EXIT

# Poll the guest agent with a short timeout until it answers. Long-timeout
# retries make a guest that never boots stall the gate for minutes.
wait_guest_ready() {  # $1 = socket
  local j r i
  j=$(python3 -c 'import json;print(json.dumps({"op":"exec","command":"echo GUEST_READY","timeout_ms":3000}))')
  for i in $(seq 1 60); do
    r=$(api "$1" "$j" 2>/dev/null)
    case "$r" in *GUEST_READY*) return 0;; esac
    sleep 2
  done
  echo "FAIL: guest never became command-ready"; exit 1
}

CMD="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
RUST_LOG=warn "$VMM" serve --socket "$S1" >/tmp/diff1.log 2>&1 & P1=$!
sleep 1
api "$S1" "{\"op\":\"create\",\"config\":{\"kernel\":{\"path\":\"$KERNEL\",\"cmdline\":\"$CMD\",\"initramfs\":null},\"memory\":{\"size_mib\":512},\"vcpus\":{\"count\":1},\"volumes\":[{\"path\":\"$ROOTFS\",\"read_only\":false}],\"net\":[]}}" >/dev/null
echo "  (waiting for guest)"; wait_guest_ready "$S1"

# region F: written BEFORE the full snapshot
guest_ram_setup "$S1"
guest 'dd if=/dev/urandom of=/run/ramcheck/F bs=1M count=48 && sync' "$S1" >/dev/null
SHA_F=$(guest 'sha256sum /run/ramcheck/F | cut -d" " -f1' "$S1")
SNAP_FULL=$(take_snapshot "$S1")
[ -n "$SNAP_FULL" ] || { echo "FAIL: full snapshot returned no path"; exit 1; }
echo "full snapshot: $SNAP_FULL  SHA_F=$SHA_F"

# region G: written AFTER the full, BEFORE the diff (must be captured by the diff)
guest 'dd if=/dev/urandom of=/run/ramcheck/G bs=1M count=32 && sync' "$S1" >/dev/null
SHA_G=$(guest 'sha256sum /run/ramcheck/G | cut -d" " -f1' "$S1")
SNAP_DIFF=$(take_snapshot "$S1" --diff)
[ -n "$SNAP_DIFF" ] || { echo "FAIL: diff snapshot returned no path"; exit 1; }
echo "diff snapshot: $SNAP_DIFF  SHA_G=$SHA_G"

api "$S1" '{"op":"stop"}' >/dev/null; stop_serve "$P1"; sleep 1

echo "=== restore from DIFF tip into a fresh serve ==="
RUST_LOG=warn "$VMM" serve --socket "$S2" >/tmp/diff2.log 2>&1 & P2=$!
sleep 1
api "$S2" "{\"op\":\"restore\",\"snapshot_path\":\"$SNAP_DIFF\"}"; echo
wait_guest_ready "$S2"
POST=$(guest 'echo ALIVE; sha256sum /run/ramcheck/F /run/ramcheck/G 2>&1' "$S2")
echo "post-restore: $POST"
RF=$(echo "$POST" | awk '/F$/{print $1}')
RG=$(echo "$POST" | awk '/G$/{print $1}')

api "$S2" '{"op":"stop"}' >/dev/null; stop_serve "$P2"; sleep 1

echo ""
echo "=== verdict ==="
PASS=1
echo "$POST" | grep -q ALIVE || { echo "FAIL: guest not alive after diff restore"; PASS=0; }
[ "$RF" = "$SHA_F" ] || { echo "FAIL: region F changed ($SHA_F -> $RF)"; PASS=0; }
[ "$RG" = "$SHA_G" ] || { echo "FAIL: region G (diff-captured) changed ($SHA_G -> $RG)"; PASS=0; }
rm -f "$SNAP_FULL" "$SNAP_DIFF"
if [ "$PASS" = 1 ]; then echo "RESULT: DIFF_RESTORE_PASS"; exit 0; else echo "RESULT: DIFF_RESTORE_FAIL"; exit 1; fi
