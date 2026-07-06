#!/usr/bin/env bash
# Provision isolated RDS Postgres for taritd global fleet store (new resources only).
# Licenses: AWS managed service; client uses tokio-postgres (MIT/Apache-2.0).
set -euo pipefail

REGION="${AWS_REGION:-us-east-1}"
VPC_ID="${TARIT_CP_VPC_ID:?set TARIT_CP_VPC_ID to the VPC id to create the RDS instance in}"
C8I_SG="${TARIT_CP_C8I_SG:?set TARIT_CP_C8I_SG to the security group id of the orchestrator host (granted Postgres ingress)}"
DB_ID="taritd-cp-pg"
SG_NAME="taritd-cp-rds-sg"
SUBNET_GROUP="taritd-cp-db-subnet"
TAG="Project=taritd-cp"

echo "== checking existing RDS =="
if aws rds describe-db-instances --db-instance-identifier "$DB_ID" --region "$REGION" >/dev/null 2>&1; then
  ENDPOINT=$(aws rds describe-db-instances --db-instance-identifier "$DB_ID" --region "$REGION" \
    --query 'DBInstances[0].Endpoint.Address' --output text)
  echo "RDS already exists: $ENDPOINT"
  exit 0
fi

echo "== create security group (does not modify existing SGs) =="
RDS_SG=$(aws ec2 create-security-group \
  --group-name "$SG_NAME" \
  --description "Tarit orchestrator CP Postgres (taritd-cp)" \
  --vpc-id "$VPC_ID" \
  --region "$REGION" \
  --query GroupId --output text 2>/dev/null || \
  aws ec2 describe-security-groups --filters "Name=group-name,Values=$SG_NAME" "Name=vpc-id,Values=$VPC_ID" \
    --query 'SecurityGroups[0].GroupId' --output text)

aws ec2 create-tags --resources "$RDS_SG" --tags Key=Project,Value=taritd-cp --region "$REGION"

# Ingress on NEW sg only — allow c8i orchestrator SG to reach Postgres.
aws ec2 authorize-security-group-ingress \
  --group-id "$RDS_SG" \
  --protocol tcp \
  --port 5432 \
  --source-group "$C8I_SG" \
  --region "$REGION" 2>/dev/null || true

echo "== create DB subnet group =="
SUBNETS=$(aws ec2 describe-subnets --filters "Name=vpc-id,Values=$VPC_ID" \
  --query 'Subnets[0:2].SubnetId' --output text | tr '\t' ' ')
aws rds create-db-subnet-group \
  --db-subnet-group-name "$SUBNET_GROUP" \
  --db-subnet-group-description "taritd-cp isolated subnet group" \
  --subnet-ids $SUBNETS \
  --tags Key=Project,Value=taritd-cp \
  --region "$REGION" 2>/dev/null || true

DB_PASSWORD="${TARIT_CP_DB_PASSWORD:-$(openssl rand -base64 24 | tr -d '/+=' | head -c 24)}"

echo "== create RDS Postgres db.t4g.micro =="
aws rds create-db-instance \
  --db-instance-identifier "$DB_ID" \
  --db-instance-class db.t4g.micro \
  --engine postgres \
  --engine-version 16 \
  --master-username taritd \
  --master-user-password "$DB_PASSWORD" \
  --allocated-storage 20 \
  --storage-type gp3 \
  --db-name taritd \
  --db-subnet-group-name "$SUBNET_GROUP" \
  --vpc-security-group-ids "$RDS_SG" \
  --no-publicly-accessible \
  --backup-retention-period 1 \
  --tags Key=Project,Value=taritd-cp \
  --region "$REGION"

echo "== waiting for RDS available (may take several minutes) =="
aws rds wait db-instance-available --db-instance-identifier "$DB_ID" --region "$REGION"

ENDPOINT=$(aws rds describe-db-instances --db-instance-identifier "$DB_ID" --region "$REGION" \
  --query 'DBInstances[0].Endpoint.Address' --output text)

CREDS_FILE="${HOME}/.taritd/cp-rds.env"
RDS_CA="${HOME}/.taritd/rds-global-bundle.pem"
mkdir -p "$(dirname "$CREDS_FILE")"
curl -sf -o "$RDS_CA" https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem
cat > "$CREDS_FILE" <<EOF
# taritd-cp RDS (isolated resource group) — do not commit
export TARIT_DATABASE_URL=postgres://taritd:${DB_PASSWORD}@${ENDPOINT}:5432/taritd?sslmode=require
export TARIT_RDS_CA_FILE=${RDS_CA}
export TARIT_CP_RDS_ENDPOINT=${ENDPOINT}
export TARIT_CP_RDS_SG=${RDS_SG}
EOF
chmod 600 "$CREDS_FILE"

echo "RDS ready: $ENDPOINT"
echo "Credentials written to $CREDS_FILE"
echo "Use: source $CREDS_FILE"
