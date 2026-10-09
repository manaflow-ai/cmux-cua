#!/usr/bin/env python3
"""Validate the bound inputs to the cua-driver PyPI publisher.

The publisher is triggered by ``workflow_run``.  The event payload is useful
for routing, but it is not an artifact manifest and it does not prove that the
tag or release still points at the completed run.  This module resolves all of
those relationships through the GitHub API before any build job receives an
artifact or an OIDC publishing capability.

The module intentionally uses only the Python standard library.  It is also
used by the workflow wiring tests with a small in-memory API implementation.
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


SOURCE_REPOSITORY = "manaflow-ai/cmux-cua"
SOURCE_WORKFLOW_ID = 311952875
SOURCE_WORKFLOW_NAME = "CD: Cua Driver (cross-platform)"
SOURCE_WORKFLOW_PATH = ".github/workflows/cd-rust-cua-driver.yml"
TAG_PREFIX = "cua-driver-rs-v"

# Keep all API responses bounded.  A normal run/release/artifact response is
# far below this limit, while a pathological response cannot exhaust a runner.
MAX_RESPONSE_BYTES = 8 * 1024 * 1024
MAX_ARTIFACT_BYTES = 512 * 1024 * 1024

SHA_RE = re.compile(r"^[0-9a-f]{40}$")
# The release workflow accepts SemVer-like prereleases and build metadata.  A
# tag is later looked up by its exact ref, so this expression also prevents
# path traversal and shell metacharacters from entering API URLs or outputs.
VERSION_RE = re.compile(
    r"^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)
PEP440_LABELS = {
    "a": "a",
    "alpha": "a",
    "b": "b",
    "beta": "b",
    "c": "rc",
    "pre": "rc",
    "preview": "rc",
    "rc": "rc",
    "dev": ".dev",
    "post": ".post",
    "rev": ".post",
    "r": ".post",
}

PLATFORM_ARTIFACTS: dict[str, str] = {
    "darwin-universal": "cua-driver-rs-darwin",
    "linux-x86_64": "cua-driver-rs-linux-x86_64",
    "linux-arm64": "cua-driver-rs-linux-arm64",
    "windows-x86_64": "cua-driver-rs-windows-x86_64",
    "windows-arm64": "cua-driver-rs-windows-arm64",
}


class ReleaseValidationError(RuntimeError):
    """Raised when a release relationship is not proven."""


class Api(Protocol):
    """Minimal API surface used by :func:`validate` and easy to fake in tests."""

    def get(self, path: str) -> Mapping[str, Any]:
        ...


class GitHubApi:
    """Small bounded GitHub REST client for read-only validation."""

    def __init__(
        self,
        token: str,
        opener: Callable[..., Any] = urlopen,
        api_root: str = "https://api.github.com",
    ) -> None:
        if not token:
            raise ReleaseValidationError("GH_TOKEN is required for provenance validation")
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
                "User-Agent": "cmux-cua-release-provenance",
            },
        )
        try:
            with self._opener(request, timeout=30) as response:
                body = response.read(MAX_RESPONSE_BYTES + 1)
        except (HTTPError, URLError, OSError, TimeoutError) as exc:
            raise ReleaseValidationError(f"GitHub API request failed for {path}: {exc}") from exc
        if len(body) > MAX_RESPONSE_BYTES:
            raise ReleaseValidationError(f"GitHub API response exceeded {MAX_RESPONSE_BYTES} bytes")
        try:
            value = json.loads(body)
        except (TypeError, ValueError) as exc:
            raise ReleaseValidationError(f"GitHub API returned invalid JSON for {path}") from exc
        if not isinstance(value, Mapping):
            raise ReleaseValidationError(f"GitHub API returned a non-object for {path}")
        return value


def _text(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise ReleaseValidationError(f"{label} is missing or not a string")
    return value


def _int(value: Any, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ReleaseValidationError(f"{label} is missing or not a positive integer")
    return value


def _same(actual: Any, expected: Any, label: str) -> None:
    if actual != expected:
        raise ReleaseValidationError(f"{label} mismatch: expected {expected!r}, got {actual!r}")


def _sha(value: Any, label: str) -> str:
    result = _text(value, label)
    if not SHA_RE.fullmatch(result):
        raise ReleaseValidationError(f"{label} is not a lowercase 40-character SHA")
    return result


def _version_from_tag(tag: str) -> str:
    if not tag.startswith(TAG_PREFIX):
        raise ReleaseValidationError(f"tag does not start with {TAG_PREFIX!r}")
    version = tag[len(TAG_PREFIX) :]
    if not VERSION_RE.fullmatch(version):
        raise ReleaseValidationError(f"tag has an invalid release version: {tag!r}")
    _pep440_version(version)
    return version


def _pep440_version(version: str) -> str:
    """Return the PEP 440 spelling emitted by Hatchling for a release tag."""

    if not VERSION_RE.fullmatch(version):
        raise ReleaseValidationError(f"invalid release version: {version!r}")
    base, _, local = version.partition("+")
    core, _, suffix = base.partition("-")
    if local:
        local_parts = local.replace("-", ".").replace("_", ".").split(".")
        normalized_local: list[str] = []
        for part in local_parts:
            if not part or not part.isalnum():
                raise ReleaseValidationError(f"invalid local version segment: {part!r}")
            normalized_local.append(str(int(part)) if part.isdigit() else part.lower())
        local = "+" + ".".join(normalized_local)
    if not suffix:
        return core + local

    parts = suffix.replace("-", ".").split(".")
    if len(parts) == 1 and parts[0].isdigit():
        return f"{core}.post{int(parts[0])}{local}"
    label_match = re.fullmatch(r"([A-Za-z]+)([0-9]*)", parts[0])
    if label_match is None or len(parts) > 2 or (len(parts) == 2 and not parts[1].isdigit()):
        raise ReleaseValidationError(f"release version is not PEP 440 compatible: {version!r}")
    label = label_match.group(1).lower()
    number_text = parts[1] if len(parts) == 2 else label_match.group(2) or "0"
    normalized_label = PEP440_LABELS.get(label)
    if normalized_label is None:
        raise ReleaseValidationError(f"release version has an unknown suffix: {version!r}")
    return f"{core}{normalized_label}{int(number_text)}{local}"


def _api_path(repository: str, suffix: str) -> str:
    # Repository is a fixed allowlisted value, but quote each component to keep
    # this helper safe if the allowlist changes later.
    owner, name = repository.split("/", 1)
    return f"/repos/{quote(owner, safe='')}/{quote(name, safe='')}{suffix}"


def _tag_commit(api: Api, repository: str, tag: str) -> str:
    encoded_tag = quote(tag, safe="")
    ref = api.get(_api_path(repository, f"/git/ref/tags/{encoded_tag}"))
    obj = ref.get("object")
    if not isinstance(obj, Mapping):
        raise ReleaseValidationError("tag ref has no object")
    object_type = _text(obj.get("type"), "tag object type")
    object_sha = _sha(obj.get("sha"), "tag object SHA")
    if object_type == "commit":
        return object_sha
    if object_type != "tag":
        raise ReleaseValidationError(f"tag ref resolves to unsupported object type {object_type!r}")

    # Annotated tags add one indirection.  Do not follow arbitrary chains.
    annotated = api.get(_api_path(repository, f"/git/tags/{object_sha}"))
    annotated_obj = annotated.get("object")
    if not isinstance(annotated_obj, Mapping):
        raise ReleaseValidationError("annotated tag has no object")
    _same(annotated_obj.get("type"), "commit", "annotated tag object type")
    return _sha(annotated_obj.get("sha"), "annotated tag commit SHA")


def _run_identity(run: Mapping[str, Any], label: str = "source run") -> None:
    _same(run.get("workflow_id"), SOURCE_WORKFLOW_ID, f"{label} workflow ID")
    _same(run.get("name"), SOURCE_WORKFLOW_NAME, f"{label} workflow name")
    _same(run.get("path"), SOURCE_WORKFLOW_PATH, f"{label} workflow path")
    _same(run.get("event"), "push", f"{label} event")
    _same(run.get("status"), "completed", f"{label} status")
    _same(run.get("conclusion"), "success", f"{label} conclusion")


def _run_repository(run: Mapping[str, Any], key: str, label: str) -> None:
    repository = run.get(key)
    if not isinstance(repository, Mapping):
        raise ReleaseValidationError(f"{label} has no repository object")
    _same(repository.get("full_name"), SOURCE_REPOSITORY, f"{label} repository")


def _validate_run(
    run: Mapping[str, Any],
    repository: str,
    payload: Mapping[str, str] | None = None,
) -> tuple[int, int, str, str]:
    _run_identity(run)
    _run_repository(run, "repository", "source run")
    _run_repository(run, "head_repository", "source run head")
    run_id = _int(run.get("id"), "source run ID")
    _int(run.get("run_number"), "source run number")
    run_attempt = _int(run.get("run_attempt"), "source run attempt")
    head_sha = _sha(run.get("head_sha"), "source run head SHA")
    head_branch = _text(run.get("head_branch"), "source run head branch")
    version = _version_from_tag(head_branch)
    if payload is not None:
        _same(payload.get("run_id"), str(run_id), "workflow_run ID")
        _same(payload.get("run_attempt"), str(run_attempt), "workflow_run attempt")
        _same(payload.get("workflow_id"), str(SOURCE_WORKFLOW_ID), "workflow_run workflow ID")
        _same(payload.get("event"), "push", "workflow_run event")
        _same(payload.get("status"), "completed", "workflow_run status")
        _same(payload.get("conclusion"), "success", "workflow_run conclusion")
        _same(payload.get("head_sha"), head_sha, "workflow_run head SHA")
        _same(payload.get("head_branch"), head_branch, "workflow_run head branch")
    _same(repository, SOURCE_REPOSITORY, "publisher repository")
    return run_id, run_attempt, head_sha, version


def _release_assets(
    release: Mapping[str, Any], version: str, head_sha: str
) -> dict[str, dict[str, Any]]:
    _int(release.get("id"), "release ID")
    expected_tag = f"{TAG_PREFIX}{version}"
    _same(release.get("tag_name"), expected_tag, "release tag")
    _same(release.get("draft"), False, "release draft flag")
    _text(release.get("published_at"), "release published timestamp")
    target = _text(release.get("target_commitish"), "release target commit or branch")
    # GitHub returns either the tag's commit SHA or the branch used to create
    # the release.  The tag ref above is the authoritative binding.  When the
    # API gives us a SHA, still require it to match that binding; a branch name
    # carries no independent provenance and is accepted for compatibility.
    if re.fullmatch(r"[0-9a-f]{40}", target.lower()):
        _same(target.lower(), head_sha, "release target commit")
    assets = release.get("assets")
    if not isinstance(assets, list):
        raise ReleaseValidationError("release has no asset list")

    names = {
        "darwin-universal": f"cua-driver-rs-{version}-darwin-universal-binary.tar.gz",
        "linux-x86_64": f"cua-driver-rs-{version}-linux-x86_64-binary.tar.gz",
        "linux-arm64": f"cua-driver-rs-{version}-linux-arm64-binary.tar.gz",
        "windows-x86_64": f"cua-driver-rs-{version}-windows-x86_64-binary.zip",
        "windows-arm64": f"cua-driver-rs-{version}-windows-arm64-binary.zip",
    }
    result: dict[str, dict[str, Any]] = {}
    for key, name in names.items():
        candidates = [
            asset
            for asset in assets
            if isinstance(asset, Mapping) and asset.get("name") == name
        ]
        if len(candidates) != 1:
            raise ReleaseValidationError(f"release must contain exactly one asset named {name}")
        asset = candidates[0]
        asset_id = _int(asset.get("id"), f"release asset {name} ID")
        _same(asset.get("state"), "uploaded", f"release asset {name} state")
        expected_url = (
            f"https://github.com/{SOURCE_REPOSITORY}/releases/download/"
            f"{expected_tag}/{name}"
        )
        _same(asset.get("browser_download_url"), expected_url, f"release asset {name} URL")
        digest = _text(asset.get("digest"), f"release asset {name} digest")
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
            raise ReleaseValidationError(f"release asset {name} has no SHA-256 digest")
        size = _int(asset.get("size"), f"release asset {name} size")
        if size > MAX_ARTIFACT_BYTES:
            raise ReleaseValidationError(f"release asset {name} is too large")
        result[key] = {
            "id": asset_id,
            "name": name,
            "sha256": digest.removeprefix("sha256:"),
            "size": size,
        }
    return result


def _source_artifacts(
    api: Api, repository: str, run_id: int, head_sha: str
) -> dict[str, dict[str, Any]]:
    response = api.get(_api_path(repository, f"/actions/runs/{run_id}/artifacts?per_page=100"))
    artifacts = response.get("artifacts")
    if not isinstance(artifacts, list):
        raise ReleaseValidationError("source run artifacts response has no list")
    result: dict[str, dict[str, Any]] = {}
    for key, expected_name in PLATFORM_ARTIFACTS.items():
        candidates = [
            artifact
            for artifact in artifacts
            if isinstance(artifact, Mapping) and artifact.get("name") == expected_name
        ]
        if len(candidates) != 1:
            raise ReleaseValidationError(
                f"source run must contain exactly one artifact named {expected_name}"
            )
        artifact = candidates[0]
        artifact_id = _int(artifact.get("id"), f"source artifact {expected_name} ID")
        expected_url = (
            f"https://api.github.com/repos/{SOURCE_REPOSITORY}/actions/artifacts/{artifact_id}/zip"
        )
        _same(
            artifact.get("archive_download_url"),
            expected_url,
            f"source artifact {expected_name} URL",
        )
        _same(artifact.get("expired"), False, f"source artifact {expected_name} expired flag")
        size = _int(artifact.get("size_in_bytes"), f"source artifact {expected_name} size")
        if size > MAX_ARTIFACT_BYTES:
            raise ReleaseValidationError(f"source artifact {expected_name} is too large")
        artifact_run = artifact.get("workflow_run")
        if not isinstance(artifact_run, Mapping):
            raise ReleaseValidationError(f"source artifact {expected_name} has no workflow run")
        _same(artifact_run.get("id"), run_id, f"source artifact {expected_name} run ID")
        _same(artifact_run.get("head_sha"), head_sha, f"source artifact {expected_name} head SHA")
        result[key] = {"id": artifact_id, "name": expected_name, "size": size}
    return result


def validate(
    api: Api,
    event_name: str,
    repository: str,
    payload: Mapping[str, str] | None = None,
) -> dict[str, Any]:
    """Return a compact provenance manifest or raise ``ReleaseValidationError``."""

    _same(repository, SOURCE_REPOSITORY, "publisher repository")
    if event_name != "workflow_run":
        raise ReleaseValidationError(f"unsupported event {event_name!r}")
    if payload is None:
        raise ReleaseValidationError("workflow_run payload is required")
    try:
        payload_run_id = int(payload.get("run_id", "0"))
    except (TypeError, ValueError) as exc:
        raise ReleaseValidationError("workflow_run ID is not an integer") from exc
    run_id = _int(payload_run_id, "workflow_run ID")
    live_run = api.get(_api_path(repository, f"/actions/runs/{run_id}"))
    run_id, run_attempt, head_sha, version = _validate_run(live_run, repository, payload)
    normalized_version = _pep440_version(version)
    tag = f"{TAG_PREFIX}{version}"

    tag_sha = _tag_commit(api, repository, tag)
    _same(tag_sha, head_sha, "tag commit")
    release = api.get(_api_path(repository, f"/releases/tags/{quote(tag, safe='')}"))
    assets = _release_assets(release, version, head_sha)
    artifacts = _source_artifacts(api, repository, run_id, head_sha)
    return {
        "repository": repository,
        "source_workflow_id": SOURCE_WORKFLOW_ID,
        "source_workflow_name": SOURCE_WORKFLOW_NAME,
        "source_workflow_path": SOURCE_WORKFLOW_PATH,
        "source_run_id": run_id,
        "source_run_attempt": run_attempt,
        "source_head_sha": head_sha,
        "tag": tag,
        "version": version,
        "normalized_version": normalized_version,
        "release_id": _int(release.get("id"), "release ID"),
        "assets": assets,
        "artifacts": artifacts,
    }


def _env_payload(env: Mapping[str, str]) -> dict[str, str]:
    env_names = {
        "run_id": "WORKFLOW_RUN_ID",
        "run_attempt": "WORKFLOW_RUN_ATTEMPT",
        "workflow_id": "WORKFLOW_RUN_WORKFLOW_ID",
        "event": "WORKFLOW_RUN_EVENT",
        "status": "WORKFLOW_RUN_STATUS",
        "conclusion": "WORKFLOW_RUN_CONCLUSION",
        "head_sha": "WORKFLOW_RUN_HEAD_SHA",
        "head_branch": "WORKFLOW_RUN_HEAD_BRANCH",
    }
    return {field: env.get(name, "") for field, name in env_names.items()}


def _write_output(manifest: Mapping[str, Any], output_path: str) -> None:
    serialized = json.dumps(manifest, separators=(",", ":"), sort_keys=True)
    if output_path:
        with Path(output_path).open("a", encoding="utf-8") as output:
            output.write(f"provenance<<CMUX_PROVENANCE\n{serialized}\nCMUX_PROVENANCE\n")
            output.write(f"source_run_id={manifest['source_run_id']}\n")
            output.write(f"source_head_sha={manifest['source_head_sha']}\n")
            output.write(f"version={manifest['version']}\n")
            output.write(f"normalized_version={manifest['normalized_version']}\n")
            output.write(f"tag={manifest['tag']}\n")
    else:
        print(serialized)


def main() -> int:
    try:
        env = os.environ
        event_name = env.get("EVENT_NAME", "")
        repository = env.get("REPOSITORY", "")
        payload = _env_payload(env) if event_name == "workflow_run" else None
        manifest = validate(
            GitHubApi(env.get("GH_TOKEN", "")),
            event_name,
            repository,
            payload=payload,
        )
        _write_output(manifest, env.get("GITHUB_OUTPUT", ""))
        return 0
    except (ReleaseValidationError, ValueError) as exc:
        print(f"::error::Release provenance validation failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
