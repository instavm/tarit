#!/usr/bin/env bash
# Open TCP 8080 on the c8i test host security group for public API + OpenAPI docs.
set -euo pipefail

SG_ID="${C8I_SG:?set C8I_SG to the security group id of the API host}"
PORT="${TARIT_PORT:-8080}"
CIDR="${TARIT_PUBLIC_CIDR:-0.0.0.0/0}"
REGION="${AWS_REGION:-us-east-1}"

if aws ec2 describe-security-groups --group-ids "$SG_ID" --region "$REGION" \
  --query "SecurityGroups[0].IpPermissions[?FromPort==\`$PORT\` && ToPort==\`$PORT\`]" \
  --output text | grep -q "$CIDR"; then
  echo "port $PORT already open to $CIDR on $SG_ID"
  exit 0
fi

aws ec2 authorize-security-group-ingress \
  --group-id "$SG_ID" \
  --protocol tcp \
  --port "$PORT" \
  --cidr "$CIDR" \
  --region "$REGION" \
  --tag-specifications "ResourceType=security-group-rule,Tags=[{Key=Project,Value=taritd-cp},{Key=Purpose,Value=openapi-public}]" \
  2>/dev/null || aws ec2 authorize-security-group-ingress \
  --group-id "$SG_ID" \
  --ip-permissions "IpProtocol=tcp,FromPort=$PORT,ToPort=$PORT,IpRanges=[{CidrIp=$CIDR,Description=taritd API and OpenAPI docs}]" \
  --region "$REGION"

echo "opened TCP $PORT on $SG_ID for $CIDR"
