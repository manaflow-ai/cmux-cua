#!/usr/bin/env python3
"""Print the release version after checking it against the Rust workspace.

usage: check-release-version.py CARGO_TOML GITHUB_REF [DISPATCH_VERSION]

A tag ref must be refs/tags/cmux-cua-vX.Y.Z; any other ref takes
DISPATCH_VERSION. Either way the version must equal [workspace.package]
version in CARGO_TOML, or the release would ship binaries that report
another version. Exit 1 with the reason on a mismatch.
"""
import re
import sys
import tomllib

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
    print(version)

if __name__ == "__main__":
    main(sys.argv[1:])
