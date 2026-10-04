#!/usr/bin/env python3
"""check-release-version.py: a cmux-cua-vX.Y.Z tag must name the workspace version.

The CD workflow took its version from the tag alone, so a tag on a commit whose
Cargo.toml says another version published binaries that report the wrong one.
"""
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check-release-version.py"


def check(version_toml: str, *args: str) -> subprocess.CompletedProcess:
    with tempfile.TemporaryDirectory() as tmp:
        cargo = Path(tmp) / "Cargo.toml"
        cargo.write_text(f'[workspace]\nmembers = []\n\n[workspace.package]\nversion = "{version_toml}"\n')
        return subprocess.run([sys.executable, str(SCRIPT), str(cargo), *args], capture_output=True, text=True)


class CheckReleaseVersionTest(unittest.TestCase):
    def test_matching_tag_passes_and_prints_the_version(self):
        done = check("0.8.0", "refs/tags/cmux-cua-v0.8.0")
        self.assertEqual((done.returncode, done.stdout.strip()), (0, "0.8.0"), done.stderr)

    def test_mismatch_or_malformed_tag_fails(self):
        for ref in ("refs/tags/cmux-cua-v0.7.1", "refs/tags/cmux-cua-v0.8", "refs/tags/v0.8.0", "refs/heads/main"):
            self.assertNotEqual(check("0.8.0", ref).returncode, 0, ref)

    def test_dispatch_version_must_match_too(self):
        self.assertEqual(check("0.8.0", "refs/heads/cmux-cua-native", "0.8.0").returncode, 0)
        self.assertNotEqual(check("0.8.0", "refs/heads/cmux-cua-native", "0.9.0").returncode, 0)


class EveryVersionStringTest(unittest.TestCase):
    """The release's other version strings (Python package, baked installer
    versions, bumpversion) must match the workspace too: on 2026-10-04 the
    0.8.0 release commit left five of them at 0.7.1."""

    def test_the_repository_names_one_version_everywhere(self):
        cargo = SCRIPT.parents[1] / "rust" / "Cargo.toml"
        import tomllib
        version = tomllib.loads(cargo.read_text())["workspace"]["package"]["version"]
        done = subprocess.run([sys.executable, str(SCRIPT), str(cargo), f"refs/tags/cmux-cua-v{version}"],
                              capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)

    def test_a_stale_string_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "rust").mkdir()
            (root / "rust" / "Cargo.toml").write_text('[workspace.package]\nversion = "0.8.0"\n')
            (root / "python").mkdir()
            (root / "python" / "pyproject.toml").write_text('[project]\nname = "cmux-cua"\nversion = "0.7.1"\n')
            done = subprocess.run([sys.executable, str(SCRIPT), str(root / "rust" / "Cargo.toml"),
                                   "refs/tags/cmux-cua-v0.8.0"], capture_output=True, text=True)
            self.assertNotEqual(done.returncode, 0)
            self.assertIn("pyproject.toml", done.stderr)


class VersionFilesExistTest(unittest.TestCase):
    """CU lead review of ca4b5800: the check skipped a listed file that does not
    exist, so a renamed file in the real repo would go unchecked."""

    def test_every_listed_version_file_exists_in_the_repo(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location("crv", SCRIPT)
        crv = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(crv)
        root = SCRIPT.parents[1]
        missing = [rel for rel, _ in crv.VERSION_STRINGS if not (root / rel).is_file()]
        self.assertEqual(missing, [])

    def test_a_missing_listed_file_fails_the_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "rust").mkdir()
            (root / "rust" / "Cargo.toml").write_text('[workspace.package]\nversion = "0.8.0"\n')
            done = subprocess.run([sys.executable, str(SCRIPT), str(root / "rust" / "Cargo.toml"),
                                   "refs/tags/cmux-cua-v0.8.0"], capture_output=True, text=True)
            self.assertNotEqual(done.returncode, 0, "a release whose version files are missing passed")
            self.assertIn("missing", done.stderr)

if __name__ == "__main__":
    unittest.main()
