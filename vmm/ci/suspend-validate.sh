#!/usr/bin/env bash
# ci/suspend-validate.sh — validate that Suspend frees host RAM and Resume is
# memory-consistent. Boots a VM, writes random data into guest RAM (tmpfs) and
# records its SHA256, suspends (RSS must drop), resumes, and re-checks the SHA256
# (must match) plus a live exec. Run as root (needs /dev/kvm). c8i box only.
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/debug/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
AGENT=$HOME/tarit/vmm/guest/agent/vmm-agent
BAKE=$HOME/tarit/vmm/guest/agent/bake-agent.sh
ROOTFS=/tmp/suspend-rootfs.ext4
SOCK=/tmp/vmm-suspend.sock
LOG=/tmp/vmm-suspend.log
rm -f "$SOCK" "$LOG"

make -C $HOME/tarit/vmm/guest/agent >/dev/null 2>&1 || true
[ -x "$AGENT" ] || { echo "FAIL: no vmm-agent"; exit 1; }
cp -f /tmp/vsock-rootfs.ext4 "$ROOTFS"; e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$BAKE" "$ROOTFS" "$AGENT" >/dev/null

api() {  # $1 = json body -> raw response
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
guest() {  # $1 = shell command -> guest stdout (trimmed)
  local j
  j=$(python3 -c 'import json,sys; print(json.dumps({"op":"exec","command":sys.argv[1],"timeout_ms":30000}))' "$1")
  api "$j" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("stdout","").strip())'
}
rss_kb() { awk '/VmRSS/{print $2}' "/proc/$1/status" 2>/dev/null; }

CMD="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
RUST_LOG=warn "$VMM" serve --socket "$SOCK" >"$LOG" 2>&1 & SP=$!
sleep 1
api "{\"op\":\"create\",\"config\":{\"kernel\":{\"path\":\"$KERNEL\",\"cmdline\":\"$CMD\",\"initramfs\":null},\"memory\":{\"size_mib\":512},\"vcpus\":{\"count\":1},\"volumes\":[{\"path\":\"$ROOTFS\",\"read_only\":false}],\"net\":[]}}" >/dev/null
echo "  (25s boot)"; sleep 25
echo "pre: $(guest 'echo PRE_SUSPEND')"

# Write 96 MiB of random data into guest RAM (tmpfs) and hash it.
guest 'dd if=/dev/urandom of=/dev/shm/fill bs=1M count=96 2>/dev/null; sync' >/dev/null
SHA_BEFORE=$(guest 'sha256sum /dev/shm/fill | cut -d" " -f1')
SIZE_BEFORE=$(guest 'wc -c < /dev/shm/fill')
echo "pre-suspend  sha=$SHA_BEFORE size=$SIZE_BEFORE"
RSS_BEFORE=$(rss_kb "$SP")
echo "RSS before suspend: ${RSS_BEFORE} kB"

echo "=== suspend ==="; api '{"op":"suspend"}'; echo
sleep 2
RSS_AFTER=$(rss_kb "$SP")
echo "RSS after suspend:  ${RSS_AFTER} kB"

echo "=== resume ==="; api '{"op":"resume"}'; echo
sleep 2
POST=$(guest 'echo POST_RESUME; uname -n')
SHA_AFTER=$(guest 'sha256sum /dev/shm/fill | cut -d" " -f1')
echo "post-resume exec: $POST"
echo "post-resume  sha=$SHA_AFTER"

api '{"op":"stop"}' >/dev/null; kill "$SP" 2>/dev/null || true; sleep 1

echo ""
echo "=== verdict ==="
PASS=1
echo "$POST" | grep -q POST_RESUME || { echo "FAIL: exec did not work after resume"; PASS=0; }
[ -n "$SHA_BEFORE" ] && [ "$SHA_BEFORE" = "$SHA_AFTER" ] || { echo "FAIL: guest RAM changed across suspend/resume ($SHA_BEFORE != $SHA_AFTER)"; PASS=0; }
if [ -n "$RSS_BEFORE" ] && [ -n "$RSS_AFTER" ]; then
  DROP=$(( RSS_BEFORE - RSS_AFTER ))
  echo "RSS drop: ${DROP} kB (before=${RSS_BEFORE} after=${RSS_AFTER})"
  [ "$DROP" -gt 51200 ] || { echo "FAIL: RSS did not drop by >50MB on suspend"; PASS=0; }
else
  echo "FAIL: could not read RSS"; PASS=0
fi
if [ "$PASS" = 1 ]; then echo "RESULT: PASS (RAM freed on suspend; SHA256 consistent after resume)"; exit 0; else echo "RESULT: FAIL"; exit 1; fi
