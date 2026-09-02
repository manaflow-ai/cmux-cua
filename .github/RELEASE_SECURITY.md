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
| `lume-release` | Apple signing, notarization, and release App secrets | Lume release |
| `cua-driver-release` | Apple signing, notarization, and release App secrets | Rust Cua Driver release |
| `benchmark-secrets` | model and Slack keys | Scheduled model benchmarks |

Do not create repository-level fallbacks for these credentials. In particular,
the Docker callers intentionally pass no secret. The called publisher reads
`DOCKER_HUB_RELEASE_TOKEN` only after the `docker-release` environment approves
the job and the tag verifier passes.

The package metadata and current PyPI/npm Trusted Publisher records belong to
`trycua/cua`. These workflows therefore fail closed when they run in the
`manaflow-ai/cmux-cua` fork. Run a release from the configured canonical
repository, or migrate the registry records and package metadata together
before changing the trusted repository inputs. Every package, Docker, GitHub
release, documentation, and cua-driver publish or write job has a job-level
`github.repository == 'trycua/cua'` condition before its credential
permissions, and executes the protected-main `validate_publisher_repository.py`
gate immediately before the credentialed operation. The gate has no
caller-controlled repository input. The fork is intentionally skipped until
the migration is complete.

GitHub resolves a `workflow_run` consumer from the default branch. Release tags
first run the credential-free `Release tag request` observer. Consumers then
recheck that observer from protected `main`, including its repository, path,
attempt, conclusion, exact tag target, source SHA, protected-main ancestry, and
moving refs. They check out release source only at the validated SHA. A missing
validator or failed recheck stops the job before a credential is requested. The
source provenance gate and the canonical registry-owner gate are independent,
so passing a source check cannot authorize a fork to publish.
Manual package, container, and documentation dispatch is limited to read-only
build jobs. The release-bump dispatcher accepts only protected `main` and its
write job is canonical-repository gated. Do not restore direct tag triggers for
credential-bearing consumers, because historical tag workflow code would
become executable again.

The Lume and Rust Cua Driver release workflows also consume the generic tag
observer from protected `main`. Their old branch-selectable manual release
entrypoints are replaced by `ci-swift-lume-manual.yml` and
`ci-rust-cua-driver-manual.yml`. Those workflows compile unsigned source with
read-only repository access and cannot sign, notarize, publish, or update a
branch.

The CuaBot, Kasm, and XFCE CI and release container callers set
`skip_arm64: true` because their current upstream base or package repositories
do not provide a supported arm64 build. The reusable Docker workflow still
builds arm64 for every other caller and fails if that platform cannot build.

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
