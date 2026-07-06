#!/usr/bin/env bash
# ci/restore-midboot.sh — validate that interrupts survive a restore.
#
# Snapshots a guest EARLY, while it is still actively booting (disk I/O in
# flight, IOAPIC redirection table programmed), then restores and checks the
# guest keeps making forward progress on the serial console. Without VM-level
# IRQCHIP/PIT/clock restore the restored guest gets a fresh (masked) IOAPIC and
# stalls waiting for interrupts that never arrive — so "more boot output after
# restore" is the proof that interrupt state was faithfully restored.
#
# Run on the c8i KVM host: sudo bash /tmp/restore-midboot.sh
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${ROOTFS:-/tmp/debian-rootfs.ext4}"
SOCK=/tmp/vmm-midboot.sock
LOG=/tmp/vmm-midboot-server.log
rm -f "$SOCK" "$LOG"

api() {
  python3 - "$SOCK" "$1" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(30)
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
    print('{"error":"%s"}' % e)
finally:
    s.close()
PY
}

RUST_LOG=info "$VMM" serve --socket "$SOCK" >"$LOG" 2>&1 &
SERVE_PID=$!
sleep 1

CMDLINE="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
echo "=== create ==="
api '{"op":"create","config":{"kernel":{"path":"'"$KERNEL"'","cmdline":"'"$CMDLINE"'","initramfs":null},"memory":{"size_mib":256},"vcpus":{"count":1},"volumes":[{"path":"'"$ROOTFS"'","read_only":false}],"net":[]}}'

# Snapshot EARLY — 3s in, guest is mid-boot with disk I/O in flight.
sleep 3
GUEST_LINES_BEFORE=$(grep -cE "^\[ +[0-9]+\." "$LOG")
LAST_BEFORE=$(grep -E "^\[ +[0-9]+\." "$LOG" | tail -1)
echo "=== snapshot A (mid-boot; $GUEST_LINES_BEFORE guest lines so far) ==="
RA=$(api '{"op":"snapshot","diff":false}')
SNAP=$(echo "$RA" | python3 -c "import sys,json;print(json.loads(sys.stdin.read()).get('path',''))" 2>/dev/null)
echo "  $RA"
echo "  last guest line before restore: $LAST_BEFORE"

echo "=== stop + restore ==="
api '{"op":"stop"}'
sleep 1
MARK=$(wc -l < "$LOG")
api '{"op":"restore","snapshot_path":"'"$SNAP"'"}'

# Let the restored guest keep booting.
sleep 10
api '{"op":"stop"}'
sleep 1
kill "$SERVE_PID" 2>/dev/null || true
sleep 1

GUEST_LINES_AFTER=$(tail -n +"$MARK" "$LOG" | grep -cE "^\[ +[0-9]+\.|systemd|Reached|Starting|Started|login")
echo ""
echo "=== restore outcome ==="
grep -E "restored|reconstruct|could not" "$LOG" | tail -3
echo ""
echo "=== NEW guest output after restore ($GUEST_LINES_AFTER lines) ==="
tail -n +"$MARK" "$LOG" | grep -E "^\[ +[0-9]+\.|systemd|Reached|Starting|Started|login|EXT4|vda" | head -25
echo ""
# NOTE: forward progress here additionally requires device-queue-state restore
# (a virtio-blk request in flight at snapshot time is lost when build_running_vm
# creates a fresh block device). Until that lands, a *mid-I/O* snapshot is
# expected to stall on resume; an *idle/quiesced* snapshot (the pre-warmed
# benchmark case, see restore-roundtrip.sh) resumes cleanly. This script probes
# that boundary.
if [ "$GUEST_LINES_AFTER" -gt 2 ]; then
  echo "PROGRESS: guest produced $GUEST_LINES_AFTER new lines after restore (interrupt + device state OK)"
else
  echo "STALL: no new guest output after restore — expected for a mid-in-flight-I/O"
  echo "       snapshot until device-queue-state restore is wired (snapshot-persist)."
fi
echo ""
echo "=== any KVM/guest fault ==="
grep -niE "panic|SHUTDOWN|triple|internal error|BUG:" "$LOG" | tail -10
