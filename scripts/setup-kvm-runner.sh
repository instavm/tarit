#!/usr/bin/env bash
# Retired: repository-wide self-hosted runner labels cannot isolate public PRs.
# Deliberately perform no installation, user/sudoers changes or registration.
set -euo pipefail
echo 'KVM Actions runner registration is disabled for this public repository.' >&2
echo 'Use a fresh disposable VM and docs/KVM_VALIDATION.md for a reviewed immutable SHA.' >&2
exit 2
