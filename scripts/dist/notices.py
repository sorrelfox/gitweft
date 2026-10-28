#!/usr/bin/env python3
"""The license notices of the macOS and Windows packages.

Usage: scripts/dist/notices.py OUT_FILE TARGET...

Writes OUT_FILE (THIRD-PARTY-LICENSES.txt) for the crates a build for the Rust targets
TARGET... uses (aarch64-apple-darwin, x86_64-pc-windows-msvc, ...): the same text sources.py
writes for the Linux packages, with the license files read from cargo's cache, which
`cargo fetch --locked` fills. Needs Python 3.11 or later, as sources.py does.
"""

import json
import pathlib
import sys

sys.dont_write_bytecode = True
import sources  # noqa: E402 (after the line above, so no __pycache__ lands in the tree)


class CrateDirs:
    """What sources.notices() reads the license files from: `dirs / "NAME-VERSION"` is that
    crate's folder in cargo's cache, as it would be in vendor/."""

    def __init__(self, packages):
        self.dirs = {
            f"{name}-{ver}": pathlib.Path(package["manifest_path"]).parent
            for (name, ver), package in packages.items()
        }

    def __truediv__(self, name):
        return self.dirs[name]


def packages(targets):
    """(name, version) → package metadata, for every crates.io package a build for any of
    `targets` uses."""
    platforms = [arg for target in targets for arg in ("--filter-platform", target)]
    meta = json.loads(sources.run(
        "cargo", "metadata", "--offline", "--locked", "--format-version", "1", *platforms,
    ))
    used = {node["id"] for node in meta["resolve"]["nodes"]}
    return {
        (p["name"], p["version"]): p
        for p in meta["packages"]
        if p["id"] in used and p["source"]
    }


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    out = pathlib.Path(sys.argv[1])
    used = packages(sys.argv[2:])
    text = sources.notices(CrateDirs(used), used)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(text, encoding="utf-8", newline="\n")
    print(f"{out}: {len(used)} crates, {len(text) // 1024} KiB")


if __name__ == "__main__":
    main()
