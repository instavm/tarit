#!/usr/bin/env bash
# Real-KVM OCI compatibility matrix. Converts, admits, boots, executes, and
# removes images sequentially so the gate is reproducible on a small worker.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
TARITD="${TARITD_BIN:-$ROOT/orch/target/release/taritd}"
VMM="${TARIT_VMM_BIN:-$ROOT/vmm/target/release/vmm}"
KERNEL="${TARIT_KERNEL:?set TARIT_KERNEL to a KVM guest kernel}"
AGENT="${TARIT_VMM_AGENT:-$ROOT/vmm/guest/agent/vmm-agent}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"
KEY="oci-compatibility-e2e-key"
PORT="${OCI_COMPATIBILITY_E2E_PORT:-}"

for required in curl e2fsck python3 setsid skopeo sqlite3 umoci; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done
[ "$(id -u)" -eq 0 ] || { echo "FAIL: this jail/KVM gate must run as root" >&2; exit 1; }
test -x "$TARITD" || { echo "FAIL: taritd not executable: $TARITD" >&2; exit 1; }
test -x "$VMM" || { echo "FAIL: vmm not executable: $VMM" >&2; exit 1; }
test -x "$AGENT" || { echo "FAIL: guest agent not executable: $AGENT" >&2; exit 1; }
test -r "$KERNEL" || { echo "FAIL: kernel not readable: $KERNEL" >&2; exit 1; }
[ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ] || {
  echo "FAIL: worker /dev/kvm is unavailable" >&2
  exit 1
}
grep -Eq '\b(vmx|svm)\b' /proc/cpuinfo || {
  echo "FAIL: worker nested-virtualization feature is unavailable" >&2
  exit 1
}

if [ -z "$PORT" ]; then
  PORT=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
fi

DIR=$(mktemp -d "$SOCKET_ROOT/tarit-oci-matrix.XXXXXX")
chmod 700 "$DIR"
mkdir -m 700 "$DIR/sockets" "$DIR/runtime" "$DIR/images" "$DIR/jails"
BASE_URL="http://127.0.0.1:$PORT"
CURRENT_VM=""
CURRENT_CHILD=""
TARITD_PID=""
TARITD_PGID=""

SOCKET_PATH_PROBE="$DIR/jails/00000000-0000-0000-0000-000000000000/root/run/vmm.sock"
if [ "${#SOCKET_PATH_PROBE}" -ge 108 ]; then
  find "$DIR" -depth -delete 2>/dev/null || true
  echo "FAIL: TARIT_TEST_SOCKET_ROOT produces a ${#SOCKET_PATH_PROBE}-byte jailed Unix socket path; require <108" >&2
  exit 1
fi
printf x >"$DIR/.reflink-source"
if ! cp --reflink=always "$DIR/.reflink-source" "$DIR/.reflink-clone" 2>/dev/null; then
  find "$DIR" -depth -delete 2>/dev/null || true
  echo "FAIL: TARIT_TEST_SOCKET_ROOT must be on a reflink-capable filesystem" >&2
  exit 1
fi
rm -f -- "$DIR/.reflink-source" "$DIR/.reflink-clone"

cleanup() {
  local status=$?
  if [ -n "$CURRENT_VM" ]; then
    curl -fsS --max-time 5 -X DELETE -H "X-API-Key: $KEY" \
      "$BASE_URL/v1/vms/$CURRENT_VM" >/dev/null 2>&1 || true
  fi
  if [ -n "$CURRENT_CHILD" ]; then
    curl -fsS --max-time 5 -X DELETE -H "X-API-Key: $KEY" \
      "$BASE_URL/v1/vms/$CURRENT_CHILD" >/dev/null 2>&1 || true
  fi
  if [ -n "$TARITD_PGID" ] && kill -0 -- "-$TARITD_PGID" 2>/dev/null; then
    kill -TERM -- "-$TARITD_PGID" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 -- "-$TARITD_PGID" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL -- "-$TARITD_PGID" 2>/dev/null || true
  fi
  [ -z "$TARITD_PID" ] || wait "$TARITD_PID" 2>/dev/null || true
  if [ "$status" -ne 0 ]; then
    echo "FAIL: OCI compatibility gate exited $status" >&2
    tail -200 "$DIR/taritd.log" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ] && [ "${TARIT_E2E_KEEP_FAILED:-0}" = 1 ]; then
    echo "FAIL: retained diagnostic directory: $DIR" >&2
  else
    find "$DIR" -depth -delete 2>/dev/null || true
  fi
  mount_target=$(findmnt -n -o TARGET -T "$SOCKET_ROOT" 2>/dev/null || true)
  if [ -n "$mount_target" ] && command -v fstrim >/dev/null 2>&1; then
    sync -f "$SOCKET_ROOT" >/dev/null 2>&1 || true
    fstrim "$mount_target" >/dev/null 2>&1 || true
  fi
  return "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

api() {
  local method=$1 url=$2 body=${3:-}
  if [ -n "$body" ]; then
    curl -fsS --max-time 180 -X "$method" -H "X-API-Key: $KEY" \
      -H 'Content-Type: application/json' -d "$body" "$url"
  else
    curl -fsS --max-time 180 -X "$method" -H "X-API-Key: $KEY" "$url"
  fi
}
image_cli() {
  env \
    TARIT_DB="$DIR/fleet.db" \
    TARIT_IMAGES_DIR="$DIR/images" \
    TARIT_VMM_BIN="$VMM" \
    TARIT_VMM_AGENT="$AGENT" \
    TARIT_CONFIG="$DIR/missing.toml" \
    "$TARITD" "$@"
}
exec_guest() {
  local vm_id=$1 command=$2 output=$3
  local body
  body=$(python3 -c \
    'import json,sys; print(json.dumps({"vm_id":sys.argv[1],"command":sys.argv[2],"timeout_ms":30000}))' \
    "$vm_id" "$command")
  api POST "$BASE_URL/v1/execute" "$body" >"$output"
}
wait_exec_success() {
  local vm_id=$1 command=$2 output=$3
  for _ in $(seq 1 90); do
    if exec_guest "$vm_id" "$command" "$output" 2>/dev/null && \
      python3 - "$output" 2>/dev/null <<'PY'
import json,sys
row=json.load(open(sys.argv[1]))
assert row.get("status") == "completed", row
assert row.get("exit_code") == 0, row
PY
    then
      return 0
    fi
    sleep 1
  done
  cat "$output" >&2
  return 1
}
wait_pid_gone() {
  local pid=$1
  for _ in $(seq 1 100); do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep 0.1
  done
  return 1
}
now_ms() { python3 -c 'import time; print(time.monotonic_ns() // 1000000)'; }

TARIT_API_KEY="$KEY" \
TARIT_LISTEN="127.0.0.1:$PORT" \
TARIT_RPC_ADDR="$BASE_URL" \
TARIT_ALLOW_INSECURE_PEER_HTTP=1 \
TARIT_HOST_ID=oci-matrix-c8i \
TARIT_VMM_BIN="$VMM" \
TARIT_KERNEL="$KERNEL" \
TARIT_ROOTFS=/bin/true \
TARIT_ROOTFS_READONLY=0 \
TARIT_VMM_AGENT="$AGENT" \
TARIT_ENABLE_NET=0 \
TARIT_SOCKET_DIR="$DIR/sockets" \
TARIT_IMAGES_DIR="$DIR/images" \
TARIT_DB="$DIR/fleet.db" \
TARIT_CONFIG="$DIR/missing.toml" \
TARIT_WARM_POOL=0 \
TARIT_MAX_VMS=2 \
TARIT_MAX_VCPUS=2 \
TARIT_MAX_MEMORY_MIB=1024 \
TARIT_VM_JAIL_BASE="$DIR/jails" \
TARIT_VM_JAIL_UID_BASE=280000 \
TARIT_VM_JAIL_GID_BASE=290000 \
TARIT_VM_JAIL_ID_COUNT=2 \
TARIT_VM_JAIL_SECCOMP=1 \
TARIT_VM_JAIL_PID_NAMESPACE=1 \
TARIT_VM_JAIL_NETWORK_NAMESPACE=1 \
TARIT_REAP_ON_SHUTDOWN=true \
TARIT_PRODUCTION=0 \
RUST_LOG=taritd=info,vmm_core=info \
TMPDIR="$DIR/runtime" \
setsid "$TARITD" serve >"$DIR/taritd.log" 2>&1 &
TARITD_PID=$!
TARITD_PGID=$TARITD_PID

for _ in $(seq 1 100); do
  curl -fsS --max-time 1 "$BASE_URL/health" >/dev/null 2>&1 && break
  kill -0 "$TARITD_PID" 2>/dev/null || { tail -120 "$DIR/taritd.log"; exit 1; }
  sleep 0.2
done
curl -fsS "$BASE_URL/health" >/dev/null

# name|OCI ref|expected /etc/os-release ID|VERSION_ID prefix|shell expectation
MATRIX=${OCI_COMPATIBILITY_MATRIX:-$(cat <<'EOF'
ubuntu2404|ubuntu:24.04|ubuntu|24.04|shell
debian12|debian:12-slim|debian|12|shell
alpine320|alpine:3.20|alpine|3.20|shell
busybox136|busybox:1.36|||shell
rocky9|rockylinux:9-minimal|rocky|9|shell
fedora42|registry.fedoraproject.org/fedora-minimal:42|fedora|42|shell
distroless12|gcr.io/distroless/static-debian12:nonroot|||no-shell
EOF
)}

CASES=0
SHELL_CASES=0
NO_SHELL_CASES=0
while IFS='|' read -r name oci_ref expected_id expected_version expectation; do
  [ -n "$name" ] || continue
  CASES=$((CASES + 1))
  echo "== OCI case $name ($oci_ref, $expectation) =="
  BUILD_START=$(now_ms)
  image_cli image build --oci "$oci_ref" --name "$name" >"$DIR/build-$name.log" 2>&1
  BUILD_END=$(now_ms)
  image_cli image verify "$name" >"$DIR/verify-$name.log"
  image_cli --json image ls >"$DIR/images-$name.json"
  ROOTFS_PATH=$(python3 - "$DIR/images-$name.json" "$name" "$oci_ref" <<'PY'
import json,re,sys
rows=json.load(open(sys.argv[1]))
row=next((candidate for candidate in rows if candidate["name"] == sys.argv[2]), None)
assert row is not None, rows
sha=re.compile(r"^sha256:[0-9a-f]{64}$")
for field in ("source_digest", "rootfs_digest", "agent_digest"):
    assert sha.fullmatch(row.get(field) or ""), (field,row)
assert "@sha256:" in row["source_ref"] and row["source_ref"].endswith(row["source_digest"]), row
assert row["size_bytes"] == 1073741824, row
print(row["rootfs_path"])
PY
)
  test -f "$ROOTFS_PATH"
  test "$(stat -c '%a' "$ROOTFS_PATH")" = 444
  e2fsck -fn "$ROOTFS_PATH" >"$DIR/e2fsck-$name.log" 2>&1

  CREATE_BODY=$(python3 -c \
    'import json,sys; print(json.dumps({"image":sys.argv[1],"vcpus":1,"memory_mib":256}))' \
    "$name")
  BOOT_START=$(now_ms)
  CURRENT_VM=$(api POST "$BASE_URL/v1/vms" "$CREATE_BODY" | \
    python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["status"] == "running", row; print(row["id"])')

  VM_PID=$(python3 - "$DIR/fleet.db" "$CURRENT_VM" <<'PY'
import sqlite3,sys
with sqlite3.connect(sys.argv[1]) as db:
    row=db.execute("select pid from vms where id=?", (sys.argv[2],)).fetchone()
assert row and row[0], row
print(row[0])
PY
)
  kill -0 "$VM_PID"
  case "$(ps -o euid= -p "$VM_PID" | tr -d ' ')" in
    280000) ;;
    *) echo "FAIL: $name VMM is not running as the allocated jail UID" >&2; exit 1 ;;
  esac

  if [ "$expectation" = shell ]; then
    SHELL_CASES=$((SHELL_CASES + 1))
    IDENTITY_COMMAND="set -eu; test -b /dev/vda; test ! -e /dev/kvm; ! grep -Eq '(^|[[:space:]])(vmx|svm)([[:space:]]|$)' /proc/cpuinfo; printf 'PID1_EXE='; readlink /proc/1/exe; printf 'tarit-oci-ok\\n'; if [ -r /etc/os-release ]; then grep -E '^(ID|VERSION_ID)=' /etc/os-release; fi"
    wait_exec_success "$CURRENT_VM" "$IDENTITY_COMMAND" "$DIR/exec-$name.json"
    BOOT_END=$(now_ms)
    python3 - "$DIR/exec-$name.json" "$expected_id" "$expected_version" <<'PY'
import json,re,sys
row=json.load(open(sys.argv[1])); out=row.get("stdout", "")
assert "tarit-oci-ok" in out, row
pid1=next((line.removeprefix("PID1_EXE=") for line in out.splitlines() if line.startswith("PID1_EXE=")), None)
assert pid1 in {"/usr/sbin/vmm-agent", "/usr/bin/vmm-agent"}, (pid1,row)
expected_id, expected_version=sys.argv[2:]
if expected_id:
    ids=dict(match.groups() for match in re.finditer(r'^(ID|VERSION_ID)=["\x27]?([^"\x27\n]+)', out, re.M))
    assert ids.get("ID") == expected_id, (ids,row)
    assert ids.get("VERSION_ID", "").startswith(expected_version), (ids,row)
PY

    # The primary Ubuntu workload and a minimal musl workload both exercise
    # live fork, private disk CoW, clone identity, and scale-to-zero resume.
    if [ "$name" = ubuntu2404 ] || [ "$name" = alpine320 ]; then
      wait_exec_success "$CURRENT_VM" \
        "mkdir -p /usr/libexec/tarit /run/tarit; printf '%s\\n' '#!/bin/sh' 'set -eu' 'test \"\$TARIT_POST_FORK\" = 1' 'test \"\${#TARIT_CLONE_ID}\" -eq 32' 'printf \"%s\\n\" \"\$TARIT_CLONE_ID\" > /run/tarit/hook-observed' 'rm -f /run/tarit/cached-token' 'printf \"repaired:%s\\n\" \"\$TARIT_CLONE_ID\" > /run/tarit/userspace-token' > /usr/libexec/tarit/post-fork; chmod 0755 /usr/libexec/tarit/post-fork; printf cloned-token > /run/tarit/cached-token" \
        "$DIR/hook-setup-$name.json"
      wait_exec_success "$CURRENT_VM" \
        "printf source-before-fork > /root/tarit-oci-fork-state; sync" \
        "$DIR/fork-seed-$name.json"
      exec_guest "$CURRENT_VM" 'cat /proc/sys/kernel/random/boot_id' \
        "$DIR/source-boot-id-$name.json"
      SOURCE_BOOT_ID=$(python3 - "$DIR/source-boot-id-$name.json" <<'PY'
import json,sys
row=json.load(open(sys.argv[1])); assert row.get("exit_code") == 0, row
print(row.get("stdout", "").strip())
PY
)
      FORK_START=$(now_ms)
      api POST "$BASE_URL/v1/vms/$CURRENT_VM/fork" '{}' >"$DIR/fork-$name.json"
      FORK_END=$(now_ms)
      CURRENT_CHILD=$(python3 - "$DIR/fork-$name.json" <<'PY'
import json,sys
row=json.load(open(sys.argv[1])); assert row.get("vm", {}).get("status") == "running", row
print(row["vm"]["id"])
PY
)
      wait_exec_success "$CURRENT_CHILD" \
        'grep -qx source-before-fork /root/tarit-oci-fork-state && test ! -e /run/tarit/cached-token && cmp -s /run/tarit/hook-observed /run/tarit/clone-id' \
        "$DIR/fork-ready-$name.json"
      wait_exec_success "$CURRENT_VM" \
        'grep -qx cloned-token /run/tarit/cached-token' \
        "$DIR/fork-parent-token-$name.json"
      wait_exec_success "$CURRENT_VM" \
        "printf parent-after-fork > /root/tarit-oci-fork-state" \
        "$DIR/fork-parent-write-$name.json"
      wait_exec_success "$CURRENT_CHILD" \
        "printf child-after-fork > /root/tarit-oci-fork-state" \
        "$DIR/fork-child-write-$name.json"
      wait_exec_success "$CURRENT_VM" \
        'grep -qx parent-after-fork /root/tarit-oci-fork-state' \
        "$DIR/fork-parent-isolation-$name.json"
      wait_exec_success "$CURRENT_CHILD" \
        'grep -qx child-after-fork /root/tarit-oci-fork-state' \
        "$DIR/fork-child-isolation-$name.json"
      exec_guest "$CURRENT_CHILD" \
        'cat /proc/sys/kernel/random/boot_id /run/tarit/clone-id /run/tarit/hook-observed /run/tarit/userspace-token' \
        "$DIR/fork-identity-$name.json"
      python3 - "$DIR/fork-identity-$name.json" "$SOURCE_BOOT_ID" <<'PY'
import json,sys,uuid
row=json.load(open(sys.argv[1])); assert row.get("exit_code") == 0, row
boot_id, clone_id, hook_id, token=row.get("stdout", "").splitlines()
uuid.UUID(boot_id)
assert boot_id != sys.argv[2], (boot_id, sys.argv[2])
assert boot_id.replace("-", "") == clone_id, (boot_id, clone_id)
assert hook_id == clone_id and token == f"repaired:{clone_id}", (hook_id, token, clone_id)
PY
      CHILD_PID=$(python3 - "$DIR/fleet.db" "$CURRENT_CHILD" <<'PY'
import sqlite3,sys
with sqlite3.connect(sys.argv[1]) as db:
    row=db.execute("select pid from vms where id=?", (sys.argv[2],)).fetchone()
assert row and row[0], row
print(row[0])
PY
)
      api DELETE "$BASE_URL/v1/vms/$CURRENT_CHILD" >/dev/null
      CURRENT_CHILD=""
      wait_pid_gone "$CHILD_PID"

      wait_exec_success "$CURRENT_VM" 'printf resume-cloned-token > /run/tarit/cached-token' \
        "$DIR/resume-token-seed-$name.json"
      api POST "$BASE_URL/v1/vms/$CURRENT_VM/hibernate" '{}' >"$DIR/hibernate-$name.json"
      python3 - "$DIR/hibernate-$name.json" <<'PY'
import json,sys
row=json.load(open(sys.argv[1])); assert row["status"] == "hibernated", row
PY
      wait_pid_gone "$VM_PID"
      RESUME_START=$(now_ms)
      wait_exec_success "$CURRENT_VM" "echo $name-http-resume" "$DIR/resume-$name.json"
      RESUME_END=$(now_ms)
      grep -q http-resume "$DIR/resume-$name.json"
      wait_exec_success "$CURRENT_VM" \
        'test ! -e /run/tarit/cached-token && cmp -s /run/tarit/hook-observed /run/tarit/clone-id' \
        "$DIR/resume-hook-$name.json"
      exec_guest "$CURRENT_VM" 'cat /proc/sys/kernel/random/boot_id' \
        "$DIR/resume-boot-id-$name.json"
      python3 - "$DIR/resume-boot-id-$name.json" "$SOURCE_BOOT_ID" <<'PY'
import json,sys,uuid
row=json.load(open(sys.argv[1])); assert row.get("exit_code") == 0, row
boot_id=row.get("stdout", "").strip(); uuid.UUID(boot_id)
assert boot_id != sys.argv[2], (boot_id, sys.argv[2])
PY
      VM_PID=$(python3 - "$DIR/fleet.db" "$CURRENT_VM" <<'PY'
import sqlite3,sys
with sqlite3.connect(sys.argv[1]) as db:
    row=db.execute("select pid from vms where id=?", (sys.argv[2],)).fetchone()
assert row and row[0], row
print(row[0])
PY
)
      echo "   live_fork_ready_ms=$((FORK_END-FORK_START)) http_resume_ms=$((RESUME_END-RESUME_START))"

      if [ "$name" = ubuntu2404 ]; then
        wait_exec_success "$CURRENT_VM" \
          "printf '%s\\n' '#!/bin/sh' 'exit 42' > /usr/libexec/tarit/post-fork; chmod 0755 /usr/libexec/tarit/post-fork" \
          "$DIR/failing-hook-setup-$name.json"
        NEGATIVE_STATUS=$(curl -sS --max-time 90 -o "$DIR/failing-hook-fork-$name.json" \
          -w '%{http_code}' -X POST -H "X-API-Key: $KEY" \
          -H 'Content-Type: application/json' -d '{}' "$BASE_URL/v1/vms/$CURRENT_VM/fork")
        [ "$NEGATIVE_STATUS" != 201 ] || {
          echo "FAIL: failing post-fork hook admitted a child" >&2
          exit 1
        }
        wait_exec_success "$CURRENT_VM" 'true' "$DIR/failing-hook-parent-$name.json"
        RUNNING_COUNT=$(sqlite3 "$DIR/fleet.db" "select count(*) from vms where status='running';")
        [ "$RUNNING_COUNT" -eq 1 ] || {
          echo "FAIL: failing post-fork hook left $RUNNING_COUNT running VM records" >&2
          exit 1
        }
      fi
    fi
  else
    NO_SHELL_CASES=$((NO_SHELL_CASES + 1))
    # Distroless has no /bin/sh by design. The VM and agent must be healthy;
    # command execution must complete deterministically with POSIX 127.
    for _ in $(seq 1 90); do
      exec_guest "$CURRENT_VM" "echo must-not-run" "$DIR/exec-$name.json" 2>/dev/null && \
        python3 - "$DIR/exec-$name.json" <<'PY' && break
import json,sys
row=json.load(open(sys.argv[1]))
assert row.get("status") == "completed", row
assert row.get("exit_code") == 127, row
assert not row.get("error"), row
PY
      sleep 1
    done
    python3 - "$DIR/exec-$name.json" <<'PY'
import json,sys
row=json.load(open(sys.argv[1]))
assert row.get("status") == "completed" and row.get("exit_code") == 127, row
PY
    BOOT_END=$(now_ms)
  fi

  DELETED_PID=$VM_PID
  api DELETE "$BASE_URL/v1/vms/$CURRENT_VM" >/dev/null
  CURRENT_VM=""
  wait_pid_gone "$DELETED_PID"
  image_cli image rm "$name" >"$DIR/remove-$name.log"
  test ! -e "$ROOTFS_PATH"
  image_cli --json image ls | python3 -c \
    'import json,sys; name=sys.argv[1]; assert all(row["name"] != name for row in json.load(sys.stdin))' "$name"
  if find "$DIR/images" -maxdepth 1 \( -name '.build-*.ext4' -o -name '.tarit-oci-*' \) -print -quit | grep -q .; then
    echo "FAIL: $name left an OCI build workspace" >&2
    exit 1
  fi
  echo "   build_ms=$((BUILD_END-BUILD_START)) boot_ready_ms=$((BOOT_END-BOOT_START))"
done <<<"$MATRIX"

test "$CASES" -gt 0
test "$(image_cli --json image ls | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')" -eq 0
test "$(find "$DIR/images" -mindepth 1 -maxdepth 1 -print | wc -l)" -eq 0
[ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]
grep -Eq '\b(vmx|svm)\b' /proc/cpuinfo
if command -v systemctl >/dev/null && systemctl list-unit-files postgresql.service >/dev/null 2>&1; then
  systemctl is-active --quiet postgresql
fi

echo "OCI_COMPATIBILITY_PASS cases=$CASES shell_images=$SHELL_CASES expected_no_shell=$NO_SHELL_CASES"
