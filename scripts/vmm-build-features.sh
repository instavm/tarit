#!/bin/sh
# Keep a build-time feature record tied to the actual artifact. The install
# path must neither invoke Cargo nor execute a build artifact under sudo.
set -eu

mode=$1
artifact=$2
features=$3
manifest="$artifact.features"

checksum() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum < "$artifact"
    else
        shasum -a 256 < "$artifact"
    fi
}

canonical_features=$(printf '%s\n' "$features" | tr ',[:space:]' '\n' | sed '/^$/d' | LC_ALL=C sort -u | paste -sd, -)
if [ "$mode" = install ]; then
    destination=$5
    tmp=$(mktemp "$destination.XXXXXXXX")
    trap 'rm -f "$tmp"' 0
    install -m755 "$artifact" "$tmp"
    artifact=$tmp
fi
hash_output=$(checksum)
hash=${hash_output%% *}

case "$mode" in
    record)
        tmp=$(mktemp "$manifest.XXXXXXXX")
        trap 'rm -f "$tmp"' 0
        printf '%s\n%s\n' "$hash" "$canonical_features" > "$tmp"
        mv -f "$tmp" "$manifest"
        ;;
    check|install)
        build=$4
        if [ ! -f "$manifest" ] || ! printf '%s\n%s\n' "$hash" "$canonical_features" | cmp -s "$manifest" -; then
            printf "error: %s does not match its recorded VMM_FEATURES (%s); run '%s' as your regular user with the same VMM_FEATURES, then install again\n" \
                "$artifact" "$canonical_features" "$build" >&2
            exit 1
        fi
        if [ "$mode" = install ]; then mv -f "$tmp" "$destination"; fi
        ;;
    *)
        echo "usage: $0 record|check|install artifact features [build-command [destination]]" >&2
        exit 2
        ;;
esac
