#!/usr/bin/env bash
# Validate first-diff fallback, incremental restore, and input/output isolation.
set -Eeuo pipefail

REPO_VMM="$(cd "$(dirname "$0")/.." && pwd)"
VMM="${VMM:-$REPO_VMM/target/debug/vmm}"
RESTORE_VMM="${RESTORE_VMM:-$VMM}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
AGENT="${AGENT:-$REPO_VMM/guest/agent/vmm-agent}"
BAKE_AGENT="${BAKE_AGENT:-$REPO_VMM/guest/agent/bake-agent.sh}"
ROOTFS_SOURCE="${ROOTFS_SOURCE:-/tmp/vsock-rootfs.ext4}"
TEST_ROOT="${TARIT_TEST_ROOT:-${TMPDIR:-/tmp}}"
HOST_SWAP_PRESSURE_MIB=${HOST_SWAP_PRESSURE_MIB:-0}

for required in python3 e2fsck sha256sum stat dd od tr; do
  command -v "$required" >/dev/null || {
    echo "FAIL: required command not found: $required" >&2
    exit 1
  }
done
test "$(id -u)" -eq 0 || { echo "FAIL: this gate must run as root" >&2; exit 1; }
test -x "$VMM" || { echo "FAIL: VMM is not executable: $VMM" >&2; exit 1; }
test -x "$RESTORE_VMM" || {
  echo "FAIL: restore VMM is not executable: $RESTORE_VMM" >&2
  exit 1
}
test -f "$KERNEL" || { echo "FAIL: kernel not found: $KERNEL" >&2; exit 1; }
test -x "$AGENT" || { echo "FAIL: guest agent is not executable: $AGENT" >&2; exit 1; }
test -f "$ROOTFS_SOURCE" || { echo "FAIL: rootfs not found: $ROOTFS_SOURCE" >&2; exit 1; }

mkdir -p -- "$TEST_ROOT"
TEST_ROOT=$(cd "$TEST_ROOT" && pwd -P)
TEST_DIR=$(mktemp -d "$TEST_ROOT/tarit-diff-restore.XXXXXX")
ROOTFS="$TEST_DIR/rootfs.ext4"
S1="$TEST_DIR/source.sock"
S2="$TEST_DIR/diff-restore.sock"
S3="$TEST_DIR/alias-restore.sock"
L1="$TEST_DIR/source.log"
L2="$TEST_DIR/diff-restore.log"
L3="$TEST_DIR/alias-restore.log"
P1=
P2=
P3=
PRESSURE_PID=
SNAP_FIRST=
SNAP_DIFF=
SNAP_FROM_ALIAS=
INPUT_ALIAS=

api() {
  python3 - "$1" "$2" <<'PY'
import socket, struct, sys

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.settimeout(90)
sock.connect(sys.argv[1])
body = sys.argv[2].encode()
sock.sendall(struct.pack(">I", len(body)) + body)
header = sock.recv(4)
if len(header) != 4:
    raise SystemExit("short API response header")
length = struct.unpack(">I", header)[0]
data = b""
while len(data) < length:
    chunk = sock.recv(length - len(data))
    if not chunk:
        raise SystemExit("short API response body")
    data += chunk
print(data.decode())
PY
}

stop_server() {
  local socket=$1 pid=$2
  if [[ -S "$socket" ]]; then
    api "$socket" '{"op":"stop"}' >/dev/null 2>&1 || true
  fi
  if [[ -n "$pid" ]]; then
    kill "$pid" >/dev/null 2>&1 || true
    for _ in $(seq 1 40); do
      kill -0 "$pid" >/dev/null 2>&1 || break
      sleep 0.25
    done
    if kill -0 "$pid" >/dev/null 2>&1; then
      kill -KILL "$pid" >/dev/null 2>&1 || true
    fi
    wait "$pid" >/dev/null 2>&1 || true
  fi
}

cleanup() {
  if [[ -n "$PRESSURE_PID" ]]; then
    kill "$PRESSURE_PID" >/dev/null 2>&1 || true
    wait "$PRESSURE_PID" >/dev/null 2>&1 || true
  fi
  stop_server "$S1" "$P1"
  stop_server "$S2" "$P2"
  stop_server "$S3" "$P3"
  for artifact in "$INPUT_ALIAS" "$SNAP_FROM_ALIAS" "$SNAP_DIFF" "$SNAP_FIRST"; do
    if [[ -n "$artifact" ]]; then
      rm -f -- "$artifact"
      rmdir -- "$(dirname "$artifact")" 2>/dev/null || true
    fi
  done
  case "$TEST_DIR" in
    "$TEST_ROOT"/tarit-diff-restore.*) rm -r -- "$TEST_DIR" ;;
  esac
}
trap cleanup EXIT INT TERM

on_error() {
  local status=$? line=${BASH_LINENO[0]:-unknown}
  trap - ERR
  echo "DIFF_RESTORE_FAIL line=$line status=$status" >&2
  for log in "$L1" "$L2" "$L3"; do
    if [[ -f "$log" ]]; then
      echo "--- $(basename "$log") ---" >&2
      tail -100 "$log" >&2
    fi
  done
  exit "$status"
}
trap on_error ERR

[[ "$HOST_SWAP_PRESSURE_MIB" =~ ^[0-9]+$ ]] || {
  echo "FAIL: HOST_SWAP_PRESSURE_MIB must be a non-negative integer" >&2
  exit 1
}

pressure_vmm_into_swap() {
  local vmm_pid=$1 pressure_mib=$2
  local ready="$TEST_DIR/pressure.ready" swap_kib=0 large_mapping_swap_kib=0
  ((pressure_mib > 0)) || return 0
  if ! swapon --noheadings --show=NAME | grep -q .; then
    echo "FAIL: HOST_SWAP_PRESSURE_MIB requires active host swap" >&2
    return 1
  fi
  python3 - "$pressure_mib" "$ready" <<'PY' &
import mmap
import pathlib
import sys
import time

size = int(sys.argv[1]) * 1024 * 1024
memory = mmap.mmap(-1, size)
for offset in range(0, size, 4096):
    memory[offset] = 1
pathlib.Path(sys.argv[2]).write_text("ready\n")
time.sleep(300)
PY
  PRESSURE_PID=$!
  for _ in $(seq 1 180); do
    kill -0 "$PRESSURE_PID" 2>/dev/null || {
      echo "FAIL: host pressure process exited before swap was observed" >&2
      return 1
    }
    if [[ -s "$ready" ]]; then
      swap_kib=$(awk '/^Swap:/ { total += $2 } END { print total + 0 }' "/proc/$vmm_pid/smaps")
      large_mapping_swap_kib=$(awk '
        /^[0-9a-f]+-[0-9a-f]+ / {
          if (size_kib >= 262144) total += swap_kib
          size_kib = 0
          swap_kib = 0
        }
        /^Size:/ { size_kib = $2 }
        /^Swap:/ { swap_kib = $2 }
        END {
          if (size_kib >= 262144) total += swap_kib
          print total + 0
        }
      ' "/proc/$vmm_pid/smaps")
      if ((large_mapping_swap_kib > 0)); then
        echo "SWAP_OBSERVED vmm_pid=$vmm_pid swap_kib=$swap_kib large_mapping_swap_kib=$large_mapping_swap_kib pressure_mib=$pressure_mib"
        return 0
      fi
    fi
    sleep 0.5
  done
  echo "FAIL: VMM pages did not enter swap under ${pressure_mib}MiB host pressure" >&2
  return 1
}

release_host_pressure() {
  if [[ -n "$PRESSURE_PID" ]]; then
    kill "$PRESSURE_PID" >/dev/null 2>&1 || true
    wait "$PRESSURE_PID" >/dev/null 2>&1 || true
    PRESSURE_PID=
  fi
}

guest_stdout() {
  local socket=$1 command=$2 response payload
  payload=$(python3 - "$command" <<'PY'
import json, sys
print(json.dumps({"op": "exec", "command": sys.argv[1], "timeout_ms": 40000}))
PY
  )
  response=$(api "$socket" "$payload")
  python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("status") == "exec" and row.get("exit_code") == 0, row; print(row.get("stdout", "").strip())' <<<"$response"
}

wait_ready() {
  local socket=$1
  for _ in $(seq 1 80); do
    if guest_stdout "$socket" 'true' >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

take_snapshot() {
  local socket=$1 mode=${2:-}
  local args=()
  [[ -n "$mode" ]] && args+=("$mode")
  "$VMM" --socket "$socket" snapshot "${args[@]}" |
    python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("status") == "snapshot" and row.get("path"), row; print(row["path"])'
}

snapshot_kind() {
  python3 - "$1" <<'PY'
import struct, sys
with open(sys.argv[1], "rb") as snapshot:
    header = snapshot.read(8)
assert len(header) == 8, header
magic = header[:4].decode("ascii", "strict")
flags = struct.unpack("<H", header[6:8])[0] if magic == "VMSN" else 0
print(f"{magic}:{flags}")
PY
}

start_server() {
  local socket=$1 log=$2 pid_var=$3 binary=${4:-$VMM}
  RUST_LOG=info "$binary" serve --socket "$socket" >"$log" 2>&1 &
  printf -v "$pid_var" '%s' "$!"
  for _ in $(seq 1 40); do
    [[ -S "$socket" ]] && return 0
    sleep 0.1
  done
  return 1
}

cp --reflink=auto -- "$ROOTFS_SOURCE" "$ROOTFS"
chmod u+w "$ROOTFS"
FSCK_STATUS=0
e2fsck -fy "$ROOTFS" >/dev/null || FSCK_STATUS=$?
if (( FSCK_STATUS > 1 )); then
  echo "FAIL: e2fsck returned $FSCK_STATUS for private rootfs copy" >&2
  exit "$FSCK_STATUS"
fi
"$BAKE_AGENT" "$ROOTFS" "$AGENT" >/dev/null
SOURCE_ROOTFS_SHA=$(sha256sum "$ROOTFS_SOURCE" | awk '{print $1}')

CMDLINE='console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw'
start_server "$S1" "$L1" P1
CREATE=$(python3 - "$KERNEL" "$CMDLINE" "$ROOTFS" <<'PY'
import json, sys
print(json.dumps({"op":"create","config":{"kernel":{"path":sys.argv[1],"cmdline":sys.argv[2],"initramfs":None},"memory":{"size_mib":512},"vcpus":{"count":1},"volumes":[{"path":sys.argv[3],"read_only":False}],"net":[]}}))
PY
)
CREATE_RESPONSE=$(api "$S1" "$CREATE")
python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("status") == "ok", row' <<<"$CREATE_RESPONSE"
wait_ready "$S1"
guest_stdout "$S1" "mkdir -p /run/ramcheck; grep -q ' /run/ramcheck tmpfs ' /proc/mounts || mount -t tmpfs -o size=160m tmpfs /run/ramcheck"

guest_stdout "$S1" 'dd if=/dev/urandom of=/run/ramcheck/F bs=1M count=48 2>/dev/null; sync'
SHA_F=$(guest_stdout "$S1" 'sha256sum /run/ramcheck/F | cut -c1-64')

# A diff request without a parent must be a complete full snapshot. Publishing
# a VMSD tip here would silently omit every clean page in the guest.
SNAP_FIRST=$(take_snapshot "$S1" --diff)
[[ "$(snapshot_kind "$SNAP_FIRST")" == "VMSN:0" ]]
FIRST_SHA_BEFORE=$(sha256sum "$SNAP_FIRST" | awk '{print $1}')
FIRST_ID=$(stat -Lc '%d:%i' "$SNAP_FIRST")
INPUT_ALIAS="$(dirname "$SNAP_FIRST")/.$(basename "$SNAP_FIRST").alias-${BASHPID}"
ln -- "$SNAP_FIRST" "$INPUT_ALIAS"
[[ "$(stat -Lc '%d:%i' "$INPUT_ALIAS")" == "$FIRST_ID" ]]

pressure_vmm_into_swap "$P1" "$HOST_SWAP_PRESSURE_MIB"

guest_stdout "$S1" 'dd if=/dev/urandom of=/run/ramcheck/G bs=1M count=32 2>/dev/null; sync'
SHA_G=$(guest_stdout "$S1" 'sha256sum /run/ramcheck/G | cut -c1-64')
SNAP_DIFF=$(take_snapshot "$S1" --diff)
[[ "$(snapshot_kind "$SNAP_DIFF")" == "VMSD:0" ]]
release_host_pressure

stop_server "$S1" "$P1"
P1=

start_server "$S2" "$L2" P2 "$RESTORE_VMM"
RESTORE_DIFF=$(python3 - "$SNAP_DIFF" <<'PY'
import json, sys
print(json.dumps({"op":"restore","snapshot_path":sys.argv[1]}))
PY
)
RESTORE_DIFF_RESPONSE=$(api "$S2" "$RESTORE_DIFF")
python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("status") == "restored", row' <<<"$RESTORE_DIFF_RESPONSE"
wait_ready "$S2"
RESTORED_F=$(guest_stdout "$S2" 'sha256sum /run/ramcheck/F | cut -c1-64')
RESTORED_G=$(guest_stdout "$S2" 'sha256sum /run/ramcheck/G | cut -c1-64')
[[ "$RESTORED_F" == "$SHA_F" ]]
[[ "$RESTORED_G" == "$SHA_G" ]]
stop_server "$S2" "$P2"
P2=

# Restore through a hardlink spelling of the full input, then take another full
# snapshot. The output must be a new inode and the loaded source must not change.
start_server "$S3" "$L3" P3 "$RESTORE_VMM"
RESTORE_ALIAS=$(python3 - "$INPUT_ALIAS" <<'PY'
import json, sys
print(json.dumps({"op":"restore","snapshot_path":sys.argv[1]}))
PY
)
RESTORE_ALIAS_RESPONSE=$(api "$S3" "$RESTORE_ALIAS")
python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("status") == "restored", row' <<<"$RESTORE_ALIAS_RESPONSE"
wait_ready "$S3"
[[ "$(guest_stdout "$S3" 'sha256sum /run/ramcheck/F | cut -c1-64')" == "$SHA_F" ]]
SNAP_FROM_ALIAS=$(take_snapshot "$S3")
[[ "$(snapshot_kind "$SNAP_FROM_ALIAS")" == "VMSN:0" ]]
[[ "$(stat -Lc '%d:%i' "$SNAP_FROM_ALIAS")" != "$FIRST_ID" ]]
[[ "$(sha256sum "$SNAP_FIRST" | awk '{print $1}')" == "$FIRST_SHA_BEFORE" ]]
[[ "$(stat -Lc '%d:%i' "$INPUT_ALIAS")" == "$FIRST_ID" ]]
stop_server "$S3" "$P3"
P3=

[[ "$(sha256sum "$ROOTFS_SOURCE" | awk '{print $1}')" == "$SOURCE_ROOTFS_SHA" ]]
if grep -Eqi 'guest panic|BUG:|KVM_RUN.*error|seccomp.*kill|snapshot.*corrupt' "$L1" "$L2" "$L3"; then
  echo "FAIL: unexpected VMM or guest error in logs" >&2
  exit 1
fi

echo "FIRST_DIFF_FULL_PASS path=$SNAP_FIRST identity=$FIRST_ID"
echo "DIFF_CHAIN_RESTORE_PASS full_sha=$SHA_F diff_sha=$SHA_G"
echo "SNAPSHOT_ALIAS_ISOLATION_PASS input=$INPUT_ALIAS output=$SNAP_FROM_ALIAS"
if [[ "$RESTORE_VMM" != "$VMM" ]]; then
  echo "CROSS_BUILD_RESTORE_PASS source_vmm=$VMM restore_vmm=$RESTORE_VMM"
fi
if ((HOST_SWAP_PRESSURE_MIB > 0)); then
  echo "SWAP_DIFF_RESTORE_PASS pressure_mib=$HOST_SWAP_PRESSURE_MIB"
fi
