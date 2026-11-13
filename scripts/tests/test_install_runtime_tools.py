"""Run vmm/guest/install-runtime-tools.sh against stub system commands.

The script needs root, a loop device and a network mirror. These tests replace
those commands with recorders so the parts that broke on real hosts (issue
#172) are checked without them: the chroot gets a private /dev with usable
character devices, and chrooted commands do not inherit the caller's
environment.
"""

import os
import stat
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "vmm/guest/install-runtime-tools.sh"

# Each stub appends one line per call to $STUB_LOG. `mount` of the image
# populates the mount point with a minimal rootfs whose /dev/null is a
# regular file, as a docker-exported or OCI-unpacked image can have.
STUBS = {
    "id": 'echo 0\n',
    "losetup": textwrap.dedent(
        """\
        echo "losetup $*" >> "$STUB_LOG"
        [ "$1" = -d ] || echo /dev/loop-stub
        """
    ),
    "mount": textwrap.dedent(
        """\
        echo "mount $*" >> "$STUB_LOG"
        target="${!#}"
        case "$*" in
          *"-t ext4"*)
            mkdir -p "$target/etc" "$target/dev" "$target/var/lib/apt/lists"
            echo 'nameserver 192.0.2.1' > "$target/etc/resolv.conf"
            : > "$target/dev/null"
            chmod 0644 "$target/dev/null"
            ;;
          *"-t tmpfs"*)
            mv "$target" "$target.image"
            mkdir "$target"
            ;;
        esac
        echo "$target" >> "$STUB_MOUNTS"
        """
    ),
    "umount": textwrap.dedent(
        """\
        echo "umount $*" >> "$STUB_LOG"
        target="$1"
        if [ -d "$target.image" ]; then
          rm -f -- "$target"/*
          rmdir -- "$target"
          mv "$target.image" "$target"
        elif [ "$(basename "$target")" != dev ]; then
          rm -rf -- "$target/etc" "$target/dev" "$target/var"
        fi
        grep -vxF -- "$target" "$STUB_MOUNTS" > "$STUB_MOUNTS.new" || true
        mv "$STUB_MOUNTS.new" "$STUB_MOUNTS"
        """
    ),
    "mountpoint": textwrap.dedent(
        """\
        [ "$1" = -q ] && shift
        grep -qxF -- "$1" "$STUB_MOUNTS"
        """
    ),
    "mknod": textwrap.dedent(
        """\
        echo "mknod $*" >> "$STUB_LOG"
        # mknod -m MODE PATH c MAJOR MINOR
        printf 'char %s %s %s\\n' "$2" "$5" "$6" > "$3"
        """
    ),
    "chroot": textwrap.dedent(
        """\
        root="$1"; shift
        {
          printf 'chroot'
          printf ' %q' "$@"
          printf '\\n'
          printf 'dev-null %s\\n' "$(cat "$root/dev/null" 2>/dev/null || echo missing)"
        } >> "$STUB_LOG"
        """
    ),
    "sync": "exit 0\n",
}


class InstallRuntimeToolsTests(unittest.TestCase):
    def run_script(self, extra_env=None):
        with tempfile.TemporaryDirectory() as work:
            work = Path(work)
            bin_dir = work / "bin"
            bin_dir.mkdir()
            for name, body in STUBS.items():
                path = bin_dir / name
                path.write_text("#!/usr/bin/env bash\nset -eu\n" + body)
                path.chmod(path.stat().st_mode | stat.S_IXUSR)
            image = work / "rootfs.ext4"
            image.write_bytes(b"")
            log = work / "calls.log"
            mounts = work / "mounts"
            log.write_text("")
            mounts.write_text("")
            env = {
                "PATH": f"{bin_dir}:{os.environ['PATH']}",
                "STUB_LOG": str(log),
                "STUB_MOUNTS": str(mounts),
                "TMPDIR": str(work),
            }
            env.update(extra_env or {})
            self.caller_tmpdir = env["TMPDIR"]
            result = subprocess.run(
                ["bash", str(SCRIPT), str(image)],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            return result, log.read_text().splitlines(), mounts.read_text().split()

    def test_chroot_gets_private_dev_with_character_devices(self):
        result, calls, mounts = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)

        dev_mounts = [c for c in calls if c.startswith("mount -t tmpfs")]
        self.assertEqual(len(dev_mounts), 1, calls)
        self.assertTrue(dev_mounts[0].endswith("/dev"), dev_mounts)
        self.assertIn("mknod -m 0666", "\n".join(calls))
        null = [c for c in calls if c.startswith("mknod") and c.split()[3].endswith("/dev/null")]
        self.assertEqual(len(null), 1, calls)
        self.assertEqual(null[0].split()[-3:], ["c", "1", "3"])

        # Every chrooted command saw the character device, not the image's file.
        chroot_calls = [i for i, c in enumerate(calls) if c.startswith("chroot")]
        self.assertGreaterEqual(len(chroot_calls), 3, calls)
        for index in chroot_calls:
            self.assertEqual(calls[index + 1], "dev-null char 0666 1 3", calls)

        # /dev is unmounted before the image, and nothing is left mounted.
        umounts = [c for c in calls if c.startswith("umount")]
        self.assertTrue(umounts[0].endswith("/dev"), umounts)
        self.assertEqual(mounts, [])

    def test_chroot_does_not_inherit_the_callers_environment(self):
        # The caller's TMPDIR is a real host directory (the script's own
        # mktemp uses it); it just does not exist inside the image.
        result, calls, _ = self.run_script(
            {
                "HOME": "/caller/home",
                "https_proxy": "http://proxy.example:3128",
            }
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        chroot_calls = [c for c in calls if c.startswith("chroot")]
        self.assertTrue(chroot_calls, calls)
        for call in chroot_calls:
            self.assertIn("/usr/bin/env -i ", call)
            self.assertNotIn("TMPDIR", call)
            self.assertNotIn(self.caller_tmpdir, call)
            self.assertNotIn("/caller/", call)
            self.assertIn("HOME=/root", call)
            self.assertIn("https_proxy=http://proxy.example:3128", call)

    def test_dev_is_unmounted_when_a_chrooted_step_fails(self):
        failing_chroot = STUBS["chroot"] + "exit 1\n"
        original = STUBS["chroot"]
        STUBS["chroot"] = failing_chroot
        try:
            result, calls, mounts = self.run_script()
        finally:
            STUBS["chroot"] = original
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("umount", " ".join(c.split()[0] for c in calls))
        self.assertEqual(mounts, [])


if __name__ == "__main__":
    unittest.main()
