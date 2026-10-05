#!/usr/bin/env python3
"""check-release-assets.py: the release ships LICENSE and the full asset set.

The 0.8.2, 0.8.3 and 0.8.4 archives had no LICENSE: the Package steps copied
`../../LICENSE.md` from libs/cmux-cua/rust (a path that does not exist) behind
`|| true`. All three releases also stayed pre-release, so no consumer could pin
one. The script opens every archive and checks the published asset list and
SHA256SUMS before the workflow marks a release as a full release.
"""
import hashlib
import io
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check-release-assets.py"
LICENSE = b"MIT License\n\nCopyright (c) 2025 Cua AI, Inc.\n"
V = "0.8.5"


def run(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, str(SCRIPT), *args], capture_output=True, text=True)


def write_tar(path: Path, members: dict) -> None:
    """members: name -> (bytes, mode)."""
    with tarfile.open(path, "w:gz") as tar:
        dirs = sorted({n.rsplit("/", 1)[0] for n in members if "/" in n})
        for d in dirs:
            info = tarfile.TarInfo(d)
            info.type = tarfile.DIRTYPE
            info.mode = 0o755
            tar.addfile(info)
        for name, (data, mode) in members.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = mode
            tar.addfile(info, io.BytesIO(data))


def write_zip(path: Path, members: dict) -> None:
    with zipfile.ZipFile(path, "w") as z:
        for name, data in members.items():
            z.writestr(name, data)


def linux_dir(label: str = "linux-x86_64", license_bytes: bytes = LICENSE, binary: bool = True,
              mode: int = 0o755) -> dict:
    top = f"cmux-cua-{V}-{label}"
    members = {f"{top}/LICENSE": (license_bytes, 0o644)}
    if binary:
        members[f"{top}/cmux-cua"] = (b"\x7fELF", mode)
    return members


def mac_dir(label: str) -> dict:
    top = f"cmux-cua-{V}-{label}"
    return {
        f"{top}/cmux-cua": (b"\xca\xfe\xba\xbe", 0o755),
        f"{top}/LICENSE": (LICENSE, 0o644),
        f"{top}/cmux Computer Use.app/Contents/Info.plist": (b"<plist/>", 0o644),
        f"{top}/cmux Computer Use.app/Contents/MacOS/cmux-cua": (b"\xca\xfe\xba\xbe", 0o755),
    }


def win_dir(label: str) -> dict:
    top = f"cmux-cua-{V}-{label}"
    return {f"{top}/cmux-cua.exe": b"MZ", f"{top}/cmux-cua-uia.exe": b"MZ", f"{top}/LICENSE": LICENSE}


def build_release(root: Path, macos: str = "unsigned") -> None:
    """Write every asset of a correct release into root, plus SHA256SUMS."""
    for arch in ("x86_64", "arm64"):
        write_tar(root / f"cmux-cua-{V}-linux-{arch}.tar.gz", linux_dir(f"linux-{arch}"))
        write_tar(root / f"cmux-cua-{V}-linux-{arch}-binary.tar.gz", {"cmux-cua": (b"\x7fELF", 0o755)})
        write_zip(root / f"cmux-cua-{V}-windows-{arch}.zip", win_dir(f"windows-{arch}"))
        write_zip(root / f"cmux-cua-{V}-windows-{arch}-binary.zip",
                  {"cmux-cua.exe": b"MZ", "cmux-cua-uia.exe": b"MZ"})
    labels = ["darwin-universal-unsigned"] if macos == "unsigned" else [
        "darwin-arm64", "darwin-x86_64", "darwin-universal"]
    for label in labels:
        write_tar(root / f"cmux-cua-{V}-{label}.tar.gz", mac_dir(label))
    universal = "darwin-universal-unsigned" if macos == "unsigned" else "darwin-universal"
    write_tar(root / f"cmux-cua-{V}-{universal}-binary.tar.gz", {"cmux-cua": (b"\xca\xfe", 0o755)})
    write_tar(root / f"cmux-cua-v{V}-skills.tar.gz", {f"cmux-cua-v{V}-skills/SKILL.md": (b"# s", 0o644)})
    for name in ("install.sh", "install.ps1", "_install-rust.sh", "uninstall.sh", "uninstall.ps1"):
        (root / name).write_text("#!/bin/sh\n")
    (root / "checksums.txt").write_text("## SHA256 Checksums\n")
    done = run("sums", str(root))
    assert done.returncode == 0, done.stderr
    (root / "SHA256SUMS").write_text(done.stdout)


class Fixture(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.license = self.root / "LICENSE.md"
        self.license.write_bytes(LICENSE)
        self.out = self.root / "out"
        self.out.mkdir()

    def tearDown(self):
        self._tmp.cleanup()

    def archive(self, *paths: Path) -> subprocess.CompletedProcess:
        return run("archive", "--license", str(self.license), *map(str, paths))


class ArchiveTest(Fixture):
    def test_linux_dir_tarball_with_binary_and_license_passes(self):
        path = self.out / f"cmux-cua-{V}-linux-x86_64.tar.gz"
        write_tar(path, linux_dir())
        done = self.archive(path)
        self.assertEqual(done.returncode, 0, done.stderr)

    def test_linux_dir_tarball_without_license_fails(self):
        # The 0.8.4 shape: the top directory holds only the binary.
        path = self.out / f"cmux-cua-{V}-linux-arm64.tar.gz"
        write_tar(path, {f"cmux-cua-{V}-linux-arm64/cmux-cua": (b"\x7fELF", 0o755)})
        done = self.archive(path)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("LICENSE", done.stderr)

    def test_linux_dir_tarball_without_executable_binary_fails(self):
        for members in (linux_dir(binary=False), linux_dir(mode=0o644)):
            path = self.out / f"cmux-cua-{V}-linux-x86_64.tar.gz"
            write_tar(path, members)
            done = self.archive(path)
            self.assertNotEqual(done.returncode, 0)
            self.assertIn("cmux-cua", done.stderr)

    def test_license_must_be_the_repository_license(self):
        path = self.out / f"cmux-cua-{V}-linux-x86_64.tar.gz"
        write_tar(path, linux_dir(license_bytes=b"Apache License\n"))
        self.assertNotEqual(self.archive(path).returncode, 0)

    def test_member_outside_the_top_directory_fails(self):
        members = linux_dir()
        members["stray"] = (b"x", 0o644)
        path = self.out / f"cmux-cua-{V}-linux-x86_64.tar.gz"
        write_tar(path, members)
        self.assertNotEqual(self.archive(path).returncode, 0)

    def test_bare_binary_tarball_needs_only_the_binary_at_the_root(self):
        path = self.out / f"cmux-cua-{V}-linux-x86_64-binary.tar.gz"
        write_tar(path, {"cmux-cua": (b"\x7fELF", 0o755)})
        self.assertEqual(self.archive(path).returncode, 0)
        write_tar(path, {f"cmux-cua-{V}-linux-x86_64/cmux-cua": (b"\x7fELF", 0o755)})
        self.assertNotEqual(self.archive(path).returncode, 0)

    def test_macos_dir_tarball_needs_license_and_app_bundle(self):
        label = "darwin-universal-unsigned"
        path = self.out / f"cmux-cua-{V}-{label}.tar.gz"
        write_tar(path, mac_dir(label))
        self.assertEqual(self.archive(path).returncode, 0)
        members = mac_dir(label)
        del members[f"cmux-cua-{V}-{label}/LICENSE"]
        write_tar(path, members)
        self.assertNotEqual(self.archive(path).returncode, 0)
        members = mac_dir(label)
        del members[f"cmux-cua-{V}-{label}/cmux Computer Use.app/Contents/MacOS/cmux-cua"]
        write_tar(path, members)
        self.assertNotEqual(self.archive(path).returncode, 0)

    def test_windows_dir_zip_needs_both_executables_and_license(self):
        path = self.out / f"cmux-cua-{V}-windows-arm64.zip"
        write_zip(path, win_dir("windows-arm64"))
        self.assertEqual(self.archive(path).returncode, 0)
        members = win_dir("windows-arm64")
        del members[f"cmux-cua-{V}-windows-arm64/LICENSE"]
        write_zip(path, members)
        self.assertNotEqual(self.archive(path).returncode, 0)

    def test_unknown_archive_name_fails(self):
        path = self.out / "something-else.tar.gz"
        write_tar(path, linux_dir())
        self.assertNotEqual(self.archive(path).returncode, 0)


class ExpectedTest(unittest.TestCase):
    def expected(self, macos: str) -> set:
        done = run("expected", "--version", V, "--macos", macos)
        self.assertEqual(done.returncode, 0, done.stderr)
        return set(done.stdout.split())

    def test_unsigned_release_lists_linux_unsigned_macos_windows_and_sums(self):
        names = self.expected("unsigned")
        for name in (f"cmux-cua-{V}-linux-x86_64.tar.gz", f"cmux-cua-{V}-linux-arm64.tar.gz",
                     f"cmux-cua-{V}-linux-x86_64-binary.tar.gz", f"cmux-cua-{V}-linux-arm64-binary.tar.gz",
                     f"cmux-cua-{V}-darwin-universal-unsigned.tar.gz",
                     f"cmux-cua-{V}-darwin-universal-unsigned-binary.tar.gz",
                     f"cmux-cua-{V}-windows-x86_64.zip", "SHA256SUMS", "checksums.txt"):
            self.assertIn(name, names)
        self.assertNotIn(f"cmux-cua-{V}-darwin-universal.tar.gz", names)

    def test_signed_release_lists_the_three_named_macos_tarballs(self):
        names = self.expected("signed")
        for label in ("darwin-arm64", "darwin-x86_64", "darwin-universal"):
            self.assertIn(f"cmux-cua-{V}-{label}.tar.gz", names)
        self.assertIn(f"cmux-cua-{V}-darwin-universal-binary.tar.gz", names)
        self.assertNotIn(f"cmux-cua-{V}-darwin-universal-unsigned.tar.gz", names)


class ReleaseTest(Fixture):
    def check(self, macos: str = "unsigned") -> subprocess.CompletedProcess:
        return run("release", "--version", V, "--macos", macos, "--license", str(self.license), str(self.out))

    def test_complete_release_passes(self):
        for macos in ("unsigned", "signed"):
            with self.subTest(macos=macos):
                for f in self.out.iterdir():
                    f.unlink()
                build_release(self.out, macos)
                done = self.check(macos)
                self.assertEqual(done.returncode, 0, done.stderr)

    def test_missing_asset_fails(self):
        build_release(self.out)
        (self.out / f"cmux-cua-{V}-linux-arm64.tar.gz").unlink()
        done = self.check()
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("linux-arm64.tar.gz", done.stderr)

    def test_unexpected_asset_fails(self):
        build_release(self.out)
        (self.out / "stray.txt").write_text("x")
        self.assertNotEqual(self.check().returncode, 0)

    def test_signed_flag_must_match_the_macos_assets(self):
        build_release(self.out, "unsigned")
        self.assertNotEqual(self.check("signed").returncode, 0)

    def test_sha256sums_mismatch_fails(self):
        build_release(self.out)
        (self.out / "install.sh").write_text("#!/bin/sh\necho changed\n")
        done = self.check()
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("install.sh", done.stderr)

    def test_sha256sums_must_cover_every_asset(self):
        build_release(self.out)
        sums = self.out / "SHA256SUMS"
        lines = [l for l in sums.read_text().splitlines() if not l.endswith("-linux-x86_64.tar.gz")]
        sums.write_text("\n".join(lines) + "\n")
        self.assertNotEqual(self.check().returncode, 0)

    def test_published_linux_tarball_without_license_fails(self):
        build_release(self.out)
        path = self.out / f"cmux-cua-{V}-linux-x86_64.tar.gz"
        write_tar(path, {f"cmux-cua-{V}-linux-x86_64/cmux-cua": (b"\x7fELF", 0o755)})
        done = run("sums", str(self.out))
        (self.out / "SHA256SUMS").write_text(done.stdout)
        self.assertNotEqual(self.check().returncode, 0)


class SumsTest(Fixture):
    def test_sums_use_sha256sum_format_and_skip_the_checksum_files(self):
        (self.out / "b.tar.gz").write_bytes(b"b")
        (self.out / "a.zip").write_bytes(b"a")
        (self.out / "checksums.txt").write_text("x")
        (self.out / "SHA256SUMS").write_text("x")
        done = run("sums", str(self.out))
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.splitlines(), [
            f"{hashlib.sha256(b'a').hexdigest()}  a.zip",
            f"{hashlib.sha256(b'b').hexdigest()}  b.tar.gz",
        ])


if __name__ == "__main__":
    unittest.main()
