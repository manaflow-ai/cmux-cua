# Release-bump migration

The release path now has two workflows. `release-bump-request.yml` accepts the
service and bump type only from a protected `main` dispatch and writes a small,
credential-free artifact. `release-bump-version.yml` runs from `workflow_run`,
revalidates that artifact and the current `main` commit, waits for the
`release-bump` environment approval, checks the service's fixed tag prefix, and
creates one tag without replacement. The prefix map is in
`.github/scripts/validate_release_bump_request.py`; package configuration cannot
select a new tag family.

Before enabling tag rulesets, audit existing release tags and record each tag's
current commit and published artifact. Historical tags are retained as-is. Do
not delete, move, or recreate them. If an old tag is wrong, publish a new patch
version with a new tag and document the correction in its release notes.

Repository setup must protect the `release-bump` environment with Austin or
Aziz as required reviewers, disable administrator bypass, and permit the
release App only the `contents:write` permission. Add active tag rulesets for
each release family after the audit, with tag creation, update, deletion, and
non-fast-forward changes blocked and no bypass actors.
