#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=kernel-version.env
. "$SCRIPT_DIR/kernel-version.env"

OUT="${OUT:?set OUT to the destination vmlinux path}"
ARTIFACT="vmlinux-${KERNEL_VERSION}-x86_64"
BASE_URL="${KERNEL_RELEASE_BASE_URL:-https://github.com/${KERNEL_RELEASE_REPOSITORY}/releases/download/${KERNEL_RELEASE_TAG}}"

case "$KERNEL_ARTIFACT_SHA256" in
  [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]*) ;;
  *)
    echo "error: KERNEL_ARTIFACT_SHA256 is not pinned in kernel-version.env" >&2
    exit 1
    ;;
esac
[ "${#KERNEL_ARTIFACT_SHA256}" -eq 64 ] || {
  echo "error: KERNEL_ARTIFACT_SHA256 must contain 64 hexadecimal characters" >&2
  exit 1
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

mkdir -p "$(dirname "$OUT")"
TMP="$(mktemp "${OUT}.download.XXXXXX")"
trap 'rm -f "$TMP"' EXIT

echo "== downloading Tarit guest kernel ${KERNEL_VERSION} =="
curl --fail --location --retry 3 --retry-all-errors \
  "${BASE_URL}/${ARTIFACT}" -o "$TMP"

ACTUAL_SHA256="$(sha256_file "$TMP")"
if [ "$ACTUAL_SHA256" != "$KERNEL_ARTIFACT_SHA256" ]; then
  echo "error: guest kernel checksum mismatch" >&2
  echo "expected: $KERNEL_ARTIFACT_SHA256" >&2
  echo "actual:   $ACTUAL_SHA256" >&2
  exit 1
fi

chmod 0644 "$TMP"
mv "$TMP" "$OUT"
trap - EXIT
echo "== verified guest kernel sha256: $ACTUAL_SHA256 =="
