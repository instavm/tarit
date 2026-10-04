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
VMM_TEST_KERNEL="${VMM_TEST_KERNEL:?set VMM_TEST_KERNEL to the candidate path inside Colima}"
VMM_TEST_ROOTFS="${VMM_TEST_ROOTFS:?set VMM_TEST_ROOTFS to the agent rootfs path inside Colima}"

echo "==> Ensuring colima + qemu are installed"
if ! command -v colima >/dev/null; then
  brew install colima qemu
fi

echo "==> Starting Colima VM '$VM_NAME' ($CPUS cpu, ${MEM}G mem, ${DISK}G disk)"
if ! colima list | grep -q "^$VM_NAME .* running"; then
  colima start "$VM_NAME" --cpu "$CPUS" --memory "$MEM" --disk "$DISK" --vm-type qemu
fi

echo "==> Inside the VM: install KVM + rust toolchain"
# shellcheck disable=SC2016 # The script expands inside Colima.
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
# shellcheck disable=SC2016 # The script expands inside Colima.
colima ssh "$VM_NAME" -- env \
  VMM_TEST_KERNEL="$VMM_TEST_KERNEL" \
  VMM_TEST_ROOTFS="$VMM_TEST_ROOTFS" \
  bash -lc '
  set -e
  cd ~/vmm
  test -r "$VMM_TEST_KERNEL" || {
    echo "error: VMM_TEST_KERNEL is not readable inside Colima: $VMM_TEST_KERNEL" >&2
    exit 1
  }
  test -r "$VMM_TEST_ROOTFS" || {
    echo "error: VMM_TEST_ROOTFS is not readable inside Colima: $VMM_TEST_ROOTFS" >&2
    exit 1
  }
  env \
    VMM_TEST_KERNEL="$VMM_TEST_KERNEL" \
    VMM_TEST_ROOTFS="$VMM_TEST_ROOTFS" \
    cargo test --workspace --features kvm -- --include-ignored
'

echo "done."
