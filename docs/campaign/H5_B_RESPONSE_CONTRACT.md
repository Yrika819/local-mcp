# H5-B — ModelWorkItem Response Contract V2

Closes **H4-D-F1**.

## 1. The defect

`ModelWorkView.response_required_fields` published field *names* only. A real
model answered a READONLY WorkItem with `"status": "COMPLETED"`; the host refused
it (correctly) because the read-only status vocabulary is
`candidate_complete | blocked | needs_replan | failed`. Nothing the client could
read said so: the only way to learn the contract was to be rejected.

## 2. Design decision: where the contract lives

| Option | Retrievable by an MCP-only client | Compatible with persisted WorkItems | Reviewable in review | Chosen |
| --- | --- | --- | --- | --- |
| (a) static `docs/schemas/external/*.json` only | **No** — a model has no host filesystem, and no MCP tool serves the file | Yes | Yes | **kept, in addition** |
| (b) full schema inside the persisted `ModelWorkItem.request` | Yes | **No** — `request` is part of `request_digest`, so every already-issued WorkItem would change identity | Yes | rejected |
| (c) schema id / digest only on the WorkItem | **No** — an id the client cannot dereference is not a contract | Yes | No | rejected |
| (d) additive projection field on `ModelWorkView` carrying the whole schema + digest + conditions | **Yes** | **Yes** — nothing persisted changes | Yes | **chosen** |

So the response now carries the complete contract, *and* the same bytes are
checked into Git, *and* a test asserts the two are byte-identical.

Compatibility: `output_contract` (persisted) is still the authoritative binding.
A WorkItem written before this change reloads and maps to the same contract by
that string alone; the new projection is computed per call and never persisted,
so no identity, digest or receipt changes meaning.

## 3. Shape published on `goal_next`

```json
"response_contract": {
  "contract_id": "goallatch.external.response.v2/READONLY/readonly-investigation-v1",
  "contract_version": 2,
  "role": "READONLY",
  "output_contract": "readonly-investigation-v1",
  "schema_digest": "sha256:aebdd9f7…",
  "byte_semantics": "UTF8_BYTES",
  "max_response_bytes": 262144,
  "identity_echo_fields": ["goal_id","task_id","attempt_id","goal_revision","plan_revision"],
  "required_fields": [...],
  "optional_fields": [...],
  "conditions": [{"code": "READONLY_STATUS_VOCABULARY", "detail": "…"}],
  "submission_decisions": ["ACCEPTED_FOR_HOST_PROCESSING","COMMITTED","REJECTED_SCHEMA", …],
  "notes": [...],
  "schema": { "type": "object", "additionalProperties": false, "properties": { … }, "required": [ … ] }
}
```

`contract_id` and `schema_digest` are computed per role; the digest is SHA-256
over the canonical (key-sorted, `serde_json::BTreeMap`) serialization of the
schema, so two servers that publish the same contract publish the same digest on
every platform.

## 4. Single source of truth, and how it is proven

The published contract and the host validators are **one description rendered
twice**, not two descriptions a test hopes still agree:

* Every byte bound that the contract publishes is a `LIMIT_*` constant in
  `src/external_response_contract.rs`, and `src/planner.rs`, `src/writer.rs` and
  `src/readonly_worker.rs` now *import* those constants instead of keeping
  private copies (`MAX_PLAN_SUMMARY_BYTES`, `MAX_WRITER_OPERATIONS`,
  `MAX_WORKER_EVIDENCE_*`, `MAX_READONLY_*`, … 30 aliases in total). Changing a
  validator bound without changing the contract is not possible without deleting
  the import.
* The **enumerations** are asserted equal to the serde rendering of the real Rust
  enums and to `worker_capability::is_production_plannable`
  (`published_worker_vocabulary_is_exactly_the_plannable_set`,
  `published_verification_vocabulary_matches_the_real_enum`,
  `published_scope_vocabularies_are_complete`).

Two directions are tested, and the asymmetry is deliberate and documented:

1. **Safety direction** — *every instance the schema accepts must be accepted by
   the real shape validator.*
   `the_schema_is_never_more_permissive_than_the_host_shape_validator` generates
   mutations of each role's valid sample (drop each required field, add an unknown
   field, retype each field, corrupt a nested discriminator) and asserts that any
   mutation the published schema accepts is also deserialized by the *real*
   `serde` type with its real `deny_unknown_fields`/enum attributes.
2. **Completeness direction** — *every published enum value is accepted, and an
   unpublished one is refused by both.* Same test class, plus per-role value
   loops.
3. **Semantic bounds** — the byte limits are checked through the **real parsers**
   at the boundary (at the limit and one byte over): read-only summary
   (`parse_and_validate`), read-only evidence count and field size and the
   cross-item total, writer summary, writer operation count. A semantically-only
   rule (total evidence bytes, `candidate_complete` requiring 1..=32 operations,
   identity echo mismatch, local-mutation scope pairing) is published as a
   `conditions[]` entry rather than as a JSON Schema keyword, because the host
   refuses it for a reason a per-field keyword cannot express.

The schema is allowed to be *stricter* than the shape validator in exactly one
place: the Planner's `worker` enum publishes only the production-plannable
subset. The test asserts that difference explicitly, so a future worker kind that
becomes plannable cannot silently stay unpublished by accident without failing
`published_worker_vocabulary_is_exactly_the_plannable_set`.

## 5. Artifacts

| File | Role | Digest |
| --- | --- | --- |
| `docs/schemas/external/planner.v2.json` | PLANNER | `sha256:e9ba33f7…` |
| `docs/schemas/external/readonly.v2.json` | READONLY | `sha256:aebdd9f7…` |
| `docs/schemas/external/writer.v2.json` | WRITER | `sha256:fb5394c1…` |
| `docs/schemas/external/reviewer.v2.json` | REVIEWER | `sha256:63e1d6c9…` |
| `docs/schemas/external/replanner.v2.json` | REPLANNER | `sha256:6791a950…` |
| `docs/schemas/external/index.v2.json` | index + conditions + bounds | — |

`checked_in_response_contract_artifacts_match_the_runtime_contract` asserts the
files are byte-identical to what the module renders; regenerate with
`cargo test regenerate_response_contract_artifacts -- --ignored`.

## 6. Real-MCP verification (the H5-B gate)

`tests/mcp_contract.rs::goal_next_publishes_a_machine_readable_response_contract`
spawns the real binary and speaks JSON-RPC to it:

1. `goal_start` (EXTERNAL) → `goal_next` → the PLANNER `response_contract` is
   present with id/version/digest/byte semantics/echo fields, and its `schema` is
   byte-equal to the checked-in artifact.
2. Submitting the exact H4-D-F1 answer (`"status": "COMPLETED"`) is **refused**
   (`REJECTED_SCHEMA`) and the schema refuses it too.
3. A plan built from the published contract is **`COMMITTED`**.
4. The next WorkItem is READONLY and publishes
   `["candidate_complete","blocked","needs_replan","failed"]` — the vocabulary
   whose absence was the defect.

## 7. Gate

| Gate | Result |
| --- | --- |
| An external client can obtain all five role contracts without reading Rust | **PASS** — `response_contract` on every `goal_next`; the same bytes are in `docs/schemas/external/` |
| Schema and host validator agree | **PASS** by construction for bounds/enums and by test in both directions; the one intentional strictness (plannable workers) is asserted explicitly |
| No identity/request-digest change | **PASS** — nothing persisted was added; `output_contract` remains the binding |
| Host validator not loosened to pass a test | **PASS** — no validator was relaxed; only constant *imports* were added |
