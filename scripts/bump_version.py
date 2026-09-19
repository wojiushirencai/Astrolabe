#!/usr/bin/env python3
"""
Astrolabe version bumper, modeled after Serena's scripts/bump_version.py.

Synchronizes version across:
  - Cargo.toml ([workspace.package] version = "...")
  - crates/astrolabe-mcp/Cargo.toml (astrolabe-core version = "...")
  - npm/package.json ("version": "...")
  - npm/packages/*/package.json ("version": "...")
  - npm/packages/astrolabe/package.json (optionalDependencies "@astrolabe/*": "...")
  - Cargo.lock (refreshed via `cargo metadata`)
  - CHANGELOG.md (moves Unreleased to versioned section)
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from datetime import datetime
from pathlib import Path

VERSION_PATTERN = re.compile(r"^(?P<major>\d+)\.(?P<minor>\d+)\.(?P<patch>\d+)(\.\w+)?$")
CARGO_WORKSPACE_VERSION_PATTERN = re.compile(
    r'(?m)^(\[workspace\.package\][^\[]*?^version\s*=\s*")(?P<version>\d+\.\d+\.\d+(?:\.\w+)?)(?P<after>"\s*$)'
)
MCP_CARGO_DEP_VERSION_PATTERN = re.compile(
    r'(?m)^(astrolabe-core\s*=\s*\{[^}]*?version\s*=\s*")(?P<version>\d+\.\d+\.\d+(?:\.\w+)?)(?P<after>"[^}]*\})'
)


def find_repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def extract_workspace_version(cargo_text: str) -> str:
    match = CARGO_WORKSPACE_VERSION_PATTERN.search(cargo_text)
    if not match:
        raise ValueError("Could not find [workspace.package] version in Cargo.toml")
    return match.group("version")


def increment_version(version: str, part: str) -> str:
    match = VERSION_PATTERN.fullmatch(version)
    if not match:
        raise ValueError(f"Unsupported version format: {version}")
    major = int(match.group("major"))
    minor = int(match.group("minor"))
    patch = int(match.group("patch"))
    if part == "major":
        return f"{major + 1}.0.0"
    elif part == "minor":
        return f"{major}.{minor + 1}.0"
    elif part == "patch":
        return f"{major}.{minor}.{patch + 1}"
    else:
        raise ValueError(f"Unknown version part: {part}")


def update_changelog(text: str, new_version: str) -> str:
    unreleased_marker = "## [Unreleased]"
    if unreleased_marker not in text:
        return text
    today = datetime.now().strftime("%Y-%m-%d")
    replacement = f"{unreleased_marker}\n\n## [{new_version}] - {today}"
    return text.replace(unreleased_marker, replacement, 1)


def bump_version(part: str | None, target_version: str | None, dry_run: bool) -> str:
    repo_root = find_repo_root()
    cargo_path = repo_root / "Cargo.toml"
    cargo_text = cargo_path.read_text(encoding="utf-8")
    current_version = extract_workspace_version(cargo_text)

    if target_version:
        if not VERSION_PATTERN.fullmatch(target_version):
            raise ValueError(f"Invalid target version: {target_version}")
        new_version = target_version
    elif part:
        new_version = increment_version(current_version, part)
    else:
        raise ValueError("Either part (--major/--minor/--patch) or --version must be specified")

    print(f"Current version: {current_version}")
    print(f"New version:     {new_version}")

    new_cargo_text = CARGO_WORKSPACE_VERSION_PATTERN.sub(
        lambda m: f"{m.group(1)}{new_version}{m.group('after')}", cargo_text
    )

    mcp_cargo_path = repo_root / "crates" / "astrolabe-mcp" / "Cargo.toml"
    mcp_cargo_text = mcp_cargo_path.read_text(encoding="utf-8")
    new_mcp_cargo_text = MCP_CARGO_DEP_VERSION_PATTERN.sub(
        lambda m: f"{m.group(1)}{new_version}{m.group('after')}", mcp_cargo_text
    )

    changelog_path = repo_root / "CHANGELOG.md"
    changelog_text = changelog_path.read_text(encoding="utf-8")
    new_changelog_text = update_changelog(changelog_text, new_version)

    npm_json_files = [
        p
        for p in (repo_root / "npm").glob("**/package.json")
        if "node_modules" not in p.parts
    ]

    if dry_run:
        print("[DRY-RUN] Would update Cargo.toml, astrolabe-mcp/Cargo.toml, CHANGELOG.md,")
        print(f"[DRY-RUN] and {len(npm_json_files)} npm package.json files (version + @astrolabe/* optionalDependencies)")
        return new_version

    cargo_path.write_text(new_cargo_text, encoding="utf-8")
    mcp_cargo_path.write_text(new_mcp_cargo_text, encoding="utf-8")
    changelog_path.write_text(new_changelog_text, encoding="utf-8")

    for pkg_json_path in npm_json_files:
        try:
            data = json.loads(pkg_json_path.read_text(encoding="utf-8"))
            changed = False
            if "version" in data:
                data["version"] = new_version
                changed = True
            # Keep the main package's optionalDependencies pinned to the exact
            # platform-package versions; publish.js asserts they match.
            optional = data.get("optionalDependencies")
            if isinstance(optional, dict):
                for dep_name, dep_version in optional.items():
                    if dep_name.startswith("@astrolabe/") and dep_version != new_version:
                        optional[dep_name] = new_version
                        changed = True
            if changed:
                pkg_json_path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
        except Exception as e:
            print(f"Warning: could not update {pkg_json_path}: {e}")

    # Refresh Cargo.lock so path-crate versions match (fast resolve, no build).
    try:
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--offline"],
            cwd=repo_root,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=True,
        )
    except subprocess.CalledProcessError:
        try:
            subprocess.run(
                ["cargo", "metadata", "--format-version", "1"],
                cwd=repo_root,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=True,
            )
        except (subprocess.CalledProcessError, FileNotFoundError) as e:
            print(f"Warning: could not refresh Cargo.lock automatically ({e}); run cargo check before building")
    except FileNotFoundError:
        print("Warning: cargo not found on PATH; Cargo.lock not refreshed")
    print(f"Successfully bumped version to {new_version}")
    return new_version


def main() -> None:
    parser = argparse.ArgumentParser(description="Bump Astrolabe workspace version (Serena pattern).")
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--major", action="store_true", help="Bump major version")
    group.add_argument("--minor", action="store_true", help="Bump minor version")
    group.add_argument("--patch", action="store_true", help="Bump patch version")
    group.add_argument("--version", "-v", dest="target_version", help="Set explicit version X.Y.Z")
    parser.add_argument("--dry-run", action="store_true", help="Dry run without writing files")

    args = parser.parse_args()
    part = "major" if args.major else "minor" if args.minor else "patch" if args.patch else None
    bump_version(part, args.target_version, args.dry_run)


if __name__ == "__main__":
    main()
