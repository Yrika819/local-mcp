# H4-E — PR stack integration readiness

Measured 2026-11-09 against live GitHub state (all values re-read, not carried
over from a handoff).

Method (non-destructive, nothing merged, no branch or worktree mutated):

* head/base SHAs and draft state: `gh pr view <n> --json …`;
* required CI: `statusCheckRollup` per PR head;
* stack integrity: each PR's base commit compared with the previous PR's head;
* integration result: `git merge-tree --write-tree --name-only origin/main <head>`
  for every PR head. This performs a real three-way merge in the object database
  without touching the index or any worktree, so it is a genuine merge check and
  not a diff heuristic.

## 1. Stack shape

The external backend stack is **linear**: every PR's base commit equals the
previous PR's head commit.

```
#35 8084dc0b4 -> #36 0a4cdb5c4 -> #37 86fa92a16 -> #38 5df8a29a1 -> #39 afbce7201
 -> #40 e01886094 -> #41 4f2264249 -> #43 b7c9d37ae -> #44 12afe6e04 -> #45 36a45fb4f
 -> #46 844dab2f0 -> #47 08f3edf9e -> #48 3320dcd93 -> #49 327b461be -> #50 072fa1564
 -> #51 d065971    (H4, this campaign)
#42 bca1230ff is a parallel docs PR based directly on main e6950fdc3.
```

**One structural fact that matters more than any CI result:** PR #35's base is
`9b8f6c11f` (`integration/external-agent-v1-e3-base`), which is *not* an ancestor
of `main`. It is `main` plus 12 merge commits that pull in PRs **#31, #32, #33,
#34** — three of which are still open drafts. So merging #35 as-is would drag
unmerged, unreviewed work onto `main`, and the stack cannot be landed by merging
#35 first.

`#42` sits on `main` and is independent of the stack.

## 2. Per-PR readiness

Required CI = the 22 checks the repository runs (quality, test ×4 OS, compat ×7).

| PR | HEAD | Files / lines | Merge-tree vs main | CI | Independent review | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| #35 E3-A ModelWorkItem | `8084dc0b4` | 7 / +2544 | clean (`5ab798da3`) | 22/22 | none recorded | **NEEDS_REVIEW** (also base is not main) |
| #36 E3-B goal_next/submit | `0a4cdb5c4` | 11 / +2270 −17 | clean | 22/22 | none | **NEEDS_REVIEW** |
| #37 E3-C recovery/compat | `86fa92a16` | 2 / +778 | clean | 22/22 | none | **NEEDS_REVIEW** |
| #38 E3 hardening (H1) | `5df8a29a1` | 9 / +1093 −124 | clean | 22/22 | none | **NEEDS_REVIEW** |
| #39 E4-A Planner | `afbce7201` | 6 / +347 −79 | clean | 22/22 | none | **NEEDS_REVIEW** |
| #40 E4-B Readonly | `e01886094` | 6 / +1276 −128 | clean | 22/22 | none | **NEEDS_REVIEW** |
| #41 E4-C dogfooding docs | `4f2264249` | 4 / +687 | clean | 22/22 | none | **MERGE_READY** (docs only) |
| #42 E5 design + 98-case plan | `bca1230ff` | 3 / +484 | clean (`52db71a8e`) | 22/22 | none | **MERGE_READY** (docs only, on main) |
| #43 E5-0 DAG/Writer/Reviewer | `b7c9d37ae` | 20 / +6034 −79 | clean | 22/22 | none | **NEEDS_REVIEW** (largest slice in the stack) |
| #44 E5-H2 writer stage race | `12afe6e04` | 7 / +1747 −34 | clean | 22/22 | none | **NEEDS_REVIEW** |
| #45 E5-B2 reviewer recovery | `36a45fb4f` | 4 / +514 −3 | clean | 22/22 | none | **NEEDS_REVIEW** |
| #46 E5-C Replanner | `844dab2f0` | 9 / +1588 −8 | clean | 22/22 | none | **NEEDS_REVIEW** |
| #47 E5-H3 reliability | `08f3edf9e` | 6 / +1977 −4 | clean | 22/22 | none | **NEEDS_REVIEW** |
| #48 Auth-B design | `3320dcd93` | 1 / +530 | clean | 22/22 | none | **MERGE_READY** (docs only) |
| #49 Auth-B durable grant + gate | `327b461be` | 23 / +5750 −620 | clean | 22/22 | rounds recorded in commit history | **NEEDS_REVIEW** |
| #50 E5-D managed worktree + finalizer | `072fa1564` | 12 / +3611 −1492 | clean | 22/22 | review rounds recorded in commit history (`security: remediate review round 8 (R8-M1)`) | **NEEDS_REVIEW** — H4-B independent review below |
| #51 H4-A/B/C (this PR) | `d065971` | see PR | clean | pending | independent review of the `072fa15` base done; the H4 diff itself is **unreviewed** | **NOT_VERIFIED** until CI + a review of this exact head |

Legend: `MERGE_READY` = docs-only, clean, CI green, and nothing in it can change
runtime behaviour. Every PR that changes runtime code is `NEEDS_REVIEW` because
no PR in this stack carries a recorded independent review of its *final* head
except #49/#50 (and those rounds predate the H4-B findings).

## 3. Why "the top of the stack is green" is not evidence for the bottom

Every PR shows 22/22 because each PR's CI only ever builds **its own head**. A
green #50 therefore proves that #35–#50 *together at that head* pass, not that
#35 alone is safe to merge, and not that the intermediate heads are individually
sound. Two consequences for the Owner:

1. The stack must be landed in order, and each merge re-triggers CI on the new
   base. A green stack head does not pre-approve the intermediate steps.
2. Any *squash*-merge decision changes the SHAs and invalidates the "base equals
   previous head" property, so the recommended landing order below assumes
   merge commits or a single squashed landing of a re-created branch.

## 4. Merge-tree results

All 15 runtime/docs PR heads merge cleanly into `origin/main` as a three-way merge
today (see the table). That is a statement about textual/structural conflict only;
it says nothing about semantic interaction. The interaction that matters —
`authorization_store` being rewritten by #49 and touched again by #51 — is
covered by #51's tests running against #50's code, not by merge-tree.

## 5. Recommended order and remaining work

1. **#31, #32, #33, #34** — decide these first: they are the base of #35. Either
   merge them to `main` (then #35 can be re-based onto `main` and its base
   becomes honest) or re-target #35 to `main` with `9b8f6c1` content folded in.
   Until this is decided, the stack cannot land at all.
2. **#42** (docs, on main) and **#41**, **#48** (docs) — safe early.
3. #35 → #37 (foundation + MCP contract + recovery tests).
4. #38 (hardening), #39, #40 (planner/readonly), #43 (DAG + writer + reviewer).
5. #44, #45, #46, #47 (race/recovery/replanner/H3).
6. #49 (authorization), then #50, then **#51** — the authorization PRs must land
   as a unit with #51, because #51 fixes two defects the H4-B review found in
   #50's authorization code. Landing #49/#50 without #51 would ship the MEDIUM
   budget-floor defect.
7. Then #51's own CI + an independent review of `2578eac`/final head.

Remaining verifications the Owner should require before landing #49–#51:

* an independent review of the **#51 head** (the H4-A schema change, the
  publication deadline, and the two authorization fixes are unreviewed by anyone
  but their author);
* a real Writer dogfooding run (**not done** — no non-interactive grant path
  exists);
* the C3 decision (see `docs/security/C3_0_WINDOWS_TRUST_BOUNDARY.md`), because
  #49–#51 ship a real write path on Windows that is only as strong as the
  same-user boundary allows.

## 6. Unresolved comments

`gh pr view … --json comments` reports 0 comments on #35–#50 except **#43 (1)**.
No PR in the stack has a blocking review thread.
