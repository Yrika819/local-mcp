# Release operator checklist (reusable)

This is the reusable procedure for publishing **any** future release. It is
written with placeholders; substitute real values for the version you are
actually releasing. `v0.1.0` was published from this procedure and is finished
— that release is not repeated or re-litigated here.

Read the whole list before starting. The steps are ordered, and the ones that
cannot be undone (the tag, and the tag push) are the reason the ordering
matters.

## Placeholders to substitute up front

| Placeholder | Meaning |
| --- | --- |
| `VERSION` | the bare version, exactly as it appears in `Cargo.toml` |
| `vX.Y.Z` | the release tag, i.e. `v` + `VERSION` |
| `<release-commit>` | the exact `main` SHA you intend to release |

`VERSION` is decided before anything else and is not changed mid-release. If you
find yourself editing it during the procedure, you are no longer doing this
checklist.

## Preconditions

- [ ] `main` is at `<release-commit>`, and CI is green on exactly that SHA.
- [ ] `Cargo.toml` `version` is exactly `VERSION`. Not still the previous
      version, not already bumped twice.
- [ ] `CHANGELOG.md` covers `VERSION`.
- [ ] `docs/release/RELEASE_NOTES_vX.Y.Z.md` exists and is accurate as of
      `<release-commit>`, including the Windows experimental status, the Linux
      aarch64 sandbox closure caveat, and the known dependency advisories.
- [ ] Every link in that notes file is an absolute, tag-pinned link.
      `scripts/release/test_workflow_policy.py` enforces this.
- [ ] `vX.Y.Z` does not already exist on the remote (step 5 checks this again).
- [ ] A final dry run has passed on exactly `<release-commit>` (step 4).

## 1. Verify main authority first

Authority comes before every other concern. Do not start a release against a
SHA you have not confirmed is `main`.

```sh
git fetch origin --tags
git rev-parse origin/main
gh repo view Yrika819/local-mcp --json visibility,defaultBranchRef
```

Expect `PUBLIC` and `main`. Record the SHA as `<release-commit>`. If it differs
from what you expected, stop and reconcile before continuing. A release must
never be cut from a branch that has not landed on `main`.

## 2. Verify CI is green on that exact SHA

```sh
gh run list --repo Yrika819/local-mcp --branch main --limit 5 \
  --json databaseId,headSha,status,conclusion
```

Every job on `<release-commit>` must be `success`. Do not accept a run on a
different SHA, and do not accept "CI is green on main" as a summary of a run
that finished before the last commit landed.

## 3. Verify the exact Cargo version

```sh
git show <release-commit>:Cargo.toml | sed -n '/^\[package\]/,/^\[/p'
```

The package `version` must equal `VERSION` character for character. This is the
same value `release.yml` re-checks at plan time, and a mismatch there fails the
run. Confirming it here means you find out before creating a tag, not after.

## 4. Final `workflow_dispatch` dry run on the release commit

This is the last rehearsal, and it must run on `<release-commit>` itself — not
on a branch, and not on a near-identical SHA.

```sh
gh workflow run release.yml --repo Yrika819/local-mcp \
  --ref main --field version=VERSION --field reason="final pre-release dry run"
```

There is deliberately no default for the `version` input: you must type the
version you mean. `release.yml` accepts only two triggers, a `v*` tag push and
an explicit `workflow_dispatch`; anything else fails the run closed rather than
inferring a version.

Then confirm the run behaved as a dry run:

```sh
gh run list --repo Yrika819/local-mcp --workflow release.yml --limit 3 \
  --json databaseId,headBranch,headSha,status,conclusion,event
```

Require `event` = `workflow_dispatch`, `headSha` = `<release-commit>`, and the
`publish` job **skipped**. If `publish` executed, stop: that is a
release-blocking regression in the guard, not a release to proceed with.

## 5. Create the annotated version tag

This is the first irreversible step.

```sh
git checkout main
git pull --ff-only
git tag -a vX.Y.Z -m "Local MCP vX.Y.Z"
git push origin vX.Y.Z
```

The tag must be **annotated** (`git tag -a`), not lightweight, so the release
commit is recorded in the tag object itself.

Push the tag exactly once. **Treat a published tag as immutable:** do not
force-move it, do not re-point it, and do not delete it to fix a mistake. If the
tag is wrong, that is a new version number, not a moved tag — a moved tag makes
every already-fetched reference ambiguous and breaks the correspondence between
the published assets and the commit they were built from. Stop and escalate
instead.

## 6. Confirm the release workflow started on the tag

```sh
gh run list --repo Yrika819/local-mcp --workflow release.yml --limit 3 \
  --json databaseId,headBranch,headSha,status,event
```

The run's `event` must be `push` and `headBranch` must be `vX.Y.Z`. The `plan`
job should report `mode=RELEASE`; if it says `DRY RUN`, stop, because that means
the guard is wrong and the publish job will skip.

## 7. Wait for the build and checksum jobs

All six platform jobs plus `checksums` must be green. The `checksums` job
re-verifies every downloaded archive and re-runs `sha256sum -c`, so a green
result means the artifacts survived upload intact.

## 8. Verify the published assets

```sh
gh release view vX.Y.Z --repo Yrika819/local-mcp --json assets \
  --jq '.assets[] | "\(.name) \(.size)"'
```

Expect exactly six platform archives plus `SHA256SUMS`, named per the convention
in `scripts/release/targets.py`. Each Linux archive must contain both
`local-mcp` and `codex-linux-sandbox`. A missing or extra asset means the
release is wrong; do not paper over it by hand-editing the release.

## 9. Verify the actual downloads, not only the Actions artifacts

Actions artifacts and Release assets are different storage with different
failure modes. A green `checksums` job proves the run produced good archives; it
does **not** prove they were attached to the Release correctly. Download from
the public Release, from a clean session and ideally a different network:

```sh
gh release download vX.Y.Z --repo Yrika819/local-mcp --dir /tmp/verify-release
cd /tmp/verify-release && sha256sum -c SHA256SUMS
```

- [ ] The release page loads at the public URL and is not marked as a draft or
      a pre-release by mistake.
- [ ] The notes render, including the Windows experimental warning and the
      known-advisory section.
- [ ] `sha256sum -c SHA256SUMS` passes against the downloaded files.
- [ ] On Linux, extracting an archive yields both binaries and
      `local-mcp --version` runs.
- [ ] `RELEASE_NOTES.md` is present in the bundle and is not uploaded as a
      downloadable asset.

## If something goes wrong

- **A build job failed before the tag was pushed:** fix on a branch, push, let
  the dry run go green, then start again from step 1.
- **A build job failed after the tag was pushed:** the tag exists and must not
  be moved. Fix on `main`, re-run CI, and release the corrected code as a new
  version. Do not re-point the existing tag.
- **The publish job failed:** the tag exists but no release does. Fix the cause
  and re-run the workflow for the same tag; the guard still allows it because
  the ref is a real tag.
- **A release exists but is wrong:** correct the notes in place where that is
  honest, or delete the release and re-run. Deleting a release is recoverable;
  moving a published tag is not.

## Hard rules for this repository

- Never publish to a registry (crates.io, npm, Homebrew) as part of a release.
  Releases ship GitHub Release assets only. Registry publication requires
  separate, explicit authorization.
- Never remove the Windows experimental status without the separate runtime and
  sandbox closure that `SECURITY.md` describes.
- Never present Linux aarch64 sandboxing as release-closure validated until it
  has been validated on that architecture.
- Never present the inherited dependency advisories as resolved. Disclose them
  honestly in the release notes, every time.
- Never drop the `nakasyou/local-mcp` upstream attribution in `LICENSE` or
  `README.md`.
- Never mark a release as "secure" or "fully sandboxed" in a way the evidence
  does not support.
- Never force-push, rewrite history, or move a published tag to land a release.
