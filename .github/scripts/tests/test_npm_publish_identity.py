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


SCRIPT = Path(__file__).resolve().parents[1] / "validate-npm-publish-identity.py"
WORKFLOW_REF = "trycua/cua/.github/workflows/cd-ts-core.yml@refs/tags/ts-core-v1.2.3"
BASE_ENV = {
    "GITHUB_REPOSITORY": "trycua/cua",
    "GITHUB_REF_TYPE": "tag",
    "GITHUB_REF_PROTECTED": "true",
    "GITHUB_WORKFLOW_REF": WORKFLOW_REF,
    "TRUSTED_PUBLISHER_REPOSITORY": "trycua/cua",
    "TRUSTED_PACKAGE_NAME": "@trycua/core",
    "TRUSTED_PUBLISHER_WORKFLOW": "cd-ts-core.yml",
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
            GITHUB_WORKFLOW_REF=(
                "trycua/cua/.github/workflows/cd-ts-cuabot.yml@refs/tags/ts-cuabot-v1.2.3"
            ),
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
        self.assertIn("protected tag", result.stderr)

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

    def test_missing_allowlist_fails_closed(self) -> None:
        result = self.run_gate(self.valid_package(), TRUSTED_PUBLISHER_REPOSITORY="")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("trusted publisher repository", result.stderr)


if __name__ == "__main__":
    unittest.main()
