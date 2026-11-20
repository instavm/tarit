"""Keep the public router's literal paths and the published OpenAPI paths in sync."""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


class OpenApiContractTests(unittest.TestCase):
    def test_public_router_paths_match_openapi(self):
        api = (ROOT / "orch/crates/taritd/src/api.rs").read_text()
        router = api.split("\npub fn router(", 1)[1].split("\nasync fn not_found(", 1)[0]
        # Ignore the separate internal and share gateway routers. Require any new
        # declaration to be a literal path so it cannot silently skip this check.
        server_paths = re.findall(r'\.route\(\s*"([^"]+)"', router)
        self.assertEqual(len(server_paths), len(re.findall(r'\.route\s*\(', router)))
        self.assertIn("/health", server_paths)
        spec = (ROOT / "orch/openapi.yaml").read_text().split("\npaths:\n", 1)[1]
        documented_paths = re.findall(r"^  (/[^:\n]+):$", spec, re.MULTILINE)
        self.assertSetEqual(set(server_paths), set(documented_paths))
