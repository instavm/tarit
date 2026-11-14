#!/bin/sh
# Refuse to install a cargo-built artifact that is missing or older than its sources.
#
# `make install` runs under sudo and must never compile, so it cannot rebuild; it
# can only check that what `make build` produced is still current. Cargo writes the
# exact list of source files a binary was built from next to it (`<artifact>.d`);
# if any of those inside this checkout (or an extra file such as Cargo.lock) is
# newer than the artifact, the build step was skipped after a change.
#
# usage: check-install-artifact.sh <artifact> <build-command> [<extra-source>...]
set -eu

artifact=$1
build=$2
shift 2

fail() {
  echo "error: $1; run '$build' as your regular user first, then install again" >&2
  exit 1
}

[ -x "$artifact" ] || fail "missing $artifact"
depinfo="$artifact.d"
[ -f "$depinfo" ] || fail "cannot tell whether $artifact is current: cargo dep-info $depinfo is missing"

root=$(cd "$(dirname "$0")/.." && pwd -P)

# Cargo's dep-info is one make rule, `<artifact>: <src> <src> ...`, with spaces in
# paths escaped as `\ `. Print one source path per line.
sources=$(awk '
  /^#/ || done { next }
  {
    sub(/^[^:]*:[ \t]*/, "")
    gsub(/\\ /, "\001")
    n = split($0, parts, /[ \t]+/)
    for (i = 1; i <= n; i++) if (parts[i] != "") { gsub(/\001/, " ", parts[i]); print parts[i] }
    done = 1
  }
' "$depinfo")

stale() {
  if [ ! -e "$1" ]; then
    fail "$artifact was built from $1, which no longer exists"
  fi
  if [ -n "$(find "$1" -prune -newer "$artifact")" ]; then
    fail "$artifact is older than $1"
  fi
}

in_repo=0
while IFS= read -r source; do
  case $source in
    "$root"/*) in_repo=$((in_repo + 1)); stale "$source" ;;
    # Registry and toolchain sources are pinned by Cargo.lock, checked below.
    *) ;;
  esac
done <<SOURCES
$sources
SOURCES

[ "$in_repo" -gt 0 ] || fail "$artifact was not built from this checkout ($root)"

for extra in "$@"; do
  stale "$extra"
done
