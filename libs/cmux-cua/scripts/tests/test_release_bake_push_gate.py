#!/usr/bin/env python3
"""The release job's bake-version push runs only with its App credentials.

Tag run 37221207278 (cmux-cua-v0.8.2) published the release, then failed in
"Generate GitHub App token (for bake-version push)": `Input required and not
supplied: app-id` (RELEASE_APP_ID is not set in this repo). The version-only
release commit already bakes the installers (check-release-version.py
enforces it), so without the App the push steps are skipped and the run is
green.
"""
import re
import unittest
from pathlib import Path

WORKFLOW = Path(__file__).resolve().parents[4] / ".github/workflows/cd-rust-cmux-cua.yml"


def step_block(text: str, name: str) -> str:
    start = text.index(f"- name: {name}")
    nxt = text.find("\n      - ", start + 1)
    return text[start: nxt if nxt != -1 else len(text)]


class BakePushGateTest(unittest.TestCase):
    def test_the_push_steps_need_the_app_credentials(self):
        text = WORKFLOW.read_text()
        probe = step_block(text, "Bake push credentials")
        self.assertIn("secrets.RELEASE_APP_ID", probe)
        self.assertIn("secrets.RELEASE_APP_PRIVATE_KEY", probe)
        for name in ("Generate GitHub App token (for bake-version push)",
                     "Bake version into install scripts (commit + push)"):
            cond = re.search(r"^\s*if: (.+)$", step_block(text, name), re.M)
            self.assertIsNotNone(cond, name)
            self.assertIn("steps.bake-creds.outputs.present == 'true'", cond.group(1), name)


if __name__ == "__main__":
    unittest.main()
