#!/usr/bin/env bash
# Rebuild the checked-in low-level clients from the public OpenAPI contract.
set -Eeuo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PYTHON_CLIENT_VERSION=0.29.1
RUFF_VERSION=0.16.5
TYPESCRIPT_CLIENT_VERSION=7.10.1

command -v uvx >/dev/null || { echo "missing uvx" >&2; exit 1; }
command -v npx >/dev/null || { echo "missing npx" >&2; exit 1; }
export RUFF_NO_CACHE=true

find "$ROOT/sdk/python" -mindepth 1 -delete 2>/dev/null || true
uvx --from "openapi-python-client==$PYTHON_CLIENT_VERSION" openapi-python-client generate \
  --path "$ROOT/orch/openapi.yaml" \
  --config "$ROOT/sdk/python-generator.yaml" \
  --custom-template-path "$ROOT/sdk/python-templates" \
  --output-path "$ROOT/sdk/python" \
  --meta uv \
  --overwrite \
  --fail-on-warning
install -m 0644 "$ROOT/sdk/python-high-level/high_level.py" \
  "$ROOT/sdk/python/tarit_sdk/high_level.py"
uvx --from "ruff==$RUFF_VERSION" ruff check --fix-only "$ROOT/sdk/python"
uvx --from "ruff==$RUFF_VERSION" ruff format "$ROOT/sdk/python"

find "$ROOT/sdk/typescript/src/generated" -mindepth 1 -delete 2>/dev/null || true
npx --yes \
  "openapi-typescript@$TYPESCRIPT_CLIENT_VERSION" \
  "$ROOT/orch/openapi.yaml" \
  --output "$ROOT/sdk/typescript/src/generated/schema.ts"
