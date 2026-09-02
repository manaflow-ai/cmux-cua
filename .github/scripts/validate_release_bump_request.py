#!/usr/bin/env python3
"""Validate a credential-free release-bump request from protected ``main``.

The dispatch-side workflow may be selected from any branch, so it never gets
repository write access or release secrets.  This validator runs in the
protected ``workflow_run`` consumer, re-reads the source run through the
GitHub API, and accepts only a small allowlisted request artifact from a
successful dispatch on the current ``main`` commit.
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
MAX_REQUEST_BYTES = 16 * 1024
SHA_RE = re.compile(r"[0-9a-fA-F]{40}\Z")

ALLOWED_SERVICES = frozenset(
    {
        "pypi/cua",
        "pypi/agent",
        "pypi/auto",
        "pypi/bench",
        "pypi/bench-ui",
        "pypi/cli",
        "pypi/computer",
        "pypi/computer-server",
        "pypi/cloud",
        "pypi/core",
        "pypi/mcp-server",
        "pypi/sandbox",
        "pypi/sandbox-apps",
        "pypi/som",
        "pypi/train",
        "npm/cli",
        "npm/computer",
        "npm/core",
        "npm/playground",
        "npm/cuabot",
        "lume",
        "cua-driver",
        "cua-driver-rs",
        "docker/cuabot",
        "docker/kasm",
        "docker/xfce",
        "docker/lumier",
        "docker/qemu-android",
        "docker/qemu-linux",
        "docker/qemu-windows",
    }
)
BUMP_TYPES = frozenset({"patch", "minor", "major"})


class ValidationError(RuntimeError):
    """Raised when the request provenance or content is not safe."""


class Api(Protocol):
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
            raise ValidationError("GH_TOKEN is required for request validation")
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
                "User-Agent": "manaflow-cmux-cua-release-bump-validator",
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


def repository_name(value: Any, name: str) -> str:
    if not isinstance(value, Mapping):
        raise ValidationError(f"{name} has no repository object")
    full_name = value.get("full_name")
    if not isinstance(full_name, str) or not full_name:
        raise ValidationError(f"{name} has no repository name")
    return full_name


def _validate_source_run(
    api: Api, values: Mapping[str, str], repository: str
) -> tuple[int, str]:
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
    same(run.get("event"), "workflow_dispatch", "source workflow event")
    same(run.get("status"), "completed", "source workflow status")
    same(run.get("conclusion"), "success", "source workflow conclusion")
    same(values.get("SOURCE_EVENT"), run.get("event"), "event source workflow event")
    same(values.get("SOURCE_STATUS"), run.get("status"), "event source workflow status")
    same(values.get("SOURCE_CONCLUSION"), run.get("conclusion"), "event source workflow conclusion")
    if values.get("SOURCE_RUN_ATTEMPT"):
        same(
            positive_int(values["SOURCE_RUN_ATTEMPT"], "SOURCE_RUN_ATTEMPT"),
            run.get("run_attempt"),
            "source workflow run attempt",
        )

    expected_repository = required(values, "EXPECTED_REPOSITORY")
    same(repository_name(run.get("repository"), "source workflow"), expected_repository, "source repository")
    same(
        repository_name(run.get("head_repository"), "source workflow head"),
        expected_repository,
        "source head repository",
    )
    same(values.get("SOURCE_REPOSITORY"), expected_repository, "event source repository")
    same(values.get("SOURCE_HEAD_REPOSITORY"), expected_repository, "event source head repository")
    same(run.get("head_branch"), "main", "source branch")
    same(values.get("SOURCE_BRANCH"), "main", "event source branch")
    source_sha = full_sha(required(values, "SOURCE_SHA"), "SOURCE_SHA")
    same(full_sha(run.get("head_sha"), "source workflow head SHA"), source_sha, "source head SHA")
    return run_id, source_sha


def _validate_artifact(api: Api, repository: str, run_id: int, source_sha: str) -> None:
    response = api.get(repository_path(repository, f"/actions/runs/{run_id}/artifacts?per_page=100"))
    artifacts = response.get("artifacts")
    if not isinstance(artifacts, list):
        raise ValidationError("source request artifact response has no list")
    candidates = [
        artifact
        for artifact in artifacts
        if isinstance(artifact, Mapping) and artifact.get("name") == "release-bump-request"
    ]
    if len(candidates) != 1:
        raise ValidationError("source run must contain exactly one release-bump-request artifact")
    artifact = candidates[0]
    positive_int(artifact.get("id"), "request artifact ID")
    same(artifact.get("expired"), False, "request artifact expired flag")
    size = positive_int(artifact.get("size_in_bytes"), "request artifact size")
    if size > MAX_REQUEST_BYTES:
        raise ValidationError(f"request artifact exceeds {MAX_REQUEST_BYTES} bytes")
    artifact_run = artifact.get("workflow_run")
    if not isinstance(artifact_run, Mapping):
        raise ValidationError("request artifact has no workflow run")
    same(artifact_run.get("id"), run_id, "request artifact run ID")
    same(artifact_run.get("head_sha"), source_sha, "request artifact head SHA")


def _read_request(path: Path) -> dict[str, str]:
    if path.is_symlink() or not path.is_file():
        raise ValidationError("request artifact path is not a regular file")
    if path.stat().st_size > MAX_REQUEST_BYTES:
        raise ValidationError(f"request file exceeds {MAX_REQUEST_BYTES} bytes")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ValidationError("request artifact is not valid UTF-8 JSON") from error
    if not isinstance(data, dict) or set(data) != {"service", "bump_type"}:
        raise ValidationError("request artifact must contain only service and bump_type")
    service = data.get("service")
    bump_type = data.get("bump_type")
    if not isinstance(service, str) or service not in ALLOWED_SERVICES:
        raise ValidationError("request service is not allowlisted")
    if not isinstance(bump_type, str) or bump_type not in BUMP_TYPES:
        raise ValidationError("request bump_type is not allowlisted")
    return {"service": service, "bump_type": bump_type}


def validate(api: Api, values: Mapping[str, str], request_path: Path) -> dict[str, str]:
    """Return validated request values or raise :class:`ValidationError`."""

    repository = required(values, "REPOSITORY")
    expected_repository = required(values, "EXPECTED_REPOSITORY")
    if repository != expected_repository:
        raise ValidationError(
            f"workflow repository {repository!r} is not {expected_repository!r}"
        )
    if values.get("EVENT_NAME") != "workflow_run":
        raise ValidationError("release-bump consumer must run from workflow_run")
    if values.get("TRUSTED_REF_PROTECTED", "").lower() != "true":
        raise ValidationError("consumer ref is not covered by a protected branch rule")
    trusted_sha = full_sha(required(values, "TRUSTED_SHA"), "TRUSTED_SHA")
    run_id, source_sha = _validate_source_run(api, values, repository)
    _validate_artifact(api, repository, run_id, source_sha)

    main_ref = api.get(repository_path(repository, "/git/ref/heads/main"))
    main_object = main_ref.get("object")
    if not isinstance(main_object, Mapping):
        raise ValidationError("main ref has no object")
    same(main_object.get("type"), "commit", "main object type")
    main_sha = full_sha(main_object.get("sha"), "main commit SHA")
    same(source_sha, main_sha, "request source and current main commit")
    same(trusted_sha, main_sha, "trusted consumer and current main commit")

    request = _read_request(request_path)
    return {
        **request,
        "commit": main_sha,
        "source_run_id": str(run_id),
    }


def write_outputs(values: Mapping[str, str], output_path: str) -> None:
    if not output_path:
        return
    with Path(output_path).open("a", encoding="utf-8") as output:
        for key in ("service", "bump_type", "commit", "source_run_id"):
            output.write(f"{key}={values[key]}\n")


def main() -> int:
    try:
        environment = os.environ
        result = validate(
            GitHubApi(environment.get("GH_TOKEN", "")),
            environment,
            Path(required(environment, "REQUEST_FILE")),
        )
        write_outputs(result, environment.get("GITHUB_OUTPUT", ""))
        print(
            f"Validated {result['service']} {result['bump_type']} "
            f"request against main {result['commit']}"
        )
        return 0
    except ValidationError as error:
        print(f"::error::Release-bump validation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
