#!/usr/bin/env python3
"""Validate a protected-main workflow_run before a credentialed publish.

This validator covers non-tagged consumers, such as the docs image.  The
tag-specific release validator remains separate so a branch run cannot satisfy
a release-tag contract by accident.
"""

from __future__ import annotations

import os
import sys
from typing import Mapping

from validate_release_request import (
    Api,
    GitHubApi,
    ValidationError,
    _branch_sha,
    _require_ancestor,
    full_sha,
    positive_int,
    repository_path,
    required,
    same,
)


EXPECTED_REPOSITORY = "manaflow-ai/cmux-cua"
OBSERVER_WORKFLOW_NAME = "Docs MCP Server publish request"
OBSERVER_WORKFLOW_PATH = ".github/workflows/docs-mcp-server-publish-request.yml"


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
    same(run.get("head_branch"), "main", "source branch")
    source_sha = full_sha(required(values, "SOURCE_SHA"), "SOURCE_SHA")
    same(full_sha(run.get("head_sha"), "source workflow head SHA"), source_sha, "source head SHA")
    expected_sha = values.get("EXPECTED_SHA", "")
    if expected_sha:
        same(source_sha, full_sha(expected_sha, "expected source SHA"), "validated source SHA")

    main_sha = _branch_sha(api, repository)
    _require_ancestor(api, repository, source_sha, main_sha, "source commit")
    _require_ancestor(api, repository, trusted_sha, main_sha, "trusted consumer commit")
    if _branch_sha(api, repository) != main_sha:
        raise ValidationError("main branch moved while provenance was checked")

    return {
        "commit": source_sha,
        "main_commit": main_sha,
        "source_run_id": str(source_run_id),
    }


def _values(environment: Mapping[str, str]) -> dict[str, str]:
    names = (
        "EVENT_NAME",
        "REPOSITORY",
        "TRUSTED_REF",
        "TRUSTED_REF_TYPE",
        "TRUSTED_REF_NAME",
        "TRUSTED_REF_PROTECTED",
        "TRUSTED_SHA",
        "SOURCE_RUN_ID",
        "SOURCE_RUN_ATTEMPT",
        "SOURCE_EVENT",
        "SOURCE_STATUS",
        "SOURCE_CONCLUSION",
        "SOURCE_REPOSITORY",
        "SOURCE_HEAD_REPOSITORY",
        "SOURCE_SHA",
        "EXPECTED_SHA",
    )
    return {name: environment.get(name, "") for name in names}


def _write_outputs(result: Mapping[str, str], path: str) -> None:
    if not path:
        return
    with open(path, "a", encoding="utf-8") as output:
        for key in ("commit", "main_commit", "source_run_id"):
            output.write(f"{key}={result[key]}\n")


def main() -> int:
    try:
        environment = os.environ
        result = validate(GitHubApi(environment.get("GH_TOKEN", "")), _values(environment))
        _write_outputs(result, environment.get("GITHUB_OUTPUT", ""))
        print(f"Validated source commit {result['commit']}; main is {result['main_commit']}")
        return 0
    except ValidationError as error:
        print(f"::error::Main workflow provenance validation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
