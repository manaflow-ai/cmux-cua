"""Declarative security contracts for the CLA workflow.

GitHub's concurrency and token permission behavior is not reproducible in a
local unit test. Keep these small checks next to the behavioral refresh tests
so a workflow edit cannot silently remove the shared ledger queue or broaden
the merged-PR lock token.
"""

from __future__ import annotations

import pathlib
import re
import unittest


WORKFLOW = pathlib.Path(__file__).parents[2] / "workflows" / "cla.yml"


def job_block(workflow: str, job_id: str) -> str:
    match = re.search(
        rf"(?ms)^  {re.escape(job_id)}:\n(?:(?!^  [A-Za-z0-9_-]+:).)*",
        workflow,
    )
    if match is None:
        raise AssertionError(f"workflow job {job_id!r} is missing")
    return match.group(0)


def field(block: str, name: str) -> str:
    match = re.search(rf"(?m)^\s+{re.escape(name)}:\s*(.+)$", block)
    if match is None:
        raise AssertionError(f"workflow field {name!r} is missing")
    return match.group(1).strip()


class ClaWorkflowContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = WORKFLOW.read_text(encoding="utf-8")

    def test_ledger_writers_share_a_stable_non_canceling_queue(self):
        groups = []
        for job_id in ("CLALedgerWriter", "CLASigner"):
            block = job_block(self.workflow, job_id)
            groups.append(field(block, "group"))
            self.assertEqual(field(block, "cancel-in-progress"), "false")
            self.assertNotIn("github.run_id", groups[-1])
            self.assertNotIn("github.run_attempt", groups[-1])
        self.assertEqual(groups, ["cla-signatures-v3-${{ github.repository }}"] * 2)

    def test_merged_lock_permissions_are_exact(self):
        block = job_block(self.workflow, "LockMergedPullRequest")
        permissions = re.search(r"(?ms)^    permissions:\n(?P<body>(?:      [^\n]+\n)+)", block)
        self.assertIsNotNone(permissions)
        assert permissions is not None
        self.assertEqual(
            {
                line.split("#", 1)[0].strip()
                for line in permissions.group("body").splitlines()
                if line.strip()
            },
            {"contents: read", "issues: write", "pull-requests: write"},
        )


if __name__ == "__main__":
    unittest.main()
