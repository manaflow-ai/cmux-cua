"""Behavior and policy tests for privileged release workflows."""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]
PROTECTED_TAG = REPO_ROOT / ".github/scripts/validate_protected_tag.py"
TRUSTED_MAIN = REPO_ROOT / ".github/scripts/validate_trusted_main.py"


def git(cwd: Path, *args: str) -> str:
    environment = os.environ.copy()
    # The developer machine enables global commit/tag signing. Tests use
    # throwaway unsigned commits so they never open an interactive editor.
    environment["GIT_CONFIG_GLOBAL"] = "/dev/null"
    result = subprocess.run(
        ["git", *args],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
        env=environment,
    )
    return result.stdout.strip()


def fixture_repository() -> tuple[tempfile.TemporaryDirectory[str], Path, str]:
    temporary_directory = tempfile.TemporaryDirectory()
    root = Path(temporary_directory.name)
    bare = root / "origin.git"
    work = root / "work"
    environment = os.environ.copy()
    environment["GIT_CONFIG_GLOBAL"] = "/dev/null"
    subprocess.run(
        ["git", "init", "--bare", str(bare)], check=True, capture_output=True, env=environment
    )
    subprocess.run(
        ["git", "init", "-b", "main", str(work)], check=True, capture_output=True, env=environment
    )
    git(work, "config", "user.name", "Test")
    git(work, "config", "user.email", "test@example.invalid")
    (work / "README").write_text("trusted\n", encoding="utf-8")
    git(work, "add", "README")
    git(work, "commit", "-m", "initial")
    git(work, "remote", "add", "origin", str(bare))
    git(work, "push", "origin", "main")
    commit = git(work, "rev-parse", "HEAD")
    return temporary_directory, work, commit


class TestProtectedTagValidation(unittest.TestCase):
    def run_validator(self, work: Path, **values: str) -> subprocess.CompletedProcess[str]:
        output = work / "outputs.txt"
        environment = os.environ.copy()
        environment["GIT_CONFIG_GLOBAL"] = "/dev/null"
        environment.update(
            {
                "GITHUB_OUTPUT": str(output),
                "REPOSITORY": "manaflow-ai/cmux-cua",
                "EXPECTED_REPOSITORY": "manaflow-ai/cmux-cua",
                "EVENT_NAME": "push",
                "REF": "refs/tags/lume-v1.2.3",
                "REF_PROTECTED": "true",
                "SHA": values.get("SHA", git(work, "rev-parse", "HEAD")),
                "TAG_PREFIX": "lume-v",
                "ALLOWED_EVENTS": "push,workflow_call",
            }
        )
        environment.update(values)
        return subprocess.run(
            ["python3", str(PROTECTED_TAG)],
            cwd=work,
            env=environment,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_accepts_tag_on_main_and_emits_validated_outputs(self) -> None:
        temporary_directory, work, commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)
        git(work, "tag", "lume-v1.2.3", commit)
        git(work, "push", "origin", "refs/tags/lume-v1.2.3")

        result = self.run_validator(work, SHA=commit)

        self.assertEqual(result.returncode, 0, result.stderr)
        outputs = (work / "outputs.txt").read_text(encoding="utf-8")
        self.assertIn("tag=lume-v1.2.3", outputs)
        self.assertIn(f"commit={commit}", outputs)

    def test_rejects_tag_not_reachable_from_main(self) -> None:
        temporary_directory, work, main_commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)
        git(work, "checkout", "-b", "release-staging")
        (work / "README").write_text("untrusted\n", encoding="utf-8")
        git(work, "add", "README")
        git(work, "commit", "-m", "untrusted")
        tag_commit = git(work, "rev-parse", "HEAD")
        git(work, "tag", "lume-v9.9.9", tag_commit)
        git(work, "push", "origin", "refs/tags/lume-v9.9.9")

        result = self.run_validator(
            work,
            SHA=tag_commit,
            REF="refs/tags/lume-v9.9.9",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not an ancestor of main", result.stderr)
        self.assertNotEqual(tag_commit, main_commit)

    def test_rejects_dispatch_event_even_with_a_tag_input(self) -> None:
        temporary_directory, work, commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)
        git(work, "tag", "lume-v1.2.3", commit)
        git(work, "push", "origin", "refs/tags/lume-v1.2.3")

        result = self.run_validator(work, EVENT_NAME="workflow_dispatch", SHA=commit)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not allowed", result.stderr)

    def test_accepts_workflow_call_when_input_matches_tag(self) -> None:
        temporary_directory, work, commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)
        git(work, "tag", "lume-v1.2.3", commit)
        git(work, "push", "origin", "refs/tags/lume-v1.2.3")

        result = self.run_validator(
            work,
            EVENT_NAME="workflow_call",
            EXPECTED_VERSION="1.2.3",
            SHA=commit,
        )

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_workflow_call_when_input_differs_from_tag(self) -> None:
        temporary_directory, work, commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)
        git(work, "tag", "lume-v1.2.3", commit)
        git(work, "push", "origin", "refs/tags/lume-v1.2.3")

        result = self.run_validator(
            work,
            EVENT_NAME="workflow_call",
            EXPECTED_VERSION="9.9.9",
            SHA=commit,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match tag version", result.stderr)

    def test_rejects_unprotected_tag(self) -> None:
        temporary_directory, work, commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)
        git(work, "tag", "lume-v1.2.3", commit)
        git(work, "push", "origin", "refs/tags/lume-v1.2.3")

        result = self.run_validator(work, SHA=commit, REF_PROTECTED="false")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("protected tag rule", result.stderr)

    def test_rejects_tag_event_sha_mismatch(self) -> None:
        temporary_directory, work, commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)
        git(work, "tag", "lume-v1.2.3", commit)
        git(work, "push", "origin", "refs/tags/lume-v1.2.3")

        result = self.run_validator(work, SHA="0" * 40)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rev-parse", result.stderr)


class TestTrustedMainValidation(unittest.TestCase):
    def run_validator(self, work: Path, **values: str) -> subprocess.CompletedProcess[str]:
        output = work / "outputs.txt"
        commit = git(work, "rev-parse", "HEAD")
        environment = os.environ.copy()
        environment["GIT_CONFIG_GLOBAL"] = "/dev/null"
        environment.update(
            {
                "GITHUB_OUTPUT": str(output),
                "REPOSITORY": "manaflow-ai/cmux-cua",
                "EXPECTED_REPOSITORY": "manaflow-ai/cmux-cua",
                "EVENT_NAME": "schedule",
                "TRUSTED_SHA": commit,
                "SOURCE_EVENT": "",
                "SOURCE_CONCLUSION": "",
                "SOURCE_BRANCH": "",
                "SOURCE_REPOSITORY": "",
                "SOURCE_SHA": "",
                "TRUSTED_REF_PROTECTED": "true",
            }
        )
        environment.update(values)
        return subprocess.run(
            ["python3", str(TRUSTED_MAIN)],
            cwd=work,
            env=environment,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_accepts_schedule_at_current_main(self) -> None:
        temporary_directory, work, commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)

        result = self.run_validator(work, TRUSTED_SHA=commit)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"commit={commit}", (work / "outputs.txt").read_text(encoding="utf-8"))

    def test_rejects_manual_request_from_feature_branch(self) -> None:
        temporary_directory, work, commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)

        result = self.run_validator(
            work,
            EVENT_NAME="workflow_run",
            SOURCE_EVENT="workflow_dispatch",
            SOURCE_CONCLUSION="success",
            SOURCE_BRANCH="feature/untrusted",
            SOURCE_REPOSITORY="manaflow-ai/cmux-cua",
            SOURCE_SHA=commit,
            TRUSTED_SHA=commit,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("protected main", result.stderr)

    def test_accepts_successful_manual_request_from_current_main(self) -> None:
        temporary_directory, work, commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)

        result = self.run_validator(
            work,
            EVENT_NAME="workflow_run",
            SOURCE_EVENT="workflow_dispatch",
            SOURCE_CONCLUSION="success",
            SOURCE_BRANCH="main",
            SOURCE_REPOSITORY="manaflow-ai/cmux-cua",
            SOURCE_SHA=commit,
            TRUSTED_SHA=commit,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"commit={commit}", (work / "outputs.txt").read_text(encoding="utf-8"))

    def test_rejects_failed_manual_request(self) -> None:
        temporary_directory, work, commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)

        result = self.run_validator(
            work,
            EVENT_NAME="workflow_run",
            SOURCE_EVENT="workflow_dispatch",
            SOURCE_CONCLUSION="failure",
            SOURCE_BRANCH="main",
            SOURCE_REPOSITORY="manaflow-ai/cmux-cua",
            SOURCE_SHA=commit,
            TRUSTED_SHA=commit,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("did not complete successfully", result.stderr)

    def test_rejects_manual_request_from_another_repository(self) -> None:
        temporary_directory, work, commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)

        result = self.run_validator(
            work,
            EVENT_NAME="workflow_run",
            SOURCE_EVENT="workflow_dispatch",
            SOURCE_CONCLUSION="success",
            SOURCE_BRANCH="main",
            SOURCE_REPOSITORY="attacker/example",
            SOURCE_SHA=commit,
            TRUSTED_SHA=commit,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("another repository", result.stderr)

    def test_rejects_unprotected_schedule(self) -> None:
        temporary_directory, work, commit = fixture_repository()
        self.addCleanup(temporary_directory.cleanup)

        result = self.run_validator(
            work,
            TRUSTED_SHA=commit,
            TRUSTED_REF_PROTECTED="false",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("protected branch rule", result.stderr)


class TestWorkflowContracts(unittest.TestCase):
    @staticmethod
    def workflow_text(relative_path: str) -> str:
        return (REPO_ROOT / relative_path).read_text(encoding="utf-8")

    def assert_top_level_permissions_empty(self, text: str, path: str) -> None:
        self.assertRegex(text, r"(?m)^permissions:\s*\{\}\s*$", path)

    def assert_no_top_level_env(self, text: str, path: str) -> None:
        self.assertNotRegex(text, r"(?m)^env:", path)

    def assert_no_manual_trigger(self, text: str, path: str) -> None:
        self.assertNotRegex(text, r"(?m)^  workflow_dispatch:", path)

    @staticmethod
    def job_block(text: str, job_id: str) -> str:
        match = re.search(
            rf"(?ms)^  {re.escape(job_id)}:\n(?:(?!^  [A-Za-z0-9_-]+:).)*",
            text,
        )
        if match is None:
            raise AssertionError(f"workflow job {job_id!r} is missing")
        return match.group(0)

    def test_privileged_workflows_have_no_manual_trigger_or_global_secrets(self) -> None:
        for path in (
            ".github/workflows/cd-swift-lume.yml",
            ".github/workflows/cd-rust-cua-driver.yml",
            ".github/workflows/ci-test-models.yml",
            ".github/workflows/ci-cold-start-benchmark.yml",
        ):
            text = self.workflow_text(path)
            self.assert_no_manual_trigger(text, path)
            self.assert_top_level_permissions_empty(text, path)
            self.assert_no_top_level_env(text, path)
            self.assertIn("REF_PROTECTED", text, path)

    def test_release_validators_execute_trusted_main_code(self) -> None:
        for path in (
            ".github/workflows/cd-swift-lume.yml",
            ".github/workflows/cd-rust-cua-driver.yml",
        ):
            text = self.workflow_text(path)
            validator_job = re.search(
                r"(?ms)^  validate-release:\n(?:(?!^  [A-Za-z0-9_-]+:).)*",
                text,
            )
            self.assertIsNotNone(validator_job, path)
            self.assertRegex(validator_job.group(0), r"ref: refs/heads/main", path)

    def test_privileged_jobs_require_named_environments(self) -> None:
        expected = {
            ".github/workflows/cd-swift-lume.yml": {
                "notarize": "lume-release",
            },
            ".github/workflows/cd-rust-cua-driver.yml": {
                "build-macos-universal": "cua-driver-release",
                "release": "cua-driver-release",
            },
            ".github/workflows/ci-test-models.yml": {
                "test-all-models": "model-tests",
                "test-summary": "model-tests",
            },
            ".github/workflows/ci-cold-start-benchmark.yml": {
                "benchmark": "cold-start-benchmark",
            },
        }
        for path, jobs in expected.items():
            text = self.workflow_text(path)
            for job_id, environment in jobs.items():
                block = self.job_block(text, job_id)
                self.assertRegex(block, rf"(?m)^    environment: {re.escape(environment)}$", path)

    def test_privileged_checkouts_use_validated_commit(self) -> None:
        for path in (
            ".github/workflows/cd-swift-lume.yml",
            ".github/workflows/cd-rust-cua-driver.yml",
        ):
            text = self.workflow_text(path)
            self.assertRegex(
                text,
                r"ref: \$\{\{ needs\.validate-release\.outputs\.commit \}\}",
                path,
            )
        for path in (
            ".github/workflows/ci-test-models.yml",
            ".github/workflows/ci-cold-start-benchmark.yml",
        ):
            text = self.workflow_text(path)
            self.assertRegex(
                text,
                r"ref: \$\{\{ needs\.validate-source\.outputs\.commit \}\}",
                path,
            )

    def test_all_action_refs_are_immutable_commits(self) -> None:
        for path in (
            ".github/workflows/cd-swift-lume.yml",
            ".github/workflows/ci-swift-lume-manual.yml",
            ".github/workflows/cd-rust-cua-driver.yml",
            ".github/workflows/ci-rust-cua-driver-manual.yml",
            ".github/workflows/ci-test-models.yml",
            ".github/workflows/ci-test-models-request.yml",
            ".github/workflows/ci-cold-start-benchmark.yml",
            ".github/workflows/ci-cold-start-benchmark-request.yml",
        ):
            text = (REPO_ROOT / path).read_text(encoding="utf-8")
            for line in text.splitlines():
                if "uses:" not in line or line.lstrip().startswith("#"):
                    continue
                reference = line.split("uses:", 1)[1].split("#", 1)[0].strip()
                self.assertRegex(reference, r"@[0-9a-f]{40}\Z", f"{path}: {line}")

    def test_manual_workflows_have_no_secret_or_write_references(self) -> None:
        for path in (
            ".github/workflows/ci-swift-lume-manual.yml",
            ".github/workflows/ci-rust-cua-driver-manual.yml",
            ".github/workflows/ci-test-models-request.yml",
            ".github/workflows/ci-cold-start-benchmark-request.yml",
        ):
            text = self.workflow_text(path)
            self.assertNotIn("secrets.", text, path)
            self.assertNotIn("contents: write", text, path)
            self.assert_top_level_permissions_empty(text, path)


if __name__ == "__main__":
    unittest.main()
