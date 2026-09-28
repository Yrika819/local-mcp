# Release operator checklist (Public v1)

This is the procedure for the **next** task, the one that actually publishes
`v0.1.0`. Nothing here has been executed. The dry run on
`release/public-v1-dry-run` was explicitly barred from creating a tag or a
release, so every step below is still pending.

Read the whole list before starting. The steps are ordered, and the two that
cannot be undone (5 and 9) are the reason the ordering matters.

## Preconditions

- [ ] `main` is at the commit you intend to release, and CI is green on it.
- [ ] The release branch has been reviewed and merged (or fast-forwarded) into
      `main`. This was not done during the dry run.
- [ ] `Cargo.toml` `version` matches the tag you are about to create.
- [ ] `CHANGELOG.md` covers the version being released.
- [ ] `docs/release/RELEASE_NOTES_v0.1.0.md` is accurate as of the release
      commit, including the Windows experimental status and the known
      dependency advisories.
- [ ] A final dry run on the exact release commit has passed. The one produced
      during preparation is on the branch, not on `main`.

## 1. Verify main authority

```sh
git fetch origin --tags
git rev-parse origin/main
gh repo view Yrika819/local-mcp --json visibility,defaultBranchRef
```

Expect `PUBLIC` and `main`. Record the SHA. If it differs from what you
expected, stop and reconcile before continuing.

## 2. Verify main CI is green on that exact SHA

```sh
gh run list --repo Yrika819/local-mcp --branch main --limit 5 \
  --json databaseId,headSha,status,conclusion
```

Every job on the release SHA must be `success`. Do not accept a run on a
different SHA.

## 3. Merge or fast-forward the release preparation

Only if this task is explicitly authorized to touch `main`:

```sh
git checkout main
git merge --ff-only origin/release/public-v1-dry-run
git push origin main
```

Never force-push and never rewrite history to land a release.

## 4. Re-run main CI after the merge

The merge produces a fresh `main` SHA, and that SHA is what gets tagged. Wait
for CI to finish and confirm every job is green. Do not tag a SHA whose CI is
still running or has failed.

## 5. Create the annotated version tag

This is the first irreversible step.

```sh
git checkout main
git pull --ff-only
git tag -a v0.1.0 -m "Local MCP v0.1.0 (Public v1)"
git push origin v0.1.0
```

Push the tag exactly once. If a tag is pushed with the wrong commit, delete it
and recreate it; do not move it in place, because a moved tag makes any
already-fetched reference ambiguous.

## 6. Confirm the release workflow started on the tag

```sh
gh run list --repo Yrika819/local-mcp --workflow release.yml --limit 3 \
  --json databaseId,headBranch,headSha,status,event
```

The run's `event` must be `push` and `headBranch` must be `v0.1.0`. The
`plan` job should report `mode=RELEASE`; if it says `DRY RUN`, stop, because
that means the guard is wrong and the publish job will skip.

## 7. Wait for the build and checksum jobs

All six platform jobs plus `checksums` must be green. The `checksums` job
re-verifies every downloaded archive and re-runs `sha256sum -c`, so a green
result means the artifacts survived upload intact.

## 8. Verify the uploaded assets

```sh
gh release view v0.1.0 --repo Yrika819/local-mcp --json assets
```

Expect six archives plus `SHA256SUMS`, named per the convention in
`scripts/release/targets.py`. Each Linux archive must contain both
`local-mcp` and `codex-linux-sandbox`.

## 9. Publish and verify the release page

The `publish` job creates the release automatically on the tag push. Confirm it
exists and is not a draft:

```sh
gh release view v0.1.0 --repo Yrika819/local-mcp --json isDraft,tagName,url
gh release view v0.1.0 --repo Yrika819/local-mcp --json assets \
  --jq '.assets[] | "\(.name) \(.size)"'
```

Then verify it externally, from a clean session and ideally a different
network:

- [ ] The release page loads at the public URL and is not marked as a draft or
      a pre-release by mistake.
- [ ] The notes render, including the Windows experimental warning and the
      known-advisory section.
- [ ] Downloading an archive and running `sha256sum -c SHA256SUMS` passes.
- [ ] On Linux, extracting the archive yields both binaries and
      `local-mcp --version` runs.

## If something goes wrong

- **A build job failed before the tag was pushed:** fix on the release branch,
  push, let the dry run go green, then start again from step 1.
- **A build job failed after the tag was pushed:** fix on `main`, delete the
  tag, re-run CI, and re-tag once green. Do not re-point the existing tag.
- **The publish job failed:** the tag exists but no release does. Fix the cause
  and re-run the workflow for the same tag; the guard still allows it because
  the ref is a real tag.
- **A release exists but is wrong:** edit the notes in place, or delete the
  release and re-run. Deleting a release is recoverable; moving a published tag
  is not.

## Hard rules for this repository

- Never publish to a registry (crates.io, npm, Homebrew) as part of the v0.1.0
  release. This release ships GitHub release assets only.
- Never remove the Windows experimental status without the separate runtime
  and sandbox closure that `SECURITY.md` describes.
- Never present the inherited dependency advisories as resolved.
- Never drop the `nakasyou/local-mcp` upstream attribution in `LICENSE` or
  `README.md`.
- Never mark a release as "secure" or "fully sandboxed" in a way the evidence
  does not support.
