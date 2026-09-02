"""Behavior tests for the fail-closed PyPI token publisher identity gate."""

from __future__ import annotations

import io
import os
import subprocess
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "validate-pypi-publish-identity.py"
BASE_ENV = {
    "GITHUB_EVENT_NAME": "push",
    "GITHUB_REPOSITORY": "trycua/cua",
    "GITHUB_REF_TYPE": "tag",
    "GITHUB_REF_PROTECTED": "true",
    "GITHUB_REF_NAME": "agent-v1.2.3",
    "GITHUB_WORKFLOW_REF": (
        "trycua/cua/.github/workflows/cd-py-agent.yml@refs/tags/agent-v1.2.3"
    ),
    "TRUSTED_PUBLISHER_REPOSITORY": "trycua/cua",
    "TRUSTED_PACKAGE_NAME": "cua-agent",
    "TRUSTED_PUBLISHER_WORKFLOW": "cd-py-agent.yml",
    "TRUSTED_TAG_PREFIX": "agent-v",
    "EXPECTED_VERSION": "1.2.3",
}


class PypiPublishIdentityTests(unittest.TestCase):
    @staticmethod
    def write_wheel(path: Path, package: str = "cua-agent", version: str = "1.2.3") -> None:
        metadata = f"Metadata-Version: 2.1\nName: {package}\nVersion: {version}\n\n".encode()
        with zipfile.ZipFile(path, "w") as archive:
            archive.writestr("cua_agent-1.2.3.dist-info/METADATA", metadata)

    @staticmethod
    def write_sdist(path: Path, package: str = "cua-agent", version: str = "1.2.3") -> None:
        metadata = f"Metadata-Version: 2.1\nName: {package}\nVersion: {version}\n\n".encode()
        with tarfile.open(path, "w:gz") as archive:
            directory = tarfile.TarInfo("cua-agent-1.2.3/")
            directory.type = tarfile.DIRTYPE
            archive.addfile(directory)
            member = tarfile.TarInfo("cua-agent-1.2.3/PKG-INFO")
            member.size = len(metadata)
            archive.addfile(member, io.BytesIO(metadata))

    def run_gate(self, **updates: str) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as directory:
            artifacts = Path(directory)
            self.write_wheel(artifacts / "cua-agent-1.2.3-py3-none-any.whl")
            environment = os.environ.copy()
            environment.update(BASE_ENV)
            environment.update(updates)
            return subprocess.run(
                ["python3", str(SCRIPT), str(artifacts)],
                env=environment,
                capture_output=True,
                text=True,
                check=False,
            )

    def test_matching_release_passes(self) -> None:
        result = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_fixture_does_not_inherit_the_host_event(self) -> None:
        with patch.dict(os.environ, {"GITHUB_EVENT_NAME": "pull_request"}):
            result = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_fork_fails_closed(self) -> None:
        result = self.run_gate(GITHUB_REPOSITORY="manaflow-ai/cmux-cua")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("this fork must not publish", result.stderr)

    def test_branch_ref_fails_closed(self) -> None:
        result = self.run_gate(GITHUB_REF_TYPE="branch")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("only from a tag", result.stderr)

    def test_unprotected_tag_fails_closed(self) -> None:
        result = self.run_gate(GITHUB_REF_PROTECTED="false")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("protected release ref", result.stderr)

    def test_workflow_run_from_protected_main_passes(self) -> None:
        result = self.run_gate(
            GITHUB_EVENT_NAME="workflow_run",
            GITHUB_REF_TYPE="branch",
            GITHUB_REF_NAME="main",
            GITHUB_WORKFLOW_REF=(
                "trycua/cua/.github/workflows/cd-py-agent.yml@refs/heads/main"
            ),
            SOURCE_TAG="agent-v1.2.3",
            SOURCE_SHA="a" * 40,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_workflow_run_requires_exact_source_identity(self) -> None:
        result = self.run_gate(
            GITHUB_EVENT_NAME="workflow_run",
            GITHUB_REF_TYPE="branch",
            GITHUB_REF_NAME="main",
            GITHUB_WORKFLOW_REF=(
                "trycua/cua/.github/workflows/cd-py-agent.yml@refs/heads/main"
            ),
            SOURCE_TAG="agent-v9.9.9",
            SOURCE_SHA="not-a-sha",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("source tag", result.stderr)

    def test_wrong_tag_fails_closed(self) -> None:
        result = self.run_gate(GITHUB_REF_NAME="agent-v9.9.9")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be exactly", result.stderr)

    def test_wrong_workflow_fails_closed(self) -> None:
        result = self.run_gate(
            GITHUB_WORKFLOW_REF=(
                "trycua/cua/.github/workflows/cd-py-other.yml@refs/tags/agent-v1.2.3"
            )
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("caller workflow", result.stderr)

    def test_wrong_workflow_ref_tag_fails_closed(self) -> None:
        result = self.run_gate(
            GITHUB_WORKFLOW_REF=(
                "trycua/cua/.github/workflows/cd-py-agent.yml@refs/heads/main"
            )
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("protected release ref", result.stderr)

    def test_wrong_package_fails_closed(self) -> None:
        result = self.run_gate(TRUSTED_PACKAGE_NAME="cua-other")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("package", result.stderr)

    def test_wrong_version_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifacts = Path(directory)
            self.write_wheel(artifacts / "cua-agent-1.2.3-py3-none-any.whl", version="2.0.0")
            environment = os.environ.copy()
            environment.update(BASE_ENV)
            result = subprocess.run(
                ["python3", str(SCRIPT), str(artifacts)],
                env=environment,
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("version", result.stderr)

    def test_invalid_expected_version_fails_closed(self) -> None:
        result = self.run_gate(EXPECTED_VERSION="1.2.3-rc.1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exact SemVer", result.stderr)

    def test_source_and_wheel_pass_together(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifacts = Path(directory)
            self.write_wheel(artifacts / "cua-agent-1.2.3-py3-none-any.whl")
            self.write_sdist(artifacts / "cua-agent-1.2.3.tar.gz")
            environment = os.environ.copy()
            environment.update(BASE_ENV)
            result = subprocess.run(
                ["python3", str(SCRIPT), str(artifacts)],
                env=environment,
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_allowlist_fails_closed(self) -> None:
        result = self.run_gate(TRUSTED_PUBLISHER_REPOSITORY="")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("trusted publisher repository", result.stderr)


if __name__ == "__main__":
    unittest.main()
