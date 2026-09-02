"""Behavior tests for the fail-closed npm trusted-publisher identity gate."""

from __future__ import annotations

import io
import json
import os
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "validate-npm-publish-identity.py"
WORKFLOW_REF = "trycua/cua/.github/workflows/cd-ts-core.yml@refs/tags/npm-core-v1.2.3"
BASE_ENV = {
    "GITHUB_REPOSITORY": "trycua/cua",
    "GITHUB_REF_TYPE": "tag",
    "GITHUB_REF_PROTECTED": "true",
    "GITHUB_REF_NAME": "npm-core-v1.2.3",
    "GITHUB_WORKFLOW_REF": WORKFLOW_REF,
    "TRUSTED_PUBLISHER_REPOSITORY": "trycua/cua",
    "TRUSTED_PACKAGE_NAME": "@trycua/core",
    "TRUSTED_PUBLISHER_WORKFLOW": "cd-ts-core.yml",
    "TRUSTED_TAG_PREFIX": "npm-core-v",
    "EXPECTED_TAG": "npm-core-v1.2.3",
    "EXPECTED_VERSION": "1.2.3",
}


class NpmPublishIdentityTests(unittest.TestCase):
    def run_archive(
        self,
        archives: list[tuple[str, list[tuple[str, bytes]]]],
        **environment_updates: str,
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            for archive_name, members in archives:
                with tarfile.open(artifact_directory / archive_name, "w:gz") as archive:
                    for member_name, payload in members:
                        member = tarfile.TarInfo(member_name)
                        member.size = len(payload)
                        archive.addfile(member, io.BytesIO(payload))

            environment = os.environ.copy()
            environment.update(BASE_ENV)
            environment.update(environment_updates)
            return subprocess.run(
                ["python3", str(SCRIPT), str(artifact_directory)],
                env=environment,
                capture_output=True,
                text=True,
                check=False,
            )

    def run_gate(
        self,
        package: dict[str, object],
        **environment_updates: str,
    ) -> subprocess.CompletedProcess[str]:
        return self.run_archive(
            [("package.tgz", [("package/package.json", json.dumps(package).encode())])],
            **environment_updates,
        )

    def valid_package(self) -> dict[str, object]:
        return {
            "name": "@trycua/core",
            "version": "1.2.3",
            "repository": {"type": "git", "url": "git+https://github.com/trycua/cua.git"},
        }

    def test_matching_identity_passes(self) -> None:
        result = self.run_gate(self.valid_package())
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_fixture_does_not_inherit_the_host_event(self) -> None:
        with patch.dict(os.environ, {"GITHUB_EVENT_NAME": "pull_request"}):
            result = self.run_gate(self.valid_package())
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_matching_unscoped_identity_passes_for_legacy_fallback(self) -> None:
        package = {
            "name": "cuabot",
            "version": "1.2.3",
            "repository": {"type": "git", "url": "https://github.com/trycua/cua"},
        }
        result = self.run_gate(
            package,
            TRUSTED_PACKAGE_NAME="cuabot",
            TRUSTED_PUBLISHER_WORKFLOW="cd-ts-cuabot.yml",
            TRUSTED_TAG_PREFIX="cuabot-v",
            EXPECTED_TAG="cuabot-v1.2.3",
            GITHUB_WORKFLOW_REF=(
                "trycua/cua/.github/workflows/cd-ts-cuabot.yml@refs/tags/cuabot-v1.2.3"
            ),
            GITHUB_REF_NAME="cuabot-v1.2.3",
            PUBLISH_PATH="legacy-token",
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_fork_repository_fails_closed(self) -> None:
        result = self.run_gate(self.valid_package(), GITHUB_REPOSITORY="manaflow-ai/cmux-cua")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("this fork must not publish", result.stderr)

    def test_fork_repository_fails_closed_for_legacy_fallback(self) -> None:
        result = self.run_gate(
            self.valid_package(),
            GITHUB_REPOSITORY="manaflow-ai/cmux-cua",
            PUBLISH_PATH="legacy-token",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("this fork must not publish", result.stderr)

    def test_branch_ref_fails_closed(self) -> None:
        result = self.run_gate(self.valid_package(), GITHUB_REF_TYPE="branch")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("only from a tag", result.stderr)

    def test_unprotected_tag_fails_closed(self) -> None:
        result = self.run_gate(self.valid_package(), GITHUB_REF_PROTECTED="false")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("protected release ref", result.stderr)

    def test_workflow_run_from_protected_main_passes(self) -> None:
        result = self.run_gate(
            self.valid_package(),
            GITHUB_EVENT_NAME="workflow_run",
            GITHUB_REF_TYPE="branch",
            GITHUB_REF_NAME="main",
            GITHUB_WORKFLOW_REF=(
                "trycua/cua/.github/workflows/cd-ts-core.yml@refs/heads/main"
            ),
            SOURCE_TAG="npm-core-v1.2.3",
            SOURCE_SHA="a" * 40,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_workflow_run_requires_exact_source_identity(self) -> None:
        result = self.run_gate(
            self.valid_package(),
            GITHUB_EVENT_NAME="workflow_run",
            GITHUB_REF_TYPE="branch",
            GITHUB_REF_NAME="main",
            GITHUB_WORKFLOW_REF=(
                "trycua/cua/.github/workflows/cd-ts-core.yml@refs/heads/main"
            ),
            SOURCE_TAG="npm-core-v9.9.9",
            SOURCE_SHA="not-a-sha",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("source tag", result.stderr)

    def test_wrong_tag_fails_closed(self) -> None:
        result = self.run_gate(self.valid_package(), GITHUB_REF_NAME="npm-core-v9.9.9")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be exactly", result.stderr)

    def test_wrong_workflow_tag_ref_fails_closed(self) -> None:
        result = self.run_gate(
            self.valid_package(),
            GITHUB_WORKFLOW_REF=(
                "trycua/cua/.github/workflows/cd-ts-core.yml@refs/heads/main"
            ),
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("protected release ref", result.stderr)

    def test_mismatched_tag_allowlist_fails_closed(self) -> None:
        result = self.run_gate(self.valid_package(), EXPECTED_TAG="npm-core-v9.9.9")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tag does not match", result.stderr)

    def test_invalid_expected_version_fails_closed(self) -> None:
        result = self.run_gate(self.valid_package(), EXPECTED_VERSION="1.2.3-rc.1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exact SemVer", result.stderr)

    def test_artifact_version_mismatch_fails_closed(self) -> None:
        package = self.valid_package()
        package["version"] = "9.9.9"
        result = self.run_gate(package)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("package version", result.stderr)

    def test_package_repository_mismatch_fails_closed(self) -> None:
        package = self.valid_package()
        package["repository"] = "https://github.com/manaflow-ai/cmux-cua"
        result = self.run_gate(package)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("package repository", result.stderr)

    def test_package_name_mismatch_fails_closed(self) -> None:
        package = self.valid_package()
        package["name"] = "@trycua/other"
        result = self.run_gate(package)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("package name", result.stderr)

    def test_package_scope_mismatch_fails_closed(self) -> None:
        package = self.valid_package()
        package["name"] = "@other/core"
        result = self.run_gate(
            package,
            TRUSTED_PACKAGE_NAME="@other/core",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("package scope", result.stderr)

    def test_workflow_mismatch_fails_closed(self) -> None:
        result = self.run_gate(self.valid_package(), TRUSTED_PUBLISHER_WORKFLOW="cd-ts-cli.yml")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("caller workflow", result.stderr)

    def test_non_github_repository_url_fails_closed(self) -> None:
        package = self.valid_package()
        package["repository"] = "https://example.com/trycua/cua"
        result = self.run_gate(package)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("HTTPS github.com URL", result.stderr)

    def test_multiple_artifacts_fail_closed(self) -> None:
        payload = json.dumps(self.valid_package()).encode()
        result = self.run_archive(
            [
                ("one.tgz", [("package/package.json", payload)]),
                ("two.tgz", [("package/package.json", payload)]),
            ]
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("expected exactly one artifact", result.stderr)

    def test_unsafe_archive_path_fails_closed(self) -> None:
        payload = json.dumps(self.valid_package()).encode()
        result = self.run_archive(
            [("package.tgz", [("../package.json", payload), ("package/package.json", payload)])]
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsafe archive path", result.stderr)

    def test_symlink_member_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            with tarfile.open(artifact_directory / "package.tgz", "w:gz") as archive:
                payload = json.dumps(self.valid_package()).encode()
                package_json = tarfile.TarInfo("package/package.json")
                package_json.size = len(payload)
                archive.addfile(package_json, io.BytesIO(payload))
                link = tarfile.TarInfo("package/link")
                link.type = tarfile.SYMTYPE
                link.linkname = "package/package.json"
                archive.addfile(link)
            environment = os.environ.copy()
            environment.update(BASE_ENV)
            result = subprocess.run(
                ["python3", str(SCRIPT), str(artifact_directory)],
                env=environment,
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("symlink or non-regular", result.stderr)

    def test_non_regular_member_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            with tarfile.open(artifact_directory / "package.tgz", "w:gz") as archive:
                payload = json.dumps(self.valid_package()).encode()
                package_json = tarfile.TarInfo("package/package.json")
                package_json.size = len(payload)
                archive.addfile(package_json, io.BytesIO(payload))
                fifo = tarfile.TarInfo("package/fifo")
                fifo.type = tarfile.FIFOTYPE
                archive.addfile(fifo)
            environment = os.environ.copy()
            environment.update(BASE_ENV)
            result = subprocess.run(
                ["python3", str(SCRIPT), str(artifact_directory)],
                env=environment,
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("symlink or non-regular", result.stderr)

    def test_directory_member_is_allowed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            with tarfile.open(artifact_directory / "package.tgz", "w:gz") as archive:
                package_directory = tarfile.TarInfo("package/")
                package_directory.type = tarfile.DIRTYPE
                archive.addfile(package_directory)
                payload = json.dumps(self.valid_package()).encode()
                package_json = tarfile.TarInfo("package/package.json")
                package_json.size = len(payload)
                archive.addfile(package_json, io.BytesIO(payload))
            environment = os.environ.copy()
            environment.update(BASE_ENV)
            result = subprocess.run(
                ["python3", str(SCRIPT), str(artifact_directory)],
                env=environment,
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_allowlist_fails_closed(self) -> None:
        result = self.run_gate(self.valid_package(), TRUSTED_PUBLISHER_REPOSITORY="")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("trusted publisher repository", result.stderr)

    def test_missing_tag_allowlist_fails_closed(self) -> None:
        result = self.run_gate(self.valid_package(), TRUSTED_TAG_PREFIX="")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("trusted release tag prefix", result.stderr)


if __name__ == "__main__":
    unittest.main()
