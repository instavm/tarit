#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixture_dir="$repo_root/orch/tests/cloud-object-emulators"
cache_dir="${TARIT_E2E_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/tarit-e2e-cloud-object}"
runtime_dir="$(mktemp -d "${TMPDIR:-/tmp}/tarit-cloud-object.XXXXXX")"
expected_source="${TARIT_EXPECT_SOURCE:-}"

minio_version="RELEASE.2025-07-23T15-54-02Z"
minio_sha256="eef6581f6509f43ece007a6f2eb4c5e3ce41498c8956e919a7ac7b4b170fa431"
mc_version="RELEASE.2025-07-21T05-28-08Z"
mc_sha256="ea4a453be116071ab1ccbd24eb8755bf0579649f41a7b94ab9e68571bb9f4a1e"

s3_port="${TARIT_TEST_S3_PORT:-19000}"
azure_port="${TARIT_TEST_AZURE_PORT:-19002}"
s3_endpoint="http://127.0.0.1:$s3_port"
azure_endpoint="http://127.0.0.1:$azure_port/devstoreaccount1"
s3_bucket="tarit-e2e-$(openssl rand -hex 8)"
azure_container="tarit-e2e-$(openssl rand -hex 8)"
s3_access_key="tarit$(openssl rand -hex 8)"
s3_secret_key="$(openssl rand -hex 24)"
azure_account="devstoreaccount1"
azure_key="Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="

minio_pid=""
azurite_pid=""

cleanup() {
  local pid
  for pid in "$azurite_pid" "$minio_pid"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf -- "$runtime_dir"
}
trap cleanup EXIT INT TERM

for command in cargo curl npm node openssl python3 sha256sum; do
  command -v "$command" >/dev/null || {
    echo "required command is unavailable: $command" >&2
    exit 1
  }
done

if [[ -n "$expected_source" ]]; then
  [[ -f "$repo_root/SOURCE_COMMIT" ]] || {
    echo "SOURCE_COMMIT is required when TARIT_EXPECT_SOURCE is set" >&2
    exit 1
  }
  [[ "$(<"$repo_root/SOURCE_COMMIT")" == "$expected_source" ]] || {
    echo "staged source identity does not match TARIT_EXPECT_SOURCE" >&2
    exit 1
  }
fi

(
  cd "$repo_root/orch"
  cargo check -p taritd --all-targets --no-default-features
  cargo clippy -p tarit-volume -p taritd --all-targets \
    --features taritd/cloud-object-store-aws,taritd/cloud-object-store-azure -- -D warnings
  cargo test -p taritd -- --test-threads=1
  cargo test -p tarit-volume \
    --features cloud-object-store-aws,cloud-object-store-azure
)

install -d -m 0700 "$cache_dir/bin" "$cache_dir/node"

download_checked() {
  local url="$1"
  local expected="$2"
  local destination="$3"
  local partial="$destination.partial.$$"
  if [[ -f "$destination" ]] && printf '%s  %s\n' "$expected" "$destination" | sha256sum -c - >/dev/null 2>&1; then
    return
  fi
  rm -f -- "$partial"
  curl --fail --location --silent --show-error "$url" --output "$partial"
  printf '%s  %s\n' "$expected" "$partial" | sha256sum -c - >/dev/null
  chmod 0755 "$partial"
  mv -f -- "$partial" "$destination"
}

download_checked \
  "https://dl.min.io/server/minio/release/linux-amd64/archive/minio.$minio_version" \
  "$minio_sha256" \
  "$cache_dir/bin/minio-$minio_version"
download_checked \
  "https://dl.min.io/client/mc/release/linux-amd64/archive/mc.$mc_version" \
  "$mc_sha256" \
  "$cache_dir/bin/mc-$mc_version"

node_cache="$cache_dir/node/azurite-3.35.0"
manifest_sha="$(sha256sum "$fixture_dir/package.json" "$fixture_dir/package-lock.json")"
if [[ ! -x "$node_cache/node_modules/.bin/azurite-blob" ]] ||
  [[ ! -f "$node_cache/manifest.sha256" ]] ||
  ! cmp -s <(printf '%s\n' "$manifest_sha") "$node_cache/manifest.sha256"; then
  rm -rf -- "$node_cache"
  install -d -m 0700 "$node_cache"
  cp "$fixture_dir/package.json" "$fixture_dir/package-lock.json" "$node_cache/"
  npm ci --prefix "$node_cache" --ignore-scripts --no-audit --no-fund
  printf '%s\n' "$manifest_sha" >"$node_cache/manifest.sha256"
fi

MINIO_ROOT_USER="$s3_access_key" \
MINIO_ROOT_PASSWORD="$s3_secret_key" \
  "$cache_dir/bin/minio-$minio_version" server \
    --address "127.0.0.1:$s3_port" \
    --console-address "127.0.0.1:0" \
    "$runtime_dir/minio" >"$runtime_dir/minio.log" 2>&1 &
minio_pid=$!

"$node_cache/node_modules/.bin/azurite-blob" \
  --silent \
  --skipApiVersionCheck \
  --location "$runtime_dir/azurite" \
  --blobHost 127.0.0.1 \
  --blobPort "$azure_port" >"$runtime_dir/azurite.log" 2>&1 &
azurite_pid=$!

wait_for_endpoint() {
  local url="$1"
  local pid="$2"
  local name="$3"
  for _ in $(seq 1 100); do
    kill -0 "$pid" 2>/dev/null || {
      echo "$name exited before becoming ready" >&2
      sed -n '1,160p' "$runtime_dir/$name.log" >&2 || true
      exit 1
    }
    if curl --silent --output /dev/null --max-time 1 "$url"; then
      return
    fi
    sleep 0.1
  done
  echo "$name did not become ready" >&2
  exit 1
}

wait_for_endpoint "$s3_endpoint/minio/health/ready" "$minio_pid" minio
wait_for_endpoint "$azure_endpoint" "$azurite_pid" azurite

mc_bin="$cache_dir/bin/mc-$mc_version"
mc_config="$runtime_dir/mc"
"$mc_bin" --config-dir "$mc_config" alias set local "$s3_endpoint" "$s3_access_key" "$s3_secret_key" >/dev/null
"$mc_bin" --config-dir "$mc_config" mb "local/$s3_bucket" >/dev/null

AZURE_ACCOUNT="$azure_account" \
AZURE_KEY="$azure_key" \
AZURE_ENDPOINT="$azure_endpoint" \
AZURE_CONTAINER="$azure_container" \
python3 - <<'PY'
import base64
import datetime
import hashlib
import hmac
import os
import urllib.error
import urllib.request

account = os.environ["AZURE_ACCOUNT"]
container = os.environ["AZURE_CONTAINER"]
endpoint = os.environ["AZURE_ENDPOINT"]
date = datetime.datetime.now(datetime.timezone.utc).strftime("%a, %d %b %Y %H:%M:%S GMT")
version = "2023-11-03"
canonical_headers = f"x-ms-date:{date}\nx-ms-version:{version}\n"
canonical_resource = f"/{account}/{container}\nrestype:container"
string_to_sign = "PUT\n\n\n\n\n\n\n\n\n\n\n\n" + canonical_headers + canonical_resource
signature = base64.b64encode(
    hmac.new(
        base64.b64decode(os.environ["AZURE_KEY"]),
        string_to_sign.encode(),
        hashlib.sha256,
    ).digest()
).decode()
request = urllib.request.Request(
    f"{endpoint}/{container}?restype=container",
    method="PUT",
    headers={
        "Authorization": f"SharedKey {account}:{signature}",
        "Content-Length": "0",
        "x-ms-date": date,
        "x-ms-version": version,
    },
)
try:
    with urllib.request.urlopen(request, timeout=10) as response:
        if response.status != 201:
            raise RuntimeError(f"unexpected container create status: {response.status}")
except urllib.error.HTTPError as error:
    raise RuntimeError(f"container creation failed with status {error.code}") from error
PY

(
  cd "$repo_root/orch"
  AWS_ACCESS_KEY_ID="$s3_access_key" \
  AWS_SECRET_ACCESS_KEY="$s3_secret_key" \
  AWS_DEFAULT_REGION="us-east-1" \
  AWS_ENDPOINT_URL_S3="$s3_endpoint" \
  AWS_VIRTUAL_HOSTED_STYLE_REQUEST="false" \
  AWS_ALLOW_HTTP="true" \
  TARIT_TEST_OBJECT_ALLOW_HTTP="1" \
  TARIT_TEST_S3_BUCKET="$s3_bucket" \
    cargo test -p tarit-volume --features cloud-object-store-aws \
      cloud_object::tests::s3_transport_round_trip -- --ignored --exact --nocapture

  AZURE_STORAGE_ACCOUNT_NAME="$azure_account" \
  AZURE_STORAGE_ACCOUNT_KEY="$azure_key" \
  AZURE_STORAGE_ENDPOINT="$azure_endpoint" \
  AZURE_ALLOW_HTTP="true" \
  TARIT_TEST_OBJECT_ALLOW_HTTP="1" \
  TARIT_TEST_AZURE_CONTAINER="$azure_container" \
    cargo test -p tarit-volume --features cloud-object-store-azure \
      cloud_object::tests::azure_transport_round_trip -- --ignored --exact --nocapture
)

if [[ -n "$("$mc_bin" --config-dir "$mc_config" find "local/$s3_bucket" --name '*.blob')" ]]; then
  echo "S3 transport test leaked immutable objects" >&2
  exit 1
fi

if [[ "${TARIT_TEST_KVM_ARTIFACTS:-0}" == 1 ]]; then
  [[ "$(uname -s)" == Linux ]] || {
    echo "KVM artifact qualification requires Linux" >&2
    exit 1
  }
  [[ "$(id -u)" == 0 ]] || {
    echo "KVM artifact qualification must run as root" >&2
    exit 1
  }
  command -v make >/dev/null || {
    echo "required command is unavailable: make" >&2
    exit 1
  }
  make -C "$repo_root" build

  AWS_ACCESS_KEY_ID="$s3_access_key" \
  AWS_SECRET_ACCESS_KEY="$s3_secret_key" \
  AWS_DEFAULT_REGION=us-east-1 \
  AWS_ENDPOINT_URL_S3="$s3_endpoint" \
  AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false \
  AWS_ALLOW_HTTP=true \
  TARIT_OBJECT_STORE_PROVIDER=aws_s3 \
  TARIT_OBJECT_STORE_BUCKET="$s3_bucket" \
  TARIT_OBJECT_STORE_PREFIX="runtime-s3-$(openssl rand -hex 8)" \
  TARIT_OBJECT_STORE_MAX_BYTES=4294967296 \
  TARIT_OBJECT_STORE_ALLOW_HTTP=true \
  TARIT_TEST_OBJECT_FALLBACK=1 \
  TARIT_SOURCE_REVISION="${expected_source:-local}" \
    "$repo_root/orch/tests/e2e_peer_artifact_replication.sh"

  AZURE_STORAGE_ACCOUNT_NAME="$azure_account" \
  AZURE_STORAGE_ACCOUNT_KEY="$azure_key" \
  AZURE_STORAGE_ENDPOINT="$azure_endpoint" \
  AZURE_ALLOW_HTTP=true \
  TARIT_OBJECT_STORE_PROVIDER=azure_blob \
  TARIT_OBJECT_STORE_CONTAINER="$azure_container" \
  TARIT_OBJECT_STORE_PREFIX="runtime-azure-$(openssl rand -hex 8)" \
  TARIT_OBJECT_STORE_MAX_BYTES=4294967296 \
  TARIT_OBJECT_STORE_ALLOW_HTTP=true \
  TARIT_TEST_OBJECT_FALLBACK=1 \
  TARIT_SOURCE_REVISION="${expected_source:-local}" \
    "$repo_root/orch/tests/e2e_peer_artifact_replication.sh"
fi

echo "CLOUD_OBJECT_STORE_E2E_PASS providers=s3,azure conditional_writers=16 cleanup=verified kvm_artifacts=${TARIT_TEST_KVM_ARTIFACTS:-0} source=${expected_source:-local}"
