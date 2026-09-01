#!/usr/bin/env python3
"""Reject product-specific and out-of-tree dependencies from Tarit."""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections.abc import Iterable
from pathlib import Path
from typing import Any

import tomllib

CARGO_DEPENDENCY_TABLES = {
    "dependencies",
    "dev-dependencies",
    "build-dependencies",
    "workspace.dependencies",
}
NPM_DEPENDENCY_TABLES = {
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
}
SOURCE_SUFFIXES = {".rs", ".py", ".ts", ".tsx", ".js", ".mjs", ".cjs"}
IGNORED_DIRECTORIES = {".git", "node_modules", "target", "dist", "__pycache__"}
PRODUCT_DEPENDENCY = re.compile(
    r"(?:^|[/@_.-])instavm(?:$|[/_.-])"
    r"|github\.com/instavm/(?!tarit(?:$|[/.#]))"
    r"|github\.com/bandarlabs/sandbox(?:$|[/.#])"
    r"|@bandarlabs/sandbox(?:$|[/_.-])",
    re.IGNORECASE,
)
SOURCE_IMPORT = re.compile(
    r"^\s*(?:use|extern\s+crate|import|from)\s+[^\n]*"
    r"(?:@instavm/|instavm\b|instavm[_-]|@bandarlabs/sandbox|app\.domain\.firecracker)",
    re.IGNORECASE | re.MULTILINE,
)


def cargo_dependency_tables(value: Any, path: tuple[str, ...] = ()) -> Iterable[dict[str, Any]]:
    if not isinstance(value, dict):
        return
    dotted = ".".join(path)
    if path and (path[-1] in CARGO_DEPENDENCY_TABLES or dotted.endswith(".workspace.dependencies")):
        yield value
        return
    for key, child in value.items():
        yield from cargo_dependency_tables(child, (*path, key))


def dependency_text(name: str, specification: Any) -> str:
    if isinstance(specification, str):
        return f"{name} {specification}"
    if isinstance(specification, dict):
        fields = [name]
        for key in ("package", "git", "path"):
            if key in specification:
                fields.append(str(specification[key]))
        return " ".join(fields)
    return name


def check_local_path(root: Path, manifest: Path, raw_path: str) -> str | None:
    resolved = (manifest.parent / raw_path).resolve()
    try:
        resolved.relative_to(root)
    except ValueError:
        return f"{manifest}: dependency path escapes repository: {raw_path}"
    return None


def check_cargo(root: Path, manifest: Path) -> list[str]:
    errors: list[str] = []
    data = tomllib.loads(manifest.read_text(encoding="utf-8"))
    for table in cargo_dependency_tables(data):
        for name, specification in table.items():
            if PRODUCT_DEPENDENCY.search(dependency_text(name, specification)):
                errors.append(f"{manifest}: product-specific Cargo dependency {name!r}")
            if (
                isinstance(specification, dict)
                and isinstance(specification.get("path"), str)
                and (error := check_local_path(root, manifest, specification["path"]))
            ):
                errors.append(error)
    workspace = data.get("workspace", {})
    for field in ("members", "default-members"):
        for raw_path in workspace.get(field, []):
            if error := check_local_path(root, manifest, str(raw_path)):
                errors.append(error)
    return errors


def check_python(root: Path, manifest: Path) -> list[str]:
    data = tomllib.loads(manifest.read_text(encoding="utf-8"))
    project = data.get("project", {})
    values = list(project.get("dependencies", []))
    for group in project.get("optional-dependencies", {}).values():
        values.extend(group)
    values.extend(data.get("build-system", {}).get("requires", []))
    for group in data.get("dependency-groups", {}).values():
        values.extend(group)
    errors = []
    for value in values:
        text = str(value)
        if PRODUCT_DEPENDENCY.search(text):
            errors.append(f"{manifest}: product-specific Python dependency {value!r}")
        if " @ file:" in text:
            raw_path = text.split(" @ file:", 1)[1]
            raw_path = raw_path.removeprefix("//")
            if error := check_local_path(root, manifest, raw_path):
                errors.append(error)
    uv = data.get("tool", {}).get("uv", {})
    for name, specification in uv.get("sources", {}).items():
        if PRODUCT_DEPENDENCY.search(dependency_text(name, specification)):
            errors.append(f"{manifest}: product-specific Python source {name!r}")
        if isinstance(specification, dict) and isinstance(specification.get("path"), str):
            if error := check_local_path(root, manifest, specification["path"]):
                errors.append(error)
    for raw_path in uv.get("workspace", {}).get("members", []):
        if error := check_local_path(root, manifest, str(raw_path)):
            errors.append(error)
    return errors


def check_npm(root: Path, manifest: Path) -> list[str]:
    data = json.loads(manifest.read_text(encoding="utf-8"))
    errors: list[str] = []
    for table_name in NPM_DEPENDENCY_TABLES:
        for name, specification in data.get(table_name, {}).items():
            if PRODUCT_DEPENDENCY.search(f"{name} {specification}"):
                errors.append(f"{manifest}: product-specific npm dependency {name!r}")
            if (
                isinstance(specification, str)
                and specification.startswith("file:")
                and (
                    error := check_local_path(
                        root, manifest, specification.removeprefix("file:")
                    )
                )
            ):
                errors.append(error)
    workspaces = data.get("workspaces", [])
    if isinstance(workspaces, dict):
        workspaces = workspaces.get("packages", [])
    for raw_path in workspaces:
        if error := check_local_path(root, manifest, str(raw_path)):
            errors.append(error)
    return errors


def source_files(root: Path) -> Iterable[Path]:
    for path in root.rglob("*"):
        if any(part in IGNORED_DIRECTORIES for part in path.parts):
            continue
        if path.is_file() and path.suffix in SOURCE_SUFFIXES:
            yield path


def check_repository(root: Path) -> list[str]:
    root = root.resolve()
    errors: list[str] = []
    for manifest in root.rglob("Cargo.toml"):
        if not any(part in IGNORED_DIRECTORIES for part in manifest.parts):
            errors.extend(check_cargo(root, manifest))
    for manifest in root.rglob("pyproject.toml"):
        if not any(part in IGNORED_DIRECTORIES for part in manifest.parts):
            errors.extend(check_python(root, manifest))
    for manifest in root.rglob("package.json"):
        if not any(part in IGNORED_DIRECTORIES for part in manifest.parts):
            errors.extend(check_npm(root, manifest))
    for source in source_files(root):
        text = source.read_text(encoding="utf-8", errors="replace")
        if SOURCE_IMPORT.search(text):
            errors.append(f"{source}: product-specific source import")
    return sorted(errors)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    errors = check_repository(args.root)
    if errors:
        print("OSS dependency boundary failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print("PASS: Tarit dependency manifests and source imports are standalone")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
