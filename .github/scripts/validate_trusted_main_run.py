#!/usr/bin/env python3
"""Validate a manual or scheduled request before a secret-bearing benchmark run.

The consumer is started by ``workflow_run`` and is loaded from the default
branch.  This validator binds its source run and checkout to the current,
protected ``main`` commit before any benchmark secret is made available.
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
REQUEST_WORKFLOWS = {
    "model-tests": (
        "CI: Test Models (request)",
        ".github/workflows/ci-test-models-request.yml",
        frozenset({"workflow_dispatch", "schedule"}),
    ),
    "cold-start-benchmark": (
        "CI: Cold Start Benchmark (request)",
        ".github/workflows/ci-cold-start-benchmark-request.yml",
        frozenset({"workflow_dispatch"}),
    ),
}
SHA_RE = re.compile(r"[0-9a-f]{40}\Z")


class ValidationError(RuntimeError):
    """Raised when request provenance cannot be proven."""


class Api(Protocol):
    def get(self, path: str) -> Mapping[str, Any]:
        """Read one GitHub REST resource."""


class GitHubApi:
    """Small, bounded, read-only GitHub REST client."""

    def __init__(
        self,
        token: str,
        opener: Callable[..., Any] = urlopen,
        api_root: str = "https://api.github.com",
    ) -> None:
        if not token:
            raise ValidationError("GH_TOKEN is required")
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
                "User-Agent": "manaflow-cmux-cua-benchmark-validator",
            },
        )
        try:
            with self._opener(request, timeout=30) as response:
                body = response.read(8 * 1024 * 1024 + 1)
        except (HTTPError, URLError, OSError, TimeoutError) as error:
            raise ValidationError(f"GitHub API request failed for {path}: {error}") from error
        if len(body) > 8 * 1024 * 1024:
            raise ValidationError("GitHub API response is too large")
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


def repository_path(suffix: str) -> str:
    owner, name = EXPECTED_REPOSITORY.split("/", 1)
    return f"/repos/{quote(owner, safe='')}/{quote(name, safe='')}{suffix}"


def main_sha(api: Api) -> str:
    branch = api.get(repository_path("/branches/main"))
    if branch.get("protected") is not True:
        raise ValidationError("main branch is not protected")
    commit = branch.get("commit")
    if not isinstance(commit, Mapping):
        raise ValidationError("main branch has no commit object")
    return full_sha(commit.get("sha"), "main branch SHA")


def validate(api: Api, values: Mapping[str, str]) -> str:
    """Return the current protected-main SHA, or raise ``ValidationError``."""

    same(required(values, "REPOSITORY"), EXPECTED_REPOSITORY, "repository")
    same(required(values, "EVENT_NAME"), "workflow_run", "consumer event")
    same(required(values, "TRUSTED_REF"), "refs/heads/main", "consumer ref")
    same(required(values, "TRUSTED_REF_TYPE"), "branch", "consumer ref type")
    if required(values, "TRUSTED_REF_PROTECTED").lower() != "true":
        raise ValidationError("consumer ref is not protected")
    trusted_sha = full_sha(required(values, "TRUSTED_SHA"), "trusted consumer SHA")

    kind = required(values, "REQUEST_KIND")
    try:
        workflow_name, workflow_path, allowed_events = REQUEST_WORKFLOWS[kind]
    except KeyError as error:
        raise ValidationError(f"unknown request kind {kind!r}") from error

    run_id = positive_int(required(values, "SOURCE_RUN_ID"), "SOURCE_RUN_ID")
    run = api.get(repository_path(f"/actions/runs/{run_id}"))
    same(run.get("id"), run_id, "source run ID")
    same(run.get("name"), workflow_name, "source workflow name")
    same(run.get("path"), workflow_path, "source workflow path")
    same(required(values, "SOURCE_WORKFLOW_NAME"), workflow_name, "event source workflow name")
    same(required(values, "SOURCE_WORKFLOW_PATH"), workflow_path, "event source workflow path")
    source_event = required(values, "SOURCE_EVENT")
    if source_event not in allowed_events:
        raise ValidationError("source workflow event is not an allowed request")
    same(run.get("event"), source_event, "source workflow event")
    source_status = required(values, "SOURCE_STATUS")
    source_conclusion = required(values, "SOURCE_CONCLUSION")
    same(source_status, "completed", "event source workflow status")
    same(source_conclusion, "success", "event source workflow conclusion")
    same(run.get("status"), source_status, "source workflow status")
    same(run.get("conclusion"), source_conclusion, "source workflow conclusion")
    same(
        run.get("run_attempt"),
        positive_int(required(values, "SOURCE_RUN_ATTEMPT"), "SOURCE_RUN_ATTEMPT"),
        "source workflow attempt",
    )

    source_repository = run.get("repository")
    source_head_repository = run.get("head_repository")
    if not isinstance(source_repository, Mapping) or not isinstance(
        source_head_repository, Mapping
    ):
        raise ValidationError("source workflow has no repository metadata")
    same(source_repository.get("full_name"), EXPECTED_REPOSITORY, "source repository")
    same(source_head_repository.get("full_name"), EXPECTED_REPOSITORY, "source head repository")
    same(required(values, "SOURCE_REPOSITORY"), EXPECTED_REPOSITORY, "event source repository")
    same(
        required(values, "SOURCE_HEAD_REPOSITORY"),
        EXPECTED_REPOSITORY,
        "event source head repository",
    )
    same(run.get("head_branch"), required(values, "SOURCE_BRANCH"), "source branch")
    same(required(values, "SOURCE_BRANCH"), "main", "source branch")
    source_sha = full_sha(required(values, "SOURCE_SHA"), "source SHA")
    same(full_sha(run.get("head_sha"), "source workflow SHA"), source_sha, "source SHA")

    branch_sha = main_sha(api)
    if trusted_sha != branch_sha:
        raise ValidationError("trusted consumer SHA is not current main")
    same(source_sha, branch_sha, "source SHA")

    # Re-read the moving branch after every identity check.  A fast branch
    # update must fail closed instead of changing the code under test.
    if main_sha(api) != branch_sha:
        raise ValidationError("main branch moved while request was checked")
    return branch_sha


def event_values(environment: Mapping[str, str]) -> dict[str, str]:
    names = (
        "EVENT_NAME",
        "REPOSITORY",
        "TRUSTED_REF",
        "TRUSTED_REF_TYPE",
        "TRUSTED_REF_PROTECTED",
        "TRUSTED_SHA",
        "REQUEST_KIND",
        "SOURCE_RUN_ID",
        "SOURCE_RUN_ATTEMPT",
        "SOURCE_EVENT",
        "SOURCE_STATUS",
        "SOURCE_CONCLUSION",
        "SOURCE_WORKFLOW_NAME",
        "SOURCE_WORKFLOW_PATH",
        "SOURCE_REPOSITORY",
        "SOURCE_HEAD_REPOSITORY",
        "SOURCE_BRANCH",
        "SOURCE_SHA",
    )
    return {name: environment.get(name, "") for name in names}


def write_output(commit: str, output_path: str) -> None:
    if output_path:
        with Path(output_path).open("a", encoding="utf-8") as output:
            output.write(f"commit={commit}\n")


def main() -> int:
    try:
        environment = os.environ
        commit = validate(GitHubApi(environment.get("GH_TOKEN", "")), event_values(environment))
        write_output(commit, environment.get("GITHUB_OUTPUT", ""))
        print(f"Validated benchmark request against protected main {commit}")
        return 0
    except ValidationError as error:
        print(f"::error::Benchmark request validation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
