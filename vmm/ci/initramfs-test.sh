#!/usr/bin/env bash
set -uo pipefail
cd $HOME/tarit/vmm/guest/agent && make >/dev/null 2>&1
ROOT=/tmp/initramfs-root
sudo rm -rf "$ROOT"; mkdir -p "$ROOT"/bin "$ROOT"/dev "$ROOT"/proc "$ROOT"/sys
cp $HOME/tarit/vmm/guest/agent/vmm-agent "$ROOT/init"; chmod +x "$ROOT/init"
cp /usr/bin/busybox "$ROOT/bin/busybox"
for a in sh uname echo id cat ls sleep true node; do ln -sf busybox "$ROOT/bin/$a"; done
sudo mknod "$ROOT/dev/console" c 5 1 2>/dev/null || true
sudo mknod "$ROOT/dev/null" c 1 3 2>/dev/null || true
sudo mknod "$ROOT/dev/ttyS0" c 4 64 2>/dev/null || true
( cd "$ROOT" && sudo find . -print0 | sudo cpio --null -o --format=newc 2>/dev/null ) > /tmp/initramfs.cpio
echo "initramfs: $(stat -c %s /tmp/initramfs.cpio) bytes"

VMM=$HOME/tarit/vmm/target/release/vmm
SOCK=/tmp/ir.sock; LOG=/tmp/ir.log; rm -f "$SOCK" "$LOG"
CMDLINE="console=ttyS0 quiet loglevel=0 reboot=k panic=-1 nomodule pci=off i8042.noaux swiotlb=noforce cryptomgr.notests random.trust_cpu=on tsc=reliable no_timer_check nowatchdog nokaslr rdinit=/init"
RUST_LOG=error "$VMM" serve --socket "$SOCK" >"$LOG" 2>&1 &
SP=$!; sleep 0.5
CFG='{"op":"create","config":{"kernel":{"path":"/tmp/vmlinux.microvm","cmdline":"'"$CMDLINE"'","initramfs":"/tmp/initramfs.cpio"},"memory":{"size_mib":256},"vcpus":{"count":1},"volumes":[],"net":[]}}'
T0=$(date +%s%N)
BODY="$CFG" python3 - "$SOCK" >/dev/null <<'PY'
import socket,struct,sys,os
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.settimeout(30);s.connect(sys.argv[1])
b=os.environ["BODY"].encode();s.sendall(struct.pack('>I',len(b))+b);rl=struct.unpack('>I',s.recv(4))[0];s.recv(rl);s.close()
PY
for i in $(seq 1 400); do
  R=$(BODY='{"op":"exec","command":"uname -a","timeout_ms":1200}' python3 - "$SOCK" 2>/dev/null <<'PY'
import socket,struct,sys,os
try:
  s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.settimeout(3);s.connect(sys.argv[1])
  b=os.environ["BODY"].encode();s.sendall(struct.pack('>I',len(b))+b);rl=struct.unpack('>I',s.recv(4))[0];d=b''
  while len(d)<rl:
    c=s.recv(rl-len(d));d+=c if c else b''
    if not c: break
  sys.stdout.write(d.decode())
except Exception: print("ERR")
PY
)
  if echo "$R" | grep -q '"exec"'; then T1=$(date +%s%N); echo "INITRAMFS cold create->first-exec = $(( (T1-T0)/1000000 )) ms"; echo "  exec result: $R"; break; fi
  sleep 0.1
done
BODY='{"op":"stop"}' python3 - "$SOCK" >/dev/null 2>&1 <<'PY'
import socket,struct,sys,os
try:
  s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.settimeout(5);s.connect(sys.argv[1])
  b=os.environ["BODY"].encode();s.sendall(struct.pack('>I',len(b))+b);s.recv(4);s.close()
except: pass
PY
kill $SP 2>/dev/null || true; sleep 1
echo "=== serial tail ==="; tail -4 "$LOG"
