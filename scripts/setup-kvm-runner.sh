#!/usr/bin/env bash
# Register this host as the self-hosted runner that .github/workflows/kvm.yml
# needs (labels self-hosted, linux, x64, kvm).
#
#   sudo RUNNER_TOKEN=<token> scripts/setup-kvm-runner.sh
#
# RUNNER_TOKEN is the registration token from the repository's Settings ->
# Actions -> Runners -> New self-hosted runner. It expires an hour after it is
# shown. Running the script again keeps the existing registration and only
# reinstalls the runner and its service (no token needed). To register again,
# pass RUNNER_REMOVE_TOKEN (the runner's Remove button shows it) together with
# a new RUNNER_TOKEN.
#
# What this does to the host:
#   - creates the system user `tarit-ci` (home /var/lib/tarit-ci), in the kvm
#     group;
#   - lets `tarit-ci` run any command as root without a password. The KVM
#     steps run under `sudo -n` (TAP networking, the jailer, OCI unpack), so
#     anyone who can push to main can run code as root here. The workflow
#     refuses every ref except main: keep main protected;
#   - installs the Actions runner (pinned version, checksum checked) in
#     /var/lib/tarit-ci/runner as a systemd service;
#   - copies the guest kernel and rootfs to the paths the workflow reads,
#     /tmp/vmlinux.microvm and /tmp/vsock-rootfs.ext4, each time the service
#     starts, because /tmp does not survive a reboot.
#
# Undo: sudo /var/lib/tarit-ci/runner/svc.sh uninstall, remove the runner in
# the repository settings, then `sudo userdel -r tarit-ci` and
# `sudo rm /etc/sudoers.d/tarit-ci`.
set -euo pipefail

RUNNER_VERSION=2.338.2
RUNNER_SHA256=baebf204307bac6193480852732f0aba2ae97b6c5597c244f4d5558390147c40
REPO_URL=${REPO_URL:-https://github.com/instavm/tarit}
REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
KERNEL=${KERNEL:-$REPO_ROOT/guest-assets/vmlinux}
ROOTFS=${ROOTFS:-$REPO_ROOT/guest-assets/rootfs.ext4}
RUNNER_NAME=${RUNNER_NAME:-$(hostname -s)-kvm}
CI_USER=tarit-ci
CI_HOME=/var/lib/$CI_USER

die() { echo "setup-kvm-runner: $*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root (sudo)"
[ "$(uname -m)" = x86_64 ] || die "the KVM job runs on x86_64 only"
[ -c /dev/kvm ] || die "/dev/kvm is missing"
[ -r "$KERNEL" ] || die "guest kernel not found at $KERNEL (set KERNEL=)"
[ -r "$ROOTFS" ] || die "guest rootfs not found at $ROOTFS (set ROOTFS=)"
for tool in curl sha256sum tar systemctl visudo; do
  command -v "$tool" >/dev/null || die "$tool is required"
done

echo "== user $CI_USER"
if ! id "$CI_USER" >/dev/null 2>&1; then
  useradd --system --create-home --home-dir "$CI_HOME" --shell /bin/bash "$CI_USER"
fi
getent group kvm >/dev/null && usermod -aG kvm "$CI_USER"
sudoers=/etc/sudoers.d/$CI_USER
printf '%s ALL=(ALL) NOPASSWD: ALL\n' "$CI_USER" > "$sudoers.tmp"
chmod 0440 "$sudoers.tmp"
visudo -cf "$sudoers.tmp" >/dev/null || die "generated sudoers entry does not parse"
mv "$sudoers.tmp" "$sudoers"

echo "== guest assets"
install -d -o "$CI_USER" -g "$CI_USER" -m 0755 "$CI_HOME/assets"
install -o "$CI_USER" -g "$CI_USER" -m 0644 "$KERNEL" "$CI_HOME/assets/vmlinux.microvm"
install -o "$CI_USER" -g "$CI_USER" -m 0644 "$ROOTFS" "$CI_HOME/assets/vsock-rootfs.ext4"

echo "== Actions runner $RUNNER_VERSION"
runner_dir=$CI_HOME/runner
tarball=$CI_HOME/actions-runner-linux-x64-$RUNNER_VERSION.tar.gz
if [ ! -f "$tarball" ]; then
  curl -fsSL -o "$tarball.part" \
    "https://github.com/actions/runner/releases/download/v$RUNNER_VERSION/actions-runner-linux-x64-$RUNNER_VERSION.tar.gz"
  mv "$tarball.part" "$tarball"
fi
echo "$RUNNER_SHA256  $tarball" | sha256sum -c - >/dev/null || {
  rm -f "$tarball"
  die "runner tarball checksum mismatch; removed it, run again"
}
if [ -x "$runner_dir/svc.sh" ] && [ -f "$runner_dir/.service" ]; then
  (cd "$runner_dir" && ./svc.sh stop || true; ./svc.sh uninstall || true)
fi
install -d -o "$CI_USER" -g "$CI_USER" "$runner_dir"
tar -xzf "$tarball" -C "$runner_dir"
chown -R "$CI_USER:$CI_USER" "$runner_dir"
"$runner_dir/bin/installdependencies.sh" >/dev/null

# The runner refuses to configure itself twice, so an existing registration
# is kept unless it is removed first with its removal token.
if [ -f "$runner_dir/.runner" ] && [ -n "${RUNNER_REMOVE_TOKEN:-}" ]; then
  sudo -u "$CI_USER" -H bash -c "cd '$runner_dir' && ./config.sh remove --token '$RUNNER_REMOVE_TOKEN'"
fi
if [ -f "$runner_dir/.runner" ]; then
  echo "== keeping the existing runner registration"
else
  [ -n "${RUNNER_TOKEN:-}" ] || die "set RUNNER_TOKEN (Settings -> Actions -> Runners -> New self-hosted runner)"
  sudo -u "$CI_USER" -H bash -c "cd '$runner_dir' && ./config.sh --unattended --replace \
    --url '$REPO_URL' --token '$RUNNER_TOKEN' --name '$RUNNER_NAME' \
    --labels self-hosted,linux,x64,kvm --work _work"
fi

echo "== service"
(cd "$runner_dir" && ./svc.sh install "$CI_USER")
unit=$(cat "$runner_dir/.service")
install -d "/etc/systemd/system/$unit.d"
cat > "/etc/systemd/system/$unit.d/tarit-assets.conf" <<EOF
[Service]
ExecStartPre=+/usr/bin/install -o $CI_USER -g $CI_USER -m 0644 $CI_HOME/assets/vmlinux.microvm /tmp/vmlinux.microvm
ExecStartPre=+/usr/bin/install -o $CI_USER -g $CI_USER -m 0644 $CI_HOME/assets/vsock-rootfs.ext4 /tmp/vsock-rootfs.ext4
EOF
systemctl daemon-reload
(cd "$runner_dir" && ./svc.sh start)
systemctl --no-pager --lines=0 status "$unit" || true
echo
echo "Runner '$RUNNER_NAME' registered for $REPO_URL. The KVM workflow runs on"
echo "pushes to main and on manual dispatch from main."
