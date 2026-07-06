#!/usr/bin/env bash
# ci/smp-snapshot-validate.sh — validate SMP (multi-vCPU) snapshot/restore.
#
# Boots a 2-vCPU guest via the VMM API, confirms the guest sees 2 online CPUs,
# snapshots it, stops it, restores it, and confirms the *restored* VM still has
# 2 online CPUs AND that vCPU1 (the AP) is actually live (accumulates jiffies
# under a taskset-pinned busy loop). A dead/never-resumed AP would leave cpu1
# online-but-frozen, so the jiffies delta is the real phase-B proof.
#
# Run on the c8i KVM host (needs sudo for /dev/kvm):
#   sudo bash /tmp/smp-snapshot-validate.sh
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${ROOTFS:-/tmp/debian-rootfs.ext4}"
SOCK=/tmp/vmm-smp.sock
LOG=/tmp/vmm-smp-server.log
rm -f "$SOCK" "$LOG"

api() {
  python3 - "$SOCK" "$1" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(45)
try:
    s.connect(sys.argv[1])
    body = sys.argv[2].encode()
    s.sendall(struct.pack('>I', len(body)) + body)
    rl = struct.unpack('>I', s.recv(4))[0]
    data = b''
    while len(data) < rl:
        chunk = s.recv(rl - len(data))
        if not chunk:
            break
        data += chunk
    print(data.decode())
except Exception as e:
    print('{"error":"client: %s"}' % e)
finally:
    s.close()
PY
}

# exec a guest command and print just stdout (stripped). Serial-exec can desync
# by one call under SMP, so callers that need a precise value should retry.
gexec() {
  local cmd="$1" tmo="${2:-8000}"
  local req
  req=$(python3 - "$cmd" "$tmo" <<'PY'
import json,sys
print(json.dumps({"op":"exec","command":sys.argv[1],"timeout_ms":int(sys.argv[2])}))
PY
)
  api "$req" | python3 -c "import sys,json
try:
    d=json.loads(sys.stdin.read())
    sys.stdout.write((d.get('stdout') or d.get('msg') or '').strip())
except Exception as e:
    sys.stdout.write('ERR:%s'%e)"
}

# retry a probe until stdout matches a numeric expectation (drains desync)
probe_nproc() {
  local want="$1" i out
  for i in 1 2 3 4 5; do
    out=$(gexec 'nproc' 6000)
    if [ "$out" = "$want" ]; then echo "$out (try $i)"; return 0; fi
    sleep 1
  done
  echo "$out (last, wanted $want)"; return 1
}

RUST_LOG=info "$VMM" serve --socket "$SOCK" >"$LOG" 2>&1 &
SERVE_PID=$!
sleep 1

CMDLINE="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"

echo "=== create (2 vCPUs, real kernel + rootfs, full boot) ==="
CFG='{"op":"create","config":{"kernel":{"path":"'"$KERNEL"'","cmdline":"'"$CMDLINE"'","initramfs":null},"memory":{"size_mib":512},"vcpus":{"count":2},"volumes":[{"path":"'"$ROOTFS"'","read_only":false}],"net":[]}}'
api "$CFG"
echo "  (booting 14s)"
sleep 14

echo "=== pre-snapshot topology ==="
echo -n "  nproc            = "; probe_nproc 2
echo    "  cpu online range = $(gexec 'cat /sys/devices/system/cpu/online')"

echo "=== AP (cpu1) liveness BEFORE snapshot ==="
B0=$(gexec "awk '/^cpu1 /{print \$2+\$4}' /proc/stat"); sleep 0
gexec "taskset -c 1 timeout 2 sh -c 'while :; do :; done'" 6000 >/dev/null
B1=$(gexec "awk '/^cpu1 /{print \$2+\$4}' /proc/stat")
echo "  cpu1 jiffies: $B0 -> $B1"

echo "=== snapshot A ==="
RA=$(api '{"op":"snapshot","diff":false}')
echo "  $RA"
SNAP=$(echo "$RA" | python3 -c "import sys,json;print(json.loads(sys.stdin.read()).get('path',''))" 2>/dev/null)
echo "  snap=$SNAP"

echo "=== stop ==="
api '{"op":"stop"}'
sleep 1

MARK=$(wc -l < "$LOG")
echo "=== restore (expect: running, 2 vCPU threads rebuilt) ==="
api '{"op":"restore","snapshot_path":"'"$SNAP"'"}'
echo "  (running 5s post-restore)"
sleep 5

echo "=== post-restore topology ==="
echo -n "  nproc            = "; probe_nproc 2
echo    "  cpu online range = $(gexec 'cat /sys/devices/system/cpu/online')"

echo "=== AP (cpu1) liveness AFTER restore (the phase-B proof) ==="
A0=$(gexec "awk '/^cpu1 /{print \$2+\$4}' /proc/stat")
gexec "taskset -c 1 timeout 2 sh -c 'while :; do :; done'" 6000 >/dev/null
A1=$(gexec "awk '/^cpu1 /{print \$2+\$4}' /proc/stat")
echo "  cpu1 jiffies: $A0 -> $A1   (delta>0 => AP thread is live after restore)"

echo "=== snapshot B (re-pauses ALL vCPUs incl. the restored AP) ==="
api '{"op":"snapshot","diff":false}'
sleep 1

echo "=== stop ==="
api '{"op":"stop"}'
sleep 1
kill "$SERVE_PID" 2>/dev/null || true
sleep 1

echo ""
echo "=== restore outcome (server log) ==="
grep -nE "restored|reconstruct|could not|AP |vcpu|online" "$LOG" | tail -12
echo ""
echo "=== any KVM errors / guest panic ==="
grep -niE "panic|SHUTDOWN|triple|KVM_RUN|internal error|fault|BUG:" "$LOG" | tail -20
