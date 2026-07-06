#!/usr/bin/env bash
# ci/check.sh — the complete local verification loop on macOS.
#
#   1. native cargo check + test + clippy + fmt  (host = aarch64-apple-darwin)
#   2. cross cargo check to x86_64-unknown-linux-gnu (+ kvm features) so the
#      Linux/KVM code paths and integration tests are type-checked before every commit.
#
# Full KVM *integration* tests run only on a real Linux+KVM host
# (`cargo test --workspace --features kvm -- --include-ignored`).
# See docs/journal/00-setup.md for why Colima/QEMU on M-series Macs is not a
# viable KVM dev environment (TCG emulation, not nested virt).

set -euo pipefail

echo "==> 1/5  cargo check (native aarch64-apple-darwin)"
cargo check --workspace --all-targets

echo "==> 2/5  cargo test  (native; non-KVM unit tests)"
cargo test --workspace

echo "==> 3/5  cargo clippy (-D warnings)"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> 4/5  cargo fmt --check"
cargo fmt --all -- --check

echo "==> 5/5  cargo check (cross x86_64-unknown-linux-gnu + kvm integration tests)"
# Propagate kvm through vmm-core -> vmm-memory-backend, type-check
# vmm-memory-backend's kvm code directly, and include all test targets so
# KVM integration tests cannot silently rot.
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=true \
  cargo check --workspace --all-targets --target x86_64-unknown-linux-gnu \
    --features vmm-core/kvm --features vmm-memory-backend/kvm --features vmm-integration/kvm

echo
echo "all green. KVM integration tests still need a real Linux+KVM host:"
echo "  cargo test --workspace --features kvm -- --include-ignored"
