#!/usr/bin/env bash
set -euo pipefail

IMAGE="${1:?usage: install-runtime-tools.sh ROOTFS_EXT4_IMAGE}"

[ -f "$IMAGE" ] || {
  echo "error: rootfs image not found: $IMAGE" >&2
  exit 1
}
[ "$(id -u)" -eq 0 ] || {
  echo "error: this script must run as root" >&2
  exit 1
}

for command in chroot cp losetup mount mountpoint readlink sync umount; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "error: required command not found: $command" >&2
    exit 1
  }
done

MNT="$(mktemp -d)"
LOOP=
MOUNTED=0
RESOLV_STATE=absent
RESOLV_LINK=
RESOLV_BACKUP=/etc/resolv.conf.tarit-backup

restore_resolv() {
  [ "$MOUNTED" -eq 1 ] || return
  [ "$RESOLV_STATE" != restored ] || return
  rm -f "$MNT/etc/resolv.conf"
  case "$RESOLV_STATE" in
    link)
      ln -s "$RESOLV_LINK" "$MNT/etc/resolv.conf"
      ;;
    file)
      mv "$MNT$RESOLV_BACKUP" "$MNT/etc/resolv.conf"
      ;;
  esac
  RESOLV_STATE=restored
}

cleanup() {
  rc=$?
  restore_resolv || rc=$?
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

LOOP="$(losetup --find --show "$IMAGE")"
mount -t ext4 "$LOOP" "$MNT"
MOUNTED=1

if [ -L "$MNT/etc/resolv.conf" ]; then
  RESOLV_STATE="link"
  RESOLV_LINK="$(readlink "$MNT/etc/resolv.conf")"
  rm "$MNT/etc/resolv.conf"
elif [ -f "$MNT/etc/resolv.conf" ]; then
  RESOLV_STATE="file"
  [ ! -e "$MNT$RESOLV_BACKUP" ] || {
    echo "error: rootfs contains reserved path $RESOLV_BACKUP" >&2
    exit 1
  }
  mv "$MNT/etc/resolv.conf" "$MNT$RESOLV_BACKUP"
fi
cp -L /etc/resolv.conf "$MNT/etc/resolv.conf"

echo "== installing guest runtime tools =="
chroot "$MNT" /usr/bin/env DEBIAN_FRONTEND=noninteractive \
  apt-get update -qq
chroot "$MNT" /usr/bin/env DEBIAN_FRONTEND=noninteractive \
  apt-get install -y -qq --no-install-recommends \
    ca-certificates curl iproute2 iputils-ping netcat-openbsd procps util-linux
chroot "$MNT" apt-get clean
rm -rf "$MNT/var/lib/apt/lists/"*

# shellcheck disable=SC2016 # The script expands inside the chroot.
chroot "$MNT" sh -eu -c '
  for command in bash curl flock ip nc ping ss sysctl timeout; do
    command -v "$command" >/dev/null
  done
'

restore_resolv
sync
umount "$MNT"
MOUNTED=0
losetup -d "$LOOP"
LOOP=
rmdir "$MNT"

echo "guest runtime tools installed in $IMAGE"
