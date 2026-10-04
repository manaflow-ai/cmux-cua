#!/usr/bin/env python3
"""Print the release version after checking it against the Rust workspace.

usage: check-release-version.py CARGO_TOML GITHUB_REF [DISPATCH_VERSION]

A tag ref must be refs/tags/cmux-cua-vX.Y.Z; any other ref takes
DISPATCH_VERSION. Either way the version must equal [workspace.package]
version in CARGO_TOML, and so must every other version string of the
release (VERSION_STRINGS), or the release would ship parts that report
another version. Exit 1 with the reason on a mismatch.
"""
import re
import sys
import tomllib
from pathlib import Path

# Every other place the release version is written, relative to libs/cmux-cua.
VERSION_STRINGS = (
    ("rust/.bumpversion.cfg", r'^current_version = (\S+)$'),
    ("python/pyproject.toml", r'^version = "([^"]+)"'),
    ("python/src/cmux_cua/__init__.py", r'^__version__ = "([^"]+)"'),
    ("scripts/_install-rust.sh", r'^CMUX_CUA_BAKED_VERSION="([^"]+)"'),
    ("scripts/install.ps1", r'CmuxCuaBakedVersion = "([^"]+)"'),
)

def main(argv):
    if len(argv) not in (2, 3):
        sys.exit(__doc__)
    cargo, ref = argv[0], argv[1]
    if ref.startswith("refs/tags/"):
        m = re.fullmatch(r"refs/tags/cmux-cua-v([0-9]+\.[0-9]+\.[0-9]+)", ref)
        if not m:
            sys.exit(f"tag must be cmux-cua-vX.Y.Z (got {ref})")
        version = m.group(1)
    else:
        version = argv[2] if len(argv) == 3 else ""
        if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
            sys.exit(f"a non-tag run needs a version X.Y.Z (got {version!r})")
    with open(cargo, "rb") as f:
        workspace = tomllib.load(f)["workspace"]["package"]["version"]
    if workspace != version:
        sys.exit(f"release version {version} differs from {cargo} workspace version {workspace}")
    root = Path(cargo).resolve().parent.parent
    stale = []
    for rel, pattern in VERSION_STRINGS:
        path = root / rel
        if not path.exists():
            continue
        m = re.search(pattern, path.read_text(), re.M)
        if not m:
            stale.append(f"{rel}: no version string matching {pattern}")
        elif m.group(1) != version:
            stale.append(f"{rel}: {m.group(1)}")
    if stale:
        sys.exit(f"release {version}: other version strings differ:\n  " + "\n  ".join(stale))
    print(version)

if __name__ == "__main__":
    main(sys.argv[1:])
