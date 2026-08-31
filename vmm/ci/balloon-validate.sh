#!/usr/bin/env bash
# Real-KVM virtio-balloon, snapshot, lazy-restore, and guest-security gate.
set -Eeuo pipefail

VMM_BIN=${VMM_BIN:?set VMM_BIN}
KERNEL=${KERNEL:?set KERNEL}
ROOTFS=${ROOTFS:?set ROOTFS to an OCI-derived ext4 image with the Tarit agent}
SOCKET=${SOCKET:-/tmp/tarit-balloon-e2e.sock}
LOG=${LOG:-/tmp/tarit-balloon-e2e.log}
RESTORE_CYCLES=${BALLOON_RESTORE_CYCLES:-1}
CGROUP_ENFORCE=${BALLOON_CGROUP_ENFORCE:-0}
CGROUP_MAX_BYTES=${BALLOON_CGROUP_MAX_BYTES:-1610612736}
CGROUP_HIGH_BYTES=${BALLOON_CGROUP_HIGH_BYTES:-335544320}
SNAPSHOT_TIMEOUT_SECS=${BALLOON_SNAPSHOT_TIMEOUT_SECS:-120}
CONTROL_TIMEOUT_SECS=${BALLOON_CONTROL_TIMEOUT_SECS:-35}
MIN_RECLAIM_KIB=${BALLOON_MIN_RECLAIM_KIB:-8192}
CACHE_MIB=${BALLOON_CACHE_MIB:-32}
RESIDENT_FREE_MIB=${BALLOON_RESIDENT_FREE_MIB:-128}
ACTIVE_TARGET_MIB=${BALLOON_ACTIVE_TARGET_MIB:-16}
MIN_ACTUAL_MIB=${BALLOON_MIN_ACTUAL_MIB:-8}
SNAPSHOT=
INTEGRITY_SIDECAR=
SERVE_PID=
CGROUP_DIR=
CGROUP_HIGH_BEFORE=0

on_error() {
  local status=$?
  trap - ERR
  echo "BALLOON_E2E_FAIL line=$1 status=$status log=$LOG" >&2
  tail -n 120 "$LOG" >&2 2>/dev/null || true
  exit "$status"
}
trap 'on_error $LINENO' ERR

[[ "$RESTORE_CYCLES" =~ ^[1-9][0-9]*$ ]] || {
  echo "BALLOON_RESTORE_CYCLES must be a positive integer" >&2
  exit 1
}
[[ "$CGROUP_ENFORCE" == 0 || "$CGROUP_ENFORCE" == 1 ]] || {
  echo "BALLOON_CGROUP_ENFORCE must be 0 or 1" >&2
  exit 1
}
[[ "$SNAPSHOT_TIMEOUT_SECS" =~ ^[1-9][0-9]*$ ]] || {
  echo "BALLOON_SNAPSHOT_TIMEOUT_SECS must be a positive integer" >&2
  exit 1
}
[[ "$CONTROL_TIMEOUT_SECS" =~ ^[1-9][0-9]*$ ]] || {
  echo "BALLOON_CONTROL_TIMEOUT_SECS must be a positive integer" >&2
  exit 1
}
[[ "$CGROUP_MAX_BYTES" =~ ^[1-9][0-9]*$ && "$CGROUP_HIGH_BYTES" =~ ^[1-9][0-9]*$ ]] || {
  echo "balloon cgroup byte limits must be positive integers" >&2
  exit 1
}
[[ "$MIN_RECLAIM_KIB" =~ ^[1-9][0-9]*$ ]] || {
  echo "BALLOON_MIN_RECLAIM_KIB must be a positive integer" >&2
  exit 1
}
[[ "$CACHE_MIB" =~ ^[1-9][0-9]*$ && "$RESIDENT_FREE_MIB" =~ ^[1-9][0-9]*$ && "$ACTIVE_TARGET_MIB" =~ ^[1-9][0-9]*$ && "$MIN_ACTUAL_MIB" =~ ^[1-9][0-9]*$ ]] || {
  echo "balloon workload sizes must be positive integers" >&2
  exit 1
}
(( CACHE_MIB >= 16 && CACHE_MIB <= 256 && RESIDENT_FREE_MIB >= 64 && RESIDENT_FREE_MIB <= 320 && CACHE_MIB + RESIDENT_FREE_MIB <= 320 && ACTIVE_TARGET_MIB <= 256 && MIN_ACTUAL_MIB <= ACTIVE_TARGET_MIB )) || {
  echo "balloon workload sizes are outside the safe 512 MiB guest envelope" >&2
  exit 1
}
(( CGROUP_HIGH_BYTES < CGROUP_MAX_BYTES )) || {
  echo "BALLOON_CGROUP_HIGH_BYTES must be below BALLOON_CGROUP_MAX_BYTES" >&2
  exit 1
}

cleanup() {
  if [[ -n "$SERVE_PID" ]]; then
    kill "$SERVE_PID" 2>/dev/null || true
    for _ in $(seq 1 20); do
      kill -0 "$SERVE_PID" 2>/dev/null || break
      sleep 0.1
    done
    if kill -0 "$SERVE_PID" 2>/dev/null; then
      kill -KILL "$SERVE_PID" 2>/dev/null || true
    fi
    wait "$SERVE_PID" 2>/dev/null || true
  fi
  rm -f -- "$SOCKET"
  if [[ -n "$SNAPSHOT" ]]; then
    rm -f -- "$SNAPSHOT"
  fi
  if [[ -n "$INTEGRITY_SIDECAR" ]]; then
    rm -f -- "$INTEGRITY_SIDECAR"
  fi
  if [[ -n "$CGROUP_DIR" ]]; then
    rmdir -- "$CGROUP_DIR" 2>/dev/null || true
  fi
}
trap cleanup EXIT

json_field() {
  local field=$1
  python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$field"
}

guest_exec() {
  local response exit_code stderr
  response=$(timeout "$CONTROL_TIMEOUT_SECS" "$VMM_BIN" --socket "$SOCKET" exec --timeout 30000 "$1") || return
  exit_code=$(json_field exit_code <<<"$response")
  if [[ "$exit_code" != 0 ]]; then
    stderr=$(json_field stderr <<<"$response")
    echo "guest command failed with exit $exit_code: $stderr" >&2
    return 1
  fi
  printf '%s\n' "$response"
}

wait_for_exec() {
  local deadline=$((SECONDS + 60))
  while (( SECONDS < deadline )); do
    if guest_exec '' >/dev/null 2>&1 && guest_exec 'printf ready' >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "guest agent did not become ready" >&2
  return 1
}

wait_for_actual_at_least() {
  local minimum=$1 deadline=$((SECONDS + 60)) state actual
  while (( SECONDS < deadline )); do
    state=$("$VMM_BIN" --socket "$SOCKET" balloon)
    actual=$(json_field actual_mib <<<"$state")
    if (( actual >= minimum )); then
      printf '%s\n' "$state"
      return 0
    fi
    sleep 1
  done
  echo "balloon actual size did not reach ${minimum} MiB; final_state=$state" >&2
  return 1
}

wait_for_actual_zero() {
  local deadline=$((SECONDS + 60)) state actual
  while (( SECONDS < deadline )); do
    state=$("$VMM_BIN" --socket "$SOCKET" balloon)
    actual=$(json_field actual_mib <<<"$state")
    if (( actual == 0 )); then
      return 0
    fi
    sleep 1
  done
  echo "balloon did not deflate to zero; final_state=$state" >&2
  return 1
}

assert_guest_kernel_live() {
  local result
  result=$(guest_exec 'if dmesg | grep -Eiq "rcu[^[:alnum:]]+.*stall|soft lockup|hard LOCKUP|kernel panic|BUG: unable to handle|general protection fault|virtio_balloon.*(error|fail)"; then dmesg | tail -n 120; exit 1; else printf KERNEL_LIVE; fi')
  grep -q KERNEL_LIVE <<<"$result"
}

rm -f -- "$SOCKET" "$LOG"
"$VMM_BIN" --socket "$SOCKET" serve >"$LOG" 2>&1 &
SERVE_PID=$!
for _ in $(seq 1 100); do
  [[ -S "$SOCKET" ]] && break
  sleep 0.05
done
[[ -S "$SOCKET" ]]

if [[ "$CGROUP_ENFORCE" == 1 ]]; then
  [[ $(id -u) -eq 0 ]] || { echo "cgroup balloon gate must run as root" >&2; exit 1; }
  [[ -f /sys/fs/cgroup/cgroup.controllers ]] || {
    echo "cgroup v2 is required for the balloon pressure gate" >&2
    exit 1
  }
  grep -qw memory /sys/fs/cgroup/cgroup.controllers || {
    echo "cgroup v2 memory controller is unavailable" >&2
    exit 1
  }
  CGROUP_DIR="/sys/fs/cgroup/tarit-balloon-e2e-$SERVE_PID"
  mkdir -m 0755 -- "$CGROUP_DIR"
  printf '%s\n' "$CGROUP_MAX_BYTES" >"$CGROUP_DIR/memory.max"
  printf '%s\n' "$CGROUP_MAX_BYTES" >"$CGROUP_DIR/memory.high"
  printf '%s\n' "$SERVE_PID" >"$CGROUP_DIR/cgroup.procs"
  [[ $(<"$CGROUP_DIR/memory.max") == "$CGROUP_MAX_BYTES" ]]
  [[ $(<"$CGROUP_DIR/memory.high") == "$CGROUP_MAX_BYTES" ]]
fi

# Keep ttyS0 dedicated to the fallback agent in this device test. Production
# prefers vsock; sharing the test UART with a Linux console would add unrelated
# line-discipline races to the balloon/RSS assertions.
CMDLINE='console=tty0 reboot=k panic=1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw init=/usr/sbin/vmm-agent'
"$VMM_BIN" --socket "$SOCKET" create \
  --kernel "$KERNEL" --rootfs "$ROOTFS" --mem 512 --vcpus 1 --cmdline "$CMDLINE" >/dev/null
wait_for_exec

# Worker KVM remains available, but nested virtualization and /dev/kvm must be
# absent from the customer Ubuntu guest.
security=$(guest_exec 'test ! -e /dev/kvm && ! grep -Eq "(^|[[:space:]])(vmx|svm)([[:space:]]|$)" /proc/cpuinfo && find /sys/bus/virtio/drivers/virtio_balloon -maxdepth 1 -type l | grep -q . && printf SECURITY_OK')
grep -q SECURITY_OK <<<"$security"
echo "balloon-e2e: guest driver and nested-virtualization isolation verified"

# Reject a target beyond configured RAM at the VMM boundary.
if "$VMM_BIN" --socket "$SOCKET" balloon --target 513 >/dev/null 2>&1; then
  echo "oversized balloon target unexpectedly succeeded" >&2
  exit 1
fi

# Populate reclaimable resident page cache. Ballooning untouched free guest
# pages would change `actual` without reducing host RSS, which is not evidence
# of useful reclaim. Preserve its hash across inflate/deflate and lazy restore.
cache_json=$(guest_exec "dd if=/dev/zero of=/tmp/balloon-cache bs=1M count=$CACHE_MIB status=none; sync; sha256sum /tmp/balloon-cache")
cache_hash_before=$(json_field stdout <<<"$cache_json" | awk '{print $1}')
[[ "$cache_hash_before" =~ ^[0-9a-f]{64}$ ]]
cache_touch='nohup setsid sh -c '\''dd if=/tmp/balloon-cache of=/dev/null bs=1M status=none; printf 1 >/tmp/balloon-cache-ready; sleep 120'\'' </dev/null >/tmp/balloon-cache-touch.log 2>&1 & echo $! >/tmp/balloon-cache-pid'
guest_exec "$cache_touch" >/dev/null
for _ in $(seq 1 60); do
  cache_ready=$(guest_exec 'test -f /tmp/balloon-cache-ready && printf READY || true')
  grep -q READY <<<"$cache_ready" && break
  sleep 0.1
done
grep -q READY <<<"$cache_ready"
# Expansion is intentionally performed by the guest shell.
# shellcheck disable=SC2016
guest_exec 'kill "$(cat /tmp/balloon-cache-pid)"; sleep .2' >/dev/null

# Make free guest pages provably resident on the host. A tmpfs write faults
# anonymous guest RAM; deleting and unmounting it returns those pages to the
# guest allocator without any host-side discard hint. Balloon inflation must
# then discard a material portion of that resident memory.
guest_exec "set -e; mkdir -p /tmp/balloon-resident; mount -t tmpfs -o size=${RESIDENT_FREE_MIB}m tmpfs /tmp/balloon-resident; dd if=/dev/zero of=/tmp/balloon-resident/pages bs=1M count=$RESIDENT_FREE_MIB status=none; sync; rm -f /tmp/balloon-resident/pages; umount /tmp/balloon-resident; sync; echo 3 >/proc/sys/vm/drop_caches" >/dev/null
mem_free_json=$(guest_exec "awk '/MemFree:/ { print \$2 }' /proc/meminfo")
mem_free_kib=$(json_field stdout <<<"$mem_free_json" | tr -cd '0-9')
if [[ ! "$mem_free_kib" =~ ^[0-9]+$ ]] || (( mem_free_kib < 65536 )); then
  echo "guest has insufficient free memory before balloon: ${mem_free_kib:-unknown} KiB" >&2
  exit 1
fi

rss_before=$(awk '/^VmRSS:/ {print $2}' "/proc/$SERVE_PID/status")
"$VMM_BIN" --socket "$SOCKET" balloon --target "$ACTIVE_TARGET_MIB" >/dev/null
# Linux ballooning is explicitly best-effort; require a material amount rather
# than pretending `num_pages` is a hard allocation guarantee.
wait_for_actual_at_least "$MIN_ACTUAL_MIB" >/dev/null
rss_after=$rss_before
for _ in $(seq 1 25); do
  rss_sample=$(awk '/^VmRSS:/ {print $2}' "/proc/$SERVE_PID/status")
  (( rss_sample < rss_after )) && rss_after=$rss_sample
  sleep 0.2
done
reclaimed_kib=$((rss_before - rss_after))
if (( reclaimed_kib < MIN_RECLAIM_KIB )); then
  echo "balloon reclaimed only ${reclaimed_kib} KiB (before=${rss_before}, after=${rss_after})" >&2
  exit 1
fi
echo "balloon-e2e: reclaimed ${reclaimed_kib} KiB of host RSS"

"$VMM_BIN" --socket "$SOCKET" balloon --target 0 >/dev/null
wait_for_actual_zero
cache_json=$(guest_exec 'sha256sum /tmp/balloon-cache')
cache_hash_after=$(json_field stdout <<<"$cache_json" | awk '{print $1}')
[[ "$cache_hash_after" == "$cache_hash_before" ]]
echo "balloon-e2e: guest data survived inflate/deflate"

if [[ "$CGROUP_ENFORCE" == 1 ]]; then
  # Discard guest pages, return them to the guest, and arm a delayed writer
  # before lowering memory.high. The writer creates a new charge after the
  # boundary is armed, while the host observes and removes the throttle without
  # waiting for a synchronous VMM control response from inside that throttle.
  "$VMM_BIN" --socket "$SOCKET" balloon --target "$ACTIVE_TARGET_MIB" >/dev/null
  wait_for_actual_at_least "$MIN_ACTUAL_MIB" >/dev/null
  "$VMM_BIN" --socket "$SOCKET" balloon --target 0 >/dev/null
  wait_for_actual_zero
  pressure_writer='rm -f /tmp/cgroup-pressure /tmp/cgroup-pressure-done; nohup setsid sh -c '\''sleep 2; dd if=/dev/zero of=/tmp/cgroup-pressure bs=1M count=32 status=none; sync; printf done > /tmp/cgroup-pressure-done'\'' </dev/null >/tmp/cgroup-pressure.log 2>&1 &'
  guest_exec "$pressure_writer" >/dev/null
  cgroup_current=$(<"$CGROUP_DIR/memory.current")
  pressure_high=$CGROUP_HIGH_BYTES
  if (( pressure_high >= cgroup_current )); then
    (( cgroup_current > 33554432 )) || {
      echo "guest working set is too small for the cgroup pressure gate" >&2
      exit 1
    }
    pressure_high=$((cgroup_current - 8388608))
  fi
  printf '%s\n' "$pressure_high" >"$CGROUP_DIR/memory.high"
  CGROUP_HIGH_BEFORE=$(awk '$1 == "high" { print $2 }' "$CGROUP_DIR/memory.events")
  for _ in $(seq 1 100); do
    cgroup_high_observed=$(awk '$1 == "high" { print $2 }' "$CGROUP_DIR/memory.events")
    (( cgroup_high_observed > CGROUP_HIGH_BEFORE )) && break
    sleep 0.1
  done
  cgroup_high_observed=$(awk '$1 == "high" { print $2 }' "$CGROUP_DIR/memory.events")
  (( cgroup_high_observed > CGROUP_HIGH_BEFORE )) || {
    echo "cgroup memory.high did not observe deterministic guest-memory pressure" >&2
    exit 1
  }
  # Pressure has been proven. Keep the hard memory.max boundary in force but
  # stop throttling below the guest's irreducible working set before snapshot.
  printf '%s\n' "$CGROUP_MAX_BYTES" >"$CGROUP_DIR/memory.high"
  # Expansion is intentionally performed by the guest shell.
  # shellcheck disable=SC2016
  pressure_done=$(guest_exec 'for i in $(seq 1 200); do test -f /tmp/cgroup-pressure-done && break; sleep .05; done; cat /tmp/cgroup-pressure-done; rm -f /tmp/cgroup-pressure /tmp/cgroup-pressure-done; sync')
  grep -q 'done' <<<"$pressure_done"
  "$VMM_BIN" --socket "$SOCKET" balloon --target "$ACTIVE_TARGET_MIB" >/dev/null
  wait_for_actual_at_least "$MIN_ACTUAL_MIB" >/dev/null
  "$VMM_BIN" --socket "$SOCKET" balloon --target 0 >/dev/null
  wait_for_actual_zero
  pressure_hash=$(guest_exec 'sha256sum /tmp/balloon-cache')
  pressure_hash_value=$(json_field stdout <<<"$pressure_hash" | awk '{print $1}')
  [[ "$pressure_hash_value" == "$cache_hash_before" ]]
fi

# Snapshot while inflated, restore lazily, and prove target/device/guest state
# survive without resurrecting discarded memory or exposing nested KVM.
"$VMM_BIN" --socket "$SOCKET" balloon --target 8 >/dev/null
wait_for_actual_at_least 4 >/dev/null
snapshot_json=$(timeout "$SNAPSHOT_TIMEOUT_SECS" "$VMM_BIN" --socket "$SOCKET" snapshot --live)
SNAPSHOT=$(json_field path <<<"$snapshot_json")
INTEGRITY_SIDECAR=$(json_field integrity_path <<<"$snapshot_json")
[[ -s "$SNAPSHOT" ]]
[[ -s "$INTEGRITY_SIDECAR" ]]
"$VMM_BIN" --socket "$SOCKET" stop >/dev/null
for cycle in $(seq 1 "$RESTORE_CYCLES"); do
  timeout "$SNAPSHOT_TIMEOUT_SECS" "$VMM_BIN" --socket "$SOCKET" restore --snapshot "$SNAPSHOT" --memory-policy lazy >/dev/null
  wait_for_exec
  restored=$("$VMM_BIN" --socket "$SOCKET" balloon)
  [[ $(json_field target_mib <<<"$restored") == 8 ]]
  post_restore=$(guest_exec 'test ! -e /dev/kvm && ! grep -Eq "(^|[[:space:]])(vmx|svm)([[:space:]]|$)" /proc/cpuinfo && sha256sum /tmp/balloon-cache')
  grep -q "$cache_hash_before" <<<"$post_restore"

  "$VMM_BIN" --socket "$SOCKET" balloon --target "$ACTIVE_TARGET_MIB" >/dev/null
  wait_for_actual_at_least "$MIN_ACTUAL_MIB" >/dev/null
  "$VMM_BIN" --socket "$SOCKET" balloon --target 0 >/dev/null
  wait_for_actual_zero
  assert_guest_kernel_live
  guest_exec 'printf balloon-liveness' | grep -q balloon-liveness

  if (( cycle < RESTORE_CYCLES )); then
    "$VMM_BIN" --socket "$SOCKET" stop >/dev/null
  fi
done
echo "balloon-e2e: $RESTORE_CYCLES lazy restore/inflate/deflate cycles preserved guest state and kernel liveness"

cgroup_high_events=0
cgroup_peak_bytes=0
if [[ "$CGROUP_ENFORCE" == 1 ]]; then
  cgroup_high_events=$(awk '$1 == "high" { print $2 }' "$CGROUP_DIR/memory.events")
  cgroup_oom_kills=$(awk '$1 == "oom_kill" { print $2 }' "$CGROUP_DIR/memory.events")
  cgroup_current=$(<"$CGROUP_DIR/memory.current")
  cgroup_peak_bytes=$(<"$CGROUP_DIR/memory.peak")
  (( cgroup_high_events > CGROUP_HIGH_BEFORE )) || {
    echo "cgroup memory.high did not observe guest-memory pressure" >&2
    exit 1
  }
  (( cgroup_oom_kills == 0 )) || { echo "cgroup killed the VMM under balloon pressure" >&2; exit 1; }
  (( cgroup_current <= CGROUP_MAX_BYTES )) || { echo "VMM exceeded cgroup memory.max" >&2; exit 1; }
fi

if grep -Eiq 'panicked at|thread .* panicked|fatal.*balloon' "$LOG"; then
  echo "VMM log contains a balloon failure" >&2
  tail -n 120 "$LOG" >&2
  exit 1
fi

echo "BALLOON_E2E_PASS cycles=$RESTORE_CYCLES cgroup_high_events=$cgroup_high_events cgroup_peak_bytes=$cgroup_peak_bytes reclaimed_kib=$reclaimed_kib rss_before_kib=$rss_before rss_after_kib=$rss_after"
