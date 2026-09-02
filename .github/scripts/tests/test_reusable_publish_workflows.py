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
WORKFLOW_README = WORKFLOWS / "README.md"


def workflow_lines(name: str) -> list[str]:
    return (WORKFLOWS / name).read_text(encoding="utf-8").splitlines()


def workflow_text(name: str) -> str:
    return (WORKFLOWS / name).read_text(encoding="utf-8")


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
                "validate-publisher-identity": {"actions": "read", "contents": "read"},
                "publish-legacy-token": {"actions": "read", "contents": "read"},
            },
            "ts-reusable-publish.yml": {
                "build": {"contents": "read"},
                "validate-publisher-identity": {"actions": "read", "contents": "read"},
                "publish-oidc": {"actions": "read", "id-token": "write"},
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
        self.assertIn("ref: ${{ inputs.source_sha || github.sha }}", build)
        self.assertIn("persist-credentials: false", build)
        self.assertIn("allow_legacy_token:", legacy)
        self.assertRegex(legacy, r"allow_legacy_token:\n(?:.*\n){0,5}\s+default: false")
        self.assertIn("reject-unless-enabled:", legacy)
        self.assertIn("environment:\n      name: pypi-token-fallback", legacy)
        self.assertIn("PYPI_LEGACY_TOKEN_FALLBACK_ENABLED", legacy)
        for input_name in (
            "trusted_publisher_repository:",
            "trusted_package_name:",
            "trusted_publisher_workflow:",
            "trusted_tag_prefix:",
        ):
            self.assertIn(input_name, legacy)
        for input_name in (
            "trusted_publisher_repository",
            "trusted_package_name",
            "trusted_publisher_workflow",
            "trusted_tag_prefix",
            "source_sha",
            "source_tag",
        ):
            self.assertRegex(
                legacy,
                rf"{input_name}:\n\s+description:.*\n\s+required: true",
            )
        identity = "\n".join(job_block("py-reusable-publish.yml", "validate-publisher-identity"))
        self.assertIn("validate-pypi-publish-identity.py", identity)
        self.assertIn("GITHUB_REF_NAME: ${{ github.ref_name }}", identity)
        self.assertIn("TRUSTED_TAG_PREFIX", identity)
        self.assertIn("EXPECTED_VERSION", identity)
        self.assertNotIn("PYPI_TOKEN", identity)

        ts = "\n".join(workflow_lines("ts-reusable-publish.yml"))
        for input_name in (
            "trusted_publisher_repository:",
            "trusted_package_name:",
            "trusted_publisher_workflow:",
            "trusted_tag_prefix:",
            "expected_tag:",
            "expected_version:",
            "source_sha:",
            "source_tag:",
        ):
            self.assertIn(input_name, ts)
        for input_name in (
            "trusted_publisher_repository",
            "trusted_package_name",
            "trusted_publisher_workflow",
            "trusted_tag_prefix",
            "expected_tag",
            "expected_version",
            "source_sha",
            "source_tag",
        ):
            self.assertRegex(
                ts,
                rf"{input_name}:\n\s+description:.*\n\s+required: true",
            )
        self.assertIn("environment:\n      name: npm", ts)
        self.assertIn("validate-publisher-identity", ts)
        validator = "\n".join(job_block("ts-reusable-publish.yml", "validate-publisher-identity"))
        self.assertIn("needs.build.outputs.should_publish == 'true'", validator)
        self.assertIn("validate-npm-publish-identity.py", validator)
        self.assertIn("validate_publish_artifacts.py", validator)
        self.assertIn("TRUSTED_PUBLISHER_REPOSITORY", validator)
        self.assertIn("GITHUB_REF_TYPE: ${{ github.ref_type }}", validator)
        self.assertIn("GITHUB_REF_PROTECTED: ${{ github.ref_protected }}", validator)
        self.assertIn("GITHUB_REF_NAME: ${{ github.ref_name }}", validator)
        self.assertIn("TRUSTED_TAG_PREFIX", validator)
        self.assertIn("EXPECTED_TAG", validator)
        self.assertIn("EXPECTED_VERSION", validator)

    def test_python_build_keeps_ci_callers_compatible(self) -> None:
        build = "\n".join(workflow_lines("py-reusable-build.yml"))
        self.assertRegex(
            build,
            r"version:\n\s+description: .*\n\s+required: false\n\s+type: string\n\s+default: \"\"",
        )
        self.assertIn("if: inputs.version != ''", build)

    def test_typescript_release_toolchain_is_pinned(self) -> None:
        publish = "\n".join(workflow_lines("ts-reusable-publish.yml"))
        self.assertIn('bun-version: "1.1.38"', publish)
        self.assertNotIn("bun-version: latest", publish)

    def test_typescript_identity_contract_is_documented(self) -> None:
        readme = WORKFLOW_README.read_text(encoding="utf-8")
        for field in (
            "trusted_publisher_repository",
            "trusted_package_name",
            "trusted_publisher_workflow",
            "trusted_tag_prefix",
            "expected_tag",
            "expected_version",
        ):
            self.assertIn(field, readme)
        self.assertIn("trycua/cua", readme)
        self.assertIn("fails closed", readme)

    def test_registry_credentials_are_not_available_to_build_jobs(self) -> None:
        for name, job in (
            ("py-reusable-build.yml", "build"),
            ("py-reusable-publish.yml", "build-package"),
            ("ts-reusable-publish.yml", "build"),
            ("ts-reusable-publish.yml", "validate-publisher-identity"),
        ):
            text = "\n".join(job_block(name, job))
            self.assertNotIn("PYPI_TOKEN", text)
            self.assertNotIn("NPM_TOKEN", text)
            self.assertNotIn("id-token", text)
            if job == "build":
                self.assertIn("persist-credentials: false", text)

    def test_credential_gates_are_loaded_from_protected_main(self) -> None:
        for name, jobs in (
            ("py-reusable-publish.yml", ("validate-publisher-identity", "publish-legacy-token")),
            ("ts-reusable-publish.yml", ("validate-publisher-identity", "publish-legacy-token")),
        ):
            for job in jobs:
                block = "\n".join(job_block(name, job))
                self.assertIn("repository: ${{ github.repository }}", block, f"{name}:{job}")
                self.assertIn("ref: main", block, f"{name}:{job}")
                self.assertIn("path: trusted-release", block, f"{name}:{job}")

        verifier = workflow_text("verify-release-tag.yml")
        self.assertIn("repository: ${{ github.repository }}", verifier)
        self.assertIn("ref: main", verifier)
        self.assertIn("path: trusted-release", verifier)
        self.assertIn("trusted-release/.github/scripts/verify_release_tag.py", verifier)

    def test_legacy_publish_requires_gate_before_upload(self) -> None:
        for name in ("py-reusable-publish.yml", "ts-reusable-publish.yml"):
            block = "\n".join(job_block(name, "publish-legacy-token"))
            self.assertIn("inputs.allow_legacy_token == true", block)
            self.assertIn("validate-legacy-publish-gate.sh", block)
        self.assertLess(
            block.index("Verify protected legacy token gate"),
            block.index("Publish with"),
        )
        py_legacy = "\n".join(job_block("py-reusable-publish.yml", "publish-legacy-token"))
        py_workflow = "\n".join(workflow_lines("py-reusable-publish.yml"))
        self.assertIn("validate_publish_artifacts.py", py_legacy)
        self.assertIn("--suffix .whl --suffix .tar.gz", py_legacy)
        self.assertIn("--expected-package", py_legacy)
        self.assertIn("--expected-version", py_legacy)
        self.assertIn("--max-files 2", py_legacy)
        self.assertIn("--repository-url https://upload.pypi.org/legacy/", py_legacy)
        self.assertIn("TWINE_PASSWORD: ${{ secrets.PYPI_TOKEN }}", py_legacy)
        self.assertLess(
            py_legacy.index("Reject unsafe package artifacts"),
            py_legacy.index("Verify protected legacy token gate"),
        )
        self.assertLess(
            py_legacy.index("Verify protected legacy token gate"),
            py_legacy.index("Publish with"),
        )
        self.assertIn("needs.validate-publisher-identity.result == 'success'", py_legacy)
        self.assertIn("base_package_name || inputs.trusted_package_name", py_workflow)
        ts_legacy = "\n".join(job_block("ts-reusable-publish.yml", "publish-legacy-token"))
        self.assertLess(
            ts_legacy.index("Revalidate package identity before token use"),
            ts_legacy.index("Verify protected legacy token gate"),
        )
        self.assertIn("NPM_CONFIG_PROVENANCE: \"false\"", ts_legacy)
        self.assertIn("--registry=https://registry.npmjs.org/", ts_legacy)
        self.assertNotIn("--provenance", ts_legacy)
        ts_oidc = "\n".join(job_block("ts-reusable-publish.yml", "publish-oidc"))
        self.assertIn("id-token: write", ts_oidc)
        self.assertIn("inputs.allow_legacy_token == false", ts_oidc)
        self.assertIn("needs.validate-publisher-identity.result == 'success'", ts_oidc)
        self.assertIn("--registry=https://registry.npmjs.org/", ts_oidc)
        self.assertIn(
            "environment:\n      name: npm",
            "\n".join(workflow_lines("ts-reusable-publish.yml")),
        )
        self.assertNotIn("NPM_TOKEN", ts_oidc)
        ts_legacy = "\n".join(job_block("ts-reusable-publish.yml", "publish-legacy-token"))
        self.assertIn("needs.validate-publisher-identity.result == 'success'", ts_legacy)


if __name__ == "__main__":
    unittest.main()
