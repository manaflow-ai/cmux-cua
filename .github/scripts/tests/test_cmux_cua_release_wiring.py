"""Regression tests for cmux-cua release and PyPI wiring."""

from pathlib import Path
import unittest


REPO_ROOT = Path(__file__).resolve().parents[3]


class TestCmuxCuaReleaseWiring(unittest.TestCase):
    """Verify cmux-cua releases feed the Python cmux-cua publisher."""

    def read(self, relative_path: str) -> str:
        return (REPO_ROOT / relative_path).read_text()

    def test_python_publish_follows_rust_workflow_run(self) -> None:
        workflow = self.read(".github/workflows/cd-py-cmux-cua.yml")

        self.assertIn('workflows: ["CD: cmux CUA (cross-platform)"]', workflow)
        self.assertNotIn("branches:\n      - main", workflow)
        self.assertIn('github.event.workflow_run.conclusion != \'cancelled\'', workflow)
        self.assertIn('gh release view "$TAG" --repo "$GITHUB_REPOSITORY"', workflow)

    def test_python_publish_defaults_to_current_rust_version(self) -> None:
        workflow = self.read(".github/workflows/cd-py-cmux-cua.yml")

        self.assertIn("required: false", workflow)
        self.assertIn('default: ""', workflow)
        self.assertIn("libs/cmux-cua/rust/Cargo.toml", workflow)

    def test_python_publish_builds_linux_arm64_wheel(self) -> None:
        workflow = self.read(".github/workflows/cd-py-cmux-cua.yml")

        self.assertIn("os: ubuntu-24.04-arm", workflow)
        self.assertIn("arch: arm64", workflow)

    def test_release_on_merge_tracks_rust_driver(self) -> None:
        workflow = self.read(".github/workflows/release-on-merge.yml")

        self.assertIn('["libs/cmux-cua/rust/"]="cmux-cua"', workflow)

    def test_release_reminder_tracks_rust_driver(self) -> None:
        workflow = self.read(".github/workflows/ci-release-reminder.yml")

        self.assertIn('["libs/cmux-cua/rust/"]="cmux-cua"', workflow)
        self.assertIn("cmux-cua desktop release validation", workflow)
        self.assertIn("e2e-rust-windows.yml", workflow)
        self.assertIn("scripts/ci/macos/run-rust-e2e.sh", workflow)
        self.assertIn("e2e-rust-linux.yml", workflow)
        self.assertIn("e2e-rust-linux-wayland.yml", workflow)

    def test_unreleased_digest_tracks_rust_driver(self) -> None:
        workflow = self.read(".github/workflows/release-unreleased-digest.yml")

        self.assertIn(
            'SERVICE_TAG_DIR["cmux-cua"]="cmux-cua-v|libs/cmux-cua/rust/"',
            workflow,
        )

    def test_rust_driver_bump_keeps_python_wrapper_version_synced(self) -> None:
        config = self.read("libs/cmux-cua/rust/.bumpversion.cfg")

        self.assertIn("[bumpversion:file:../python/pyproject.toml]", config)
        self.assertIn("[bumpversion:file:../python/src/cmux_cua/__init__.py]", config)


if __name__ == "__main__":
    unittest.main()
