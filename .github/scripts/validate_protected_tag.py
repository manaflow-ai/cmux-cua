#!/usr/bin/env python3
"""Validate that a release tag names the exact commit that is on ``main``.

The release workflows call this script before any signing or publishing job.
It treats all event values as untrusted strings, resolves the tag and main
branch through Git, and emits only validated values to ``GITHUB_OUTPUT``.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path


SHA_RE = re.compile(r"[0-9a-fA-F]{40}\Z")
PREFIX_RE = re.compile(r"[A-Za-z0-9._/-]+\Z")


class ValidationError(RuntimeError):
    """Raised when the release event is not safe to publish."""


def env(name: str) -> str:
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


def write_outputs(values: dict[str, str]) -> None:
    output_path = Path(env("GITHUB_OUTPUT"))
    with output_path.open("a", encoding="utf-8") as output:
        for key, value in values.items():
            output.write(f"{key}={value}\n")


def main() -> int:
    try:
        repository = env("REPOSITORY")
        expected_repository = env("EXPECTED_REPOSITORY")
        if repository != expected_repository:
            raise ValidationError(
                f"workflow repository {repository!r} is not {expected_repository!r}"
            )

        event_name = env("EVENT_NAME")
        allowed_events = {
            value for value in os.environ.get("ALLOWED_EVENTS", "push").split(",") if value
        }
        if event_name not in allowed_events:
            raise ValidationError(
                f"event {event_name!r} is not allowed; expected one of {sorted(allowed_events)}"
            )

        ref = env("REF")
        if not ref.startswith("refs/tags/"):
            raise ValidationError(f"release ref must be a tag, got {ref!r}")
        if os.environ.get("REF_PROTECTED", "").lower() != "true":
            raise ValidationError("release tag is not covered by a protected tag rule")
        tag = ref.removeprefix("refs/tags/")
        prefix = env("TAG_PREFIX")
        if not PREFIX_RE.fullmatch(prefix):
            raise ValidationError("TAG_PREFIX contains unsupported characters")
        if not tag.startswith(prefix):
            raise ValidationError(f"tag {tag!r} does not use the required {prefix!r} prefix")

        # The tag suffix is used as a version in shell and package metadata.
        # Keep it deliberately narrow so no event value can become shell code.
        version = tag.removeprefix(prefix)
        if not re.fullmatch(
            r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?\Z", version
        ):
            raise ValidationError(f"tag {tag!r} does not contain a valid release version")
        if event_name == "workflow_call":
            expected_version = env("EXPECTED_VERSION")
            if expected_version != version:
                raise ValidationError(
                    f"workflow_call version {expected_version!r} does not match tag version {version!r}"
                )

        event_sha = env("SHA").lower()
        if not SHA_RE.fullmatch(event_sha):
            raise ValidationError("event SHA must be a full 40-character hexadecimal commit")

        # Resolve both refs after checkout.  The explicit refspecs prevent a
        # shallow checkout from silently validating an unrelated local ref.
        git(
            "fetch",
            "--no-tags",
            "origin",
            "refs/heads/main:refs/remotes/origin/main",
            f"refs/tags/{tag}:refs/tags/{tag}",
        )
        resolved_event_sha = git("rev-parse", f"{event_sha}^{{commit}}").lower()
        tag_sha = git("rev-parse", f"refs/tags/{tag}^{{commit}}").lower()
        main_sha = git("rev-parse", "refs/remotes/origin/main^{commit}").lower()

        if resolved_event_sha != event_sha:
            raise ValidationError("event SHA does not resolve to itself as a commit")
        if tag_sha != event_sha:
            raise ValidationError(
                f"tag {tag} resolves to {tag_sha}, but the event names {event_sha}"
            )
        try:
            subprocess.run(
                ["git", "merge-base", "--is-ancestor", tag_sha, main_sha],
                check=True,
                capture_output=True,
                text=True,
            )
        except (OSError, subprocess.CalledProcessError) as error:
            raise ValidationError(
                f"tag {tag} commit {tag_sha} is not an ancestor of main {main_sha}"
            ) from error

        write_outputs(
            {
                "tag": tag,
                "version": version,
                "commit": event_sha,
                "main_commit": main_sha,
            }
        )
        print(f"Validated {tag} at {event_sha}; main is {main_sha}")
        return 0
    except ValidationError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
