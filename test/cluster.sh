#!/usr/bin/env bash
# test/cluster.sh — provision an ephemeral N-node taritd cluster on EC2, run a
# true multi-host cluster + leader-failover test, then destroy everything it made.
#
# This runner executes on a workstation with AWS credentials and the cluster SSH
# key; it does NOT need local KVM (the guests run on the remote nodes). It is
# written to run on stock bash (including macOS bash 3.2).
#
#   AMI=<kvm-capable-ami> SUBNET=<subnet-id> VPC=<vpc-id> KEYNAME=<key-pair> \
#     ARTIFACT_HOST=<built-host-ip> NODES=4 test/cluster.sh
#
# SAFETY (the AWS account may be shared with other workloads):
#   * Every instance is tagged Project=tarit-e2e and TaritE2ERun=<RUN_ID>.
#   * A per-run security group (tarit-e2e-<RUN_ID>) carries intra-cluster traffic;
#     the shared prod SG is never modified.
#   * Teardown terminates ONLY instances whose TaritE2ERun tag equals this run's
#     RUN_ID (re-verified per instance id), then deletes the per-run SG. It never
#     matches by AMI/type/name and never touches any other instance.
set -uo pipefail
HERE="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
. "$HERE/lib/preflight.sh"

REGION="${REGION:-us-east-1}"
NODES="${NODES:-4}"
AMI="${AMI:?set AMI to your KVM-capable AMI id (e.g. an Ubuntu c8i image with nested virt)}"
TYPE="${TYPE:-c8i.xlarge}"
SUBNET="${SUBNET:?set SUBNET to the subnet id to launch the cluster nodes in}"
VPC="${VPC:?set VPC to the VPC id that owns SUBNET (used for the per-run security group)}"
KEYNAME="${KEYNAME:?set KEYNAME to your EC2 key pair name}"
# Nested virtualization must be enabled at launch (fresh instances have no /dev/kvm
# otherwise).
CPU_OPTIONS="${CPU_OPTIONS:-CoreCount=2,ThreadsPerCore=1,NestedVirtualization=enabled}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/$KEYNAME.pem}"
SSH_USER="${SSH_USER:-ubuntu}"
ARTIFACT_HOST="${ARTIFACT_HOST:?set ARTIFACT_HOST to a host with the repo built (binaries + kernel are copied from it)}"
SUBNET_CIDR="${SUBNET_CIDR:-172.31.32.0/20}"

require_cmd aws "install the AWS CLI v2"
require_cmd ssh; require_cmd scp; require_cmd curl; require_cmd python3
require_aws

MY_IP="${MY_IP:-$(curl -s https://checkip.amazonaws.com | tr -d '\n')}"
RUN_ID="tarit-e2e-$(date +%Y%m%d-%H%M%S)-$$"
STAGE="/tmp/$RUN_ID"; mkdir -p "$STAGE"
SSHO="-i $SSH_KEY -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=15"
API_KEY="cluster-e2e-key"
PEER_SECRET="$(head -c 18 /dev/urandom | base64 | tr -d '/+=')"
PASS=1; fail(){ warn "$*"; PASS=0; }
SG_ID=""

teardown() {
  info "=== teardown (run=$RUN_ID) ==="
  local ids id t
  ids=$(aws ec2 describe-instances --region "$REGION" \
    --filters "Name=tag:TaritE2ERun,Values=$RUN_ID" "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --query 'Reservations[].Instances[].InstanceId' --output text 2>/dev/null)
  for id in $ids; do
    t=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$id" \
      --query "Reservations[].Instances[].Tags[?Key=='TaritE2ERun']|[0][0].Value" --output text 2>/dev/null)
    if [ "$t" = "$RUN_ID" ]; then info "terminating $id"; aws ec2 terminate-instances --region "$REGION" --instance-ids "$id" >/dev/null
    else info "SKIP $id (tag '$t' != $RUN_ID)"; fi
  done
  [ -n "$ids" ] && aws ec2 wait instance-terminated --region "$REGION" --instance-ids $ids 2>/dev/null
  if [ -n "$SG_ID" ]; then
    for _ in $(seq 1 12); do aws ec2 delete-security-group --region "$REGION" --group-id "$SG_ID" 2>/dev/null && { info "deleted SG $SG_ID"; break; }; sleep 6; done
  fi
  rm -rf "$STAGE"
}
trap teardown EXIT
trap 'exit 143' TERM
trap 'exit 130' INT

# ------------------------------------------------------------------ ephemeral SG
info "creating per-run security group in $VPC"
SG_ID=$(aws ec2 create-security-group --region "$REGION" --vpc-id "$VPC" \
  --group-name "tarit-e2e-$RUN_ID" --description "ephemeral tarit cluster e2e $RUN_ID" \
  --tag-specifications "ResourceType=security-group,Tags=[{Key=Project,Value=tarit-e2e},{Key=TaritE2ERun,Value=$RUN_ID}]" \
  --query 'GroupId' --output text)
info "SG $SG_ID"
aws ec2 authorize-security-group-ingress --region "$REGION" --group-id "$SG_ID" \
  --ip-permissions "IpProtocol=-1,UserIdGroupPairs=[{GroupId=$SG_ID}]" >/dev/null
aws ec2 authorize-security-group-ingress --region "$REGION" --group-id "$SG_ID" \
  --ip-permissions "IpProtocol=tcp,FromPort=22,ToPort=22,IpRanges=[{CidrIp=$MY_IP/32}]" \
                   "IpProtocol=tcp,FromPort=8080,ToPort=8080,IpRanges=[{CidrIp=$MY_IP/32}]" >/dev/null

# ------------------------------------------------------------------ launch
info "launching $NODES x $TYPE"
IIDS=$(aws ec2 run-instances --region "$REGION" --image-id "$AMI" --instance-type "$TYPE" \
  --count "$NODES" --subnet-id "$SUBNET" --security-group-ids "$SG_ID" --key-name "$KEYNAME" \
  --cpu-options "$CPU_OPTIONS" \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Project,Value=tarit-e2e},{Key=TaritE2ERun,Value=$RUN_ID},{Key=Name,Value=$RUN_ID}]" \
  --query 'Instances[].InstanceId' --output text)
info "instances: $IIDS"
aws ec2 wait instance-running --region "$REGION" --instance-ids $IIDS

PUB=($(aws ec2 describe-instances --region "$REGION" --instance-ids $IIDS \
  --query 'Reservations[].Instances[].PublicIpAddress' --output text))
PRIV=($(aws ec2 describe-instances --region "$REGION" --instance-ids $IIDS \
  --query 'Reservations[].Instances[].PrivateIpAddress' --output text))
info "public:  ${PUB[*]}"
info "private: ${PRIV[*]}"

ssh_n(){ i="$1"; shift; ssh $SSHO "$SSH_USER@${PUB[$i]}" "$@"; }
info "waiting for SSH on all nodes"
i=0; while [ "$i" -lt "$NODES" ]; do
  n=0; while [ "$n" -lt 40 ]; do ssh_n "$i" true 2>/dev/null && break; sleep 5; n=$((n+1)); done
  i=$((i+1))
done

# ------------------------------------------------------------------ stage + deploy
ARTIFACT_REPO="${ARTIFACT_REPO:-/home/$SSH_USER/tarit}"
info "ensuring binaries + agent are built on $ARTIFACT_HOST"
ssh $SSHO "$SSH_USER@$ARTIFACT_HOST" "export PATH=\$HOME/.cargo/bin:\$PATH; cd $ARTIFACT_REPO && \
  make -C vmm/guest/agent >/dev/null 2>&1; \
  [ -x orch/target/release/taritd ] || ( cd orch && cargo build --release -p taritd ) >/dev/null 2>&1; \
  [ -x vmm/target/debug/vmm ] || ( cd vmm && cargo build -p vmm --features boot ) >/dev/null 2>&1" || die "artifact host build failed"
info "fetching binaries + kernel + agent from $ARTIFACT_HOST"
scp $SSHO "$SSH_USER@$ARTIFACT_HOST:$ARTIFACT_REPO/orch/target/release/taritd" "$STAGE/taritd"
scp $SSHO "$SSH_USER@$ARTIFACT_HOST:$ARTIFACT_REPO/vmm/target/debug/vmm" "$STAGE/vmm"
scp $SSHO "$SSH_USER@$ARTIFACT_HOST:/tmp/vmlinux.microvm" "$STAGE/vmlinux"
scp $SSHO "$SSH_USER@$ARTIFACT_HOST:$ARTIFACT_REPO/vmm/guest/agent/vmm-agent" "$STAGE/vmm-agent"

deploy_node(){
  i="$1"
  scp $SSHO "$STAGE/taritd" "$STAGE/vmm" "$STAGE/vmlinux" "$STAGE/vmm-agent" "$SSH_USER@${PUB[$i]}:/tmp/" 2>/dev/null
  ssh_n "$i" 'sudo mkdir -p /opt/tarit && sudo mv /tmp/taritd /tmp/vmm /tmp/vmlinux /tmp/vmm-agent /opt/tarit/ && \
    sudo chmod +x /opt/tarit/taritd /opt/tarit/vmm /opt/tarit/vmm-agent
    sudo modprobe kvm_intel 2>/dev/null || sudo modprobe kvm 2>/dev/null || true
    sudo apt-get update -qq >/dev/null 2>&1 || true
    command -v umoci  >/dev/null || sudo apt-get install -y -qq umoci  >/dev/null 2>&1
    command -v skopeo >/dev/null || sudo apt-get install -y -qq skopeo >/dev/null 2>&1
    sudo sh -c "/opt/tarit/vmm pull docker://ubuntu:22.04 --output /opt/tarit/rootfs.ext4 --agent /opt/tarit/vmm-agent >/opt/tarit/pull.log 2>&1"
    sudo e2fsck -fy /opt/tarit/rootfs.ext4 >/dev/null 2>&1 || true'
}
info "deploying to all nodes (parallel)"
i=0; while [ "$i" -lt "$NODES" ]; do deploy_node "$i" & i=$((i+1)); done; wait
info "verifying kvm + rootfs on all nodes"
i=0; while [ "$i" -lt "$NODES" ]; do
  ssh_n "$i" 'test -e /dev/kvm' && info "  node$i /dev/kvm ok" || fail "node$i has no /dev/kvm (nested virtualization unavailable on $TYPE?)"
  if ssh_n "$i" 'test -s /opt/tarit/rootfs.ext4'; then info "  node$i rootfs ok"
  else fail "node$i is missing /opt/tarit/rootfs.ext4 (pull failed)"; info "  node$i pull.log:"; ssh_n "$i" 'tail -10 /opt/tarit/pull.log' 2>/dev/null; fi
  i=$((i+1))
done

# ------------------------------------------------------------------ postgres on node0
PG_HOST="${PRIV[0]}"
DBURL="postgresql://taritd:taritd@$PG_HOST:5432/taritd?sslmode=disable"
info "installing Postgres on node0 ($PG_HOST)"
ssh_n 0 "sudo apt-get update -qq >/dev/null 2>&1; sudo apt-get install -y -qq postgresql >/dev/null 2>&1
  sudo -u postgres psql -qc \"DROP DATABASE IF EXISTS taritd;\" >/dev/null 2>&1
  sudo -u postgres psql -qc \"DROP ROLE IF EXISTS taritd;\" >/dev/null 2>&1
  sudo -u postgres psql -qc \"CREATE ROLE taritd LOGIN PASSWORD 'taritd';\" >/dev/null 2>&1
  sudo -u postgres psql -qc \"CREATE DATABASE taritd OWNER taritd;\" >/dev/null 2>&1
  PGCONF=\$(sudo -u postgres psql -qAtc 'show config_file'); PGDIR=\$(dirname \"\$PGCONF\")
  echo \"listen_addresses = '*'\" | sudo tee -a \"\$PGCONF\" >/dev/null
  echo \"host all all $SUBNET_CIDR md5\" | sudo tee -a \"\$PGDIR/pg_hba.conf\" >/dev/null
  sudo systemctl restart postgresql"

# ------------------------------------------------------------------ start cluster
start_taritd(){
  i="$1"
  ssh_n "$i" "sudo bash -c 'mkdir -p /opt/tarit/sockets
    TARIT_API_KEY=$API_KEY TARIT_PEER_SECRET=$PEER_SECRET TARIT_DATABASE_URL=\"$DBURL\" \
    TARIT_LISTEN=0.0.0.0:8080 TARIT_RPC_ADDR=http://${PRIV[$i]}:8080 TARIT_HOST_ID=node$i \
    TARIT_SOCKET_DIR=/opt/tarit/sockets TARIT_DB=/opt/tarit/local.sqlite TARIT_CONFIG=/opt/tarit/none.toml \
    TARIT_VMM_BIN=/opt/tarit/vmm TARIT_KERNEL=/opt/tarit/vmlinux TARIT_ROOTFS=/opt/tarit/rootfs.ext4 \
    TARIT_ROOTFS_READONLY=1 TARIT_WARM_POOL=0 TARIT_MAX_VMS=4 RUST_LOG=taritd=info \
    nohup /opt/tarit/taritd serve >/opt/tarit/taritd.log 2>&1 &'"
}
info "starting taritd on all nodes"
i=0; while [ "$i" -lt "$NODES" ]; do start_taritd "$i"; i=$((i+1)); done

api(){ curl -sS --max-time 20 -H "X-API-Key: $API_KEY" "$@"; }
jget(){ python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get(sys.argv[1],""))' "$1" 2>/dev/null; }
N0="http://${PUB[0]}:8080"
info "waiting for cluster to reach $NODES healthy nodes"
ok=0; body=""
for _ in $(seq 1 40); do
  body=$(api "$N0/v1/cluster" 2>/dev/null || true)
  h=$(printf '%s' "$body" | jget healthy_nodes); h="${h:-0}"
  info "  healthy_nodes=$h"
  [ "$h" -ge "$NODES" ] 2>/dev/null && { ok=1; break; }
  sleep 5
done
[ "$ok" = 1 ] && info "cluster healthy: $NODES nodes" || fail "cluster never reached $NODES healthy nodes (last=$body)"

# ------------------------------------------------------------------ exercise + failover
info "create + exec a VM through the cluster API"
CRESP=$(api -H 'content-type: application/json' -d '{"vcpus":1,"memory_mib":256}' "$N0/v1/vms" 2>/dev/null)
VM=$(printf '%s' "$CRESP" | jget id)
info "  create response: $CRESP"
info "  vm=$VM"
if [ -z "$VM" ]; then
  fail "VM create returned no id"
  info "  node0 taritd.log tail:"; ssh_n 0 'tail -20 /opt/tarit/taritd.log' 2>/dev/null
else
  sleep 12
  OUT=$(api -H 'content-type: application/json' -d "{\"vm_id\":\"$VM\",\"command\":\"echo cluster-exec-ok\",\"timeout_ms\":8000}" "$N0/v1/execute" 2>/dev/null)
  printf '%s' "$OUT" | grep -q cluster-exec-ok && info "  exec ok on cluster" || fail "cluster exec failed: $OUT"
fi

info "leader failover: stop taritd on node0, expect >= $((NODES-1)) healthy"
ssh_n 0 'sudo bash -c "for p in \$(pgrep -f /opt/tarit/taritd); do kill \$p; done"'
sleep 15
NL="http://${PUB[1]}:8080"; h=0
for _ in $(seq 1 24); do
  h=$(api "$NL/v1/cluster" 2>/dev/null | jget healthy_nodes); h="${h:-0}"
  info "  post-failover healthy_nodes=$h"
  [ "$h" -ge "$((NODES-1))" ] 2>/dev/null && break
  sleep 5
done
[ "$h" -ge "$((NODES-1))" ] 2>/dev/null && info "failover ok: cluster still serving on $((NODES-1)) nodes" || fail "cluster did not recover after node0 down (h=$h)"

info "rejoin: restart taritd on node0, expect $NODES healthy again"
start_taritd 0; h=0
for _ in $(seq 1 24); do
  h=$(api "$NL/v1/cluster" 2>/dev/null | jget healthy_nodes); h="${h:-0}"
  [ "$h" -ge "$NODES" ] 2>/dev/null && break; sleep 5
done
[ "$h" -ge "$NODES" ] 2>/dev/null && info "rejoin ok: $NODES healthy" || fail "node0 did not rejoin (h=$h)"

echo
echo "============= test/cluster summary ============="
echo "  nodes: $NODES ($TYPE)   run: $RUN_ID"
[ "$PASS" = 1 ] && { echo "  RESULT: CLUSTER_E2E_PASS"; exit 0; } || { echo "  RESULT: CLUSTER_E2E_FAIL"; exit 1; }
