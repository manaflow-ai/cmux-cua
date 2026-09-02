#!/usr/bin/env python3
"""Resolve and validate a package release version without shell interpolation."""

from __future__ import annotations

import json
import os
import re
from pathlib import Path

SEMVER_RE = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
PREFIX_RE = re.compile(r"^[A-Za-z0-9._/-]+-v$")


class VersionError(ValueError):
    """Raised when a release version cannot be trusted."""


def resolve_version(
    *,
    event_name: str,
    ref_type: str,
    ref_name: str,
    version_input: str,
    tag_prefix: str,
    package_json: str = "",
) -> tuple[str, str]:
    """Return ``(version, release_tag)`` for a push or manual build."""

    if not PREFIX_RE.fullmatch(tag_prefix):
        raise VersionError("release tag prefix is invalid")

    if event_name == "push":
        if ref_type != "tag":
            raise VersionError("push release must use a tag ref")
        if not ref_name.startswith(tag_prefix):
            raise VersionError("push ref does not use the expected release prefix")
        version = ref_name[len(tag_prefix) :]
    elif event_name == "workflow_dispatch":
        version = version_input.strip()
        if not version:
            if not package_json:
                raise VersionError("manual builds require a version")
            try:
                version = json.loads(Path(package_json).read_text(encoding="utf-8"))["version"]
            except (OSError, KeyError, TypeError, json.JSONDecodeError) as error:
                raise VersionError("package.json does not contain a readable version") from error
    else:
        raise VersionError("release workflows accept only push or workflow_dispatch events")

    if not isinstance(version, str) or not SEMVER_RE.fullmatch(version):
        raise VersionError("release version must be exact SemVer major.minor.patch")
    return version, f"{tag_prefix}{version}"


def main() -> int:
    version, tag = resolve_version(
        event_name=os.environ.get("RELEASE_EVENT_NAME", ""),
        ref_type=os.environ.get("RELEASE_REF_TYPE", ""),
        ref_name=os.environ.get("RELEASE_REF_NAME", ""),
        version_input=os.environ.get("RELEASE_VERSION_INPUT", ""),
        tag_prefix=os.environ.get("RELEASE_TAG_PREFIX", ""),
        package_json=os.environ.get("RELEASE_PACKAGE_JSON", ""),
    )
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as stream:
            stream.write(f"version={version}\n")
            stream.write(f"tag={tag}\n")
    print(f"Resolved release version {version}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except VersionError as error:
        print(f"::error::{error}")
        raise SystemExit(1) from error
