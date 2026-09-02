# Release workflow contract

The reusable npm publisher requires three explicit identity inputs on every
caller:

```yaml
with:
  trusted_publisher_repository: trycua/cua
  trusted_package_name: "@trycua/core"
  trusted_publisher_workflow: cd-ts-core.yml
```

These values must match the npm Trusted Publishing configuration. The caller
must also grant `id-token: write` to the reusable job. The build creates an
artifact without registry credentials. The identity job checks the artifact's
package name, GitHub repository URL, current repository, and exact caller
workflow before either publisher can run.

The normal path uses npm Trusted Publishing and the protected `npm`
environment, and publishes only to `https://registry.npmjs.org/`. The legacy
token path is disabled by default. It requires an explicit
`allow_legacy_token: true`, the protected `npm-token-fallback` environment,
and its enablement secret. It is still subject to the same identity check,
publishes only to `https://registry.npmjs.org/`, and never receives an OIDC
token.

The legacy PyPI reusable workflow is also disabled by default. Its protected
fallback checks that every downloaded wheel or source archive is a regular
file, then uploads only to `https://upload.pypi.org/legacy/`. Normal PyPI
Trusted Publishing remains in each top-level caller because PyPI binds OIDC
to that caller's workflow file.

The check fails closed when an input is missing, a package or repository does
not match, a run comes from a fork, or the ref is not a protected tag. Do not
configure the canonical `trycua/cua` publisher to run from
`manaflow-ai/cmux-cua`; update the npm trusted-publisher owner and workflow
configuration together before changing these allowlisted values.
