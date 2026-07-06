#!/usr/bin/env bash
# ci/perf-gates.sh — performance regression gate (PRD §2).
#
# Boots one VM on the local KVM host and measures cold create->first-exec,
# snapshot (full + diff), restore, and the vmm process RSS, comparing each to a
# ceiling. Ceilings default to the c8i NESTED-virt baselines with headroom
# (bare metal runs ~10x faster, PRD §12.0); override per metric via env.
#
# Exit status: with VMM_PERF_STRICT=1, non-zero if any metric regresses past its
# ceiling (for CI). Otherwise it only warns. Run on Linux+KVM (needs /dev/kvm).
#
#   VMM_PERF_STRICT=1 sudo bash ci/perf-gates.sh
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.minimal}"
ROOTFS="${ROOTFS:-/tmp/vsock-rootfs.ext4}"

# Ceilings (ms / KB). c8i nested baselines + headroom; override via env.
MAX_COLD_EXEC_MS="${MAX_COLD_EXEC_MS:-700}"     # baseline ~310
MAX_SNAPSHOT_MS="${MAX_SNAPSHOT_MS:-350}"       # baseline ~117
MAX_DIFF_MS="${MAX_DIFF_MS:-80}"                # baseline ~18
MAX_RESTORE_MS="${MAX_RESTORE_MS:-350}"         # baseline ~113
MAX_RSS_KB="${MAX_RSS_KB:-131072}"              # 128 MiB (incl. touched guest pages)

SOCK=/tmp/perfgate.sock
LOG=/tmp/perfgate.log
rm -f "$SOCK" "$LOG"
FAIL=0

frame() {
  BODY="$2" python3 - "$1" 2>/dev/null <<'PY'
import socket, struct, sys, os
try:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(60); s.connect(sys.argv[1])
    b = os.environ["BODY"].encode(); s.sendall(struct.pack('>I', len(b)) + b)
    rl = struct.unpack('>I', s.recv(4))[0]; d = b''
    while len(d) < rl:
        c = s.recv(rl - len(d))
        if not c: break
        d += c
    sys.stdout.write(d.decode())
except Exception as e:
    print("ERR", e)
PY
}

now_ms() { echo $(( $(date +%s%N) / 1000000 )); }

check() { # name value ceiling unit
  local name="$1" val="$2" max="$3" unit="$4"
  if [ "$val" -le "$max" ]; then
    printf '  PASS  %-26s %6s %s  (<= %s)\n' "$name" "$val" "$unit" "$max"
  else
    printf '  FAIL  %-26s %6s %s  (>  %s)\n' "$name" "$val" "$unit" "$max"
    FAIL=1
  fi
}

CMD="console=ttyS0 quiet loglevel=0 reboot=k panic=-1 nomodule i8042.noaux swiotlb=noforce random.trust_cpu=on nokaslr pci=off root=/dev/vda rw init=/usr/sbin/vmm-agent"
RUST_LOG=error "$VMM" serve --socket "$SOCK" >"$LOG" 2>&1 &
SP=$!
sleep 0.4

CFG='{"op":"create","config":{"kernel":{"path":"'"$KERNEL"'","cmdline":"'"$CMD"'","initramfs":null},"memory":{"size_mib":256},"vcpus":{"count":1},"volumes":[{"path":"'"$ROOTFS"'","read_only":false}],"net":[]}}'
T0=$(now_ms)
frame "$SOCK" "$CFG" >/dev/null
# cold create -> first successful exec
COLD=0
DL=$((T0 + 30000))
while [ "$(now_ms)" -lt "$DL" ]; do
  if frame "$SOCK" '{"op":"exec","command":"true","timeout_ms":200}' | grep -q '"exec"'; then
    COLD=$(( $(now_ms) - T0 )); break
  fi
  sleep 0.015
done

RSS=$(ps -o rss= -p "$SP" 2>/dev/null | tr -d ' '); RSS=${RSS:-0}

t=$(now_ms); R=$(frame "$SOCK" '{"op":"snapshot","diff":false}'); SNAP_MS=$(( $(now_ms) - t ))
SNAP=$(echo "$R" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("path",""))' 2>/dev/null)
t=$(now_ms); frame "$SOCK" '{"op":"snapshot","diff":true}' >/dev/null; DIFF_MS=$(( $(now_ms) - t ))

frame "$SOCK" '{"op":"stop"}' >/dev/null; sleep 1
t=$(now_ms); frame "$SOCK" "{\"op\":\"restore\",\"snapshot_path\":\"$SNAP\"}" >/dev/null; RESTORE_MS=$(( $(now_ms) - t ))
frame "$SOCK" '{"op":"stop"}' >/dev/null
kill "$SP" 2>/dev/null || true

echo "== perf gates (host: $(uname -n); nested-virt numbers ~10x bare metal) =="
check "cold create->first-exec" "$COLD"       "$MAX_COLD_EXEC_MS" ms
check "snapshot (full)"         "$SNAP_MS"    "$MAX_SNAPSHOT_MS"  ms
check "snapshot (diff)"         "$DIFF_MS"    "$MAX_DIFF_MS"      ms
check "restore"                 "$RESTORE_MS" "$MAX_RESTORE_MS"   ms
check "vmm RSS"                 "$RSS"        "$MAX_RSS_KB"       KB

if [ "$FAIL" -ne 0 ]; then
  echo "perf: REGRESSION vs ceilings above"
  if [ "${VMM_PERF_STRICT:-0}" = "1" ]; then exit 1; fi
  echo "perf: VMM_PERF_STRICT!=1, not failing the build"
else
  echo "perf: all gates within ceilings"
fi
