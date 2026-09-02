"""Behavioral tests for the cua-driver release provenance validator."""

from __future__ import annotations

import copy
import json
import sys
import unittest
from pathlib import Path
from typing import Any, Mapping


SCRIPT_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPT_DIR))

import verify_cua_driver_release as validator


HEAD_SHA = "a" * 40
TAG = "cua-driver-rs-v1.2.3"
VERSION = "1.2.3"
RUN_ID = 123456
RUN_ATTEMPT = 2


class FakeApi:
    """Return isolated fixture objects and record every requested path."""

    def __init__(self, responses: Mapping[str, Any]):
        self.responses = copy.deepcopy(dict(responses))
        self.calls: list[str] = []

    def get(self, path: str) -> Mapping[str, Any]:
        self.calls.append(path)
        try:
            value = self.responses[path]
        except KeyError as exc:
            raise AssertionError(f"unexpected API request: {path}") from exc
        return copy.deepcopy(value)


def _paths() -> dict[str, str]:
    base = "/repos/manaflow-ai/cmux-cua"
    return {
        "run": f"{base}/actions/runs/{RUN_ID}",
        "ref": f"{base}/git/ref/tags/{TAG}",
        "release": f"{base}/releases/tags/{TAG}",
        "artifacts": f"{base}/actions/runs/{RUN_ID}/artifacts?per_page=100",
    }


def _run() -> dict[str, Any]:
    return {
        "id": RUN_ID,
        "run_number": 88,
        "run_attempt": RUN_ATTEMPT,
        "workflow_id": validator.SOURCE_WORKFLOW_ID,
        "name": validator.SOURCE_WORKFLOW_NAME,
        "path": validator.SOURCE_WORKFLOW_PATH,
        "event": "push",
        "status": "completed",
        "conclusion": "success",
        "head_branch": TAG,
        "head_sha": HEAD_SHA,
        "repository": {"full_name": validator.SOURCE_REPOSITORY},
        "head_repository": {"full_name": validator.SOURCE_REPOSITORY},
    }


def _release() -> dict[str, Any]:
    names = {
        "darwin-universal": f"cua-driver-rs-{VERSION}-darwin-universal-binary.tar.gz",
        "linux-x86_64": f"cua-driver-rs-{VERSION}-linux-x86_64-binary.tar.gz",
        "linux-arm64": f"cua-driver-rs-{VERSION}-linux-arm64-binary.tar.gz",
        "windows-x86_64": f"cua-driver-rs-{VERSION}-windows-x86_64-binary.zip",
        "windows-arm64": f"cua-driver-rs-{VERSION}-windows-arm64-binary.zip",
    }
    assets = []
    for index, name in enumerate(names.values(), start=1):
        assets.append(
            {
                "id": 9000 + index,
                "name": name,
                "state": "uploaded",
                "size": 100,
                "digest": "sha256:" + "b" * 64,
                "browser_download_url": f"https://github.com/{validator.SOURCE_REPOSITORY}/releases/download/{TAG}/{name}",
            }
        )
    assets.append(
        {
            "id": 9010,
            "name": "checksums.txt",
            "state": "uploaded",
            "size": 100,
            "digest": "sha256:" + "c" * 64,
            "browser_download_url": (
                f"https://github.com/{validator.SOURCE_REPOSITORY}/releases/download/{TAG}/checksums.txt"
            ),
        }
    )
    return {
        "id": 7000,
        "tag_name": TAG,
        "draft": False,
        "published_at": "2026-09-01T00:00:00Z",
        # GitHub may report the branch used to create a release instead of the
        # tag commit.  The tag ref is checked separately and is authoritative.
        "target_commitish": "main",
        "assets": assets,
    }


def _artifacts() -> dict[str, Any]:
    artifacts = []
    for index, name in enumerate(validator.PLATFORM_ARTIFACTS.values(), start=1):
        artifact_id = 8000 + index
        artifacts.append(
            {
                "id": artifact_id,
                "name": name,
                "expired": False,
                "size_in_bytes": 200,
                "archive_download_url": f"https://api.github.com/repos/{validator.SOURCE_REPOSITORY}/actions/artifacts/{artifact_id}/zip",
                "workflow_run": {"id": RUN_ID, "head_sha": HEAD_SHA},
            }
        )
    return {"artifacts": artifacts}


def _responses(include_run: bool = True) -> dict[str, Any]:
    paths = _paths()
    result = {
        paths["ref"]: {"object": {"type": "commit", "sha": HEAD_SHA}},
        paths["release"]: _release(),
        paths["artifacts"]: _artifacts(),
    }
    if include_run:
        result[paths["run"]] = _run()
    return result


def _payload() -> dict[str, str]:
    return {
        "run_id": str(RUN_ID),
        "run_attempt": str(RUN_ATTEMPT),
        "workflow_id": str(validator.SOURCE_WORKFLOW_ID),
        "event": "push",
        "status": "completed",
        "conclusion": "success",
        "head_sha": HEAD_SHA,
        "head_branch": TAG,
    }


class TestReleaseProvenance(unittest.TestCase):
    def test_workflow_run_returns_bound_manifest(self) -> None:
        api = FakeApi(_responses())
        manifest = validator.validate(
            api,
            "workflow_run",
            validator.SOURCE_REPOSITORY,
            payload=_payload(),
        )

        self.assertEqual(manifest["source_run_id"], RUN_ID)
        self.assertEqual(manifest["source_run_attempt"], RUN_ATTEMPT)
        self.assertEqual(manifest["source_head_sha"], HEAD_SHA)
        self.assertEqual(manifest["tag"], TAG)
        self.assertEqual(manifest["version"], VERSION)
        self.assertEqual(manifest["normalized_version"], VERSION)
        self.assertEqual(set(manifest["assets"]), set(validator.PLATFORM_ARTIFACTS))
        self.assertEqual(set(manifest["artifacts"]), set(validator.PLATFORM_ARTIFACTS))
        self.assertIn(_paths()["run"], api.calls)

    def test_rejects_unsuccessful_source_run(self) -> None:
        responses = _responses()
        responses[_paths()["run"]]["conclusion"] = "failure"
        with self.assertRaises(validator.ReleaseValidationError):
            validator.validate(
                FakeApi(responses),
                "workflow_run",
                validator.SOURCE_REPOSITORY,
                payload=_payload(),
            )

    def test_rejects_wrong_workflow_identity(self) -> None:
        responses = _responses()
        responses[_paths()["run"]]["workflow_id"] += 1
        with self.assertRaises(validator.ReleaseValidationError):
            validator.validate(
                FakeApi(responses),
                "workflow_run",
                validator.SOURCE_REPOSITORY,
                payload=_payload(),
            )

    def test_rejects_payload_head_sha_race(self) -> None:
        payload = _payload()
        payload["head_sha"] = "c" * 40
        with self.assertRaises(validator.ReleaseValidationError):
            validator.validate(
                FakeApi(_responses()),
                "workflow_run",
                validator.SOURCE_REPOSITORY,
                payload=payload,
            )

    def test_rejects_tag_commit_mismatch(self) -> None:
        responses = _responses()
        responses[_paths()["ref"]]["object"]["sha"] = "d" * 40
        with self.assertRaises(validator.ReleaseValidationError):
            validator.validate(
                FakeApi(responses),
                "workflow_run",
                validator.SOURCE_REPOSITORY,
                payload=_payload(),
            )

    def test_rejects_release_target_mismatch(self) -> None:
        responses = _responses()
        responses[_paths()["release"]]["target_commitish"] = "e" * 40
        with self.assertRaises(validator.ReleaseValidationError):
            validator.validate(
                FakeApi(responses),
                "workflow_run",
                validator.SOURCE_REPOSITORY,
                payload=_payload(),
            )

    def test_rejects_missing_release_target(self) -> None:
        responses = _responses()
        responses[_paths()["release"]]["target_commitish"] = None
        with self.assertRaises(validator.ReleaseValidationError):
            validator.validate(
                FakeApi(responses),
                "workflow_run",
                validator.SOURCE_REPOSITORY,
                payload=_payload(),
            )

    def test_rejects_cross_run_artifact(self) -> None:
        responses = _responses()
        responses[_paths()["artifacts"]]["artifacts"][0]["workflow_run"]["id"] = RUN_ID + 1
        with self.assertRaises(validator.ReleaseValidationError):
            validator.validate(
                FakeApi(responses),
                "workflow_run",
                validator.SOURCE_REPOSITORY,
                payload=_payload(),
            )

    def test_rejects_non_workflow_run_events(self) -> None:
        with self.assertRaises(validator.ReleaseValidationError):
            validator.validate(
                FakeApi(_responses()),
                "workflow_dispatch",
                validator.SOURCE_REPOSITORY,
            )

    def test_normalizes_semver_prereleases_for_wheel_metadata(self) -> None:
        cases = {
            "1.2.3-alpha": "1.2.3a0",
            "1.2.3-alpha.01": "1.2.3a1",
            "1.2.3-beta-2": "1.2.3b2",
            "1.2.3-rc1+Build-01": "1.2.3rc1+build.1",
            "1.2.3-dev": "1.2.3.dev0",
            "1.2.3-1": "1.2.3.post1",
        }
        for source, expected in cases.items():
            with self.subTest(source=source):
                self.assertEqual(validator._pep440_version(source), expected)

    def test_rejects_unknown_semver_suffix(self) -> None:
        with self.assertRaises(validator.ReleaseValidationError):
            validator._pep440_version("1.2.3-preview.foo")


if __name__ == "__main__":
    unittest.main()
