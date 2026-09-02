# Release workflow security

Release workflows publish from protected semantic-version tags. A manual run
can build or validate code, but it never receives a registry, package-index, or
GitHub App credential.

## Required GitHub controls

Create an active tag ruleset with the pattern `*-v*` (for example `agent-v*`)
and deny tag creation, update, and deletion except to the release automation
App and the trusted release maintainers. The trusted maintainers are
`austinywang` and `azooz2003-bit`. Require pull requests and the required checks
on `main`. The verifier accepts a tag only when its exact commit is an ancestor
of a stable snapshot of the current `main` history and the tag is protected.

Configure these environments with required reviewers and deployment branch or
tag restrictions:

| Environment | Credential | Used by |
| --- | --- | --- |
| `pypi-release` | PyPI Trusted Publishing (OIDC) | Python package callers |
| `npm` | npm Trusted Publishing (OIDC) | TypeScript reusable publisher |
| `docker-release` | `DOCKER_HUB_RELEASE_TOKEN` | Docker reusable publisher |
| `release-app` | `RELEASE_APP_ID`, `RELEASE_APP_PRIVATE_KEY` | Cua Driver docs PR |
| `github-release` | GitHub Actions token (environment-scoped) | GitHub release reusable workflow |
| `benchmark-secrets` | model and Slack keys | Scheduled model benchmarks |

Do not create repository-level fallbacks for these credentials. In particular,
the Docker callers intentionally pass no secret. The called publisher reads
`DOCKER_HUB_RELEASE_TOKEN` only after the `docker-release` environment approves
the job and the tag verifier passes.

The package metadata and current PyPI/npm Trusted Publisher records belong to
`trycua/cua`. These workflows therefore fail closed when they run in the
`manaflow-ai/cmux-cua` fork. Run a release from the configured canonical
repository, or migrate the registry records and package metadata together
before changing the trusted repository inputs.

GitHub loads a tag-triggered workflow from the tagged commit. A historical tag
could therefore contain an older provenance helper. Every credential-bearing
job keeps release source and artifacts on the tag, but checks out the verifier,
artifact validator, and legacy-token gate from the executing repository's
protected `main` branch under `trusted-release`. A missing helper or failed
checkout stops the job before a credential is requested. Keep `main` protected
and reviewed; changing this checkout back to the tag reintroduces the
historical-workflow risk.

The CuaBot, Kasm, and XFCE container callers set `skip_arm64: true` because
their current upstream base or package repositories do not provide a supported
arm64 build. The reusable Docker workflow still builds arm64 for every other
caller and fails if that platform cannot build.

## Workflow invariants

Each credentialed caller has a tag-only publish job. It checks the protected-tag
flag, verifies the tag target and main ancestry, keeps release source bound to
the triggering SHA, and performs a second verification with the protected-main
helper before requesting a credential. Build-only manual jobs have read-only
contents access.

The reusable Docker publisher has no `workflow_dispatch` trigger. Its callers'
manual dispatch paths use `docker-reusable-build.yml`, which never logs in or
pushes. The publisher requires every expected platform digest before creating
the version, major, minor, and `latest` manifests.

When changing a release workflow, keep third-party actions pinned to full commit
SHAs and run `actionlint`, `zizmor`, and the focused security contract tests.
