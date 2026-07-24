#!/usr/bin/env bash
# Release gate for a candidate guest kernel. Runs every orch/tests/e2e_*.sh
# program against the candidate on one isolated Linux/KVM host.
set -Eeuo pipefail

HERE="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=lib/preflight.sh
. "$HERE/lib/preflight.sh"

require_kvm
require_root
require_tools

for tool in cmp flock ip mktemp mountpoint nft psql sha256sum sqlite3 stat timeout; do
  require_cmd "$tool" "install the full c8i e2e dependency set"
done
: "${TARIT_KERNEL:?set TARIT_KERNEL to the candidate release kernel}"
: "${TARIT_ROOTFS:?set TARIT_ROOTFS to the candidate test rootfs}"
require_fixtures
# shellcheck source=../vmm/guest/kernel-version.env
. "$REPO_ROOT/vmm/guest/kernel-version.env"
candidate_sha256="$(sha256sum "$TARIT_KERNEL" | awk '{print $1}')"
[ "$candidate_sha256" = "$KERNEL_ARTIFACT_SHA256" ] ||
  die "candidate kernel sha256 $candidate_sha256 does not match release pin $KERNEL_ARTIFACT_SHA256"
if nft list table ip taritd_nat >/dev/null 2>&1; then
  die "refusing promotion with a pre-existing taritd_nat nft table"
fi
if ip -o link show | awk -F': ' '$2 ~ /^insta[0-9]+(@|$)/ { found=1 } END { exit !found }'; then
  die "refusing promotion with a pre-existing managed insta TAP"
fi
if nft list tables 2>/dev/null | grep -q '^table netdev taritd_ingress_'; then
  die "refusing promotion with a pre-existing managed ingress nft table"
fi
if [[ "${TARIT_CADDY_BIN:-caddy}" == */* ]]; then
  [ -x "$TARIT_CADDY_BIN" ] || die "configured Caddy is not executable: $TARIT_CADDY_BIN"
else
  require_cmd "${TARIT_CADDY_BIN:-caddy}" "install Caddy for the shares gate"
fi

: "${TARIT_DATABASE_URL:?set TARIT_DATABASE_URL for the fleet e2e gates}"
: "${TARIT_RDS_CA_FILE:?set TARIT_RDS_CA_FILE for the fleet e2e gates}"

TARIT_SHARE_ROOTFS="${TARIT_SHARE_ROOTFS:-$TARIT_ROOTFS}"
BASE_ROOTFS="${BASE_ROOTFS:-$TARIT_ROOTFS}"
EGRESS_TEST_IP="${EGRESS_TEST_IP:-1.1.1.1}"
EGRESS_TEST_PORT="${EGRESS_TEST_PORT:-443}"
SUITE_TIMEOUT="${KERNEL_PROMOTION_SUITE_TIMEOUT:-1800}"

export TARIT_KERNEL TARIT_ROOTFS TARIT_SHARE_ROOTFS BASE_ROOTFS
export TARIT_DATABASE_URL TARIT_RDS_CA_FILE TARIT_CADDY_BIN
export EGRESS_TEST_IP EGRESS_TEST_PORT

VMM_DEBUG="$REPO_ROOT/vmm/target/debug/vmm"
VMM_RELEASE="$REPO_ROOT/vmm/target/release/vmm"
TARIT_DEBUG="$REPO_ROOT/orch/target/debug/taritd"
TARIT_RELEASE="$REPO_ROOT/orch/target/release/taritd"

for binary in "$VMM_DEBUG" "$VMM_RELEASE" "$TARIT_DEBUG" "$TARIT_RELEASE"; do
  [ -x "$binary" ] || die "required promotion binary is missing: $binary"
done

run_c8i_api_suite() (
  local run_dir pid
  run_dir="$(mktemp -d /tmp/tarit-kernel-c8i.XXXXXX)"
  mkdir -p "$run_dir/sockets"

  env -u TARIT_DATABASE_URL -u TARIT_RDS_CA_FILE \
    TARIT_API_KEY=kernel-candidate \
    TARIT_LISTEN=127.0.0.1:8080 \
    TARIT_RPC_ADDR=http://127.0.0.1:8080 \
    TARIT_HOST_ID=kernel-candidate \
    TARIT_VMM_BIN="$VMM_RELEASE" \
    TARIT_KERNEL="$TARIT_KERNEL" \
    TARIT_ROOTFS="$TARIT_ROOTFS" \
    TARIT_ROOTFS_READONLY=1 \
    TARIT_ENABLE_NET=1 \
    TARIT_SOCKET_DIR="$run_dir/sockets" \
    TARIT_DB="$run_dir/fleet.db" \
    TARIT_CONFIG="$run_dir/none.toml" \
    TARIT_WARM_POOL=0 \
    "$TARIT_RELEASE" serve >"$run_dir/taritd.log" 2>&1 &
  pid=$!

  # shellcheck disable=SC2329 # Invoked by the EXIT trap.
  cleanup_c8i_api_suite() {
    local status=$?
    if [ "$status" -ne 0 ]; then
      echo "--- e2e_c8i taritd log tail ---" >&2
      tail -80 "$run_dir/taritd.log" >&2 || true
    fi
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    rm -rf "$run_dir"
    return "$status"
  }
  trap cleanup_c8i_api_suite EXIT

  for _ in $(seq 1 30); do
    curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 && break
    sleep 1
  done
  curl -fsS http://127.0.0.1:8080/health >/dev/null
  TARIT_API_KEY=kernel-candidate TARIT_URL=http://127.0.0.1:8080 \
    bash "$REPO_ROOT/orch/tests/e2e_c8i.sh"
)

reclaim_managed_network_state() {
  local listing
  if ip -o link show | awk -F': ' '$2 ~ /^insta[0-9]+(@|$)/ { found=1 } END { exit !found }'; then
    die "suite leaked a managed insta TAP"
  fi
  if nft list tables 2>/dev/null | grep -q '^table netdev taritd_ingress_'; then
    die "suite leaked a managed ingress nft table"
  fi
  if listing=$(nft -a list table ip taritd_nat 2>/dev/null); then
    if grep -q 'vm=' <<<"$listing"; then
      die "suite leaked tagged taritd_nat policy"
    fi
    nft delete table ip taritd_nat
  fi
}

SUITES=(
  e2e_smoke_mono.sh
  e2e_c8i.sh
  e2e_cli.sh
  e2e_ssh_pty.sh
  e2e_image_pipeline.sh
  e2e_warmpool_restore.sh
  e2e_multitenant.sh
  e2e_net_scale.sh
  e2e_lifecycle.sh
  e2e_suspend_resume.sh
  e2e_snapshot_disk.sh
  e2e_cpu_refill.sh
  e2e_egress_recovery.sh
  e2e_shares.sh
  e2e_cluster.sh
  e2e_autoscale.sh
  e2e_ha_failover.sh
  e2e_pg_blip.sh
  e2e_usage_audit.sh
)

[ "${#SUITES[@]}" -eq 19 ] || die "kernel promotion gate must contain exactly 19 suites"

for suite in "${SUITES[@]}"; do
  info "kernel promotion: $suite"
  suite_log="$(mktemp "/tmp/kernel-promotion-${suite%.sh}.XXXXXX")"
  set +e
  if [ "$suite" = e2e_c8i.sh ]; then
    run_c8i_api_suite 2>&1 | tee "$suite_log"
  elif [[ "$suite" =~ ^e2e_(shares|cluster|autoscale|ha_failover|pg_blip|usage_audit)\.sh$ ]]; then
    timeout --foreground "$SUITE_TIMEOUT" bash "$REPO_ROOT/orch/tests/$suite" 2>&1 | tee "$suite_log"
  else
    timeout --foreground "$SUITE_TIMEOUT" \
      env -u TARIT_DATABASE_URL -u TARIT_RDS_CA_FILE \
      bash "$REPO_ROOT/orch/tests/$suite" 2>&1 | tee "$suite_log"
  fi
  suite_status="${PIPESTATUS[0]}"
  set -e
  if [ "$suite_status" -ne 0 ]; then
    rm -f "$suite_log"
    reclaim_managed_network_state
    die "$suite failed with status $suite_status"
  fi
  if grep -q '^SKIP:' "$suite_log"; then
    rm -f "$suite_log"
    reclaim_managed_network_state
    die "$suite reported SKIP"
  fi
  rm -f "$suite_log"
  reclaim_managed_network_state
done

echo "RESULT: KERNEL_PROMOTION_E2E_PASS (19/19)"
