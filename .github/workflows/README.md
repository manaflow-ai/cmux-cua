# Release workflow contract

The reusable npm publisher requires three explicit identity inputs on every
caller:

```yaml
with:
  trusted_publisher_repository: trycua/cua
  trusted_package_name: "@trycua/core"
  trusted_publisher_workflow: cd-ts-core.yml
  trusted_tag_prefix: npm-core-v
  expected_tag: ${{ needs.prepare.outputs.tag }}
  expected_version: ${{ needs.prepare.outputs.version }}
  source_sha: ${{ needs.verify-tag.outputs.commit }}
  source_tag: ${{ needs.verify-tag.outputs.tag }}
```

These values must match the npm Trusted Publishing configuration. The caller
must also grant `id-token: write` to the reusable job. The build creates an
artifact without registry credentials. The identity job checks the artifact's
package name, GitHub repository URL, current repository, and exact caller
workflow, protected tag, and package version before either publisher can run.

Every credentialed publisher also runs the protected-main
`.github/scripts/validate_publisher_repository.py` gate. It hard-codes
`trycua/cua`, the current package owner, and rejects `manaflow-ai/cmux-cua` and
other forks. This ownership check is separate from source SHA and release-tag
provenance. Do not replace it with a workflow input. Migrate package metadata
and the registry Trusted Publisher record together before changing the gate.

Credentialed jobs also have a job-level `github.repository == 'trycua/cua'`
condition. GitHub evaluates this condition before granting the job's
`id-token`, App, AWS, or registry-token permissions, so the manaflow fork is
skipped until the migration is deliberate. The in-job gate remains as defense
in depth and gives a clear error when a configuration is incomplete.

The normal path uses npm Trusted Publishing and the protected `npm`
environment, and publishes only to `https://registry.npmjs.org/`. The legacy
token path is disabled by default. It requires an explicit
`allow_legacy_token: true`, the protected `npm-token-fallback` environment,
and its enablement secret. It is still subject to the same identity check,
publishes only to `https://registry.npmjs.org/`, and never receives an OIDC
token.

The legacy PyPI reusable workflow is also disabled by default. Its protected
fallback requires four explicit identity inputs on every caller:

```yaml
with:
  trusted_publisher_repository: trycua/cua
  trusted_package_name: cua-agent
  trusted_publisher_workflow: cd-py-agent.yml
  trusted_tag_prefix: agent-v
  source_sha: ${{ needs.verify-tag.outputs.commit }}
  source_tag: ${{ needs.verify-tag.outputs.tag }}
```

The fallback checks the current repository, exact protected tag and caller
workflow, then validates the wheel and source archive metadata (package name,
version, archive paths, regular files, and a two-file maximum) before the token
is exposed to Twine. It uploads only to
`https://upload.pypi.org/legacy/`. Normal PyPI Trusted Publishing remains in
each top-level caller because PyPI binds OIDC to that caller's workflow file.

The check fails closed when an input is missing, a package or repository does
not match, a run comes from a fork, or the ref is not a protected release. Do
not configure the canonical `trycua/cua` publisher to run from
`manaflow-ai/cmux-cua`; update the npm trusted-publisher owner and workflow
configuration together before changing these allowlisted values.

Release tags first trigger the credential-free `Release tag request` observer.
Package, container, documentation, and GitHub-release consumers listen only to
that observer's successful `workflow_run` event. Each consumer is resolved from
protected `main`, calls `validate-release-request.yml`, and rechecks the
observer run, exact tag, source SHA, protected-main ancestry, and moving refs
before requesting credentials. Credentialed jobs then run the canonical
publisher gate from protected `main`. The source checkout uses the validated
SHA.
Manual package, container, and documentation dispatch remains build-only and
has no registry or release credentials. The release-bump dispatcher still
accepts only protected `main` in the canonical repository, and its write job is
skipped in this fork. This split prevents workflow code stored in a historical
tag from selecting a credentialed path.

Lume and Rust Cua Driver use the same observer. Their branch-selectable build
entrypoints are separate unsigned CI workflows with read-only repository
access. Signing and release jobs run only from the observer consumer, require
the exact validated tag commit, repeat the protected-main provenance and owner
checks, and validate their release asset names before a write.
