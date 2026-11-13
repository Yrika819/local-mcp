# H5-C — Isolated Writer grant test harness

## 1. The constraint that makes this necessary

A Writer `ModelWorkItem` is only issued when an Owner grant exists
(`src/external_writer.rs`, issuance gate). A grant only exists after a live Owner
Control Surface approval with a console nonce. A campaign must not fabricate
that approval — that is the authority boundary the campaign exists to verify.
But without *some* grant the Writer leg cannot be exercised end to end.

The candidate designs from the campaign brief, and what H5 did with each:

| Option | Verdict |
| --- | --- |
| **A.** in-crate `Authorization Test Support` inside a Rust test | Already exists (`authorization_store::test_support::install_grant`, and the `#[cfg(test)]` auto-grant in `external_writer.rs`). It stays, and H5 did not weaken it. It cannot drive an **out-of-process** server, which is the real-model case |
| **B.** a dedicated test host process that injects a grant fixture | **Chosen, in the narrowest form that keeps the invariant.** A `test-grant` command that exists only under the non-default `test-grant-fixture` cargo feature, issues into a **marked disposable state root only**, and stamps `TEST_FIXTURE_V1` |
| **C.** a test-only build configuration | Rejected as the *primary* mechanism: `cfg(test)` does **not** apply to a spawned binary, so a test that launches `local-mcp.exe` inherits none of it. That trap is exactly the one the brief warns about, and it is why the mechanism is (i) a cargo feature that changes which code is compiled into the binary, plus (ii) a runtime check that does not depend on the build at all |
| **D.** the real Owner Control Surface, manually approved | **Not automated, and not automated on purpose.** It remains the correct production acceptance test and is explicitly *not* replaced by A–C |

## 2. The mechanism

### 2.1 Two grant identities, never confused

`OwnerAttestation` gains exactly one variant:

```rust
enum OwnerAttestation { ConsoleLiveNonceV1, TestFixtureV1 }
```

A `TEST_FIXTURE_V1` grant also carries `test_fixture_id` (the fixture root it is
confined to). `GoalWriteGrantV1::validate` enforces the pairing in both
directions: a fixture attestation without an id is corrupt, and an Owner-issued
grant that carries a fixture id is corrupt. The field is
`#[serde(default, skip_serializing_if = "Option::is_none")]`, so grants persisted
before H5 still load, unchanged.

The authorization projection reports `owner_attestation: "TEST_FIXTURE_V1"` for
such a grant, in the ACTIVE branch and in the refused branch, so nothing downstream
(console, API, evidence, journal) can accidentally read a fixture as an Owner
decision.

### 2.2 Compile-time boundary

* `Cargo.toml`: `[features] test-grant-fixture = []`, **not** in `default`.
* `src/test_grant_fixture.rs` and the `test-grant` CLI subcommand are
  `#[cfg(feature = "test-grant-fixture")]`.
* The marker **writer** (`config::mark_test_grant_fixture_root`) is
  `#[cfg(all(feature = "test-grant-fixture", test))]`, so it is not in any
  shipped binary at all.
* `the_default_build_cannot_mint_a_test_grant` asserts the default binary's clap
  command list has no `test-grant`.

So a release/operator build cannot mint one: the code is absent, not disabled.

### 2.3 Runtime boundary (does not depend on the build)

* Issuance refuses unless the state root carries
  `test-grant-fixture.json` — which must be absolute, must name the root it lives
  in (both canonicalized and compared), and must contain a bounded `fixture_id`.
* Issuance refuses a non-EXTERNAL Goal.
* `mark_test_grant_fixture_root` refuses the default state root, a root inside a
  Git working tree, and a workspace that is not a sibling under the same
  disposable parent.
* **The gate that matters:** `AuthorizationLedgerV1::resolve_grant` — the single
  decision used by the issuance, stage, publication and revocation paths —
  refuses a `TEST_FIXTURE_V1` grant whose `test_fixture_id` does not equal the
  marker id of the state root the host is actually running against
  (`WRITE_AUTHORIZATION_STATE_MISMATCH`).

That last point is the answer to "an auto-generated grant must not be
restorable into real operation": copying the ledger *and* the Goal anchor into a
real state root does not help, because there is no marker there. Deleting the
marker from a fixture root revokes the grant's authority immediately, which is
asserted by
`a_fixture_grant_stops_being_authority_when_its_root_stops_being_a_fixture`.

### 2.4 Production code paths stay shared

The fixture reuses, unchanged:

* `authorization_store::begin_request_for_goal` (scope validation against the
  canonical workspace root, TTL bounds, pending-request rules),
* `AuthorizationLedgerV1::build_grant` (nonce, epoch, deterministic grant id,
  expiry, no-active-grant rule),
* `authorization_store::record_anchor_grant` (anchor **before** ledger, so a
  crash leaves an anchor ahead of the ledger),
* `AuthorizationLedgerV1::record_approved_with_attestation` and `save_ledger`.

Publication, postimage verification, the mutation journal, revocation, the Task
Verifier, the Goal Verifier and the Finalizer are the production implementations.
Only the *decision* is a fixture, and the durable record says so.

## 3. What is proven by test

Under `--features test-grant-fixture`:

| Test | Assertion |
| --- | --- |
| `a_fixture_grant_is_issued_and_accepted_inside_a_marked_root` | The production `resolve_grant` accepts the grant *inside* its root; the attestation is `TEST_FIXTURE_V1`; the projection says so and reports `same_uid_process_resistance: false` |
| `a_fixture_grant_cannot_be_issued_into_an_unmarked_root` | Refused, and **nothing was written**: the Goal anchor stays at epoch 0 |
| `a_fixture_grant_stops_being_authority_when_its_root_stops_being_a_fixture` | Removing the marker makes the still-intact grant unusable (`WRITE_AUTHORIZATION_STATE_MISMATCH`), the publication gate refuses it, and the projection reports `INVALID` with the fixture attestation |
| `a_fixture_grant_for_a_codex_goal_is_refused` | A CODEX Goal cannot receive a fixture grant |

Without the feature:

| Test | Assertion |
| --- | --- |
| `the_default_build_cannot_mint_a_test_grant` | No grant-issuing command exists in the default build |

## 4. H5-C gate

| Gate | Result |
| --- | --- |
| Owner approval boundary unchanged | **PASS** — `ConsoleLiveNonceV1` path untouched; `record_approved` still defaults to it |
| No grant issuance added to any MCP tool | **PASS** — no MCP tool was added or changed; `tests/mcp_contract.rs` still asserts no model-facing grant tool exists |
| Release build cannot use it | **PASS** — non-default cargo feature; asserted by test |
| Test-only state root required | **PASS** — marker + sibling-workspace + not-default + not-in-repo rules |
| Cannot be restored into real operation | **PASS** — `resolve_grant` refuses without a matching marker |
| Test identity not confused with production identity | **PASS** — separate attestation, published in the projection and in the durable record |
| Production publication/revocation/journal code shared | **PASS** |
| Isolated Writer E2E can be *run* | **PARTIAL** — the machinery to issue and be authorized exists and is tested in-process, but the out-of-process real-model Writer loop (H5-D Scenario B) was **not executed** in this campaign. The `test-grant` command is available for that leg and is not yet used by any automated scenario |

## 5. Honest limits

* The marker is a file in the state root, so an attacker who already runs as the
  same OS user can forge one. That is **C3 / R-1**, unchanged: this harness does
  not add an OS trust boundary and does not claim to. What it does is make the
  fixture identity unforgeable *as an Owner decision*: a fixture grant still says
  `TEST_FIXTURE_V1` no matter who created the marker.
* The fixture nonce is generated locally and is not an Owner proof; it exists only
  so the production `build_grant` validation is exercised faithfully.
* Enabling `--features test-grant-fixture` in a build that is then used as a
  production server would be a mistake, but it would still not allow a grant in a
  non-fixture root. The feature should never appear in a release build; CI and
  packaging build without it.
