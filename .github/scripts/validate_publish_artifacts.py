#!/usr/bin/env python3
"""Validate downloaded release artifacts before a registry upload."""

from __future__ import annotations

import argparse
import stat
import sys
from pathlib import Path


class ArtifactError(ValueError):
    """Raised when an artifact directory is not safe to publish."""


def regular_files(
    directory: Path,
    suffixes: tuple[str, ...],
    *,
    exactly_one: bool = False,
) -> tuple[Path, ...]:
    """Return regular files with approved suffixes, rejecting everything else."""
    if not suffixes or any(not suffix for suffix in suffixes):
        raise ArtifactError("at least one non-empty artifact suffix is required")
    try:
        directory_mode = directory.lstat().st_mode
    except OSError as error:
        raise ArtifactError(f"cannot inspect artifact directory: {error}") from error
    if not stat.S_ISDIR(directory_mode):
        raise ArtifactError(f"artifact directory is not a real directory: {directory}")
    try:
        entries = sorted(directory.iterdir(), key=lambda path: path.name)
    except OSError as error:
        raise ArtifactError(f"cannot inspect artifact directory: {error}") from error

    artifacts: list[Path] = []
    for entry in entries:
        try:
            entry_mode = entry.lstat().st_mode
        except OSError as error:
            raise ArtifactError(f"cannot inspect artifact entry: {error}") from error
        if not stat.S_ISREG(entry_mode):
            raise ArtifactError(f"artifact entry is not a regular file: {entry.name}")
        if not entry.name.endswith(suffixes):
            raise ArtifactError(f"artifact has an unexpected file type: {entry.name}")
        artifacts.append(entry)

    if not artifacts:
        raise ArtifactError(
            f"artifact directory contains no files ending in {', '.join(suffixes)}"
        )
    if exactly_one and len(artifacts) != 1:
        raise ArtifactError(f"expected exactly one artifact, found {len(artifacts)}")
    return tuple(artifacts)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--suffix", action="append", required=True)
    parser.add_argument("--exactly-one", action="store_true")
    args = parser.parse_args()
    try:
        artifacts = regular_files(
            args.directory,
            tuple(args.suffix),
            exactly_one=args.exactly_one,
        )
    except (ArtifactError, OSError) as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    print(f"validated {len(artifacts)} release artifact(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
