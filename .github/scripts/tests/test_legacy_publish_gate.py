"""Behavior tests for the opt-in legacy registry-token gate."""

from __future__ import annotations

import os
import subprocess
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "validate-legacy-publish-gate.sh"


class LegacyPublishGateTests(unittest.TestCase):
    def run_gate(self, **updates: str) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        for key in ("ALLOW_LEGACY_TOKEN", "LEGACY_TOKEN_GATE", "REGISTRY_TOKEN"):
            environment.pop(key, None)
        environment.update(updates)
        return subprocess.run(
            ["bash", str(SCRIPT)],
            env=environment,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_missing_token_fails_closed(self) -> None:
        result = self.run_gate(
            ALLOW_LEGACY_TOKEN="true",
            LEGACY_TOKEN_GATE="enabled",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("token is missing", result.stderr)

    def test_unapproved_gate_fails_closed(self) -> None:
        result = self.run_gate(
            ALLOW_LEGACY_TOKEN="true",
            LEGACY_TOKEN_GATE="disabled",
            REGISTRY_TOKEN="secret",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("environment is not enabled", result.stderr)

    def test_opt_in_gate_allows_token(self) -> None:
        result = self.run_gate(
            ALLOW_LEGACY_TOKEN="true",
            LEGACY_TOKEN_GATE="enabled",
            REGISTRY_TOKEN="secret",
        )
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
