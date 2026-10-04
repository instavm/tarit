#!/usr/bin/env bash
# Disposable loopback NFSv4.1 provider qualification for Linux/c8i.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
SCRATCH_ROOT="${TARIT_NFS_SCRATCH_ROOT:-${TMPDIR:-/tmp}}"
CARGO_BIN="${TARIT_CARGO_BIN:-$(command -v cargo || true)}"
RUSTC_BIN="${TARIT_RUSTC_BIN:-$(command -v rustc || true)}"
for command in exportfs mount.nfs4 systemctl timeout; do
  command -v "$command" >/dev/null || { echo "FAIL: missing $command" >&2; exit 1; }
done
[ -x "$CARGO_BIN" ] || { echo "FAIL: cargo is not executable: $CARGO_BIN" >&2; exit 1; }
[ -x "$RUSTC_BIN" ] || { echo "FAIL: rustc is not executable: $RUSTC_BIN" >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "FAIL: NFS provider gate must run as root" >&2; exit 1; }

UNIT=""
for candidate in nfs-server.service nfs-kernel-server.service; do
  if systemctl cat "$candidate" >/dev/null 2>&1; then
    UNIT="$candidate"
    break
  fi
done
[ -n "$UNIT" ] || { echo "FAIL: no NFS server systemd unit" >&2; exit 1; }
WAS_ACTIVE=0
systemctl is-active --quiet "$UNIT" && WAS_ACTIVE=1

DIR=$(mktemp -d "$SCRATCH_ROOT/tarit-nfs.XXXXXX")
EXPORT="$DIR/export"
MOUNTS="$DIR/mounts"
TARGET="$DIR/target"
mkdir -m 700 "$EXPORT" "$MOUNTS"
EXPORTED=0
EXPORT_CONFIG="/etc/exports.d/tarit-provider-test-$$.exports"

cleanup() {
  local status=$?
  local mounted_target
  while IFS= read -r mounted_target; do
    [[ "$mounted_target" == "$MOUNTS/"* ]] || continue
    if ! timeout --signal=TERM --kill-after=2s 10s umount -- "$mounted_target" 2>/dev/null; then
      # Only the gate's exact prefix is eligible. Lazy detach prevents cleanup
      # from traversing a dead hard mount after an interrupted server test.
      umount -l -- "$mounted_target" 2>/dev/null || status=1
    fi
  done < <(findmnt -rn -t nfs4 -o TARGET || true)
  if [ "$EXPORTED" -eq 1 ]; then
    exportfs -u "127.0.0.1:$EXPORT" 2>/dev/null || status=1
  fi
  rm -f -- "$EXPORT_CONFIG"
  exportfs -ra 2>/dev/null || status=1
  if [ "$WAS_ACTIVE" -eq 0 ]; then
    systemctl stop "$UNIT" >/dev/null 2>&1 || status=1
  fi
  find "$DIR" -depth -delete 2>/dev/null || status=1
  return "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

systemctl start "$UNIT"
# `noresvport` is intentionally part of the client profile (and is required by
# AWS EFS guidance), so this loopback-only fixture must permit a high client
# source port. The export remains restricted to 127.0.0.1 and is removed by the
# EXIT trap.
install -d -m 755 /etc/exports.d
printf '%s 127.0.0.1(rw,sync,no_subtree_check,no_root_squash,fsid=0,insecure)\n' \
  "$EXPORT" >"$EXPORT_CONFIG"
chmod 600 "$EXPORT_CONFIG"
exportfs -ra
EXPORTED=1

timeout --signal=TERM --kill-after=20s 180s env \
  CARGO_TARGET_DIR="$TARGET" \
  TARIT_TEST_NFS_ENDPOINT=127.0.0.1 \
  TARIT_TEST_NFS_EXPORT=/ \
  TARIT_TEST_NFS_MOUNT_ROOT="$MOUNTS" \
  TARIT_TEST_NFS_SYSTEMD_UNIT="$UNIT" \
  RUSTC="$RUSTC_BIN" \
  "$CARGO_BIN" test --manifest-path "$ROOT/orch/Cargo.toml" -p tarit-volume \
    nfs::tests::disposable_nfs_ -- --nocapture --test-threads=1

[ "$(cat "$EXPORT/proof")" = "before-interruption-after-reconnect" ] || {
  echo "FAIL: NFS proof did not survive server interruption" >&2
  exit 1
}

echo "NFS_PROVIDER_PASS dialect=nfs4 reconnect=1 busy_detach=1 block_reopen=1 credentials_in_guest=0"
