#!/usr/bin/env bash
# Wrapper to run the 3-node cluster e2e against the taritd-cp RDS on c8i.
# Sources the RDS env from TARITD_HOME (or TARITD_ENV), keeps paths
# env-overridable, bakes a dedicated rootfs copy, then runs e2e_cluster.sh.
set -u
ORCH_ROOT="${ORCH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"

set -a; . "${TARITD_ENV:-$TARITD_HOME/cp-rds.env}"; set +a
# Pin the CA with an env-overridable default so sudo callers can keep a stable path.
export TARIT_RDS_CA_FILE="${TARIT_RDS_CA_FILE:-$TARITD_HOME/rds-global-bundle.pem}"
export PGSSLMODE=require PGSSLROOTCERT="${PGSSLROOTCERT:-$TARIT_RDS_CA_FILE}" PGCONNECT_TIMEOUT=6

# Dedicated, agent-baked, journal-clean rootfs for the cluster VMs.
ROOTFS=/tmp/cluster-rootfs.ext4
cp -f /tmp/vsock-rootfs.ext4 "$ROOTFS"
make -C "$VMM_ROOT/guest/agent" >/dev/null 2>&1 || true
sh "$VMM_ROOT/guest/agent/bake-agent.sh" "$ROOTFS" "$VMM_ROOT/guest/agent/vmm-agent" >/dev/null 2>&1 || true
e2fsck -fy "$ROOTFS" >/dev/null 2>&1 || true

export TARIT_VMM_BIN="$VMM_ROOT/target/debug/vmm"
export TARIT_KERNEL=/tmp/vmlinux.microvm
export TARIT_ROOTFS="$ROOTFS"
export TARIT_ROOTFS_READONLY=1
export TARIT_DATABASE_URL
export TARIT_MAX_VMS="${TARIT_MAX_VMS:-2}"
# Give admission room to place across peers while async teardown of a prior
# case's VMs completes (debug-build teardown lags a tight window).
export TARIT_ADMISSION_TIMEOUT_MS="${TARIT_ADMISSION_TIMEOUT_MS:-4000}"

# Clear stale fleet rows from earlier aborted runs on this dedicated test DB so
# placement/capacity only sees the live cluster.
psql "$TARIT_DATABASE_URL" -qAtc "delete from fleet_vms; delete from fleet_hosts; delete from fleet_leader;" >/dev/null 2>&1 || true

# Clear stray cluster taritds (explicit PIDs; pkill is disallowed).
for p in $(pgrep -f 'target/release/taritd' 2>/dev/null); do kill "$p" 2>/dev/null; done
sleep 2

cd "$ORCH_ROOT"
bash tests/e2e_cluster.sh > /tmp/cluster-result.log 2>&1
echo "EXIT=$?" >> /tmp/cluster-result.log
