#!/usr/bin/env bash
# ci/exec-validate.sh — validate real exec in a running guest via the guest agent.
# Boots the agent-baked rootfs, waits for the agent, then execs commands through
# the API and checks for faithful stdout + exit codes.
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${ROOTFS:-/tmp/agent-rootfs.ext4}"
SOCK=/tmp/vmm-exec.sock
LOG=/tmp/vmm-exec-server.log
rm -f "$SOCK" "$LOG"

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
echo "=== create (agent rootfs) ==="
api '{"op":"create","config":{"kernel":{"path":"'"$KERNEL"'","cmdline":"'"$CMDLINE"'","initramfs":null},"memory":{"size_mib":512},"vcpus":{"count":1},"volumes":[{"path":"'"$ROOTFS"'","read_only":false}],"net":[]}}'
echo ""
echo "  (waiting 25s for systemd + vmm-agent to start)"
sleep 25

for cmd in "uname -a" "echo HELLO_FROM_GUEST" "id -u" "sh -c 'exit 7'" "cat /etc/os-release"; do
  echo ""
  echo "=== exec: $cmd ==="
  api '{"op":"exec","command":"'"$cmd"'","timeout_ms":20000}'
  echo ""
done

echo ""
echo "=== stop ==="
api '{"op":"stop"}'
sleep 1
kill "$SERVE_PID" 2>/dev/null || true
sleep 1

echo ""
echo "=== did the agent service start? (serial log grep) ==="
grep -iE "vmm-agent|VMM_EXEC|Reached target|multi-user|systemd\[1\]: Started" "$LOG" | tail -15
