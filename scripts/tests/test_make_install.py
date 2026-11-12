"""`make install` copies prebuilt artifacts and refuses missing or stale ones.

The install targets run under sudo, so they must never compile. These tests run the
real Makefile against a fake checkout: fake cargo dep-info, fake binaries, and a
CARGO/CC that record any call, then check what gets installed.
"""

import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OLD = 1_700_000_000
BUILT = OLD + 100
NEW = OLD + 200


@unittest.skipUnless(shutil.which("make"), "make is required")
class MakeInstallTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.repo = (self.tmp / "checkout").resolve()
        (self.repo / "scripts").mkdir(parents=True)
        shutil.copy(ROOT / "Makefile", self.repo / "Makefile")
        shutil.copy(ROOT / "scripts/check-install-artifact.sh", self.repo / "scripts")
        agent = self.repo / "vmm/guest/agent"
        agent.mkdir(parents=True)
        shutil.copy(ROOT / "vmm/guest/agent/Makefile", agent / "Makefile")
        self.registry = self.tmp / "cargo-home/registry/src/dep-1.0.0/src/lib.rs"
        self.sources = {
            "vmm/target/release/vmm": [
                "vmm/src/main.rs",
                "vmm/crates/vmm-core/src/with space.rs",
                "proto/src/lib.rs",
            ],
            "orch/target/release/taritd": ["orch/crates/taritd/src/main.rs", "proto/src/lib.rs"],
        }
        files = [path for paths in self.sources.values() for path in paths]
        files += ["vmm/Cargo.lock", "orch/Cargo.lock", "vmm/guest/agent/vmm-agent.c"]
        for path in files:
            self.write(path, "source", OLD)
        self.registry.parent.mkdir(parents=True)
        self.registry.write_text("dependency")
        os.utime(self.registry, (OLD, OLD))
        for artifact, sources in self.sources.items():
            self.write_artifact(artifact, sources)
        self.write("vmm/guest/agent/vmm-agent", "agent", BUILT, executable=True)
        self.calls = self.tmp / "build-calls"
        self.recorder = self.tmp / "record-build"
        self.recorder.write_text(f'#!/bin/sh\necho "$0 $*" >> "{self.calls}"\nexit 1\n')
        self.recorder.chmod(0o755)

    def write(self, path, text, mtime, executable=False):
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)
        if executable:
            file.chmod(file.stat().st_mode | stat.S_IXUSR)
        os.utime(file, (mtime, mtime))

    def write_artifact(self, artifact, sources):
        self.write(artifact, "binary", BUILT, executable=True)
        paths = [str(self.repo / source) for source in sources] + [str(self.registry)]
        escaped = " ".join(path.replace(" ", "\\ ") for path in paths)
        self.write(f"{artifact}.d", f"{self.repo / artifact}: {escaped}\n", BUILT)

    def touch(self, path, mtime=NEW):
        os.utime(self.repo / path, (mtime, mtime))

    def make(self, target="install"):
        dest = self.tmp / "dest"
        result = subprocess.run(
            ["make", "-s", target, f"DESTDIR={dest}", "PREFIX=/usr", f"CARGO={self.recorder}", f"CC={self.recorder}"],
            cwd=self.repo,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertFalse(self.calls.exists(), "install must never run cargo or the C compiler")
        return result, dest

    def assert_refused(self, result, dest, *fragments):
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("make build", result.stderr)
        for fragment in fragments:
            self.assertIn(fragment, result.stderr)
        self.assertFalse((dest / "usr/bin/vmm").exists(), "nothing may be installed after a refusal")

    def test_installs_current_artifacts_without_building(self):
        result, dest = self.make()
        self.assertEqual(result.returncode, 0, result.stderr)
        for path in ("usr/bin/vmm", "usr/bin/taritd", "usr/libexec/tarit/vmm-agent"):
            self.assertTrue((dest / path).is_file(), path)

    def test_refuses_missing_binary(self):
        (self.repo / "orch/target/release/taritd").unlink()
        self.assert_refused(*self.make(), "missing orch/target/release/taritd")

    def test_refuses_binary_older_than_a_changed_source(self):
        self.touch("proto/src/lib.rs")
        self.assert_refused(*self.make(), "vmm/target/release/vmm is older than", "proto/src/lib.rs")

    def test_refuses_binary_older_than_a_source_path_with_a_space(self):
        self.touch("vmm/crates/vmm-core/src/with space.rs")
        self.assert_refused(*self.make(), "with space.rs")

    def test_refuses_binary_older_than_its_lockfile(self):
        self.touch("orch/Cargo.lock")
        self.assert_refused(*self.make(), "taritd is older than", "orch/Cargo.lock")

    def test_refuses_binary_built_from_a_deleted_source(self):
        (self.repo / "orch/crates/taritd/src/main.rs").unlink()
        self.assert_refused(*self.make(), "no longer exists")

    def test_refuses_binary_without_dep_info(self):
        (self.repo / "vmm/target/release/vmm.d").unlink()
        self.assert_refused(*self.make(), "dep-info")

    def test_refuses_binary_built_from_another_checkout(self):
        artifact = self.repo / "vmm/target/release/vmm"
        (self.repo / "vmm/target/release/vmm.d").write_text(f"{artifact}: /elsewhere/vmm/src/main.rs\n")
        os.utime(self.repo / "vmm/target/release/vmm.d", (BUILT, BUILT))
        self.assert_refused(*self.make(), "not built from this checkout")

    def test_refuses_stale_guest_agent(self):
        self.touch("vmm/guest/agent/vmm-agent.c")
        self.assert_refused(*self.make(), "vmm-agent is missing or older than its source")

    def test_ignores_dependency_sources_outside_the_checkout(self):
        os.utime(self.registry, (NEW, NEW))
        result, _ = self.make()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_install_vmm_needs_only_a_current_vmm(self):
        (self.repo / "orch/target/release/taritd").unlink()
        result, dest = self.make("install-vmm")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((dest / "usr/bin/vmm").is_file())

    def test_install_vmm_refuses_a_stale_vmm(self):
        self.touch("vmm/src/main.rs")
        result, dest = self.make("install-vmm")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("make vmm", result.stderr)
        self.assertFalse((dest / "usr/bin/vmm").exists())


if __name__ == "__main__":
    unittest.main()
