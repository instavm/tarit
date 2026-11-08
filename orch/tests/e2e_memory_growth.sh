#!/usr/bin/env bash
# Hardware acceptance; builds witnesses and stages a private copy of the fixture.
set -Eeuo pipefail
umask 077
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
: "${TARIT_KERNEL:?Use a kernel built with this branch with CONFIG_VIRTIO_MEM=y}"
: "${TARIT_ROOTFS:?Use an ext4 agent-enabled Linux rootfs}"
for cmd in gcc debugfs cmp cp python3; do command -v "$cmd" >/dev/null; done
[ "$(id -u)" = 0 ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]
DIR=$(mktemp -d "${TARIT_TEST_SOCKET_ROOT:-/tmp}/tarit-memory-build.XXXXXX")
trap 'find "$DIR" -depth -delete' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
gcc -std=c11 -O2 -Wall -Wextra -Werror -pedantic -static \
    "$ROOT/orch/tests/memory_growth_workload.c" -o "$DIR/workload"
gcc -O2 -Wall -Wextra -Werror -pedantic -static \
    "$ROOT/vmm/guest/agent/vmm-agent.c" -lutil -o "$DIR/agent"
cp --reflink=auto --sparse=always "$TARIT_ROOTFS" "$DIR/rootfs.ext4"
cmp -s "$TARIT_ROOTFS" "$DIR/rootfs.ext4"
# debugfs changes only our private, unmounted copy; verify the resulting bytes.
debugfs -w -R 'rm /usr/sbin/vmm-agent' "$DIR/rootfs.ext4" >/dev/null 2>&1 || true
debugfs -w -R "write $DIR/agent /usr/sbin/vmm-agent" "$DIR/rootfs.ext4"
debugfs -w -R "write $DIR/workload /tarit-memory-workload" "$DIR/rootfs.ext4"
debugfs -R "dump /usr/sbin/vmm-agent $DIR/agent-check" "$DIR/rootfs.ext4"
debugfs -R "dump /tarit-memory-workload $DIR/workload-check" "$DIR/rootfs.ext4"
cmp -s "$DIR/agent" "$DIR/agent-check"
cmp -s "$DIR/workload" "$DIR/workload-check"
TARIT_ROOTFS="$DIR/rootfs.ext4" python3 "$ROOT/orch/tests/e2e_memory_growth.py"
