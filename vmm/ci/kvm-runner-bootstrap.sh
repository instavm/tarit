#!/usr/bin/env bash
# ci/kvm-runner-bootstrap.sh — provision a Colima Linux VM with KVM + the
# Rust toolchain, then run the VMM's KVM integration suite inside it.
#
# Run on the macOS host. Idempotent.

set -euo pipefail

VM_NAME="${VM_NAME:-vmm-kvm}"
CPUS="${CPUS:-4}"
MEM="${MEM:-8}"
DISK="${DISK:-50}"

echo "==> Ensuring colima + qemu are installed"
if ! command -v colima >/dev/null; then
  brew install colima qemu
fi

echo "==> Starting Colima VM '$VM_NAME' ($CPUS cpu, ${MEM}G mem, ${DISK}G disk)"
if ! colima list | grep -q "^$VM_NAME .* running"; then
  colima start "$VM_NAME" --cpu "$CPUS" --memory "$MEM" --disk "$DISK" --vm-type qemu
fi

echo "==> Inside the VM: install KVM + rust toolchain"
colima ssh "$VM_NAME" -- bash -lc '
  set -e
  if ! command -v cargo >/dev/null; then
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.95.0
    . "$HOME/.cargo/env"
  fi
  sudo apt-get update -y
  sudo apt-get install -y qemu-kvm libvirt-dev build-essential curl
  sudo usermod -aG kvm "$USER" || true
  ls -l /dev/kvm || true
  cargo --version
'

echo "==> Sync repo into the VM and run integration tests"
colima ssh "$VM_NAME" -- bash -lc '
  set -e
  cd ~/vmm 2>/dev/null || true
  cargo test --workspace --features kvm -- --include-ignored
'

echo "done."
