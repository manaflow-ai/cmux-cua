"""Behavior tests for the non-tagged protected-main workflow validator."""

from __future__ import annotations

import unittest
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPT_DIR))

from validate_main_workflow_run import (
    EXPECTED_REPOSITORY,
    OBSERVER_WORKFLOW_NAME,
    OBSERVER_WORKFLOW_PATH,
    ValidationError,
    validate,
)


SHA = "a" * 40
MAIN_SHA = "b" * 40


class FakeApi:
    def __init__(self) -> None:
        self.responses = {
            "/repos/trycua/cua/actions/runs/42": {
                "id": 42,
                "name": OBSERVER_WORKFLOW_NAME,
                "path": OBSERVER_WORKFLOW_PATH,
                "event": "push",
                "status": "completed",
                "conclusion": "success",
                "run_attempt": 1,
                "repository": {"full_name": EXPECTED_REPOSITORY},
                "head_repository": {"full_name": EXPECTED_REPOSITORY},
                "head_branch": "main",
                "head_sha": SHA,
            },
            "/repos/trycua/cua/branches/main": {
                "protected": True,
                "commit": {"sha": MAIN_SHA},
            },
            f"/repos/trycua/cua/compare/{SHA}...{MAIN_SHA}": {
                "status": "ahead",
                "base_commit": {"sha": SHA},
                "head_commit": {"sha": MAIN_SHA},
                "merge_base_commit": {"sha": SHA},
            },
            f"/repos/trycua/cua/compare/{MAIN_SHA}...{MAIN_SHA}": {
                "status": "identical",
                "base_commit": {"sha": MAIN_SHA},
                "head_commit": {"sha": MAIN_SHA},
                "merge_base_commit": {"sha": MAIN_SHA},
            },
        }

    def get(self, path: str):
        if path.endswith("/branches/main") and path not in self.responses:
            return self.responses["/repos/trycua/cua/branches/main"]
        try:
            return self.responses[path]
        except KeyError as error:
            raise AssertionError(f"unexpected API path: {path}") from error


def values() -> dict[str, str]:
    return {
        "EVENT_NAME": "workflow_run",
        "REPOSITORY": EXPECTED_REPOSITORY,
        "TRUSTED_REF": "refs/heads/main",
        "TRUSTED_REF_TYPE": "branch",
        "TRUSTED_REF_NAME": "main",
        "TRUSTED_REF_PROTECTED": "true",
        "TRUSTED_SHA": MAIN_SHA,
        "SOURCE_RUN_ID": "42",
        "SOURCE_RUN_ATTEMPT": "1",
        "SOURCE_EVENT": "push",
        "SOURCE_STATUS": "completed",
        "SOURCE_CONCLUSION": "success",
        "SOURCE_REPOSITORY": EXPECTED_REPOSITORY,
        "SOURCE_HEAD_REPOSITORY": EXPECTED_REPOSITORY,
        "SOURCE_SHA": SHA,
    }


class MainWorkflowValidatorTests(unittest.TestCase):
    def test_valid_run_is_accepted(self) -> None:
        result = validate(FakeApi(), values())
        self.assertEqual(result["commit"], SHA)
        self.assertEqual(result["main_commit"], MAIN_SHA)

    def test_fork_source_is_rejected(self) -> None:
        candidate = values()
        candidate["SOURCE_REPOSITORY"] = "attacker/cmux-cua"
        with self.assertRaisesRegex(ValidationError, "event source repository"):
            validate(FakeApi(), candidate)

    def test_manaflow_fork_repository_is_rejected(self) -> None:
        candidate = values()
        candidate["REPOSITORY"] = "manaflow-ai/cmux-cua"
        with self.assertRaisesRegex(ValidationError, "publisher repository"):
            validate(FakeApi(), candidate)

    def test_unprotected_consumer_is_rejected(self) -> None:
        candidate = values()
        candidate["TRUSTED_REF_PROTECTED"] = "false"
        with self.assertRaisesRegex(ValidationError, "not protected"):
            validate(FakeApi(), candidate)

    def test_wrong_observer_path_is_rejected(self) -> None:
        api = FakeApi()
        api.responses["/repos/trycua/cua/actions/runs/42"]["path"] = "old.yml"
        with self.assertRaisesRegex(ValidationError, "source workflow path"):
            validate(api, values())

    def test_source_sha_must_be_ancestor(self) -> None:
        api = FakeApi()
        api.responses[f"/repos/trycua/cua/compare/{SHA}...{MAIN_SHA}"]["status"] = "behind"
        with self.assertRaisesRegex(ValidationError, "not an ancestor"):
            validate(api, values())


if __name__ == "__main__":
    unittest.main()
