"""Behavior tests for the protected-main release request validator."""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path
from typing import Any, Mapping


ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / ".github" / "scripts" / "validate_release_request.py"
SPEC = importlib.util.spec_from_file_location("validate_release_request", SCRIPT)
assert SPEC and SPEC.loader
validator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validator)


REPOSITORY = validator.EXPECTED_REPOSITORY
SOURCE_SHA = "a" * 40
MAIN_SHA = "b" * 40
TAG = "npm-core-v1.2.3"
RUN_ID = 42
RUN_ATTEMPT = 1


class FakeApi:
    def __init__(self, responses: Mapping[str, Mapping[str, Any]]) -> None:
        self.responses = dict(responses)
        self.calls: list[str] = []

    def get(self, path: str) -> Mapping[str, Any]:
        self.calls.append(path)
        try:
            return self.responses[path]
        except KeyError as error:
            raise AssertionError(f"unexpected API path: {path}") from error


def path(suffix: str) -> str:
    return f"/repos/trycua/cua{suffix}"


def responses() -> dict[str, Mapping[str, Any]]:
    run = {
        "id": RUN_ID,
        "run_attempt": RUN_ATTEMPT,
        "name": validator.OBSERVER_WORKFLOW_NAME,
        "path": validator.OBSERVER_WORKFLOW_PATH,
        "event": "push",
        "status": "completed",
        "conclusion": "success",
        "repository": {"full_name": REPOSITORY},
        "head_repository": {"full_name": REPOSITORY},
        "head_branch": TAG,
        "head_sha": SOURCE_SHA,
    }
    comparison = {
        "status": "ahead",
        "base_commit": {"sha": SOURCE_SHA},
        "head_commit": {"sha": MAIN_SHA},
        "merge_base_commit": {"sha": SOURCE_SHA},
    }
    return {
        path(f"/actions/runs/{RUN_ID}"): run,
        path("/git/ref/tags/npm-core-v1.2.3"): {
            "object": {"type": "commit", "sha": SOURCE_SHA}
        },
        path("/branches/main"): {"protected": True, "commit": {"sha": MAIN_SHA}},
        path(f"/compare/{SOURCE_SHA}...{MAIN_SHA}"): comparison,
    }


def values(**overrides: str) -> dict[str, str]:
    result = {
        "EVENT_NAME": "workflow_run",
        "REPOSITORY": REPOSITORY,
        "TRUSTED_REF": "refs/heads/main",
        "TRUSTED_REF_TYPE": "branch",
        "TRUSTED_REF_NAME": "main",
        "TRUSTED_REF_PROTECTED": "true",
        "TRUSTED_SHA": SOURCE_SHA,
        "TAG_PREFIX": "npm-core-v",
        "SOURCE_RUN_ID": str(RUN_ID),
        "SOURCE_RUN_ATTEMPT": str(RUN_ATTEMPT),
        "SOURCE_EVENT": "push",
        "SOURCE_STATUS": "completed",
        "SOURCE_CONCLUSION": "success",
        "SOURCE_REPOSITORY": REPOSITORY,
        "SOURCE_HEAD_REPOSITORY": REPOSITORY,
        "SOURCE_BRANCH": TAG,
        "SOURCE_SHA": SOURCE_SHA,
    }
    result.update(overrides)
    return result


class ReleaseRequestTests(unittest.TestCase):
    def test_accepts_valid_request_and_rechecks_refs(self) -> None:
        api = FakeApi(responses())
        result = validator.validate(api, values())
        self.assertEqual(result["tag"], TAG)
        self.assertEqual(result["version"], "1.2.3")
        self.assertEqual(result["commit"], SOURCE_SHA)
        self.assertEqual(result["main_commit"], MAIN_SHA)
        self.assertGreaterEqual(api.calls.count(path("/branches/main")), 2)
        self.assertGreaterEqual(api.calls.count(path("/git/ref/tags/npm-core-v1.2.3")), 2)

    def test_rejects_non_workflow_run_event(self) -> None:
        with self.assertRaises(validator.ValidationError):
            validator.validate(FakeApi(responses()), values(EVENT_NAME="push"))

    def test_rejects_manaflow_fork_repository(self) -> None:
        with self.assertRaisesRegex(validator.ValidationError, "publisher repository"):
            validator.validate(
                FakeApi(responses()),
                values(REPOSITORY="manaflow-ai/cmux-cua"),
            )

    def test_rejects_unprotected_consumer(self) -> None:
        with self.assertRaises(validator.ValidationError):
            validator.validate(FakeApi(responses()), values(TRUSTED_REF_PROTECTED="false"))

    def test_rejects_fork_source(self) -> None:
        api = FakeApi(responses())
        with self.assertRaises(validator.ValidationError):
            validator.validate(api, values(SOURCE_REPOSITORY="attacker/cmux-cua"))

    def test_rejects_tag_for_another_package(self) -> None:
        with self.assertRaises(validator.ValidationError):
            validator.validate(FakeApi(responses()), values(TAG_PREFIX="npm-cli-v"))

    def test_rejects_moved_tag(self) -> None:
        api_responses = responses()
        tag_path = path("/git/ref/tags/npm-core-v1.2.3")
        api_responses[tag_path] = {
            "object": {"type": "commit", "sha": "c" * 40}
        }
        with self.assertRaises(validator.ValidationError):
            validator.validate(FakeApi(api_responses), values())

    def test_rejects_mismatched_expected_source(self) -> None:
        with self.assertRaises(validator.ValidationError):
            validator.validate(FakeApi(responses()), values(EXPECTED_TAG="npm-core-v9.9.9"))

    def test_rejects_malformed_expected_sha(self) -> None:
        with self.assertRaises(validator.ValidationError):
            validator.validate(FakeApi(responses()), values(EXPECTED_SHA="not-a-sha"))


if __name__ == "__main__":
    unittest.main()
