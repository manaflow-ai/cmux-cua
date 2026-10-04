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


if __name__ == "__main__":
    unittest.main()
