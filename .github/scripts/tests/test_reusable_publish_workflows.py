"""Least-privilege contracts for reusable release workflows.

These tests are intentionally small workflow contracts. They catch accidental
credential or mutable-action regressions before a release workflow runs.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
WORKFLOWS = ROOT / ".github" / "workflows"


def workflow_lines(name: str) -> list[str]:
    return (WORKFLOWS / name).read_text(encoding="utf-8").splitlines()


def job_block(name: str, job: str) -> list[str]:
    source = workflow_lines(name)
    start = next(i for i, line in enumerate(source) if line == f"  {job}:")
    end = len(source)
    for i in range(start + 1, len(source)):
        if re.fullmatch(r"  [A-Za-z0-9_-]+:", source[i]):
            end = i
            break
    return source[start:end]


def permissions(name: str, job: str) -> dict[str, str]:
    block = job_block(name, job)
    try:
        start = block.index("    permissions:") + 1
    except ValueError:
        return {}
    result: dict[str, str] = {}
    for line in block[start:]:
        match = re.fullmatch(r"      ([a-z-]+): (read|write|none)", line)
        if match:
            result[match.group(1)] = match.group(2)
        elif line and not line.startswith("      "):
            break
    return result


class ReusablePublishWorkflowTests(unittest.TestCase):
    def assert_default_permissions_empty(self, name: str) -> None:
        self.assertIn("permissions: {}", workflow_lines(name), name)

    def assert_actions_pinned(self, name: str) -> None:
        for line_number, line in enumerate(workflow_lines(name), start=1):
            if "uses:" not in line or "./.github/" in line:
                continue
            match = re.search(r"uses:\s+[^\s@]+@([^\s#]+)", line)
            self.assertIsNotNone(match, f"{name}:{line_number} malformed action")
            self.assertRegex(
                match.group(1) if match else "",
                r"^[0-9a-f]{40}$",
                f"{name}:{line_number} must pin actions to a commit SHA",
            )

    def test_workflow_defaults_and_job_scopes(self) -> None:
        expected = {
            "py-reusable-build.yml": {"build": {"contents": "read"}},
            "py-reusable-publish.yml": {
                "reject-unless-enabled": {},
                "build-package": {"contents": "read"},
                "publish-legacy-token": {"actions": "read", "contents": "read"},
            },
            "ts-reusable-publish.yml": {
                "build": {"contents": "read"},
                "publish": {"actions": "read", "id-token": "write"},
                "publish-legacy-token": {"actions": "read", "contents": "read"},
            },
        }
        for name, jobs in expected.items():
            self.assert_default_permissions_empty(name)
            self.assert_actions_pinned(name)
            for job, expected_permissions in jobs.items():
                self.assertEqual(permissions(name, job), expected_permissions, f"{name}:{job}")

    def test_pypi_oidc_is_not_hidden_in_a_reusable_workflow(self) -> None:
        build = "\n".join(workflow_lines("py-reusable-build.yml"))
        legacy = "\n".join(workflow_lines("py-reusable-publish.yml"))
        self.assertNotIn("pypa/gh-action-pypi-publish", build)
        self.assertNotIn("pypa/gh-action-pypi-publish", legacy)
        self.assertIn("ref: ${{ github.sha }}", build)
        self.assertIn("allow_legacy_token:", legacy)
        self.assertRegex(legacy, r"allow_legacy_token:\n(?:.*\n){0,5}\s+default: false")
        self.assertIn("reject-unless-enabled:", legacy)
        self.assertIn("environment:\n      name: pypi-token-fallback", legacy)
        self.assertIn("PYPI_LEGACY_TOKEN_FALLBACK_ENABLED", legacy)

    def test_registry_credentials_are_not_available_to_build_jobs(self) -> None:
        for name, job in (
            ("py-reusable-build.yml", "build"),
            ("py-reusable-publish.yml", "build-package"),
            ("ts-reusable-publish.yml", "build"),
        ):
            text = "\n".join(job_block(name, job))
            self.assertNotIn("PYPI_TOKEN", text)
            self.assertNotIn("NPM_TOKEN", text)
            self.assertNotIn("id-token", text)

    def test_legacy_publish_requires_gate_before_upload(self) -> None:
        for name in ("py-reusable-publish.yml", "ts-reusable-publish.yml"):
            block = "\n".join(job_block(name, "publish-legacy-token"))
            self.assertIn("if: inputs.allow_legacy_token == true", block)
            self.assertIn("validate-legacy-publish-gate.sh", block)
            self.assertLess(block.index("Verify protected legacy token gate"), block.index("Publish with"))
        ts_legacy = "\n".join(job_block("ts-reusable-publish.yml", "publish-legacy-token"))
        self.assertIn("NPM_CONFIG_PROVENANCE: \"false\"", ts_legacy)
        self.assertNotIn("--provenance", ts_legacy)


if __name__ == "__main__":
    unittest.main()
