#!/usr/bin/env bash
# Repeated cold-create/exec/stop gate across OCI userspaces and guest kernels.
set -Eeuo pipefail

ROOT="${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_BIN="${VMM_BIN:-$ROOT/target/release/vmm}"
UBUNTU_ROOTFS="${TARIT_OCI_UBUNTU_ROOTFS:?set TARIT_OCI_UBUNTU_ROOTFS}"
ALPINE_ROOTFS="${TARIT_OCI_ALPINE_ROOTFS:?set TARIT_OCI_ALPINE_ROOTFS}"
KERNEL_510="${TARIT_KERNEL_510:?set TARIT_KERNEL_510}"
KERNEL_66="${TARIT_KERNEL_66:?set TARIT_KERNEL_66}"
CYCLES="${BOOT_READINESS_CYCLES:-10}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"
CASE_DIR=
SERVE_PID=

cleanup() {
  if [[ -n "$SERVE_PID" ]]; then
    kill "$SERVE_PID" 2>/dev/null || true
    wait "$SERVE_PID" 2>/dev/null || true
  fi
  if [[ -n "$CASE_DIR" ]]; then
    rm -rf -- "$CASE_DIR"
  fi
}
trap cleanup EXIT

[[ "$CYCLES" =~ ^[1-9][0-9]*$ ]] || {
  echo "BOOT_READINESS_CYCLES must be a positive integer" >&2
  exit 1
}
for path in "$VMM_BIN" "$UBUNTU_ROOTFS" "$ALPINE_ROOTFS" "$KERNEL_510" "$KERNEL_66"; do
  [[ -f "$path" ]] || { echo "FAIL: required matrix input is not a file: $path" >&2; exit 1; }
done

json_field() {
  python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"
}

guest_exec() {
  local socket=$1 command=$2 response exit_code
  response=$(timeout 12 "$VMM_BIN" --socket "$socket" exec --timeout 8000 "$command") || return
  exit_code=$(json_field exit_code <<<"$response")
  [[ "$exit_code" == 0 ]] || return 1
  printf '%s\n' "$response"
}

run_case() {
  local kernel_name=$1 kernel=$2 os_id=$3 source_rootfs=$4
  local socket log rootfs marker response observed
  CASE_DIR=$(mktemp -d "$SOCKET_ROOT/tarit-boot-readiness.XXXXXX")
  chmod 0700 "$CASE_DIR"
  socket="$CASE_DIR/vmm.sock"
  log="$CASE_DIR/vmm.log"
  rootfs="$CASE_DIR/rootfs.ext4"
  cp --reflink=always --sparse=auto "$source_rootfs" "$rootfs"
  cmp --silent "$source_rootfs" "$rootfs"

  RUST_LOG=info "$VMM_BIN" serve --socket "$socket" >"$log" 2>&1 &
  SERVE_PID=$!
  for _ in $(seq 1 100); do
    [[ -S "$socket" ]] && break
    kill -0 "$SERVE_PID" 2>/dev/null || break
    sleep 0.05
  done
  [[ -S "$socket" && -e "/proc/$SERVE_PID/status" ]]

  echo "== boot readiness: kernel=$kernel_name oci=$os_id cycles=$CYCLES =="
  for cycle in $(seq 1 "$CYCLES"); do
    marker=$(wc -l <"$log")
    "$VMM_BIN" --socket "$socket" create \
      --kernel "$kernel" --rootfs "$rootfs" --mem 512 --vcpus 1 \
      --cmdline 'console=tty0 reboot=k panic=1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw init=/usr/sbin/vmm-agent' \
      >/dev/null

    observed=0
    for _ in $(seq 1 60); do
      if guest_exec "$socket" '' >/dev/null 2>&1; then
        # Expansion is intentionally performed by the guest shell.
        # shellcheck disable=SC2016
        response=$(guest_exec "$socket" 'test ! -e /dev/kvm && ! grep -Eq "(^|[[:space:]])(vmx|svm)([[:space:]]|$)" /proc/cpuinfo && . /etc/os-release && printf "%s" "$ID"') || true
        if [[ -n "$response" ]]; then
          observed=$(json_field stdout <<<"$response")
          break
        fi
      fi
      kill -0 "$SERVE_PID" 2>/dev/null || break
      sleep 1
    done
    [[ "$observed" == "$os_id" ]] || {
      echo "FAIL: boot $cycle/$CYCLES observed=${observed:-none}" >&2
      tail -n +"$marker" "$log" >&2
      return 1
    }

    "$VMM_BIN" --socket "$socket" stop >/dev/null
    if tail -n +"$marker" "$log" | grep -Eq 'terminated abnormally|threads should not terminate unexpectedly|seccomp.*(kill|violation)'; then
      echo "FAIL: boot $cycle/$CYCLES had an abnormal runtime exit" >&2
      tail -n +"$marker" "$log" >&2
      return 1
    fi
  done

  kill "$SERVE_PID" 2>/dev/null || true
  wait "$SERVE_PID" 2>/dev/null || true
  SERVE_PID=
  rm -rf -- "$CASE_DIR"
  CASE_DIR=
}

for kernel_case in "6.6:$KERNEL_66" "5.10:$KERNEL_510"; do
  kernel_name="${kernel_case%%:*}"
  kernel_path="${kernel_case#*:}"
  run_case "$kernel_name" "$kernel_path" ubuntu "$UBUNTU_ROOTFS"
  run_case "$kernel_name" "$kernel_path" alpine "$ALPINE_ROOTFS"
done

echo "PASS: $((CYCLES * 4)) cold OCI boots completed readiness, security, exec, and clean stop"
