#!/usr/bin/env python3
"""Every shell use of the bundle path "cmux Computer Use.app" is quoted.

Tag run 37219661069 (cmux-cua-v0.8.1) failed in "Assemble cmux Computer Use.app
bundle" with `cp: Use.app/Contents: Not a directory`: refactor 886aed7e renamed
the bundle to a name with spaces, and the workflow's unquoted paths split into
three words. Comments and step names are not shell and are skipped.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
FILES = [
    ROOT / ".github/workflows/cd-rust-cmux-cua.yml",
    ROOT / "libs/cmux-cua/scripts/_install-rust.sh",
    ROOT / "libs/cmux-cua/scripts/_install-local-rust.sh",
    ROOT / "libs/cmux-cua/scripts/uninstall.sh",
]
NAME = "Computer Use.app"


def unquoted_uses(line: str) -> bool:
    """True when NAME appears outside single and double quotes on this shell line."""
    out, quote = [], None
    for ch in line:
        if quote:
            if ch == quote:
                quote = None
            out.append(" " if ch != quote else " ")
            continue
        if ch in "\"'":
            quote = ch
            out.append(" ")
            continue
        out.append(ch)
    return NAME in "".join(out) or "Computer Use" in "".join(out)


class BundlePathQuotingTest(unittest.TestCase):
    def test_every_shell_use_is_quoted(self):
        bad = []
        for path in FILES:
            for number, line in enumerate(path.read_text().splitlines(), 1):
                text = line.strip()
                if NAME not in text or text.startswith("#") or re.match(r"^-?\s*name:", text):
                    continue
                if path.suffix == ".yml" and re.match(r"^- `", text):
                    continue  # release-notes markdown inside the body
                if unquoted_uses(text):
                    bad.append(f"{path.relative_to(ROOT)}:{number}: {text}")
        self.assertEqual(bad, [], "\n" + "\n".join(bad))


if __name__ == "__main__":
    unittest.main()
