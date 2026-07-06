#!/usr/bin/env bash
# ci/vsock-exec-validate.sh — validate the virtio-vsock exec channel on KVM.
#
# Boots a guest whose agent dials the host over vsock, then hammers exec with
# rapid + multi-line commands. Over the serial console these desync under IRQ
# load; over vsock (a dedicated framed stream) they must all come back exact.
#
# Run on the c8i KVM host (needs sudo for /dev/kvm):
#   sudo bash /tmp/vsock-exec-validate.sh
#
# Env: VMM, KERNEL, ROOTFS (a rootfs baked with the vsock-capable vmm-agent).
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${ROOTFS:-/tmp/vsock-rootfs.ext4}"
SOCK=/tmp/vmm-vsock-val.sock
LOG=/tmp/vmm-vsock-val.log
rm -f "$SOCK" "$LOG"

req() { python3 -c 'import json,sys;print(json.dumps({"op":"exec","command":sys.argv[1],"timeout_ms":8000}))' "$1"; }
api() {
  python3 - "$SOCK" "$1" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(15)
try:
    s.connect(sys.argv[1]); b = sys.argv[2].encode()
    s.sendall(struct.pack('>I', len(b)) + b)
    rl = struct.unpack('>I', s.recv(4))[0]; d = b''
    while len(d) < rl:
        c = s.recv(rl - len(d))
        if not c: break
        d += c
    print(d.decode())
except Exception as e:
    print('ERR', e)
finally:
    s.close()
PY
}
gx() { api "$(req "$1")" | python3 -c 'import sys,json;d=json.load(sys.stdin);print((d.get("stdout") or d.get("msg") or "").strip())'; }

CMDLINE="console=ttyS0 reboot=k panic=1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
RUST_LOG=info "$VMM" serve --socket "$SOCK" >"$LOG" 2>&1 &
SP=$!
sleep 1
api "{\"op\":\"create\",\"config\":{\"kernel\":{\"path\":\"$KERNEL\",\"cmdline\":\"$CMDLINE\",\"initramfs\":null},\"memory\":{\"size_mib\":256},\"vcpus\":{\"count\":1},\"volumes\":[{\"path\":\"$ROOTFS\",\"read_only\":false}],\"net\":[]}}" >/dev/null
echo "  (booting 12s)"
sleep 12

echo "=== pre-snapshot vsock smoke ==="
pre=$(gx "echo pre-snapshot-vsock")
echo "  pre-snapshot exec: $pre"

echo "=== snapshot + restore (guest must re-dial vsock) ==="
RA=$(api '{"op":"snapshot","diff":false}')
echo "  $RA"
SNAP=$(echo "$RA" | python3 -c "import sys,json;print(json.loads(sys.stdin.read()).get('path',''))" 2>/dev/null)
echo "  snap=$SNAP"
MARK=$(wc -l < "$LOG")
api '{"op":"restore","snapshot_path":"'"$SNAP"'"}' >/dev/null
echo "  (running 5s post-restore)"
sleep 5
post=$(gx "echo post-restore-vsock")
echo "  post-restore exec: $post"
post_vsock=$(tail -n +"$MARK" "$LOG" | grep -c "via vsock" || true)
post_connects=$(tail -n +"$MARK" "$LOG" | grep -c "guest agent connected" || true)
echo "  post-restore execs via vsock: $post_vsock"
echo "  post-restore vsock connections: $post_connects"
if [ "$pre" != "pre-snapshot-vsock" ] || [ "$post" != "post-restore-vsock" ] || [ "$post_vsock" -lt 1 ]; then
  echo "  ERROR: restore did not re-establish exec over vsock"
  api "{\"op\":\"stop\"}" >/dev/null
  kill "$SP" 2>/dev/null || true
  exit 1
fi

echo "=== 25 rapid execs (each must echo back exactly) ==="
ok=0; bad=0
for i in $(seq 1 25); do
  out=$(gx "echo id-$i")
  if [ "$out" = "id-$i" ]; then ok=$((ok + 1)); else bad=$((bad + 1)); echo "  MISMATCH #$i: [$out]"; fi
done
echo "  RESULT: $ok/25 correct, $bad wrong"

echo "=== multi-line output (serial desyncs on this) ==="
echo "  $(gx 'uname -s; id -u; echo done' | tr '\n' '|')"

echo "=== channel used ==="
grep -c "via vsock" "$LOG" | sed 's/^/  execs via vsock: /'
grep -c "guest agent connected" "$LOG" | sed 's/^/  vsock connections: /'

api "{\"op\":\"stop\"}" >/dev/null
kill "$SP" 2>/dev/null || true
