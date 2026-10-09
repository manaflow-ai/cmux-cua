"""Behavioral tests for release archive verification and extraction."""

from __future__ import annotations

import hashlib
import io
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPT_DIR))

import prepare_cua_driver_binary as extractor


class TestPrepareCuaDriverBinary(unittest.TestCase):
    def _write_tar(self, path: Path, name: str = "cua-driver", data: bytes = b"driver") -> str:
        with tarfile.open(path, "w:gz") as archive:
            info = tarfile.TarInfo(f"release/{name}")
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
        return hashlib.sha256(path.read_bytes()).hexdigest()

    def _write_zip(self, path: Path, include_uia: bool = True) -> str:
        with zipfile.ZipFile(path, "w") as archive:
            archive.writestr("release/cua-driver.exe", b"driver")
            if include_uia:
                archive.writestr("release/cua-driver-uia.exe", b"uia")
        return hashlib.sha256(path.read_bytes()).hexdigest()

    def test_extracts_and_verifies_tar_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "cua-driver-rs-1.2.3-linux-x86_64-binary.tar.gz"
            digest = self._write_tar(archive)
            destination = root / "bin"
            extractor.prepare(
                root,
                archive.name,
                digest,
                "linux-x86_64",
                destination,
            )
            self.assertEqual((destination / "cua-driver").read_bytes(), b"driver")

    def test_extracts_both_windows_binaries(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "cua-driver-rs-1.2.3-windows-x86_64-binary.zip"
            digest = self._write_zip(archive)
            destination = root / "bin"
            extractor.prepare(root, archive.name, digest, "windows-x86_64", destination)
            self.assertEqual((destination / "cua-driver.exe").read_bytes(), b"driver")
            self.assertEqual((destination / "cua-driver-uia.exe").read_bytes(), b"uia")

    def test_rejects_digest_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "driver.tar.gz"
            self._write_tar(archive)
            with self.assertRaises(extractor.ArtifactError):
                extractor.prepare(root, archive.name, "0" * 64, "linux-x86_64", root / "bin")

    def test_rejects_unsafe_tar_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "driver.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                info = tarfile.TarInfo("../../cua-driver")
                info.size = 1
                tar.addfile(info, io.BytesIO(b"x"))
            with self.assertRaises(extractor.ArtifactError):
                extractor.prepare(
                    root,
                    archive.name,
                    hashlib.sha256(archive.read_bytes()).hexdigest(),
                    "linux-x86_64",
                    root / "bin",
                )

    def test_rejects_missing_windows_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "driver.zip"
            digest = self._write_zip(archive, include_uia=False)
            with self.assertRaises(extractor.ArtifactError):
                extractor.prepare(root, archive.name, digest, "windows-x86_64", root / "bin")

    def test_rejects_unexpected_existing_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "driver.tar.gz"
            digest = self._write_tar(archive)
            destination = root / "bin"
            destination.mkdir()
            (destination / "unexpected").write_bytes(b"x")
            with self.assertRaises(extractor.ArtifactError):
                extractor.prepare(root, archive.name, digest, "linux-x86_64", destination)

    def test_rejects_symlinked_destination(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "driver.tar.gz"
            digest = self._write_tar(archive)
            target = root / "outside"
            target.mkdir()
            destination = root / "bin"
            destination.symlink_to(target, target_is_directory=True)
            with self.assertRaises(extractor.ArtifactError):
                extractor.prepare(root, archive.name, digest, "linux-x86_64", destination)


if __name__ == "__main__":
    unittest.main()
