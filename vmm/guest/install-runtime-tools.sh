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

for command in chroot cp losetup mknod mount mountpoint readlink sync umount; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "error: required command not found: $command" >&2
    exit 1
  }
done

MNT="$(mktemp -d)"
LOOP=
MOUNTED=0
DEV_MOUNTED=0
RESOLV_STATE=absent
RESOLV_LINK=
RESOLV_BACKUP=/etc/resolv.conf.tarit-backup

restore_resolv() {
  [ "$MOUNTED" -eq 1 ] || return 0
  [ "$RESOLV_STATE" != restored ] || return 0
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
  if [ "$DEV_MOUNTED" -eq 1 ] && mountpoint -q "$MNT/dev"; then
    umount "$MNT/dev" || rc=$?
  fi
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

# A rootfs unpacked from image layers or exported from a container engine has
# no usable /dev: /dev/null may be missing (a write to it inside the chroot
# then leaves a root-only regular file) or already be such a file, and apt's
# unprivileged download user cannot open it. Give the chroot a private /dev
# with the standard character devices instead of the host's, so the image
# itself is not changed; the guest mounts devtmpfs over /dev at boot anyway.
[ -d "$MNT/dev" ] || install -d -m 0755 "$MNT/dev"
mount -t tmpfs -o mode=0755,size=64k,nosuid tarit-dev "$MNT/dev"
DEV_MOUNTED=1
for node in null:3 zero:5 full:7 random:8 urandom:9 tty:5:0; do
  name=${node%%:*}
  numbers=${node#*:}
  case "$numbers" in
    *:*) major=${numbers%%:*} minor=${numbers#*:} ;;
    *) major=1 minor=$numbers ;;
  esac
  mknod -m 0666 "$MNT/dev/$name" c "$major" "$minor"
done

# Chrooted commands get a fixed environment. The caller's variables name host
# paths (TMPDIR, HOME) that do not exist inside the image, and package
# maintainer scripts fail on them. Proxy settings are kept so apt can still
# reach its mirrors.
CHROOT_ENV=(
  PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
  HOME=/root
  LC_ALL=C
  DEBIAN_FRONTEND=noninteractive
)
for name in http_proxy https_proxy no_proxy HTTP_PROXY HTTPS_PROXY NO_PROXY; do
  [ -z "${!name:-}" ] || CHROOT_ENV+=("$name=${!name}")
done
in_chroot() {
  chroot "$MNT" /usr/bin/env -i "${CHROOT_ENV[@]}" "$@"
}

echo "== installing guest runtime tools =="
in_chroot apt-get update -qq
in_chroot apt-get install -y -qq --no-install-recommends \
  ca-certificates curl iproute2 iputils-ping netcat-openbsd procps util-linux
in_chroot apt-get clean
rm -rf "$MNT/var/lib/apt/lists/"*

# shellcheck disable=SC2016 # The script expands inside the chroot.
in_chroot sh -eu -c '
  for command in bash curl flock ip nc ping ss sysctl timeout; do
    command -v "$command" >/dev/null
  done
'

umount "$MNT/dev"
DEV_MOUNTED=0
restore_resolv
sync
umount "$MNT"
MOUNTED=0
losetup -d "$LOOP"
LOOP=
rmdir "$MNT"

echo "guest runtime tools installed in $IMAGE"
