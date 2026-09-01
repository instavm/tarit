from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "check_oss_boundary.py"
SPEC = importlib.util.spec_from_file_location("check_oss_boundary", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class OssBoundaryTest(unittest.TestCase):
    def check(self, files: dict[str, str]) -> list[str]:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, contents in files.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(contents, encoding="utf-8")
            return MODULE.check_repository(root)

    def test_allows_internal_paths_and_generic_sandbox_language(self) -> None:
        errors = self.check(
            {
                "Cargo.toml": '[dependencies]\nworker = { path = "crates/worker" }\n',
                "crates/worker/Cargo.toml": '[package]\nname = "worker"\nversion = "0.1.0"\n',
                "src/lib.rs": "// A generic sandbox runtime.\n",
                "package.json": '{"repository":"https://github.com/instavm/tarit"}',
            }
        )
        self.assertEqual(errors, [])

    def test_rejects_out_of_tree_path_dependency(self) -> None:
        errors = self.check(
            {
                "Cargo.toml": (
                    '[workspace]\nmembers = ["../member"]\n'
                    '[dependencies]\nlegacy = { path = "../legacy" }\n'
                ),
                "package.json": (
                    '{"workspaces":["../packages/*"],'
                    '"dependencies":{"legacy":"file:../legacy"}}'
                ),
                "pyproject.toml": (
                    '[project]\nname="x"\nversion="0.1"\n'
                    '[tool.uv.sources]\nlegacy={path="../python-legacy"}\n'
                ),
            }
        )
        self.assertEqual(sum("escapes repository" in error for error in errors), 5, errors)

    def test_rejects_product_dependencies_and_imports(self) -> None:
        errors = self.check(
            {
                "Cargo.toml": '[dependencies]\ninstavm-sandbox = "1"\n',
                "pyproject.toml": '[project]\nname="x"\nversion="0.1"\ndependencies=["instavm-client"]\n',
                "package.json": '{"dependencies":{"@instavm/runtime":"1.0.0"}}',
                "src/consumer.py": "from instavm_sdk import Client\n",
                "src/consumer.rs": "use instavm::Client;\n",
                "src/legacy.py": "from app.domain.firecracker import MicroVM\n",
                "nested/package.json": (
                    '{"dependencies":{"legacy":'
                    '"git+https://github.com/BandarLabs/sandbox.git"}}'
                ),
            }
        )
        self.assertEqual(len(errors), 7, errors)


if __name__ == "__main__":
    unittest.main()
