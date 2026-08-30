#!/usr/bin/env bash
# Validate VM Generation ID delivery and clone entropy on a real KVM host.
set -euo pipefail

VMM="${VMM:?set VMM to the vmm binary}"
KERNEL="${KERNEL:?set KERNEL to a VMGenID-capable kernel}"
ROOTFS="${ROOTFS:?set ROOTFS to an Ubuntu guest rootfs}"
EXPECT_VMGENID_DRIVER="${EXPECT_VMGENID_DRIVER:?set EXPECT_VMGENID_DRIVER to 0 or 1}"
case "$EXPECT_VMGENID_DRIVER" in 0|1) ;; *) echo "EXPECT_VMGENID_DRIVER must be 0 or 1" >&2; exit 2 ;; esac

TEST_DIR=$(mktemp -d /tmp/tarit-vmgenid.XXXXXX)
SOURCE_SOCK="$TEST_DIR/source.sock"
CLONE_A_SOCK="$TEST_DIR/clone-a.sock"
CLONE_B_SOCK="$TEST_DIR/clone-b.sock"
SOURCE_LOG="$TEST_DIR/source.log"
CLONE_A_LOG="$TEST_DIR/clone-a.log"
CLONE_B_LOG="$TEST_DIR/clone-b.log"
BASE="$TEST_DIR/ubuntu.ext4"
GOLDEN_OVERLAY="$TEST_DIR/golden.overlay"
CLONE_A_OVERLAY="$TEST_DIR/clone-a.overlay"
CLONE_B_OVERLAY="$TEST_DIR/clone-b.overlay"
SNAPSHOT_COPY="$TEST_DIR/golden.snap"
SOURCE_PID=
CLONE_A_PID=
CLONE_B_PID=

api() {
  python3 - "$1" "$2" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(60)
s.connect(sys.argv[1])
body = sys.argv[2].encode()
s.sendall(struct.pack(">I", len(body)) + body)
header = s.recv(4)
if len(header) != 4:
    raise SystemExit("short API response header")
length = struct.unpack(">I", header)[0]
data = b""
while len(data) < length:
    chunk = s.recv(length - len(data))
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
    wait "$pid" >/dev/null 2>&1 || true
  fi
}

cleanup() {
  stop_server "$SOURCE_SOCK" "$SOURCE_PID"
  stop_server "$CLONE_A_SOCK" "$CLONE_A_PID"
  stop_server "$CLONE_B_SOCK" "$CLONE_B_PID"
  case "$TEST_DIR" in
    /tmp/tarit-vmgenid.*) rm -r -- "$TEST_DIR" ;;
  esac
}
trap cleanup EXIT INT TERM

on_error() {
  local status=$? line=${BASH_LINENO[0]:-unknown}
  trap - ERR
  echo "VMGENID_E2E_FAIL line=$line status=$status" >&2
  for log in "$SOURCE_LOG" "$CLONE_A_LOG" "$CLONE_B_LOG"; do
    if [[ -f "$log" ]]; then
      echo "--- $(basename "$log") ---" >&2
      tail -80 "$log" >&2
    fi
  done
  exit "$status"
}
trap on_error ERR

guest_stdout() {
  local socket=$1 command=$2 response payload
  payload=$(python3 - "$command" <<'PY'
import json, sys
print(json.dumps({"op": "exec", "command": sys.argv[1], "timeout_ms": 15000}))
PY
  )
  response=$(api "$socket" "$payload")
  python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("status") == "exec" and row.get("exit_code") == 0, row; print(row.get("stdout", "").strip())' <<<"$response"
}

wait_ready() {
  local socket=$1
  for _ in $(seq 1 60); do
    if guest_stdout "$socket" 'true' >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

generation_id() {
  guest_stdout "$1" "python3 -c 'import os; f=os.open(\"/dev/mem\", os.O_RDONLY); print(os.pread(f,16,0xe6000).hex())'"
}

random_sample() {
  guest_stdout "$1" "python3 -c 'import os; print(os.getrandom(64).hex())'"
}

ged_count() {
  guest_stdout "$1" "awk '/ACPI:Ged/{sum=0; for(i=2;i<=NF;i++){if(\$i ~ /^[0-9]+\$/) sum+=\$i; else break} print sum}' /proc/interrupts"
}

boot_id() {
  guest_stdout "$1" 'cat /proc/sys/kernel/random/boot_id'
}

vmgen_reseed_count() {
  guest_stdout "$1" "dmesg | grep -c 'crng reseeded due to virtual machine fork' || true"
}

clone_id() {
  guest_stdout "$1" 'cat /run/tarit/clone-id'
}

sample_digest() {
  printf '%s' "$1" | sha256sum | awk '{print $1}'
}

cp --reflink=auto -- "$ROOTFS" "$BASE"
BASE_SHA_BEFORE=$(sha256sum "$BASE" | awk '{print $1}')
CMDLINE='console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw'

RUST_LOG=info "$VMM" serve --socket "$SOURCE_SOCK" >"$SOURCE_LOG" 2>&1 &
SOURCE_PID=$!
for _ in $(seq 1 40); do [[ -S "$SOURCE_SOCK" ]] && break; sleep 0.1; done
CREATE=$(python3 - "$KERNEL" "$CMDLINE" "$BASE" "$GOLDEN_OVERLAY" <<'PY'
import json, sys
print(json.dumps({"op":"create","config":{"kernel":{"path":sys.argv[1],"cmdline":sys.argv[2],"initramfs":None},"memory":{"size_mib":512},"vcpus":{"count":1},"volumes":[{"path":sys.argv[3],"read_only":True,"overlay":sys.argv[4]}],"net":[]}}))
PY
)
CREATE_RESPONSE=$(api "$SOURCE_SOCK" "$CREATE")
python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("status") == "ok", row' <<<"$CREATE_RESPONSE"
echo "source: waiting for Ubuntu guest agent"
wait_ready "$SOURCE_SOCK"
guest_stdout "$SOURCE_SOCK" 'test -d /sys/bus/acpi/devices/VMGENCTR:00 && grep -q "ACPI:Ged" /proc/interrupts'
if [[ "$EXPECT_VMGENID_DRIVER" == 1 ]]; then
  guest_stdout "$SOURCE_SOCK" 'test -L /sys/bus/acpi/devices/VMGENCTR:00/driver && readlink /sys/bus/acpi/devices/VMGENCTR:00/driver | grep -q "/vmgenid$"'
else
  guest_stdout "$SOURCE_SOCK" 'test ! -e /sys/bus/acpi/devices/VMGENCTR:00/driver'
fi

SOURCE_GENERATION=$(generation_id "$SOURCE_SOCK")
SOURCE_RANDOM=$(random_sample "$SOURCE_SOCK")
SOURCE_BOOT_ID=$(boot_id "$SOURCE_SOCK")
SOURCE_GED=$(ged_count "$SOURCE_SOCK")
SOURCE_VMGEN_RESEEDS=$(vmgen_reseed_count "$SOURCE_SOCK")
SNAPSHOT_RESPONSE=$(api "$SOURCE_SOCK" '{"op":"snapshot","diff":false}')
SNAPSHOT=$(python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("status") == "snapshot", row; print(row["path"])' <<<"$SNAPSHOT_RESPONSE")
cp --reflink=auto -- "$SNAPSHOT" "$SNAPSHOT_COPY"
# Keep the source alive while direct VMM restores seed their private disk
# overlays. The orchestrator's durable snapshot path separately owns both the
# memory and disk artifacts before it allows source deletion.

restore_clone() {
  local socket=$1 log=$2 overlay=$3 pid_var=$4
  RUST_LOG=info "$VMM" serve --socket "$socket" >"$log" 2>&1 &
  printf -v "$pid_var" '%s' "$!"
  for _ in $(seq 1 40); do [[ -S "$socket" ]] && break; sleep 0.1; done
  local restore
  restore=$(python3 - "$SNAPSHOT_COPY" "$overlay" <<'PY'
import json, sys
print(json.dumps({"op":"restore","snapshot_path":sys.argv[1],"overlay":sys.argv[2]}))
PY
)
  local restore_response
  restore_response=$(api "$socket" "$restore")
  python3 -c 'import json,sys; row=json.load(sys.stdin); assert row.get("status") == "restored", row' <<<"$restore_response"
  wait_ready "$socket"
}

restore_clone "$CLONE_A_SOCK" "$CLONE_A_LOG" "$CLONE_A_OVERLAY" CLONE_A_PID
echo "clone-a: restored and ready"
CLONE_A_GENERATION=$(generation_id "$CLONE_A_SOCK")
CLONE_A_RANDOM=$(random_sample "$CLONE_A_SOCK")
CLONE_A_BOOT_ID=$(boot_id "$CLONE_A_SOCK")
CLONE_A_GED=$(ged_count "$CLONE_A_SOCK")
CLONE_A_VMGEN_RESEEDS=$(vmgen_reseed_count "$CLONE_A_SOCK")
CLONE_A_ID=$(clone_id "$CLONE_A_SOCK")
stop_server "$CLONE_A_SOCK" "$CLONE_A_PID"
CLONE_A_PID=

restore_clone "$CLONE_B_SOCK" "$CLONE_B_LOG" "$CLONE_B_OVERLAY" CLONE_B_PID
echo "clone-b: restored and ready"
CLONE_B_GENERATION=$(generation_id "$CLONE_B_SOCK")
CLONE_B_RANDOM=$(random_sample "$CLONE_B_SOCK")
CLONE_B_BOOT_ID=$(boot_id "$CLONE_B_SOCK")
CLONE_B_GED=$(ged_count "$CLONE_B_SOCK")
CLONE_B_VMGEN_RESEEDS=$(vmgen_reseed_count "$CLONE_B_SOCK")
CLONE_B_ID=$(clone_id "$CLONE_B_SOCK")

echo "generation source=$SOURCE_GENERATION clone_a=$CLONE_A_GENERATION clone_b=$CLONE_B_GENERATION"
echo "clone_id clone_a=$CLONE_A_ID clone_b=$CLONE_B_ID"
echo "random_sha256 source=$(sample_digest "$SOURCE_RANDOM") clone_a=$(sample_digest "$CLONE_A_RANDOM") clone_b=$(sample_digest "$CLONE_B_RANDOM")"
echo "ged source=$SOURCE_GED clone_a=$CLONE_A_GED clone_b=$CLONE_B_GED"
echo "vmgen_reseeds source=$SOURCE_VMGEN_RESEEDS clone_a=$CLONE_A_VMGEN_RESEEDS clone_b=$CLONE_B_VMGEN_RESEEDS"

CLONE_A_EXPECTED_BOOT="${CLONE_A_ID:0:8}-${CLONE_A_ID:8:4}-${CLONE_A_ID:12:4}-${CLONE_A_ID:16:4}-${CLONE_A_ID:20:12}"
CLONE_B_EXPECTED_BOOT="${CLONE_B_ID:0:8}-${CLONE_B_ID:8:4}-${CLONE_B_ID:12:4}-${CLONE_B_ID:16:4}-${CLONE_B_ID:20:12}"

[[ "$SOURCE_GENERATION" != "$CLONE_A_GENERATION" ]]
[[ "$SOURCE_GENERATION" != "$CLONE_B_GENERATION" ]]
[[ "$CLONE_A_GENERATION" != "$CLONE_B_GENERATION" ]]
[[ "$CLONE_A_ID" != "$CLONE_B_ID" ]]
[[ "$SOURCE_RANDOM" != "$CLONE_A_RANDOM" ]]
[[ "$SOURCE_RANDOM" != "$CLONE_B_RANDOM" ]]
[[ "$CLONE_A_RANDOM" != "$CLONE_B_RANDOM" ]]
[[ "$CLONE_A_GED" -gt "$SOURCE_GED" ]]
[[ "$CLONE_B_GED" -gt "$SOURCE_GED" ]]
if [[ "$EXPECT_VMGENID_DRIVER" == 1 ]]; then
  [[ "$CLONE_A_VMGEN_RESEEDS" -gt "$SOURCE_VMGEN_RESEEDS" ]]
  [[ "$CLONE_B_VMGEN_RESEEDS" -gt "$SOURCE_VMGEN_RESEEDS" ]]
else
  [[ "$SOURCE_VMGEN_RESEEDS" == 0 ]]
  [[ "$CLONE_A_VMGEN_RESEEDS" == 0 ]]
  [[ "$CLONE_B_VMGEN_RESEEDS" == 0 ]]
fi
[[ "$SOURCE_BOOT_ID" != "$CLONE_A_BOOT_ID" ]]
[[ "$SOURCE_BOOT_ID" != "$CLONE_B_BOOT_ID" ]]
[[ "$CLONE_A_BOOT_ID" != "$CLONE_B_BOOT_ID" ]]
[[ "$CLONE_A_BOOT_ID" == "$CLONE_A_EXPECTED_BOOT" ]]
[[ "$CLONE_B_BOOT_ID" == "$CLONE_B_EXPECTED_BOOT" ]]

BASE_SHA_AFTER=$(sha256sum "$BASE" | awk '{print $1}')
[[ "$BASE_SHA_BEFORE" == "$BASE_SHA_AFTER" ]]
grep -q 'irqfd+routing registered: gsi=3' "$SOURCE_LOG"
grep -q 'irqfd+routing registered: gsi=3' "$CLONE_A_LOG"
grep -q 'irqfd+routing registered: gsi=3' "$CLONE_B_LOG"
if grep -Eqi 'guest panic|BUG:|KVM_RUN.*error|seccomp.*kill|VM Generation ID.*error' "$SOURCE_LOG" "$CLONE_A_LOG" "$CLONE_B_LOG"; then
  echo "unexpected VMM or guest error in test logs" >&2
  exit 1
fi

echo "VMGENID_E2E_PASS source_ged=$SOURCE_GED clone_a_ged=$CLONE_A_GED clone_b_ged=$CLONE_B_GED"
echo "VMGENID_BOOT_ID_PASS source=$SOURCE_BOOT_ID clone_a=$CLONE_A_BOOT_ID clone_b=$CLONE_B_BOOT_ID"
