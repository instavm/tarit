"""Fail closed when release versions do not describe one Tarit source release."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

import tomllib

SEMVER = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")


def _toml_version(path: Path, table: str) -> str:
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    value: object = data
    for part in table.split("."):
        if not isinstance(value, dict) or part not in value:
            raise ValueError(f"missing {table}.version source in {path}")
        value = value[part]
    if not isinstance(value, dict):
        raise TypeError(f"invalid version table in {path}")
    version = value.get("version")
    if not isinstance(version, str):
        raise TypeError(f"missing version in {path}")
    return version


def _generator_version(path: Path) -> str:
    match = re.search(r"^package_version_override:\s*([^\s#]+)\s*$", path.read_text(encoding="utf-8"), re.MULTILINE)
    if match is None:
        raise ValueError(f"missing package_version_override in {path}")
    return match.group(1)


def collect_versions(root: Path) -> dict[str, str]:
    package = json.loads((root / "sdk/typescript/package.json").read_text(encoding="utf-8"))
    package_lock = json.loads((root / "sdk/typescript/package-lock.json").read_text(encoding="utf-8"))
    lock_root = package_lock.get("packages", {}).get("", {})
    values: dict[str, object] = {
        "proto workspace": _toml_version(root / "proto/Cargo.toml", "package"),
        "VMM workspace": _toml_version(root / "vmm/Cargo.toml", "workspace.package"),
        "orchestrator workspace": _toml_version(root / "orch/Cargo.toml", "workspace.package"),
        "Python SDK": _toml_version(root / "sdk/python/pyproject.toml", "project"),
        "Python generator": _generator_version(root / "sdk/python-generator.yaml"),
        "TypeScript SDK": package.get("version"),
        "TypeScript lock": package_lock.get("version"),
        "TypeScript lock root": lock_root.get("version"),
    }
    invalid = [name for name, value in values.items() if not isinstance(value, str)]
    if invalid:
        raise ValueError(f"missing release version: {', '.join(invalid)}")
    return {name: value for name, value in values.items() if isinstance(value, str)}


def validate_versions(versions: dict[str, str], tag: str | None = None) -> str:
    distinct = set(versions.values())
    if len(distinct) != 1:
        details = ", ".join(f"{name}={version}" for name, version in sorted(versions.items()))
        raise ValueError(f"release versions disagree: {details}")
    version = next(iter(distinct))
    if SEMVER.fullmatch(version) is None:
        raise ValueError(f"release version is not stable SemVer: {version}")
    if tag is not None and tag != f"v{version}":
        raise ValueError(f"release tag {tag!r} does not match v{version}")
    return version


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tag", help="Require this exact vMAJOR.MINOR.PATCH tag")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args(argv)
    try:
        version = validate_versions(collect_versions(args.root), args.tag)
    except (OSError, TypeError, ValueError, json.JSONDecodeError, tomllib.TOMLDecodeError) as error:
        print(f"release metadata invalid: {error}", file=sys.stderr)
        return 1
    print(version)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
