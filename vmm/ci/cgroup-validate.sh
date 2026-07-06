#!/usr/bin/env bash
# ci/cgroup-validate.sh — validate that `vmm serve` applies cgroup v2 limits to
# the VM and the guest still boots + execs. Run as root on c8i (cgroup v2).
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/debug/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
AGENT=$HOME/tarit/vmm/guest/agent/vmm-agent
BAKE=$HOME/tarit/vmm/guest/agent/bake-agent.sh
ROOTFS=/tmp/cg-rootfs.ext4
SOCK=/tmp/vmm-cg.sock
LOG=/tmp/vmm-cg.log
CG=/sys/fs/cgroup/vmm-test
rm -f "$SOCK" "$LOG"

make -C $HOME/tarit/vmm/guest/agent >/dev/null 2>&1 || true
cp -f /tmp/vsock-rootfs.ext4 "$ROOTFS"; e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true
sh "$BAKE" "$ROOTFS" "$AGENT" >/dev/null

# cgroup v2: enable the controllers we need for children of the root.
echo "+memory +cpu +pids +cpuset" > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || true
rmdir "$CG" 2>/dev/null || true

api() {
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

CMD="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
RUST_LOG=warn "$VMM" serve --socket "$SOCK" \
  --cgroup "$CG" --cgroup-memory-max 700M --cgroup-cpu-max 1000m \
  --cgroup-pids-max 128 --cpuset 0-1 >"$LOG" 2>&1 & SP=$!
sleep 2
if ! kill -0 "$SP" 2>/dev/null; then echo "FAIL: serve exited:"; tail -20 "$LOG"; exit 1; fi

echo "=== cgroup files ==="
MEM=$(cat "$CG/memory.max" 2>&1); echo "memory.max: $MEM (want 734003200)"
echo "cpu.max:    $(cat "$CG/cpu.max" 2>&1)"
echo "pids.max:   $(cat "$CG/pids.max" 2>&1)"
echo "cpuset.cpus:$(cat "$CG/cpuset.cpus" 2>&1)"
echo "serve /proc/$SP/cgroup: $(cat /proc/$SP/cgroup 2>&1)"

api "{\"op\":\"create\",\"config\":{\"kernel\":{\"path\":\"$KERNEL\",\"cmdline\":\"$CMD\",\"initramfs\":null},\"memory\":{\"size_mib\":512},\"vcpus\":{\"count\":1},\"volumes\":[{\"path\":\"$ROOTFS\",\"read_only\":false}],\"net\":[]}}" >/dev/null
echo "  (25s boot)"; sleep 25
EXEC=$(api '{"op":"exec","command":"echo CG_EXEC_OK; nproc","timeout_ms":15000}')
echo "exec under cgroup: $EXEC"
echo "cgroup.procs count: $(wc -l < "$CG/cgroup.procs" 2>/dev/null)"

api '{"op":"stop"}' >/dev/null; kill "$SP" 2>/dev/null || true; sleep 1
rmdir "$CG" 2>/dev/null || true

echo ""
echo "=== verdict ==="
PASS=1
[ "$MEM" = "734003200" ] || { echo "FAIL: memory.max not applied (got $MEM)"; PASS=0; }
echo "$EXEC" | grep -q CG_EXEC_OK || { echo "FAIL: guest exec did not work under cgroup"; PASS=0; }
if [ "$PASS" = 1 ]; then echo "RESULT: CGROUP_PASS (limits applied; guest works under cgroup)"; exit 0; else echo "RESULT: CGROUP_FAIL"; exit 1; fi
