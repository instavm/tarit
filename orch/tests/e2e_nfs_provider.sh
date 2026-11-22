#!/usr/bin/env bash
# Disposable loopback NFSv4.1 provider qualification for Linux.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
SCRATCH_ROOT="${TARIT_NFS_SCRATCH_ROOT:-${TMPDIR:-/tmp}}"
# A cross-compiled test executable can be copied into an isolated guest.
if [[ -n ${TARIT_NFS_TEST_BINARY:-} ]]; then
  [[ -x $TARIT_NFS_TEST_BINARY ]] || { echo "Missing NFS test executable" >&2; exit 1; }
  test_cmd=("$TARIT_NFS_TEST_BINARY")
else
  CARGO_BIN="${TARIT_CARGO_BIN:-$(command -v cargo || true)}"
  export RUSTC="${TARIT_RUSTC_BIN:-$(command -v rustc || true)}"
  [[ -x $CARGO_BIN && -x $RUSTC ]] || { echo "Missing Rust toolchain" >&2; exit 1; }
  test_cmd=("$CARGO_BIN" test --locked --manifest-path "$ROOT/orch/Cargo.toml" -p tarit-volume --)
fi
SECURITY=${TARIT_TEST_NFS_SECURITY:-sys}
ENDPOINT=${TARIT_TEST_NFS_ENDPOINT:-127.0.0.1}
case "$SECURITY" in sys|krb5p) ;; *) echo "Unsupported NFS test security: $SECURITY" >&2; exit 1 ;; esac
# Both the export and the nfsd service controlled below belong to this host.
getent ahostsv4 "$ENDPOINT" | awk 'NF && $1 !~ /^127\./ { bad=1 } END { exit (NR == 0 || bad) }' || {
  echo "The NFS test endpoint must resolve to loopback" >&2
  exit 1
}
for command in exportfs mount.nfs4 systemctl timeout; do
  command -v "$command" >/dev/null || { echo "FAIL: missing $command" >&2; exit 1; }
done
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
SERVER_STARTED=0
EXPORT_CONFIG="/etc/exports.d/tarit-provider-test-$$.exports"

cleanup() {
  local status=$?
  local mounted_target
  # Restore the server before touching a hard mount if a test was interrupted.
  if [[ $SERVER_STARTED == 1 ]] && ! systemctl is-active --quiet "$UNIT"; then
    systemctl start "$UNIT" || status=1
  fi
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
  find "$DIR" -xdev -depth -delete 2>/dev/null || status=1
  return "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

systemctl start "$UNIT"
SERVER_STARTED=1
# `noresvport` is intentionally part of the client profile (and is required by
# AWS EFS guidance), so this loopback-only fixture must permit a high client
# source port. The export remains restricted to 127.0.0.1 and is removed by the
# EXIT trap.
install -d -m 755 /etc/exports.d
printf '%s 127.0.0.1(rw,sync,no_subtree_check,no_root_squash,fsid=0,insecure,sec=%s)\n' \
  "$EXPORT" "$SECURITY" >"$EXPORT_CONFIG"
chmod 600 "$EXPORT_CONFIG"
exportfs -ra
EXPORTED=1

timeout --signal=TERM --kill-after=20s 180s env \
  CARGO_TARGET_DIR="$TARGET" \
  TARIT_TEST_NFS_ENDPOINT="$ENDPOINT" \
  TARIT_TEST_NFS_SECURITY="$SECURITY" \
  TARIT_TEST_NFS_EXPORT=/ \
  TARIT_TEST_NFS_MOUNT_ROOT="$MOUNTS" \
  TARIT_TEST_NFS_SYSTEMD_UNIT="$UNIT" \
  "${test_cmd[@]}" nfs::tests::disposable_nfs_ --nocapture --test-threads=1

[ "$(cat "$EXPORT/proof")" = "before-interruption-during-outage-after-reconnect" ] || {
  echo "FAIL: NFS proof did not survive server interruption" >&2
  exit 1
}

echo "NFS_PROVIDER_PASS dialect=nfs4 security=$SECURITY in_flight_write=1 reconnect=1 busy_detach=1 block_reopen=1 credentials_in_guest=0"
