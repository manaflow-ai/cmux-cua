#!/usr/bin/env python3
"""Validate release identity and Python metadata before a PyPI token upload."""

from __future__ import annotations

import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path

from validate_publish_artifacts import ArtifactError, validate_python_artifacts


REPOSITORY_PATTERN = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
WORKFLOW_PATTERN = re.compile(r"[^/]+\.(?:yml|yaml)\Z")
TAG_PREFIX_PATTERN = re.compile(r"[A-Za-z0-9._/-]+-v\Z")
VERSION_PATTERN = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z"
)
SHA_PATTERN = re.compile(r"[0-9a-f]{40}\Z")


class IdentityError(ArtifactError):
    """Raised when a release does not match its explicit publisher allowlist."""


@dataclass(frozen=True)
class PublisherAllowlist:
    repository: str
    package: str
    workflow: str
    tag_prefix: str
    version: str

    @property
    def tag(self) -> str:
        return f"{self.tag_prefix}{self.version}"


def required_environment(name: str, description: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        raise IdentityError(f"{description} ({name}) is required; refusing to publish")
    return value


def workflow_parts(workflow_ref: str, expected_repository: str) -> tuple[str, str]:
    prefix = f"{expected_repository}/.github/workflows/"
    if not workflow_ref.startswith(prefix):
        raise IdentityError("GitHub workflow identity is not from the expected repository")
    path, separator, ref = workflow_ref.partition("@")
    filename = path[len(prefix) :]
    if not separator or not ref or not WORKFLOW_PATTERN.fullmatch(filename):
        raise IdentityError("GitHub workflow identity has an invalid filename or ref")
    return filename, ref


def read_allowlist() -> PublisherAllowlist:
    repository = required_environment(
        "TRUSTED_PUBLISHER_REPOSITORY", "trusted publisher repository"
    )
    if not REPOSITORY_PATTERN.fullmatch(repository):
        raise IdentityError("trusted publisher repository must use owner/repository form")
    package = required_environment("TRUSTED_PACKAGE_NAME", "trusted package name")
    workflow = required_environment(
        "TRUSTED_PUBLISHER_WORKFLOW", "trusted publisher workflow filename"
    )
    if not WORKFLOW_PATTERN.fullmatch(workflow):
        raise IdentityError("trusted publisher workflow must be a .yml or .yaml filename")
    tag_prefix = required_environment("TRUSTED_TAG_PREFIX", "trusted tag prefix")
    if not TAG_PREFIX_PATTERN.fullmatch(tag_prefix):
        raise IdentityError("trusted tag prefix contains unsupported characters")
    version = required_environment("EXPECTED_VERSION", "expected release version")
    if not VERSION_PATTERN.fullmatch(version):
        raise IdentityError("expected release version must be exact SemVer major.minor.patch")
    return PublisherAllowlist(repository, package, workflow, tag_prefix, version)


def _expected_workflow_ref(
    event_name: str, ref_type: str, ref_name: str, expected_tag: str
) -> str:
    if event_name == "workflow_run":
        if ref_type != "branch" or ref_name != "main":
            raise IdentityError("workflow_run publishing requires the protected main ref")
        source_tag = required_environment("SOURCE_TAG", "source release tag")
        if source_tag != expected_tag:
            raise IdentityError(
                f"source tag must be exactly {expected_tag!r} for this package release"
            )
        source_sha = required_environment("SOURCE_SHA", "source release commit")
        if not SHA_PATTERN.fullmatch(source_sha):
            raise IdentityError("source release commit must be a full lowercase SHA")
        return "refs/heads/main"
    if event_name == "push":
        if ref_type != "tag":
            raise IdentityError("PyPI publishing is allowed only from a tag ref")
        if ref_name != expected_tag:
            raise IdentityError(
                f"GitHub tag must be exactly {expected_tag!r} for this package release"
            )
        return f"refs/tags/{expected_tag}"
    raise IdentityError(f"unsupported GitHub event {event_name!r}")


def validate_context(allowlist: PublisherAllowlist) -> None:
    expected_tag = allowlist.tag

    if required_environment("GITHUB_REF_PROTECTED", "GitHub ref protection") != "true":
        raise IdentityError("PyPI publishing requires a protected release ref")
    current_repository = required_environment("GITHUB_REPOSITORY", "GitHub repository")
    if current_repository != allowlist.repository:
        raise IdentityError(
            "current GitHub repository "
            f"{current_repository!r} does not match the allowlisted trusted publisher "
            f"{allowlist.repository!r}; this fork must not publish the package"
        )
    expected_workflow_ref = _expected_workflow_ref(
        os.environ.get("GITHUB_EVENT_NAME", "push").strip(),
        required_environment("GITHUB_REF_TYPE", "GitHub ref type"),
        required_environment("GITHUB_REF_NAME", "GitHub ref name"),
        expected_tag,
    )

    actual_workflow, workflow_ref = workflow_parts(
        required_environment("GITHUB_WORKFLOW_REF", "GitHub workflow reference"),
        allowlist.repository,
    )
    if actual_workflow != allowlist.workflow:
        raise IdentityError(
            f"caller workflow {actual_workflow!r} does not match allowlisted workflow "
            f"{allowlist.workflow!r}"
        )
    if workflow_ref != expected_workflow_ref:
        raise IdentityError("caller workflow ref is not the protected release ref")


def validate(artifact_directory: Path) -> None:
    allowlist = read_allowlist()
    validate_context(allowlist)
    try:
        validate_python_artifacts(
            artifact_directory,
            expected_package=allowlist.package,
            expected_version=allowlist.version,
            max_files=2,
        )
    except ArtifactError as error:
        raise IdentityError(f"Python release artifact identity is invalid: {error}") from error


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {Path(sys.argv[0]).name} ARTIFACT_DIRECTORY", file=sys.stderr)
        return 2
    try:
        validate(Path(sys.argv[1]))
    except (ArtifactError, OSError) as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    print("PyPI trusted publisher identity validated")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
