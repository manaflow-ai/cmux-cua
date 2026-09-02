#!/usr/bin/env python3
"""Verify and extract one binary from an immutable release artifact.

The source release workflow uploads an artifact containing several archives.
This helper selects one exact archive, verifies the release API digest captured
by the provenance validator, and extracts only regular files with the expected
names.  It never trusts archive paths or follows symlinks.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import sys
import tarfile
import zipfile
from pathlib import Path, PurePosixPath
from typing import Any


MAX_ARCHIVE_BYTES = 512 * 1024 * 1024
MAX_BINARY_BYTES = 256 * 1024 * 1024
MAX_ARCHIVE_MEMBERS = 2_000
PLATFORM_BINARY_NAMES = {
    "darwin-universal": ("cua-driver",),
    "linux-x86_64": ("cua-driver",),
    "linux-arm64": ("cua-driver",),
    "windows-x86_64": ("cua-driver.exe", "cua-driver-uia.exe"),
    "windows-arm64": ("cua-driver.exe", "cua-driver-uia.exe"),
}


class ArtifactError(RuntimeError):
    """Raised when an artifact is not the expected release payload."""


def _safe_member_name(name: str) -> PurePosixPath:
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts or not path.parts:
        raise ArtifactError(f"archive contains an unsafe path: {name!r}")
    return path


def _digest(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def _find_archive(root: Path, archive_name: str) -> Path:
    if not archive_name or Path(archive_name).name != archive_name:
        raise ArtifactError("archive name must be a plain file name")
    candidates = sorted(
        path for path in root.rglob(archive_name) if path.is_file() and not path.is_symlink()
    )
    if len(candidates) != 1:
        raise ArtifactError(
            f"expected exactly one {archive_name!r} in {root}, found {len(candidates)}"
        )
    archive = candidates[0]
    size = archive.stat().st_size
    if size <= 0 or size > MAX_ARCHIVE_BYTES:
        raise ArtifactError(f"archive size is outside the permitted range: {size}")
    return archive


def _write_binary(destination: Path, data: bytes, name: str) -> None:
    if not data or len(data) > MAX_BINARY_BYTES:
        raise ArtifactError(f"extracted {name} has an invalid size")
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f".{destination.name}.tmp-{os.getpid()}")
    try:
        temporary.write_bytes(data)
        if os.name != "nt":
            temporary.chmod(0o755)
        os.replace(temporary, destination)
    except OSError as exc:
        raise ArtifactError(f"cannot write extracted {name}") from exc
    finally:
        if temporary.exists():
            temporary.unlink()


def _ensure_safe_directory(destination: Path) -> None:
    """Create ``destination`` without traversing a symlink from the checkout."""

    def check(path: Path) -> None:
        try:
            info = path.lstat()
        except OSError as exc:
            raise ArtifactError(f"cannot inspect destination path: {path}") from exc
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
            raise ArtifactError(f"destination path is not a real directory: {path}")

    absolute = Path(os.path.abspath(destination))
    working_directory = Path.cwd().absolute()
    missing: list[Path] = []
    try:
        relative = absolute.relative_to(working_directory)
    except ValueError:
        # A caller may use an absolute destination outside the checkout.  In
        # that case, check the nearest existing parent and do not reject
        # unrelated system links such as macOS /var -> /private/var.
        current = absolute
        while not current.exists() and not current.is_symlink():
            missing.append(current)
            parent = current.parent
            if parent == current:
                break
            current = parent
        check(current)
    else:
        current = working_directory
        check(current)
        for part in relative.parts:
            current /= part
            if current.exists() or current.is_symlink():
                check(current)
            else:
                missing.append(current)
    for path in reversed(missing):
        try:
            path.mkdir()
        except FileExistsError:
            pass
        check(path)


def _extract_tar(archive: Path, names: tuple[str, ...], destination: Path) -> None:
    found: dict[str, bytes] = {}
    try:
        with tarfile.open(archive, mode="r:gz") as tar:
            members = tar.getmembers()
            if len(members) > MAX_ARCHIVE_MEMBERS:
                raise ArtifactError("archive contains too many members")
            for member in members:
                path = _safe_member_name(member.name)
                if not member.isreg():
                    # The release binary archive must not contain symlinks,
                    # hardlinks, devices, or other special files.
                    if path.name in names:
                        raise ArtifactError(f"expected binary {path.name!r} is not regular")
                    continue
                if path.name not in names:
                    continue
                if path.name in found:
                    raise ArtifactError(f"archive contains duplicate {path.name!r}")
                source = tar.extractfile(member)
                if source is None:
                    raise ArtifactError(f"cannot read {path.name!r} from archive")
                data = source.read(MAX_BINARY_BYTES + 1)
                found[path.name] = data
    except (tarfile.TarError, OSError) as exc:
        raise ArtifactError(f"cannot read tar archive {archive.name}") from exc
    if set(found) != set(names):
        missing = sorted(set(names) - set(found))
        raise ArtifactError(f"archive is missing binaries: {', '.join(missing)}")
    for name, data in found.items():
        _write_binary(destination / name, data, name)


def _zip_is_symlink(info: zipfile.ZipInfo) -> bool:
    mode = (info.external_attr >> 16) & 0xFFFF
    return stat.S_ISLNK(mode) or stat.S_ISCHR(mode) or stat.S_ISBLK(mode) or stat.S_ISFIFO(mode)


def _extract_zip(archive: Path, names: tuple[str, ...], destination: Path) -> None:
    found: dict[str, bytes] = {}
    try:
        with zipfile.ZipFile(archive) as zip_file:
            members = zip_file.infolist()
            if len(members) > MAX_ARCHIVE_MEMBERS:
                raise ArtifactError("archive contains too many members")
            for info in members:
                path = _safe_member_name(info.filename)
                if info.is_dir() or _zip_is_symlink(info):
                    if path.name in names:
                        raise ArtifactError(f"expected binary {path.name!r} is not regular")
                    continue
                if path.name not in names:
                    continue
                if path.name in found:
                    raise ArtifactError(f"archive contains duplicate {path.name!r}")
                with zip_file.open(info) as source:
                    data = source.read(MAX_BINARY_BYTES + 1)
                found[path.name] = data
    except (zipfile.BadZipFile, OSError) as exc:
        raise ArtifactError(f"cannot read zip archive {archive.name}") from exc
    if set(found) != set(names):
        missing = sorted(set(names) - set(found))
        raise ArtifactError(f"archive is missing binaries: {', '.join(missing)}")
    for name, data in found.items():
        _write_binary(destination / name, data, name)


def prepare(
    artifact_dir: Path,
    archive_name: str,
    expected_sha256: str,
    platform_name: str,
    destination: Path,
) -> None:
    if platform_name not in PLATFORM_BINARY_NAMES:
        raise ArtifactError(f"unsupported platform key: {platform_name!r}")
    if len(expected_sha256) != 64 or any(
        char not in "0123456789abcdef" for char in expected_sha256
    ):
        raise ArtifactError("expected SHA-256 must be 64 lowercase hexadecimal characters")
    archive = _find_archive(artifact_dir, archive_name)
    actual = _digest(archive)
    if actual != expected_sha256:
        raise ArtifactError(
            f"SHA-256 mismatch for {archive.name}: expected {expected_sha256}, got {actual}"
        )
    names = PLATFORM_BINARY_NAMES[platform_name]
    _ensure_safe_directory(destination)
    existing = {
        path.name
        for path in destination.iterdir()
        if path.is_file() or path.is_symlink()
    }
    unexpected = existing - set(names)
    if unexpected:
        raise ArtifactError(
            "binary directory contains unexpected files: " + ", ".join(sorted(unexpected))
        )
    if archive.name.endswith(".zip"):
        _extract_zip(archive, names, destination)
    elif archive.name.endswith(".tar.gz"):
        _extract_tar(archive, names, destination)
    else:
        raise ArtifactError(f"unsupported archive type: {archive.name}")


def _manifest_value(manifest_path: Path, key: str, platform_name: str) -> str:
    try:
        manifest: Any = json.loads(manifest_path.read_text(encoding="utf-8"))
        value = manifest["assets"][platform_name][key]
    except (OSError, KeyError, TypeError, ValueError) as exc:
        raise ArtifactError(f"provenance manifest has no asset {platform_name!r} {key}") from exc
    if not isinstance(value, str) or not value:
        raise ArtifactError(f"provenance asset {platform_name!r} {key} is invalid")
    return value


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact-dir", type=Path, required=True)
    parser.add_argument("--archive-name", required=True)
    parser.add_argument("--platform", required=True, choices=sorted(PLATFORM_BINARY_NAMES))
    parser.add_argument("--provenance", type=Path, required=True)
    parser.add_argument("--destination", type=Path, required=True)
    args = parser.parse_args()
    try:
        expected = _manifest_value(args.provenance, "sha256", args.platform)
        prepare(args.artifact_dir, args.archive_name, expected, args.platform, args.destination)
        print(f"Verified and extracted {args.archive_name} for {args.platform}")
        return 0
    except ArtifactError as exc:
        print(f"::error::Artifact verification failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
