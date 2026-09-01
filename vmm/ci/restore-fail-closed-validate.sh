#!/usr/bin/env bash
# Reject malformed or incompatible runtime state before VM publication. Set
# INCOMPATIBLE_VMM_BIN to a VMM built with
# `--features boot,test-incompatible-snapshot-abi` to exercise a real binary
# state-schema boundary in addition to checksum-valid artifact mutations.
set -Eeuo pipefail

VMM_BIN=${VMM_BIN:?set VMM_BIN}
KERNEL=${KERNEL:?set KERNEL}
ROOTFS=${ROOTFS:?set ROOTFS to an OCI-derived ext4 image with the Tarit agent}
GUEST_AGENT_BIN=${GUEST_AGENT_BIN:?set GUEST_AGENT_BIN to the candidate vmm-agent}
INCOMPATIBLE_VMM_BIN=${INCOMPATIBLE_VMM_BIN:-}
SOCKET_ROOT=${SOCKET_ROOT:-/tmp}
CONTROL_TIMEOUT_SECS=${CONTROL_TIMEOUT_SECS:-35}
RESTORE_TIMEOUT_SECS=${RESTORE_TIMEOUT_SECS:-120}
VCPUS=${VCPUS:-1}
LIVE_SNAPSHOT=${LIVE_SNAPSHOT:-1}
FAIL_PHASE=${FAIL_PHASE:-}

WORK_DIR=$(mktemp -d "$SOCKET_ROOT/tarit-restore-fail-closed.XXXXXX")
SOCKET="$WORK_DIR/vmm.sock"
INCOMPATIBLE_SOCKET="$WORK_DIR/incompatible-vmm.sock"
LOG="$WORK_DIR/vmm.log"
INCOMPATIBLE_LOG="$WORK_DIR/incompatible-vmm.log"
CORRUPT="$WORK_DIR/corrupt.snap"
INCOMPATIBLE="$WORK_DIR/incompatible.snap"
DOWNGRADED="$WORK_DIR/downgraded.snap"
TEST_ROOTFS="$WORK_DIR/rootfs.ext4"
RESTORE_ERROR="$WORK_DIR/restore-error.log"
STATUS_ERROR="$WORK_DIR/status-error.log"
SNAPSHOT_FAILURE="$WORK_DIR/snapshot-failure.log"
PAUSED_LIVE_ERROR="$WORK_DIR/paused-live-error.log"
SERVE_PID=
INCOMPATIBLE_SERVE_PID=
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
  if [[ -n "$INCOMPATIBLE_SERVE_PID" ]]; then
    kill "$INCOMPATIBLE_SERVE_PID" 2>/dev/null || true
    for _ in $(seq 1 30); do
      kill -0 "$INCOMPATIBLE_SERVE_PID" 2>/dev/null || break
      sleep 0.1
    done
    if kill -0 "$INCOMPATIBLE_SERVE_PID" 2>/dev/null; then
      kill -KILL "$INCOMPATIBLE_SERVE_PID" 2>/dev/null || true
    fi
    wait "$INCOMPATIBLE_SERVE_PID" 2>/dev/null || true
  fi
  for artifact in "$SNAPSHOT" "$INTEGRITY" "$OVERLAY"; do
    if [[ -n "$artifact" ]]; then
      rm -f -- "$artifact"
      rmdir -- "$(dirname "$artifact")" 2>/dev/null || true
    fi
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
[[ -z "$INCOMPATIBLE_VMM_BIN" || -x "$INCOMPATIBLE_VMM_BIN" ]]
[[ "$VCPUS" =~ ^[1-8]$ ]] || {
  echo "VCPUS must be between 1 and 8" >&2
  exit 1
}
[[ "$LIVE_SNAPSHOT" == 0 || "$LIVE_SNAPSHOT" == 1 ]] || {
  echo "LIVE_SNAPSHOT must be 0 or 1" >&2
  exit 1
}
case "$FAIL_PHASE" in
  '' | dirty_logging | bulk | dirty_round | final_pause | state_capture) ;;
  *)
    echo "unsupported FAIL_PHASE: $FAIL_PHASE" >&2
    exit 1
    ;;
esac

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

verify_ap_progress() {
  local label=$1 ap_jiffies ap_output ap_seen ap_cpu ap_before ap_after
  ((VCPUS > 1)) || return 0
  # More CPU-bound workers than vCPUs avoids depending on taskset, which is
  # absent from minimal Alpine images. Every restored AP must receive work and
  # advance its scheduler accounting.
  # shellcheck disable=SC2016
  ap_jiffies=$(guest_exec 'set -eu; before=/tmp/tarit-ap-before.$$; cp /proc/stat "$before"; worker=0; workers='"$((VCPUS * 2))"'; iterations='"$((4000 / (VCPUS * 2)))"'; while test "$worker" -lt "$workers"; do (i=0; while test "$i" -lt "$iterations"; do sha256sum /usr/sbin/vmm-agent >/dev/null; i=$((i + 1)); done) & worker=$((worker + 1)); done; wait; awk -v limit='"$VCPUS"' '\''FNR == NR { if ($1 ~ /^cpu[0-9]+$/) before[$1] = $2 + $4; next } $1 ~ /^cpu[0-9]+$/ { cpu = substr($1, 4) + 0; if (cpu > 0 && cpu < limit) printf "%d:%d:%d\n", cpu, before[$1], $2 + $4 }'\'' "$before" /proc/stat; rm -f "$before"' 30000)
  ap_output=$(json_field stdout <<<"$ap_jiffies")
  ap_seen=0
  while IFS=: read -r ap_cpu ap_before ap_after; do
    [[ "$ap_cpu" =~ ^[1-9][0-9]*$ && "$ap_before" =~ ^[0-9]+$ && "$ap_after" =~ ^[0-9]+$ ]]
    echo "restore-e2e: $label AP cpu$ap_cpu jiffies $ap_before -> $ap_after"
    ((ap_after > ap_before))
    ((ap_seen += 1))
  done <<<"$ap_output"
  ((ap_seen == VCPUS - 1))
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
TARIT_TEST_LIVE_SNAPSHOT_FAIL_PHASE="$FAIL_PHASE" \
  "$VMM_BIN" --socket "$SOCKET" serve --allow-unverified-restore >"$LOG" 2>&1 &
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

if [[ "$LIVE_SNAPSHOT" == 1 ]]; then
  "$VMM_BIN" --socket "$SOCKET" pause >/dev/null
  if "$VMM_BIN" --socket "$SOCKET" snapshot --live >"$PAUSED_LIVE_ERROR" 2>&1; then
    echo "live snapshot unexpectedly accepted an already-paused VM" >&2
    exit 1
  fi
  grep -Fq 'live snapshot requires a running VM' "$PAUSED_LIVE_ERROR"
  status_json=$("$VMM_BIN" --socket "$SOCKET" status)
  status_state=$(json_field state <<<"$status_json")
  [[ "${status_state,,}" == paused ]]
  "$VMM_BIN" --socket "$SOCKET" resume >/dev/null
  wait_for_exec
  guest_exec 'grep -qx strict-restore-proof /root/strict-restore-proof' >/dev/null
fi

if [[ -n "$FAIL_PHASE" ]]; then
  if timeout "$RESTORE_TIMEOUT_SECS" "$VMM_BIN" --socket "$SOCKET" snapshot --live \
    >"$SNAPSHOT_FAILURE" 2>&1; then
    echo "injected live snapshot failure unexpectedly succeeded" >&2
    exit 1
  fi
  grep -Fq "injected live snapshot failure at $FAIL_PHASE" "$SNAPSHOT_FAILURE"
  guest_exec 'set -eu; grep -qx strict-restore-proof /root/strict-restore-proof; test ! -e /dev/kvm; ! grep -Eq "(^|[[:space:]])(vmx|svm)([[:space:]]|$)" /proc/cpuinfo' >/dev/null
  verify_ap_progress source-after-failure
  status_json=$("$VMM_BIN" --socket "$SOCKET" status)
  status_state=$(json_field state <<<"$status_json")
  [[ "${status_state,,}" == running ]]
  runtime_dir="${SOCKET_ROOT%/}/.vmm-runtime/vmm-$SERVE_PID"
  if [[ -d "$runtime_dir" ]] && \
    find "$runtime_dir" -maxdepth 1 -type f -name '*live*' -print -quit | grep -q .; then
    echo "failed live snapshot leaked a staged artifact" >&2
    exit 1
  fi
  "$VMM_BIN" --socket "$SOCKET" stop >/dev/null
  echo "LIVE_SMP_FAILPOINT_E2E_PASS phase=$FAIL_PHASE source_resumed=yes artifacts=clean nested_virtualization=hidden vcpus=$VCPUS"
  exit 0
fi

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
guest_exec 'set -eu; grep -qx strict-restore-proof /root/strict-restore-proof' >/dev/null
verify_ap_progress source
cp --reflink=auto --sparse=always -- "$SNAPSHOT" "$CORRUPT"
cp --reflink=auto --sparse=always -- "$SNAPSHOT" "$INCOMPATIBLE"
cp --reflink=auto --sparse=always -- "$SNAPSHOT" "$DOWNGRADED"

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

if [[ -n "$INCOMPATIBLE_VMM_BIN" ]]; then
  rm -f -- "$INCOMPATIBLE_SOCKET" "$INCOMPATIBLE_LOG"
  "$INCOMPATIBLE_VMM_BIN" --socket "$INCOMPATIBLE_SOCKET" serve \
    --allow-unverified-restore \
    >"$INCOMPATIBLE_LOG" 2>&1 &
  INCOMPATIBLE_SERVE_PID=$!
  for _ in $(seq 1 100); do
    [[ -S "$INCOMPATIBLE_SOCKET" ]] && break
    kill -0 "$INCOMPATIBLE_SERVE_PID" 2>/dev/null || {
      tail -120 "$INCOMPATIBLE_LOG" >&2
      exit 1
    }
    sleep 0.05
  done
  [[ -S "$INCOMPATIBLE_SOCKET" ]]
  if timeout "$RESTORE_TIMEOUT_SECS" "$INCOMPATIBLE_VMM_BIN" \
    --socket "$INCOMPATIBLE_SOCKET" restore --snapshot "$SNAPSHOT" \
    --memory-policy lazy --allow-unverified-restore >"$RESTORE_ERROR" 2>&1; then
    echo "incompatible VMM binary unexpectedly restored the snapshot" >&2
    exit 1
  fi
  grep -q 'incompatible snapshot state ABI' "$RESTORE_ERROR"
  if "$INCOMPATIBLE_VMM_BIN" --socket "$INCOMPATIBLE_SOCKET" status \
    >"$STATUS_ERROR" 2>&1; then
    echo "incompatible VMM binary published a VM slot" >&2
    exit 1
  fi
  grep -q 'no VM' "$STATUS_ERROR"
  kill "$INCOMPATIBLE_SERVE_PID"
  wait "$INCOMPATIBLE_SERVE_PID" 2>/dev/null || true
  INCOMPATIBLE_SERVE_PID=
  rm -f -- "$INCOMPATIBLE_SOCKET"
  echo "restore-e2e: incompatible VMM binary rejected the state ABI before publication"
fi

# Change only the named CPU template inside the compatibility trailer and
# recompute the state CRC. The state remains syntactically valid, so restore
# must reject it as incompatible rather than as generic corruption.
python3 - "$INCOMPATIBLE" <<'PY'
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
    state = bytearray(snapshot.read(state_len))
    trailer = state.find(b"TRTCMP01")
    if trailer < 0:
        raise SystemExit("snapshot has no compatibility trailer")
    template = state.find(b"bare", trailer)
    if template < 0:
        raise SystemExit("compatibility trailer has no bare template")
    state[template:template + 4] = b"fake"
    snapshot.seek(32)
    snapshot.write(state)
    snapshot.seek(16)
    snapshot.write(struct.pack("<I", zlib.crc32(state) & 0xFFFFFFFF))
    snapshot.flush()
    os.fsync(snapshot.fileno())
PY

if timeout "$RESTORE_TIMEOUT_SECS" "$VMM_BIN" --socket "$SOCKET" restore \
  --snapshot "$INCOMPATIBLE" --memory-policy lazy --allow-unverified-restore \
  >"$RESTORE_ERROR" 2>&1; then
  echo "incompatible CPU template unexpectedly restored" >&2
  exit 1
fi
grep -q 'incompatible snapshot CPU template' "$RESTORE_ERROR"
if "$VMM_BIN" --socket "$SOCKET" status >"$STATUS_ERROR" 2>&1; then
  echo "incompatible restore published a VM slot" >&2
  exit 1
fi
grep -q 'no VM' "$STATUS_ERROR"

# Removing the compatibility trailer from a version-2 envelope is a downgrade,
# not a legacy snapshot. Zero the trailer while preserving the state length and
# a valid CRC; restore must still fail before VM publication.
python3 - "$DOWNGRADED" <<'PY'
import os
import struct
import sys
import zlib

path = sys.argv[1]
with open(path, "r+b", buffering=0) as snapshot:
    header = snapshot.read(32)
    if len(header) != 32 or header[:4] != b"VMSN":
        raise SystemExit("not a VMSN snapshot")
    version = struct.unpack_from("<H", header, 4)[0]
    if version != 2:
        raise SystemExit(f"downgrade gate requires VMSN version 2, got {version}")
    state_len = struct.unpack_from("<Q", header, 8)[0]
    if not 1 <= state_len <= 16 * 1024 * 1024:
        raise SystemExit(f"unsafe state length: {state_len}")
    state = bytearray(snapshot.read(state_len))
    trailer = state.find(b"TRTCMP01")
    if trailer < 0:
        raise SystemExit("snapshot has no compatibility trailer")
    state[trailer:] = b"\0" * (len(state) - trailer)
    snapshot.seek(32)
    snapshot.write(state)
    snapshot.seek(16)
    snapshot.write(struct.pack("<I", zlib.crc32(state) & 0xFFFFFFFF))
    snapshot.flush()
    os.fsync(snapshot.fileno())
PY

if timeout "$RESTORE_TIMEOUT_SECS" "$VMM_BIN" --socket "$SOCKET" restore \
  --snapshot "$DOWNGRADED" --memory-policy lazy --allow-unverified-restore \
  >"$RESTORE_ERROR" 2>&1; then
  echo "compatibility-manifest downgrade unexpectedly restored" >&2
  exit 1
fi
grep -q 'snapshot compatibility manifest is missing' "$RESTORE_ERROR"
if "$VMM_BIN" --socket "$SOCKET" status >"$STATUS_ERROR" 2>&1; then
  echo "downgraded restore published a VM slot" >&2
  exit 1
fi
grep -q 'no VM' "$STATUS_ERROR"

if timeout "$RESTORE_TIMEOUT_SECS" "$VMM_BIN" --socket "$SOCKET" restore \
  --snapshot "$CORRUPT" --memory-policy lazy --allow-unverified-restore \
  >"$RESTORE_ERROR" 2>&1; then
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
  --snapshot "$SNAPSHOT" --memory-policy lazy --allow-unverified-restore >/dev/null
wait_for_exec
proof=$(guest_exec 'set -eu; cat /root/strict-restore-proof; test ! -e /dev/kvm; ! grep -Eq "(^|[[:space:]])(vmx|svm)([[:space:]]|$)" /proc/cpuinfo')
grep -q strict-restore-proof <<<"$proof"
verify_ap_progress restored
"$VMM_BIN" --socket "$SOCKET" stop >/dev/null

if grep -Eiq 'panicked at|thread .* panicked|kernel panic|BUG: unable to handle' "$LOG"; then
  echo "restore gate observed a VMM or guest panic" >&2
  exit 1
fi

binary_boundary=not_requested
[[ -z "$INCOMPATIBLE_VMM_BIN" ]] || binary_boundary=rejected
echo "RESTORE_FAIL_CLOSED_E2E_PASS malformed_state=rejected incompatible_template=rejected incompatible_binary=$binary_boundary manifest_downgrade=rejected vm_published=no valid_lazy_restore=passed nested_virtualization=hidden vcpus=$VCPUS live_snapshot=$LIVE_SNAPSHOT"
