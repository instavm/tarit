#!/usr/bin/env bash
# Usage: sudo test/conntrack-reuse-isolated.sh /absolute/path/to/taritd-test-binary
# Build the Linux test binary first with: cargo test --manifest-path orch/Cargo.toml -p taritd --no-run
set -euo pipefail
[[ $(uname -s) == Linux ]] || { echo 'Linux is required.' >&2; exit 1; }
[[ ${EUID} == 0 ]] || { echo 'Run with root privileges for an isolated network namespace.' >&2; exit 1; }
[[ $# == 1 && -x $1 ]] || { echo "Usage: $0 /path/to/taritd-test-binary" >&2; exit 2; }
for tool in unshare conntrack readlink; do
  command -v "$tool" >/dev/null || { echo "Missing required tool: $tool" >&2; exit 1; }
done
test_binary=$(readlink -f -- "$1")
export TARIT_TEST_CT_OUTER_NS
TARIT_TEST_CT_OUTER_NS=$(readlink /proc/self/ns/net)
# No setup or cleanup touches the host firewall. The kernel drops all test
# conntrack state when this private namespace exits, including on test failure.
exec unshare --net /bin/bash -s -- "$test_binary" <<'ISOLATED'
set -euo pipefail
export TARIT_TEST_CT_ISOLATED_NS
TARIT_TEST_CT_ISOLATED_NS=$(readlink /proc/self/ns/net)
[[ $TARIT_TEST_CT_ISOLATED_NS != "$TARIT_TEST_CT_OUTER_NS" ]] || {
  echo 'Refusing to run in the caller network namespace.' >&2; exit 1;
}
test_name=net::tests::conntrack_linux_namespace_invalidation_preserves_unrelated_flows
tests=$("$1" --list --ignored)
[[ $tests == *"$test_name: test"* ]] || {
  echo "Test binary does not contain required test: $test_name" >&2; exit 1;
}
exec "$1" --ignored --exact "$test_name" --nocapture --test-threads=1
ISOLATED
