#!/usr/bin/env python3
"""Rerun the failed CLA check for one authenticated, exact issue comment.

This file is checked out from the workflow commit, never from the pull request.
It has a narrow contract: it can only rerun the exact failed GitHub Actions job
named "CLA Assistant v3" for the live head of the pull request that owns the comment.
The job, workflow, check, run, head, and bounded step records are cross-checked
immediately before the rerun request.
The final read and POST are not atomic because GitHub has no compare-and-swap
rerun endpoint. A force-push in that small window can therefore cause a stale
rerun, but it cannot grant or persist a CLA signature.
"""

from __future__ import annotations

import base64
import datetime as datetime_module
import json
import os
import re
import subprocess
import sys
import threading
from dataclasses import dataclass
from typing import Any
from urllib.parse import quote, urlsplit


SIGN_PHRASE = "I have read the CLA Document v2.2 and I hereby sign the CLA"
RECHECK = "recheck"
SIGNATURES_BRANCH = "cla-signatures"
SIGNATURES_PATH = "signatures/version2/cla.json"
WORKFLOW_PATH = ".github/workflows/cla.yml"
EXPECTED_REPOSITORY = "manaflow-ai/cmux-cua"
WORKFLOW_NAME = "CLA Assistant v3"
JOB_NAME = "CLA Assistant v3"
CLA_GENERATION = "v2.2-action-212a0f2dd659b24b48a30ba35966e06dc41736af"
GITHUB_ACTIONS_APP_ID = 15368
MAX_SAFE_INTEGER = 9_007_199_254_740_991
PAGE_SIZE = 100
MAX_CHECK_PAGES = 10
MAX_CHECK_RUNS = PAGE_SIZE * MAX_CHECK_PAGES
MAX_OPEN_PR_PAGES = 2
MAX_OPEN_PR_ITEMS = PAGE_SIZE * MAX_OPEN_PR_PAGES
MAX_COMMENT_BYTES = 65_536
MAX_METADATA_BYTES = 16_384
MAX_JOB_STEPS = 100
MAX_STEP_NUMBER = 1000
MAX_STEP_NAME_LENGTH = 512
MAX_LEDGER_BYTES = 1_000_000
MAX_LEDGER_RECORDS = 10_000
MAX_API_RESPONSE_BYTES = 2_000_000
MAX_API_ERROR_BYTES = 65_536
FAILURE_CONCLUSIONS = frozenset(
    {"action_required", "cancelled", "failure", "startup_failure", "stale", "timed_out"}
)
ASSOCIATIONS = frozenset(
    {
        "COLLABORATOR",
        "CONTRIBUTOR",
        "FIRST_TIME_CONTRIBUTOR",
        "FIRST_TIMER",
        "MEMBER",
        "NONE",
        "OWNER",
    }
)
RECHECK_ASSOCIATIONS = frozenset({"OWNER", "MEMBER", "COLLABORATOR"})
STATUSES = frozenset({"queued", "in_progress", "completed"})
CONCLUSIONS = FAILURE_CONCLUSIONS | frozenset({"neutral", "success", "skipped"})


class Rejected(RuntimeError):
    """A malformed or unauthorized event. No write is attempted."""


def reject(message: str) -> None:
    raise Rejected(message)


def require(condition: bool, message: str) -> None:
    if not condition:
        reject(message)


def string(value: Any, label: str, *, nonempty: bool = True) -> str:
    require(isinstance(value, str), f"{label} is not a string")
    require("\x00" not in value, f"{label} contains a NUL")
    if nonempty:
        require(bool(value), f"{label} is empty")
    return value


def integer(value: Any, label: str) -> int:
    require(isinstance(value, int) and not isinstance(value, bool), f"{label} is not an integer")
    require(value > 0 and value <= MAX_SAFE_INTEGER, f"{label} is out of range")
    return value


def count(value: Any, label: str) -> int:
    require(isinstance(value, int) and not isinstance(value, bool), f"{label} is not an integer")
    require(0 <= value <= MAX_SAFE_INTEGER, f"{label} is out of range")
    return value


def env_value(environment: dict[str, str], name: str, *, optional: bool = False) -> str:
    value = environment.get(name)
    if value is None:
        if optional:
            return ""
        reject(f"{name} is unavailable")
    return value


def safe_metadata(value: str, label: str) -> str:
    string(value, label, nonempty=False)
    require("\r" not in value and "\n" not in value, f"{label} contains a line break")
    try:
        metadata_bytes = len(value.encode("utf-8"))
    except UnicodeEncodeError:
        reject(f"{label} is not valid UTF-8")
    require(metadata_bytes <= MAX_METADATA_BYTES, f"{label} is too large")
    return value


def sha(value: Any, label: str) -> str:
    value = string(value, label)
    require(re.fullmatch(r"[0-9a-fA-F]{40}", value) is not None, f"{label} is not a commit SHA")
    return value.lower()


def timestamp(value: Any, label: str) -> datetime_module.datetime:
    value = string(value, label)
    require(
        re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z", value) is not None,
        f"{label} is not an RFC3339 UTC timestamp",
    )
    try:
        return datetime_module.datetime.fromisoformat(value[:-1] + "+00:00")
    except ValueError:
        reject(f"{label} is not a valid timestamp")


def optional_timestamp(value: Any, label: str) -> datetime_module.datetime | None:
    if value is None:
        return None
    return timestamp(value, label)


def obj(value: Any, label: str) -> dict[str, Any]:
    require(isinstance(value, dict), f"{label} is not an object")
    return value


def array(value: Any, label: str) -> list[Any]:
    require(isinstance(value, list), f"{label} is not an array")
    return value


def repository_matches(
    value: Any, expected_name: str, expected_id: int, label: str
) -> dict[str, Any]:
    """Validate the stable repository identity fields GitHub returns."""
    repository = obj(value, label)
    require(integer(repository.get("id"), f"{label}.id") == expected_id, f"{label}.id changed")
    short_name = expected_name.rsplit("/", 1)[-1]
    if "full_name" in repository:
        require(
            safe_metadata(
                string(repository["full_name"], f"{label}.full_name"), f"{label}.full_name"
            )
            == expected_name,
            f"{label}.full_name changed",
        )
    if "name" in repository:
        require(
            safe_metadata(string(repository["name"], f"{label}.name"), f"{label}.name")
            == short_name,
            f"{label}.name changed",
        )
    if "url" in repository:
        require(
            safe_metadata(string(repository["url"], f"{label}.url"), f"{label}.url")
            == f"https://api.github.com/repos/{expected_name}",
            f"{label}.url changed",
        )
    require(
        "full_name" in repository or "url" in repository,
        f"{label} has no canonical identity",
    )
    return repository


def unique_object_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        require(key not in result, "The CLA ledger contains a duplicate JSON key")
        result[key] = value
    return result


@dataclass(frozen=True)
class Context:
    event_name: str
    repo: str
    event_repo: str
    event_repo_id: int
    pr_number: int
    issue_number: int
    comment_id: int
    comment_body: str
    comment_author_id: int
    comment_author_login: str
    comment_author_type: str
    comment_author_association: str
    comment_created_at: str
    comment_updated_at: str
    comment_url: str
    comment_api_url: str
    target_base_ref: str
    workflow_sha: str
    workflow_path: str
    generation: str
    writer_result: str
    signature_recorded: str
    cla_passed: str
    event_pr: dict[str, str]

    @classmethod
    def from_environment(cls, environment: dict[str, str]) -> "Context":
        event_name = safe_metadata(env_value(environment, "EVENT_NAME"), "EVENT_NAME")
        require(event_name == "issue_comment", "The helper received an unexpected event")
        repo = safe_metadata(env_value(environment, "GH_REPO"), "GH_REPO")
        require(
            re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo) is not None, "GH_REPO is invalid"
        )
        require(
            repo == EXPECTED_REPOSITORY, "The helper is not running in the canonical repository"
        )
        event_repo = safe_metadata(
            env_value(environment, "EVENT_REPO_FULL_NAME"), "EVENT_REPO_FULL_NAME"
        )
        event_repo_id = int_decimal(env_value(environment, "EVENT_REPO_ID"), "EVENT_REPO_ID")
        require(event_repo == repo, "The event repository differs from GH_REPO")
        pr_number = int_decimal(env_value(environment, "PR_NUMBER"), "PR_NUMBER")
        issue_number = int_decimal(env_value(environment, "ISSUE_NUMBER"), "ISSUE_NUMBER")
        require(pr_number == issue_number, "The issue and pull request numbers differ")
        comment_id = int_decimal(env_value(environment, "COMMENT_ID"), "COMMENT_ID")
        body = string(env_value(environment, "COMMENT_BODY"), "COMMENT_BODY")
        try:
            body_bytes = len(body.encode("utf-8"))
        except UnicodeEncodeError:
            reject("The comment body is not valid UTF-8")
        require(body_bytes <= MAX_COMMENT_BYTES, "The comment body exceeds 64 KiB")
        require(body in (SIGN_PHRASE, RECHECK), "The comment is not an accepted CLA trigger")
        login = safe_metadata(
            env_value(environment, "COMMENT_AUTHOR_LOGIN"), "COMMENT_AUTHOR_LOGIN"
        )
        require(
            login and not login.casefold().endswith("[bot]"), "Bot comments cannot trigger a rerun"
        )
        author_type = safe_metadata(
            env_value(environment, "COMMENT_AUTHOR_TYPE"), "COMMENT_AUTHOR_TYPE"
        )
        require(author_type == "User", "The comment author is not a human user")
        association = safe_metadata(
            env_value(environment, "COMMENT_AUTHOR_ASSOCIATION"), "COMMENT_AUTHOR_ASSOCIATION"
        )
        require(association in ASSOCIATIONS, "The comment author association is invalid")
        created = safe_metadata(env_value(environment, "COMMENT_CREATED_AT"), "COMMENT_CREATED_AT")
        updated = safe_metadata(env_value(environment, "COMMENT_UPDATED_AT"), "COMMENT_UPDATED_AT")
        timestamp(created, "COMMENT_CREATED_AT")
        timestamp(updated, "COMMENT_UPDATED_AT")
        require(created == updated, "The comment was edited")
        comment_url = safe_metadata(env_value(environment, "COMMENT_URL"), "COMMENT_URL")
        comment_api_url = safe_metadata(
            env_value(environment, "COMMENT_API_URL"), "COMMENT_API_URL"
        )
        require(
            comment_url == f"https://github.com/{repo}/pull/{pr_number}#issuecomment-{comment_id}",
            "COMMENT_URL is not the canonical pull-request comment URL",
        )
        require(
            comment_api_url == f"https://api.github.com/repos/{repo}/issues/comments/{comment_id}",
            "COMMENT_API_URL is not the canonical comment API URL",
        )
        target_base_ref = safe_metadata(
            env_value(environment, "TARGET_BASE_REF"), "TARGET_BASE_REF"
        )
        require(target_base_ref == "main", "The target base branch is not main")
        workflow_sha = sha(env_value(environment, "WORKFLOW_SHA"), "WORKFLOW_SHA")
        workflow_path = safe_metadata(
            env_value(environment, "WORKFLOW_PATH", optional=True) or WORKFLOW_PATH, "WORKFLOW_PATH"
        )
        require(
            workflow_path == WORKFLOW_PATH, "The workflow path is not the maintained CLA workflow"
        )
        generation = safe_metadata(env_value(environment, "CLA_GENERATION"), "CLA_GENERATION")
        require(generation == CLA_GENERATION, "CLA_GENERATION is not the reviewed action release")
        writer_result = safe_metadata(env_value(environment, "WRITER_RESULT"), "WRITER_RESULT")
        require(writer_result == "success", "The issue-comment writer did not succeed")
        signature_recorded = safe_metadata(
            env_value(environment, "SIGNATURE_RECORDED", optional=True), "SIGNATURE_RECORDED"
        )
        require(signature_recorded in ("", "true", "false"), "SIGNATURE_RECORDED is invalid")
        cla_passed = safe_metadata(
            env_value(environment, "CLA_PASSED", optional=True), "CLA_PASSED"
        )
        require(cla_passed in ("", "true", "false"), "CLA_PASSED is invalid")
        if body == SIGN_PHRASE:
            require(signature_recorded == "true", "The signing action did not persist a signature")
            require(
                cla_passed == "true", "The signing action did not report a final all-signed result"
            )
        else:
            # A recheck can discover and persist another contributor's exact
            # signing comment while it recomputes the ledger. It is therefore
            # valid with either output value, provided the action completed and
            # reported the final all-signed result below.
            require(
                signature_recorded in ("true", "false"),
                "A recheck comment did not report a writer result",
            )
            require(
                cla_passed == "true",
                "The recheck writer did not report a final all-signed result",
            )
        event_pr = {}
        for name in (
            "EVENT_PR_NUMBER",
            "EVENT_PR_AUTHOR_ID",
            "EVENT_PR_AUTHOR_LOGIN",
            "EVENT_PR_AUTHOR_TYPE",
            "EVENT_HEAD_SHA",
            "EVENT_HEAD_REF",
            "EVENT_HEAD_REPO_FULL_NAME",
            "EVENT_HEAD_REPO_ID",
            "EVENT_BASE_SHA",
            "EVENT_BASE_REF",
            "EVENT_BASE_REPO_FULL_NAME",
            "EVENT_BASE_REPO_ID",
        ):
            event_pr[name] = safe_metadata(env_value(environment, name, optional=True), name)
        present = [value for value in event_pr.values() if value]
        if present:
            require(all(event_pr.values()), "The pull request event binding is incomplete")
            int_decimal(event_pr["EVENT_PR_NUMBER"], "EVENT_PR_NUMBER")
            int_decimal(event_pr["EVENT_HEAD_REPO_ID"], "EVENT_HEAD_REPO_ID")
            int_decimal(event_pr["EVENT_BASE_REPO_ID"], "EVENT_BASE_REPO_ID")
            sha(event_pr["EVENT_HEAD_SHA"], "EVENT_HEAD_SHA")
            sha(event_pr["EVENT_BASE_SHA"], "EVENT_BASE_SHA")
        return cls(
            event_name=event_name,
            repo=repo,
            event_repo=event_repo,
            event_repo_id=event_repo_id,
            pr_number=pr_number,
            issue_number=issue_number,
            comment_id=comment_id,
            comment_body=body,
            comment_author_id=int_decimal(
                env_value(environment, "COMMENT_AUTHOR_ID"), "COMMENT_AUTHOR_ID"
            ),
            comment_author_login=login,
            comment_author_type=author_type,
            comment_author_association=association,
            comment_created_at=created,
            comment_updated_at=updated,
            comment_url=comment_url,
            comment_api_url=comment_api_url,
            target_base_ref=target_base_ref,
            workflow_sha=workflow_sha,
            workflow_path=workflow_path,
            generation=generation,
            writer_result=writer_result,
            signature_recorded=signature_recorded,
            cla_passed=cla_passed,
            event_pr=event_pr,
        )


def int_decimal(value: str, label: str) -> int:
    require(isinstance(value, str), f"{label} is invalid")
    require(re.fullmatch(r"[1-9][0-9]*", value) is not None, f"{label} is invalid")
    require(len(value) <= 16, f"{label} is out of range")
    try:
        parsed = int(value)
    except ValueError:
        reject(f"{label} is out of range")
    require(parsed <= MAX_SAFE_INTEGER, f"{label} is out of range")
    return parsed


def _kill_process(process: subprocess.Popen[bytes]) -> None:
    try:
        process.kill()
    except OSError:
        pass


def _drain_bounded(
    stream: Any,
    limit: int,
    overflow: threading.Event,
    kill_process: Any,
    reader_error: list[Exception],
) -> bytes:
    data = bytearray()
    while True:
        try:
            chunk = stream.read(65_536)
        except (OSError, ValueError) as error:  # pragma: no cover - OS pipe failure
            reader_error.append(error)
            return bytes(data)
        if not chunk:
            return bytes(data)
        if len(data) <= limit:
            remaining = limit + 1 - len(data)
            data.extend(chunk[:remaining])
            if len(data) > limit:
                overflow.set()
                kill_process()


def _start_bounded_readers(
    stdout: Any,
    stderr: Any,
    overflow: threading.Event,
    kill_process: Any,
    reader_error: list[Exception],
) -> tuple[list[bytes], tuple[threading.Thread, threading.Thread]]:
    """Drain both child pipes concurrently so either stream cannot deadlock."""
    output_holder: list[bytes] = [b"", b""]
    stdout_thread = threading.Thread(
        target=lambda: output_holder.__setitem__(
            0,
            _drain_bounded(stdout, MAX_API_RESPONSE_BYTES, overflow, kill_process, reader_error),
        ),
        daemon=True,
    )
    stderr_thread = threading.Thread(
        target=lambda: output_holder.__setitem__(
            1,
            _drain_bounded(stderr, MAX_API_ERROR_BYTES, overflow, kill_process, reader_error),
        ),
        daemon=True,
    )
    stdout_thread.start()
    stderr_thread.start()
    return output_holder, (stdout_thread, stderr_thread)


def _wait_bounded_process(process: subprocess.Popen[bytes], kill_process: Any) -> tuple[int, bool]:
    timed_out = False
    try:
        returncode = process.wait(timeout=45)
    except subprocess.TimeoutExpired:
        timed_out = True
        kill_process()
        try:
            returncode = process.wait(timeout=5)
        except subprocess.TimeoutExpired:  # pragma: no cover - unkillable child
            returncode = -9
    return returncode, timed_out


def _finish_bounded_readers(
    process: subprocess.Popen[bytes],
    readers: tuple[threading.Thread, threading.Thread],
    kill_process: Any,
    reader_error: list[Exception],
) -> None:
    for reader in readers:
        reader.join(timeout=5)
    if not any(reader.is_alive() for reader in readers):
        return
    # Closing the pipes wakes a reader that survived process death. A still
    # live reader is a failed bounded-read invariant, not a partial response.
    kill_process()
    for stream in (process.stdout, process.stderr):
        try:
            stream.close()  # type: ignore[union-attr]
        except OSError:
            pass
    for reader in readers:
        reader.join(timeout=1)
    if any(reader.is_alive() for reader in readers):
        reader_error.append(RuntimeError("API reader did not terminate"))


class GhApi:
    """Minimal argv-only wrapper around gh, with no shell interpolation."""

    def __init__(self, environment: dict[str, str]):
        token = environment.get("GH_TOKEN", "")
        require(token, "GH_TOKEN is unavailable")
        # Keep the helper's child process environment small. In particular,
        # do not forward unrelated runner secrets to `gh`.
        self.environment = {
            key: os.environ[key]
            for key in ("HOME", "PATH", "XDG_CONFIG_HOME", "LANG", "LC_ALL")
            if key in os.environ
        }
        self.environment.update({"GH_TOKEN": token, "GH_HOST": "github.com"})

    def request(
        self,
        method: str,
        endpoint: str,
        query: dict[str, str] | None = None,
        *,
        expect_json: bool = True,
    ) -> Any:
        require(method in ("GET", "POST"), "Unsupported GitHub method")
        require(
            re.fullmatch(
                r"repos/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/[A-Za-z0-9_.:/?&=%+-]+",
                endpoint,
            )
            is not None,
            "Invalid API endpoint",
        )
        command = [
            "gh",
            "api",
            "--method",
            method,
            "--header",
            "Accept: application/vnd.github+json",
        ]
        for key, value in (query or {}).items():
            require(re.fullmatch(r"[a-z_]+", key) is not None, "Invalid API query key")
            require(
                isinstance(value, str)
                and len(value) <= 512
                and "\x00" not in value
                and "\r" not in value
                and "\n" not in value,
                "Invalid API query value",
            )
            command.extend(["--raw-field", f"{key}={value}"])
        command.append(endpoint)
        result = self._run_bounded(command)
        require(not result.timed_out, "GitHub API request timed out")
        require(not result.overflow, "GitHub API response is too large")
        require(result.reader_error is None, "GitHub API response could not be read")
        require(result.returncode == 0, f"GitHub API request failed for {method} {endpoint}")
        try:
            output = result.stdout.decode("utf-8")
        except UnicodeDecodeError:
            reject(f"GitHub API returned invalid UTF-8 for {endpoint}")
        if not expect_json:
            return None
        try:
            return json.loads(output)
        except (RecursionError, ValueError):
            reject(f"GitHub API returned invalid JSON for {endpoint}")

    @dataclass(frozen=True)
    class _Result:
        stdout: bytes
        stderr: bytes
        returncode: int
        overflow: bool
        timed_out: bool
        reader_error: Exception | None

    def _run_bounded(self, command: list[str]) -> _Result:
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=self.environment,
        )
        assert process.stdout is not None and process.stderr is not None
        overflow = threading.Event()
        reader_error: list[Exception] = []

        def kill_process() -> None:
            _kill_process(process)

        output_holder, readers = _start_bounded_readers(
            process.stdout,
            process.stderr,
            overflow,
            kill_process,
            reader_error,
        )
        returncode, timed_out = _wait_bounded_process(process, kill_process)
        _finish_bounded_readers(
            process,
            readers,
            kill_process,
            reader_error,
        )
        return self._Result(
            stdout=output_holder[0],
            stderr=output_holder[1],
            returncode=returncode,
            overflow=overflow.is_set(),
            timed_out=timed_out,
            reader_error=reader_error[0] if reader_error else None,
        )

    def get(self, endpoint: str, query: dict[str, str] | None = None) -> Any:
        return self.request("GET", endpoint, query)

    def post(self, endpoint: str) -> Any:
        return self.request("POST", endpoint, expect_json=False)


@dataclass(frozen=True)
class Snapshot:
    number: int
    base_ref: str
    base_sha: str
    base_repo: str
    base_repo_id: int
    head_ref: str
    head_sha: str
    head_repo: str
    head_repo_id: int
    author_login: str
    author_id: int
    author_type: str

    def identity(self) -> tuple[Any, ...]:
        return (
            self.number,
            self.base_ref,
            self.base_sha,
            self.base_repo,
            self.base_repo_id,
            self.head_ref,
            self.head_sha,
            self.head_repo,
            self.head_repo_id,
            self.author_login,
            self.author_id,
            self.author_type,
        )


def issue_endpoint(context: Context) -> str:
    return f"repos/{context.repo}/issues/{context.pr_number}"


def pull_endpoint(context: Context) -> str:
    return f"repos/{context.repo}/pulls/{context.pr_number}"


def comment_endpoint(context: Context) -> str:
    return f"repos/{context.repo}/issues/comments/{context.comment_id}"


def validate_issue(api: Any, context: Context) -> None:
    issue = obj(api.get(issue_endpoint(context)), "issue")
    require(
        integer(issue.get("number"), "issue.number") == context.pr_number,
        "The issue number changed",
    )
    require(issue.get("state") == "open", "The issue is not open")
    pull = obj(issue.get("pull_request"), "issue.pull_request")
    require(
        pull.get("url") == f"https://api.github.com/repos/{context.repo}/pulls/{context.pr_number}",
        "The issue is not the exact pull request",
    )


def validate_comment(api: Any, context: Context) -> None:
    comment = obj(api.get(comment_endpoint(context)), "comment")
    require(
        integer(comment.get("id"), "comment.id") == context.comment_id, "The comment ID changed"
    )
    require(comment.get("body") == context.comment_body, "The comment body changed")
    require(
        comment.get("created_at") == context.comment_created_at, "The comment creation time changed"
    )
    require(comment.get("updated_at") == context.comment_updated_at, "The comment was edited")
    require(comment.get("url") == context.comment_api_url, "The comment API URL changed")
    require(comment.get("html_url") == context.comment_url, "The comment URL changed")
    require(
        comment.get("issue_url")
        == f"https://api.github.com/repos/{context.repo}/issues/{context.pr_number}",
        "The comment moved to another issue",
    )
    user = obj(comment.get("user"), "comment.user")
    require(
        integer(user.get("id"), "comment.user.id") == context.comment_author_id,
        "The commenter ID changed",
    )
    require(user.get("login") == context.comment_author_login, "The commenter login changed")
    require(user.get("type") == context.comment_author_type, "The commenter type changed")
    require(
        comment.get("author_association") == context.comment_author_association,
        "The commenter association changed",
    )


def validate_event_binding(snapshot: Snapshot, context: Context) -> None:
    event = context.event_pr
    if not event["EVENT_PR_NUMBER"]:
        return
    require(
        int_decimal(event["EVENT_PR_NUMBER"], "EVENT_PR_NUMBER") == snapshot.number,
        "The event PR changed",
    )
    require(
        sha(event["EVENT_HEAD_SHA"], "EVENT_HEAD_SHA") == snapshot.head_sha,
        "The event head changed",
    )
    require(event["EVENT_HEAD_REF"] == snapshot.head_ref, "The event head ref changed")
    require(
        event["EVENT_HEAD_REPO_FULL_NAME"] == snapshot.head_repo,
        "The event head repository changed",
    )
    require(
        int_decimal(event["EVENT_HEAD_REPO_ID"], "EVENT_HEAD_REPO_ID") == snapshot.head_repo_id,
        "The event head repository ID changed",
    )
    require(
        sha(event["EVENT_BASE_SHA"], "EVENT_BASE_SHA") == snapshot.base_sha,
        "The event base changed",
    )
    require(event["EVENT_BASE_REF"] == snapshot.base_ref, "The event base ref changed")
    require(
        event["EVENT_BASE_REPO_FULL_NAME"] == snapshot.base_repo,
        "The event base repository changed",
    )
    require(
        int_decimal(event["EVENT_BASE_REPO_ID"], "EVENT_BASE_REPO_ID") == snapshot.base_repo_id,
        "The event base repository ID changed",
    )
    require(
        int_decimal(event["EVENT_PR_AUTHOR_ID"], "EVENT_PR_AUTHOR_ID") == snapshot.author_id,
        "The event opener ID changed",
    )
    require(
        event["EVENT_PR_AUTHOR_LOGIN"] == snapshot.author_login, "The event opener login changed"
    )
    require(event["EVENT_PR_AUTHOR_TYPE"] == snapshot.author_type, "The event opener type changed")


def live_snapshot(api: Any, context: Context) -> Snapshot:
    pull = obj(api.get(pull_endpoint(context)), "pull request")
    require(
        integer(pull.get("number"), "pull.number") == context.pr_number,
        "The pull request number changed",
    )
    require(
        pull.get("state") == "open" and pull.get("merged_at") is None,
        "The pull request is not open",
    )
    base = obj(pull.get("base"), "pull.base")
    head = obj(pull.get("head"), "pull.head")
    base_repo = repository_matches(
        base.get("repo"), context.repo, context.event_repo_id, "pull.base.repo"
    )
    head_repo = obj(head.get("repo"), "pull.head.repo")
    head_repo_id = integer(head_repo.get("id"), "pull.head.repo.id")
    head_repo_name = safe_metadata(
        string(head_repo.get("full_name"), "pull.head.repo.full_name"),
        "pull.head.repo.full_name",
    )
    require(
        re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", head_repo_name) is not None,
        "pull.head.repo.full_name is invalid",
    )
    require(
        head_repo.get("url") in (None, f"https://api.github.com/repos/{head_repo_name}"),
        "pull.head.repo.url changed",
    )
    author = obj(pull.get("user"), "pull.user")
    snapshot = Snapshot(
        number=context.pr_number,
        base_ref=safe_metadata(string(base.get("ref"), "pull.base.ref"), "pull.base.ref"),
        base_sha=sha(base.get("sha"), "pull.base.sha"),
        base_repo=safe_metadata(
            string(base_repo.get("full_name") or context.repo, "pull.base.repo.full_name"),
            "pull.base.repo.full_name",
        ),
        base_repo_id=integer(base_repo.get("id"), "pull.base.repo.id"),
        head_ref=safe_metadata(string(head.get("ref"), "pull.head.ref"), "pull.head.ref"),
        head_sha=sha(head.get("sha"), "pull.head.sha"),
        head_repo=head_repo_name,
        head_repo_id=head_repo_id,
        author_login=safe_metadata(
            string(author.get("login"), "pull.user.login"), "pull.user.login"
        ),
        author_id=integer(author.get("id"), "pull.user.id"),
        author_type=safe_metadata(string(author.get("type"), "pull.user.type"), "pull.user.type"),
    )
    require(
        snapshot.base_ref == context.target_base_ref, "The pull request targets a different branch"
    )
    require(snapshot.base_repo == context.repo, "The pull request targets a different repository")
    require(snapshot.base_repo_id == context.event_repo_id, "The base repository ID changed")
    require(snapshot.author_type == "User", "The pull request opener is not a human user")
    validate_event_binding(snapshot, context)
    return snapshot


def validate_workflow(api: Any, context: Context) -> int:
    encoded = quote(context.workflow_path, safe="")
    workflow = obj(
        api.get(f"repos/{context.repo}/actions/workflows/{encoded}"),
        "CLA workflow",
    )
    workflow_id = integer(workflow.get("id"), "CLA workflow.id")
    require(workflow.get("name") == WORKFLOW_NAME, "The active workflow name changed")
    require(workflow.get("path") == context.workflow_path, "The active workflow path changed")
    require(workflow.get("state") == "active", "The CLA workflow is not active")
    return workflow_id


def check_run_record(raw: Any, label: str) -> dict[str, Any]:
    record = obj(raw, label)
    record_id = integer(record.get("id"), f"{label}.id")
    name = safe_metadata(string(record.get("name"), f"{label}.name"), f"{label}.name")
    status = string(record.get("status"), f"{label}.status")
    require(status in STATUSES, f"{label}.status is invalid")
    conclusion = record.get("conclusion")
    if conclusion is not None:
        conclusion = safe_metadata(string(conclusion, f"{label}.conclusion"), f"{label}.conclusion")
        require(conclusion in CONCLUSIONS, f"{label}.conclusion is invalid")
    app = obj(record.get("app"), f"{label}.app")
    app_id = integer(app.get("id"), f"{label}.app.id")
    app_name = safe_metadata(string(app.get("name"), f"{label}.app.name"), f"{label}.app.name")
    app_slug_value = app.get("slug")
    app_slug = None
    if app_slug_value is not None:
        app_slug = safe_metadata(string(app_slug_value, f"{label}.app.slug"), f"{label}.app.slug")
    created_at = optional_timestamp(record.get("created_at"), f"{label}.created_at")
    updated_at = optional_timestamp(record.get("updated_at"), f"{label}.updated_at")
    started_at = optional_timestamp(record.get("started_at"), f"{label}.started_at")
    completed_at = optional_timestamp(record.get("completed_at"), f"{label}.completed_at")
    require(
        any(value is not None for value in (created_at, updated_at, started_at, completed_at)),
        f"{label} has no timestamp",
    )
    if created_at is not None and updated_at is not None:
        require(updated_at >= created_at, f"{label}.updated_at precedes created_at")
    if created_at is not None and started_at is not None:
        require(started_at >= created_at, f"{label}.started_at precedes created_at")
    if created_at is not None and completed_at is not None:
        require(completed_at >= created_at, f"{label}.completed_at precedes created_at")
    if started_at is not None and completed_at is not None:
        require(completed_at >= started_at, f"{label}.completed_at precedes started_at")
    if status == "completed":
        require(
            conclusion is not None and completed_at is not None,
            f"{label} is completed without a conclusion",
        )
    else:
        require(conclusion is None, f"{label} is not completed but has a conclusion")
    details_url = record.get("details_url")
    if details_url is not None:
        details_url = safe_metadata(
            string(details_url, f"{label}.details_url"), f"{label}.details_url"
        )
    require(details_url is not None, f"{label}.details_url is missing")
    head_sha = sha(record.get("head_sha"), f"{label}.head_sha")
    return {
        "id": record_id,
        "name": name,
        "head_sha": head_sha,
        "status": status,
        "conclusion": conclusion,
        "app_id": app_id,
        "app_name": app_name,
        "app_slug": app_slug,
        "created_at": created_at,
        "updated_at": updated_at,
        "started_at": started_at,
        "completed_at": completed_at,
        "details_url": details_url,
    }


def list_check_runs(api: Any, context: Context, head_sha: str) -> list[dict[str, Any]]:
    endpoint = f"repos/{context.repo}/commits/{head_sha}/check-runs"
    runs: list[dict[str, Any]] = []
    seen: set[int] = set()
    expected_total: int | None = None
    for page in range(1, MAX_CHECK_PAGES + 1):
        payload = obj(
            api.get(endpoint, {"filter": "all", "page": str(page), "per_page": str(PAGE_SIZE)}),
            f"check-runs page {page}",
        )
        total = count(payload.get("total_count"), f"check-runs page {page}.total_count")
        require(total <= MAX_CHECK_RUNS, "The check-run list exceeds the safety bound")
        if expected_total is None:
            expected_total = total
        else:
            require(total == expected_total, "The check-run total changed during pagination")
        page_runs = array(payload.get("check_runs"), f"check-runs page {page}.check_runs")
        require(len(page_runs) <= PAGE_SIZE, f"check-runs page {page} is oversized")
        for index, raw in enumerate(page_runs):
            parsed = check_run_record(raw, f"check-runs page {page}[{index}]")
            require(parsed["id"] not in seen, "The check-run list contains a duplicate ID")
            seen.add(parsed["id"])
            runs.append(parsed)
        require(len(runs) <= MAX_CHECK_RUNS, "The check-run list overflowed its bound")
        if expected_total == len(runs):
            break
        require(len(page_runs) == PAGE_SIZE, "The check-run page ended before total_count")
    else:
        reject("The check-run list exceeded its page bound")
    require(expected_total == len(runs), "The check-run pagination is incomplete")
    return runs


def details_ids(url: str, context: Context) -> tuple[int, int]:
    try:
        parsed = urlsplit(url)
    except (ValueError, UnicodeError):
        reject("The check URL is malformed")
    try:
        port = parsed.port
    except ValueError:
        reject("The check URL has an invalid port")
    require(
        parsed.scheme == "https"
        and parsed.netloc == "github.com"
        and not parsed.fragment
        and not parsed.query
        and parsed.username is None
        and parsed.password is None
        and port is None,
        "The check URL is not a canonical GitHub URL",
    )
    expected_prefix = f"/{context.repo}/actions/runs/"
    require(parsed.path.startswith(expected_prefix), "The check URL targets another repository")
    match = re.fullmatch(
        r"/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/actions/runs/([1-9][0-9]*)/job/([1-9][0-9]*)",
        parsed.path,
    )
    require(match is not None, "The check URL is not an Actions job URL")
    return int_decimal(match.group(1), "workflow run ID"), int_decimal(
        match.group(2), "workflow job ID"
    )


def matching_check(
    run: dict[str, Any], context: Context, comment_time: datetime_module.datetime
) -> bool:
    if run["name"] != JOB_NAME:
        return False
    if run["app_name"] != "GitHub Actions":
        return False
    if run["app_slug"] != "github-actions":
        return False
    require(run["app_id"] == GITHUB_ACTIONS_APP_ID, "The CLA check uses an unexpected GitHub App")
    # The current issue-comment workflow can create its own check after the
    # comment. It is not evidence for the historical failed check we may
    # rerun, so ignore post-comment records instead of treating them as the
    # selected context.
    require(
        any(
            value is not None
            for value in (
                run["created_at"],
                run["updated_at"],
                run["started_at"],
                run["completed_at"],
            )
        ),
        "The CLA check has no usable timestamp",
    )
    return all(
        value is None or value <= comment_time
        for value in (
            run["created_at"],
            run["updated_at"],
            run["started_at"],
            run["completed_at"],
        )
    )


def latest_matching_check(api: Any, context: Context, snapshot: Snapshot) -> dict[str, Any] | None:
    comment_time = timestamp(context.comment_created_at, "COMMENT_CREATED_AT")
    candidates = [
        run
        for run in list_check_runs(api, context, snapshot.head_sha)
        if run["head_sha"] == snapshot.head_sha and matching_check(run, context, comment_time)
    ]
    if not candidates:
        return None
    candidates.sort(
        key=lambda run: (
            run["completed_at"] or run["updated_at"] or run["started_at"] or run["created_at"],
            run["id"],
        ),
        reverse=True,
    )
    selected = candidates[0]
    # A duplicate check binding is ambiguous. Historical failures for the
    # same source head are normal, but two records naming one exact Actions
    # job cannot be safely distinguished by the rerun endpoint.
    require(
        sum(run["details_url"] == selected["details_url"] for run in candidates) == 1,
        "The failed CLA check binding is ambiguous",
    )
    return selected


def validate_check_detail(api: Any, context: Context, candidate: dict[str, Any]) -> None:
    detail = check_run_record(
        api.get(f"repos/{context.repo}/check-runs/{candidate['id']}"),
        "selected check-run",
    )
    for key in (
        "id",
        "name",
        "head_sha",
        "status",
        "conclusion",
        "app_id",
        "app_name",
        "app_slug",
        "details_url",
        "created_at",
        "updated_at",
        "started_at",
        "completed_at",
    ):
        require(detail[key] == candidate[key], f"The selected check-run {key} changed")
    details_ids(detail["details_url"], context)


def validate_live_open_head_association(api: Any, context: Context, snapshot: Snapshot) -> None:
    """Require one exact open PR for a source head when run metadata is sparse."""
    require(
        re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", snapshot.head_repo) is not None,
        "The pull request head repository name is invalid",
    )
    owner, _ = snapshot.head_repo.split("/", 1)
    endpoint = f"repos/{context.repo}/pulls"
    query = {
        "state": "open",
        "base": snapshot.base_ref,
        "head": f"{owner}:{snapshot.head_ref}",
        "per_page": str(PAGE_SIZE),
        "page": "1",
    }
    first = array(api.get(endpoint, query), "live open pull requests page 1")
    require(len(first) <= PAGE_SIZE, "The live open pull request page is oversized")
    pages = [first]
    if len(first) == PAGE_SIZE:
        second_query = dict(query, page="2")
        second = array(api.get(endpoint, second_query), "live open pull requests page 2")
        require(len(second) <= PAGE_SIZE, "The live open pull request page is oversized")
        require(
            len(second) < PAGE_SIZE,
            "The live open pull request result window is full",
        )
        pages.append(second)
    items = [item for page in pages for item in page]
    require(len(items) <= MAX_OPEN_PR_ITEMS, "The live open pull request list is oversized")

    matches = 0
    for index, raw in enumerate(items):
        pull = obj(raw, f"live open pull request[{index}]")
        require(
            integer(pull.get("number"), f"live open pull request[{index}].number") > 0,
            "The live open pull request number is invalid",
        )
        require(pull.get("state") == "open", "The live open pull request state is invalid")
        require(pull.get("merged_at") is None, "The live open pull request is merged")
        base = obj(pull.get("base"), f"live open pull request[{index}].base")
        head = obj(pull.get("head"), f"live open pull request[{index}].head")
        base_repo = repository_matches(
            base.get("repo"),
            snapshot.base_repo,
            snapshot.base_repo_id,
            f"live open pull request[{index}].base.repo",
        )
        head_repo = repository_matches(
            head.get("repo"),
            snapshot.head_repo,
            snapshot.head_repo_id,
            f"live open pull request[{index}].head.repo",
        )
        base_ref = safe_metadata(
            string(base.get("ref"), f"live open pull request[{index}].base.ref"),
            f"live open pull request[{index}].base.ref",
        )
        head_ref = safe_metadata(
            string(head.get("ref"), f"live open pull request[{index}].head.ref"),
            f"live open pull request[{index}].head.ref",
        )
        base_sha = sha(base.get("sha"), f"live open pull request[{index}].base.sha")
        head_sha = sha(head.get("sha"), f"live open pull request[{index}].head.sha")
        if (
            integer(pull.get("number"), f"live open pull request[{index}].number")
            == snapshot.number
            and base_ref == snapshot.base_ref
            and base_sha == snapshot.base_sha
            and head_ref == snapshot.head_ref
            and head_sha == snapshot.head_sha
            and base_repo.get("id") == snapshot.base_repo_id
            and head_repo.get("id") == snapshot.head_repo_id
        ):
            matches += 1
    require(
        matches == 1, "The live source head is not associated with exactly one open pull request"
    )


def validate_workflow_run(
    api: Any,
    context: Context,
    snapshot: Snapshot,
    workflow_id: int,
    candidate: dict[str, Any],
) -> tuple[int, int, str]:
    run_id, job_id = details_ids(candidate["details_url"], context)
    # The check itself was discovered on the live source SHA. Keep that
    # assertion explicit because pull_request_target run metadata can report
    # the base execution SHA on some GitHub API responses.
    require(
        candidate["head_sha"] == snapshot.head_sha,
        "The CLA check is not attached to the live source head",
    )
    run = obj(api.get(f"repos/{context.repo}/actions/runs/{run_id}"), "workflow run")
    require(integer(run.get("id"), "workflow run.id") == run_id, "The workflow run ID changed")
    require(
        integer(run.get("workflow_id"), "workflow run.workflow_id") == workflow_id,
        "The check belongs to another workflow",
    )
    require(run.get("name") == WORKFLOW_NAME, "The check belongs to another workflow name")
    require(run.get("path") == context.workflow_path, "The check belongs to another workflow path")
    require(
        run.get("html_url") == f"https://github.com/{context.repo}/actions/runs/{run_id}",
        "The workflow run HTML URL changed",
    )
    require(
        run.get("event") == "pull_request_target",
        "The check was not produced by pull_request_target",
    )
    require(run.get("status") == "completed", "The workflow run is not complete")
    run_conclusion = safe_metadata(
        string(run.get("conclusion"), "workflow run.conclusion"),
        "workflow run.conclusion",
    )
    require(run_conclusion in FAILURE_CONCLUSIONS, "The workflow run is not failed")
    execution_sha = sha(run.get("head_sha"), "workflow run.head_sha")
    require(run.get("head_branch") == snapshot.head_ref, "The workflow run head ref changed")
    run_head_repository = run.get("head_repository")
    if run_head_repository is not None:
        if execution_sha == snapshot.head_sha:
            repository_matches(
                run_head_repository,
                snapshot.head_repo,
                snapshot.head_repo_id,
                "workflow run.head_repository",
            )
        elif execution_sha == snapshot.base_sha:
            repository_matches(
                run_head_repository,
                snapshot.base_repo,
                snapshot.base_repo_id,
                "workflow run.head_repository",
            )
        else:
            reject("The workflow run execution SHA is not the source or base revision")
    else:
        require(
            execution_sha in (snapshot.head_sha, snapshot.base_sha),
            "The workflow run has no trusted source or base execution SHA",
        )
    repository_matches(
        run.get("repository"),
        snapshot.base_repo,
        snapshot.base_repo_id,
        "workflow run.repository",
    )
    created_at = timestamp(run.get("created_at"), "workflow run.created_at")
    updated_at = timestamp(run.get("updated_at"), "workflow run.updated_at")
    require(
        created_at <= timestamp(context.comment_created_at, "COMMENT_CREATED_AT"),
        "The workflow run started after the comment",
    )
    require(updated_at >= created_at, "workflow run.updated_at precedes created_at")
    require(
        updated_at <= timestamp(context.comment_created_at, "COMMENT_CREATED_AT"),
        "The workflow run changed after the comment",
    )
    raw_associations = run.get("pull_requests", [])
    associations = (
        [] if raw_associations is None else array(raw_associations, "workflow run.pull_requests")
    )
    require(len(associations) <= 100, "The workflow run has too many pull request associations")
    if associations:
        exact_matches = 0
        for index, association in enumerate(associations):
            item = obj(association, f"workflow run.pull_requests[{index}]")
            if integer(item.get("number"), "workflow association.number") != snapshot.number:
                continue
            base = obj(item.get("base"), "workflow association.base")
            head = obj(item.get("head"), "workflow association.head")
            base_repo = repository_matches(
                base.get("repo"),
                snapshot.base_repo,
                snapshot.base_repo_id,
                "workflow association.base.repo",
            )
            head_repo = repository_matches(
                head.get("repo"),
                snapshot.head_repo,
                snapshot.head_repo_id,
                "workflow association.head.repo",
            )
            exact = (
                base.get("ref") == snapshot.base_ref
                and sha(base.get("sha"), "workflow association.base.sha") == snapshot.base_sha
                and head.get("ref") == snapshot.head_ref
                and sha(head.get("sha"), "workflow association.head.sha") == snapshot.head_sha
            )
            if exact:
                exact_matches += 1
        require(
            exact_matches == 1,
            "The workflow run is not associated with exactly one pull request",
        )
    else:
        # GitHub often omits pull_requests and head_repository for
        # pull_request_target runs. In that case, bind the run to the live
        # source PR before accepting the check as rerunnable. The lookup is
        # repeated during the final TOCTOU revalidation.
        validate_live_open_head_association(api, context, snapshot)
    return run_id, job_id, execution_sha


def validate_workflow_job(
    api: Any,
    context: Context,
    snapshot: Snapshot,
    candidate: dict[str, Any],
    run_id: int,
    job_id: int,
    execution_sha: str,
) -> None:
    """Bind the rerun to the exact failed job, not only its check and run."""
    job = obj(api.get(f"repos/{context.repo}/actions/jobs/{job_id}"), "CLA workflow job")
    require(integer(job.get("id"), "CLA workflow job.id") == job_id, "The workflow job ID changed")
    require(
        integer(job.get("run_id"), "CLA workflow job.run_id") == run_id,
        "The workflow job run changed",
    )
    require(job.get("name") == JOB_NAME, "The selected job name changed")
    require(job.get("workflow_name") == WORKFLOW_NAME, "The selected job workflow name changed")
    job_head_sha = sha(job.get("head_sha"), "CLA workflow job.head_sha")
    require(
        job_head_sha == execution_sha,
        "The workflow job head is not bound to the selected run",
    )
    require(job.get("head_branch") == snapshot.head_ref, "The workflow job head ref changed")
    require(job.get("status") == "completed", "The workflow job is not complete")
    conclusion = safe_metadata(
        string(job.get("conclusion"), "CLA workflow job.conclusion"),
        "CLA workflow job.conclusion",
    )
    require(conclusion in FAILURE_CONCLUSIONS, "The workflow job is not failed")
    require(
        conclusion == candidate["conclusion"], "The workflow job conclusion differs from its check"
    )

    expected_run_url = f"https://api.github.com/repos/{context.repo}/actions/runs/{run_id}"
    require(job.get("run_url") == expected_run_url, "The workflow job belongs to another run URL")
    expected_job_url = f"https://api.github.com/repos/{context.repo}/actions/jobs/{job_id}"
    require(job.get("url") == expected_job_url, "The workflow job URL changed")
    expected_html_url = f"https://github.com/{context.repo}/actions/runs/{run_id}/job/{job_id}"
    require(job.get("html_url") == expected_html_url, "The workflow job HTML URL changed")
    expected_check_url = f"https://api.github.com/repos/{context.repo}/check-runs/{candidate['id']}"
    require(
        job.get("check_run_url") == expected_check_url, "The workflow job belongs to another check"
    )

    started_at = timestamp(job.get("started_at"), "CLA workflow job.started_at")
    completed_at = timestamp(job.get("completed_at"), "CLA workflow job.completed_at")
    comment_time = timestamp(context.comment_created_at, "COMMENT_CREATED_AT")
    require(completed_at >= started_at, "CLA workflow job.completed_at precedes started_at")
    require(completed_at <= comment_time, "The workflow job completed after the comment")

    if job.get("head_repository") is not None:
        expected_job_repo = (
            snapshot.head_repo if job_head_sha == snapshot.head_sha else snapshot.base_repo
        )
        expected_job_repo_id = (
            snapshot.head_repo_id if job_head_sha == snapshot.head_sha else snapshot.base_repo_id
        )
        repository_matches(
            job.get("head_repository"),
            expected_job_repo,
            expected_job_repo_id,
            "CLA workflow job.head_repository",
        )

    steps = array(job.get("steps"), "CLA workflow job.steps")
    require(0 < len(steps) <= MAX_JOB_STEPS, "The workflow job has an invalid step count")
    seen_numbers: set[int] = set()
    workflow_marker = f"CLA generation {context.generation}"
    marker_seen = False
    for index, raw_step in enumerate(steps):
        step = obj(raw_step, f"CLA workflow job.steps[{index}]")
        step_number = integer(step.get("number"), f"CLA workflow job.steps[{index}].number")
        require(step_number <= MAX_STEP_NUMBER, "The workflow job step number is too large")
        require(step_number not in seen_numbers, "The workflow job contains duplicate step numbers")
        seen_numbers.add(step_number)
        step_name = safe_metadata(
            string(step.get("name"), f"CLA workflow job.steps[{index}].name"),
            f"CLA workflow job.steps[{index}].name",
        )
        require(
            0 < len(step_name) <= MAX_STEP_NAME_LENGTH, "The workflow job step name is too long"
        )
        step_status = string(step.get("status"), f"CLA workflow job.steps[{index}].status")
        require(step_status in STATUSES, "The workflow job step status is invalid")
        step_conclusion = step.get("conclusion")
        if step_conclusion is not None:
            step_conclusion = safe_metadata(
                string(step_conclusion, f"CLA workflow job.steps[{index}].conclusion"),
                f"CLA workflow job.steps[{index}].conclusion",
            )
            require(step_conclusion in CONCLUSIONS, "The workflow job step conclusion is invalid")
        if step_status == "completed":
            require(step_conclusion is not None, "A completed workflow job step has no conclusion")
        else:
            require(step_conclusion is None, "An incomplete workflow job step has a conclusion")
        if step_name == workflow_marker:
            require(not marker_seen, "The workflow job contains duplicate generation markers")
            require(
                step_status == "completed" and step_conclusion == "success",
                "The workflow generation marker did not execute",
            )
            marker_seen = True
        step_started = optional_timestamp(
            step.get("started_at"), f"CLA workflow job.steps[{index}].started_at"
        )
        step_completed = optional_timestamp(
            step.get("completed_at"), f"CLA workflow job.steps[{index}].completed_at"
        )
        if step_started is not None and step_completed is not None:
            require(
                step_completed >= step_started, "A workflow job step completed before it started"
            )
        if step_completed is not None:
            require(
                step_completed <= comment_time, "A workflow job step completed after the comment"
            )
    require(marker_seen, "The workflow job has no immutable generation marker")


def validate_ledger(api: Any, context: Context, snapshot: Snapshot) -> None:
    response = obj(
        api.get(
            f"repos/{context.repo}/contents/{SIGNATURES_PATH}",
            {"ref": SIGNATURES_BRANCH},
        ),
        "CLA signature ledger response",
    )
    require(
        response.get("type") == "file" and response.get("encoding") == "base64",
        "The CLA ledger response is malformed",
    )
    encoded = string(response.get("content"), "CLA ledger content")
    compact = re.sub(r"[ \t\r\n]", "", encoded)
    require(
        len(compact) <= 2_000_000 and len(compact) % 4 == 0,
        "The CLA ledger is too large or not base64",
    )
    require(
        re.fullmatch(r"[A-Za-z0-9+/]*={0,2}", compact) is not None, "The CLA ledger is not base64"
    )
    try:
        decoded = base64.b64decode(compact, validate=True)
    except (ValueError, base64.binascii.Error):
        reject("The CLA ledger is not valid base64")
    require(len(decoded) <= MAX_LEDGER_BYTES, "The CLA ledger exceeds its byte bound")
    try:
        ledger = json.loads(decoded, object_pairs_hook=unique_object_pairs)
    except (UnicodeDecodeError, RecursionError, ValueError):
        reject("The CLA ledger is not valid JSON")
    ledger = obj(ledger, "CLA ledger")
    records = array(ledger.get("signedContributors"), "CLA ledger signedContributors")
    require(len(records) <= MAX_LEDGER_RECORDS, "The CLA ledger has too many records")
    seen_ids: set[int] = set()
    found = False
    for index, raw in enumerate(records):
        record = obj(raw, f"CLA ledger record {index}")
        name = safe_metadata(
            string(record.get("name"), f"CLA ledger record {index}.name"),
            f"CLA ledger record {index}.name",
        )
        record_id = integer(record.get("id"), f"CLA ledger record {index}.id")
        require(record_id not in seen_ids, "The CLA ledger contains duplicate contributor IDs")
        seen_ids.add(record_id)
        for key in ("comment_id", "repoId", "pullRequestNo"):
            if key in record:
                integer(record[key], f"CLA ledger record {index}.{key}")
        if "created_at" in record:
            timestamp(record["created_at"], f"CLA ledger record {index}.created_at")
        if (
            name == context.comment_author_login
            and record_id == context.comment_author_id
            and record.get("comment_id") == context.comment_id
            and record.get("created_at") == context.comment_created_at
            and record.get("repoId") == snapshot.base_repo_id
            and record.get("pullRequestNo") == context.pr_number
        ):
            found = True
    require(found, "The exact signing comment is not present in the trusted ledger")


def revalidate_before_write(
    api: Any,
    context: Context,
    original_snapshot: Snapshot,
    workflow_id: int,
    candidate: dict[str, Any],
) -> int:
    validate_issue(api, context)
    validate_comment(api, context)
    current_snapshot = live_snapshot(api, context)
    require(
        current_snapshot.identity() == original_snapshot.identity(),
        "The pull request changed before rerun",
    )
    latest = latest_matching_check(api, context, current_snapshot)
    require(latest is not None, "The selected CLA check disappeared")
    require(latest["id"] == candidate["id"], "A newer CLA check appeared before rerun")
    require(
        latest["status"] == "completed" and latest["conclusion"] in FAILURE_CONCLUSIONS,
        "The selected CLA check is no longer failed",
    )
    validate_check_detail(api, context, latest)
    run_id, job_id, execution_sha = validate_workflow_run(
        api, context, current_snapshot, workflow_id, latest
    )
    if context.comment_body == SIGN_PHRASE:
        validate_ledger(api, context, current_snapshot)
    validate_workflow_job(api, context, current_snapshot, latest, run_id, job_id, execution_sha)
    return job_id


def execute(environment: dict[str, str], api: Any, git_head: str | None = None) -> bool:
    context = Context.from_environment(environment)
    if git_head is None:
        completed = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            check=False,
            capture_output=True,
            text=True,
            timeout=15,
        )
        require(completed.returncode == 0, "Unable to determine the workflow checkout")
        git_head = completed.stdout.strip()
    require(
        sha(git_head, "workflow checkout") == context.workflow_sha,
        "The workflow checkout is not immutable",
    )
    api = api or GhApi(environment)
    validate_issue(api, context)
    validate_comment(api, context)
    snapshot = live_snapshot(api, context)
    if context.comment_body == RECHECK:
        require(
            context.comment_author_id == snapshot.author_id
            or context.comment_author_association in RECHECK_ASSOCIATIONS,
            "The recheck commenter is not authorized",
        )
    workflow_id = validate_workflow(api, context)
    if context.comment_body == SIGN_PHRASE:
        validate_ledger(api, context, snapshot)
    candidate = latest_matching_check(api, context, snapshot)
    if (
        candidate is None
        or candidate["status"] != "completed"
        or candidate["conclusion"] not in FAILURE_CONCLUSIONS
    ):
        print("::notice title=CLA refresh::No failed historical CLA check requires a rerun.")
        return False
    validate_check_detail(api, context, candidate)
    run_id, job_id, execution_sha = validate_workflow_run(
        api, context, snapshot, workflow_id, candidate
    )
    validate_workflow_job(api, context, snapshot, candidate, run_id, job_id, execution_sha)
    job_id = revalidate_before_write(api, context, snapshot, workflow_id, candidate)
    api.post(f"repos/{context.repo}/actions/jobs/{job_id}/rerun")
    print(
        "::notice title=CLA refresh::Reran the exact failed CLA job for the exact pull-request head."
    )
    return True


def main() -> int:
    try:
        execute(dict(os.environ))
    except Rejected as error:
        print(f"::error title=CLA refresh policy::{error}", file=sys.stderr)
        return 1
    except (OSError, subprocess.SubprocessError) as error:
        print(f"::error title=CLA refresh setup::{error.__class__.__name__}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
