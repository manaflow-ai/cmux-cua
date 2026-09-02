"""Behavior tests for release-artifact type and symlink validation."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "validate_publish_artifacts.py"


class PublishArtifactTests(unittest.TestCase):
    def run_validator(
        self,
        directory: Path,
        *suffixes: str,
        exactly_one: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        command = ["python3", str(SCRIPT), str(directory)]
        for suffix in suffixes:
            command.extend(("--suffix", suffix))
        if exactly_one:
            command.append("--exactly-one")
        return subprocess.run(command, capture_output=True, text=True, check=False)

    def test_regular_wheel_and_source_archive_pass(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            (artifact_directory / "package-1.0-py3-none-any.whl").write_bytes(b"wheel")
            (artifact_directory / "package-1.0.tar.gz").write_bytes(b"source")
            result = self.run_validator(artifact_directory, ".whl", ".tar.gz")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_symlink_artifact_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            target = artifact_directory / "outside.tgz"
            target.write_bytes(b"not-an-artifact")
            link = artifact_directory / "package.tgz"
            try:
                link.symlink_to(target)
            except OSError as error:
                self.skipTest(f"symlinks unavailable: {error}")
            result = self.run_validator(artifact_directory, ".tgz", exactly_one=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a regular file", result.stderr)

    def test_non_regular_artifact_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            (artifact_directory / "package.tgz").mkdir()
            result = self.run_validator(artifact_directory, ".tgz", exactly_one=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a regular file", result.stderr)

    def test_unexpected_file_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            (artifact_directory / "package.whl").write_bytes(b"wheel")
            (artifact_directory / "notes.txt").write_bytes(b"unexpected")
            result = self.run_validator(artifact_directory, ".whl")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected file type", result.stderr)

    def test_symlink_directory_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "real-dist"
            target.mkdir()
            (target / "package.whl").write_bytes(b"wheel")
            link = root / "dist"
            try:
                link.symlink_to(target, target_is_directory=True)
            except OSError as error:
                self.skipTest(f"symlinks unavailable: {error}")
            result = self.run_validator(link, ".whl")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a real directory", result.stderr)

    def test_missing_suffix_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_validator(Path(directory), "")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("non-empty artifact suffix", result.stderr)


if __name__ == "__main__":
    unittest.main()
