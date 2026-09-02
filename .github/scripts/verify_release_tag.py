#!/usr/bin/env python3
"""Verify that a release tag names a commit already reachable from main.

The release workflows run on tag pushes.  This small verifier gives every
publisher the same fail-closed check before it handles a package artifact or
requests a registry credential.  It uses only the GitHub REST API and the
Python standard library so the check has no third-party supply-chain input.
"""

from __future__ import annotations

import json
import os
import re
import sys
from dataclasses import dataclass
from typing import Any, Callable
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen

SHA_RE = re.compile(r"^[0-9a-f]{40}$")
SEMVER_RE = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
REPOSITORY_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
PREFIX_RE = re.compile(r"^[A-Za-z0-9._/-]+$")
MAX_RESPONSE_BYTES = 2 * 1024 * 1024


class VerificationError(RuntimeError):
    """A release provenance check failed."""


@dataclass(frozen=True)
class VerificationResult:
    """Validated release identity returned to the workflow."""

    tag: str
    version: str
    tag_sha: str
    main_sha: str


ApiGet = Callable[[str], dict[str, Any]]


def _require_sha(value: Any, label: str) -> str:
    if not isinstance(value, str) or not SHA_RE.fullmatch(value):
        raise VerificationError(f"{label} is not a full 40-character commit SHA")
    return value.lower()


def _validate_inputs(
    *,
    tag_prefix: str,
    tag: str,
    event_sha: str,
    repository: str,
    base_branch: str,
    trusted_repository: str = "",
) -> str:
    if not REPOSITORY_RE.fullmatch(repository):
        raise VerificationError("repository name is invalid")
    if trusted_repository:
        if not REPOSITORY_RE.fullmatch(trusted_repository):
            raise VerificationError("trusted repository name is invalid")
        if repository != trusted_repository:
            raise VerificationError(
                f"repository {repository!r} is not the trusted release repository"
            )
    if not PREFIX_RE.fullmatch(tag_prefix) or not tag_prefix.endswith("-v"):
        raise VerificationError("release tag prefix is invalid")
    if not tag.startswith(tag_prefix):
        raise VerificationError(f"tag {tag!r} does not use the expected release prefix")
    version = tag[len(tag_prefix) :]
    if not SEMVER_RE.fullmatch(version):
        raise VerificationError(f"tag {tag!r} does not contain an exact SemVer version")
    _require_sha(event_sha, "event SHA")
    if not re.fullmatch(r"^[A-Za-z0-9._/-]+$", base_branch):
        raise VerificationError("base branch name is invalid")
    return version


def _resolve_tag_target(api_get: ApiGet, repository: str, tag: str) -> str:
    """Resolve lightweight or one-level annotated tags to a commit SHA."""

    ref_path = f"/repos/{repository}/git/ref/tags/{quote(tag, safe='')}"
    ref = api_get(ref_path)
    if not isinstance(ref, dict) or not isinstance(ref.get("object"), dict):
        raise VerificationError("GitHub returned an invalid tag reference")

    obj = ref["object"]
    object_type = obj.get("type")
    object_sha = _require_sha(obj.get("sha"), "tag object SHA")
    if object_type == "commit":
        return object_sha
    if object_type != "tag":
        raise VerificationError(f"tag reference has unsupported object type {object_type!r}")

    annotated = api_get(f"/repos/{repository}/git/tags/{object_sha}")
    if not isinstance(annotated, dict) or not isinstance(annotated.get("object"), dict):
        raise VerificationError("GitHub returned an invalid annotated tag")
    target = annotated["object"]
    if target.get("type") != "commit":
        raise VerificationError("annotated release tag must point directly to a commit")
    return _require_sha(target.get("sha"), "annotated tag target SHA")


def _branch_sha(
    api_get: ApiGet,
    repository: str,
    base_branch: str,
    *,
    recheck: bool = False,
) -> str:
    branch = api_get(f"/repos/{repository}/branches/{quote(base_branch, safe='')}")
    label = "base branch on recheck" if recheck else "base branch"
    if not isinstance(branch, dict) or not isinstance(branch.get("commit"), dict):
        raise VerificationError(f"GitHub returned an invalid {label}")
    suffix = " on recheck" if recheck else ""
    return _require_sha(
        branch["commit"].get("sha"), f"{base_branch} branch SHA{suffix}"
    )


def _comparison_sha(comparison: dict[str, Any], key: str, label: str) -> str:
    commit = comparison.get(key)
    if not isinstance(commit, dict):
        raise VerificationError(f"ancestry comparison has no {label}")
    return _require_sha(commit.get("sha"), f"comparison {label} SHA")


def _validate_ancestry(
    api_get: ApiGet,
    repository: str,
    base_branch: str,
    tag_sha: str,
    main_sha: str,
) -> None:
    comparison = api_get(f"/repos/{repository}/compare/{tag_sha}...{main_sha}")
    if not isinstance(comparison, dict):
        raise VerificationError("GitHub returned an invalid ancestry comparison")
    status = comparison.get("status")
    if status not in {"ahead", "identical"}:
        raise VerificationError(
            f"release tag commit is not an ancestor of {base_branch} (status={status!r})"
        )
    if _comparison_sha(comparison, "base_commit", "base") != tag_sha:
        raise VerificationError("ancestry comparison base does not match the tag SHA")
    if _comparison_sha(comparison, "head_commit", "head") != main_sha:
        raise VerificationError("ancestry comparison head does not match main")
    if _comparison_sha(comparison, "merge_base_commit", "merge base") != tag_sha:
        raise VerificationError(
            "ancestry comparison merge base does not equal the release tag commit"
        )


def _validate_stable_refs(
    api_get: ApiGet,
    repository: str,
    base_branch: str,
    tag: str,
    expected_tag_sha: str,
    expected_main_sha: str,
) -> None:
    if _branch_sha(api_get, repository, base_branch, recheck=True) != expected_main_sha:
        raise VerificationError(f"{base_branch} moved while provenance was checked")
    if _resolve_tag_target(api_get, repository, tag) != expected_tag_sha:
        raise VerificationError("release tag moved while provenance was checked")


def verify_release_tag(
    *,
    tag_prefix: str,
    tag: str,
    event_sha: str,
    repository: str,
    api_get: ApiGet,
    base_branch: str = "main",
    trusted_repository: str = "",
) -> VerificationResult:
    """Validate tag syntax, ref identity, and ancestry from the live API."""

    version = _validate_inputs(
        tag_prefix=tag_prefix,
        tag=tag,
        event_sha=event_sha,
        repository=repository,
        base_branch=base_branch,
        trusted_repository=trusted_repository,
    )
    expected_sha = _require_sha(event_sha, "event SHA")

    first_target = _resolve_tag_target(api_get, repository, tag)
    if first_target != expected_sha:
        raise VerificationError(
            f"tag target {first_target} does not match event SHA {expected_sha}"
        )

    main_sha = _branch_sha(api_get, repository, base_branch)
    _validate_ancestry(
        api_get,
        repository,
        base_branch,
        expected_sha,
        main_sha,
    )
    _validate_stable_refs(
        api_get,
        repository,
        base_branch,
        tag,
        expected_sha,
        main_sha,
    )

    return VerificationResult(
        tag=tag,
        version=version,
        tag_sha=expected_sha,
        main_sha=main_sha,
    )


def _api_client(token: str, api_url: str) -> ApiGet:
    if not token:
        raise VerificationError("GH_TOKEN is required for release provenance checks")

    base_url = api_url.rstrip("/")

    def get(path: str) -> dict[str, Any]:
        request = Request(
            f"{base_url}{path}",
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {token}",
                "User-Agent": "manaflow-release-tag-verifier",
                "X-GitHub-Api-Version": "2022-11-28",
            },
            method="GET",
        )
        try:
            with urlopen(request, timeout=20) as response:
                payload = response.read(MAX_RESPONSE_BYTES + 1)
        except HTTPError as error:
            raise VerificationError(
                f"GitHub API rejected provenance request (HTTP {error.code})"
            ) from error
        except URLError as error:
            raise VerificationError("GitHub API provenance request failed") from error
        if len(payload) > MAX_RESPONSE_BYTES:
            raise VerificationError("GitHub API response exceeded the safety limit")
        try:
            value = json.loads(payload)
        except json.JSONDecodeError as error:
            raise VerificationError("GitHub API returned invalid JSON") from error
        if not isinstance(value, dict):
            raise VerificationError("GitHub API returned a non-object response")
        return value

    return get


def _write_outputs(result: VerificationResult) -> None:
    output_path = os.environ.get("GITHUB_OUTPUT")
    if not output_path:
        return
    with open(output_path, "a", encoding="utf-8") as output:
        output.write(f"tag={result.tag}\n")
        output.write(f"version={result.version}\n")
        output.write(f"tag_sha={result.tag_sha}\n")
        output.write(f"main_sha={result.main_sha}\n")


def main() -> int:
    event_name = os.environ.get("GITHUB_EVENT_NAME", "")
    if event_name != "push":
        raise VerificationError("release provenance is valid only for push events")
    if os.environ.get("GITHUB_REF_PROTECTED", "").lower() != "true":
        raise VerificationError("release tag is not protected by a GitHub ruleset")
    ref = os.environ.get("GITHUB_REF", "")
    if not ref.startswith("refs/tags/"):
        raise VerificationError("release provenance requires a tag push")
    tag = os.environ.get("GITHUB_REF_NAME") or ref.removeprefix("refs/tags/")
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    event_sha = os.environ.get("GITHUB_SHA", "")
    tag_prefix = os.environ.get("RELEASE_TAG_PREFIX", "")
    base_branch = os.environ.get("RELEASE_BASE_BRANCH", "main")
    trusted_repository = os.environ.get("RELEASE_TRUSTED_REPOSITORY", "").strip()
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    api_url = os.environ.get("GITHUB_API_URL", "https://api.github.com")

    result = verify_release_tag(
        tag_prefix=tag_prefix,
        tag=tag,
        event_sha=event_sha,
        repository=repository,
        api_get=_api_client(token or "", api_url),
        base_branch=base_branch,
        trusted_repository=trusted_repository,
    )
    _write_outputs(result)
    print(
        f"Verified {result.tag} -> {result.tag_sha}; "
        f"{result.tag_sha} is reachable from {base_branch} at {result.main_sha}"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except VerificationError as error:
        print(f"::error::{error}", file=sys.stderr)
        raise SystemExit(1) from error
