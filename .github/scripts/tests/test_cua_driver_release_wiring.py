"""Regression tests for cua-driver-rs release and PyPI wiring."""

from pathlib import Path
import unittest


REPO_ROOT = Path(__file__).resolve().parents[3]


class TestCuaDriverReleaseWiring(unittest.TestCase):
    """Verify cua-driver-rs releases feed the Python cua-driver publisher."""

    def read(self, relative_path: str) -> str:
        return (REPO_ROOT / relative_path).read_text()

    def test_python_publish_follows_rust_workflow_run(self) -> None:
        workflow = self.read(".github/workflows/cd-py-cua-driver.yml")

        self.assertIn("workflow_run:", workflow)
        self.assertNotIn("workflow_dispatch:", workflow)
        self.assertIn("github.ref == 'refs/heads/main'", workflow)
        self.assertIn('workflows: ["CD: Cua Driver (cross-platform)"]', workflow)
        self.assertIn("workflow_id == 311952875", workflow)
        self.assertIn("github.event.workflow_run.conclusion == 'success'", workflow)
        self.assertIn("verify_cua_driver_release.py", workflow)
        self.assertIn("prepare_cua_driver_binary.py", workflow)
        self.assertIn("python -m build --wheel --no-isolation", workflow)
        self.assertIn('line.startswith("Tag: ")', workflow)
        self.assertIn("normalized_version:", workflow)
        self.assertIn('Path("pyproject.toml")', workflow)
        self.assertIn('Path("src/cua_driver/__init__.py")', workflow)

    def test_python_publish_is_tokenless_and_actions_are_pinned(self) -> None:
        workflow = self.read(".github/workflows/cd-py-cua-driver.yml")

        self.assertNotIn("PYPI_TOKEN", workflow)
        self.assertIn("id-token: write", workflow)
        self.assertIn("environment: pypi", workflow)
        self.assertIn(
            "--require-hashes -r trusted-release/.github/scripts/"
            "cua-driver-build-requirements.txt",
            workflow,
        )
        # The provenance check runs once before the build and once again in
        # the credentialed publish job. Each of the four checkouts must keep
        # its token out of the working tree.
        self.assertEqual(workflow.count("persist-credentials: false"), 4)
        self.assertIn(
            "pypa/gh-action-pypi-publish@dc37677b2e1c63e2034f94d8a5b11f265b73ba33",
            workflow,
        )
        self.assertNotIn("@v4", workflow)
        self.assertNotIn("@v5", workflow)
        self.assertNotIn("@v6", workflow)

    def test_python_publish_checks_source_commit_and_run_artifacts(self) -> None:
        workflow = self.read(".github/workflows/cd-py-cua-driver.yml")

        self.assertIn("source_head_sha:", workflow)
        self.assertIn("source_head_sha", self.read(".github/scripts/verify_cua_driver_release.py"))
        self.assertIn("ref: ${{ needs.validate-provenance.outputs.source_head_sha }}", workflow)
        self.assertIn('test "$(git -C source rev-parse HEAD)" = "$SOURCE_HEAD_SHA"', workflow)
        self.assertIn("artifact-ids: ${{ steps.source-artifact.outputs.id }}", workflow)
        self.assertIn("run-id: ${{ needs.validate-provenance.outputs.source_run_id }}", workflow)
        self.assertIn("path: source\n", workflow)
        self.assertIn("--destination source/libs/cua-driver/python/src/cua_driver/bin", workflow)
        self.assertIn("working-directory: source/libs/cua-driver/python", workflow)
        self.assertIn(
            "target commit",
            self.read(".github/scripts/verify_cua_driver_release.py"),
        )

    def test_python_publish_uses_source_release_version(self) -> None:
        workflow = self.read(".github/workflows/cd-py-cua-driver.yml")

        self.assertIn("WORKFLOW_RUN_HEAD_BRANCH", workflow)
        self.assertIn(
            'TAG_PREFIX = "cua-driver-rs-v"',
            self.read(".github/scripts/verify_cua_driver_release.py"),
        )
        self.assertNotIn("MANUAL_VERSION", workflow)
        self.assertNotIn("DEFAULT_VERSION_FILE", workflow)

    def test_credential_jobs_use_the_protected_main_verifier(self) -> None:
        workflow = self.read(".github/workflows/cd-py-cua-driver.yml")

        # A workflow_run payload can point at a historical source revision.
        # Both the initial gate and the last gate before OIDC must therefore
        # load the verifier from this repository's protected main branch.
        trusted_checkouts = workflow.count(
            "repository: ${{ github.repository }}\n"
            "          ref: main\n"
            "          path: trusted-release"
        )
        self.assertEqual(trusted_checkouts, 3)
        self.assertEqual(
            workflow.count("python3 trusted-release/.github/scripts/verify_cua_driver_release.py"),
            2,
        )
        self.assertIn("Recheck source provenance before PyPI OIDC", workflow)

        build = workflow[workflow.index("  build-wheels:") : workflow.index("  publish-pypi:")]
        self.assertIn("path: trusted-release", build)
        self.assertIn(
            "--require-hashes -r trusted-release/.github/scripts/"
            "cua-driver-build-requirements.txt",
            build,
        )
        self.assertIn(
            "python trusted-release/.github/scripts/prepare_cua_driver_binary.py",
            build,
        )

        # No verifier may execute from the tag/source checkout. This catches
        # a missing trusted checkout or a path silently changed back to the
        # event-associated workspace.
        self.assertNotIn("python3 .github/scripts/verify_cua_driver_release.py", workflow)
        self.assertNotIn("python3 source/.github/scripts/verify_cua_driver_release.py", workflow)
        self.assertNotIn("python .github/scripts/prepare_cua_driver_binary.py", workflow)

    def test_trusted_verifier_contract_fails_closed_when_checkout_is_changed(self) -> None:
        workflow = self.read(".github/workflows/cd-py-cua-driver.yml")

        # Keep the contract explicit so a future edit cannot substitute a
        # caller-controlled ref or a different repository without review.
        self.assertNotIn("repository: trycua/cua", workflow)
        self.assertNotIn("ref: ${{ github.event.workflow_run.head_sha }}", workflow)
        self.assertNotIn("ref: ${{ github.event.workflow_run.head_branch }}", workflow)
        self.assertNotIn("path: source/.github/scripts", workflow)

    def test_python_publish_builds_linux_arm64_wheel(self) -> None:
        workflow = self.read(".github/workflows/cd-py-cua-driver.yml")

        self.assertIn("os: ubuntu-24.04-arm", workflow)
        self.assertIn("arch: arm64", workflow)

    def test_release_on_merge_tracks_rust_driver(self) -> None:
        workflow = self.read(".github/workflows/release-on-merge.yml")

        self.assertIn("['libs/cua-driver/rust/', 'cua-driver-rs']", workflow)

    def test_release_reminder_tracks_rust_driver(self) -> None:
        workflow = self.read(".github/workflows/ci-release-reminder.yml")

        self.assertIn('["libs/cua-driver/rust/"]="cua-driver-rs"', workflow)
        self.assertIn("cua-driver desktop release validation", workflow)
        self.assertIn("e2e-rust-windows.yml", workflow)
        self.assertIn("scripts/ci/macos/run-rust-e2e.sh", workflow)
        self.assertIn("e2e-rust-linux.yml", workflow)
        self.assertIn("e2e-rust-linux-wayland.yml", workflow)

    def test_unreleased_digest_tracks_rust_driver(self) -> None:
        workflow = self.read(".github/workflows/release-unreleased-digest.yml")

        self.assertIn(
            'SERVICE_TAG_DIR["cua-driver-rs"]="cua-driver-rs-v|libs/cua-driver/rust/"',
            workflow,
        )

    def test_rust_driver_bump_keeps_python_wrapper_version_synced(self) -> None:
        config = self.read("libs/cua-driver/rust/.bumpversion.cfg")

        self.assertIn("[bumpversion:file:../python/pyproject.toml]", config)
        self.assertIn("[bumpversion:file:../python/src/cua_driver/__init__.py]", config)


if __name__ == "__main__":
    unittest.main()
