"""Security contracts for release and credential-bearing caller workflows."""

from __future__ import annotations

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
WORKFLOWS = ROOT / ".github" / "workflows"

PY_CALLERS = sorted(
    path
    for path in WORKFLOWS.glob("cd-py-*.yml")
    if path.name != "cd-py-cua-driver.yml"
)
TS_CALLERS = sorted(WORKFLOWS.glob("cd-ts-*.yml"))
CONTAINER_CALLERS = sorted(WORKFLOWS.glob("cd-container-*.yml"))
RELEASE_CALLERS = PY_CALLERS + TS_CALLERS + CONTAINER_CALLERS
DOCKER_PUBLISH = WORKFLOWS / "docker-reusable-publish.yml"
DOCKER_BUILD = WORKFLOWS / "docker-reusable-build.yml"
RELEASE_REUSABLE = WORKFLOWS / "release-github-reusable.yml"

FULL_SHA_ACTION = re.compile(r"uses:\s+[^\s@]+@([0-9a-f]{40})(?:\s+#.*)?$")


def text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def job_block(source: str, name: str) -> str:
    match = re.search(rf"^  {re.escape(name)}:\n", source, re.MULTILINE)
    if match is None:
        raise AssertionError(f"missing job {name}")
    end = re.search(r"^  [A-Za-z0-9_-]+:\n", source[match.end() :], re.MULTILINE)
    stop = match.end() + end.start() if end else len(source)
    return source[match.start() : stop]


class ReleaseCallerSecurityTests(unittest.TestCase):
    def test_callers_are_dispatchable_builds_not_reusable_entrypoints(self) -> None:
        for path in RELEASE_CALLERS:
            source = text(path)
            self.assertIn("  workflow_dispatch:", source, path.name)
            self.assertNotIn("  workflow_call:", source, path.name)
            self.assertIn("permissions: {}", source, path.name)
            self.assertIn("  verify-tag:", source, path.name)
            self.assertIn("  verify-publish:", source, path.name)

    def test_release_caller_actions_are_immutable(self) -> None:
        for path in RELEASE_CALLERS:
            for line_number, line in enumerate(text(path).splitlines(), start=1):
                if "uses:" not in line or "./.github/" in line:
                    continue
                self.assertRegex(
                    line,
                    FULL_SHA_ACTION,
                    f"{path.name}:{line_number} must pin third-party actions",
                )

    def test_docker_reusable_workflows_are_immutable_and_split(self) -> None:
        publisher = text(DOCKER_PUBLISH)
        builder = text(DOCKER_BUILD)

        # The publisher is callable only from a release caller. Manual builds
        # use the no-secret builder, so a UI dispatch can never reach Docker Hub.
        self.assertNotIn("  workflow_dispatch:", publisher)
        self.assertIn("  workflow_call:", publisher)
        self.assertIn("permissions: {}", publisher)
        self.assertIn("  verify-tag:", publisher)
        self.assertIn("github.ref_type == 'tag'", publisher)
        self.assertIn("github.ref_protected == true", publisher)
        self.assertIn("environment: docker-release", publisher)
        self.assertEqual(publisher.count("Recheck release tag provenance"), 2)
        self.assertNotIn("github.ref == 'refs/heads/main'", publisher)
        self.assertNotIn("event_name == 'pull_request'", publisher)
        self.assertIn("SOURCE_SHA: ${{ github.sha }}", publisher)
        self.assertIn("/tmp/digests/${PLATFORM_NAME}-${SOURCE_SHA}.txt", publisher)
        self.assertIn('name "*-${SOURCE_SHA}.txt"', publisher)

        # Every registry action is immutable and appears after the provenance
        # check in each credentialed job.
        for line_number, line in enumerate(publisher.splitlines(), start=1):
            if "uses:" not in line or "./.github/" in line:
                continue
            self.assertRegex(
                line,
                FULL_SHA_ACTION,
                f"docker-reusable-publish.yml:{line_number} must pin actions",
            )
        for job in ("build-and-push", "publish-manifest-list"):
            block = job_block(publisher, job)
            self.assertLess(
                block.index("Recheck release tag provenance"),
                block.index("docker/login-action@"),
                job,
            )
            self.assertIn("persist-credentials: false", block, job)

        manifest = job_block(publisher, "publish-manifest-list")
        self.assertLess(
            manifest.index("Download platform digests"),
            manifest.index("docker/login-action@"),
        )
        self.assertLess(
            manifest.index("Validate platform digests"),
            manifest.index("docker/login-action@"),
        )
        self.assertIn("Digest artifacts must not contain symlinks", manifest)

        # The reusable build remains available to CI and manual caller builds,
        # but it has no secret or publishing path of its own.
        self.assertIn("  workflow_call:", builder)
        self.assertNotIn("  workflow_dispatch:", builder)
        self.assertNotIn("secrets.", builder)
        self.assertNotIn("push: true", builder)
        for line_number, line in enumerate(builder.splitlines(), start=1):
            if "uses:" not in line:
                continue
            self.assertRegex(
                line,
                FULL_SHA_ACTION,
                f"docker-reusable-build.yml:{line_number} must pin actions",
            )

    def test_publish_jobs_require_a_verified_tag(self) -> None:
        expected_python_packages = {
            "cd-py-agent.yml": "cua-agent",
            "cd-py-auto.yml": "cua-auto",
            "cd-py-bench-ui.yml": "cua-bench-ui",
            "cd-py-bench.yml": "cua-bench",
            "cd-py-cli.yml": "cua-cli",
            "cd-py-cloud.yml": "cua-cloud",
            "cd-py-computer-server.yml": "cua-computer-server",
            "cd-py-computer.yml": "cua-computer",
            "cd-py-core.yml": "cua-core",
            "cd-py-cua.yml": "cua",
            "cd-py-mcp-server.yml": "cua-mcp-server",
            "cd-py-sandbox-apps.yml": "cua-sandbox-apps",
            "cd-py-sandbox.yml": "cua-sandbox",
            "cd-py-som.yml": "cua-som",
            "cd-py-train.yml": "cua-train",
        }
        for path in PY_CALLERS:
            block = job_block(text(path), "publish")
            self.assertIn("github.event_name == 'push'", block, path.name)
            self.assertIn("github.ref_type == 'tag'", block, path.name)
            self.assertIn("needs.verify-publish.result == 'success'", block, path.name)
            self.assertIn("Recheck release tag provenance", block, path.name)
            self.assertIn("environment: pypi-release", block, path.name)
            self.assertIn("id-token: write", block, path.name)
            self.assertIn("RELEASE_TRUSTED_REPOSITORY: trycua/cua", block, path.name)
            package = expected_python_packages[path.name]
            self.assertIn(
                f"name: pypi-{package.removeprefix('cua-')}",
                block,
                path.name,
            )
            self.assertIn("validate_publish_artifacts.py", block, path.name)
            self.assertIn(f"EXPECTED_PACKAGE: {package}", block, path.name)
            self.assertIn("--expected-package", block, path.name)
            self.assertIn("--expected-version", block, path.name)
            self.assertIn("--max-files 2", block, path.name)

        expected_packages = {
            "cd-ts-cli.yml": "@trycua/cli",
            "cd-ts-computer.yml": "@trycua/computer",
            "cd-ts-core.yml": "@trycua/core",
            "cd-ts-cuabot.yml": "cuabot",
            "cd-ts-playground.yml": "@trycua/playground",
        }
        for path in TS_CALLERS:
            job_name = "publish-npm" if "publish-npm:" in text(path) else "publish"
            block = job_block(text(path), job_name)
            self.assertIn("github.event_name == 'push'", block, path.name)
            self.assertIn("github.ref_type == 'tag'", block, path.name)
            self.assertIn("needs.verify-publish.result == 'success'", block, path.name)
            self.assertIn("allow_legacy_token: false", block, path.name)
            self.assertIn("trusted_publisher_repository: \"trycua/cua\"", block, path.name)
            self.assertIn(
                f"trusted_package_name: \"{expected_packages[path.name]}\"",
                block,
                path.name,
            )
            self.assertIn(f"trusted_publisher_workflow: \"{path.name}\"", block, path.name)
            self.assertRegex(
                block,
                r'trusted_tag_prefix: "(?:npm-[a-z-]+|cuabot)-v"',
                path.name,
            )
            self.assertIn("expected_tag: ${{ needs.prepare.outputs.tag }}", block, path.name)
            self.assertIn(
                "expected_version: ${{ needs.prepare.outputs.version }}",
                block,
                path.name,
            )
            self.assertNotIn("secrets.", block, path.name)

        for path in CONTAINER_CALLERS:
            block = job_block(text(path), "publish")
            self.assertIn("github.event_name == 'push'", block, path.name)
            self.assertIn("github.ref_type == 'tag'", block, path.name)
            self.assertIn("needs.verify-publish.result == 'success'", block, path.name)
            self.assertNotIn("secrets.", block, path.name)

    def test_docker_credentials_are_environment_scoped(self) -> None:
        for path in CONTAINER_CALLERS:
            source = text(path)
            publish = job_block(source, "publish")
            self.assertNotIn("DOCKER_HUB_TOKEN", publish, path.name)
            self.assertIn("environment: docker-release", text(DOCKER_PUBLISH), path.name)

        publisher = text(DOCKER_PUBLISH)
        self.assertIn("DOCKER_HUB_RELEASE_TOKEN", publisher)
        self.assertNotIn("DOCKER_HUB_TOKEN", publisher)
        self.assertEqual(publisher.count("Require the protected Docker Hub credential"), 2)

    def test_xfce_skips_unsupported_arm64_base_image(self) -> None:
        source = text(WORKFLOWS / "cd-container-xfce.yml")
        manual = job_block(source, "manual-build")
        publish = job_block(source, "publish")
        self.assertIn("skip_arm64: true", manual)
        self.assertIn("skip_arm64: true", publish)
        self.assertIn("kicad/kicad:9.0", source)

    def test_known_container_arm64_gaps_are_explicit(self) -> None:
        for filename in ("cd-container-cuabot.yml", "cd-container-kasm.yml"):
            source = text(WORKFLOWS / filename)
            manual = job_block(source, "manual-build")
            publish = job_block(source, "publish")
            self.assertIn("skip_arm64: true", manual, filename)
            self.assertIn("skip_arm64: true", publish, filename)

    def test_ci_container_arm64_gaps_are_explicit(self) -> None:
        for filename in (
            "ci-container-cuabot.yml",
            "ci-container-kasm.yml",
            "ci-container-xfce.yml",
        ):
            source = text(WORKFLOWS / filename)
            self.assertIn("permissions: {}", source, filename)
            self.assertIn("    permissions:\n      contents: read", source, filename)
            self.assertIn("skip_arm64: true", source, filename)

    def test_manual_build_paths_have_no_credentials(self) -> None:
        for path in PY_CALLERS + TS_CALLERS + CONTAINER_CALLERS:
            source = text(path)
            for block in re.split(r"(?m)^  (?=[A-Za-z0-9_-]+:\n)", source)[1:]:
                if "if: github.event_name == 'workflow_dispatch'" not in block:
                    continue
                self.assertNotIn("secrets.", block, path.name)
                self.assertNotIn("id-token", block, path.name)

        for path in TS_CALLERS + CONTAINER_CALLERS:
            block = job_block(text(path), "manual-build")
            self.assertIn("github.event_name == 'workflow_dispatch'", block, path.name)
            self.assertNotIn("secrets.", block, path.name)
            self.assertNotIn("id-token", block, path.name)

        for path in PY_CALLERS:
            block = job_block(text(path), "build-package")
            self.assertIn("github.event_name == 'workflow_dispatch'", block, path.name)
            self.assertNotIn("secrets.", block, path.name)
            self.assertNotIn("id-token", block, path.name)

    def test_checkout_is_bound_to_the_triggering_revision(self) -> None:
        for path in RELEASE_CALLERS:
            source = text(path)
            for block in re.findall(
                r"(?ms)^\s*- name:?(?:[^\n]*)\n.*?(?=^\s*- |\Z)", source
            ):
                if "actions/checkout@" not in block:
                    continue
                if "path: trusted-release" in block:
                    self.assertIn(
                        "repository: ${{ github.repository }}", block, path.name
                    )
                    self.assertIn("ref: main", block, path.name)
                    self.assertIn("persist-credentials: false", block, path.name)
                    continue
                self.assertIn("ref: ${{ github.sha }}", block, path.name)
                self.assertIn("persist-credentials: false", block, path.name)

    def test_credential_gates_use_protected_main_helpers(self) -> None:
        """Historical tags must not select verifier or artifact-gate code."""
        for path in PY_CALLERS:
            block = job_block(text(path), "publish")
            self.assertIn("path: trusted-release", block, path.name)
            self.assertIn("repository: ${{ github.repository }}", block, path.name)
            self.assertIn("ref: main", block, path.name)
            self.assertIn("trusted-release/.github/scripts/verify_release_tag.py", block)
            self.assertIn(
                "trusted-release/.github/scripts/validate_publish_artifacts.py", block
            )
            self.assertNotIn(
                '"${GITHUB_WORKSPACE}/.github/scripts/verify_release_tag.py"', block
            )

        docs_block = job_block(text(WORKFLOWS / "cd-cua-driver-docs.yml"), "open-reference-pr")
        self.assertIn("path: trusted-release", docs_block)
        self.assertIn("repository: ${{ github.repository }}", docs_block)
        self.assertIn("ref: main", docs_block)
        self.assertIn("trusted-release/.github/scripts/verify_release_tag.py", docs_block)

        release = text(RELEASE_REUSABLE)
        release_block = job_block(release, "create-release")
        self.assertIn("path: trusted-release", release_block)
        self.assertIn("repository: ${{ github.repository }}", release_block)
        self.assertIn("ref: main", release_block)
        self.assertIn("trusted-release/.github/scripts/verify_release_tag.py", release_block)

        for workflow, jobs in (
            (WORKFLOWS / "verify-release-tag.yml", ("verify",)),
            (WORKFLOWS / "py-reusable-publish.yml", ("validate-publisher-identity", "publish-legacy-token")),
            (WORKFLOWS / "ts-reusable-publish.yml", ("validate-publisher-identity", "publish-legacy-token")),
            (DOCKER_PUBLISH, ("build-and-push", "publish-manifest-list")),
        ):
            source = text(workflow)
            for job in jobs:
                block = job_block(source, job)
                self.assertIn("path: trusted-release", block, f"{workflow.name}:{job}")
                self.assertIn(
                    "repository: ${{ github.repository }}", block, f"{workflow.name}:{job}"
                )
                self.assertIn("ref: main", block, f"{workflow.name}:{job}")

    def test_ci_secret_jobs_are_schedule_only(self) -> None:
        models = text(WORKFLOWS / "ci-test-models.yml")
        benchmark = text(WORKFLOWS / "ci-cold-start-benchmark.yml")
        for source, secret_job in ((models, "test-all-models"), (benchmark, "benchmark")):
            self.assertIn("schedule:", source)
            block = job_block(source, secret_job)
            self.assertIn("if: github.event_name == 'schedule'", block)
            self.assertIn("secrets.", block)
            manual = job_block(source, "manual-validation")
            self.assertIn("if: github.event_name == 'workflow_dispatch'", manual)
            self.assertNotIn("secrets.", manual)

    def test_driver_docs_credentials_follow_verified_tag(self) -> None:
        source = text(WORKFLOWS / "cd-cua-driver-docs.yml")
        self.assertIn("  verify-tag:", source)
        self.assertIn("  open-reference-pr:", source)
        block = job_block(source, "open-reference-pr")
        self.assertIn("github.event_name == 'push'", block)
        self.assertIn("needs.verify-tag.result == 'success'", block)
        self.assertIn("environment: release-app", block)
        self.assertLess(
            block.index("Recheck tag provenance before credentials"),
            block.index("Generate GitHub App token"),
        )

    def test_release_reusable_workflow_is_source_and_tag_bound(self) -> None:
        source = text(RELEASE_REUSABLE)
        self.assertIn("  workflow_call:", source)
        self.assertIn("    tag_prefix:", source)
        self.assertIn("permissions: {}", source)
        self.assertIn("github.event_name == 'push'", source)
        self.assertIn("github.ref_type == 'tag'", source)
        self.assertIn("github.ref_protected == true", source)
        self.assertIn("github.ref_name == inputs.tag_name", source)
        self.assertIn("environment: github-release", source)
        self.assertIn("Recheck tag provenance before write operations", source)
        self.assertIn("persist-credentials: false", source)
        self.assertNotIn("uses: actions/checkout@v", source)
        self.assertNotIn("uses: actions/download-artifact@v", source)
        self.assertIn("Create release with GitHub CLI", source)
        self.assertIn("gh \"${release_args[@]}\"", source)
        self.assertIn("GH_TOKEN: ${{ github.token }}", source)
        release_job = job_block(source, "create-release")
        self.assertIn("contents: write", release_job)
        self.assertIn("--repo \"$REPOSITORY\"", source)
        self.assertIn("--verify-tag", source)
        self.assertIn("--notes-file", source)
        self.assertIn("--generate-notes", source)
        self.assertIn("--draft", source)
        self.assertIn("--prerelease", source)
        self.assertIn("release_args+=(\"$artifact\")", source)
        self.assertIn("artifact count exceeds the safety limit", source)
        for line_number, line in enumerate(source.splitlines(), start=1):
            if "uses:" not in line or "./.github/" in line:
                continue
            self.assertRegex(
                line,
                FULL_SHA_ACTION,
                f"release-github-reusable.yml:{line_number} must pin actions",
            )

        expected_prefixes = {
            "cd-py-agent.yml": "agent-v",
            "cd-py-auto.yml": "auto-v",
            "cd-py-bench-ui.yml": "bench-ui-v",
            "cd-py-bench.yml": "bench-v",
            "cd-py-cli.yml": "cli-v",
            "cd-py-cloud.yml": "cloud-v",
            "cd-py-computer-server.yml": "computer-server-v",
            "cd-py-computer.yml": "computer-v",
            "cd-py-core.yml": "core-v",
            "cd-py-cua.yml": "cua-v",
            "cd-py-mcp-server.yml": "mcp-server-v",
            "cd-py-sandbox-apps.yml": "sandbox-apps-v",
            "cd-py-sandbox.yml": "sandbox-v",
            "cd-py-som.yml": "som-v",
            "cd-py-train.yml": "train-v",
            "cd-ts-cli.yml": "npm-cli-v",
            "cd-ts-computer.yml": "npm-computer-v",
            "cd-ts-core.yml": "npm-core-v",
            "cd-ts-cuabot.yml": "cuabot-v",
            "cd-ts-playground.yml": "npm-playground-v",
        }
        for filename, prefix in expected_prefixes.items():
            caller = text(WORKFLOWS / filename)
            release_job = job_block(
                caller,
                "create-release",
            )
            self.assertIn("uses: ./.github/workflows/release-github-reusable.yml", release_job)
            self.assertIn(f"tag_prefix: {prefix}", release_job, filename)


if __name__ == "__main__":
    unittest.main()
