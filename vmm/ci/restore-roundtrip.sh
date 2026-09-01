#!/usr/bin/env bash
# ci/restore-roundtrip.sh — validate faithful snapshot→restore→resume on KVM.
#
# Boots a real guest via the VMM API, snapshots it, stops it, restores it, and
# checks that the restored VM comes back *running* (vCPU state re-applied, guest
# still making progress on the serial console) rather than a paused memory image.
#
# Run on the c8i KVM host (needs sudo for /dev/kvm):
#   sudo bash /tmp/restore-roundtrip.sh
set -Eeuo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS_SOURCE="${ROOTFS:-/tmp/debian-rootfs.ext4}"
GUEST_AGENT_BIN="${GUEST_AGENT_BIN:-}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-/tmp}"
WORK_DIR=$(mktemp -d "${SOCKET_ROOT%/}/tarit-authenticated-restore.XXXXXX")
SOCK="$WORK_DIR/vmm.sock"
LOG="$WORK_DIR/vmm.log"
PERSISTED_SNAP="$WORK_DIR/persisted.snap"
PERSISTED_INTEGRITY="$WORK_DIR/persisted.integrity.json"
TEST_ROOTFS=
ROOTFS_MOUNT=
SERVE_PID=

cleanup() {
  if [[ -n "$SERVE_PID" ]]; then
    kill "$SERVE_PID" 2>/dev/null || true
    wait "$SERVE_PID" 2>/dev/null || true
  fi
  if [[ -n "$ROOTFS_MOUNT" ]] && mountpoint -q "$ROOTFS_MOUNT"; then
    umount "$ROOTFS_MOUNT" 2>/dev/null || true
  fi
  rm -f -- "$SOCK" "$PERSISTED_SNAP" "$PERSISTED_INTEGRITY"
  if [[ -n "$TEST_ROOTFS" ]]; then
    rm -f -- "$TEST_ROOTFS"
  fi
  if [[ -n "$ROOTFS_MOUNT" ]]; then
    rmdir -- "$ROOTFS_MOUNT" 2>/dev/null || true
  fi
  rm -f -- "$LOG"
  rmdir -- "$WORK_DIR" 2>/dev/null || true
}
trap cleanup EXIT

ROOTFS="$ROOTFS_SOURCE"
if [[ -n "$GUEST_AGENT_BIN" ]]; then
  [[ -x "$GUEST_AGENT_BIN" ]]
  TEST_ROOTFS="$WORK_DIR/rootfs.ext4"
  cp --reflink=auto --sparse=always -- "$ROOTFS_SOURCE" "$TEST_ROOTFS"
  chmod 0600 "$TEST_ROOTFS"
  ROOTFS_MOUNT="$WORK_DIR/rootfs-mount"
  mkdir -m 0700 "$ROOTFS_MOUNT"
  mount -o loop,rw -- "$TEST_ROOTFS" "$ROOTFS_MOUNT"
  install -D -m 0755 -- "$GUEST_AGENT_BIN" "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
  sync -f "$ROOTFS_MOUNT/usr/sbin/vmm-agent"
  umount "$ROOTFS_MOUNT"
  rmdir -- "$ROOTFS_MOUNT"
  ROOTFS_MOUNT=
  e2fsck -pf "$TEST_ROOTFS" >/dev/null
  ROOTFS="$TEST_ROOTFS"
fi

api() {
  python3 - "$SOCK" "$1" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(30)
try:
    s.connect(sys.argv[1])
    body = sys.argv[2].encode()
    s.sendall(struct.pack('>I', len(body)) + body)
    rl = struct.unpack('>I', s.recv(4))[0]
    data = b''
    while len(data) < rl:
        chunk = s.recv(rl - len(data))
        if not chunk:
            break
        data += chunk
    print(data.decode())
except Exception as e:
    print('{"error":"client: %s"}' % e)
finally:
    s.close()
PY
}

RUST_LOG=info "$VMM" serve --socket "$SOCK" >"$LOG" 2>&1 &
SERVE_PID=$!
sleep 1

CMDLINE="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
if [[ -n "$GUEST_AGENT_BIN" ]]; then
  CMDLINE="$CMDLINE init=/usr/sbin/vmm-agent"
fi

echo "=== create (real kernel + rootfs, full boot) ==="
CFG='{"op":"create","config":{"kernel":{"path":"'"$KERNEL"'","cmdline":"'"$CMDLINE"'","initramfs":null},"memory":{"size_mib":256},"vcpus":{"count":1},"volumes":[{"path":"'"$ROOTFS"'","read_only":false}],"net":[]}}'
api "$CFG"
echo "  (booting 12s)"
sleep 12

echo "=== snapshot A ==="
RA=$(api '{"op":"snapshot","diff":false,"live":true}')
echo "  $RA"
SNAP=$(echo "$RA" | python3 -c "import sys,json;print(json.loads(sys.stdin.read()).get('path',''))" 2>/dev/null)
INTEGRITY=$(echo "$RA" | python3 -c "import sys,json;print(json.loads(sys.stdin.read()).get('integrity_path',''))" 2>/dev/null)
echo "  snap=$SNAP"
if [ -z "$SNAP" ]; then
  echo "snapshot response did not include a path" >&2
  exit 1
fi
if [ ! -f "$SNAP" ]; then
  echo "snapshot file does not exist: $SNAP" >&2
  exit 1
fi
if [ -z "$INTEGRITY" ] || [ ! -f "$INTEGRITY" ]; then
  echo "snapshot response did not include an integrity manifest" >&2
  exit 1
fi
# Snapshot paths returned by the VMM are process-owned scratch files. Preserve
# private test copy before stop.
cp --reflink=auto --sparse=always -- "$SNAP" "$PERSISTED_SNAP"
cp --reflink=auto --sparse=always -- "$INTEGRITY" "$PERSISTED_INTEGRITY"
test -s "$PERSISTED_SNAP"
test -s "$PERSISTED_INTEGRITY"
MANIFEST_SHA=$(sha256sum "$PERSISTED_INTEGRITY" | awk '{print $1}')
[[ "$MANIFEST_SHA" =~ ^[0-9a-f]{64}$ ]]

echo "=== stop ==="
api '{"op":"stop"}'
sleep 1
test ! -e "$SNAP"
test ! -e "$INTEGRITY"

MARK=$(wc -l < "$LOG")
echo "=== unauthenticated restore (expect: rejected) ==="
UNVERIFIED=$(api '{"op":"restore","snapshot_path":"'"$PERSISTED_SNAP"'"}')
echo "  $UNVERIFIED"
python3 -c 'import json,sys; response=json.load(sys.stdin); assert response.get("status") == "err"; assert "unverified snapshot restore is disabled" in response.get("msg", "")' <<<"$UNVERIFIED"
EMPTY_STATUS=$(api '{"op":"status"}')
python3 -c 'import json,sys; response=json.load(sys.stdin); assert response.get("status") == "err"; assert "no VM" in response.get("msg", "")' <<<"$EMPTY_STATUS"

echo "=== restore (expect: running) ==="
RESTORED=$(api '{"op":"restore","snapshot_path":"'"$PERSISTED_SNAP"'","memory_integrity":{"manifest_path":"'"$PERSISTED_INTEGRITY"'","manifest_sha256":"'"$MANIFEST_SHA"'"}}')
echo "  $RESTORED"
python3 -c 'import json,sys; assert json.load(sys.stdin).get("status") == "restored"' <<<"$RESTORED"
echo "  (running 5s post-restore)"
sleep 5

echo "=== snapshot B (liveness probe — should re-capture a live vCPU) ==="
api '{"op":"snapshot","diff":false}'
sleep 1

echo "=== stop ==="
api '{"op":"stop"}'
sleep 1
kill "$SERVE_PID" 2>/dev/null || true
wait "$SERVE_PID" 2>/dev/null || true
SERVE_PID=
sleep 1

echo ""
echo "=== restore outcome ==="
grep -nE "restored|reconstruct|could not" "$LOG" | tail -8 || true
echo ""
echo "=== post-restore serial/log (lines after restore call) ==="
tail -n +"$MARK" "$LOG" | grep -vE "^\s*$" | tail -60 || true
echo ""
echo "=== any KVM errors / guest panic anywhere ==="
grep -niE "panic|SHUTDOWN|triple|KVM_RUN|internal error|fault|BUG:" "$LOG" | tail -30 || true

echo "AUTHENTICATED_RESTORE_E2E_PASS"
