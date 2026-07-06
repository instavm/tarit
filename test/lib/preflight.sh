#!/usr/bin/env bash
# test/lib/preflight.sh — shared preflight guards and helpers for the Tarit test
# and benchmark runners. Source this from a runner:
#   . "$(dirname "$0")/lib/preflight.sh"
#
# The guards are deliberately loud and specific: microVM tests need a real Linux
# host with KVM, and people run these on laptops, containers, and cloud VMs that
# often lack it. Every guard explains what is wrong and how to fix it.

# Resolve the repo root (two levels up from test/lib) unless the caller set it.
: "${REPO_ROOT:=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]:-$0}")/../.." && pwd)}"

# Under sudo, root's PATH usually lacks the invoking user's cargo (rustup installs
# to ~/.cargo/bin). Make it visible so cargo builds work with `sudo test/...`.
if [ -n "${SUDO_USER:-}" ]; then
  _uh="$(python3 -c 'import pwd, sys; print(pwd.getpwnam(sys.argv[1]).pw_dir)' "$SUDO_USER" 2>/dev/null || true)"
  for _d in "$_uh/.cargo/bin"; do
    [ -d "$_d" ] && case ":$PATH:" in *":$_d:"*) ;; *) PATH="$_d:$PATH";; esac
  done
  [ -d "$_uh/.rustup" ] && export RUSTUP_HOME="$_uh/.rustup"
  [ -d "$_uh/.cargo" ] && export CARGO_HOME="$_uh/.cargo"
  export PATH
fi

_c_red=$'\033[31m'; _c_grn=$'\033[32m'; _c_ylw=$'\033[33m'; _c_off=$'\033[0m'
info(){ printf '%s==>%s %s\n' "$_c_grn" "$_c_off" "$*"; }
warn(){ printf '%s[warn]%s %s\n' "$_c_ylw" "$_c_off" "$*" >&2; }
die(){  printf '%s[error]%s %s\n' "$_c_red" "$_c_off" "$*" >&2; exit 1; }
have(){ command -v "$1" >/dev/null 2>&1; }

# require_cmd <cmd> [install hint]
require_cmd(){
  have "$1" && return 0
  die "required command '$1' not found.${2:+ $2}"
}

require_linux(){
  [ "$(uname -s)" = "Linux" ] || die "these tests boot guests under KVM and need a Linux host (found $(uname -s)). Run on a Linux machine with KVM."
}

# require_kvm — the big one. microVMs cannot boot without /dev/kvm.
require_kvm(){
  require_linux
  if [ ! -e /dev/kvm ]; then
    die "/dev/kvm not found — this host has no KVM.

  Tarit boots real hardware-virtualized microVMs, so it needs KVM:
    * bare-metal Linux, or
    * a VM with nested virtualization enabled (AWS *.metal or c8i with nested
      virt, GCP --enable-nested-virtualization, or QEMU/KVM with host-passthrough).

  It will NOT work on macOS/Windows, most containers, or cloud VMs without nested
  virtualization. Check with:  ls -l /dev/kvm   and   lscpu | grep -i virt"
  fi
  if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
    die "/dev/kvm exists but is not accessible by $(id -un). Run as root (sudo), or add yourself to the 'kvm' group:  sudo usermod -aG kvm \$USER  (then re-login)."
  fi
}

require_root(){
  [ "$(id -u)" = "0" ] || die "must run as root (microVM networking, the jailer, and the OCI unpack need it). Re-run with sudo."
}

# detect_virt — informational: warn about the nested-KVM performance tax.
detect_virt(){
  local model
  if have lscpu && lscpu | grep -qi hypervisor; then model="nested (running inside a VM)"; else model="bare metal"; fi
  info "host: $(uname -srm), $(nproc 2>/dev/null || echo '?') vCPUs, virt: $model"
  case "$model" in
    nested*) warn "nested virtualization: microVM boot pays a KVM-exit tax (~10x). Numbers are directional; use a bare-metal host for headline figures." ;;
  esac
}

# require_tools — the common userspace the runners need.
require_tools(){
  require_cmd curl "install curl"
  require_cmd python3 "install python3"
  require_cmd cargo "install the Rust toolchain from https://rustup.rs"
}

# build_binaries — build taritd (release) + vmm (debug) if missing.
build_binaries(){
  local vmm="$REPO_ROOT/vmm/target/debug/vmm"
  local taritd="$REPO_ROOT/orch/target/release/taritd"
  if [ ! -x "$vmm" ]; then info "building vmm (debug)…"; ( cd "$REPO_ROOT/vmm" && cargo build -p vmm --features boot ) || die "vmm build failed"; fi
  if [ ! -x "$taritd" ]; then info "building taritd (release)…"; ( cd "$REPO_ROOT/orch" && cargo build --release -p taritd ) || die "taritd build failed"; fi
  export TARIT_VMM_BIN="$vmm" TARITD_BIN="$taritd"
}

# require_fixtures — a guest kernel + a rootfs with the exec agent.
# Honors TARIT_KERNEL / TARIT_ROOTFS; points at `make guest` when missing.
require_fixtures(){
  : "${TARIT_KERNEL:=/tmp/vmlinux.microvm}"
  : "${TARIT_ROOTFS:=/tmp/vsock-rootfs.ext4}"
  [ -f "$TARIT_KERNEL" ] || die "guest kernel not found at TARIT_KERNEL=$TARIT_KERNEL. Build one with:  sudo make guest   (or set TARIT_KERNEL to a virtio-blk+vsock vmlinux)."
  [ -f "$TARIT_ROOTFS" ] || die "guest rootfs not found at TARIT_ROOTFS=$TARIT_ROOTFS. Build one with:  sudo make guest   (or 'vmm pull docker://ubuntu:22.04 --output rootfs.ext4 --agent vmm/guest/agent/vmm-agent')."
  export TARIT_KERNEL TARIT_ROOTFS
  info "kernel=$TARIT_KERNEL  rootfs=$TARIT_ROOTFS"
}

# require_aws — for the cluster runner (runs from a workstation, not the KVM host).
require_aws(){
  require_cmd aws "install the AWS CLI v2"
  aws sts get-caller-identity >/dev/null 2>&1 || die "AWS credentials are not configured or expired. Run 'aws configure' or export AWS_* env vars."
  [ -n "${SSH_KEY:-}" ] || die "SSH_KEY is not set. Set SSH_KEY to the private key for the EC2 key pair (KEYNAME)."
  [ -f "$SSH_KEY" ] || die "cluster SSH key not found at SSH_KEY=$SSH_KEY. Set SSH_KEY to the private key for the EC2 key pair."
}

# reap_taritd_vmm — kill leftover taritd / vmm serve processes between runs.
reap_taritd_vmm(){
  local p
  for p in $(pgrep -f 'release/taritd' 2>/dev/null) $(pgrep -f 'debug/taritd' 2>/dev/null); do kill "$p" 2>/dev/null; done
  for p in $(pgrep -f 'vmm serve' 2>/dev/null); do kill "$p" 2>/dev/null; done
  sleep 1
}
