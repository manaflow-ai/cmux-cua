#!/usr/bin/env python3
"""Validate an unprivileged release-tag request before a privileged publish.

Tag pushes run a tiny, credential-free observer.  The privileged consumer is
loaded from protected ``main`` by ``workflow_run`` and calls this module before
it checks out source or exposes a registry credential.  Every value from the
event is treated as untrusted and is bound to a fresh GitHub API read.
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


EXPECTED_REPOSITORY = "manaflow-ai/cmux-cua"
OBSERVER_WORKFLOW_NAME = "Release tag request"
OBSERVER_WORKFLOW_PATH = ".github/workflows/release-tag-request.yml"
SHA_RE = re.compile(r"[0-9a-f]{40}\Z")
REPOSITORY_RE = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
TAG_PREFIX_RE = re.compile(r"[A-Za-z0-9._/-]+-v\Z")
VERSION_RE = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z"
)
MAX_RESPONSE_BYTES = 8 * 1024 * 1024


class ValidationError(RuntimeError):
    """Raised when release provenance cannot be proven."""


class Api(Protocol):
    """Minimal API surface used by :func:`validate` and test fakes."""

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
                "User-Agent": "manaflow-cmux-cua-release-request-validator",
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


def full_sha(value: Any, name: str) -> str:
    if not isinstance(value, str) or not SHA_RE.fullmatch(value):
        raise ValidationError(f"{name} must be a lowercase 40-character commit SHA")
    return value


def positive_int(value: Any, name: str) -> int:
    if isinstance(value, bool):
        raise ValidationError(f"{name} must be a positive integer")
    try:
        result = int(value)
    except (TypeError, ValueError) as error:
        raise ValidationError(f"{name} must be a positive integer") from error
    if result <= 0:
        raise ValidationError(f"{name} must be a positive integer")
    return result


def same(actual: Any, expected: Any, name: str) -> None:
    if actual != expected:
        raise ValidationError(f"{name} mismatch: expected {expected!r}, got {actual!r}")


def repository_path(repository: str, suffix: str) -> str:
    if not REPOSITORY_RE.fullmatch(repository):
        raise ValidationError("repository name is invalid")
    owner, name = repository.split("/", 1)
    return f"/repos/{quote(owner, safe='')}/{quote(name, safe='')}{suffix}"


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


def _branch_sha(api: Api, repository: str) -> str:
    branch = api.get(repository_path(repository, "/branches/main"))
    if branch.get("protected") is not True:
        raise ValidationError("main branch is not protected")
    commit = branch.get("commit")
    if not isinstance(commit, Mapping):
        raise ValidationError("main branch has no commit object")
    return full_sha(commit.get("sha"), "main branch SHA")


def _require_ancestor(api: Api, repository: str, ancestor: str, descendant: str, name: str) -> None:
    comparison = api.get(
        repository_path(repository, f"/compare/{ancestor}...{descendant}")
    )
    if comparison.get("status") not in {"ahead", "identical"}:
        raise ValidationError(
            f"{name} {ancestor} is not an ancestor of {descendant} "
            f"(status={comparison.get('status')!r})"
        )
    base = comparison.get("base_commit")
    head = comparison.get("head_commit")
    merge_base = comparison.get("merge_base_commit")
    if not isinstance(base, Mapping) or full_sha(base.get("sha"), "comparison base SHA") != ancestor:
        raise ValidationError("ancestry comparison base does not match the requested ancestor")
    if not isinstance(head, Mapping) or full_sha(head.get("sha"), "comparison head SHA") != descendant:
        raise ValidationError("ancestry comparison head does not match the requested descendant")
    if not isinstance(merge_base, Mapping) or full_sha(
        merge_base.get("sha"), "comparison merge-base SHA"
    ) != ancestor:
        raise ValidationError("ancestry comparison merge base does not equal the ancestor")


def _validate_source_run(api: Api, values: Mapping[str, str], repository: str) -> tuple[int, str, str]:
    source_run_id = positive_int(required(values, "SOURCE_RUN_ID"), "SOURCE_RUN_ID")
    run = api.get(repository_path(repository, f"/actions/runs/{source_run_id}"))
    same(run.get("id"), source_run_id, "source workflow run ID")
    same(run.get("name"), OBSERVER_WORKFLOW_NAME, "source workflow name")
    same(run.get("path"), OBSERVER_WORKFLOW_PATH, "source workflow path")
    same(run.get("event"), "push", "source workflow event")
    same(run.get("status"), "completed", "source workflow status")
    same(run.get("conclusion"), "success", "source workflow conclusion")

    expected_attempt = positive_int(required(values, "SOURCE_RUN_ATTEMPT"), "SOURCE_RUN_ATTEMPT")
    same(run.get("run_attempt"), expected_attempt, "source workflow run attempt")
    source_repository = run.get("repository")
    source_head_repository = run.get("head_repository")
    if not isinstance(source_repository, Mapping) or not isinstance(source_head_repository, Mapping):
        raise ValidationError("source workflow run has no repository metadata")
    same(source_repository.get("full_name"), repository, "source workflow repository")
    same(source_head_repository.get("full_name"), repository, "source workflow head repository")
    same(values.get("SOURCE_REPOSITORY"), repository, "event source repository")
    same(values.get("SOURCE_HEAD_REPOSITORY"), repository, "event source head repository")
    same(values.get("SOURCE_EVENT"), "push", "event source workflow event")
    same(values.get("SOURCE_STATUS"), "completed", "event source workflow status")
    same(values.get("SOURCE_CONCLUSION"), "success", "event source workflow conclusion")

    tag = required(values, "SOURCE_BRANCH")
    same(run.get("head_branch"), tag, "source tag")
    source_sha = full_sha(required(values, "SOURCE_SHA"), "SOURCE_SHA")
    same(full_sha(run.get("head_sha"), "source workflow head SHA"), source_sha, "source head SHA")
    return source_run_id, tag, source_sha


def validate(api: Api, values: Mapping[str, str]) -> dict[str, str]:
    """Return validated source values or raise :class:`ValidationError`."""

    repository = required(values, "REPOSITORY")
    same(repository, EXPECTED_REPOSITORY, "publisher repository")
    same(values.get("EVENT_NAME"), "workflow_run", "consumer event")
    same(values.get("TRUSTED_REF"), "refs/heads/main", "consumer ref")
    same(values.get("TRUSTED_REF_TYPE"), "branch", "consumer ref type")
    same(values.get("TRUSTED_REF_NAME"), "main", "consumer ref name")
    if values.get("TRUSTED_REF_PROTECTED", "").lower() != "true":
        raise ValidationError("consumer ref is not protected")
    trusted_sha = full_sha(required(values, "TRUSTED_SHA"), "TRUSTED_SHA")

    prefix = required(values, "TAG_PREFIX")
    if not TAG_PREFIX_RE.fullmatch(prefix):
        raise ValidationError("tag prefix is invalid")
    source_run_id, tag, source_sha = _validate_source_run(api, values, repository)
    expected_tag = values.get("EXPECTED_TAG", "")
    if expected_tag:
        same(tag, expected_tag, "validated tag")
    expected_sha = values.get("EXPECTED_SHA", "")
    if expected_sha:
        same(source_sha, full_sha(expected_sha, "expected source SHA"), "validated source SHA")
    if not tag.startswith(prefix):
        raise ValidationError(f"tag {tag!r} does not use the required {prefix!r} prefix")
    version = tag.removeprefix(prefix)
    if not VERSION_RE.fullmatch(version):
        raise ValidationError(f"tag {tag!r} does not contain an exact SemVer version")

    tag_sha = _tag_commit(api, repository, tag)
    same(tag_sha, source_sha, "tag commit")
    main_sha = _branch_sha(api, repository)
    _require_ancestor(api, repository, source_sha, main_sha, "release tag commit")
    _require_ancestor(api, repository, trusted_sha, main_sha, "trusted consumer commit")

    # Re-read moving refs after the ancestry checks.  A force-moved tag or
    # branch must never pass with the identity observed at the start.
    if _branch_sha(api, repository) != main_sha:
        raise ValidationError("main branch moved while provenance was checked")
    if _tag_commit(api, repository, tag) != source_sha:
        raise ValidationError("release tag moved while provenance was checked")

    return {
        "tag": tag,
        "version": version,
        "commit": source_sha,
        "main_commit": main_sha,
        "source_run_id": str(source_run_id),
    }


def write_outputs(values: Mapping[str, str], output_path: str) -> None:
    if not output_path:
        return
    with Path(output_path).open("a", encoding="utf-8") as output:
        for key in ("tag", "version", "commit", "main_commit", "source_run_id"):
            output.write(f"{key}={values[key]}\n")


def _event_values(environment: Mapping[str, str]) -> dict[str, str]:
    names = {
        "EVENT_NAME": "EVENT_NAME",
        "REPOSITORY": "REPOSITORY",
        "TRUSTED_REF": "TRUSTED_REF",
        "TRUSTED_REF_TYPE": "TRUSTED_REF_TYPE",
        "TRUSTED_REF_NAME": "TRUSTED_REF_NAME",
        "TRUSTED_REF_PROTECTED": "TRUSTED_REF_PROTECTED",
        "TRUSTED_SHA": "TRUSTED_SHA",
        "TAG_PREFIX": "TAG_PREFIX",
        "SOURCE_RUN_ID": "SOURCE_RUN_ID",
        "SOURCE_RUN_ATTEMPT": "SOURCE_RUN_ATTEMPT",
        "SOURCE_EVENT": "SOURCE_EVENT",
        "SOURCE_STATUS": "SOURCE_STATUS",
        "SOURCE_CONCLUSION": "SOURCE_CONCLUSION",
        "SOURCE_REPOSITORY": "SOURCE_REPOSITORY",
        "SOURCE_HEAD_REPOSITORY": "SOURCE_HEAD_REPOSITORY",
        "SOURCE_BRANCH": "SOURCE_BRANCH",
        "SOURCE_SHA": "SOURCE_SHA",
        "EXPECTED_TAG": "EXPECTED_TAG",
        "EXPECTED_SHA": "EXPECTED_SHA",
    }
    return {key: environment.get(name, "") for key, name in names.items()}


def main() -> int:
    try:
        environment = os.environ
        values = _event_values(environment)
        result = validate(GitHubApi(environment.get("GH_TOKEN", "")), values)
        write_outputs(result, environment.get("GITHUB_OUTPUT", ""))
        print(f"Validated {result['tag']} at {result['commit']}; main is {result['main_commit']}")
        return 0
    except ValidationError as error:
        print(f"::error::Release provenance validation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
