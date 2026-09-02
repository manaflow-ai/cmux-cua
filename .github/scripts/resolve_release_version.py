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


def _version_from_tag(tag: str, tag_prefix: str, label: str) -> str:
    if not tag:
        raise VersionError(f"{label} release requires a validated tag")
    if not tag.startswith(tag_prefix):
        raise VersionError(f"{label} tag does not use the expected release prefix")
    return tag[len(tag_prefix) :]


def _manual_version(version_input: str, package_json: str) -> object:
    version = version_input.strip()
    if version:
        return version
    if not package_json:
        raise VersionError("manual builds require a version")
    try:
        return json.loads(Path(package_json).read_text(encoding="utf-8"))["version"]
    except (OSError, KeyError, TypeError, json.JSONDecodeError) as error:
        raise VersionError("package.json does not contain a readable version") from error


def resolve_version(
    *,
    event_name: str,
    ref_type: str,
    ref_name: str,
    version_input: str,
    tag_prefix: str,
    package_json: str = "",
    source_tag: str = "",
) -> tuple[str, str]:
    """Return ``(version, release_tag)`` for a trusted release or manual build.

    ``workflow_run`` consumers receive the source tag from the protected
    observer validator.  The tag is passed explicitly instead of reading the
    untrusted event payload in a shell step.
    """

    if not PREFIX_RE.fullmatch(tag_prefix):
        raise VersionError("release tag prefix is invalid")

    if event_name == "push":
        if ref_type != "tag":
            raise VersionError("push release must use a tag ref")
        version = _version_from_tag(ref_name, tag_prefix, "push")
    elif event_name == "workflow_dispatch":
        version = _manual_version(version_input, package_json)
    elif event_name == "workflow_run":
        version = _version_from_tag(source_tag, tag_prefix, "validated source")
    else:
        raise VersionError(
            "release workflows accept only workflow_run, push, or workflow_dispatch events"
        )

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
        source_tag=os.environ.get("RELEASE_SOURCE_TAG", ""),
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
