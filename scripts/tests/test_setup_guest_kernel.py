"""Exercise the quickstart kernel cache with two builds of one Linux version."""

import hashlib
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


class GuestKernelCacheTests(unittest.TestCase):
    def test_quickstart_rechecks_the_release_pin(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            guest = repo / "vmm/guest"
            (guest / "agent").mkdir(parents=True)
            (repo / "scripts").mkdir()
            script = repo / "scripts/setup-guest.sh"
            shutil.copy2(ROOT / "scripts/setup-guest.sh", script)
            fake_bin = repo / "bin"
            fake_bin.mkdir()
            apt = fake_bin / "apt-get"
            apt.write_text("#!/bin/sh\nexit 1\n")
            apt.chmod(0o700)
            agent = guest / "agent/vmm-agent"
            agent.touch(mode=0o700)
            (guest / "download-kernel.sh").write_text('printf new > "$OUT"\n')
            assets = repo / "assets"
            assets.mkdir()
            (assets / "rootfs.ext4").touch()
            (assets / "rootfs.tools.version").write_text("1\n")
            old = hashlib.sha256(b"old").hexdigest()
            new = hashlib.sha256(b"new").hexdigest()
            (guest / "kernel-version.env").write_text(
                f"KERNEL_VERSION=1.2.3\nKERNEL_ARTIFACT_SHA256={new}\n"
            )
            kernel = assets / "vmlinux"
            marker = assets / "kernel.version"

            def setup():
                return subprocess.run(
                    ["bash", str(script)], check=True, capture_output=True, text=True,
                    env={**os.environ, "OUT_DIR": str(assets), "VMM": "true", "KERNEL_DOWNLOAD": "1",
                         "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}"},
                ).stdout

            kernel.write_bytes(b"old")
            marker.write_text(f"1.2.3 {old}\n")
            setup()
            self.assertEqual(kernel.read_bytes(), b"new")
            self.assertEqual(marker.read_text().split(), ["1.2.3", new, new])

            # A valid two-field legacy marker and a local source build of the
            # current release can both be reused without reaching the downloader.
            (guest / "download-kernel.sh").write_text('echo "unexpected download" >&2; exit 1\n')
            for data, record in [(b"new", f"1.2.3 {new}"), (b"old", f"1.2.3 {old} {new}")]:
                with self.subTest(record=record):
                    kernel.write_bytes(data)
                    marker.write_text(record + "\n")
                    self.assertIn("already at", setup())
                    self.assertEqual(kernel.read_bytes(), data)
