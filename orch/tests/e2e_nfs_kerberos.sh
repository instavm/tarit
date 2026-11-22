#!/usr/bin/env bash
# Run only on a fresh disposable Ubuntu 24.04 VM. This sets its hostname and
# Kerberos configuration, installs nfsd, and runs the two provider profiles.
# From a checkout with rustup installed, run:
#   sudo env TARIT_CARGO_BIN="$(rustup which cargo)" TARIT_RUSTC_BIN="$(rustup which rustc)" \
#     bash orch/tests/e2e_nfs_kerberos.sh --disposable-vm
# Alternatively pass TARIT_NFS_TEST_BINARY with the path to a tarit-volume
# --lib test executable built for the guest using cargo test --no-run.
set -euo pipefail
if [[ ${1:-} != --disposable-vm || $EUID != 0 ]]; then
  echo "Usage: sudo e2e_nfs_kerberos.sh --disposable-vm" >&2
  exit 2
fi
. /etc/os-release
[[ $ID == ubuntu && $VERSION_ID == 24.04 ]]
[[ ! -e /etc/krb5.keytab && ! -e /var/lib/krb5kdc/principal ]]
if [[ -s /etc/exports ]] && grep -Eq '^[[:space:]]*[^#[:space:]]' /etc/exports; then
  echo "Refusing to reconfigure a machine with NFS exports" >&2
  exit 1
fi
[[ -z $(find /etc/exports.d -maxdepth 1 -name '*.exports' -print -quit 2>/dev/null) ]]

apt-get -qq update
env DEBIAN_FRONTEND=noninteractive apt-get -qq install --no-install-recommends \
  nfs-kernel-server nfs-common krb5-kdc krb5-admin-server krb5-user
if ! modprobe nfsd || ! modprobe rpcsec_gss_krb5; then
  apt-get -qq install "linux-modules-extra-$(uname -r)"
  modprobe nfsd
  modprobe rpcsec_gss_krb5
fi

umask 077
realm=TARIT.TEST
fqdn=nfs.tarit.test
hostnamectl set-hostname "$fqdn"
echo "127.0.0.2 $fqdn nfs" >> /etc/hosts
cat >/etc/krb5.conf <<EOF
[libdefaults]
  default_realm = $realm
  dns_lookup_kdc = false
  rdns = false
[realms]
  $realm = {
    kdc = 127.0.0.1
  }
EOF
cat >>/etc/idmapd.conf <<EOF

[Translation]
  GSS-Methods = static,nsswitch
[Static]
  root/$fqdn@$realm = root
EOF
kdb5_util -r "$realm" -P "$(openssl rand -hex 32)" create -s
for service in root nfs; do
  kadmin.local -q "addprinc -randkey $service/$fqdn"
  kadmin.local -q "ktadd -norandkey $service/$fqdn"
done
systemctl restart krb5-kdc
kinit -k "root/$fqdn"
kvno "nfs/$fqdn"
kdestroy
systemctl restart rpc-gssd rpc-svcgssd
systemctl is-active rpc-gssd
systemctl is-active rpc-svcgssd

for security in sys krb5p; do
  TARIT_TEST_NFS_SECURITY="$security" TARIT_TEST_NFS_ENDPOINT="$fqdn" \
    bash "$(dirname "$0")/e2e_nfs_provider.sh"
done
