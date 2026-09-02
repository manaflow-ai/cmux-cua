#!/usr/bin/env python3
"""Validate a tag release request before a protected consumer gets secrets.

The tag-triggered request workflow is intentionally unprivileged.  This
validator runs from the protected default branch in a ``workflow_run``
consumer and binds the request to a successful run of the expected workflow,
the exact tag object, and a commit reachable from the current ``main`` ref.
All event values are treated as untrusted input.  The GitHub API is queried
again so a stale or forged event value cannot select a different run or ref.
"""

from __future__ import annotations

import json
import os
import re
import sys
from pathlib import Path
from typing import Any, Callable, Mapping, Protocol
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen


MAX_RESPONSE_BYTES = 8 * 1024 * 1024
SHA_RE = re.compile(r"[0-9a-fA-F]{40}\Z")
PREFIX_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._/-]*\Z")
VERSION_RE = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\Z"
)


class ValidationError(RuntimeError):
    """Raised when release provenance cannot be proven."""


class Api(Protocol):
    """Small API surface that keeps validation deterministic in tests."""

    def get(self, path: str) -> Mapping[str, Any]:
        ...


class GitHubApi:
    """Bounded, read-only GitHub REST client."""

    def __init__(
        self,
        token: str,
        opener: Callable[..., Any] = urlopen,
        api_root: str = "https://api.github.com",
    ) -> None:
        if not token:
            raise ValidationError("GH_TOKEN is required for release validation")
        self._token = token
        self._opener = opener
        self._api_root = api_root.rstrip("/")

    def get(self, path: str) -> Mapping[str, Any]:
        request = Request(
            f"{self._api_root}{path}",
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {self._token}",
                "X-GitHub-Api-Version": "2022-11-28",
                "User-Agent": "manaflow-cmux-cua-release-validator",
            },
        )
        try:
            with self._opener(request, timeout=30) as response:
                body = response.read(MAX_RESPONSE_BYTES + 1)
        except (HTTPError, URLError, OSError, TimeoutError) as error:
            raise ValidationError(f"GitHub API request failed for {path}: {error}") from error
        if len(body) > MAX_RESPONSE_BYTES:
            raise ValidationError(f"GitHub API response exceeded {MAX_RESPONSE_BYTES} bytes")
        try:
            value = json.loads(body)
        except (TypeError, ValueError) as error:
            raise ValidationError(f"GitHub API returned invalid JSON for {path}") from error
        if not isinstance(value, Mapping):
            raise ValidationError(f"GitHub API returned a non-object for {path}")
        return value


def required(values: Mapping[str, str], name: str) -> str:
    value = values.get(name, "")
    if not value:
        raise ValidationError(f"{name} is required")
    return value


def positive_int(value: Any, name: str) -> int:
    try:
        parsed = int(value)
    except (TypeError, ValueError) as error:
        raise ValidationError(f"{name} must be a positive integer") from error
    if parsed <= 0:
        raise ValidationError(f"{name} must be a positive integer")
    return parsed


def full_sha(value: Any, name: str) -> str:
    if not isinstance(value, str) or not SHA_RE.fullmatch(value):
        raise ValidationError(f"{name} must be a full 40-character hexadecimal commit")
    return value.lower()


def same(actual: Any, expected: Any, name: str) -> None:
    if actual != expected:
        raise ValidationError(f"{name} mismatch: expected {expected!r}, got {actual!r}")


def repository_path(repository: str, suffix: str) -> str:
    owner, name = repository.split("/", 1)
    return f"/repos/{quote(owner, safe='')}/{quote(name, safe='')}{suffix}"


def _full_name(value: Any, name: str) -> str:
    if not isinstance(value, Mapping):
        raise ValidationError(f"{name} has no repository object")
    result = value.get("full_name")
    if not isinstance(result, str) or not result:
        raise ValidationError(f"{name} has no repository name")
    return result


def _tag_commit(api: Api, repository: str, tag: str) -> str:
    encoded_tag = quote(tag, safe="")
    reference = api.get(repository_path(repository, f"/git/ref/tags/{encoded_tag}"))
    tag_object = reference.get("object")
    if not isinstance(tag_object, Mapping):
        raise ValidationError("tag ref has no object")
    object_type = tag_object.get("type")
    object_sha = full_sha(tag_object.get("sha"), "tag object SHA")
    if object_type == "commit":
        return object_sha
    if object_type != "tag":
        raise ValidationError(f"tag ref resolves to unsupported object type {object_type!r}")

    annotated = api.get(repository_path(repository, f"/git/tags/{object_sha}"))
    annotated_object = annotated.get("object")
    if not isinstance(annotated_object, Mapping):
        raise ValidationError("annotated tag has no object")
    same(annotated_object.get("type"), "commit", "annotated tag object type")
    return full_sha(annotated_object.get("sha"), "annotated tag commit SHA")


def _ref_commit(api: Api, repository: str, ref: str) -> str:
    reference = api.get(repository_path(repository, f"/git/ref/{ref}"))
    ref_object = reference.get("object")
    if not isinstance(ref_object, Mapping):
        raise ValidationError(f"{ref} ref has no object")
    same(ref_object.get("type"), "commit", f"{ref} object type")
    return full_sha(ref_object.get("sha"), f"{ref} commit SHA")


def _is_ancestor(api: Api, repository: str, base: str, candidate: str, name: str) -> None:
    comparison = api.get(repository_path(repository, f"/compare/{base}...{candidate}"))
    status = comparison.get("status")
    ahead_by = comparison.get("ahead_by")
    if (
        status not in {"behind", "identical"}
        or isinstance(ahead_by, bool)
        or not isinstance(ahead_by, int)
        or ahead_by != 0
    ):
        raise ValidationError(
            f"{name} {candidate} is not an ancestor of main {base} "
            f"(status={status!r}, ahead_by={ahead_by!r})"
        )


def _validate_source_run(
    api: Api, values: Mapping[str, str], repository: str
) -> tuple[int, str, str]:
    run_id = positive_int(required(values, "SOURCE_RUN_ID"), "SOURCE_RUN_ID")
    run = api.get(repository_path(repository, f"/actions/runs/{run_id}"))
    same(run.get("id"), run_id, "source workflow run ID")
    same(run.get("name"), required(values, "SOURCE_WORKFLOW_NAME"), "source workflow name")
    same(run.get("path"), required(values, "SOURCE_WORKFLOW_PATH"), "source workflow path")
    if values.get("SOURCE_WORKFLOW_ID"):
        same(
            positive_int(values["SOURCE_WORKFLOW_ID"], "SOURCE_WORKFLOW_ID"),
            run.get("workflow_id"),
            "source workflow ID",
        )
    same(run.get("event"), "push", "source workflow event")
    same(run.get("status"), "completed", "source workflow status")
    same(run.get("conclusion"), "success", "source workflow conclusion")
    same(values.get("SOURCE_EVENT"), run.get("event"), "event source workflow event")
    same(values.get("SOURCE_STATUS"), run.get("status"), "event source workflow status")
    same(values.get("SOURCE_CONCLUSION"), run.get("conclusion"), "event source workflow conclusion")
    if "SOURCE_RUN_ATTEMPT" in values and values["SOURCE_RUN_ATTEMPT"]:
        same(
            positive_int(values["SOURCE_RUN_ATTEMPT"], "SOURCE_RUN_ATTEMPT"),
            run.get("run_attempt"),
            "source workflow run attempt",
        )

    expected_repository = required(values, "EXPECTED_REPOSITORY")
    same(_full_name(run.get("repository"), "source workflow"), expected_repository, "source repository")
    same(
        _full_name(run.get("head_repository"), "source workflow head"),
        expected_repository,
        "source head repository",
    )
    same(values.get("SOURCE_REPOSITORY"), expected_repository, "event source repository")
    same(values.get("SOURCE_HEAD_REPOSITORY"), expected_repository, "event source head repository")

    tag = required(values, "SOURCE_BRANCH")
    same(run.get("head_branch"), tag, "source tag")
    source_sha = full_sha(required(values, "SOURCE_SHA"), "SOURCE_SHA")
    same(full_sha(run.get("head_sha"), "source workflow head SHA"), source_sha, "source head SHA")
    return run_id, tag, source_sha


def validate(api: Api, values: Mapping[str, str]) -> dict[str, str]:
    """Return validated release values or raise :class:`ValidationError`."""

    repository = required(values, "REPOSITORY")
    expected_repository = required(values, "EXPECTED_REPOSITORY")
    if repository != expected_repository:
        raise ValidationError(
            f"workflow repository {repository!r} is not {expected_repository!r}"
        )
    if values.get("EVENT_NAME") != "workflow_run":
        raise ValidationError("release consumer must run from workflow_run")
    if values.get("TRUSTED_REF_PROTECTED", "").lower() != "true":
        raise ValidationError("consumer ref is not covered by a protected branch rule")
    trusted_sha = full_sha(required(values, "TRUSTED_SHA"), "TRUSTED_SHA")
    prefix = required(values, "TAG_PREFIX")
    if not PREFIX_RE.fullmatch(prefix):
        raise ValidationError("TAG_PREFIX contains unsupported characters")

    run_id, tag, source_sha = _validate_source_run(api, values, repository)
    if not tag.startswith(prefix):
        raise ValidationError(f"tag {tag!r} does not use the required {prefix!r} prefix")
    version = tag.removeprefix(prefix)
    if not VERSION_RE.fullmatch(version):
        raise ValidationError(f"tag {tag!r} does not contain a valid release version")

    tag_sha = _tag_commit(api, repository, tag)
    same(tag_sha, source_sha, "tag commit")
    main_sha = _ref_commit(api, repository, "heads/main")
    _is_ancestor(api, repository, main_sha, trusted_sha, "trusted consumer commit")
    _is_ancestor(api, repository, main_sha, source_sha, "release tag commit")

    return {
        "tag": tag,
        "version": version,
        "commit": source_sha,
        "main_commit": main_sha,
        "source_run_id": str(run_id),
    }


def write_outputs(values: Mapping[str, str], output_path: str) -> None:
    if not output_path:
        return
    with Path(output_path).open("a", encoding="utf-8") as output:
        for key in ("tag", "version", "commit", "main_commit", "source_run_id"):
            output.write(f"{key}={values[key]}\n")


def main() -> int:
    try:
        environment = os.environ
        result = validate(GitHubApi(environment.get("GH_TOKEN", "")), environment)
        write_outputs(result, environment.get("GITHUB_OUTPUT", ""))
        print(f"Validated {result['tag']} at {result['commit']}; main is {result['main_commit']}")
        return 0
    except ValidationError as error:
        print(f"::error::Release provenance validation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
