#!/usr/bin/env bash
# Exercise OCI resource admission through the public pull CLI.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM="${VMM_BIN:-$ROOT/target/release/vmm}"
WORK_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"

for required in python3 skopeo umoci mke2fs; do
  command -v "$required" >/dev/null || {
    echo "FAIL: missing $required" >&2
    exit 1
  }
done
test -x "$VMM" || { echo "FAIL: vmm not executable: $VMM" >&2; exit 1; }

DIR=$(mktemp -d "$WORK_ROOT/tarit-oci-limits.XXXXXX")
chmod 700 "$DIR"
cleanup() {
  local status=$?
  if [ "$status" -ne 0 ] && [ "${TARIT_E2E_KEEP_FAILED:-0}" = 1 ]; then
    echo "FAIL: retained diagnostic directory: $DIR" >&2
  else
    find "$DIR" -depth -delete 2>/dev/null || true
  fi
  return "$status"
}
trap cleanup EXIT

make_fixture() {
  local mode=$1 layout=$2
  python3 - "$mode" "$layout" <<'PY'
import gzip
import hashlib
import io
import json
import pathlib
import sys
import tarfile

mode, raw_root = sys.argv[1:]
root = pathlib.Path(raw_root)
(root / "blobs" / "sha256").mkdir(parents=True)

def blob(payload):
    digest = hashlib.sha256(payload).hexdigest()
    (root / "blobs" / "sha256" / digest).write_bytes(payload)
    return digest

archive_buffer = io.BytesIO()
with tarfile.open(fileobj=archive_buffer, mode="w", format=tarfile.GNU_FORMAT) as archive:
    if mode == "expansion":
        for index in range(3000):
            payload = b"\0" * 1024
            entry = tarfile.TarInfo(f"payload/{index:05d}")
            entry.mode = 0o644
            entry.size = len(payload)
            archive.addfile(entry, io.BytesIO(payload))
    elif mode == "inodes":
        for index in range(16385):
            entry = tarfile.TarInfo(f"empty/{index:05d}")
            entry.mode = 0o644
            entry.size = 0
            archive.addfile(entry, io.BytesIO())
    else:
        raise SystemExit(f"unknown fixture mode: {mode}")

uncompressed = archive_buffer.getvalue()
compressed_buffer = io.BytesIO()
with gzip.GzipFile(fileobj=compressed_buffer, mode="wb", mtime=0) as compressor:
    compressor.write(uncompressed)
compressed = compressed_buffer.getvalue()
layer_digest = blob(compressed)

config = json.dumps({
    "architecture": "amd64",
    "os": "linux",
    "rootfs": {
        "type": "layers",
        "diff_ids": [f"sha256:{hashlib.sha256(uncompressed).hexdigest()}"],
    },
}, separators=(",", ":")).encode()
config_digest = blob(config)
manifest = json.dumps({
    "schemaVersion": 2,
    "mediaType": "application/vnd.oci.image.manifest.v1+json",
    "config": {
        "mediaType": "application/vnd.oci.image.config.v1+json",
        "digest": f"sha256:{config_digest}",
        "size": len(config),
    },
    "layers": [{
        "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
        "digest": f"sha256:{layer_digest}",
        "size": len(compressed),
    }],
}, separators=(",", ":")).encode()
manifest_digest = blob(manifest)
index = {
    "schemaVersion": 2,
    "manifests": [{
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "digest": f"sha256:{manifest_digest}",
        "size": len(manifest),
        "annotations": {"org.opencontainers.image.ref.name": "fixture"},
    }],
}
(root / "index.json").write_text(json.dumps(index, separators=(",", ":")))
(root / "oci-layout").write_text('{"imageLayoutVersion":"1.0.0"}')
PY
}

assert_rejected() {
  local mode=$1 size_mib=$2 expected=$3
  local layout="$DIR/$mode-layout" output="$DIR/$mode.ext4" log="$DIR/$mode.log"
  make_fixture "$mode" "$layout"
  if "$VMM" pull --size "$size_mib" --output "$output" \
    "oci:$layout:fixture" >"$log" 2>&1; then
    echo "FAIL: $mode fixture was admitted" >&2
    exit 1
  fi
  grep -F "OCI resource limit exceeded" "$log" >/dev/null || {
    echo "FAIL: $mode fixture returned the wrong error" >&2
    cat "$log" >&2
    exit 1
  }
  grep -F "$expected" "$log" >/dev/null || {
    echo "FAIL: $mode fixture did not report $expected" >&2
    cat "$log" >&2
    exit 1
  }
  test ! -e "$output" || {
    echo "FAIL: $mode fixture published an output image" >&2
    exit 1
  }
  if find "$DIR" -maxdepth 1 -type d -name '.tarit-oci-*' -print -quit | grep -q .; then
    echo "FAIL: $mode fixture left an OCI build workspace" >&2
    exit 1
  fi
}

assert_rejected expansion 1 "expanded layers exceed limit"
assert_rejected inodes 16 "layer entries exceed limit"
echo "OCI_RESOURCE_LIMITS_PASS cases=2"
