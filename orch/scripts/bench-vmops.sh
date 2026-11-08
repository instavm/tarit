#!/usr/bin/env bash
# Keep the historical entry point on the same API contract and cleanup path.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
exec "$ROOT/scripts/bench-vmops.sh" "$@"
