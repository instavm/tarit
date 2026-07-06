#!/usr/bin/env bash
# ci/net-validate.sh — validate the virtio-net data path end to end on KVM.
#
# Proves that a VM created via the API with a `net` device actually moves
# packets: sets up a host TAP + a /24 with NAT to the host's uplink, boots the
# agent rootfs with a virtio-net device, configures the guest interface over the
# exec channel, then checks
#   1. host -> guest ICMP (the guest kernel auto-replies; no guest tools needed;
#      this alone proves virtio-net RX+TX work), and
#   2. guest -> gateway and guest -> internet (best effort; needs guest tools).
#
# Production host networking (tap/netns/NAT) is the orchestrator's job; this
# harness stands that up itself so the VMM's net wiring can be validated alone.
#
# Run on the c8i KVM host (needs sudo for /dev/kvm + iproute/nft):
#   sudo bash $HOME/tarit/vmm/ci/net-validate.sh
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${ROOTFS:-/tmp/agent-rootfs.ext4}"
SOCK=/tmp/vmm-net.sock
LOG=/tmp/vmm-net-server.log

TAP=tapvmm0
HOST_IP=172.16.0.1
GUEST_IP=172.16.0.2
CIDR=24
GUEST_MAC="02:00:00:00:00:02"
UPLINK="$(ip route get 8.8.8.8 2>/dev/null | grep -oE 'dev [^ ]+' | awk '{print $2}')"

echo "== uplink=$UPLINK tap=$TAP host=$HOST_IP guest=$GUEST_IP =="

cleanup() {
  api '{"op":"stop"}' >/dev/null 2>&1 || true
  [ -n "${SP:-}" ] && kill "$SP" 2>/dev/null || true
  ip link del "$TAP" 2>/dev/null || true
  nft delete table ip vmmnat 2>/dev/null || true
  sysctl -q -w net.ipv4.ip_forward=0 2>/dev/null || true
}
trap cleanup EXIT

api() {
  python3 - "$SOCK" "$1" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(45)
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
gexec() { api "{\"op\":\"exec\",\"command\":\"$1\",\"timeout_ms\":15000}"; echo; }

# --- host networking: tap + /24 + NAT to the uplink ---
ip link del "$TAP" 2>/dev/null || true
ip tuntap add dev "$TAP" mode tap
ip addr add "$HOST_IP/$CIDR" dev "$TAP"
ip link set "$TAP" up
sysctl -q -w net.ipv4.ip_forward=1
nft delete table ip vmmnat 2>/dev/null || true
nft add table ip vmmnat
nft add chain ip vmmnat post '{ type nat hook postrouting priority 100 ; }'
nft add rule ip vmmnat post ip saddr 172.16.0.0/24 oif "$UPLINK" masquerade
echo "-- host tap + NAT up --"

# --- boot the VM with a net device ---
rm -f "$SOCK" "$LOG"
RUST_LOG=warn "$VMM" serve --socket "$SOCK" >"$LOG" 2>&1 &
SP=$!
sleep 1
CMDLINE="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
echo "-- create (net) --"
api "{\"op\":\"create\",\"config\":{\"kernel\":{\"path\":\"$KERNEL\",\"cmdline\":\"$CMDLINE\",\"initramfs\":null},\"memory\":{\"size_mib\":512},\"vcpus\":{\"count\":1},\"volumes\":[{\"path\":\"$ROOTFS\",\"read_only\":false}],\"net\":[{\"tap\":\"$TAP\",\"guest_mac\":\"$GUEST_MAC\",\"guest_ip\":\"$GUEST_IP\"}]}}"; echo
echo "  (25s boot)"; sleep 25

# --- configure the guest interface (first non-lo link) over exec ---
echo "-- guest links --"; gexec "ip -o link show | awk -F': ' '{print \$2}'"
IFACE=eth0
echo "-- configure $IFACE --"
gexec "ip link set $IFACE up"
gexec "ip addr add $GUEST_IP/$CIDR dev $IFACE"
gexec "ip route add default via $HOST_IP"
echo "-- guest addr --"; gexec "ip -o -4 addr show $IFACE"

# --- 1. host -> guest ICMP (proves virtio-net RX+TX; no guest tools needed) ---
echo "== host -> guest ping (data-path proof) =="
if ping -c 3 -W 2 "$GUEST_IP"; then
  echo "RESULT host->guest: PASS"
else
  echo "RESULT host->guest: FAIL"
fi

# --- 2. guest -> gateway + internet (best effort) ---
echo "== guest -> gateway/internet (best effort) =="
gexec "ping -c1 -W2 $HOST_IP 2>&1 | tail -2 || echo no-ping-in-guest"
gexec "getent hosts deb.debian.org 2>&1 | head -1 || true"
gexec "(command -v curl >/dev/null && curl -sS -m 10 -o /dev/null -w 'HTTP %{http_code}' http://deb.debian.org/ ) 2>&1 | tail -1 || echo no-curl"

echo "== done =="
