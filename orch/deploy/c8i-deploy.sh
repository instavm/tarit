#!/usr/bin/env bash
# Sync the orchestrator to a KVM build/test host, build release taritd, run e2e API tests.
set -euo pipefail

C8I_HOST="${C8I_HOST:?set C8I_HOST to your KVM build/test host (IP or hostname)}"
C8I_USER="${C8I_USER:-ubuntu}"
C8I_KEY="${C8I_KEY:-$HOME/.ssh/vmm-kvm-test.pem}"
C8I_KNOWN_HOSTS="${C8I_KNOWN_HOSTS:-$HOME/.ssh/known_hosts}"
REMOTE_DIR="${REMOTE_DIR:-/home/$C8I_USER/tarit-c8i}"
VMM_DIR="${VMM_DIR:-$REMOTE_DIR/vmm}"
API_KEY="${TARIT_API_KEY:?set TARIT_API_KEY to a strong test-host admin key}"
LISTEN="${TARIT_LISTEN:-127.0.0.1:8080}"
KERNEL="${TARIT_KERNEL:?set TARIT_KERNEL to the candidate vmlinux path on the c8i host}"
ROOTFS="${TARIT_ROOTFS:?set TARIT_ROOTFS to an agent rootfs with curl on the c8i host}"
[ "${#API_KEY}" -ge 32 ] || {
  echo "error: TARIT_API_KEY must contain at least 32 characters" >&2
  exit 1
}

[ -r "$C8I_KEY" ] || {
  echo "error: C8I_KEY is not readable: $C8I_KEY" >&2
  exit 1
}
if [ ! -r "$C8I_KNOWN_HOSTS" ] ||
    ! ssh-keygen -F "$C8I_HOST" -f "$C8I_KNOWN_HOSTS" >/dev/null; then
  echo "error: $C8I_HOST must have a verified entry in $C8I_KNOWN_HOSTS" >&2
  exit 1
fi
SSH_OPTIONS=(
  -i "$C8I_KEY"
  -o StrictHostKeyChecking=yes
  -o "UserKnownHostsFile=$C8I_KNOWN_HOSTS"
  -o ConnectTimeout=15
)
SSH=(ssh "${SSH_OPTIONS[@]}")
printf -v RSYNC_SSH '%q ' ssh "${SSH_OPTIONS[@]}"
RSYNC=(rsync -az --delete --exclude target --exclude .git -e "$RSYNC_SSH")

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ORCH_DIR="$REMOTE_DIR/orch"
[[ "$C8I_USER" =~ ^[a-z_][a-z0-9_-]*$ ]] || {
  echo "error: invalid C8I_USER" >&2
  exit 1
}
REMOTE_HOME="/home/$C8I_USER"
REMOTE_CHILD="${REMOTE_DIR#"$REMOTE_HOME/"}"
case "$REMOTE_CHILD" in
  "" | "." | ".." | */*)
    echo "error: REMOTE_DIR must be a direct child of $REMOTE_HOME" >&2
    exit 1
    ;;
esac
REMOTE_DIR_Q="$(printf '%q' "$REMOTE_DIR")"
VMM_DIR_Q="$(printf '%q' "$VMM_DIR")"
ORCH_DIR_Q="$(printf '%q' "$ORCH_DIR")"
API_KEY_Q="$(printf '%q' "$API_KEY")"
LISTEN_Q="$(printf '%q' "$LISTEN")"
KERNEL_Q="$(printf '%q' "$KERNEL")"
ROOTFS_Q="$(printf '%q' "$ROOTFS")"

echo "== rsync orchestrator -> $C8I_USER@$C8I_HOST:$REMOTE_DIR"
"${SSH[@]}" "$C8I_USER@$C8I_HOST" "set -e
home=\$(readlink -f -- $REMOTE_HOME)
target=$REMOTE_DIR_Q
[ ! -L \"\$target\" ] || { echo 'error: REMOTE_DIR must not be a symlink' >&2; exit 1; }
if [ -e \"\$target\" ]; then
  resolved=\$(readlink -f -- \"\$target\")
  case \"\$resolved\" in
    \"\$home\"/*) ;;
    *) echo 'error: REMOTE_DIR resolves outside the remote home' >&2; exit 1 ;;
  esac
  sudo chown -R \$(id -u):\$(id -g) \"\$resolved\"
fi"
"${RSYNC[@]}" "$ROOT/" "$C8I_USER@$C8I_HOST:$REMOTE_DIR/"

echo "== remote build + e2e =="
"${SSH[@]}" "$C8I_USER@$C8I_HOST" bash -s <<REMOTE
set -euo pipefail
export PATH="\$HOME/.cargo/bin:\$PATH"

mkdir -p ~/.taritd/sockets
PID_FILE="\$HOME/.taritd/taritd.pid"
if [[ -f "\$PID_FILE" ]]; then
  old_pid=\$(<"\$PID_FILE")
  [[ "\$old_pid" =~ ^[0-9]+\$ ]] || {
    echo "error: invalid taritd PID file: \$PID_FILE" >&2
    exit 1
  }
  if sudo kill -0 "\$old_pid" 2>/dev/null; then
    old_exe=\$(sudo readlink -f "/proc/\$old_pid/exe")
    [[ "\$old_exe" == "$ORCH_DIR_Q/target/release/taritd" ]] || {
      echo "error: refusing to stop PID \$old_pid: \$old_exe" >&2
      exit 1
    }
    sudo kill "\$old_pid"
    for _ in {1..30}; do
      sudo kill -0 "\$old_pid" 2>/dev/null || break
      sleep 0.1
    done
    sudo kill -0 "\$old_pid" 2>/dev/null && {
      echo "error: prior taritd PID \$old_pid did not stop" >&2
      exit 1
    }
  fi
  rm -f "\$PID_FILE"
fi

cd $ORCH_DIR_Q
cargo build --release -p taritd

# Ensure vmm exists (reuse or build)
if [[ ! -x $VMM_DIR_Q/target/release/vmm ]]; then
  (
    cd $VMM_DIR_Q
    cargo build --release --features boot
  )
fi
cd $ORCH_DIR_Q

export TARIT_API_KEY=$API_KEY_Q
export TARIT_VMM_BIN=$VMM_DIR_Q/target/release/vmm
export TARIT_KERNEL=$KERNEL_Q
export TARIT_ROOTFS=$ROOTFS_Q
export TARIT_ENABLE_NET=1
export TARIT_HOST_ID=\$(hostname)
export TARIT_RPC_ADDR="http://127.0.0.1:8080"
export TARIT_LISTEN=$LISTEN_Q
export TARIT_SOCKET_DIR="\$HOME/.taritd/sockets"
export TARIT_DB="\$HOME/.taritd/fleet.db"
export TARIT_CONFIG="\$HOME/.taritd/none.toml"
export TARIT_WARM_POOL=0
test -r "\$TARIT_KERNEL"
test -r "\$TARIT_ROOTFS"

# Optional Postgres fleet sync (MIT/Apache tokio-postgres client)
if [[ -f ~/.taritd/cp-rds.env ]]; then
  source ~/.taritd/cp-rds.env
  if [[ -n "\${TARIT_RDS_CA_FILE:-}" && ! -f "\$TARIT_RDS_CA_FILE" ]]; then
    curl -sf -o "\$TARIT_RDS_CA_FILE" https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem
  fi
fi

export TARIT_LOG="\$HOME/.taritd/taritd.log"
TARIT_PID=\$(sudo -n -E sh -c \
  'nohup ./target/release/taritd >"\$TARIT_LOG" 2>&1 & echo \$!')
printf '%s\n' "\$TARIT_PID" > "\$PID_FILE"
sleep 3
sudo kill -0 "\$TARIT_PID"
curl -sf http://127.0.0.1:8080/health | grep -q ok

export TARIT_URL=http://127.0.0.1:8080
chmod +x tests/e2e_c8i.sh
./tests/e2e_c8i.sh
REMOTE

echo "deploy + e2e OK"
