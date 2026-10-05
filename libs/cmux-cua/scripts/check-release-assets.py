#!/usr/bin/env python3
"""Check cmux-cua release archives and the published asset set.

usage:
  check-release-assets.py archive --license LICENSE ARCHIVE...
  check-release-assets.py expected --version X.Y.Z --macos signed|unsigned
  check-release-assets.py sums DIR
  check-release-assets.py release --version X.Y.Z --macos signed|unsigned --license LICENSE DIR

archive   Open each archive. A directory archive (cmux-cua-V-LABEL.tar.gz or
          .zip) must hold only the top directory cmux-cua-V-LABEL, with the
          binary (executable) and LICENSE (equal to the repository license)
          in it; a macOS one also holds the app bundle. A bare binary archive
          (*-binary.*) holds the binaries at the root and nothing else.
expected  Print the asset names a complete release has, one per line.
sums      Print SHA256SUMS (sha256sum format) for every file in DIR except
          SHA256SUMS and checksums.txt, sorted by name.
release   DIR holds the downloaded release assets. The names must equal the
          expected set, SHA256SUMS must list every other asset with its hash,
          and every archive must pass the archive check.

Exit 1 with every reason on failure. Python 3.9+ (the Linux build container
is Debian 11).
"""
import argparse
import hashlib
import re
import stat
import sys
import tarfile
import zipfile
from pathlib import Path

SUM_FILES = ("SHA256SUMS", "checksums.txt")
SCRIPTS = ("install.sh", "install.ps1", "_install-rust.sh", "uninstall.sh", "uninstall.ps1")
APP_BINARY = "cmux Computer Use.app/Contents/MacOS/cmux-cua"
ARCHIVE_RE = re.compile(
    r"cmux-cua-(?P<version>[0-9]+\.[0-9]+\.[0-9]+)-(?P<label>(?:linux|darwin|windows)-[a-z0-9_]+(?:-unsigned)?)"
    r"(?P<binary>-binary)?\.(?P<ext>tar\.gz|zip)"
)


def macos_labels(macos):
    if macos == "unsigned":
        return ["darwin-universal-unsigned"], "darwin-universal-unsigned"
    return ["darwin-arm64", "darwin-x86_64", "darwin-universal"], "darwin-universal"


def expected_assets(version, macos):
    names = []
    for arch in ("x86_64", "arm64"):
        names += [f"cmux-cua-{version}-linux-{arch}.tar.gz", f"cmux-cua-{version}-linux-{arch}-binary.tar.gz",
                  f"cmux-cua-{version}-windows-{arch}.zip", f"cmux-cua-{version}-windows-{arch}-binary.zip"]
    dirs, universal = macos_labels(macos)
    names += [f"cmux-cua-{version}-{label}.tar.gz" for label in dirs]
    names.append(f"cmux-cua-{version}-{universal}-binary.tar.gz")
    names.append(f"cmux-cua-v{version}-skills.tar.gz")
    names += list(SCRIPTS) + list(SUM_FILES)
    return sorted(names)


def read_members(path):
    """Return {name: (is_file, executable, read)} for a .tar.gz or .zip."""
    members = {}
    if path.name.endswith(".zip"):
        z = zipfile.ZipFile(path)
        for info in z.infolist():
            name = info.filename.rstrip("/")
            members[name] = (not info.is_dir(), True, lambda i=info: z.read(i))
    else:
        tar = tarfile.open(path, "r:gz")
        for info in tar.getmembers():
            name = info.name.rstrip("/")
            if name.startswith("./"):
                name = name[2:]
            if not name or name == ".":
                continue
            members[name] = (info.isfile(), bool(info.mode & stat.S_IXUSR),
                             lambda i=info: tar.extractfile(i).read())
    return members


def check_archive(path, license_bytes):
    """Return a list of problems with one archive."""
    m = ARCHIVE_RE.fullmatch(path.name)
    if not m:
        return [f"{path.name}: not a cmux-cua binary archive name"]
    try:
        members = read_members(path)
    except (tarfile.TarError, zipfile.BadZipFile, OSError, EOFError) as e:
        return [f"{path.name}: cannot open: {e}"]
    label = m.group("label")
    windows = label.startswith("windows-")
    binaries = ["cmux-cua.exe", "cmux-cua-uia.exe"] if windows else ["cmux-cua"]
    problems = []

    def need_file(name, executable):
        entry = members.get(name)
        if entry is None or not entry[0]:
            problems.append(f"{path.name}: missing {name}")
        elif executable and not entry[1]:
            problems.append(f"{path.name}: {name} is not executable")

    if m.group("binary"):
        extra = sorted(set(members) - set(binaries))
        if extra:
            problems.append(f"{path.name}: bare binary archive holds more than {binaries}: {extra}")
        for b in binaries:
            need_file(b, True)
        return problems

    top = f"cmux-cua-{m.group('version')}-{label}"
    outside = sorted(n for n in members if n != top and not n.startswith(top + "/"))
    if outside:
        problems.append(f"{path.name}: members outside {top}/: {outside[:5]}")
    for b in binaries:
        need_file(f"{top}/{b}", True)
    if label.startswith("darwin-"):
        need_file(f"{top}/{APP_BINARY}", True)
    lic = f"{top}/LICENSE"
    need_file(lic, False)
    entry = members.get(lic)
    if entry is not None and entry[0] and entry[2]() != license_bytes:
        problems.append(f"{path.name}: {lic} differs from the repository license")
    return problems


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def sums_lines(directory):
    files = sorted(p for p in directory.iterdir() if p.is_file() and p.name not in SUM_FILES)
    return [f"{sha256(p)}  {p.name}" for p in files]


def check_release(directory, version, macos, license_bytes):
    problems = []
    present = {p.name for p in directory.iterdir() if p.is_file()}
    expected = set(expected_assets(version, macos))
    for name in sorted(expected - present):
        problems.append(f"missing asset: {name}")
    for name in sorted(present - expected):
        problems.append(f"unexpected asset: {name}")

    sums_path = directory / "SHA256SUMS"
    if sums_path.is_file():
        listed = {}
        for line in sums_path.read_text().splitlines():
            parts = line.split()
            if len(parts) != 2:
                problems.append(f"SHA256SUMS: malformed line {line!r}")
                continue
            listed[parts[1].lstrip("*")] = parts[0]
        for name in sorted(present - set(SUM_FILES)):
            if name not in listed:
                problems.append(f"SHA256SUMS: no entry for {name}")
            elif listed[name] != sha256(directory / name):
                problems.append(f"SHA256SUMS: hash mismatch for {name}")
        for name in sorted(set(listed) - present):
            problems.append(f"SHA256SUMS: lists {name}, which is not an asset")

    for name in sorted(present):
        if ARCHIVE_RE.fullmatch(name):
            problems += check_archive(directory / name, license_bytes)
    return problems


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    a = sub.add_parser("archive")
    a.add_argument("--license", required=True, type=Path)
    a.add_argument("archives", nargs="+", type=Path)
    e = sub.add_parser("expected")
    e.add_argument("--version", required=True)
    e.add_argument("--macos", required=True, choices=("signed", "unsigned"))
    s = sub.add_parser("sums")
    s.add_argument("dir", type=Path)
    r = sub.add_parser("release")
    r.add_argument("--version", required=True)
    r.add_argument("--macos", required=True, choices=("signed", "unsigned"))
    r.add_argument("--license", required=True, type=Path)
    r.add_argument("dir", type=Path)
    args = parser.parse_args(argv)

    if args.cmd == "expected":
        print("\n".join(expected_assets(args.version, args.macos)))
        return 0
    if args.cmd == "sums":
        print("\n".join(sums_lines(args.dir)))
        return 0
    license_bytes = args.license.read_bytes()
    if args.cmd == "archive":
        problems = []
        for path in args.archives:
            problems += check_archive(path, license_bytes)
        checked = len(args.archives)
    else:
        problems = check_release(args.dir, args.version, args.macos, license_bytes)
        checked = len(list(args.dir.iterdir()))
    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 1
    print(f"ok: {checked} file(s) checked")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
