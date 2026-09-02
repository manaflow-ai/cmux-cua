"""Behavior tests for release tag provenance verification."""

from __future__ import annotations

import importlib.util
import pathlib
import sys
import unittest
from typing import Any


ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "verify_release_tag", ROOT / "verify_release_tag.py"
)
if SPEC is None or SPEC.loader is None:  # pragma: no cover - import guard
    raise RuntimeError("could not load release tag verifier")
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


EVENT_SHA = "a" * 40
MAIN_SHA = "b" * 40
OTHER_SHA = "c" * 40
REPOSITORY = "manaflow-ai/cmux-cua"
TRUSTED_REPOSITORY = "trycua/cua"
TAG = "cli-v1.2.3"
PREFIX = "cli-v"


class FakeApi:
    def __init__(self, responses: dict[str, dict[str, Any]]) -> None:
        self.responses = responses
        self.paths: list[str] = []

    def __call__(self, path: str) -> dict[str, Any]:
        self.paths.append(path)
        try:
            return self.responses[path]
        except KeyError as error:
            raise AssertionError(f"unexpected API path: {path}") from error


def lightweight_api(*, status: str = "ahead", target: str = EVENT_SHA) -> FakeApi:
    return FakeApi(
        {
            f"/repos/{REPOSITORY}/git/ref/tags/{TAG}": {
                "object": {"type": "commit", "sha": target}
            },
            f"/repos/{REPOSITORY}/branches/main": {
                "commit": {"sha": MAIN_SHA}
            },
            f"/repos/{REPOSITORY}/compare/{EVENT_SHA}...{MAIN_SHA}": {
                "status": status,
                "base_commit": {"sha": EVENT_SHA},
                "head_commit": {"sha": MAIN_SHA},
                "merge_base_commit": {"sha": EVENT_SHA},
            },
        }
    )


class VerifyReleaseTagTests(unittest.TestCase):
    def test_accepts_lightweight_tag_reachable_from_main(self) -> None:
        api = lightweight_api()

        result = MODULE.verify_release_tag(
            tag_prefix=PREFIX,
            tag=TAG,
            event_sha=EVENT_SHA,
            repository=REPOSITORY,
            api_get=api,
        )

        self.assertEqual(result.version, "1.2.3")
        self.assertEqual(result.tag_sha, EVENT_SHA)
        self.assertEqual(result.main_sha, MAIN_SHA)
        # The second ref read detects a force-move during the check.
        self.assertEqual(api.paths.count(f"/repos/{REPOSITORY}/git/ref/tags/{TAG}"), 2)

    def test_accepts_annotated_tag_that_points_to_event_commit(self) -> None:
        api = FakeApi(
            {
                f"/repos/{REPOSITORY}/git/ref/tags/{TAG}": {
                    "object": {"type": "tag", "sha": OTHER_SHA}
                },
                f"/repos/{REPOSITORY}/git/tags/{OTHER_SHA}": {
                    "object": {"type": "commit", "sha": EVENT_SHA}
                },
                f"/repos/{REPOSITORY}/branches/main": {
                    "commit": {"sha": MAIN_SHA}
                },
                f"/repos/{REPOSITORY}/compare/{EVENT_SHA}...{MAIN_SHA}": {
                    "status": "identical",
                    "base_commit": {"sha": EVENT_SHA},
                    "head_commit": {"sha": MAIN_SHA},
                    "merge_base_commit": {"sha": EVENT_SHA},
                },
            }
        )

        result = MODULE.verify_release_tag(
            tag_prefix=PREFIX,
            tag=TAG,
            event_sha=EVENT_SHA,
            repository=REPOSITORY,
            api_get=api,
        )

        self.assertEqual(result.tag_sha, EVENT_SHA)
        self.assertEqual(
            api.paths.count(f"/repos/{REPOSITORY}/git/tags/{OTHER_SHA}"), 2
        )

    def test_rejects_non_semver_tag(self) -> None:
        with self.assertRaisesRegex(MODULE.VerificationError, "exact SemVer"):
            MODULE.verify_release_tag(
                tag_prefix=PREFIX,
                tag="cli-v1.2.3-rc1",
                event_sha=EVENT_SHA,
                repository=REPOSITORY,
                api_get=lightweight_api(),
            )

    def test_rejects_tag_target_mismatch(self) -> None:
        with self.assertRaisesRegex(MODULE.VerificationError, "does not match event SHA"):
            MODULE.verify_release_tag(
                tag_prefix=PREFIX,
                tag=TAG,
                event_sha=EVENT_SHA,
                repository=REPOSITORY,
                api_get=lightweight_api(target=OTHER_SHA),
            )

    def test_rejects_commit_not_reachable_from_main(self) -> None:
        with self.assertRaisesRegex(MODULE.VerificationError, "not an ancestor"):
            MODULE.verify_release_tag(
                tag_prefix=PREFIX,
                tag=TAG,
                event_sha=EVENT_SHA,
                repository=REPOSITORY,
                api_get=lightweight_api(status="diverged"),
            )

    def test_rejects_comparison_with_unexpected_merge_base(self) -> None:
        api = lightweight_api()
        compare_path = f"/repos/{REPOSITORY}/compare/{EVENT_SHA}...{MAIN_SHA}"
        api.responses[compare_path]["merge_base_commit"] = {"sha": OTHER_SHA}

        with self.assertRaisesRegex(MODULE.VerificationError, "merge base"):
            MODULE.verify_release_tag(
                tag_prefix=PREFIX,
                tag=TAG,
                event_sha=EVENT_SHA,
                repository=REPOSITORY,
                api_get=api,
            )

    def test_rejects_tag_moved_during_check(self) -> None:
        api = lightweight_api()
        tag_path = f"/repos/{REPOSITORY}/git/ref/tags/{TAG}"
        calls = 0

        def moving_api(path: str) -> dict[str, Any]:
            nonlocal calls
            calls += 1
            response = api(path)
            if path == tag_path and api.paths.count(tag_path) == 2:
                return {"object": {"type": "commit", "sha": OTHER_SHA}}
            return response

        with self.assertRaisesRegex(MODULE.VerificationError, "moved"):
            MODULE.verify_release_tag(
                tag_prefix=PREFIX,
                tag=TAG,
                event_sha=EVENT_SHA,
                repository=REPOSITORY,
                api_get=moving_api,
            )
        self.assertGreater(calls, 0)

    def test_rejects_untrusted_repository(self) -> None:
        with self.assertRaisesRegex(
            MODULE.VerificationError, "not the trusted release repository"
        ):
            MODULE.verify_release_tag(
                tag_prefix=PREFIX,
                tag=TAG,
                event_sha=EVENT_SHA,
                repository=REPOSITORY,
                trusted_repository=TRUSTED_REPOSITORY,
                api_get=lightweight_api(),
            )

    def test_rejects_base_branch_moved_during_check(self) -> None:
        api = lightweight_api()
        branch_path = f"/repos/{REPOSITORY}/branches/main"
        branch_reads = 0

        def moving_api(path: str) -> dict[str, Any]:
            nonlocal branch_reads
            response = api(path)
            if path == branch_path:
                branch_reads += 1
                if branch_reads == 2:
                    return {"commit": {"sha": OTHER_SHA}}
            return response

        with self.assertRaisesRegex(MODULE.VerificationError, "moved"):
            MODULE.verify_release_tag(
                tag_prefix=PREFIX,
                tag=TAG,
                event_sha=EVENT_SHA,
                repository=REPOSITORY,
                api_get=moving_api,
            )


if __name__ == "__main__":
    unittest.main()
