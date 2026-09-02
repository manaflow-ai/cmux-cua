"""Security behavior tests for the model and cold-start benchmark workflows."""

from __future__ import annotations

import importlib.util
import re
import unittest
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[3]
VALIDATOR_PATH = REPO_ROOT / ".github/scripts/validate_trusted_main_run.py"
WORKFLOWS = REPO_ROOT / ".github/workflows"
CANONICAL_REPOSITORY = "manaflow-ai/cmux-cua"


def load_validator() -> Any:
    spec = importlib.util.spec_from_file_location("validate_trusted_main_run", VALIDATOR_PATH)
    if spec is None or spec.loader is None:
        raise AssertionError("could not load trusted-main validator")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FakeApi:
    def __init__(self, responses: dict[str, dict[str, Any]]) -> None:
        self.responses = responses
        self.calls: list[str] = []

    def get(self, path: str) -> dict[str, Any]:
        self.calls.append(path)
        try:
            return self.responses[path]
        except KeyError as error:
            raise AssertionError(f"unexpected API request: {path}") from error


def valid_fixture(
    *,
    kind: str = "model-tests",
    source_event: str = "workflow_dispatch",
    source_sha: str = "a" * 40,
    trusted_sha: str | None = None,
) -> tuple[dict[str, str], FakeApi]:
    trusted_sha = trusted_sha or source_sha
    workflow_names = {
        "model-tests": (
            "CI: Test Models (request)",
            ".github/workflows/ci-test-models-request.yml",
        ),
        "cold-start-benchmark": (
            "CI: Cold Start Benchmark (request)",
            ".github/workflows/ci-cold-start-benchmark-request.yml",
        ),
    }
    workflow_name, workflow_path = workflow_names[kind]
    repository_path = f"/repos/{CANONICAL_REPOSITORY}"
    run_path = f"{repository_path}/actions/runs/9001"
    branch_path = f"{repository_path}/branches/main"
    run = {
        "id": 9001,
        "name": workflow_name,
        "path": workflow_path,
        "event": source_event,
        "status": "completed",
        "conclusion": "success",
        "run_attempt": 1,
        "head_branch": "main",
        "head_sha": source_sha,
        "repository": {"full_name": CANONICAL_REPOSITORY},
        "head_repository": {"full_name": CANONICAL_REPOSITORY},
    }
    branch = {"protected": True, "commit": {"sha": source_sha}}
    values = {
        "EVENT_NAME": "workflow_run",
        "REPOSITORY": CANONICAL_REPOSITORY,
        "TRUSTED_REF": "refs/heads/main",
        "TRUSTED_REF_TYPE": "branch",
        "TRUSTED_REF_PROTECTED": "true",
        "TRUSTED_SHA": trusted_sha,
        "REQUEST_KIND": kind,
        "SOURCE_RUN_ID": "9001",
        "SOURCE_RUN_ATTEMPT": "1",
        "SOURCE_EVENT": source_event,
        "SOURCE_STATUS": "completed",
        "SOURCE_CONCLUSION": "success",
        "SOURCE_WORKFLOW_NAME": workflow_name,
        "SOURCE_WORKFLOW_PATH": workflow_path,
        "SOURCE_REPOSITORY": CANONICAL_REPOSITORY,
        "SOURCE_HEAD_REPOSITORY": CANONICAL_REPOSITORY,
        "SOURCE_BRANCH": "main",
        "SOURCE_SHA": source_sha,
    }
    return values, FakeApi({run_path: run, branch_path: branch})


class TestTrustedMainRunValidation(unittest.TestCase):
    def test_accepts_current_main_manual_request(self) -> None:
        validator = load_validator()
        values, api = valid_fixture()

        result = validator.validate(api, values)

        self.assertEqual(result, "a" * 40)
        self.assertGreaterEqual(api.calls.count("/repos/manaflow-ai/cmux-cua/branches/main"), 2)

    def test_accepts_current_main_schedule_request(self) -> None:
        validator = load_validator()
        values, api = valid_fixture(source_event="schedule")

        self.assertEqual(validator.validate(api, values), "a" * 40)

    def test_rejects_feature_branch_even_when_sha_matches_main(self) -> None:
        validator = load_validator()
        values, api = valid_fixture()
        values["SOURCE_BRANCH"] = "feature/attacker"
        api.responses["/repos/manaflow-ai/cmux-cua/actions/runs/9001"]["head_branch"] = (
            "feature/attacker"
        )

        with self.assertRaisesRegex(validator.ValidationError, "main"):
            validator.validate(api, values)

    def test_rejects_stale_main_commit(self) -> None:
        validator = load_validator()
        values, api = valid_fixture(source_sha="a" * 40, trusted_sha="b" * 40)

        with self.assertRaisesRegex(validator.ValidationError, "current main"):
            validator.validate(api, values)

    def test_rejects_foreign_or_failed_source_run(self) -> None:
        validator = load_validator()
        values, api = valid_fixture()
        values["SOURCE_HEAD_REPOSITORY"] = "attacker/example"
        api.responses["/repos/manaflow-ai/cmux-cua/actions/runs/9001"]["head_repository"] = {
            "full_name": "attacker/example"
        }
        with self.assertRaises(validator.ValidationError):
            validator.validate(api, values)

        values, api = valid_fixture()
        values["SOURCE_CONCLUSION"] = "failure"
        api.responses["/repos/manaflow-ai/cmux-cua/actions/runs/9001"]["conclusion"] = "failure"
        with self.assertRaises(validator.ValidationError):
            validator.validate(api, values)

    def test_rejects_wrong_workflow_and_unprotected_main(self) -> None:
        validator = load_validator()
        values, api = valid_fixture()
        values["SOURCE_WORKFLOW_PATH"] = ".github/workflows/attacker.yml"
        with self.assertRaises(validator.ValidationError):
            validator.validate(api, values)

        values, api = valid_fixture()
        api.responses["/repos/manaflow-ai/cmux-cua/branches/main"]["protected"] = False
        with self.assertRaisesRegex(validator.ValidationError, "protected"):
            validator.validate(api, values)


class TestWorkflowContracts(unittest.TestCase):
    privileged = ("ci-test-models.yml", "ci-cold-start-benchmark.yml")
    request = (
        "ci-test-models-request.yml",
        "ci-cold-start-benchmark-request.yml",
    )

    @staticmethod
    def text(name: str) -> str:
        return (WORKFLOWS / name).read_text(encoding="utf-8")

    def test_privileged_workflows_use_workflow_run_and_empty_global_permissions(self) -> None:
        for name in self.privileged:
            source = self.text(name)
            self.assertIn("workflow_run:", source, name)
            self.assertNotRegex(source, r"(?m)^  workflow_dispatch:", name)
            self.assertRegex(source, r"(?m)^permissions:\s*\{\}\s*$", name)
            self.assertIn("validate_trusted_main_run.py", source, name)

    def test_request_workflows_have_no_secrets_or_write_permissions(self) -> None:
        for name in self.request:
            source = self.text(name)
            self.assertRegex(source, r"(?m)^  workflow_dispatch:\s*$", name)
            self.assertRegex(source, r"(?m)^permissions:\s*\{\}\s*$", name)
            self.assertNotIn("secrets.", source, name)
            self.assertNotRegex(source, r"(?m)^\s+contents:\s+write", name)
            self.assertNotIn("actions/checkout", source, name)

    def test_secret_jobs_are_canonical_and_environment_protected(self) -> None:
        models = self.text("ci-test-models.yml")
        cold_start = self.text("ci-cold-start-benchmark.yml")
        for source, jobs in (
            (models, ("test-all-models", "test-summary")),
            (cold_start, ("benchmark",)),
        ):
            for job in jobs:
                block = re.search(
                    rf"(?ms)^  {re.escape(job)}:\n(?:(?!^  [A-Za-z0-9_-]+:).)*",
                    source,
                )
                self.assertIsNotNone(block, job)
                job_text = block.group(0)
                self.assertIn("github.repository == 'manaflow-ai/cmux-cua'", job_text)
                self.assertIn("environment:", job_text)
                self.assertLess(job_text.index("github.repository"), job_text.index("permissions:"))

    def test_all_external_actions_are_commit_pinned(self) -> None:
        for name in self.privileged + self.request:
            for line in self.text(name).splitlines():
                if "uses:" not in line or line.lstrip().startswith("#"):
                    continue
                reference = line.split("uses:", 1)[1].split("#", 1)[0].strip()
                if reference.startswith("./"):
                    continue
                self.assertRegex(reference, r"@[0-9a-f]{40}\Z", f"{name}: {line}")

    def test_slack_action_is_pinned(self) -> None:
        for name in self.privileged:
            source = self.text(name)
            self.assertRegex(
                source,
                r"rtCamp/action-slack-notify@[0-9a-f]{40}",
                name,
            )


if __name__ == "__main__":
    unittest.main()
