#!/usr/bin/env bash
# Linux/c8i qualification for stable-identity attached cloud/raw block plumbing.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
SCRATCH_ROOT="${TARIT_ATTACHED_BLOCK_SCRATCH_ROOT:-${TMPDIR:-/tmp}}"
for command in cargo losetup truncate; do
  command -v "$command" >/dev/null || { echo "FAIL: missing $command" >&2; exit 1; }
done
[ "$(id -u)" -eq 0 ] || { echo "FAIL: attached-block gate must run as root" >&2; exit 1; }
[ -c /dev/loop-control ] || { echo "FAIL: loop-control unavailable" >&2; exit 1; }

DIR=$(mktemp -d "$SCRATCH_ROOT/tarit-attached-block.XXXXXX")
TARGET="$DIR/target"
BACKING="$DIR/cloud-device.raw"
LOOP=""

cleanup() {
  local status=$?
  if [ -n "$LOOP" ] && losetup "$LOOP" >/dev/null 2>&1; then
    losetup -d "$LOOP" || status=1
  fi
  find "$DIR" -depth -delete 2>/dev/null || status=1
  return "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

truncate -s 67108864 "$BACKING"
LOOP=$(losetup --find --show "$BACKING")
test -b "$LOOP"

CARGO_TARGET_DIR="$TARGET" \
TARIT_TEST_ATTACHED_BLOCK_DEVICE="$LOOP" \
TARIT_TEST_ATTACHED_BLOCK_SIZE=67108864 \
cargo test --manifest-path "$ROOT/orch/Cargo.toml" -p tarit-volume

proof=$(dd if="$LOOP" bs=1 skip=4096 count=20 status=none)
[ "$proof" = "tarit-attached-block" ] || {
  echo "FAIL: attached block proof was not durable through reopen" >&2
  exit 1
}

echo "ATTACHED_BLOCK_PROVIDER_PASS device_kind=loop size_bytes=67108864 generation_fence=1 identity_fence=1"
