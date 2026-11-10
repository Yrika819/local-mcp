//! H5-B — proof that the published response contract and the host validator agree.
//!
//! The contract in [`crate::external_response_contract`] is one description
//! rendered twice (JSON Schema for the client, `LIMIT_*` constants for the
//! validators). These tests attack that claim from both sides:
//!
//! * **Safety direction** — every instance the published schema accepts must
//!   also be accepted by the real Rust shape validator, so a client that follows
//!   the contract cannot be surprised by a shape refusal.
//! * **Completeness direction** — every value the published `enum` lists must be
//!   accepted by the real validator, and a value outside the list must be
//!   refused by both. A drifted vocabulary therefore fails a test.
//! * **Semantic bounds** — the byte bounds are checked through the real
//!   read-only parser at the boundary (at the limit and one byte over), because
//!   that parser is the one that produced finding H4-D-F1.
//! * **Artifacts** — the checked-in `docs/schemas/external/*.json` are asserted
//!   byte-identical to what the module renders.
//!
//! Nothing here loosens a validator to make a test pass. Every "accepted" claim
//! comes from the real parser and every "refused" claim comes from the real
//! refusal path.

use serde_json::{Map, Value, json};

use crate::external_model_work::{
    OUTPUT_CONTRACT_PLANNER_V1, OUTPUT_CONTRACT_READONLY_V1, OUTPUT_CONTRACT_REPLANNER_V1,
    OUTPUT_CONTRACT_REVIEWER_V1, OUTPUT_CONTRACT_WRITER_V1, WorkRole,
};
use crate::external_response_contract::{
    self as contract, CONTRACTS, LIMIT_PLAN_OBJECTIVE_BYTES, LIMIT_PLAN_SUMMARY_BYTES,
    LIMIT_PLAN_TASKS, LIMIT_PLAN_TITLE_BYTES, LIMIT_PROPOSAL_ID_BYTES,
    LIMIT_READONLY_EVIDENCE_FIELD_BYTES, LIMIT_READONLY_EVIDENCE_ITEMS,
    LIMIT_READONLY_EVIDENCE_TOTAL_BYTES, LIMIT_READONLY_SUMMARY_BYTES, LIMIT_WRITER_OPERATIONS,
    LIMIT_WRITER_SUMMARY_BYTES, RESPONSE_CONTRACT_ID, RESPONSE_CONTRACT_VERSION,
};
use crate::readonly_worker;
use crate::task::{ReplaySafety, TaskOperationKind};
use crate::worker_capability;

// ---------------------------------------------------------------------------
// A minimal JSON Schema evaluator for exactly the keywords the contract emits.
//
// Unknown keywords are a hard failure rather than silently ignored: an
// unsupported keyword would otherwise make a schema look stricter than it is.
// ---------------------------------------------------------------------------

const SUPPORTED_KEYWORDS: &[&str] = &[
    "type",
    "title",
    "enum",
    "maxLength",
    "x-max-bytes",
    "minItems",
    "maxItems",
    "items",
    "minimum",
    "maximum",
    "properties",
    "required",
    "additionalProperties",
    "oneOf",
    "description",
];

fn schema_accepts(schema: &Value, instance: &Value) -> Result<(), String> {
    match schema {
        Value::Bool(true) => Ok(()),
        Value::Bool(false) => Err("false schema".to_owned()),
        Value::Object(object) => {
            for key in object.keys() {
                assert!(
                    SUPPORTED_KEYWORDS.contains(&key.as_str()),
                    "the contract emitted an unsupported keyword {key}, which this test cannot check"
                );
            }
            if let Some(expected) = object.get("type") {
                let ok = match expected.as_str().unwrap_or_default() {
                    "object" => instance.is_object(),
                    "array" => instance.is_array(),
                    "string" => instance.is_string(),
                    "integer" => instance.is_i64() || instance.is_u64(),
                    "boolean" => instance.is_boolean(),
                    other => panic!("unsupported type {other}"),
                };
                if !ok {
                    return Err(format!("type mismatch, expected {expected}"));
                }
            }
            if let Some(values) = object.get("enum").and_then(Value::as_array)
                && !values.contains(instance)
            {
                return Err(format!("{} is not in the published enum", instance));
            }
            if let Some(text) = instance.as_str() {
                if let Some(limit) = object.get("x-max-bytes").and_then(Value::as_u64)
                    && text.len() as u64 > limit
                {
                    return Err(format!(
                        "string is {} bytes, published byte bound is {limit}",
                        text.len()
                    ));
                }
                if let Some(limit) = object.get("maxLength").and_then(Value::as_u64)
                    && text.chars().count() as u64 > limit
                {
                    return Err(format!("string is too long for maxLength {limit}"));
                }
            }
            if let Some(maximum) = object.get("maximum").and_then(Value::as_i64)
                && instance.as_i64().is_some_and(|number| number > maximum)
            {
                return Err(format!("{instance} is above maximum {maximum}"));
            }
            if let Some(minimum) = object.get("minimum").and_then(Value::as_i64) {
                let below = match (instance.as_i64(), instance.as_u64()) {
                    (Some(number), _) => number < minimum,
                    (None, Some(number)) => (number as i128) < (minimum as i128),
                    _ => false,
                };
                if below {
                    return Err(format!("{instance} is below minimum {minimum}"));
                }
            }
            if let Some(items) = instance.as_array() {
                if let Some(min) = object.get("minItems").and_then(Value::as_u64)
                    && (items.len() as u64) < min
                {
                    return Err(format!("array has {} items, minItems {min}", items.len()));
                }
                if let Some(max) = object.get("maxItems").and_then(Value::as_u64)
                    && (items.len() as u64) > max
                {
                    return Err(format!("array has {} items, maxItems {max}", items.len()));
                }
                if let Some(item_schema) = object.get("items") {
                    for (index, item) in items.iter().enumerate() {
                        schema_accepts(item_schema, item)
                            .map_err(|error| format!("item {index}: {error}"))?;
                    }
                }
            }
            if let Some(object_instance) = instance.as_object() {
                if let Some(required) = object.get("required").and_then(Value::as_array) {
                    for name in required {
                        let name = name.as_str().unwrap_or_default();
                        if !object_instance.contains_key(name) {
                            return Err(format!("missing required property {name}"));
                        }
                    }
                }
                if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                    for (name, value) in object_instance {
                        match properties.get(name) {
                            Some(property_schema) => schema_accepts(property_schema, value)
                                .map_err(|error| format!("property {name}: {error}"))?,
                            None => {
                                if object.get("additionalProperties") == Some(&Value::Bool(false)) {
                                    return Err(format!(
                                        "additional property {name} is not allowed"
                                    ));
                                }
                            }
                        }
                    }
                } else if object.get("additionalProperties") == Some(&Value::Bool(false)) {
                    return Err("object accepts no properties".to_owned());
                }
            }
            if let Some(branches) = object.get("oneOf").and_then(Value::as_array) {
                let matches = branches
                    .iter()
                    .filter(|branch| schema_accepts(branch, instance).is_ok())
                    .count();
                if matches != 1 {
                    return Err(format!("{matches} oneOf branches matched, expected 1"));
                }
            }
            Ok(())
        }
        other => panic!("schema node is not an object: {other}"),
    }
}

fn assert_schema_accepts(role: &str, instance: &Value) {
    let contract = contract_by_role(role);
    let schema = contract::schema(contract);
    if let Err(error) = schema_accepts(&schema, instance) {
        panic!("{role} schema refused an instance the host accepts: {error}\n{instance:#}");
    }
}

fn assert_schema_refuses(role: &str, instance: &Value) -> String {
    let contract = contract_by_role(role);
    let schema = contract::schema(contract);
    match schema_accepts(&schema, instance) {
        Ok(()) => panic!("{role} schema accepted an instance it must refuse: {instance:#}"),
        Err(error) => error,
    }
}

fn contract_by_role(role: &str) -> &'static contract::ResponseContract {
    CONTRACTS
        .iter()
        .find(|contract| contract.role == role)
        .expect("role is registered")
}

// ---------------------------------------------------------------------------
// Positive samples: one per role, built only from published contract values.
// ---------------------------------------------------------------------------

fn plan_task(proposal_id: &str, worker: &str, operation_kind: &str, replay_safety: &str) -> Value {
    json!({
        "proposal_id": proposal_id,
        "title": "inspect the fixture",
        "objective": "read every file under src and report the count",
        "mandatory": true,
        "worker": worker,
        "dependencies": [],
        "scope": {
            "allowed_paths": ["src"],
            "forbidden_paths": [],
            "operation_kind": operation_kind,
            "replay_safety": replay_safety
        },
        "verification": [
            {"kind": "STRUCTURED_EVIDENCE", "requirement_id": "survey"}
        ]
    })
}

fn planner_sample() -> Value {
    json!({
        "goal_id": "00000000-0000-4000-8000-000000000001",
        "goal_revision": 3,
        "summary": "one bounded read-only survey",
        "tasks": [plan_task("survey", "CODEX_READONLY", "READ_ONLY", "SAFE_READ_ONLY")],
        "criterion_bindings": [
            {"criterion_id": "00000000-0000-4000-8000-0000000000c1", "task_refs": ["survey"]}
        ]
    })
}

fn readonly_sample() -> Value {
    json!({
        "goal_id": "00000000-0000-4000-8000-000000000001",
        "task_id": "00000000-0000-4000-8000-000000000002",
        "attempt_id": "00000000-0000-4000-8000-000000000003",
        "goal_revision": 2,
        "plan_revision": 1,
        "status": "candidate_complete",
        "summary": "src contains one file",
        "evidence": [{"kind": "OBSERVATION", "value": "one file"}]
    })
}

fn writer_sample() -> Value {
    json!({
        "goal_id": "00000000-0000-4000-8000-000000000001",
        "task_id": "00000000-0000-4000-8000-000000000002",
        "attempt_id": "00000000-0000-4000-8000-000000000003",
        "goal_revision": 7,
        "plan_revision": 3,
        "status": "candidate_complete",
        "summary": "rename the marker",
        "evidence": [{"kind": "INSPECTION", "value": "read src/main.rs"}],
        "proposed_operations": [{
            "kind": "WRITE_UTF8",
            "path": "src/main.rs",
            "expected_preimage": {"kind": "ABSENT"},
            "content": "fn main() {}\n"
        }]
    })
}

fn reviewer_sample() -> Value {
    json!({
        "goal_id": "00000000-0000-4000-8000-000000000001",
        "task_id": "00000000-0000-4000-8000-000000000002",
        "attempt_id": "00000000-0000-4000-8000-000000000003",
        "goal_revision": 8,
        "plan_revision": 3,
        "summary": "the published change matches the intent",
        "blocking_findings": 0,
        "evidence": [{"kind": "DIFF", "value": "one file changed"}]
    })
}

fn replanner_sample() -> Value {
    json!({
        "goal_id": "00000000-0000-4000-8000-000000000001",
        "base_goal_revision": 11,
        "base_plan_revision": 2,
        "summary": "add a bounded prerequisite before the mutation",
        "add_tasks": [plan_task("probe", "CODEX_READONLY", "READ_ONLY", "SAFE_READ_ONLY")],
        "add_dependencies": [],
        "strengthen_verification": [],
        "strengthen_mandatory": [],
        "strengthen_criterion_bindings": [],
        "resolve_needs_replan": [],
        "replace_tasks": []
    })
}

fn samples() -> Vec<(&'static str, Value)> {
    vec![
        ("PLANNER", planner_sample()),
        ("READONLY", readonly_sample()),
        ("WRITER", writer_sample()),
        ("REVIEWER", reviewer_sample()),
        ("REPLANNER", replanner_sample()),
    ]
}

/// The real shape validator for a role, as the host parser uses it.
fn host_shape_accepts(role: &str, instance: &Value) -> bool {
    match role {
        "PLANNER" => crate::planner::response_shape_accepts(instance),
        "READONLY" => readonly_worker::readonly_response_shape_accepts(instance),
        "WRITER" => crate::writer::writer_response_shape_accepts(instance),
        "REVIEWER" => crate::writer::reviewer_response_shape_accepts(instance),
        "REPLANNER" => crate::replanner::response_shape_accepts(instance),
        other => panic!("unknown role {other}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Contract identity.
// ---------------------------------------------------------------------------

#[test]
fn every_role_is_bound_to_its_persisted_output_contract() {
    for (role, output_contract) in [
        (WorkRole::Planner, OUTPUT_CONTRACT_PLANNER_V1),
        (WorkRole::Readonly, OUTPUT_CONTRACT_READONLY_V1),
        (WorkRole::Writer, OUTPUT_CONTRACT_WRITER_V1),
        (WorkRole::Reviewer, OUTPUT_CONTRACT_REVIEWER_V1),
        (WorkRole::Replanner, OUTPUT_CONTRACT_REPLANNER_V1),
    ] {
        let by_role = contract::contract_for_role(role);
        let by_string = contract::contract_for_output_contract(output_contract)
            .expect("the persisted output contract string resolves");
        assert_eq!(by_role.output_contract, output_contract);
        assert_eq!(by_role.output_contract, by_string.output_contract);
        // A WorkItem reloaded from disk carries only the string, so the two
        // lookups must be the same contract, not merely similar.
        assert_eq!(
            contract::schema_digest(by_role),
            contract::schema_digest(by_string)
        );
    }
    assert!(
        contract::contract_for_output_contract("codex-shell-v0").is_none(),
        "an unknown persisted contract must not be invented into a known one"
    );
}

#[test]
fn contract_ids_and_digests_are_unique_and_stable() {
    use std::collections::BTreeSet;

    let mut ids = BTreeSet::new();
    let mut digests = BTreeSet::new();
    for contract in CONTRACTS {
        assert!(ids.insert(contract::contract_id(contract)));
        let digest = contract::schema_digest(contract);
        assert!(digest.starts_with("sha256:"));
        assert_eq!(digest.len(), "sha256:".len() + 64);
        assert!(digests.insert(digest.clone()));
        // Same process, same contract, same bytes.
        assert_eq!(digest, contract::schema_digest(contract));
        assert_eq!(
            Some(&digest),
            contract::schema_digests().get(contract.role),
            "the cached digest table must agree with the live one"
        );
    }
    assert_eq!(ids.len(), 5);
    assert_eq!(digests.len(), 5);
    assert_eq!(RESPONSE_CONTRACT_VERSION, 2);
    assert_eq!(RESPONSE_CONTRACT_ID, "goallatch.external.response.v2");
}

#[test]
fn published_schema_is_small_enough_to_travel_inline() {
    for contract in CONTRACTS {
        let bytes = serde_json::to_vec(&contract::schema(contract)).unwrap();
        assert!(
            bytes.len() <= 64 * 1024,
            "{} schema is {} bytes, which is too large to publish on every goal_next",
            contract.role,
            bytes.len()
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Positive samples: schema and the real validator both accept.
// ---------------------------------------------------------------------------

#[test]
fn a_contract_valid_sample_is_accepted_by_the_schema_and_the_host() {
    for (role, sample) in samples() {
        assert_schema_accepts(role, &sample);
        assert!(
            host_shape_accepts(role, &sample),
            "{role} sample is contract-valid but the real validator refuses it: {sample:#}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Safety direction: whatever the schema accepts, the host accepts.
// ---------------------------------------------------------------------------

#[test]
fn the_schema_is_never_more_permissive_than_the_host_shape_validator() {
    for (role, sample) in samples() {
        for mutation in shape_mutations(&sample) {
            let schema_ok =
                schema_accepts(&contract::schema(contract_by_role(role)), &mutation).is_ok();
            if !schema_ok {
                continue;
            }
            assert!(
                host_shape_accepts(role, &mutation),
                "{role}: the published schema accepted a response the host refuses: {mutation:#}"
            );
        }
    }
}

/// Shape mutations that both sides must agree about: drop a required field, add
/// an unknown field, retype a field, and corrupt a nested discriminator.
fn shape_mutations(sample: &Value) -> Vec<Value> {
    let mut mutations = Vec::new();
    let object = sample.as_object().expect("samples are objects");
    for name in object.keys() {
        let mut removed = object.clone();
        removed.remove(name);
        mutations.push(Value::Object(removed));

        let mut unknown = object.clone();
        unknown.insert("host_authority_override".to_owned(), json!("OWNER"));
        mutations.push(Value::Object(unknown));

        let mut retyped = object.clone();
        let original = object.get(name).cloned().unwrap_or(Value::Null);
        let replacement = match original {
            Value::String(_) => json!(17),
            Value::Number(_) => json!("seventeen"),
            Value::Array(_) => json!({"not": "an array"}),
            Value::Object(_) => json!("not an object"),
            Value::Bool(_) => json!("not a bool"),
            Value::Null => json!(null),
        };
        retyped.insert(name.clone(), replacement);
        mutations.push(Value::Object(retyped));
    }
    // Nested: flip a tagged discriminator inside an array of tagged objects.
    let mut nested = object.clone();
    if let Some(tasks) = nested.get("tasks").and_then(Value::as_array) {
        let mut tasks = tasks.clone();
        if let Some(task) = tasks.get_mut(0).and_then(Value::as_object_mut) {
            task.insert("worker".to_owned(), json!("LOCAL_OPERATION"));
        }
        nested.insert("tasks".to_owned(), Value::Array(tasks));
        mutations.push(Value::Object(nested));
    }
    if let Some(operations) = object.get("proposed_operations").and_then(Value::as_array) {
        let mut operations_value = operations.clone();
        if let Some(operation) = operations_value.get_mut(0).and_then(Value::as_object_mut) {
            operation.insert("kind".to_owned(), json!("DELETE_FILE"));
        }
        let mut nested = object.clone();
        nested.insert(
            "proposed_operations".to_owned(),
            Value::Array(operations_value),
        );
        mutations.push(Value::Object(nested));
    }
    mutations
}

// ---------------------------------------------------------------------------
// 4. Completeness direction: published vocabularies are exact.
// ---------------------------------------------------------------------------

#[test]
fn published_worker_vocabulary_is_exactly_the_plannable_set() {
    let published = contract::DEFINITIONS
        .iter()
        .filter_map(|def| match def.body {
            contract::DefBody::Object(fields) => Some(fields),
            contract::DefBody::Union(_) => None,
        })
        .flat_map(|fields| fields.iter())
        .filter(|field| field.name == "worker")
        .map(|field| field.values)
        .next()
        .expect("plan_task publishes a worker vocabulary");

    for kind in contract::ALL_WORKER_KINDS {
        let wire = contract::serde_wire_name(kind);
        let plannable = worker_capability::is_production_plannable(*kind);
        assert_eq!(
            published.contains(&wire.as_str()),
            plannable,
            "published planner worker vocabulary disagrees with worker_capability for {wire}"
        );
    }

    // Every published value is usable in a real plan, and nothing else is.
    for worker in published {
        let sample = planner_with_worker(worker);
        assert!(
            crate::planner::response_shape_accepts(&sample),
            "published worker {worker} is refused by the plan proposal validator"
        );
        assert_schema_accepts("PLANNER", &sample);
    }
    // A non-plannable worker is a *semantic* refusal (the shape validator still
    // deserializes `WorkerKind`), so the published schema is deliberately
    // stricter than the shape and the difference is published as a condition.
    for worker in ["LOCAL_OPERATION", "CODEX_REVIEWER", "VERIFIER"] {
        let sample = planner_with_worker(worker);
        assert!(
            crate::planner::response_shape_accepts(&sample),
            "the plan proposal shape validator deserializes every WorkerKind"
        );
        assert_schema_refuses("PLANNER", &sample);
    }
    // And a value the enum cannot represent is refused by both.
    let bogus = planner_with_worker("codex_readonly");
    assert!(!crate::planner::response_shape_accepts(&bogus));
    assert_schema_refuses("PLANNER", &bogus);
}

fn planner_with_worker(worker: &str) -> Value {
    let mut sample = planner_sample();
    sample["tasks"][0]["worker"] = json!(worker);
    sample
}

#[test]
fn published_scope_vocabularies_are_complete() {
    let operation_values: Vec<String> = contract::ALL_OPERATION_KINDS
        .iter()
        .map(contract::serde_wire_name)
        .collect();
    let replay_values: Vec<String> = contract::ALL_REPLAY_SAFETY
        .iter()
        .map(contract::serde_wire_name)
        .collect();

    for operation in &operation_values {
        for replay in &replay_values {
            let mut sample = planner_sample();
            sample["tasks"][0]["scope"]["operation_kind"] = json!(operation);
            sample["tasks"][0]["scope"]["replay_safety"] = json!(replay);
            // Shape-wise every combination deserializes; the *authority* pairing
            // rule is a semantic condition, not a shape rule, and is published
            // as PLANNER_SCOPE_WITHIN_AUTHORITY.
            assert!(
                crate::planner::response_shape_accepts(&sample),
                "operation_kind {operation} / replay_safety {replay} is not shape-valid"
            );
            assert_schema_accepts("PLANNER", &sample);
        }
    }

    let mut bad_operation = planner_sample();
    bad_operation["tasks"][0]["scope"]["operation_kind"] = json!("DELETE_EVERYTHING");
    assert!(!crate::planner::response_shape_accepts(&bad_operation));
    assert_schema_refuses("PLANNER", &bad_operation);

    let mut bad_replay = planner_sample();
    bad_replay["tasks"][0]["scope"]["replay_safety"] = json!("HOPE_FOR_THE_BEST");
    assert!(!crate::planner::response_shape_accepts(&bad_replay));
    assert_schema_refuses("PLANNER", &bad_replay);

    // ReadOnly must not carry a mutating operation kind in a real plan: the
    // planner authority check refuses it. This is a *semantic* refusal, so the
    // shape validator accepts it, which is exactly why the contract publishes it
    // as a condition rather than as a schema keyword.
    let mut readonly_mutation = planner_sample();
    readonly_mutation["tasks"][0]["scope"]["operation_kind"] =
        json!(TaskOperationKind::LocalMutation);
    readonly_mutation["tasks"][0]["scope"]["replay_safety"] =
        json!(ReplaySafety::VerifyBeforeRetry);
    assert_schema_accepts("PLANNER", &readonly_mutation);
    assert!(crate::planner::response_shape_accepts(&readonly_mutation));
}

#[test]
fn published_verification_vocabulary_matches_the_real_enum() {
    let real = contract::all_verification_kinds();
    assert_eq!(real.len(), 7, "every VerificationSpec variant is sampled");

    let published = published_verification_kinds();
    // H5-A round 2 (R2-9): matching the *serde* enum identity is not enough. What
    // matters is the vocabulary the host actually accepts, so a kind that the
    // shared validator refuses (COMMAND_EXIT) must not be published even though
    // the type can represent it.
    assert!(
        published.len() <= real.len(),
        "the contract publishes more kinds than the enum can represent"
    );
    assert!(
        !published.contains(&"COMMAND_EXIT".to_owned()),
        "COMMAND_EXIT is refused unconditionally by validate_and_normalize_verification,          so it must not be advertised as an accepted verification form"
    );
    assert_eq!(
        published.len(),
        real.len() - 1,
        "exactly the one unsupported kind is withheld"
    );

    for kind in &published {
        assert!(
            real.contains(kind),
            "the contract publishes {kind}, which the VerificationSpec enum cannot represent"
        );
        let spec = verification_sample(kind);
        let sample = planner_with_verification(spec);
        assert!(
            crate::planner::response_shape_accepts(&sample),
            "verification kind {kind} is refused by the proposal shape validator"
        );
        assert_schema_accepts("PLANNER", &sample);
    }
    // And the withheld kind is still refused end to end.
    let withheld = planner_with_verification(verification_sample("COMMAND_EXIT"));
    assert!(crate::planner::response_shape_accepts(&withheld));
    assert_schema_refuses("PLANNER", &withheld);

    let mut bad = planner_sample();
    bad["tasks"][0]["verification"] = json!([{"kind": "RUN_ANYTHING", "command": ["rm"]}]);
    assert!(!crate::planner::response_shape_accepts(&bad));
    assert_schema_refuses("PLANNER", &bad);
}

fn verification_sample(kind: &str) -> Value {
    match kind {
        "COMMAND_EXIT" => json!({
            "kind": kind,
            "command": ["true"],
            "accepted_exit_codes": [0]
        }),
        "FILE_EXISTS" => json!({"kind": kind, "path": "src/main.rs", "must_be_file": true}),
        "FILE_DIGEST" => json!({
            "kind": kind,
            "path": "src/main.rs",
            "expected_sha256": "0".repeat(64)
        }),
        "GIT_SCOPE" => json!({
            "kind": kind,
            "allowed_changed_paths": ["src/main.rs"],
            "require_no_other_changes": true
        }),
        "NO_FORBIDDEN_CHANGES" => json!({"kind": kind, "forbidden_paths": ["src/secret.rs"]}),
        "STRUCTURED_EVIDENCE" => json!({"kind": kind, "requirement_id": "survey"}),
        "REVIEW_GATE" => json!({"kind": kind, "max_blocking_findings": 0}),
        other => panic!("no sample for verification kind {other}"),
    }
}

fn planner_with_verification(spec: Value) -> Value {
    let mut sample = planner_sample();
    sample["tasks"][0]["verification"] = json!([spec]);
    sample
}

fn published_verification_kinds() -> Vec<String> {
    let def = contract::DEFINITIONS
        .iter()
        .find(|def| def.name == "verification_spec")
        .expect("verification_spec is registered");
    let contract::DefBody::Union(branches) = def.body else {
        panic!("verification_spec is a union")
    };
    branches
        .iter()
        .filter_map(|branch| {
            branch
                .iter()
                .find(|field| field.name == "kind")
                .and_then(|field| field.values.first().copied())
                .map(str::to_owned)
        })
        .collect()
}

#[test]
fn published_status_vocabulary_is_enforced_by_the_read_only_parser() {
    let request = readonly_worker::readonly_request_for_model_backend_test(std::env::temp_dir());

    for status in contract::READONLY_STATUS_VALUES {
        let mut sample = readonly_sample();
        sample["status"] = json!(status);
        if *status == "candidate_complete" {
            // unchanged
        }
        assert_schema_accepts("READONLY", &sample);
        let raw = serde_json::to_vec(&sample).unwrap();
        readonly_worker::parse_and_validate(&raw, &request)
            .unwrap_or_else(|error| panic!("published status {status} was refused: {error}"));
    }

    // The exact H4-D-F1 failure: a model that answers "COMPLETED".
    for bogus in [
        "COMPLETED",
        "CandidateComplete",
        "CANDIDATE_COMPLETE",
        "complete",
        "",
    ] {
        let mut sample = readonly_sample();
        sample["status"] = json!(bogus);
        let raw = serde_json::to_vec(&sample).unwrap();
        assert!(
            readonly_worker::parse_and_validate(&raw, &request).is_err(),
            "the read-only parser accepted the out-of-contract status {bogus:?}"
        );
        assert_schema_refuses("READONLY", &sample);
    }
}

// ---------------------------------------------------------------------------
// 5. Semantic bounds, checked through the real read-only parser.
// ---------------------------------------------------------------------------

#[test]
fn read_only_summary_bound_is_published_and_enforced() {
    let request = readonly_worker::readonly_request_for_model_backend_test(std::env::temp_dir());

    let mut at_limit = readonly_sample();
    at_limit["summary"] = json!("s".repeat(LIMIT_READONLY_SUMMARY_BYTES));
    assert_schema_accepts("READONLY", &at_limit);
    readonly_worker::parse_and_validate(&serde_json::to_vec(&at_limit).unwrap(), &request)
        .expect("a summary exactly at the published bound is valid");

    let mut over = readonly_sample();
    over["summary"] = json!("s".repeat(LIMIT_READONLY_SUMMARY_BYTES + 1));
    assert_schema_refuses("READONLY", &over);
    assert!(
        readonly_worker::parse_and_validate(&serde_json::to_vec(&over).unwrap(), &request).is_err(),
        "a summary one byte over the published bound must be refused"
    );
}

#[test]
fn read_only_evidence_bounds_are_published_and_enforced() {
    let request = readonly_worker::readonly_request_for_model_backend_test(std::env::temp_dir());

    let mut at_limit = readonly_sample();
    at_limit["evidence"] = json!(vec![
        json!({"kind": "OBSERVATION", "value": "v"});
        LIMIT_READONLY_EVIDENCE_ITEMS
    ]);
    assert_schema_accepts("READONLY", &at_limit);
    readonly_worker::parse_and_validate(&serde_json::to_vec(&at_limit).unwrap(), &request)
        .expect("exactly the published evidence count is valid");

    let mut over = readonly_sample();
    over["evidence"] = json!(vec![
        json!({"kind": "OBSERVATION", "value": "v"});
        LIMIT_READONLY_EVIDENCE_ITEMS + 1
    ]);
    assert_schema_refuses("READONLY", &over);
    assert!(
        readonly_worker::parse_and_validate(&serde_json::to_vec(&over).unwrap(), &request).is_err(),
        "one evidence item over the published bound must be refused"
    );

    let mut field_over = readonly_sample();
    field_over["evidence"] = json!([{
        "kind": "OBSERVATION",
        "value": "v".repeat(LIMIT_READONLY_EVIDENCE_FIELD_BYTES + 1)
    }]);
    assert_schema_refuses("READONLY", &field_over);
    assert!(
        readonly_worker::parse_and_validate(&serde_json::to_vec(&field_over).unwrap(), &request)
            .is_err(),
        "an evidence value one byte over the published bound must be refused"
    );

    // The total-evidence bound is a condition, not a per-field keyword: two
    // items each inside the field bound can still exceed the sum.
    // Nine items, each exactly at the published per-item bound, exceed the total
    // bound while every individual item is schema-valid.
    let items = (LIMIT_READONLY_EVIDENCE_TOTAL_BYTES / LIMIT_READONLY_EVIDENCE_FIELD_BYTES) + 1;
    let mut total_over = readonly_sample();
    total_over["evidence"] = Value::Array(
        (0..items)
            .map(|_| {
                json!({
                    "kind": "OBSERVATION",
                    "value": "v".repeat(LIMIT_READONLY_EVIDENCE_FIELD_BYTES)
                })
            })
            .collect(),
    );
    assert_schema_accepts("READONLY", &total_over);
    let refusal =
        readonly_worker::parse_and_validate(&serde_json::to_vec(&total_over).unwrap(), &request);
    assert!(
        refusal.is_err(),
        "the total-evidence condition must be enforced"
    );
    assert!(
        contract::CONTRACTS
            .iter()
            .find(|contract| contract.role == "READONLY")
            .unwrap()
            .conditions
            .iter()
            .any(|condition| condition.code == "READONLY_EVIDENCE_TOTAL_BOUND"),
        "the total-evidence rule must be published as a condition"
    );
}

#[test]
fn read_only_identity_echo_is_refused_by_the_real_parser() {
    let request = readonly_worker::readonly_request_for_model_backend_test(std::env::temp_dir());

    for (field, value) in [
        ("goal_id", json!("00000000-0000-4000-8000-0000000000ff")),
        ("task_id", json!("00000000-0000-4000-8000-0000000000fe")),
        ("attempt_id", json!("00000000-0000-4000-8000-0000000000fd")),
        ("goal_revision", json!(99)),
        ("plan_revision", json!(99)),
    ] {
        let mut sample = readonly_sample();
        sample[field] = value;
        // Shape-valid: echoing the wrong value is an authority error, not a
        // shape error, which is why the contract publishes it as a condition.
        assert_schema_accepts("READONLY", &sample);
        let raw = serde_json::to_vec(&sample).unwrap();
        assert!(
            readonly_worker::parse_and_validate(&raw, &request).is_err(),
            "a mismatched {field} must be refused by the read-only parser"
        );
    }
}

#[test]
fn read_only_rejects_write_operations_even_when_shape_valid() {
    let request = readonly_worker::readonly_request_for_model_backend_test(std::env::temp_dir());
    let mut sample = readonly_sample();
    sample["proposed_operations"] = json!([]);
    // `deny_unknown_fields` means a read-only answer cannot smuggle a write
    // vocabulary at all: it is a shape refusal, so the schema refuses it too.
    assert_schema_refuses("READONLY", &sample);
    assert!(
        readonly_worker::parse_and_validate(&serde_json::to_vec(&sample).unwrap(), &request)
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// 6. The writer's candidate_complete condition.
// ---------------------------------------------------------------------------

#[test]
fn writer_operation_bound_is_published_and_matches_the_real_limit() {
    let request = crate::writer::writer_request_for_model_backend_test(std::env::temp_dir());

    let mut at_limit = writer_sample();
    at_limit["proposed_operations"] = json!(vec![
        json!({
            "kind": "WRITE_UTF8",
            "path": "src/main.rs",
            "expected_preimage": {"kind": "ABSENT"},
            "content": "x"
        });
        LIMIT_WRITER_OPERATIONS
    ]);
    assert_schema_accepts("WRITER", &at_limit);
    crate::writer::parse_and_validate_writer_result(
        &serde_json::to_vec(&at_limit).unwrap(),
        &request,
    )
    .expect("exactly the published operation count is valid");

    let mut over = writer_sample();
    over["proposed_operations"] = json!(vec![
        json!({
            "kind": "WRITE_UTF8",
            "path": "src/main.rs",
            "expected_preimage": {"kind": "ABSENT"},
            "content": "x"
        });
        LIMIT_WRITER_OPERATIONS + 1
    ]);
    assert_schema_refuses("WRITER", &over);
    assert!(
        crate::writer::parse_and_validate_writer_result(
            &serde_json::to_vec(&over).unwrap(),
            &request
        )
        .is_err(),
        "one operation over the published bound must be refused"
    );

    // candidate_complete with zero operations is refused by the writer
    // validator; the contract publishes that as a condition.
    let mut empty = writer_sample();
    empty["proposed_operations"] = json!([]);
    assert_schema_accepts("WRITER", &empty);
    assert!(
        crate::writer::parse_and_validate_writer_result(
            &serde_json::to_vec(&empty).unwrap(),
            &request
        )
        .is_err(),
        "candidate_complete with no operations must be refused"
    );
    let writer_contract = contract_by_role("WRITER");
    assert!(
        writer_contract
            .conditions
            .iter()
            .any(|condition| condition.code == "WRITER_CANDIDATE_COMPLETE_REQUIRES_OPERATIONS")
    );
}

#[test]
fn writer_summary_bound_is_shared_with_the_contract() {
    let request = crate::writer::writer_request_for_model_backend_test(std::env::temp_dir());

    let mut at_limit = writer_sample();
    at_limit["summary"] = json!("s".repeat(LIMIT_WRITER_SUMMARY_BYTES));
    assert_schema_accepts("WRITER", &at_limit);
    crate::writer::parse_and_validate_writer_result(
        &serde_json::to_vec(&at_limit).unwrap(),
        &request,
    )
    .expect("a writer summary at the published bound is valid");

    let mut over = writer_sample();
    over["summary"] = json!("s".repeat(LIMIT_WRITER_SUMMARY_BYTES + 1));
    assert_schema_refuses("WRITER", &over);
    assert!(
        crate::writer::parse_and_validate_writer_result(
            &serde_json::to_vec(&over).unwrap(),
            &request
        )
        .is_err()
    );
}

#[test]
fn reviewer_blocking_findings_is_a_count_not_a_verdict() {
    for count in [0, 1, 7] {
        let mut sample = reviewer_sample();
        sample["blocking_findings"] = json!(count);
        assert_schema_accepts("REVIEWER", &sample);
        assert!(crate::writer::reviewer_response_shape_accepts(&sample));
    }
    for bogus in [json!(-1), json!("none"), json!(true)] {
        let mut sample = reviewer_sample();
        sample["blocking_findings"] = bogus.clone();
        assert_schema_refuses("REVIEWER", &sample);
        assert!(
            !crate::writer::reviewer_response_shape_accepts(&sample),
            "blocking_findings {bogus} must not be accepted"
        );
    }
}

// ---------------------------------------------------------------------------
// 7. Planner / Replanner bounds are shared constants, and the schema says so.
// ---------------------------------------------------------------------------

#[test]
fn planner_bounds_are_the_shared_constants() {
    // The validators import these, so this asserts the published schema renders
    // the very same numbers the validator enforces. If someone changes the
    // validator constant and not the contract, this fails.
    let published = published_text_bounds("PLANNER");
    assert_eq!(
        published.get("summary").copied(),
        Some(LIMIT_PLAN_SUMMARY_BYTES)
    );
    let task_bounds = published_numeric_bounds("plan_task");
    assert_eq!(
        task_bounds.get("title").copied(),
        Some(LIMIT_PLAN_TITLE_BYTES)
    );
    assert_eq!(
        task_bounds.get("objective").copied(),
        Some(LIMIT_PLAN_OBJECTIVE_BYTES)
    );
    assert_eq!(
        task_bounds.get("proposal_id").copied(),
        Some(LIMIT_PROPOSAL_ID_BYTES)
    );
    let item_bounds = published_item_bounds("PLANNER");
    assert_eq!(item_bounds.get("tasks"), Some(&(1usize, LIMIT_PLAN_TASKS)));

    // And the validator rejects one byte over the shared bound.
    let mut over = planner_sample();
    over["summary"] = json!("s".repeat(LIMIT_PLAN_SUMMARY_BYTES + 1));
    // The plan proposal *shape* accepts it: the byte bound is semantic. The
    // contract therefore publishes the byte bound as a keyword so a client sees
    // it, and the semantic enforcement is the real planner's.
    assert!(crate::planner::response_shape_accepts(&over));
    assert_schema_refuses("PLANNER", &over);
}

fn published_text_bounds(role: &str) -> std::collections::BTreeMap<String, usize> {
    let mut bounds = std::collections::BTreeMap::new();
    for field in contract_by_role(role).response {
        if field.max_bytes > 0 {
            bounds.insert(field.name.to_owned(), field.max_bytes);
        }
    }
    bounds
}

fn published_numeric_bounds(def_name: &str) -> std::collections::BTreeMap<String, usize> {
    let def = contract::DEFINITIONS
        .iter()
        .find(|def| def.name == def_name)
        .expect("definition is registered");
    let contract::DefBody::Object(fields) = def.body else {
        panic!("{def_name} is an object")
    };
    fields
        .iter()
        .filter(|field| field.max_bytes > 0)
        .map(|field| (field.name.to_owned(), field.max_bytes))
        .collect()
}

fn published_item_bounds(role: &str) -> std::collections::BTreeMap<String, (usize, usize)> {
    contract_by_role(role)
        .response
        .iter()
        .filter(|field| field.max_items > 0)
        .map(|field| (field.name.to_owned(), (field.min_items, field.max_items)))
        .collect()
}

// ---------------------------------------------------------------------------
// 8. Checked-in artifacts.
// ---------------------------------------------------------------------------

const ARTIFACT_DIR: &str = "docs/schemas/external";

fn artifact_path(role: &str) -> std::path::PathBuf {
    std::path::Path::new(ARTIFACT_DIR).join(format!(
        "{}.v{}.json",
        role.to_lowercase(),
        RESPONSE_CONTRACT_VERSION
    ))
}

#[test]
fn checked_in_response_contract_artifacts_match_the_runtime_contract() {
    for contract in CONTRACTS {
        let path = artifact_path(contract.role);
        let on_disk = std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "{} is missing ({error}); regenerate with `cargo test regenerate_response_contract_artifacts -- --ignored`",
                path.display()
            )
        });
        assert_eq!(
            on_disk,
            contract::artifact_bytes(contract),
            "{} is stale; regenerate with `cargo test regenerate_response_contract_artifacts -- --ignored`",
            path.display()
        );
    }
    let index = std::fs::read(std::path::Path::new(ARTIFACT_DIR).join("index.v2.json"))
        .expect("the contract index is checked in");
    assert_eq!(index, contract::index_artifact_bytes());
}

/// Regenerate the checked-in artifacts. Ignored by default so a normal test run
/// never writes to the repository.
#[test]
#[ignore = "writes docs/schemas/external/*.json; run explicitly after changing the contract"]
fn regenerate_response_contract_artifacts() {
    std::fs::create_dir_all(ARTIFACT_DIR).unwrap();
    for contract in CONTRACTS {
        std::fs::write(
            artifact_path(contract.role),
            contract::artifact_bytes(contract),
        )
        .unwrap();
    }
    std::fs::write(
        std::path::Path::new(ARTIFACT_DIR).join("index.v2.json"),
        contract::index_artifact_bytes(),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// 9. The projection the client actually receives.
// ---------------------------------------------------------------------------

#[test]
fn contract_projection_carries_everything_a_client_needs() {
    for contract in CONTRACTS {
        let required = contract::required_fields(contract);
        let echo = contract::identity_echo_fields(contract);
        assert!(!required.is_empty());
        assert!(
            !echo.is_empty(),
            "{} must tell the client which identity fields to echo",
            contract.role
        );
        for field in echo {
            assert!(
                required.contains(&field),
                "an echoed field must also be required ({field})"
            );
        }
        assert!(!contract.conditions.is_empty());
        assert!(contract.max_response_bytes > 0);
        // Every published enum must be non-empty and drawn from the real
        // vocabulary; this is the invariant H4-D-F1 violated.
        for field in contract.response {
            if !field.values.is_empty() {
                let mut seen = std::collections::BTreeSet::new();
                for value in field.values {
                    assert!(!value.is_empty());
                    assert!(seen.insert(*value), "duplicate enum value {value}");
                }
            }
        }
    }

    // The read-only status vocabulary is published with its enum, not just as a
    // prose note.
    let status_field = contract_by_role("READONLY")
        .response
        .iter()
        .find(|field| field.name == "status")
        .expect("readonly publishes status");
    assert_eq!(status_field.values, contract::READONLY_STATUS_VALUES);
}

#[test]
fn schema_renders_the_published_enum_and_bounds_verbatim() {
    let schema = contract::schema(contract_by_role("READONLY"));
    let status = &schema["properties"]["status"];
    assert_eq!(status["enum"], json!(contract::READONLY_STATUS_VALUES));
    let summary = &schema["properties"]["summary"];
    assert_eq!(summary["x-max-bytes"], json!(LIMIT_READONLY_SUMMARY_BYTES));
    let evidence = &schema["properties"]["evidence"];
    assert_eq!(evidence["maxItems"], json!(LIMIT_READONLY_EVIDENCE_ITEMS));
    assert_eq!(
        evidence["items"]["properties"]["kind"]["x-max-bytes"],
        json!(LIMIT_READONLY_EVIDENCE_FIELD_BYTES)
    );
    assert_eq!(schema["additionalProperties"], json!(false));
}

#[test]
fn every_contract_definition_is_reachable_and_renderable() {
    // A reference to a definition that does not exist panics in the renderer, and
    // a definition nobody references is dead contract surface. Walk the graph
    // from each role's top-level fields and require it to be exactly closed.
    let mut referenced = std::collections::BTreeSet::new();
    let mut pending: Vec<&'static str> = CONTRACTS
        .iter()
        .flat_map(|contract| contract.response.iter())
        .filter_map(|field| field.def)
        .collect();
    while let Some(name) = pending.pop() {
        if !referenced.insert(name) {
            continue;
        }
        let def = contract::DEFINITIONS
            .iter()
            .find(|def| def.name == name)
            .unwrap_or_else(|| panic!("field references unknown definition {name}"));
        match def.body {
            contract::DefBody::Object(fields) => {
                pending.extend(fields.iter().filter_map(|field| field.def));
            }
            contract::DefBody::Union(branches) => {
                for branch in branches {
                    pending.extend(branch.iter().filter_map(|field| field.def));
                }
            }
        }
    }
    for def in contract::DEFINITIONS {
        assert!(
            referenced.contains(def.name),
            "definition {} is not reachable from any role contract",
            def.name
        );
    }
    // Rendering must not panic, and every role must render a distinct schema.
    let all = contract::all_schemas();
    assert_eq!(all.len(), 5);
    let distinct: std::collections::BTreeSet<String> =
        all.values().map(|schema| schema.to_string()).collect();
    assert_eq!(distinct.len(), 5);
}

#[test]
fn planner_sample_survives_a_map_key_roundtrip() {
    // Guard against a schema that only validates because of key ordering.
    let sample = planner_sample();
    let mut reordered = Map::new();
    for key in sample.as_object().unwrap().keys().rev() {
        reordered.insert(key.clone(), sample[key].clone());
    }
    let reordered = Value::Object(reordered);
    assert_eq!(sample, reordered);
    assert_schema_accepts("PLANNER", &reordered);
}

// ---------------------------------------------------------------------------
// 10. H5-A round 2 (R2-6): the Replanner's task refs are typed, not strings.
// ---------------------------------------------------------------------------

#[test]
fn replanner_task_dependencies_are_published_as_typed_refs() {
    // The Planner's dependencies are proposal-id strings; the Replanner's are
    // `{"ref_kind": ...}` objects. `add_tasks` must publish the Replanner shape,
    // or a client that follows the contract produces an unparseable proposal.
    let mut typed = replanner_sample();
    typed["add_tasks"][0]["dependencies"] = json!([
        {"ref_kind": "NEW", "proposal_id": "probe"}
    ]);
    assert_schema_accepts("REPLANNER", &typed);
    assert!(
        crate::replanner::response_shape_accepts(&typed),
        "the typed dependency form is what the real proposal deserializes"
    );

    let mut strings = replanner_sample();
    strings["add_tasks"][0]["dependencies"] = json!(["probe"]);
    assert_schema_refuses("REPLANNER", &strings);
    assert!(
        !crate::replanner::response_shape_accepts(&strings),
        "a plain-string Replanner dependency is not a valid task ref"
    );

    // The Planner still publishes strings, and its own validator agrees.
    let mut planner_typed = planner_sample();
    planner_typed["tasks"][0]["dependencies"] = json!([{"ref_kind": "NEW", "proposal_id": "x"}]);
    assert_schema_refuses("PLANNER", &planner_typed);
    assert!(!crate::planner::response_shape_accepts(&planner_typed));
}

#[test]
fn serde_defaulted_replanner_fields_are_optional_in_the_contract() {
    // Every additive array is `#[serde(default)]`, so a minimal replan that only
    // adds Tasks must be contract-valid.
    let minimal = json!({
        "goal_id": "00000000-0000-4000-8000-000000000001",
        "base_goal_revision": 11,
        "base_plan_revision": 2,
        "summary": "one additive prerequisite",
        "add_tasks": [plan_task("probe", "CODEX_READONLY", "READ_ONLY", "SAFE_READ_ONLY")]
    });
    assert_schema_accepts("REPLANNER", &minimal);
    assert!(
        crate::replanner::response_shape_accepts(&minimal),
        "a replan that omits the defaulted arrays must be accepted"
    );

    // A Planner task may also omit `dependencies` and the scope's
    // `forbidden_paths`, both of which are serde-defaulted.
    let mut minimal_plan = planner_sample();
    minimal_plan["tasks"][0]
        .as_object_mut()
        .unwrap()
        .remove("dependencies");
    minimal_plan["tasks"][0]["scope"]
        .as_object_mut()
        .unwrap()
        .remove("forbidden_paths");
    assert_schema_accepts("PLANNER", &minimal_plan);
    assert!(
        crate::planner::response_shape_accepts(&minimal_plan),
        "serde-defaulted fields must not be required by the published contract"
    );
}
