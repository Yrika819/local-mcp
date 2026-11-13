# H5.3 campaign checkpoint — External Backend V1 closure

Campaign: **GoalLatch H5.3 — External Backend Final Validation**
Branch: `feat/h5-3-external-backend-closure-v1`
Base: PR #56 HEAD `0dcab4a109b73d88b7f74871bde70d184e8f147d`
Worktree: `.local/h53`
Base repository `main` at H5.3 start: `e6950fdc3bab72982285888a81cce822743fbba5`

Re-derive every row from the working tree, the run artifacts under
`%TEMP%\goallatch-h53\`, and the H5.3 PR CI before trusting it.

---

## 1. Baseline verification (H5.3-0 start)

| Item | Brief | Observed at start | Match |
| --- | --- | --- | --- |
| `main` HEAD | `e6950fd` | `e6950fdc3bab72982285888a81cce822743fbba5` | yes |
| PR #55 HEAD | `31540b7` | `31540b7fe3fc82cdcc4333678399dd1d4ad642c9` | yes |
| PR #56 HEAD | `0dcab4a` | `0dcab4a109b73d88b7f74871bde70d184e8f147d` | yes |
| PR #56 state | OPEN / Draft / MERGEABLE | OPEN / Draft / MERGEABLE | yes |
| PR #56 CI | 22/22 SUCCESS | 22 checks, `Counter({'SUCCESS': 22})` | yes |
| Free space on `C:` | — | 20 GB at start (91% used) | recorded |

No PR, branch or tag was created, modified, merged or un-drafted.
`.local/h52` and every other pre-existing worktree were left untouched; H5.3
works only in `.local/h53`.

## 2. Phase status

| Phase | Status | Evidence |
| --- | --- | --- |
| H5.3-0 driver safety | **DONE** | `docs/campaign/H5_3_0_DRIVER_SAFETY.md`; 30 Python safety tests; CI step added |
| H5.3-A Replanner contract | **DONE** | published condition + notes example, regenerated artifacts, drift test, 2 new replanner tests |
| H5.3-B C1 host path (real model) | **PASS** | `docs/campaign/H5_3_EVIDENCE.md` §2 |
| H5.3-B C2 real-task replan to completion | **PASS** | §3 — `COMPLETED`, 6 model calls |
| H5.3-C D1 host kill / Job Object | **PASS** | §4 |
| H5.3-C D2 surviving lock holder | **PASS** | §5 |
| H5.3-D R2-11 / F5 | **PARTIAL** | §4 below |
| H5.3-E independent review | **DONE** | 5 rounds, `gpt-6.1-terra` high, 0 Critical / 0 High; H5_3_EVIDENCE.md §8 |
| H5.3-F CI / integration readiness | see §6 and the PR #57 checks | |
| **H5.3.1 final review closure** | **DONE** | `docs/campaign/H5_3_1_FINAL_CLOSURE.md`; the Windows half of review finding R1b is fixed and measured; round 6 recorded in `H5_3_EVIDENCE.md` §10 |

### H5.3.1 (integration campaign phase I)

| Item | Result |
| --- | --- |
| Reviewer `NOT_READY` at `f74b805` | Answered, not overridden: R1b was still open **on Windows** because `_assert_private_fixture_root` returned early there and `Path.mkdir()` inherited the temporary directory's DACL |
| Measurement | `%TEMP%` on the campaign host grants `Modify` to two further local SIDs and to `CodexSandboxUsers`; a plain directory created there carries four non-owner write ACEs |
| Fix | `scripts/h5-d/private_root.py`: root created **with** an owner-only protected descriptor, DACL read back and verified, reused roots verified, owner identity checked, everything unprovable refused |
| Tests | `test_fixture_safety.py` 30 tests OK, with positive and negative controls |
| Rust sources | unchanged in H5.3.1 (regression check of the fused HEAD only) |

## 3. Defects found by real execution and closed in H5.3

| Id | Defect | Severity | Disposition |
| --- | --- | --- | --- |
| H53-1 | Real-model driver deleted an arbitrary `--work-dir` with `shutil.rmtree` | Critical (data loss) | Fixed: create-only fixture, ownership marker, reparse refusal, per-attempt evidence |
| H53-2 | `PENDING -> READY` refusal surfaced to the external client as `GOAL_PERSISTENCE_ERROR` | Medium (misdiagnosis) | Fixed: `GOAL_TASK_TRANSITION_REFUSED` |
| H53-3 | A `WRITE_UTF8` with `expected_preimage: ABSENT` could never publish: the scope check canonicalized a not-yet-existing path | High (published capability unreachable) | Fixed: resolve through the parent, with tests |
| H53-4 | `codex exec --sandbox read-only` makes the workspace unreadable to the model on Windows | High (evidence validity, not product safety) | Recorded as a deployment constraint; harness gained an explicit `--readonly-sandbox` with a per-call no-write proof |
| H53-5 | The Replanner contract did not state the `resolve_needs_replan` -> `add_dependencies` requirement that the validator enforces | High (contract/validator drift) | Fixed in H5.3-A |
| H53-6 | `codex` subprocess failures, oversized answers and greedy JSON extraction were unclassified | Medium (harness reliability) | Fixed: classified `ModelInvocationError` taxonomy |

## 4. H5.3-D residual quality

* **F5 (non-WorkItem schema drift)** — new test
  `every_published_tool_schema_and_the_runtime_both_refuse_an_unknown_argument`
  asserts, for every published tool, `additionalProperties: false` plus schema
  rejection of an undeclared field, and asserts the runtime refuses the same
  field on `goal_status` / `goal_next` / `goal_resume` / `goal_result` with
  `INVALID_ARGUMENT`.
* **R2-11 (boundary probes)** — not extended in H5.3 beyond what H5.2 covered
  (read-only and writer byte bounds at the limit and one over, integer `maximum`
  publication). Grant TTL, quarantine schema and durable-JSON corruption
  boundaries keep their existing coverage. Recorded as a residual coverage gap,
  not a defect.

## 5. Next work

1. Finish the local suite and the new `mcp_contract` test.
2. Independent read-only review of the frozen HEAD by `gpt-6.1-terra`
   (reasoning `high`), recording model id, reasoning, HEAD, timestamp, scope and
   findings by severity.
3. Push the branch and open a **new stacked Draft PR** based on
   `feat/h5-2-real-model-validation-v1`; confirm Required CI.
4. Update `docs/campaign/H5_E_INTEGRATION_DECISION_PACKET.md` with the current
   PR #30…#56 dependency picture and the unresolved C3 constraint.

## 6. Not claimed

* C3 / R-1 (same-OS-user impersonation) is **unresolved**. Nothing in H5.3 makes
  an unrestricted EXTERNAL Writer safe to run against production data on a
  normal Windows account.
* POSIX detached descendants are `NOT_VERIFIED`; no Linux/macOS real-process
  crash run was performed.
* No `main` merge, Draft clearance, Release, Tag, base/head change or force push.
* The `test-grant` command remains compiled only under the non-default
  `test-grant-fixture` feature.
