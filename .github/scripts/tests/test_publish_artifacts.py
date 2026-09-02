"""Behavior tests for release-artifact type and symlink validation."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
import io
import stat
import tarfile
import zipfile
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "validate_publish_artifacts.py"


class PublishArtifactTests(unittest.TestCase):
    def run_validator(
        self,
        directory: Path,
        *suffixes: str,
        exactly_one: bool = False,
        expected_package: str | None = None,
        expected_version: str | None = None,
        max_files: int | None = None,
    ) -> subprocess.CompletedProcess[str]:
        command = ["python3", str(SCRIPT), str(directory)]
        for suffix in suffixes:
            command.extend(("--suffix", suffix))
        if exactly_one:
            command.append("--exactly-one")
        if expected_package is not None:
            command.extend(("--expected-package", expected_package))
        if expected_version is not None:
            command.extend(("--expected-version", expected_version))
        if max_files is not None:
            command.extend(("--max-files", str(max_files)))
        return subprocess.run(command, capture_output=True, text=True, check=False)

    @staticmethod
    def write_wheel(path: Path, package: str = "cua_agent", version: str = "1.2.3") -> None:
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

    def test_python_metadata_and_name_normalization_pass(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            self.write_wheel(artifact_directory / "cua_agent-1.2.3-py3-none-any.whl")
            result = self.run_validator(
                artifact_directory,
                ".whl",
                ".tar.gz",
                expected_package="cua-agent",
                expected_version="1.2.3",
                max_files=2,
            )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_python_wheel_and_sdist_metadata_both_pass(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            self.write_wheel(artifact_directory / "cua_agent-1.2.3-py3-none-any.whl")
            self.write_sdist(artifact_directory / "cua-agent-1.2.3.tar.gz")
            result = self.run_validator(
                artifact_directory,
                ".whl",
                ".tar.gz",
                expected_package="cua-agent",
                expected_version="1.2.3",
                max_files=2,
            )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_python_metadata_package_mismatch_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            self.write_wheel(artifact_directory / "cua_agent-1.2.3-py3-none-any.whl")
            result = self.run_validator(
                artifact_directory,
                ".whl",
                ".tar.gz",
                expected_package="other-package",
                expected_version="1.2.3",
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match expected package", result.stderr)

    def test_python_metadata_version_mismatch_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            self.write_wheel(artifact_directory / "cua_agent-1.2.3-py3-none-any.whl")
            result = self.run_validator(
                artifact_directory,
                ".whl",
                ".tar.gz",
                expected_package="cua-agent",
                expected_version="9.9.9",
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match expected version", result.stderr)

    def test_python_expected_version_must_be_exact_semver(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            self.write_wheel(artifact_directory / "cua_agent-1.2.3-py3-none-any.whl")
            result = self.run_validator(
                artifact_directory,
                ".whl",
                ".tar.gz",
                expected_package="cua-agent",
                expected_version="1.2.3-rc.1",
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exact SemVer", result.stderr)

    def test_python_duplicate_distribution_kind_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            self.write_wheel(artifact_directory / "one.whl")
            self.write_wheel(artifact_directory / "two.whl")
            result = self.run_validator(
                artifact_directory,
                ".whl",
                ".tar.gz",
                expected_package="cua-agent",
                expected_version="1.2.3",
                max_files=2,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("more than one .whl", result.stderr)

    def test_python_artifact_count_limit_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            self.write_wheel(artifact_directory / "one.whl")
            self.write_sdist(artifact_directory / "two.tar.gz")
            result = self.run_validator(
                artifact_directory,
                ".whl",
                ".tar.gz",
                max_files=1,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("at most 1 artifacts", result.stderr)

    def test_python_wheel_symlink_member_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact_directory = Path(directory)
            wheel = artifact_directory / "cua-agent.whl"
            metadata = b"Metadata-Version: 2.1\nName: cua-agent\nVersion: 1.2.3\n\n"
            with zipfile.ZipFile(wheel, "w") as archive:
                archive.writestr("cua_agent-1.2.3.dist-info/METADATA", metadata)
                link = zipfile.ZipInfo("link")
                link.external_attr = (stat.S_IFLNK | 0o777) << 16
                archive.writestr(link, b"../../outside")
            result = self.run_validator(
                artifact_directory,
                ".whl",
                ".tar.gz",
                expected_package="cua-agent",
                expected_version="1.2.3",
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("contains a link", result.stderr)


if __name__ == "__main__":
    unittest.main()
