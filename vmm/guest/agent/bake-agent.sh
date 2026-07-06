#!/bin/sh
set -eu

usage() {
    echo "Usage: $0 ROOTFS_EXT4_IMAGE VMM_AGENT_BINARY" >&2
    exit 2
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "error: required command not found: $1" >&2
        exit 1
    }
}

[ "$#" -eq 2 ] || usage
IMAGE=$1
AGENT=$2

[ -f "$IMAGE" ] || { echo "error: rootfs image not found: $IMAGE" >&2; exit 1; }
[ -f "$AGENT" ] || { echo "error: vmm-agent binary not found: $AGENT" >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "error: this script must run as root" >&2; exit 1; }

need_cmd losetup
need_cmd mount
need_cmd umount
need_cmd install
need_cmd ln
need_cmd sync
need_cmd mountpoint

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
MNT="$SCRIPT_DIR/.mount"
LOOP=
MOUNTED=0

cleanup() {
    rc=$?
    if [ "$MOUNTED" -eq 1 ] && mountpoint -q "$MNT"; then
        umount "$MNT" || rc=$?
    fi
    if [ -n "$LOOP" ]; then
        losetup -d "$LOOP" || rc=$?
    fi
    rmdir "$MNT" 2>/dev/null || true
    exit "$rc"
}
trap cleanup EXIT INT TERM HUP

mkdir -p "$MNT"
LOOP=$(losetup --find --show "$IMAGE")
mount -t ext4 "$LOOP" "$MNT"
MOUNTED=1

install -D -m 0755 "$AGENT" "$MNT/usr/sbin/vmm-agent"

UNIT_DIR="$MNT/etc/systemd/system"
UNIT="$UNIT_DIR/vmm-agent.service"
WANTS="$UNIT_DIR/multi-user.target.wants"

mkdir -p "$UNIT_DIR" "$WANTS"
cat > "$UNIT" <<'UNIT_EOF'
[Unit]
Description=VMM serial exec guest agent
Documentation=file:/usr/sbin/vmm-agent
Conflicts=serial-getty@ttyS0.service
After=dev-ttyS0.device

[Service]
Type=simple
ExecStart=/usr/sbin/vmm-agent
Restart=always
RestartSec=1

[Install]
WantedBy=multi-user.target
UNIT_EOF

ln -sfn ../vmm-agent.service "$WANTS/vmm-agent.service"
ln -sfn /dev/null "$UNIT_DIR/serial-getty@ttyS0.service"
rm -f "$UNIT_DIR/getty.target.wants/serial-getty@ttyS0.service"
rm -f "$WANTS/serial-getty@ttyS0.service"

sync
umount "$MNT"
MOUNTED=0
losetup -d "$LOOP"
LOOP=
rmdir "$MNT" 2>/dev/null || true

echo "Installed /usr/sbin/vmm-agent and enabled vmm-agent.service in $IMAGE"
