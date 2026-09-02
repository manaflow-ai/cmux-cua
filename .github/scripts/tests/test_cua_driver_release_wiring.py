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
        self.assertIn('workflows: ["CD: Cua Driver (cross-platform)"]', workflow)
        self.assertIn("workflow_id == 311952875", workflow)
        self.assertIn("github.event.workflow_run.conclusion == 'success'", workflow)
        self.assertIn("verify_cua_driver_release.py", workflow)
        self.assertIn("prepare_cua_driver_binary.py", workflow)
        self.assertIn("python -m build --wheel --no-isolation", workflow)
        self.assertIn('line.startswith("Tag: ")', workflow)
        self.assertIn("normalized_version:", workflow)

    def test_python_publish_is_tokenless_and_actions_are_pinned(self) -> None:
        workflow = self.read(".github/workflows/cd-py-cua-driver.yml")

        self.assertNotIn("PYPI_TOKEN", workflow)
        self.assertIn("id-token: write", workflow)
        self.assertIn("environment: pypi", workflow)
        self.assertIn(
            "--require-hashes -r .github/scripts/cua-driver-build-requirements.txt",
            workflow,
        )
        self.assertEqual(workflow.count("persist-credentials: false"), 3)
        self.assertIn(
            "pypa/gh-action-pypi-publish@dc37677b2e1c63e2034f94d8a5b11f265b73ba33",
            workflow,
        )
        self.assertNotIn("@v4", workflow)
        self.assertNotIn("@v5", workflow)
        self.assertNotIn("@v6", workflow)

    def test_python_publish_checks_source_commit_and_run_artifacts(self) -> None:
        workflow = self.read(".github/workflows/cd-py-cua-driver.yml")

        self.assertIn("ref: ${{ needs.validate-provenance.outputs.source_head_sha }}", workflow)
        self.assertIn("artifact-ids: ${{ steps.source-artifact.outputs.id }}", workflow)
        self.assertIn("run-id: ${{ needs.validate-provenance.outputs.source_run_id }}", workflow)
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
