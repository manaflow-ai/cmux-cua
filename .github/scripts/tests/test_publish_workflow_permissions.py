"""Static security contracts for package and container release workflows.

These checks intentionally inspect workflow structure rather than implementation
details.  They prevent a future edit from restoring a broad token at workflow
scope, making a build job registry-capable, or reintroducing mutable action
references in the reusable publishers.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
WORKFLOWS = ROOT / ".github" / "workflows"

PY_CALLERS = sorted(WORKFLOWS.glob("cd-py-*.yml"))
PY_CALLERS = [path for path in PY_CALLERS if path.name != "cd-py-cua-driver.yml"]
TS_CALLERS = sorted(WORKFLOWS.glob("cd-ts-*.yml"))
CONTAINER_CALLERS = sorted(WORKFLOWS.glob("cd-container-*.yml"))


def lines(path: Path) -> list[str]:
    return path.read_text(encoding="utf-8").splitlines()


def job_block(path: Path, job_name: str) -> list[str]:
    """Return one top-level job block from a workflow."""

    source = lines(path)
    start = next(
        index
        for index, line in enumerate(source)
        if line == f"  {job_name}:"
    )
    end = len(source)
    for index in range(start + 1, len(source)):
        if re.match(r"^  [A-Za-z0-9_-]+:", source[index]):
            end = index
            break
    return source[start:end]


def permission_map(block: list[str]) -> dict[str, str]:
    """Read a job's explicit permission map without a YAML dependency."""

    try:
        start = next(index for index, line in enumerate(block) if line == "    permissions:")
    except StopIteration:
        return {}

    values: dict[str, str] = {}
    for line in block[start + 1 :]:
        match = re.fullmatch(r"      ([a-z-]+): (read|write|none)", line)
        if match:
            values[match.group(1)] = match.group(2)
        elif line and not line.startswith("      "):
            break
    return values


def caller_jobs(path: Path, workflow: str) -> list[tuple[str, dict[str, str]]]:
    """Find jobs that call one of the local reusable publish workflows."""

    source = lines(path)
    jobs: list[tuple[str, dict[str, str]]] = []
    for index, line in enumerate(source):
        match = re.fullmatch(r"  ([A-Za-z0-9_-]+):", line)
        if not match:
            continue
        job_name = match.group(1)
        block = job_block(path, job_name)
        if f"    uses: ./.github/workflows/{workflow}" in block:
            jobs.append((job_name, permission_map(block)))
    return jobs


class PublishWorkflowPermissionsTests(unittest.TestCase):
    def assert_empty_workflow_permissions(self, path: Path) -> None:
        self.assertIn(
            "permissions: {}",
            lines(path),
            f"{path.name} must default the workflow token to no permissions",
        )

    def assert_pinned_actions(self, path: Path) -> None:
        for line_number, line in enumerate(lines(path), start=1):
            if "uses:" not in line or "./.github/" in line:
                continue
            match = re.search(r"uses:\s+[^\s@]+@([^\s#]+)", line)
            self.assertIsNotNone(match, f"{path.name}:{line_number} has an invalid uses entry")
            reference = match.group(1) if match else ""
            self.assertRegex(
                reference,
                r"^[0-9a-f]{40}$",
                f"{path.name}:{line_number} uses a mutable action reference {reference!r}",
            )

    def test_reusable_workflows_have_least_privilege_jobs(self) -> None:
        reusable = {
            "py-reusable-publish.yml": {
                "build": {"contents": "read"},
                "publish": {"actions": "read", "id-token": "write"},
            },
            "ts-reusable-publish.yml": {
                "build": {"contents": "read"},
                "publish": {"actions": "read", "id-token": "write"},
            },
            "docker-reusable-publish.yml": {
                "build-and-push": {"contents": "read"},
                "publish-manifest-list": {"actions": "read"},
            },
            "release-github-reusable.yml": {
                "create-release": {"actions": "read", "contents": "write"},
            },
        }
        for filename, jobs in reusable.items():
            path = WORKFLOWS / filename
            self.assert_empty_workflow_permissions(path)
            self.assert_pinned_actions(path)
            for job_name, expected in jobs.items():
                actual = permission_map(job_block(path, job_name))
                for permission, level in expected.items():
                    self.assertEqual(
                        actual.get(permission),
                        level,
                        f"{filename}:{job_name} must grant {permission}:{level}",
                    )
                if job_name in {"build", "build-and-push"}:
                    self.assertNotIn("id-token", actual)

    def test_publish_secrets_are_optional_and_builds_do_not_read_them(self) -> None:
        py = (WORKFLOWS / "py-reusable-publish.yml").read_text(encoding="utf-8")
        ts = (WORKFLOWS / "ts-reusable-publish.yml").read_text(encoding="utf-8")
        self.assertRegex(py, r"PYPI_TOKEN:\n\s+required: false")
        self.assertRegex(ts, r"NPM_TOKEN:\n\s+required: false")
        self.assertNotIn("PYPI_TOKEN: ${{ secrets.PYPI_TOKEN }}", "\n".join(job_block(WORKFLOWS / "py-reusable-publish.yml", "build")))
        self.assertNotIn("NPM_TOKEN: ${{ secrets.NPM_TOKEN }}", "\n".join(job_block(WORKFLOWS / "ts-reusable-publish.yml", "build")))
        self.assertIn("pypa/gh-action-pypi-publish@dc37677b2e1c63e2034f94d8a5b11f265b73ba33", py)
        self.assertIn("steps.auth.outputs.mode == 'oidc'", py)
        self.assertIn("steps.auth.outputs.mode == 'token'", py)
        self.assertIn("npm publish ./*.tgz --access public --provenance --ignore-scripts", ts)
        self.assertIn("trap 'rm -f \"$npm_config_userconfig\"' EXIT", ts)

    def test_all_publish_callers_clear_workflow_defaults(self) -> None:
        for path in [*PY_CALLERS, *TS_CALLERS, *CONTAINER_CALLERS]:
            self.assert_empty_workflow_permissions(path)
            self.assertNotIn(
                "workflow_dispatch:",
                "\n".join(lines(path)),
                f"{path.name} must not expose a branch-selectable secret publisher",
            )

        for path in PY_CALLERS:
            calls = caller_jobs(path, "py-reusable-publish.yml")
            self.assertEqual(len(calls), 1, path.name)
            _, permissions = calls[0]
            for key, level in {
                "contents": "read",
                "actions": "read",
                "id-token": "write",
            }.items():
                self.assertEqual(permissions.get(key), level, f"{path.name} publish permission")

        for path in TS_CALLERS:
            calls = caller_jobs(path, "ts-reusable-publish.yml")
            self.assertEqual(len(calls), 1, path.name)
            _, permissions = calls[0]
            for key, level in {
                "contents": "read",
                "actions": "read",
                "id-token": "write",
            }.items():
                self.assertEqual(permissions.get(key), level, f"{path.name} publish permission")
            self.assertNotIn("packages: write", "\n".join(lines(path)))

        for path in CONTAINER_CALLERS:
            calls = caller_jobs(path, "docker-reusable-publish.yml")
            self.assertEqual(len(calls), 1, path.name)
            _, permissions = calls[0]
            for key, level in {
                "contents": "read",
                "actions": "read",
            }.items():
                self.assertEqual(permissions.get(key), level, f"{path.name} publish permission")

    def test_docker_login_is_not_run_for_pull_requests(self) -> None:
        source = (WORKFLOWS / "docker-reusable-publish.yml").read_text(encoding="utf-8")
        self.assertNotIn("workflow_dispatch:", source)
        login_blocks = re.findall(
            r"(?ms)^      - name: Login to Docker Hub\n(.*?)(?=^      - name:|^  [A-Za-z0-9_-]+:|\Z)",
            source,
        )
        self.assertEqual(len(login_blocks), 2)
        for block in login_blocks:
            self.assertIn("github.event_name != 'pull_request'", block)


if __name__ == "__main__":
    unittest.main()
