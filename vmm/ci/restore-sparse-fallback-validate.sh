#!/usr/bin/env bash
# Qualify restore overlay publication when source and target cannot be reflinked.
set -Eeuo pipefail

VMM=${VMM:?set VMM to the candidate vmm binary}
KERNEL=${KERNEL:?set KERNEL to the guest kernel}
ROOTFS=${ROOTFS:?set ROOTFS to an ext4 OCI-derived guest image}
AGENT=${AGENT:?set AGENT to the candidate vmm-agent binary}
SOURCE_ROOT=${SOURCE_ROOT:-/t}
TARGET_ROOT=${TARGET_ROOT:-/tmp}

for path in "$VMM" "$KERNEL" "$ROOTFS" "$AGENT"; do
  test -f "$path" || { echo "missing test input: $path" >&2; exit 1; }
done
test -c /dev/kvm || { echo '/dev/kvm is unavailable' >&2; exit 1; }

source_dir=$(mktemp -d "$SOURCE_ROOT/tarit-sparse-source.XXXXXX")
target_dir=$(mktemp -d "$TARGET_ROOT/tarit-sparse-target.XXXXXX")
socket="$target_dir/vmm.sock"
log="$target_dir/vmm.log"
staged_rootfs="$source_dir/rootfs.ext4"
golden_overlay="$source_dir/golden.cow"
restore_overlay="$target_dir/restored.cow"
snapshot=
vmm_pid=

cleanup() {
  set +e
  if [[ -n "$vmm_pid" ]] && kill -0 "$vmm_pid" 2>/dev/null; then
    kill -TERM "$vmm_pid"
    wait "$vmm_pid"
  fi
  [[ -z "$snapshot" ]] || rm -f -- "$snapshot"
  rm -rf -- "$source_dir" "$target_dir"
}
trap cleanup EXIT

cp --reflink=auto --sparse=always -- "$ROOTFS" "$staged_rootfs"
"$(dirname "$AGENT")/bake-agent.sh" "$staged_rootfs" "$AGENT" >/dev/null
base_digest=$(sha256sum "$staged_rootfs" | awk '{print $1}')

api() {
  python3 - "$socket" "$1" <<'PY'
import json
import socket
import struct
import sys

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.settimeout(90)
sock.connect(sys.argv[1])
body = sys.argv[2].encode()
sock.sendall(struct.pack(">I", len(body)) + body)
header = sock.recv(4)
if len(header) != 4:
    raise SystemExit("short API response header")
remaining = struct.unpack(">I", header)[0]
response = bytearray()
while len(response) < remaining:
    chunk = sock.recv(remaining - len(response))
    if not chunk:
        raise SystemExit("short API response body")
    response.extend(chunk)
payload = json.loads(response)
if isinstance(payload, dict) and payload.get("error"):
    raise SystemExit(f"VMM API error: {payload['error']}")
print(json.dumps(payload, separators=(",", ":")))
PY
}

RUST_LOG=info "$VMM" serve --socket "$socket" >"$log" 2>&1 &
vmm_pid=$!
for _ in $(seq 1 100); do
  [[ -S "$socket" ]] && break
  kill -0 "$vmm_pid"
  sleep 0.1
done
[[ -S "$socket" ]]

cmdline='console=ttyS0 reboot=k panic=1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw init=/usr/sbin/vmm-agent'
create=$(python3 - "$KERNEL" "$staged_rootfs" "$golden_overlay" "$cmdline" <<'PY'
import json
import sys
print(json.dumps({
    "op": "create",
    "config": {
        "kernel": {"path": sys.argv[1], "cmdline": sys.argv[4], "initramfs": None},
        "memory": {"size_mib": 256},
        "vcpus": {"count": 1},
        "volumes": [{"path": sys.argv[2], "read_only": True, "overlay": sys.argv[3]}],
        "net": [],
    },
}))
PY
)
api "$create" >/dev/null

ready=0
for _ in $(seq 1 180); do
  if api '{"op":"exec","command":"printf sparse-fallback-ready","timeout_ms":5000}' \
      | grep -Fq sparse-fallback-ready; then
    ready=1
    break
  fi
  sleep 0.5
done
[[ "$ready" == 1 ]]
api '{"op":"exec","command":"printf golden-state > /root/tarit-sparse-state; sync","timeout_ms":15000}' >/dev/null

snapshot_response=$(api '{"op":"snapshot","diff":false}')
snapshot=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["path"])' <<<"$snapshot_response")
test -s "$snapshot"
golden_digest=$(sha256sum "$golden_overlay" | awk '{print $1}')

restore=$(python3 - "$snapshot" "$restore_overlay" <<'PY'
import json
import sys
print(json.dumps({"op": "restore", "snapshot_path": sys.argv[1], "overlay": sys.argv[2]}))
PY
)
api "$restore" >/dev/null
restored=$(api '{"op":"exec","command":"cat /root/tarit-sparse-state","timeout_ms":15000}')
grep -Fq golden-state <<<"$restored"
api '{"op":"exec","command":"printf restored-private > /root/tarit-sparse-state; sync","timeout_ms":15000}' >/dev/null

[[ $(sha256sum "$staged_rootfs" | awk '{print $1}') == "$base_digest" ]]
[[ $(sha256sum "$golden_overlay" | awk '{print $1}') == "$golden_digest" ]]
virtual_bytes=$(stat -c %s "$restore_overlay")
allocated_bytes=$(( $(stat -c %b "$restore_overlay") * 512 ))
(( virtual_bytes > 0 ))
(( allocated_bytes < virtual_bytes / 2 )) || {
  echo "restore overlay is unexpectedly dense: allocated=$allocated_bytes virtual=$virtual_bytes" >&2
  exit 1
}
grep -Fq 'restore overlay seed published (copy_mode=sparse_extent)' "$log"
if grep -Fq 'copy_mode=dense' "$log"; then
  echo 'dense restore overlay publication was used on Linux' >&2
  exit 1
fi

api '{"op":"stop"}' >/dev/null
echo "RESTORE_SPARSE_FALLBACK_PASS allocated=$allocated_bytes virtual=$virtual_bytes"
