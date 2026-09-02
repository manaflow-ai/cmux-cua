"""Behavior tests for the canonical publisher ownership gate."""

from __future__ import annotations

import os
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPT_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPT_DIR))

from validate_publisher_repository import CANONICAL_REPOSITORY, main, validate


class PublisherRepositoryTests(unittest.TestCase):
    def test_canonical_repository_is_allowed(self) -> None:
        validate(CANONICAL_REPOSITORY)

    def test_fork_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "canonical registry owner"):
            validate("manaflow-ai/cmux-cua")

    def test_missing_repository_is_rejected(self) -> None:
        with patch.dict(os.environ, {}, clear=True):
            self.assertEqual(main(), 1)

    def test_environment_repository_is_checked_exactly(self) -> None:
        with patch.dict(os.environ, {"GITHUB_REPOSITORY": CANONICAL_REPOSITORY}):
            self.assertEqual(main(), 0)
        with patch.dict(os.environ, {"GITHUB_REPOSITORY": "trycua/cua-fork"}):
            self.assertEqual(main(), 1)


if __name__ == "__main__":
    unittest.main()
