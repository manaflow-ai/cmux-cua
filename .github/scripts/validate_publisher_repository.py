#!/usr/bin/env python3
"""Reject registry and release writes from non-canonical repositories.

Source provenance is checked separately.  This gate protects the registry
ownership boundary and must remain hard-coded until package metadata and the
provider's trusted-publisher configuration are migrated together.
"""

from __future__ import annotations

import os
import sys


CANONICAL_REPOSITORY = "trycua/cua"


def validate(repository: str | None = None) -> None:
    """Require the exact repository that owns the published packages."""

    value = repository if repository is not None else os.environ.get("GITHUB_REPOSITORY")
    if value != CANONICAL_REPOSITORY:
        actual = value or "<missing>"
        raise ValueError(
            "publisher writes are disabled for "
            f"{actual}; the canonical registry owner is {CANONICAL_REPOSITORY}. "
            "Migrate package metadata and trusted-publisher configuration "
            "together before enabling this fork."
        )


def main() -> int:
    try:
        validate()
    except ValueError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    print(f"publisher repository verified: {CANONICAL_REPOSITORY}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
