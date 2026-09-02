"""Behavior tests for release version resolution."""

from __future__ import annotations

import importlib.util
import json
import pathlib
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "resolve_release_version", ROOT / "resolve_release_version.py"
)
if SPEC is None or SPEC.loader is None:  # pragma: no cover
    raise RuntimeError("could not load release version resolver")
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class ResolveReleaseVersionTests(unittest.TestCase):
    def test_push_requires_exact_prefix_and_semver(self) -> None:
        self.assertEqual(
            MODULE.resolve_version(
                event_name="push",
                ref_type="tag",
                ref_name="npm-cli-v1.2.3",
                version_input="",
                tag_prefix="npm-cli-v",
            ),
            ("1.2.3", "npm-cli-v1.2.3"),
        )

    def test_manual_version_is_used_without_a_tag(self) -> None:
        self.assertEqual(
            MODULE.resolve_version(
                event_name="workflow_dispatch",
                ref_type="branch",
                ref_name="main",
                version_input="2.0.0",
                tag_prefix="agent-v",
            ),
            ("2.0.0", "agent-v2.0.0"),
        )

    def test_manual_version_can_be_read_from_package_json(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            package = pathlib.Path(directory) / "package.json"
            package.write_text(json.dumps({"version": "3.4.5"}), encoding="utf-8")
            self.assertEqual(
                MODULE.resolve_version(
                    event_name="workflow_dispatch",
                    ref_type="branch",
                    ref_name="main",
                    version_input="",
                    tag_prefix="npm-core-v",
                    package_json=str(package),
                ),
                ("3.4.5", "npm-core-v3.4.5"),
            )

    def test_workflow_run_uses_validated_source_tag(self) -> None:
        self.assertEqual(
            MODULE.resolve_version(
                event_name="workflow_run",
                ref_type="branch",
                ref_name="main",
                version_input="",
                tag_prefix="core-v",
                source_tag="core-v4.5.6",
            ),
            ("4.5.6", "core-v4.5.6"),
        )

    def test_workflow_run_requires_matching_validated_source_tag(self) -> None:
        with self.assertRaisesRegex(MODULE.VersionError, "validated source tag"):
            MODULE.resolve_version(
                event_name="workflow_run",
                ref_type="branch",
                ref_name="main",
                version_input="",
                tag_prefix="core-v",
                source_tag="other-v4.5.6",
            )

    def test_rejects_untrusted_event_and_version(self) -> None:
        with self.assertRaisesRegex(MODULE.VersionError, "only workflow_run"):
            MODULE.resolve_version(
                event_name="workflow_call",
                ref_type="branch",
                ref_name="main",
                version_input="1.0.0",
                tag_prefix="agent-v",
            )
        with self.assertRaisesRegex(MODULE.VersionError, "exact SemVer"):
            MODULE.resolve_version(
                event_name="workflow_dispatch",
                ref_type="branch",
                ref_name="main",
                version_input="1.0.0-rc1",
                tag_prefix="agent-v",
            )
        with self.assertRaisesRegex(MODULE.VersionError, "must use a tag"):
            MODULE.resolve_version(
                event_name="push",
                ref_type="branch",
                ref_name="agent-v1.0.0",
                version_input="",
                tag_prefix="agent-v",
            )


if __name__ == "__main__":
    unittest.main()
