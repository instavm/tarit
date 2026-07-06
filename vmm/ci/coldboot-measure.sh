#!/usr/bin/env bash
# ci/coldboot-measure.sh — cold create → first-exec-result latency.
#
# Times the whole cold path: create the VM, then retry exec until the guest is
# up and the first command completes. Reports the create-return split and the
# median over N runs. Nested-virt c8i numbers run ~10x a bare-metal host
# (docs/cold-boot-exec.md) — use them for relative gains.
#
# Usage (on the c8i KVM host): bash ci/coldboot-measure.sh
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL_FULL="${KERNEL_FULL:-/tmp/vmlinux.microvm}"   # has virtio-vsock
KERNEL_MIN="${KERNEL_MIN:-/tmp/vmlinux.minimal}"     # smaller, no virtio-vsock
ROOTFS="${ROOTFS:-/tmp/vsock-rootfs.ext4}"           # rootfs with the vsock-capable agent
RUNS="${RUNS:-5}"
POLL_MS="${POLL_MS:-15}"

# fast cmdline (matches default_cmdline()) + rootfs init.
FAST="console=ttyS0 quiet loglevel=0 reboot=k panic=-1 nomodule i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd swiotlb=noforce cryptomgr.notests random.trust_cpu=on tsc=reliable no_timer_check nokaslr pci=off root=/dev/vda rw init=/usr/sbin/vmm-agent"

frame() {
  BODY="$2" python3 - "$1" 2>/dev/null <<'PY'
import socket, struct, sys, os
try:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(30); s.connect(sys.argv[1])
    b = os.environ["BODY"].encode(); s.sendall(struct.pack('>I', len(b)) + b)
    rl = struct.unpack('>I', s.recv(4))[0]; d = b''
    while len(d) < rl:
        c = s.recv(rl - len(d))
        if not c: break
        d += c
    sys.stdout.write(d.decode())
except Exception:
    print("ERR")
PY
}

# One cold run: prints "<create_ms> <first_exec_ms>".
one_run() {
  local kernel="$1" env_prefix="$2"
  local sock=/tmp/cb.sock log=/tmp/cb.log
  rm -f "$sock" "$log"
  # shellcheck disable=SC2086
  env $env_prefix RUST_LOG=error "$VMM" serve --socket "$sock" >"$log" 2>&1 &
  local sp=$!
  sleep 0.4
  local cfg
  cfg='{"op":"create","config":{"kernel":{"path":"'"$kernel"'","cmdline":"'"$FAST"'","initramfs":null},"memory":{"size_mib":256},"vcpus":{"count":1},"volumes":[{"path":"'"$ROOTFS"'","read_only":false}],"net":[]}}'
  local t0 t_created t_exec
  t0=$(date +%s%N)
  frame "$sock" "$cfg" >/dev/null
  t_created=$(date +%s%N)
  t_exec=$t_created
  local deadline=$((t0 + 30000000000))
  while [ "$(date +%s%N)" -lt "$deadline" ]; do
    r=$(frame "$sock" '{"op":"exec","command":"true","timeout_ms":200}')
    if echo "$r" | grep -q '"exec"'; then
      t_exec=$(date +%s%N)
      break
    fi
    sleep "0.$(printf '%03d' "$POLL_MS")"
  done
  frame "$sock" '{"op":"stop"}' >/dev/null
  kill "$sp" 2>/dev/null || true
  sleep 0.5
  echo "$(( (t_created - t0) / 1000000 )) $(( (t_exec - t0) / 1000000 ))"
}

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:int((a[NR/2]+a[NR/2+1])/2)}'; }

bench() {
  local label="$1" kernel="$2" env_prefix="$3"
  local creates=() execs=()
  for _ in $(seq 1 "$RUNS"); do
    read -r c e <<<"$(one_run "$kernel" "$env_prefix")"
    creates+=("$c"); execs+=("$e")
  done
  printf '%-34s create-return p50=%4s ms   create->first-exec p50=%5s ms\n' \
    "$label" "$(median "${creates[@]}")" "$(median "${execs[@]}")"
}

echo "== cold create -> first-exec (c8i nested-virt; bare metal ~10x faster), ${RUNS} runs, poll ${POLL_MS}ms =="
bench "microvm kernel + vsock exec"   "$KERNEL_FULL" ""
bench "microvm kernel + serial exec"  "$KERNEL_FULL" "VMM_VSOCK_EXEC=0"
bench "minimal kernel + serial exec"  "$KERNEL_MIN"  "VMM_VSOCK_EXEC=0"
