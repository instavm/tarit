#!/usr/bin/env bash
# Reject a CRC-valid snapshot with malformed runtime state before VM publication.
set -Eeuo pipefail

VMM_BIN=${VMM_BIN:?set VMM_BIN}
KERNEL=${KERNEL:?set KERNEL}
ROOTFS=${ROOTFS:?set ROOTFS to an OCI-derived ext4 image with the Tarit agent}
GUEST_AGENT_BIN=${GUEST_AGENT_BIN:?set GUEST_AGENT_BIN to the candidate vmm-agent}
SOCKET_ROOT=${SOCKET_ROOT:-/tmp}
CONTROL_TIMEOUT_SECS=${CONTROL_TIMEOUT_SECS:-35}
RESTORE_TIMEOUT_SECS=${RESTORE_TIMEOUT_SECS:-120}
VCPUS=${VCPUS:-1}
LIVE_SNAPSHOT=${LIVE_SNAPSHOT:-1}

WORK_DIR=$(mktemp -d "$SOCKET_ROOT/tarit-restore-fail-closed.XXXXXX")
SOCKET="$WORK_DIR/vmm.sock"
LOG="$WORK_DIR/vmm.log"
CORRUPT="$WORK_DIR/corrupt.snap"
TEST_ROOTFS="$WORK_DIR/rootfs.ext4"
RESTORE_ERROR="$WORK_DIR/restore-error.log"
STATUS_ERROR="$WORK_DIR/status-error.log"
SERVE_PID=
ROOTFS_MOUNT=
SNAPSHOT=
INTEGRITY=
OVERLAY=

cleanup() {
  local status=$?
  if [[ -n "$ROOTFS_MOUNT" ]] && mountpoint -q "$ROOTFS_MOUNT"; then
    umount "$ROOTFS_MOUNT" || true
  fi
  if [[ -n "$SERVE_PID" ]]; then
    if [[ -S "$SOCKET" ]] && kill -0 "$SERVE_PID" 2>/dev/null; then
      timeout 20 "$VMM_BIN" --socket "$SOCKET" stop >/dev/null 2>&1 || true
    fi
    kill "$SERVE_PID" 2>/dev/null || true
    for _ in $(seq 1 30); do
      kill -0 "$SERVE_PID" 2>/dev/null || break
      sleep 0.1
    done
    if kill -0 "$SERVE_PID" 2>/dev/null; then
      kill -KILL "$SERVE_PID" 2>/dev/null || true
    fi
    wait "$SERVE_PID" 2>/dev/null || true
  fi
  for artifact in "$SNAPSHOT" "$INTEGRITY" "$OVERLAY"; do
    [[ -z "$artifact" ]] || rm -f -- "$artifact"
  done
  if (( status == 0 )); then
    find "$WORK_DIR" -depth -delete
  else
    echo "FAIL: retained restore diagnostics at $WORK_DIR" >&2
  fi
  return "$status"
}
trap cleanup EXIT

on_error() {
  local status=$?
  trap - ERR
  echo "RESTORE_FAIL_CLOSED_E2E_FAIL line=$1 status=$status" >&2
  tail -n 120 "$LOG" >&2 2>/dev/null || true
  exit "$status"
}
trap 'on_error $LINENO' ERR

for required in cp e2fsck install mount mountpoint python3 sha256sum timeout umount; do
  command -v "$required" >/dev/null || {
    echo "missing required command: $required" >&2
    exit 1
  }
done
[[ $(id -u) -eq 0 ]] || {
  echo "restore fail-closed gate must run as root" >&2
  exit 1
}
[[ -x "$GUEST_AGENT_BIN" ]]
[[ "$VCPUS" =~ ^[1-8]$ ]] || {
  echo "VCPUS must be between 1 and 8" >&2
  exit 1
}
[[ "$LIVE_SNAPSHOT" == 0 || "$LIVE_SNAPSHOT" == 1 ]] || {
  echo "LIVE_SNAPSHOT must be 0 or 1" >&2
  exit 1
}

json_field() {
  local field=$1
  python3 -c 'import json,sys; value=json.load(sys.stdin).get(sys.argv[1]); print("" if value is None else value)' "$field"
}

guest_exec() {
  local command=$1 guest_timeout_ms=${2:-30000} response exit_code stderr
  response=$(timeout "$CONTROL_TIMEOUT_SECS" "$VMM_BIN" --socket "$SOCKET" exec --timeout "$guest_timeout_ms" "$command")
  exit_code=$(json_field exit_code <<<"$response")
  if [[ "$exit_code" != 0 ]]; then
    stderr=$(json_field stderr <<<"$response")
    echo "guest command failed with exit $exit_code: $stderr" >&2
    return 1
  fi
  printf '%s\n' "$response"
}

wait_for_exec() {
  local deadline=$((SECONDS + 90))
  while (( SECONDS < deadline )); do
    # The first serial fallback request on SMP can consume stale readiness
    # bytes. A bounded empty request drains that frame before the proof command.
    guest_exec '' 3000 >/dev/null 2>&1 || true
    if guest_exec 'printf ready' 5000 2>/dev/null | grep -q ready; then
      return 0
    fi
    sleep 1
  done
  echo "guest agent did not become ready" >&2
  return 1
}

rm -f -- "$SOCKET" "$LOG"
cp --reflink=auto --sparse=always -- "$ROOTFS" "$TEST_ROOTFS"
chmod 0600 "$TEST_ROOTFS"
ROOTFS_MOUNT="$WORK_DIR/rootfs-mount"
mkdir -m 0700 "$ROOTFS_MOUNT"
mount -o loop,rw -- "$TEST_ROOTFS" "$ROOTFS_MOUNT"
install -D -m 0755 -- "$GUEST_AGENT_BIN" "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
sync -f "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
umount "$ROOTFS_MOUNT"
ROOTFS_MOUNT=
e2fsck -pf "$TEST_ROOTFS" >/dev/null
echo "restore-e2e: candidate guest agent sha256=$(sha256sum "$GUEST_AGENT_BIN" | awk '{print $1}')"
"$VMM_BIN" --socket "$SOCKET" serve >"$LOG" 2>&1 &
SERVE_PID=$!
for _ in $(seq 1 100); do
  [[ -S "$SOCKET" ]] && break
  sleep 0.05
done
[[ -S "$SOCKET" ]]

CMDLINE='console=tty0 reboot=k panic=1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw init=/usr/sbin/vmm-agent'
"$VMM_BIN" --socket "$SOCKET" create \
  --kernel "$KERNEL" --rootfs "$TEST_ROOTFS" --mem 256 --vcpus "$VCPUS" --cmdline "$CMDLINE" >/dev/null
wait_for_exec
guest_exec "set -eu; test \"\$(nproc)\" -eq $VCPUS; printf strict-restore-proof >/root/strict-restore-proof; sync; test ! -e /dev/kvm; ! grep -Eq \"(^|[[:space:]])(vmx|svm)([[:space:]]|$)\" /proc/cpuinfo" >/dev/null

snapshot_args=(snapshot)
if [[ "$LIVE_SNAPSHOT" == 1 ]]; then
  snapshot_args+=(--live)
fi
snapshot_json=$(timeout "$RESTORE_TIMEOUT_SECS" "$VMM_BIN" --socket "$SOCKET" "${snapshot_args[@]}")
SNAPSHOT=$(json_field path <<<"$snapshot_json")
INTEGRITY=$(json_field integrity_path <<<"$snapshot_json")
OVERLAY=$(json_field overlay_path <<<"$snapshot_json")
[[ -s "$SNAPSHOT" ]]
[[ -z "$INTEGRITY" || -s "$INTEGRITY" ]]
[[ -z "$OVERLAY" || -s "$OVERLAY" ]]
cp --reflink=auto --sparse=always -- "$SNAPSHOT" "$CORRUPT"

# Preserve the VMSN envelope and memory image while replacing only the state
# area. Recompute its CRC so the restore reaches runtime-state decoding instead
# of being rejected by the outer corruption check.
python3 - "$CORRUPT" <<'PY'
import os
import struct
import sys
import zlib

path = sys.argv[1]
with open(path, "r+b", buffering=0) as snapshot:
    header = snapshot.read(32)
    if len(header) != 32 or header[:4] != b"VMSN":
        raise SystemExit("not a VMSN snapshot")
    state_len = struct.unpack_from("<Q", header, 8)[0]
    if not 1 <= state_len <= 16 * 1024 * 1024:
        raise SystemExit(f"unsafe state length: {state_len}")
    chunk = b"\xff" * min(state_len, 64 * 1024)
    remaining = state_len
    checksum = 0
    snapshot.seek(32)
    while remaining:
        part = chunk[: min(remaining, len(chunk))]
        snapshot.write(part)
        checksum = zlib.crc32(part, checksum)
        remaining -= len(part)
    snapshot.seek(16)
    snapshot.write(struct.pack("<I", checksum & 0xFFFFFFFF))
    snapshot.flush()
    os.fsync(snapshot.fileno())
PY

"$VMM_BIN" --socket "$SOCKET" stop >/dev/null

if timeout "$RESTORE_TIMEOUT_SECS" "$VMM_BIN" --socket "$SOCKET" restore \
  --snapshot "$CORRUPT" --memory-policy lazy >"$RESTORE_ERROR" 2>&1; then
  echo "malformed runtime state unexpectedly restored" >&2
  exit 1
fi
grep -q 'snapshot state blob is malformed or unsupported' "$RESTORE_ERROR"

if "$VMM_BIN" --socket "$SOCKET" status >"$STATUS_ERROR" 2>&1; then
  echo "failed restore published a VM slot" >&2
  exit 1
fi
grep -q 'no VM' "$STATUS_ERROR"

# The same server must remain usable after rejecting the corrupt artifact.
timeout "$RESTORE_TIMEOUT_SECS" "$VMM_BIN" --socket "$SOCKET" restore \
  --snapshot "$SNAPSHOT" --memory-policy lazy >/dev/null
wait_for_exec
proof=$(guest_exec 'set -eu; cat /root/strict-restore-proof; test ! -e /dev/kvm; ! grep -Eq "(^|[[:space:]])(vmx|svm)([[:space:]]|$)" /proc/cpuinfo')
grep -q strict-restore-proof <<<"$proof"
if (( VCPUS > 1 )); then
  # Expansion is intentionally performed by the guest shell.
  # shellcheck disable=SC2016
  ap_jiffies=$(guest_exec 'set -eu; before=$(awk '\''/^cpu1 / { print $2 + $4 }'\'' /proc/stat); worker=0; while test "$worker" -lt 4; do (i=0; while test "$i" -lt 2000; do sha256sum /usr/sbin/vmm-agent >/dev/null; i=$((i + 1)); done) & worker=$((worker + 1)); done; wait; after=$(awk '\''/^cpu1 / { print $2 + $4 }'\'' /proc/stat); printf "%s:%s" "$before" "$after"' 30000)
  ap_delta=$(json_field stdout <<<"$ap_jiffies")
  ap_before=${ap_delta%%:*}
  ap_after=${ap_delta##*:}
  [[ "$ap_before" =~ ^[0-9]+$ && "$ap_after" =~ ^[0-9]+$ ]]
  echo "restore-e2e: AP cpu1 jiffies $ap_before -> $ap_after"
  (( ap_after > ap_before ))
fi
"$VMM_BIN" --socket "$SOCKET" stop >/dev/null

if grep -Eiq 'panicked at|thread .* panicked|kernel panic|BUG: unable to handle' "$LOG"; then
  echo "restore gate observed a VMM or guest panic" >&2
  exit 1
fi

echo "RESTORE_FAIL_CLOSED_E2E_PASS malformed_state=rejected vm_published=no valid_lazy_restore=passed nested_virtualization=hidden vcpus=$VCPUS live_snapshot=$LIVE_SNAPSHOT"
