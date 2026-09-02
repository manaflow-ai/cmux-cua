#!/usr/bin/env python3
"""Behavior tests for the privileged CLA refresh worker."""

from __future__ import annotations

import copy
import base64
import importlib.util
import json
import pathlib
import sys
import unittest


SCRIPT = pathlib.Path(__file__).parents[1] / "rerun-failed-cla.py"
SPEC = importlib.util.spec_from_file_location("cla_refresh", SCRIPT)
assert SPEC and SPEC.loader
cla = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = cla
SPEC.loader.exec_module(cla)


HEAD_SHA = "a" * 40
BASE_SHA = "b" * 40
WORKFLOW_SHA = "c" * 40
REPO = "manaflow-ai/cmux-cua"
COMMENT_ID = 9001
RUN_ID = 900
JOB_ID = 901
CHECK_ID = 901
COMMENT_TIME = "2026-08-31T12:00:00Z"
CLA_GENERATION = cla.CLA_GENERATION
COMMENT_URL = f"https://github.com/{REPO}/pull/17#issuecomment-{COMMENT_ID}"
COMMENT_API_URL = f"https://api.github.com/repos/{REPO}/issues/comments/{COMMENT_ID}"
MISSING = object()


def environment(body: str = cla.RECHECK) -> dict[str, str]:
    return {
        "EVENT_NAME": "issue_comment",
        "GH_REPO": REPO,
        "EVENT_REPO_FULL_NAME": REPO,
        "EVENT_REPO_ID": "123",
        "PR_NUMBER": "17",
        "ISSUE_NUMBER": "17",
        "COMMENT_ID": str(COMMENT_ID),
        "COMMENT_BODY": body,
        "COMMENT_AUTHOR_ID": "77",
        "COMMENT_AUTHOR_LOGIN": "alice",
        "COMMENT_AUTHOR_TYPE": "User",
        "COMMENT_AUTHOR_ASSOCIATION": "NONE",
        "COMMENT_CREATED_AT": COMMENT_TIME,
        "COMMENT_UPDATED_AT": COMMENT_TIME,
        "COMMENT_URL": COMMENT_URL,
        "COMMENT_API_URL": COMMENT_API_URL,
        "TARGET_BASE_REF": "main",
        "WORKFLOW_SHA": WORKFLOW_SHA,
        "WORKFLOW_PATH": cla.WORKFLOW_PATH,
        "CLA_GENERATION": CLA_GENERATION,
        "WRITER_RESULT": "success",
        "SIGNATURE_RECORDED": "true" if body == cla.SIGN_PHRASE else "false",
        # A rerun is permitted only after the issue-comment writer reports a
        # final, all-signed result. Partial signing leaves the required check
        # failed.
        "CLA_PASSED": "true" if body in (cla.SIGN_PHRASE, cla.RECHECK) else "",
        "EVENT_PR_NUMBER": "",
        "EVENT_PR_AUTHOR_ID": "",
        "EVENT_PR_AUTHOR_LOGIN": "",
        "EVENT_PR_AUTHOR_TYPE": "",
        "EVENT_HEAD_SHA": "",
        "EVENT_HEAD_REF": "",
        "EVENT_HEAD_REPO_FULL_NAME": "",
        "EVENT_HEAD_REPO_ID": "",
        "EVENT_BASE_SHA": "",
        "EVENT_BASE_REF": "",
        "EVENT_BASE_REPO_FULL_NAME": "",
        "EVENT_BASE_REPO_ID": "",
    }


def check_record(
    *,
    name: str = "CLA Assistant v3",
    app_name: str = "GitHub Actions",
    app_slug: str = "github-actions",
    conclusion: str | None = "failure",
    check_id: int = CHECK_ID,
    job_id: int = JOB_ID,
    run_id: int = RUN_ID,
    completed_at: str = "2026-08-31T11:00:00Z",
    head_sha: str = HEAD_SHA,
) -> dict:
    return {
        "id": check_id,
        "name": name,
        "head_sha": head_sha,
        "status": "completed",
        "conclusion": conclusion,
        "app": {"id": cla.GITHUB_ACTIONS_APP_ID, "name": app_name, "slug": app_slug},
        "created_at": "2026-08-31T10:00:00Z",
        "updated_at": "2026-08-31T11:00:00Z",
        "started_at": "2026-08-31T10:01:00Z",
        "completed_at": completed_at,
        "details_url": f"https://github.com/{REPO}/actions/runs/{run_id}/job/{job_id}",
    }


def pull_record(*, head_repo: str = REPO, head_repo_id: int = 123) -> dict:
    return {
        "number": 17,
        "state": "open",
        "merged_at": None,
        "base": {
            "ref": "main",
            "sha": BASE_SHA,
            "repo": {"full_name": REPO, "id": 123},
        },
        "head": {
            "ref": "feature",
            "sha": HEAD_SHA,
            "repo": {"full_name": head_repo, "id": head_repo_id},
        },
        "user": {"login": "alice", "id": 77, "type": "User"},
    }


def run_record(*, head_repo: str = REPO, head_repo_id: int = 123) -> dict:
    return {
        "id": RUN_ID,
        "workflow_id": 4567,
        "name": cla.WORKFLOW_NAME,
        "path": cla.WORKFLOW_PATH,
        "html_url": f"https://github.com/{REPO}/actions/runs/{RUN_ID}",
        "event": "pull_request_target",
        "status": "completed",
        "conclusion": "failure",
        "head_sha": HEAD_SHA,
        "head_branch": "feature",
        "head_repository": {"full_name": head_repo, "id": head_repo_id},
        "repository": {"full_name": REPO, "id": 123},
        "created_at": "2026-08-31T10:00:00Z",
        "updated_at": "2026-08-31T11:00:00Z",
        "pull_requests": [],
    }


def association_record(*, head_repo: str = REPO, head_repo_id: int = 123) -> dict:
    return {
        "number": 17,
        "base": {
            "ref": "main",
            "sha": BASE_SHA,
            "repo": {
                "id": 123,
                "name": "cmux-cua",
                "url": f"https://api.github.com/repos/{REPO}",
            },
        },
        "head": {
            "ref": "feature",
            "sha": HEAD_SHA,
            "repo": {
                "id": head_repo_id,
                "name": head_repo.rsplit("/", 1)[-1],
                "url": f"https://api.github.com/repos/{head_repo}",
            },
        },
    }


def job_record(
    *,
    name: str = "CLA Assistant v3",
    workflow_name: str = cla.WORKFLOW_NAME,
    conclusion: str | None = "failure",
    run_id: int = RUN_ID,
    head_sha: str = HEAD_SHA,
) -> dict:
    return {
        "id": JOB_ID,
        "run_id": run_id,
        "run_url": f"https://api.github.com/repos/{REPO}/actions/runs/{RUN_ID}",
        "url": f"https://api.github.com/repos/{REPO}/actions/jobs/{JOB_ID}",
        "check_run_url": f"https://api.github.com/repos/{REPO}/check-runs/{CHECK_ID}",
        "head_sha": head_sha,
        "status": "completed",
        "conclusion": conclusion,
        "started_at": "2026-08-31T10:01:00Z",
        "completed_at": "2026-08-31T11:00:00Z",
        "name": name,
        "workflow_name": workflow_name,
        "head_branch": "feature",
        # GitHub returns null for head_repository on pull_request_target jobs.
        "head_repository": None,
        "html_url": f"https://github.com/{REPO}/actions/runs/{run_id}/job/{JOB_ID}",
        "steps": [
            {
                "name": "Require GitHub-hosted runner",
                "status": "completed",
                "conclusion": "success",
                "number": 1,
                "started_at": "2026-08-31T10:01:00Z",
                "completed_at": "2026-08-31T10:01:01Z",
            },
            {
                "name": f"CLA generation {CLA_GENERATION}",
                "status": "completed",
                "conclusion": "success",
                "number": 2,
                "started_at": "2026-08-31T10:01:01Z",
                "completed_at": "2026-08-31T11:00:00Z",
            },
        ],
    }


class FakeApi:
    def __init__(
        self,
        check_runs: list[dict] | None = None,
        total: int | None = None,
        *,
        head_repo: str = REPO,
        head_repo_id: int = 123,
        run_execution_sha: str = HEAD_SHA,
        run_head_repository: dict | None = None,
        run_head_repository_null: bool = False,
        run_pull_requests: list[dict] | None = None,
        open_pull_requests: list[dict] | None = None,
    ):
        self.check_runs = [check_record()] if check_runs is None else check_runs
        self.total = len(self.check_runs) if total is None else total
        self.head_repo = head_repo
        self.head_repo_id = head_repo_id
        self.run_execution_sha = run_execution_sha
        self.run_head_repository = run_head_repository
        self.run_head_repository_null = run_head_repository_null
        self.run_pull_requests = run_pull_requests
        self.open_pull_requests = (
            [pull_record(head_repo=head_repo, head_repo_id=head_repo_id)]
            if open_pull_requests is None
            else open_pull_requests
        )
        self.posts: list[str] = []
        self.job = job_record()
        self.gets: list[str] = []
        self.comment = {
            "id": COMMENT_ID,
            "body": cla.RECHECK,
            "created_at": COMMENT_TIME,
            "updated_at": COMMENT_TIME,
            "url": COMMENT_API_URL,
            "html_url": COMMENT_URL,
            "issue_url": f"https://api.github.com/repos/{REPO}/issues/17",
            "author_association": "NONE",
            "user": {"id": 77, "login": "alice", "type": "User"},
        }

    def get(self, endpoint: str, query: dict[str, str] | None = None):
        self.gets.append(endpoint)
        for route in (
            self._get_issue_route,
            self._get_check_route,
            self._get_run_route,
            self._get_ledger_route,
        ):
            result = route(endpoint, query)
            if result is not MISSING:
                return result
        raise AssertionError(f"unexpected GET {endpoint} {query}")

    def _get_issue_route(self, endpoint: str, _query: dict[str, str] | None):
        if endpoint.endswith("/issues/17"):
            return {
                "number": 17,
                "state": "open",
                "pull_request": {"url": f"https://api.github.com/repos/{REPO}/pulls/17"},
            }
        if endpoint.endswith("/issues/comments/9001"):
            return copy.deepcopy(self.comment)
        if endpoint.endswith("/pulls/17"):
            return pull_record(head_repo=self.head_repo, head_repo_id=self.head_repo_id)
        if endpoint.endswith("/pulls"):
            return copy.deepcopy(self.open_pull_requests)
        return MISSING

    def _get_check_route(self, endpoint: str, query: dict[str, str] | None):
        if "/actions/workflows/" in endpoint:
            return {
                "id": 4567,
                "name": cla.WORKFLOW_NAME,
                "path": cla.WORKFLOW_PATH,
                "state": "active",
            }
        if endpoint.endswith("/check-runs"):
            page = (query or {}).get("page", "1")
            if page != "1":
                return {"total_count": self.total, "check_runs": []}
            return {"total_count": self.total, "check_runs": copy.deepcopy(self.check_runs)}
        if endpoint.endswith(f"/check-runs/{CHECK_ID}"):
            for check in self.check_runs:
                if check.get("id") == CHECK_ID:
                    return copy.deepcopy(check)
            raise AssertionError(f"missing check {CHECK_ID}")
        return MISSING

    def _get_run_route(self, endpoint: str, _query: dict[str, str] | None):
        if endpoint.endswith(f"/actions/runs/{RUN_ID}"):
            value = run_record(head_repo=self.head_repo, head_repo_id=self.head_repo_id)
            value["head_sha"] = self.run_execution_sha
            if self.run_head_repository_null:
                value["head_repository"] = None
            elif self.run_head_repository is not None:
                value["head_repository"] = copy.deepcopy(self.run_head_repository)
            if self.run_pull_requests is not None:
                value["pull_requests"] = copy.deepcopy(self.run_pull_requests)
            return value
        if endpoint.endswith(f"/actions/jobs/{JOB_ID}"):
            return copy.deepcopy(self.job)
        return MISSING

    def _get_ledger_route(self, endpoint: str, _query: dict[str, str] | None):
        if endpoint.endswith("/contents/signatures/version2/cla.json"):
            ledger = {
                "signedContributors": [
                    {
                        "name": "alice",
                        "id": 77,
                        "comment_id": COMMENT_ID,
                        "created_at": COMMENT_TIME,
                        "repoId": 123,
                        "pullRequestNo": 17,
                    }
                ]
            }
            encoded = base64.b64encode(json.dumps(ledger).encode()).decode()
            return {"type": "file", "encoding": "base64", "content": encoded}
        return MISSING

    def post(self, endpoint: str):
        self.posts.append(endpoint)
        return {}


class RefreshWorkerTests(unittest.TestCase):
    def execute(self, api: FakeApi, body: str = cla.RECHECK) -> bool:
        env = environment(body)
        return cla.execute(env, api, git_head=WORKFLOW_SHA)

    def test_case_folded_actions_job_name_is_rejected(self):
        api = FakeApi(
            [
                check_record(
                    name="cLa AsSiStAnT v3",
                    app_name="gItHuB aCtIoNs",
                    app_slug=None,
                )
            ]
        )
        self.assertFalse(self.execute(api))
        self.assertEqual(api.posts, [])

    def test_noncanonical_actions_app_name_is_ignored(self):
        api = FakeApi([check_record(app_name="gItHuB aCtIoNs")])
        self.assertFalse(self.execute(api))
        self.assertEqual(api.posts, [])

    def test_wrong_job_is_rejected_without_post(self):
        api = FakeApi()
        api.job["name"] = "Untrusted job"
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_signing_writer_check_is_ignored_on_recheck(self):
        api = FakeApi(
            [
                check_record(
                    name="CLA Signature Writer",
                    check_id=902,
                    job_id=903,
                    completed_at="2026-08-31T11:30:00Z",
                ),
                check_record(name="CLA Assistant v3"),
            ]
        )
        self.assertTrue(self.execute(api))
        self.assertEqual(api.posts, [f"repos/{REPO}/actions/jobs/{JOB_ID}/rerun"])

    def test_signed_recheck_refreshes(self):
        api = FakeApi()
        self.assertTrue(self.execute(api, cla.RECHECK))
        self.assertEqual(api.posts, [f"repos/{REPO}/actions/jobs/{JOB_ID}/rerun"])

    def test_recheck_with_new_signature_also_refreshes(self):
        api = FakeApi()
        env = environment(cla.RECHECK)
        env["SIGNATURE_RECORDED"] = "true"
        self.assertTrue(cla.execute(env, api, git_head=WORKFLOW_SHA))
        self.assertEqual(api.posts, [f"repos/{REPO}/actions/jobs/{JOB_ID}/rerun"])

    def test_partial_recheck_does_not_refresh(self):
        api = FakeApi()
        env = environment(cla.RECHECK)
        env["CLA_PASSED"] = "false"
        with self.assertRaises(cla.Rejected):
            cla.execute(env, api, git_head=WORKFLOW_SHA)
        self.assertEqual(api.posts, [])

    def test_failed_recheck_writer_does_not_refresh(self):
        api = FakeApi()
        env = environment(cla.RECHECK)
        env["WRITER_RESULT"] = "failure"
        with self.assertRaises(cla.Rejected):
            cla.execute(env, api, git_head=WORKFLOW_SHA)
        self.assertEqual(api.posts, [])

    def test_recheck_requires_opener_or_trusted_association(self):
        api = FakeApi()
        api.comment["user"] = {"id": 88, "login": "reviewer", "type": "User"}
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_legacy_required_check_name_is_not_rerun(self):
        api = FakeApi([check_record(name="CLA Assistant")])
        self.assertFalse(self.execute(api))
        self.assertEqual(api.posts, [])

    def test_fork_head_with_null_job_repository_is_accepted(self):
        api = FakeApi(head_repo="contributor/fork", head_repo_id=456)
        self.assertTrue(self.execute(api))
        self.assertEqual(api.posts, [f"repos/{REPO}/actions/jobs/{JOB_ID}/rerun"])

    def test_null_run_repository_requires_live_open_pull_request(self):
        api = FakeApi(run_head_repository_null=True, open_pull_requests=[])
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_duplicate_live_open_pull_requests_fail_closed(self):
        api = FakeApi(open_pull_requests=[pull_record(), pull_record()])
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_full_live_open_pull_request_window_fails_closed(self):
        api = FakeApi(open_pull_requests=[pull_record()] * cla.PAGE_SIZE)
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_base_execution_metadata_is_accepted_when_check_binds_source_head(self):
        api = FakeApi(
            run_execution_sha=BASE_SHA,
            run_head_repository={"full_name": REPO, "id": 123},
        )
        api.job["head_sha"] = BASE_SHA
        api.job["head_repository"] = {"full_name": REPO, "id": 123}
        self.assertTrue(self.execute(api))
        self.assertEqual(api.posts, [f"repos/{REPO}/actions/jobs/{JOB_ID}/rerun"])

    def test_association_repository_identity_uses_api_url_and_id(self):
        api = FakeApi(run_pull_requests=[association_record()])
        self.assertTrue(self.execute(api))
        self.assertEqual(api.posts, [f"repos/{REPO}/actions/jobs/{JOB_ID}/rerun"])

    def test_wrong_association_fails_closed(self):
        api = FakeApi(
            run_pull_requests=[association_record(head_repo="other/repo", head_repo_id=999)]
        )
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_stale_base_sha_in_run_association_fails_closed(self):
        association = association_record()
        association["base"]["sha"] = HEAD_SHA
        api = FakeApi(run_pull_requests=[association])
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_duplicate_run_association_is_not_hidden_by_a_matching_entry(self):
        first = association_record()
        second = association_record()
        second["head"]["ref"] = "other"
        api = FakeApi(run_pull_requests=[first, second])
        self.assertTrue(self.execute(api))
        self.assertEqual(api.posts, [f"repos/{REPO}/actions/jobs/{JOB_ID}/rerun"])

    def test_duplicate_run_pull_request_association_fails_closed(self):
        api = FakeApi(run_pull_requests=[association_record(), association_record()])
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_untrusted_run_execution_sha_is_rejected(self):
        api = FakeApi(run_execution_sha="d" * 40)
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_job_head_must_match_run_execution_sha(self):
        api = FakeApi(run_execution_sha=BASE_SHA)
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_run_html_url_is_bound_to_run_id(self):
        api = FakeApi()
        api.run_html_url = "https://github.com/other/repo/actions/runs/900"
        original_get = api.get

        def get(endpoint, query=None):
            value = original_get(endpoint, query)
            if endpoint.endswith(f"/actions/runs/{RUN_ID}"):
                value["html_url"] = api.run_html_url
            return value

        api.get = get
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_run_name_is_bound_to_workflow_name(self):
        class WrongNameApi(FakeApi):
            def get(self, endpoint, query=None):
                value = super().get(endpoint, query)
                if endpoint.endswith(f"/actions/runs/{RUN_ID}"):
                    value["name"] = "Untrusted workflow"
                return value

        api = WrongNameApi()
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_workflow_path_suffix_is_rejected(self):
        class WrongPathApi(FakeApi):
            def get(self, endpoint, query=None):
                value = super().get(endpoint, query)
                if "/actions/workflows/" in endpoint:
                    value["path"] = f"{cla.WORKFLOW_PATH}@main"
                return value

        api = WrongPathApi()
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_successful_job_is_rejected_without_post(self):
        api = FakeApi()
        api.job["conclusion"] = "success"
        api.job["steps"][1]["conclusion"] = "success"
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_job_without_current_workflow_marker_is_rejected(self):
        api = FakeApi()
        api.job["steps"][1]["name"] = "Run the immutable CLA action"
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_failed_generation_marker_is_rejected(self):
        api = FakeApi()
        api.job["steps"][1]["conclusion"] = "failure"
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_oversized_comment_is_rejected_before_api_access(self):
        api = FakeApi()
        env = environment()
        env["COMMENT_BODY"] = "x" * (cla.MAX_COMMENT_BYTES + 1)
        with self.assertRaises(cla.Rejected):
            cla.execute(env, api, git_head=WORKFLOW_SHA)
        self.assertEqual(api.gets, [])
        self.assertEqual(api.posts, [])

    def test_edited_comment_fails_closed_without_post(self):
        api = FakeApi()
        api.comment["updated_at"] = "2026-08-31T12:01:00Z"
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_duplicate_check_id_fails_closed_without_post(self):
        api = FakeApi([check_record(), check_record()])
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_duplicate_check_binding_fails_closed_without_post(self):
        duplicate = check_record(check_id=902)
        api = FakeApi([check_record(), duplicate])
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_oversized_check_list_fails_closed_without_post(self):
        api = FakeApi([check_record()], total=cla.MAX_CHECK_RUNS + 1)
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_latest_successful_check_is_not_rerun(self):
        api = FakeApi([check_record(conclusion="success")])
        self.assertFalse(self.execute(api))
        self.assertEqual(api.posts, [])

    def test_empty_check_list_is_a_safe_noop(self):
        api = FakeApi([])
        self.assertFalse(self.execute(api))
        self.assertEqual(api.posts, [])

    def test_force_push_between_snapshots_fails_closed(self):
        class ForcePushApi(FakeApi):
            pull_calls = 0

            def get(self, endpoint, query=None):
                value = super().get(endpoint, query)
                if endpoint.endswith("/pulls/17"):
                    self.pull_calls += 1
                    if self.pull_calls > 1:
                        value["head"]["sha"] = "d" * 40
                return value

        api = ForcePushApi()
        with self.assertRaises(cla.Rejected):
            self.execute(api)
        self.assertEqual(api.posts, [])

    def test_signing_comment_requires_the_exact_persisted_ledger_record(self):
        api = FakeApi()
        api.comment["body"] = cla.SIGN_PHRASE
        self.assertTrue(self.execute(api, cla.SIGN_PHRASE))
        self.assertEqual(len(api.posts), 1)

    def test_signing_comment_with_partial_cla_result_does_not_refresh(self):
        api = FakeApi()
        api.comment["body"] = cla.SIGN_PHRASE
        env = environment(cla.SIGN_PHRASE)
        env["CLA_PASSED"] = "false"
        with self.assertRaises(cla.Rejected):
            cla.execute(env, api, git_head=WORKFLOW_SHA)
        self.assertEqual(api.posts, [])

    def test_signing_comment_requires_cla_result_output(self):
        api = FakeApi()
        api.comment["body"] = cla.SIGN_PHRASE
        env = environment(cla.SIGN_PHRASE)
        env["CLA_PASSED"] = ""
        with self.assertRaises(cla.Rejected):
            cla.execute(env, api, git_head=WORKFLOW_SHA)
        self.assertEqual(api.posts, [])

    def test_invalid_cla_result_fails_closed(self):
        api = FakeApi()
        env = environment()
        env["CLA_PASSED"] = "unknown"
        with self.assertRaises(cla.Rejected):
            cla.execute(env, api, git_head=WORKFLOW_SHA)
        self.assertEqual(api.posts, [])

    def test_recheck_requires_a_final_all_signed_result(self):
        api = FakeApi()
        env = environment()
        env["CLA_PASSED"] = "false"
        with self.assertRaises(cla.Rejected):
            cla.execute(env, api, git_head=WORKFLOW_SHA)
        self.assertEqual(api.posts, [])

    def test_recheck_can_carry_a_new_signature_result(self):
        api = FakeApi()
        env = environment()
        env["SIGNATURE_RECORDED"] = "true"
        self.assertTrue(cla.execute(env, api, git_head=WORKFLOW_SHA))
        self.assertEqual(api.posts, [f"repos/{REPO}/actions/jobs/{JOB_ID}/rerun"])

    def test_legacy_unversioned_sign_phrase_is_rejected(self):
        api = FakeApi()
        legacy_phrase = "I have read the CLA Document and I hereby sign the CLA"
        api.comment["body"] = legacy_phrase
        with self.assertRaises(cla.Rejected):
            self.execute(api, legacy_phrase)
        self.assertEqual(api.posts, [])

    def test_duplicate_ledger_key_fails_closed(self):
        class DuplicateLedgerApi(FakeApi):
            def get(self, endpoint, query=None):
                if endpoint.endswith("/contents/signatures/version2/cla.json"):
                    raw = b'{"signedContributors": [], "signedContributors": []}'
                    return {
                        "type": "file",
                        "encoding": "base64",
                        "content": base64.b64encode(raw).decode(),
                    }
                return super().get(endpoint, query)

        api = DuplicateLedgerApi()
        api.comment["body"] = cla.SIGN_PHRASE
        with self.assertRaises(cla.Rejected):
            self.execute(api, cla.SIGN_PHRASE)
        self.assertEqual(api.posts, [])

    def test_nonstandard_json_number_in_ledger_fails_closed(self):
        class NonstandardLedgerApi(FakeApi):
            def get(self, endpoint, query=None):
                if endpoint.endswith("/contents/signatures/version2/cla.json"):
                    raw = (
                        b'{"signedContributors": [{"name": "alice", "id": 77,'
                        b' "comment_id": 9001, "created_at": "2026-08-31T12:00:00Z",'
                        b' "repoId": 123, "pullRequestNo": 17}], "unexpected": NaN}'
                    )
                    return {
                        "type": "file",
                        "encoding": "base64",
                        "content": base64.b64encode(raw).decode(),
                    }
                return super().get(endpoint, query)

        api = NonstandardLedgerApi()
        api.comment["body"] = cla.SIGN_PHRASE
        with self.assertRaises(cla.Rejected):
            self.execute(api, cla.SIGN_PHRASE)
        self.assertEqual(api.posts, [])

    def test_api_stdout_is_bounded(self):
        api = cla.GhApi({"GH_TOKEN": "test-token"})
        result = api._run_bounded(
            [
                sys.executable,
                "-c",
                "import sys; sys.stdout.write('x' * 2000001)",
            ]
        )
        self.assertTrue(result.overflow)
        self.assertLessEqual(len(result.stdout), cla.MAX_API_RESPONSE_BYTES + 1)

    def test_api_stderr_is_bounded(self):
        api = cla.GhApi({"GH_TOKEN": "test-token"})
        result = api._run_bounded(
            [
                sys.executable,
                "-c",
                "import sys; sys.stderr.write('x' * 65537)",
            ]
        )
        self.assertTrue(result.overflow)
        self.assertLessEqual(len(result.stderr), cla.MAX_API_ERROR_BYTES + 1)

    def test_api_query_values_reject_line_breaks_before_spawn(self):
        api = cla.GhApi({"GH_TOKEN": "test-token"})
        with self.assertRaises(cla.Rejected):
            api.request(
                "GET",
                f"repos/{REPO}/issues/17",
                {"page": "1\n--header=evil"},
            )

    def test_comment_url_must_be_canonical_before_api_access(self):
        api = FakeApi()
        env = environment()
        env["COMMENT_URL"] = f"https://evil.example/{COMMENT_ID}"
        with self.assertRaises(cla.Rejected):
            cla.execute(env, api, git_head=WORKFLOW_SHA)
        self.assertEqual(api.gets, [])

    def test_unexpected_event_is_rejected_before_api_access(self):
        api = FakeApi()
        env = environment()
        env["EVENT_NAME"] = "pull_request_target"
        with self.assertRaises(cla.Rejected):
            cla.execute(env, api, git_head=WORKFLOW_SHA)
        self.assertEqual(api.gets, [])


if __name__ == "__main__":
    unittest.main()
