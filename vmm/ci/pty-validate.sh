#!/usr/bin/env bash
# ci/pty-validate.sh — validate the interactive vsock PTY channel on KVM (c8i).
#
# Bakes the freshly built vmm-agent (with the PTY listener on vsock port 1025)
# into a private rootfs copy, boots it under `vmm serve`, then drives the
# AttachPty STREAM protocol: opens a PTY, resizes it, runs a command, and checks
# for faithful output + a clean exit. Run as root (needs /dev/kvm + loop mount).
#
#   sudo bash $HOME/tarit/vmm/ci/pty-validate.sh
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/debug/vmm}"
AGENT="${AGENT:-$HOME/tarit/vmm/guest/agent/vmm-agent}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
BASE_ROOTFS="${BASE_ROOTFS:-/tmp/vsock-rootfs.ext4}"
ROOTFS=/tmp/pty-rootfs.ext4
SOCK=/tmp/vmm-pty-val.sock
LOG=/tmp/vmm-pty-val.log
BAKE=$HOME/tarit/vmm/guest/agent/bake-agent.sh
rm -f "$SOCK" "$LOG"

echo "=== prepare private rootfs copy with the new PTY agent ==="
cp -f "$BASE_ROOTFS" "$ROOTFS"
e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$BAKE" "$ROOTFS" "$AGENT"

api() {
  python3 - "$SOCK" "$1" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(40)
try:
    s.connect(sys.argv[1]); b = sys.argv[2].encode()
    s.sendall(struct.pack('>I', len(b)) + b)
    rl = struct.unpack('>I', s.recv(4))[0]; d = b''
    while len(d) < rl:
        c = s.recv(rl - len(d))
        if not c: break
        d += c
    sys.stdout.write(d.decode())
except Exception as e:
    print('{"error":"%s"}' % e)
finally:
    s.close()
PY
}

RUST_LOG=info "$VMM" serve --socket "$SOCK" >"$LOG" 2>&1 &
SERVE_PID=$!
sleep 1

CMDLINE="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
echo "=== create (PTY agent rootfs) ==="
api '{"op":"create","config":{"kernel":{"path":"'"$KERNEL"'","cmdline":"'"$CMDLINE"'","initramfs":null},"memory":{"size_mib":512},"vcpus":{"count":1},"volumes":[{"path":"'"$ROOTFS"'","read_only":false}],"net":[]}}'
echo ""
echo "  (waiting 25s for systemd + vmm-agent PTY listener)"
sleep 25

echo ""
echo "=== drive AttachPty STREAM protocol ==="
PTY_RESULT=$(python3 - "$SOCK" <<'PY'
import socket, struct, sys, json, time

sock_path = sys.argv[1]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(30)
s.connect(sock_path)

# 1) AttachPty request frame (normal length-prefixed JSON), then STREAM mode.
req = json.dumps({"op": "attach_pty", "cols": 80, "rows": 24, "shell": "/bin/sh"}).encode()
s.sendall(struct.pack('>I', len(req)) + req)

DATA, RESIZE, EXIT, ERROR = 0, 1, 2, 3

def send(t, payload=b''):
    s.sendall(struct.pack('>BI', t, len(payload)) + payload)

def recv_exact(n):
    d = b''
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c:
            return None
        d += c
    return d

# 2) Resize to 40x120, then run a couple of commands and exit.
send(RESIZE, json.dumps({"cols": 120, "rows": 40}).encode())
time.sleep(0.3)
send(DATA, b"stty size; echo PTY_OK_MARKER_7788; id -u; exit\n")

out = b''
exit_code = None
err = None
deadline = time.time() + 25
while time.time() < deadline:
    try:
        hdr = recv_exact(5)
    except socket.timeout:
        break
    if not hdr:
        break
    t, ln = struct.unpack('>BI', hdr)
    payload = recv_exact(ln) if ln else b''
    if payload is None:
        break
    if t == DATA:
        out += payload
    elif t == EXIT:
        try:
            exit_code = json.loads(payload).get("exit_code")
        except Exception:
            exit_code = "?"
        break
    elif t == ERROR:
        err = payload.decode(errors="replace")
        break

s.close()
text = out.decode(errors="replace")
sys.stdout.write(text)
sys.stdout.write("\n---\n")
ok = ("PTY_OK_MARKER_7788" in text) and ("40 120" in text) and (exit_code == 0) and (err is None)
sys.stdout.write("exit_code=%s err=%s winsize_ok=%s marker_ok=%s\n" % (
    exit_code, err, "40 120" in text, "PTY_OK_MARKER_7788" in text))
sys.stdout.write("PTY_VALIDATION_PASS\n" if ok else "PTY_VALIDATION_FAIL\n")
PY
)
echo "$PTY_RESULT"

echo ""
echo "=== stop ==="
api '{"op":"stop"}'
sleep 1
kill "$SERVE_PID" 2>/dev/null || true
sleep 1

echo ""
echo "=== agent PTY evidence (serial log) ==="
grep -iE "vmm-agent|pty|Started|multi-user" "$LOG" | tail -12

echo ""
if echo "$PTY_RESULT" | grep -q PTY_VALIDATION_PASS; then
  echo "RESULT: PASS"
  exit 0
else
  echo "RESULT: FAIL"
  exit 1
fi
