"""Publish the VMM executable and dep-info selected by Cargo for make install."""

import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def build(features, cargo):
    requested = sorted(set(features.replace(",", " ").split()))
    variant = hashlib.sha256(",".join(requested).encode()).hexdigest()[:16]
    # Keep Cargo's mutable release binary isolated from builds with different
    # features, even if a developer runs ordinary cargo build concurrently.
    root = Path(os.environ.get("CARGO_TARGET_DIR") or os.environ.get("CARGO_BUILD_TARGET_DIR") or "vmm/target")
    target = root / "make-vmm" / variant
    command = [*cargo, "build", "--release", "--manifest-path", "vmm/Cargo.toml",
               "-p", "vmm", "--bin", "vmm", "--target-dir", str(target),
               "--message-format=json-render-diagnostics"]
    if requested:
        command.extend(["--features", ",".join(requested)])
    result = subprocess.run(command, stdout=subprocess.PIPE, check=True)
    artifact = None
    for line in result.stdout.splitlines():
        record = json.loads(line)
        if (record["reason"] == "compiler-artifact" and record["target"]["name"] == "vmm"
                and "bin" in record["target"]["kind"] and record.get("executable")):
            actual = set(record["features"]) - {"default"}
            if actual != set(requested) - {"default"}:
                raise SystemExit(f"Cargo built VMM features {sorted(actual)}, requested {requested}")
            artifact = Path(record["executable"])
    if artifact is None:
        raise SystemExit("Cargo did not report a VMM executable")
    release = Path("vmm/target/release")
    release.mkdir(parents=True, exist_ok=True)
    # Publish a coherent snapshot. A failed publish invalidates the old record.
    with Path("Makefile").open("rb") as lock, tempfile.TemporaryDirectory(dir=release) as tmp:
        fcntl.flock(lock, fcntl.LOCK_EX)
        staged = Path(tmp) / "vmm"
        shutil.copy2(artifact, staged)
        shutil.copy2(artifact.with_suffix(".d"), staged.with_suffix(".d"))
        subprocess.run(["./scripts/vmm-build-features.sh", "record", staged, features], check=True)
        (release / "vmm.features").unlink(missing_ok=True)
        for name in ("vmm.d", "vmm", "vmm.features"):
            os.replace(staged.parent / name, release / name)


if __name__ == "__main__":
    build(sys.argv[1], sys.argv[2:])
