from __future__ import annotations

import unittest
from pathlib import Path

from sdk.verify_release import collect_versions, validate_versions

ROOT = Path(__file__).resolve().parents[3]


class ReleaseMetadataTests(unittest.TestCase):
    def test_repository_versions_match(self) -> None:
        versions = collect_versions(ROOT)
        self.assertEqual(validate_versions(versions, "v0.1.2"), "0.1.2")

    def test_mismatched_component_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "versions disagree"):
            validate_versions({"server": "0.1.2", "SDK": "0.1.1"})

    def test_wrong_or_unstable_tag_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "does not match"):
            validate_versions({"server": "0.1.2"}, "v0.1.1")
        with self.assertRaisesRegex(ValueError, "not stable SemVer"):
            validate_versions({"server": "0.2.0-rc.1"}, "v0.2.0-rc.1")


if __name__ == "__main__":
    unittest.main()
