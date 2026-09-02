#!/usr/bin/env python3
"""Validate downloaded release artifacts before a registry upload."""

from __future__ import annotations

import argparse
from email import errors as email_errors
from email import policy
from email.parser import BytesParser
import io
import re
import stat
import sys
import tarfile
import zipfile
from collections.abc import Iterable
from pathlib import Path
from posixpath import normpath
from typing import BinaryIO, Callable, TypeVar


class ArtifactError(ValueError):
    """Raised when an artifact directory is not safe to publish."""


MAX_ARCHIVE_MEMBERS = 4096
MAX_ARCHIVE_BYTES = 256 * 1024 * 1024
MAX_METADATA_BYTES = 1024 * 1024
PACKAGE_NAME_PATTERN = re.compile(r"[A-Za-z0-9]+(?:[-_.]+[A-Za-z0-9]+)*\Z")
VERSION_PATTERN = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z"
)
ArchiveMember = TypeVar("ArchiveMember")


def regular_files(
    directory: Path,
    suffixes: tuple[str, ...],
    *,
    exactly_one: bool = False,
    max_files: int | None = None,
) -> tuple[Path, ...]:
    """Return regular files with approved suffixes, rejecting everything else."""
    _validate_file_limits(suffixes, max_files)
    entries = _artifact_entries(directory)
    artifacts = [_regular_artifact(entry, suffixes) for entry in entries]
    _validate_artifact_count(artifacts, exactly_one=exactly_one, max_files=max_files)
    return tuple(artifacts)


def _validate_file_limits(suffixes: tuple[str, ...], max_files: int | None) -> None:
    if not suffixes or any(not suffix for suffix in suffixes):
        raise ArtifactError("at least one non-empty artifact suffix is required")
    if max_files is not None and max_files < 1:
        raise ArtifactError("max_files must be at least one")


def _artifact_entries(directory: Path) -> tuple[Path, ...]:
    try:
        directory_mode = directory.lstat().st_mode
    except OSError as error:
        raise ArtifactError(f"cannot inspect artifact directory: {error}") from error
    if not stat.S_ISDIR(directory_mode):
        raise ArtifactError(f"artifact directory is not a real directory: {directory}")
    try:
        return tuple(sorted(directory.iterdir(), key=lambda path: path.name))
    except OSError as error:
        raise ArtifactError(f"cannot inspect artifact directory: {error}") from error

def _regular_artifact(entry: Path, suffixes: tuple[str, ...]) -> Path:
    try:
        entry_mode = entry.lstat().st_mode
    except OSError as error:
        raise ArtifactError(f"cannot inspect artifact entry: {error}") from error
    if not stat.S_ISREG(entry_mode):
        raise ArtifactError(f"artifact entry is not a regular file: {entry.name}")
    if not entry.name.endswith(suffixes):
        raise ArtifactError(f"artifact has an unexpected file type: {entry.name}")
    return entry


def _validate_artifact_count(
    artifacts: list[Path], *, exactly_one: bool, max_files: int | None
) -> None:
    """Apply count limits after every directory entry has been checked."""
    if not artifacts:
        raise ArtifactError(
            "artifact directory contains no approved release artifacts"
        )
    if exactly_one and len(artifacts) != 1:
        raise ArtifactError(f"expected exactly one artifact, found {len(artifacts)}")
    if max_files is not None and len(artifacts) > max_files:
        raise ArtifactError(
            f"expected at most {max_files} artifacts, found {len(artifacts)}"
        )


def canonical_package_name(value: str) -> str:
    """Return the PEP 503 spelling for a distribution name."""
    if not isinstance(value, str) or not value or not PACKAGE_NAME_PATTERN.fullmatch(value):
        raise ArtifactError(f"invalid Python package name: {value!r}")
    return re.sub(r"[-_.]+", "-", value).lower()


def _safe_archive_name(name: str, *, directory: bool = False) -> None:
    """Reject archive names that can escape their package root."""
    if not name or "\x00" in name or name.startswith("/") or "\\" in name:
        raise ArtifactError("artifact archive contains an unsafe path")
    # Tar directories commonly carry a trailing slash. Normalize that one
    # representation while rejecting all other ambiguous spellings.
    candidate = name[:-1] if directory and name.endswith("/") else name
    if not candidate or normpath(candidate) != candidate:
        raise ArtifactError("artifact archive contains an unsafe path")
    if any(part in {"", ".", ".."} for part in candidate.split("/")):
        raise ArtifactError("artifact archive contains an unsafe path")


def _read_bounded(stream: BinaryIO, label: str) -> bytes:
    payload = stream.read(MAX_METADATA_BYTES + 1)
    if len(payload) > MAX_METADATA_BYTES:
        raise ArtifactError(f"{label} metadata is unexpectedly large")
    return payload


def _metadata_fields(payload: bytes, label: str) -> tuple[str, str]:
    """Read exactly one Name and Version field from core metadata."""
    try:
        message = BytesParser(policy=policy.strict).parsebytes(payload)
    except (email_errors.MessageDefect, UnicodeDecodeError, ValueError) as error:
        raise ArtifactError(f"{label} metadata is invalid: {error}") from error
    if message.defects:
        raise ArtifactError(f"{label} metadata is malformed")
    names = [str(value).strip() for value in message.get_all("Name", [])]
    versions = [str(value).strip() for value in message.get_all("Version", [])]
    if len(names) != 1 or not names[0]:
        raise ArtifactError(f"{label} metadata must contain exactly one Name field")
    if len(versions) != 1 or not versions[0]:
        raise ArtifactError(f"{label} metadata must contain exactly one Version field")
    return names[0], versions[0]


def _validate_archive_members(
    members: Iterable[ArchiveMember],
    *,
    name_getter: Callable[[ArchiveMember], str],
    is_directory: Callable[[ArchiveMember], bool],
    is_regular: Callable[[ArchiveMember], bool],
    is_symlink: Callable[[ArchiveMember], bool],
    is_hardlink: Callable[[ArchiveMember], bool],
    size_getter: Callable[[ArchiveMember], int],
) -> None:
    """Validate common archive shape without extracting untrusted members."""
    # Kept as a small protocol helper so tar and zip checks cannot drift.
    seen: set[str] = set()
    total_size = 0
    for member in members:
        name = name_getter(member)
        directory = is_directory(member)
        _safe_archive_name(name, directory=directory)
        canonical_name = name[:-1] if directory and name.endswith("/") else name
        if canonical_name in seen:
            raise ArtifactError(f"artifact archive contains duplicate path: {name}")
        seen.add(canonical_name)
        if len(seen) > MAX_ARCHIVE_MEMBERS:
            raise ArtifactError("artifact archive contains too many members")
        if is_symlink(member) or is_hardlink(member):
            raise ArtifactError(f"artifact archive contains a link: {name}")
        if not directory and not is_regular(member):
            raise ArtifactError(f"artifact archive contains a non-regular member: {name}")
        size = size_getter(member)
        if size < 0:
            raise ArtifactError(f"artifact archive contains a negative member size: {name}")
        total_size += size
        if total_size > MAX_ARCHIVE_BYTES:
            raise ArtifactError("artifact archive is larger than the safety limit")


def _wheel_metadata(artifact: Path) -> tuple[str, str]:
    try:
        if artifact.stat().st_size > MAX_ARCHIVE_BYTES:
            raise ArtifactError("wheel artifact is larger than the safety limit")
        with zipfile.ZipFile(artifact) as archive:
            members = archive.infolist()
            if len(members) > MAX_ARCHIVE_MEMBERS:
                raise ArtifactError("artifact archive contains too many members")

            def is_symlink(member: zipfile.ZipInfo) -> bool:
                mode = (member.external_attr >> 16) & 0o170000
                return stat.S_ISLNK(mode)

            def is_regular(member: zipfile.ZipInfo) -> bool:
                mode = (member.external_attr >> 16) & 0o170000
                # Zip files created by common tooling leave the type bits at
                # zero. If present, they must identify a regular file.
                return mode in (0, stat.S_IFREG)

            _validate_archive_members(
                members,
                name_getter=lambda member: member.filename,
                is_directory=lambda member: member.is_dir(),
                is_regular=is_regular,
                is_symlink=is_symlink,
                is_hardlink=lambda _member: False,
                size_getter=lambda member: member.file_size,
            )
            metadata = []
            for member in members:
                parts = member.filename.split("/")
                if (
                    len(parts) >= 2
                    and parts[-2].endswith(".dist-info")
                    and parts[-1] == "METADATA"
                ):
                    metadata.append(member)
            if len(metadata) != 1:
                raise ArtifactError("wheel must contain exactly one dist-info/METADATA file")
            member = metadata[0]
            if member.is_dir() or is_symlink(member):
                raise ArtifactError("wheel metadata is not a regular file")
            with archive.open(member, "r") as stream:
                payload = _read_bounded(stream, "wheel")
    except (OSError, zipfile.BadZipFile, RuntimeError) as error:
        raise ArtifactError(f"cannot read wheel artifact {artifact.name}: {error}") from error
    return _metadata_fields(payload, f"wheel {artifact.name}")


def _sdist_metadata(artifact: Path) -> tuple[str, str]:
    try:
        if artifact.stat().st_size > MAX_ARCHIVE_BYTES:
            raise ArtifactError("source artifact is larger than the safety limit")
        with tarfile.open(artifact, mode="r:gz") as archive:
            members = archive.getmembers()
            if len(members) > MAX_ARCHIVE_MEMBERS:
                raise ArtifactError("artifact archive contains too many members")
            _validate_archive_members(
                members,
                name_getter=lambda member: member.name,
                is_directory=lambda member: member.isdir(),
                is_regular=lambda member: member.isreg(),
                is_symlink=lambda member: member.issym(),
                is_hardlink=lambda member: member.islnk(),
                size_getter=lambda member: member.size,
            )
            metadata = [member for member in members if Path(member.name).name == "PKG-INFO"]
            if len(metadata) != 1:
                raise ArtifactError("source archive must contain exactly one PKG-INFO file")
            member = metadata[0]
            if not member.isreg():
                raise ArtifactError("source metadata is not a regular file")
            if member.size > MAX_METADATA_BYTES:
                raise ArtifactError("source metadata is unexpectedly large")
            stream = archive.extractfile(member)
            if stream is None:
                raise ArtifactError("source metadata cannot be read")
            with stream:
                payload = _read_bounded(stream, "source")
    except (OSError, tarfile.TarError) as error:
        raise ArtifactError(f"cannot read source artifact {artifact.name}: {error}") from error
    return _metadata_fields(payload, f"source {artifact.name}")


def validate_python_artifacts(
    directory: Path,
    *,
    expected_package: str | None = None,
    expected_version: str | None = None,
    max_files: int | None = None,
) -> tuple[Path, ...]:
    """Validate Python distribution files and optionally their identity."""
    if (expected_package is None) != (expected_version is None):
        raise ArtifactError("expected package and version must be supplied together")
    expected_canonical = (
        canonical_package_name(expected_package) if expected_package is not None else None
    )
    if expected_version is not None and not VERSION_PATTERN.fullmatch(expected_version):
        raise ArtifactError("expected version must be exact SemVer major.minor.patch")

    artifacts = regular_files(
        directory,
        (".whl", ".tar.gz"),
        max_files=max_files,
    )
    kinds: set[str] = set()
    for artifact in artifacts:
        kind = ".whl" if artifact.name.endswith(".whl") else ".tar.gz"
        if kind in kinds:
            raise ArtifactError(f"more than one {kind} artifact is not allowed")
        kinds.add(kind)
        actual_package, actual_version = (
            _wheel_metadata(artifact) if kind == ".whl" else _sdist_metadata(artifact)
        )
        if expected_canonical is not None:
            if canonical_package_name(actual_package) != expected_canonical:
                raise ArtifactError(
                    f"artifact package {actual_package!r} does not match expected "
                    f"package {expected_package!r}"
                )
            if actual_version != expected_version:
                raise ArtifactError(
                    f"artifact version {actual_version!r} does not match expected "
                    f"version {expected_version!r}"
                )
    return artifacts


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--suffix", action="append", required=True)
    parser.add_argument("--exactly-one", action="store_true")
    parser.add_argument("--expected-package")
    parser.add_argument("--expected-version")
    parser.add_argument("--max-files", type=int)
    args = parser.parse_args()
    try:
        if args.expected_package is not None or args.expected_version is not None:
            if tuple(args.suffix) != (".whl", ".tar.gz"):
                raise ArtifactError(
                    "metadata validation requires --suffix .whl --suffix .tar.gz"
                )
            artifacts = validate_python_artifacts(
                args.directory,
                expected_package=args.expected_package,
                expected_version=args.expected_version,
                max_files=args.max_files,
            )
            if args.exactly_one and len(artifacts) != 1:
                raise ArtifactError(f"expected exactly one artifact, found {len(artifacts)}")
        else:
            artifacts = regular_files(
                args.directory,
                tuple(args.suffix),
                exactly_one=args.exactly_one,
                max_files=args.max_files,
            )
    except (ArtifactError, OSError) as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    print(f"validated {len(artifacts)} release artifact(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
