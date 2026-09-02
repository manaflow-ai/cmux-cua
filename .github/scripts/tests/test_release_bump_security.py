"""Security contracts for the protected release-bump consumer."""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[3]
VALIDATOR_PATH = REPO_ROOT / ".github/scripts/validate_release_bump_request.py"


class FakeApi:
    def __init__(self, responses: dict[str, dict[str, Any]]) -> None:
        self.responses = responses

    def get(self, path: str) -> dict[str, Any]:
        try:
            return self.responses[path]
        except KeyError as error:
            raise AssertionError(f"unexpected API path: {path}") from error


class TestReleaseBumpRequestValidator(unittest.TestCase):
    @staticmethod
    def validator() -> Any:
        spec = importlib.util.spec_from_file_location(
            "validate_release_bump_request", VALIDATOR_PATH
        )
        if spec is None or spec.loader is None:
            raise AssertionError("could not load release-bump validator")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    @staticmethod
    def values(commit: str = "a" * 40) -> dict[str, str]:
        return {
            "EVENT_NAME": "workflow_run",
            "REPOSITORY": "manaflow-ai/cmux-cua",
            "EXPECTED_REPOSITORY": "manaflow-ai/cmux-cua",
            "SOURCE_RUN_ID": "9001",
            "SOURCE_RUN_ATTEMPT": "1",
            "SOURCE_WORKFLOW_ID": "9002",
            "SOURCE_WORKFLOW_NAME": "CD: Bump Version (request)",
            "SOURCE_WORKFLOW_PATH": ".github/workflows/release-bump-request.yml",
            "SOURCE_EVENT": "workflow_dispatch",
            "SOURCE_STATUS": "completed",
            "SOURCE_CONCLUSION": "success",
            "SOURCE_REPOSITORY": "manaflow-ai/cmux-cua",
            "SOURCE_HEAD_REPOSITORY": "manaflow-ai/cmux-cua",
            "SOURCE_BRANCH": "main",
            "SOURCE_SHA": commit,
            "TRUSTED_SHA": commit,
            "TRUSTED_REF_PROTECTED": "true",
        }

    @staticmethod
    def responses(values: dict[str, str], request_size: int = 128) -> dict[str, dict[str, Any]]:
        base = "/repos/manaflow-ai/cmux-cua"
        run_id = values["SOURCE_RUN_ID"]
        return {
            f"{base}/actions/runs/{run_id}": {
                "id": int(run_id),
                "workflow_id": int(values["SOURCE_WORKFLOW_ID"]),
                "name": values["SOURCE_WORKFLOW_NAME"],
                "path": values["SOURCE_WORKFLOW_PATH"],
                "event": "workflow_dispatch",
                "status": "completed",
                "conclusion": "success",
                "run_attempt": 1,
                "head_branch": "main",
                "head_sha": values["SOURCE_SHA"],
                "repository": {"full_name": values["EXPECTED_REPOSITORY"]},
                "head_repository": {"full_name": values["EXPECTED_REPOSITORY"]},
            },
            f"{base}/git/ref/heads/main": {
                "object": {"type": "commit", "sha": values["SOURCE_SHA"]}
            },
            f"{base}/actions/runs/{run_id}/artifacts?per_page=100": {
                "artifacts": [
                    {
                        "id": 7001,
                        "name": "release-bump-request",
                        "expired": False,
                        "size_in_bytes": request_size,
                        "workflow_run": {"id": int(run_id), "head_sha": values["SOURCE_SHA"]},
                    }
                ]
            },
        }

    @staticmethod
    def request_file(
        service: str = "lume", bump_type: str = "patch"
    ) -> tempfile.TemporaryDirectory[str]:
        directory = tempfile.TemporaryDirectory()
        path = Path(directory.name) / "request.json"
        path.write_text(
            json.dumps({"service": service, "bump_type": bump_type}),
            encoding="utf-8",
        )
        return directory

    def test_accepts_request_bound_to_current_main(self) -> None:
        validator = self.validator()
        values = self.values()
        request_dir = self.request_file()
        self.addCleanup(request_dir.cleanup)
        result = validator.validate(
            FakeApi(self.responses(values)),
            values,
            Path(request_dir.name) / "request.json",
        )
        self.assertEqual(result["service"], "lume")
        self.assertEqual(result["bump_type"], "patch")
        self.assertEqual(result["commit"], values["SOURCE_SHA"])

    def test_rejects_feature_branch_or_invalid_request(self) -> None:
        validator = self.validator()
        values = self.values()
        request_dir = self.request_file(service="not-a-service")
        self.addCleanup(request_dir.cleanup)
        with self.assertRaises(validator.ValidationError):
            validator.validate(
                FakeApi(self.responses(values)),
                values,
                Path(request_dir.name) / "request.json",
            )

        values = self.values()
        values["SOURCE_BRANCH"] = "feature/release"
        with self.assertRaises(validator.ValidationError):
            validator.validate(FakeApi(self.responses(values)), values, Path(request_dir.name) / "request.json")

    def test_rejects_oversized_or_missing_artifact(self) -> None:
        validator = self.validator()
        values = self.values()
        request_dir = self.request_file()
        self.addCleanup(request_dir.cleanup)
        with self.assertRaises(validator.ValidationError):
            validator.validate(
                FakeApi(self.responses(values, request_size=validator.MAX_REQUEST_BYTES + 1)),
                values,
                Path(request_dir.name) / "request.json",
            )


class TestReleaseBumpWorkflowContracts(unittest.TestCase):
    @staticmethod
    def read(path: str) -> str:
        return (REPO_ROOT / path).read_text(encoding="utf-8")

    def test_dispatch_side_is_credential_free(self) -> None:
        request = self.read(".github/workflows/release-bump-request.yml")
        self.assertIn("workflow_dispatch:", request)
        self.assertIn("permissions: {}", request)
        self.assertNotIn("secrets.", request)
        self.assertNotIn("contents: write", request)
        self.assertNotIn("environment:", request)
        self.assertNotIn("actions/checkout", request)

    def test_privileged_consumer_is_main_sourced(self) -> None:
        workflow = self.read(".github/workflows/release-bump-version.yml")
        self.assertIn("workflow_run:", workflow)
        self.assertIn('workflows: ["CD: Bump Version (request)"]', workflow)
        self.assertNotIn("workflow_dispatch:", workflow)
        self.assertIn("validate_release_bump_request.py", workflow)
        self.assertIn("environment: release-bump", workflow)
        self.assertIn("ref: ${{ needs.validate-request.outputs.commit }}", workflow)

    def test_tag_creation_is_immutable(self) -> None:
        workflow = self.read(".github/workflows/release-bump-version.yml")
        self.assertNotIn("git tag -d", workflow)
        self.assertNotIn("git tag -f", workflow)
        self.assertNotIn("git push origin :refs/tags", workflow)
        self.assertNotIn("git push origin \":refs/tags", workflow)
        self.assertNotIn("git push --force", workflow)
        self.assertNotIn("gh api -X DELETE", workflow)
        self.assertNotIn("git tag -f", workflow)
        self.assertIn("--no-tag", workflow)
        self.assertIn('refs/tags/${TAG_NAME}', workflow)
        self.assertIn("TARGET_BRANCH: main", workflow)
        self.assertNotIn("target_branch:", workflow)

    def test_all_new_action_refs_are_immutable(self) -> None:
        for path in (
            ".github/workflows/release-bump-version.yml",
            ".github/workflows/release-bump-request.yml",
        ):
            for line in self.read(path).splitlines():
                if "uses:" not in line or line.lstrip().startswith("#"):
                    continue
                reference = line.split("uses:", 1)[1].split("#", 1)[0].strip()
                self.assertRegex(reference, r"@[0-9a-f]{40}\Z", f"{path}: {line}")


if __name__ == "__main__":
    unittest.main()
