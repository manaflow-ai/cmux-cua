#!/usr/bin/env python3
"""Validate the package and workflow identity before an npm OIDC publish."""

from __future__ import annotations

import json
import os
import posixpath
import re
import sys
import tarfile
from pathlib import Path
from urllib.parse import urlsplit

from validate_publish_artifacts import ArtifactError, regular_files


MAX_PACKAGE_JSON_BYTES = 1024 * 1024
REPOSITORY_PATTERN = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
WORKFLOW_PATTERN = re.compile(r"[^/]+\.(?:yml|yaml)\Z")


class IdentityError(ArtifactError):
    """Raised when a publish identity does not match the explicit allowlist."""


def required_environment(name: str, description: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        raise IdentityError(f"{description} ({name}) is required; refusing to publish")
    return value


def repository_from_url(value: object) -> str:
    if not isinstance(value, str):
        raise IdentityError("package.json repository.url is missing or not a string")

    url = value.strip()
    if url.startswith("git+"):
        url = url[4:]
    parsed = urlsplit(url)
    try:
        port = parsed.port
    except ValueError as error:
        raise IdentityError("package.json repository.url has an invalid port") from error
    if parsed.scheme != "https" or parsed.hostname != "github.com":
        raise IdentityError("package.json repository.url must be an HTTPS github.com URL")
    if parsed.username or parsed.password or port or parsed.query or parsed.fragment:
        raise IdentityError("package.json repository.url contains unsupported URL components")

    path = parsed.path.rstrip("/")
    if path.endswith(".git"):
        path = path[:-4]
    repository = path.lstrip("/")
    if not REPOSITORY_PATTERN.fullmatch(repository):
        raise IdentityError(
            "package.json repository.url must identify exactly one GitHub repository"
        )
    return repository


def package_repository(package: dict[str, object]) -> str:
    repository = package.get("repository")
    if isinstance(repository, str):
        return repository_from_url(repository)
    if isinstance(repository, dict):
        return repository_from_url(repository.get("url"))
    raise IdentityError("package.json repository is missing")


def package_json_from_artifact(artifact_directory: Path) -> dict[str, object]:
    try:
        archives = regular_files(artifact_directory, (".tgz",), exactly_one=True)
    except ArtifactError as error:
        raise IdentityError(f"npm artifact directory is invalid: {error}") from error

    try:
        with tarfile.open(archives[0], mode="r:gz") as archive:
            package_json_members: list[tarfile.TarInfo] = []
            for member in archive.getmembers():
                if (
                    member.name.startswith("/")
                    or "\\" in member.name
                    or posixpath.normpath(member.name) != member.name
                    or ".." in member.name.split("/")
                ):
                    raise IdentityError("npm artifact contains an unsafe archive path")
                if member.name == "package/package.json":
                    if not member.isreg():
                        raise IdentityError("npm artifact package.json is not a regular file")
                    package_json_members.append(member)

            if len(package_json_members) != 1:
                raise IdentityError("npm artifact must contain exactly one package/package.json")
            member = package_json_members[0]
            if member.size > MAX_PACKAGE_JSON_BYTES:
                raise IdentityError("npm artifact package.json is unexpectedly large")
            extracted = archive.extractfile(member)
            if extracted is None:
                raise IdentityError("npm artifact package.json cannot be read")
            payload = extracted.read(MAX_PACKAGE_JSON_BYTES + 1)
    except (OSError, tarfile.TarError) as error:
        raise IdentityError(f"cannot read npm artifact: {error}") from error

    if len(payload) > MAX_PACKAGE_JSON_BYTES:
        raise IdentityError("npm artifact package.json is unexpectedly large")
    try:
        package = json.loads(payload)
    except json.JSONDecodeError as error:
        raise IdentityError(f"npm artifact package.json is invalid JSON: {error}") from error
    if not isinstance(package, dict):
        raise IdentityError("npm artifact package.json must contain a JSON object")
    return package


def workflow_filename(workflow_ref: str, repository: str) -> str:
    prefix = f"{repository}/.github/workflows/"
    if not workflow_ref.startswith(prefix):
        raise IdentityError("GitHub workflow identity is not from the expected repository")
    filename = workflow_ref[len(prefix) :].split("@", 1)[0]
    if not WORKFLOW_PATTERN.fullmatch(filename):
        raise IdentityError("GitHub workflow identity has an invalid workflow filename")
    return filename


def validate(artifact_directory: Path) -> None:
    expected_repository = required_environment(
        "TRUSTED_PUBLISHER_REPOSITORY", "trusted publisher repository"
    )
    if not REPOSITORY_PATTERN.fullmatch(expected_repository):
        raise IdentityError("trusted publisher repository must use owner/repository form")
    expected_package = required_environment("TRUSTED_PACKAGE_NAME", "trusted package name")
    expected_workflow = required_environment(
        "TRUSTED_PUBLISHER_WORKFLOW", "trusted publisher workflow filename"
    )
    if not WORKFLOW_PATTERN.fullmatch(expected_workflow):
        raise IdentityError("trusted publisher workflow must be a .yml or .yaml filename")

    ref_type = required_environment("GITHUB_REF_TYPE", "GitHub ref type")
    if ref_type != "tag":
        raise IdentityError("npm publishing is allowed only from a tag ref")
    if required_environment("GITHUB_REF_PROTECTED", "GitHub ref protection") != "true":
        raise IdentityError("npm publishing requires a protected tag ref")

    current_repository = required_environment("GITHUB_REPOSITORY", "GitHub repository")
    if current_repository != expected_repository:
        raise IdentityError(
            "current GitHub repository "
            f"{current_repository!r} does not match the allowlisted trusted publisher "
            f"{expected_repository!r}; this fork must not publish the package"
        )

    actual_workflow = workflow_filename(
        required_environment("GITHUB_WORKFLOW_REF", "GitHub workflow reference"),
        expected_repository,
    )
    if actual_workflow != expected_workflow:
        raise IdentityError(
            f"caller workflow {actual_workflow!r} does not match allowlisted workflow "
            f"{expected_workflow!r}"
        )

    package = package_json_from_artifact(artifact_directory)
    actual_package = package.get("name")
    if actual_package != expected_package:
        raise IdentityError(
            f"artifact package name {actual_package!r} does not match allowlisted name "
            f"{expected_package!r}"
        )
    if not isinstance(actual_package, str) or not actual_package:
        raise IdentityError("artifact package.json name is missing or invalid")

    owner = expected_repository.split("/", 1)[0]
    if actual_package.startswith("@"):
        scope, separator, _ = actual_package.partition("/")
        if not separator or scope != f"@{owner}":
            raise IdentityError(
                f"artifact package scope {scope!r} does not match trusted publisher owner "
                f"{owner!r}"
            )
    if package_repository(package) != expected_repository:
        raise IdentityError(
            "artifact package repository does not match the allowlisted trusted publisher "
            f"{expected_repository!r}"
        )


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {Path(sys.argv[0]).name} ARTIFACT_DIRECTORY", file=sys.stderr)
        return 2
    try:
        validate(Path(sys.argv[1]))
    except (ArtifactError, OSError) as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    print("npm trusted publisher identity validated")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
