#!/usr/bin/env bash
# ci/full-restore-check.sh — full snapshot -> restore-into-fresh-process memory
# consistency. Writes random data, full-snapshots, stops, restores into a fresh
# serve, and checks the data's SHA256 survives. Root/KVM. c8i only.
set -uo pipefail

REPO_VMM="$(cd "$(dirname "$0")/.." && pwd)"
VMM="${VMM:-$REPO_VMM/target/debug/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
AGENT="${AGENT:-$REPO_VMM/guest/agent/vmm-agent}"
BAKE="$REPO_VMM/guest/agent/bake-agent.sh"
ROOTFS=/tmp/fullchk-rootfs.ext4
S1=/tmp/vmm-full1.sock
S2=/tmp/vmm-full2.sock
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
RUST_LOG=warn "$VMM" serve --socket "$S1" >/tmp/full1.log 2>&1 & P1=$!
sleep 1
api "$S1" "{\"op\":\"create\",\"config\":{\"kernel\":{\"path\":\"$KERNEL\",\"cmdline\":\"$CMD\",\"initramfs\":null},\"memory\":{\"size_mib\":512},\"vcpus\":{\"count\":1},\"volumes\":[{\"path\":\"$ROOTFS\",\"read_only\":false}],\"net\":[]}}" >/dev/null
echo "  (waiting for guest)"; wait_guest_ready "$S1"

guest_ram_setup "$S1"
guest 'dd if=/dev/urandom of=/run/ramcheck/M bs=1M count=64 && sync' "$S1" >/dev/null
SHA=$(guest 'sha256sum /run/ramcheck/M | cut -d" " -f1' "$S1")
SNAP=$(take_snapshot "$S1")
[ -n "$SNAP" ] || { echo "FAIL: snapshot returned no path"; exit 1; }
echo "full snapshot: $SNAP  SHA=$SHA"
api "$S1" '{"op":"stop"}' >/dev/null; stop_serve "$P1"; sleep 1

echo "=== restore into a fresh serve ==="
RUST_LOG=warn "$VMM" serve --socket "$S2" >/tmp/full2.log 2>&1 & P2=$!
sleep 1
api "$S2" "{\"op\":\"restore\",\"snapshot_path\":\"$SNAP\"}"; echo
wait_guest_ready "$S2"
POST=$(guest 'echo ALIVE; sha256sum /run/ramcheck/M 2>&1' "$S2")
echo "post-restore: $POST"
RM=$(echo "$POST" | awk '/M$/{print $1}')
api "$S2" '{"op":"stop"}' >/dev/null; stop_serve "$P2"; sleep 1

echo ""
echo "=== verdict ==="
PASS=1
echo "$POST" | grep -q ALIVE || { echo "FAIL: guest not alive after restore"; PASS=0; }
[ "$RM" = "$SHA" ] || { echo "FAIL: RAM changed across full snapshot/restore ($SHA -> $RM)"; PASS=0; }
rm -f "$SNAP"
if [ "$PASS" = 1 ]; then echo "RESULT: FULL_RESTORE_PASS"; exit 0; else echo "RESULT: FULL_RESTORE_FAIL"; exit 1; fi
