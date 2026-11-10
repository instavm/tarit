#!/usr/bin/env bash
# Run only after independently reviewing this script and the exact commit.
# This is an operator acknowledgement, not proof of VM isolation or code safety.
set -euo pipefail
die() { echo "run-trusted-kvm: $*" >&2; exit 2; }
[[ $# == 1 && $1 =~ ^[0-9a-f]{40}$ ]] || die 'pass a full lowercase reviewed commit SHA'
[[ ${TARIT_DISPOSABLE_KVM_ACK:-} == destroy-after-this-run ]] || die 'acknowledge disposable host destruction with TARIT_DISPOSABLE_KVM_ACK=destroy-after-this-run'
[[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || die 'Linux x86_64 required'
[[ $(id -u) == 0 ]] || die 'run as root inside the disposable VM'
[[ -c /dev/kvm ]] || die '/dev/kvm required'
repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
[[ $(git rev-parse HEAD) == "$1" ]] || die 'checkout does not match reviewed SHA'
[[ -z $(git status --porcelain --untracked-files=normal) ]] || die 'checkout must be clean, including untracked files'
# Fixed public upstream: a caller-controlled origin must not establish trust.
git fetch --no-tags https://github.com/instavm/tarit.git main
[[ $(git rev-parse "$1^{commit}") == "$1" ]] || die 'not a commit object'
git merge-base --is-ancestor "$1" FETCH_HEAD || die 'reviewed SHA is not on upstream main'
invoker_home=$(getent passwd "${SUDO_USER:-$(id -un)}" | cut -d: -f6)
[[ -n $invoker_home ]] || die 'cannot locate invoking user toolchain home'
export PATH=$invoker_home/.cargo/bin:$PATH
export RUSTUP_HOME=${RUSTUP_HOME:-$invoker_home/.rustup}
export CARGO_HOME=${CARGO_HOME:-$invoker_home/.cargo}
# Builds and tests execute arbitrary code as root. No runner is registered,
# no persistent sudo policy is installed, and no GitHub token is needed.
(cd vmm && cargo build --release --locked -p vmm --features boot)
(cd orch && cargo build --release --locked -p taritd -p tarit-bench)
export GATES='e2e kvm-suite cold-create warm-create perf'
export SUSPEND_MIN_RSS_DROP_KIB=32768 SUSPEND_RESUME_EXEC_MAX_MS=5000
exec bash scripts/run-readiness-gates.sh
