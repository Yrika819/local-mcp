# GoalLatch E5 — External OSS Survey, License Review & Reuse Decision

> Review date: 2026-11-08. Engineering license review, not legal advice.
> Scope: design patterns for durable AI workflows, code editing, review, process execution, permissions and MCP interoperability.
> GoalLatch baseline: Yrika819/local-mcp LICENSE, MIT, copyright 2026 Shotaro Nakamura.
> Checked the official GitHub LICENSE files, not only README badges or third-party license summaries.
> No third-party code was copied into GoalLatch by this design PR. The decisions below describe design references and future optional integrations.

## 1. Decision summary

**Recommended:** Reuse conceptual patterns from permissively licensed projects, but implement E5 on GoalLatch's EXISTING host-authority primitives. Directly importing another orchestration engine or editor is not justified by the codebase or security boundary at PR #41.

| Project / official repository | Official license file inspected | E5 relevant concepts | Recommendation |
|---|---|---|---|
| GoalLatch (Yrika819/local-mcp) | MIT, https://github.com/Yrika819/local-mcp/blob/main/LICENSE | Current TaskStore, Writer, Journal, Verification, Approval | **PREFER EXISTING CODE**; preserve its license |
| OpenHands Core (OpenHands/OpenHands) | MIT, https://github.com/OpenHands/OpenHands/blob/main/LICENSE | Agent runtime/action separation, external agent observations, execution isolation | **CONCEPTUAL REFERENCE ONLY**; avoid importing its agent/cloud runtime |
| Aider (Aider-AI/aider) | Apache-2.0, https://github.com/Aider-AI/aider/blob/main/LICENSE.txt | Structured edits, diffs, feedback from lint/test and Git-aware workflows | **CONCEPTUAL REFERENCE ONLY**; do not bypass GoalLatch scope or publish |
| OpenAI Codex (openai/codex) | Apache-2.0, https://github.com/openai/codex/blob/main/LICENSE | Coding agent tool boundaries, execution approvals, auditability | **CONCEPTUAL REFERENCE ONLY**; GoalLatch EXTERNAL must never spawn Codex |
| LangGraph (langchain-ai/langgraph) | MIT, https://github.com/langchain-ai/langgraph/blob/main/LICENSE | Graph execution, checkpoints, interrupts, resumed state | **CONCEPTUAL REFERENCE ONLY**; TaskStore already supplies durable graph |
| Temporal Rust SDK (temporalio/sdk-rust) | MIT, https://github.com/temporalio/sdk-rust/blob/main/LICENSE.txt | Replay-safe workflows, activity outcomes, deduplication, crash recovery | **CONCEPTUAL REFERENCE ONLY**; requiring Temporal service is a poor fit for a standalone MCP |
| MCP Rust SDK (modelcontextprotocol/rust-sdk) | Mixed transition MIT/Apache-2.0; non-spec docs CC-BY-4.0, https://github.com/modelcontextprotocol/rust-sdk/blob/main/LICENSE | MCP wire compliance and current schema/tool interoperability | **REVIEW SDK VERSION/Licenses FIRST**; use for conformance ideas, not blind source copy |
| OpenHands Cloud (OpenHands/OpenHands-Cloud) | PolyForm Free Trial 1.0.0, https://github.com/OpenHands/OpenHands-Cloud/blob/main/LICENSE | Enterprise/cloud deployment service | **DO NOT IMPORT** into MIT GoalLatch without separate specific legal permission/license analysis |

## 2. Why existing GoalLatch code wins

GoalLatch already has implementation features that a generic agent framework cannot replace without widening the trusted computing base:
- Session-bound TaskStore, Goal/Task revisions, atomic snapshots and durable migration.
- Writer parse/validate, scoped WRITE_UTF8, expected preimage, canonical path revalidation, MutationIntent, operation IDs, per-operation journal, WriteBoundary and workspace_publish.
- Reviewer re-entry after reconciled Writer effects, ReviewGate and deterministic TaskVerifier.
- Replanner host validation of replacement closure, criterion rebindings, active-plan history limits, and anti-amplification rules.
- Managed worktree ownership and verification evidence.
- Working external MCP transport path with GoalLatch Codex spawn count fixed at zero.

Direct reuse of outside Writer or orchestration code risks bypassing existing authority boundaries. E5 should **extract narrower internal seams from the current Rust implementations**, not rewrite them or embed a Python/Node orchestration server.

## 3. Design ideas worth adopting (without source copying)

### LangGraph — workflow checkpoint and resumption

Adopt the **concept** of an explicit durable state at every externally visible wait boundary. A pending WorkItem/approved side effect must be recoverable with no model-dependent hidden state. This is already natural for TaskStore and should be enforced with fault-injection tests.

Do NOT introduce LangGraph runtime, graph-store formats, checkpoints, Python/JavaScript dependencies, or a second workflow state machine.

### Temporal Rust — replay and activity idempotency

Adopt the **concept** that a completed activity result is durable evidence, while an activity whose effect is uncertain must not be replayed without reconciliation. Link Writer operation identity and per-operation journal to immutable content digests and the postimage.

Do NOT launch a Temporal server, require its workers, or assume its MIT license alone resolves the operational complexity.

### Aider — surgical editing plus compiler feedback

Adopt the **concept** of bounded, exact file edits and independent lint/test feedback. In E5, preserve GoalLatch's WRITE_UTF8 + expected-preimage contract, with WorkspacePublish and ReviewGate.

Do NOT import a patch application algorithm or auto-commit behavior until its edge cases, security authority and all licensing obligations have been separately reviewed.

### OpenHands Core — model/tool boundary

Adopt the **concept** of separating a model's suggested action from trusted execution. GoalLatch ModelWorkItem/Receipt already serves that purpose. Inspect OpenHands Core only; license is MIT there.

Do NOT infer that all repositories under the OpenHands organization or hosted cloud assets use MIT.

### Codex — approval and tool-call observability

Adopt the **concept** of separate model proposal, host approval, execution receipt and verification. EXTERNAL's goal remains to use DeepAI Code inference without spawning Codex.

Do NOT link GoalLatch EXTERNAL to Codex as a fallback or import Codex internals without review.

### MCP Rust SDK — protocol compatibility

Use the official protocol schema as a conformance reference when changing goal_next/goal_submit. Do NOT make automatic source imports from this repository: its official LICENSE explicitly describes an ongoing MIT → Apache-2.0 relicensing transition, with documentation CC-BY-4.0, so file-level provenance and the specific version need verification.

## 4. Conditions for copying code or adding third-party dependencies later

Before incorporation:
1. Identify precise upstream repository, commit SHA/tag, file paths, and full license/NOTICE provenance.
2. Verify each copied file's own header and adjacent licensing conditions; root LICENSE may not cover every subdirectory, enterprise component, asset or generated artifact.
3. For MIT components: retain applicable copyright and permission notice in copies or substantial portions, as required.
4. For Apache-2.0 components: include applicable license text, notices if a NOTICE file exists, retain relevant attributions and document modifications. Review patent/trademark clauses and transitive licenses where relevant.
5. For CC-BY-4.0 documentation: provide proper attribution and license/change indication when adapting, or write original documentation instead.
6. For mixed/restricted licensing (MCP SDK transition, OpenHands Cloud): do not silently copy. Seek separate review or omit.
7. Record dependency versions, Cargo.lock changes, vulnerabilities and MSRV/build impact; verify Windows, Linux and macOS distribution.
8. Add a THIRD_PARTY_NOTICES or equivalent inventory when actually distributing reused portions; no new third-party notice claims are needed for this design-only PR.
9. Run tests proving the introduced code cannot access workspace mutation or host approval outside GoalLatch's existing entry points.
10. Prefer original independently written Rust implementations of simple design patterns; copyright licenses govern copying protected expression, not ideas alone.

## 5. Concrete E5 reuse choice

- E5-0 DAG: **GoalLatch scheduler/Task::transition_to** (not LangGraph runtime).
- E5-A Writer: **GoalLatch writer.rs + workspace_publish.rs + MutationIntent** (not Aider editor/auto-commit).
- E5-B Reviewer: **GoalLatch resume_writer_reviewer + existing ReviewGate** (not a new external review engine).
- E5-C Replanner: **GoalLatch replanner.rs + TaskStore** (not Temporal runtime).
- E5-D Managed: **GoalLatch managed_worktree* + existing verification evidence**.
- E5-E Dogfooding: **GoalLatch MCP + DeepAI Code client**, only sanctioned client tools and no additional Codex processes.

**Result: no new runtime dependency required for the E5 initial implementation.** Further library adoption is conditional on measurable need and a separate reviewed PR.

## 6. Source of truth links

- GoalLatch license: https://github.com/Yrika819/local-mcp/blob/main/LICENSE
- OpenHands Core license: https://github.com/OpenHands/OpenHands/blob/main/LICENSE
- OpenHands Cloud license (restricted): https://github.com/OpenHands/OpenHands-Cloud/blob/main/LICENSE
- Aider license: https://github.com/Aider-AI/aider/blob/main/LICENSE.txt
- Codex license: https://github.com/openai/codex/blob/main/LICENSE
- LangGraph license: https://github.com/langchain-ai/langgraph/blob/main/LICENSE
- Temporal Rust SDK license: https://github.com/temporalio/sdk-rust/blob/main/LICENSE.txt
- MCP Rust SDK mixed-transition license: https://github.com/modelcontextprotocol/rust-sdk/blob/main/LICENSE

License classifications reflect the inspected upstream main license files at the review date; verify them again at an exact upstream commit before any future source reuse.
