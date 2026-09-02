"""Behavior tests for the inline release-reminder workflow script."""

from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]
WORKFLOW_PATH = REPO_ROOT / ".github/workflows/ci-release-reminder.yml"


def read_workflow_script() -> str:
    """Extract the bash block that GitHub runs for the reminder step."""

    lines = WORKFLOW_PATH.read_text(encoding="utf-8").splitlines()
    run_line = next(index for index, line in enumerate(lines) if line.strip() == "run: |")
    script_lines: list[str] = []
    for line in lines[run_line + 1 :]:
        if line.startswith("          "):
            script_lines.append(line[10:])
        elif not line:
            script_lines.append("")
        else:
            break
    return "\n".join(script_lines) + "\n"


class TestReleaseReminderWorkflow(unittest.TestCase):
    """Exercise the reminder's API failure behavior with a fake GitHub CLI."""

    def test_comment_lookup_failure_still_posts_reminder(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary_path = Path(temporary_directory)
            bin_path = temporary_path / "bin"
            bin_path.mkdir()
            comment_path = temporary_path / "comment.md"

            (bin_path / "gh").write_text(
                """#!/bin/sh
set -eu
if [ \"$1\" = pr ] && [ \"$2\" = diff ]; then
  printf '%s\\n' 'libs/python/cua-cli/example.py'
  exit 0
fi
if [ \"$1\" = api ]; then
  exit 42
fi
if [ \"$1\" = pr ] && [ \"$2\" = comment ]; then
  cat > \"$GH_COMMENT_FILE\"
  exit 0
fi
printf 'unexpected gh invocation: %s\\n' \"$*\" >&2
exit 1
""",
                encoding="utf-8",
            )
            (bin_path / "gh").chmod(0o755)
            (bin_path / "timeout").write_text(
                """#!/bin/sh
set -eu
[ \"$1\" = 30s ]
shift
exec \"$@\"
""",
                encoding="utf-8",
            )
            (bin_path / "timeout").chmod(0o755)

            environment = os.environ.copy()
            environment.update(
                {
                    "GH_COMMENT_FILE": str(comment_path),
                    "GH_TOKEN": "test-token",
                    "LABELS_JSON": '["release:pypi/cli"]',
                    "PR_NUMBER": "123",
                    "REPO": "manaflow-ai/cmux-cua",
                    "PATH": f"{bin_path}{os.pathsep}{environment['PATH']}",
                }
            )
            result = subprocess.run(
                ["bash", "-c", read_workflow_script()],
                cwd=REPO_ROOT,
                env=environment,
                capture_output=True,
                check=False,
                text=True,
                timeout=10,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Could not inspect the previous release reminder", result.stdout)
            self.assertTrue(comment_path.exists())
            self.assertIn("<!-- release-reminder -->", comment_path.read_text(encoding="utf-8"))
            self.assertIn("pypi/cli", comment_path.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
