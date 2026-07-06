#!/usr/bin/env bash
# Diagnose where a networked (TARIT_ENABLE_NET=1) create hangs on the c8i box.
# Captures the taritd log and the per-VM `vmm serve` log at the hang point,
# plus host tap/nft state, then tears everything down. Run under sudo.
set -u

ORCH="${ORCH:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM="${VMM:-$ORCH/../vmm/target/debug/vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
KERNEL=/tmp/vmlinux.microvm
ROOTFS=${ROOTFS:-/tmp/net-rootfs.ext4}
SOCKDIR="${SOCKDIR:-$TARITD_HOME/sockets}"
LOG=/tmp/net_diag_taritd.log
DB=/tmp/net_diag_fleet.db
EMPTY_CFG=/tmp/net_diag_empty.toml

echo "=== cleanup previous ==="
for p in $(pgrep -f "target/debug/taritd") $(pgrep -f "vmm serve"); do kill "$p" 2>/dev/null; done
sleep 1
rm -f "$LOG" "$DB" /tmp/net_diag_empty.toml /tmp/taritd_net.json
: > "$EMPTY_CFG"
mkdir -p "$SOCKDIR"
for t in $(ip -o link show | grep -oE "insta[a-z0-9_-]+" | sort -u); do ip link del "$t" 2>/dev/null; done

echo "=== rootfs in use: $ROOTFS ==="
ls -la "$ROOTFS" 2>&1

echo "=== start taritd (net enabled) ==="
cd "$ORCH" || exit 1
TARIT_API_KEY=k \
TARIT_LISTEN=127.0.0.1:8080 \
TARIT_VMM_BIN="$VMM" \
TARIT_KERNEL="$KERNEL" \
TARIT_ROOTFS="$ROOTFS" \
TARIT_ENABLE_NET=1 \
TARIT_SOCKET_DIR="$SOCKDIR" \
TARIT_DB="$DB" \
TARIT_CONFIG="$EMPTY_CFG" \
TARIT_NET_STATE=/tmp/taritd_net.json \
RUST_LOG=info,taritd=debug,tarit_vmm_client=debug \
"$ORCH/target/debug/taritd" serve >"$LOG" 2>&1 &
TARIT_PID=$!
sleep 3
echo "taritd pid=$TARIT_PID alive=$(kill -0 $TARIT_PID 2>/dev/null && echo yes || echo no)"

echo "=== fire create in background (40s client timeout) ==="
curl -s -m 40 -H "X-API-Key: k" -H "Content-Type: application/json" \
  -d '{"memory_mib":256,"vcpus":1}' \
  http://127.0.0.1:8080/v1/vms >/tmp/net_diag_createresp 2>&1 &
CURL_PID=$!

sleep 12
echo "=== taps during create ==="
ip -o link show | grep -oE "insta[a-z0-9_-]+" | sort -u
echo "=== nft ruleset (taritd) ==="
nft list table ip taritd_nat 2>&1 | head -20
echo "=== taritd log tail ==="
tail -30 "$LOG"
echo "=== per-VM vmm serve logs (if any) ==="
for f in $(ls -t /tmp/*.log 2>/dev/null | grep -v net_diag | head -3); do echo "--- $f ---"; tail -15 "$f"; done
echo "=== vmm serve processes ==="
pgrep -af "vmm serve" | head
echo "=== create response so far ==="
cat /tmp/net_diag_createresp 2>/dev/null; echo

echo "=== teardown ==="
kill "$CURL_PID" 2>/dev/null
for p in $(pgrep -f "target/debug/taritd") $(pgrep -f "vmm serve"); do kill "$p" 2>/dev/null; done
sleep 1
for t in $(ip -o link show | grep -oE "insta[a-z0-9_-]+" | sort -u); do ip link del "$t" 2>/dev/null; done
nft delete table ip taritd_nat 2>/dev/null
echo "=== done ==="
