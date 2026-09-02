#!/usr/bin/env python3
"""Bind a privileged workflow run to the current protected ``main`` commit."""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path


SHA_RE = re.compile(r"[0-9a-fA-F]{40}\Z")


class ValidationError(RuntimeError):
    """Raised when a manual request is not bound to protected main."""


def required(name: str) -> str:
    value = os.environ.get(name, "")
    if not value:
        raise ValidationError(f"{name} is required")
    return value


def git(*args: str) -> str:
    try:
        result = subprocess.run(
            ["git", *args],
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        detail = getattr(error, "stderr", "") or str(error)
        raise ValidationError(f"git {' '.join(args)} failed: {detail.strip()}") from error
    return result.stdout.strip()


def full_sha(name: str) -> str:
    value = required(name).lower()
    if not SHA_RE.fullmatch(value):
        raise ValidationError(f"{name} must be a full 40-character hexadecimal commit")
    return value


def write_output(commit: str) -> None:
    output_path = Path(required("GITHUB_OUTPUT"))
    with output_path.open("a", encoding="utf-8") as output:
        output.write(f"commit={commit}\n")


def main() -> int:
    try:
        repository = required("REPOSITORY")
        expected_repository = required("EXPECTED_REPOSITORY")
        if repository != expected_repository:
            raise ValidationError(
                f"workflow repository {repository!r} is not {expected_repository!r}"
            )

        event_name = required("EVENT_NAME")
        if event_name not in {"schedule", "workflow_run"}:
            raise ValidationError(f"unsupported privileged event {event_name!r}")
        if os.environ.get("TRUSTED_REF_PROTECTED", "").lower() != "true":
            raise ValidationError("privileged ref is not covered by a protected branch rule")

        trusted_sha = full_sha("TRUSTED_SHA")
        if event_name == "workflow_run":
            source_event = required("SOURCE_EVENT")
            source_conclusion = required("SOURCE_CONCLUSION")
            source_branch = required("SOURCE_BRANCH")
            source_repository = required("SOURCE_REPOSITORY")
            source_sha = full_sha("SOURCE_SHA")
            if source_event != "workflow_dispatch":
                raise ValidationError("source workflow was not a manual request")
            if source_conclusion != "success":
                raise ValidationError("manual request did not complete successfully")
            if source_branch != "main":
                raise ValidationError("manual request must run from protected main")
            if source_repository != expected_repository:
                raise ValidationError("manual request came from another repository")
        else:
            source_sha = trusted_sha

        git(
            "fetch",
            "--no-tags",
            "origin",
            "refs/heads/main:refs/remotes/origin/main",
        )
        main_sha = git("rev-parse", "refs/remotes/origin/main^{commit}").lower()
        if not SHA_RE.fullmatch(main_sha):
            raise ValidationError("origin/main did not resolve to a full commit SHA")
        if trusted_sha != main_sha:
            raise ValidationError(
                f"privileged workflow checkout {trusted_sha} is not current main {main_sha}"
            )
        if source_sha != main_sha:
            raise ValidationError(
                f"manual request source {source_sha} is not current main {main_sha}"
            )

        write_output(main_sha)
        print(f"Validated privileged run against protected main {main_sha}")
        return 0
    except ValidationError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
