#!/usr/bin/env bash
# tests/e2e_image_pipeline.sh — build a golden rootfs from an OCI image via the
# orchestrator, then boot a VM and assert Ubuntu identity. Run as root on c8i.
set -uo pipefail
umask 077

ORCH_ROOT="${ORCH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
VMM_ROOT="${VMM_ROOT:-$ORCH_ROOT/../vmm}"
TARITD_HOME="${TARITD_HOME:-$HOME/.taritd}"
TARIT="${TARIT:-$ORCH_ROOT/target/debug/taritd}"
export TARIT_API_KEY="img-key"
export TARIT_LISTEN="${TARIT_LISTEN:-127.0.0.1:8080}"
export TARIT_VMM_BIN="${TARIT_VMM_BIN:-$VMM_ROOT/target/debug/vmm}"
export TARIT_KERNEL="${TARIT_KERNEL:-/tmp/vmlinux.microvm}"
export TARIT_ROOTFS="${TARIT_ROOTFS:-/tmp/vsock-rootfs.ext4}"
export TARIT_ROOTFS_READONLY="0"
export TARIT_ENABLE_NET="0"
export TARIT_MAX_VMS="6"
export TARIT_SOCKET_DIR="${TARIT_SOCKET_DIR:-$TARITD_HOME/sockets}"
export TARIT_DB="${TARIT_DB:-$TARITD_HOME/fleet.db}"
export TARIT_IMAGES_DIR="${TARIT_IMAGES_DIR:-$TARITD_HOME/images}"
export TARIT_VMM_AGENT="${TARIT_VMM_AGENT:-$VMM_ROOT/guest/agent/vmm-agent}"
export TARIT_BASE_URL="${TARIT_BASE_URL:-http://$TARIT_LISTEN}"
export TARIT_CONFIG="${TARIT_CONFIG:-/tmp/img-empty.toml}"
TARIT_LOG="${TARIT_LOG:-/tmp/taritd-img.log}"
export RUST_LOG="info"
: > "$TARIT_CONFIG"
PASS=1
install -d -m 0700 "$TARITD_HOME" "$TARIT_SOCKET_DIR" "$TARIT_IMAGES_DIR"
chown "$(id -u):$(id -g)" "$TARITD_HOME" "$TARIT_SOCKET_DIR" "$TARIT_IMAGES_DIR"
chmod 0700 "$TARITD_HOME" "$TARIT_SOCKET_DIR" "$TARIT_IMAGES_DIR"
for private_dir in "$TARIT_SOCKET_DIR/overlays" "$TARIT_SOCKET_DIR/snapshots"; do
  if [ -d "$private_dir" ]; then
    chown "$(id -u):$(id -g)" "$private_dir"
    chmod 0700 "$private_dir"
  fi
done
rm -f "$TARIT_DB"
rm -f "$TARIT_IMAGES_DIR"/ubuntu2404*.ext4 2>/dev/null || true
make -C "$VMM_ROOT/guest/agent" >/dev/null 2>&1 || true
echo "=== check OCI tooling ==="
for t in umoci skopeo; do command -v "$t" >/dev/null 2>&1 && echo "  $t: $(command -v $t)" || echo "  $t: MISSING"; done

"$TARIT" serve >"$TARIT_LOG" 2>&1 & SP=$!
sleep 4
cleanup() { [ -n "${VM_ID:-}" ] && "$TARIT" vm delete "$VM_ID" >/dev/null 2>&1 || true; kill "$SP" 2>/dev/null || true; sleep 1; }
trap cleanup EXIT

echo "=== taritd image build --oci ubuntu:24.04 --name ubuntu2404 (slow: pull+convert) ==="
"$TARIT" image build --oci ubuntu:24.04 --name ubuntu2404 2>&1 | tail -8 || { echo "FAIL: image build"; PASS=0; }

echo "=== taritd image ls ==="
"$TARIT" image ls 2>&1 | tail -6
"$TARIT" image ls 2>&1 | grep -q ubuntu2404 || { echo "FAIL: ubuntu2404 not registered"; PASS=0; }
"$TARIT" --json image ls | python3 -c '
import json, re, sys
rows = json.load(sys.stdin)
row = next((item for item in rows if item["name"] == "ubuntu2404"), None)
assert row is not None, "ubuntu2404 record missing"
sha = re.compile(r"^sha256:[0-9a-f]{64}$")
assert sha.fullmatch(row.get("source_digest") or ""), "source digest missing"
assert sha.fullmatch(row.get("rootfs_digest") or ""), "rootfs digest missing"
assert sha.fullmatch(row.get("agent_digest") or ""), "agent digest missing"
assert row["source_ref"].endswith(row["source_digest"]), "source is not digest-pinned"
' || { echo "FAIL: immutable image metadata"; PASS=0; }

echo "=== create VM from image ubuntu2404 ==="
VM_ID=$("$TARIT" --json vm create --image ubuntu2404 --vcpus 1 --memory-mib 512 2>/dev/null | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
echo "  VM_ID=$VM_ID"
[ -n "$VM_ID" ] || { echo "FAIL: create from image"; tail -20 "$TARIT_LOG"; PASS=0; }
echo "  (20s boot)"; sleep 20

echo "=== assert Ubuntu guest identity ==="
OUT=$("$TARIT" exec "$VM_ID" "grep -E '^(ID|VERSION_ID)=' /etc/os-release" 2>&1)
echo "  $OUT"
echo "$OUT" | grep -q '^ID=ubuntu$' || { echo "FAIL: guest is not Ubuntu"; PASS=0; }
echo "$OUT" | grep -q '^VERSION_ID="24.04"$' || { echo "FAIL: guest is not Ubuntu 24.04"; PASS=0; }

echo "=== snapshot admitted OCI VM and publish immutable artifact ==="
SNAPSHOT_JSON=$(curl -fsS -H "X-API-Key: $TARIT_API_KEY" \
  -H 'Content-Type: application/json' -d '{"diff":false}' \
  "$TARIT_BASE_URL/v1/vms/$VM_ID/snapshot") || {
  echo "FAIL: OCI snapshot request"; PASS=0; SNAPSHOT_JSON='{}'
}
SNAPSHOT_ID=$(printf '%s' "$SNAPSHOT_JSON" | python3 -c '
import json,sys,uuid
row=json.load(sys.stdin)
assert set(row) == {"snapshot_id"}, row
uuid.UUID(row["snapshot_id"])
print(row["snapshot_id"])
' 2>/dev/null) || { echo "FAIL: snapshot response is not opaque"; PASS=0; SNAPSHOT_ID=; }

if [ -n "$SNAPSHOT_ID" ]; then
  python3 - "$TARIT_DB" "$SNAPSHOT_ID" <<'PY' || { echo "FAIL: immutable artifact publication"; PASS=0; }
import re, sqlite3, sys
db, artifact_id = sys.argv[1:]
sha = re.compile(r"^sha256:[0-9a-f]{64}$")
with sqlite3.connect(db) as conn:
    artifact = conn.execute(
        "select owner_key,host_id,storage_locator,status,content_digest,size_bytes,"
        "immutable_image_digest,agent_digest,integrity_manifest_digest,chunk_size_bytes,"
        "chunk_count,replication_state,reference_count from artifacts where artifact_id=?",
        (artifact_id,),
    ).fetchone()
    assert artifact, "artifact row missing"
    owner, host, locator, status, content, size, image, agent, manifest, chunk_size, chunks, replication, refs = artifact
    assert owner and host and locator.startswith("/"), artifact
    assert status == "available" and replication == "ready" and refs == 0, artifact
    assert size > 0 and chunk_size == 65536 and chunks > 0, artifact
    for digest in (content, image, agent, manifest):
        assert sha.fullmatch(digest), digest
    replica = conn.execute(
        "select owner_key,host_id,failure_domain,storage_locator,status,content_digest,"
        "size_bytes,integrity_manifest_digest,verified_at from artifact_replicas "
        "where artifact_id=?", (artifact_id,),
    ).fetchall()
    assert len(replica) == 1, replica
    assert replica[0][0] == owner and replica[0][1] == host, replica
    assert replica[0][2] == host and replica[0][3] == locator, replica
    assert replica[0][4] == "available" and replica[0][5] == content, replica
    assert replica[0][6] == size and replica[0][7] == manifest and replica[0][8], replica
PY

  BRANCH_ID=$(python3 -c 'import uuid; print(uuid.uuid4())')
  BRANCH_BODY=$(python3 - "$BRANCH_ID" "$SNAPSHOT_ID" "$VM_ID" <<'PY'
import json,sys
print(json.dumps({"branch_id":sys.argv[1], "name":"oci-main", "head_artifact_id":sys.argv[2], "source_vm_id":sys.argv[3]}))
PY
)
  BRANCH_RESPONSE="$TARITD_HOME/branch-response.json"
  BRANCH_STATUS=$(curl -sS -o "$BRANCH_RESPONSE" -w '%{http_code}' \
    -H "X-API-Key: $TARIT_API_KEY" -H 'Content-Type: application/json' \
    -d "$BRANCH_BODY" "$TARIT_BASE_URL/v1/branches")
  [ "$BRANCH_STATUS" = 201 ] || { echo "FAIL: branch create HTTP $BRANCH_STATUS"; PASS=0; }
  python3 - "$BRANCH_RESPONSE" "$BRANCH_ID" "$SNAPSHOT_ID" <<'PY' || { echo "FAIL: branch response leaked private placement or changed identity"; PASS=0; }
import json,sys
row=json.load(open(sys.argv[1]))
assert row["branch_id"] == sys.argv[2] and row["head_artifact_id"] == sys.argv[3], row
assert not ({"owner_key", "host_id", "storage_locator", "failure_domain"} & set(row)), row
PY
  REPLAY_STATUS=$(curl -sS -o /dev/null -w '%{http_code}' \
    -H "X-API-Key: $TARIT_API_KEY" -H 'Content-Type: application/json' \
    -d "$BRANCH_BODY" "$TARIT_BASE_URL/v1/branches")
  [ "$REPLAY_STATUS" = 200 ] || { echo "FAIL: idempotent branch replay HTTP $REPLAY_STATUS"; PASS=0; }
  python3 - "$TARIT_DB" "$SNAPSHOT_ID" <<'PY' || { echo "FAIL: replay changed artifact reference count"; PASS=0; }
import sqlite3,sys
with sqlite3.connect(sys.argv[1]) as db:
    refs=db.execute("select reference_count from artifacts where artifact_id=?", (sys.argv[2],)).fetchone()
    assert refs == (1,), refs
PY

  RESTORED_ID=$(curl -fsS -H "X-API-Key: $TARIT_API_KEY" \
    -H 'Content-Type: application/json' -d "{\"snapshot_id\":\"$SNAPSHOT_ID\"}" \
    "$TARIT_BASE_URL/v1/restore" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])') || {
    echo "FAIL: restore admitted OCI artifact"; PASS=0; RESTORED_ID=
  }
  if [ -n "$RESTORED_ID" ]; then
    RESTORED_OUT=$("$TARIT" exec "$RESTORED_ID" "grep -E '^(ID|VERSION_ID)=' /etc/os-release" 2>&1)
    echo "$RESTORED_OUT" | grep -q '^ID=ubuntu$' || { echo "FAIL: restored artifact is not Ubuntu"; PASS=0; }
    "$TARIT" vm delete "$RESTORED_ID" >/dev/null 2>&1 || PASS=0
  fi
  curl -fsS -X DELETE -H "X-API-Key: $TARIT_API_KEY" \
    "$TARIT_BASE_URL/v1/branches/$BRANCH_ID" >/dev/null || { echo "FAIL: branch delete"; PASS=0; }
  python3 - "$TARIT_DB" "$SNAPSHOT_ID" <<'PY' || { echo "FAIL: branch delete did not release artifact"; PASS=0; }
import sqlite3,sys
with sqlite3.connect(sys.argv[1]) as db:
    refs=db.execute("select reference_count from artifacts where artifact_id=?", (sys.argv[2],)).fetchone()
    assert refs == (0,), refs
PY
fi

echo ""
if [ "$PASS" = 1 ]; then echo "RESULT: IMAGE_PIPELINE_PASS"; exit 0; else echo "RESULT: IMAGE_PIPELINE_FAIL"; exit 1; fi
