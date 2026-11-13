//! H5-B — machine-readable response contract for external `ModelWorkItem`s.
//!
//! # The defect this closes (H4-D-F1)
//!
//! `ModelWorkView.response_required_fields` published *field names only*. A real
//! model answered a READONLY WorkItem with `"status": "COMPLETED"`; the host
//! refused it, correctly, because the read-only status vocabulary is
//! `candidate_complete | blocked | needs_replan | failed`. Nothing the client
//! could read said so, so a client could only discover the contract by being
//! rejected.
//!
//! # What this module is
//!
//! One description per role, in one place, that is used for three things:
//!
//! 1. the JSON Schema published on `goal_next` (`response_contract.schema`),
//! 2. the byte/enum limits the *real validators* enforce (they read the
//!    `LIMIT_*` constants below instead of keeping private copies), and
//! 3. the checked-in `docs/schemas/external/*.json` artifacts, which a test
//!    asserts are byte-identical to what this module renders.
//!
//! That is what keeps the published contract and the host validator from
//! drifting apart: they are not two descriptions that a test hopes still agree,
//! they are one description rendered twice.
//!
//! # What this module is *not*
//!
//! It is not authority. A schema-valid response is still untrusted model output:
//! `goal_submit` re-validates identity, revision, preimage, and scope against the
//! durable Goal, and a role validator refusal is final for that submission.
//!
//! # Compatibility
//!
//! Publishing the contract is **additive**: the durable `ModelWorkItem` keeps its
//! existing `output_contract` string, its `request_digest`, and its identity
//! fields, so a WorkItem persisted before this change still reloads and is bound
//! to the same contract version by `output_contract` alone. Nothing here is
//! parsed back out of MCP input.
//!
//! # Why the schema travels inline
//!
//! Considered: (a) a static `docs/schemas/external/*.json` lookup, (b) the full
//! schema embedded in the WorkItem request payload, (c) only a schema id/digest
//! on the WorkItem.
//!
//! * (b) is rejected: the request payload is part of `request_digest`, so adding
//!   a schema would change the identity of every already-issued WorkItem.
//! * (c) is rejected for a pure MCP client: there is no MCP tool that serves the
//!   schema, and a model has no host filesystem access, so an id it cannot
//!   dereference is not a contract.
//! * (a) alone is rejected for the same reason, but it is kept *in addition* so
//!   the contract is reviewable in Git and diffable in code review.
//!
//! So the projection carries the whole schema inline (`response_contract`), plus
//! a `schema_digest` so a client can tell whether two servers agree, plus the
//! checked-in artifacts for humans and CI.

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::sync::OnceLock;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::external_model_work::WorkRole;
use crate::external_model_work::{
    OUTPUT_CONTRACT_PLANNER_V1, OUTPUT_CONTRACT_READONLY_V1, OUTPUT_CONTRACT_REPLANNER_V1,
    OUTPUT_CONTRACT_REVIEWER_V1, OUTPUT_CONTRACT_WRITER_V1,
};
#[cfg(test)]
use crate::task::{ReplaySafety, TaskOperationKind, VerificationSpec, WorkerKind};

/// Contract generation. Bumped only when the *published* contract changes shape.
pub(crate) const RESPONSE_CONTRACT_VERSION: u32 = 2;

/// Stable contract id embedded in every projection and in the artifact index.
pub(crate) const RESPONSE_CONTRACT_ID: &str = "goallatch.external.response.v2";

/// Every string bound in this contract counts **UTF-8 bytes**, because that is
/// what the host validators measure (`str::len`). JSON Schema `maxLength` counts
/// characters, so the schemas also carry `x-max-bytes`, which is authoritative.
pub(crate) const BYTE_SEMANTICS: &str = "UTF8_BYTES";

// ---------------------------------------------------------------------------
// Single-sourced numeric limits.
//
// The planners / writer / readonly validators import these instead of keeping a
// private copy, so a schema that says 8192 and a validator that enforces 8192 is
// a compile-time fact rather than a review hope.
// ---------------------------------------------------------------------------

pub(crate) const LIMIT_PLAN_PROPOSAL_BYTES: usize = 256 * 1024;
pub(crate) const LIMIT_PLAN_SUMMARY_BYTES: usize = 8 * 1024;
pub(crate) const LIMIT_PLAN_TITLE_BYTES: usize = 256;
pub(crate) const LIMIT_PLAN_OBJECTIVE_BYTES: usize = 16 * 1024;
pub(crate) const LIMIT_PLAN_TASKS: usize = 128;
pub(crate) const LIMIT_PLAN_DEPENDENCY_EDGES: usize = 1024;
pub(crate) const LIMIT_PROPOSAL_ID_BYTES: usize = 64;
pub(crate) const LIMIT_DEPENDENCIES_PER_TASK: usize = 64;
pub(crate) const LIMIT_SCOPE_PATHS_PER_KIND: usize = 64;
pub(crate) const LIMIT_SCOPE_PATHS_TOTAL: usize = 1024;
pub(crate) const LIMIT_VERIFICATION_PER_TASK: usize = 32;
pub(crate) const LIMIT_VERIFICATION_TOTAL: usize = 1024;
pub(crate) const LIMIT_PATH_BYTES: usize = 4096;
pub(crate) const LIMIT_EVIDENCE_REQUIREMENT_ID_BYTES: usize = 128;

pub(crate) const LIMIT_READONLY_RESULT_BYTES: usize = 256 * 1024;
pub(crate) const LIMIT_READONLY_SUMMARY_BYTES: usize = 16 * 1024;
pub(crate) const LIMIT_READONLY_EVIDENCE_ITEMS: usize = 32;
pub(crate) const LIMIT_READONLY_EVIDENCE_FIELD_BYTES: usize = 8 * 1024;
pub(crate) const LIMIT_READONLY_EVIDENCE_TOTAL_BYTES: usize = 64 * 1024;

pub(crate) const LIMIT_WRITER_RESULT_BYTES: usize = 1024 * 1024;
pub(crate) const LIMIT_WRITER_OPERATIONS: usize = 32;
pub(crate) const LIMIT_WRITER_PATH_BYTES: usize = 4096;
pub(crate) const LIMIT_WRITE_CONTENT_BYTES: usize = 256 * 1024;
pub(crate) const LIMIT_TOTAL_WRITE_CONTENT_BYTES: usize = 512 * 1024;
pub(crate) const LIMIT_WRITER_SUMMARY_BYTES: usize = 8 * 1024;
pub(crate) const LIMIT_WORKER_EVIDENCE_ITEMS: usize = 64;
pub(crate) const LIMIT_WORKER_EVIDENCE_KIND_BYTES: usize = 128;
pub(crate) const LIMIT_WORKER_EVIDENCE_VALUE_BYTES: usize = 16 * 1024;
pub(crate) const LIMIT_WORKER_EVIDENCE_TOTAL_BYTES: usize = 64 * 1024;
pub(crate) const LIMIT_REVIEW_RESULT_BYTES: usize = 128 * 1024;

/// SHA-256 hex is exactly 64 lowercase hex characters.
pub(crate) const LIMIT_SHA256_HEX: usize = 64;

// ---------------------------------------------------------------------------
// Wire vocabularies.
//
// Each list is asserted equal to the serde wire rendering of the corresponding
// Rust enum (`enum_vocabularies_match_serde`), so a renamed variant cannot leave
// the published contract stale.
// ---------------------------------------------------------------------------

pub(crate) const READONLY_STATUS_VALUES: &[&str] =
    &["candidate_complete", "blocked", "needs_replan", "failed"];

pub(crate) const REPLANNER_RESOLVE_NOTE: &str = "resolve_needs_replan is a JSON array of existing Task id strings, never a boolean; each listed Task must also gain a new hard dependency in this proposal's add_dependencies";

/// The exact pairing `replanner.rs` enforces when it applies a resolution.
///
/// H5.2-C: a real model repeatedly answered `resolve_needs_replan` without the
/// matching `add_dependencies` edge. The host was right to refuse it, but the
/// published contract did not say so. The wording below is derived from, and
/// must stay equivalent to, the `dependency_targets` check in
/// `src/replanner.rs::validate`.
pub(crate) const REPLANNER_RESOLVE_DEPENDENCY_RULE: &str = "each id in resolve_needs_replan must be the `task` of an add_dependencies entry in the same proposal, so the blocked Task gains a new hard dependency; the `dependency` is that Task's new prerequisite, which is normally a repair Task added by this same proposal (`ref_kind` `NEW`), and an existing Task (`ref_kind` `EXISTING`) is also accepted; a resolution without such an edge is refused as REPLANNER_SCHEMA_VIOLATION";

/// A short, valid JSON fragment of the resolution relationship.
///
/// It is a fragment on purpose: the identity echo is published by
/// `REPLANNER_BASE_REVISION_ECHO` and the full repair Task object by the
/// `replanner_task` definition. `the_published_resolve_needs_replan_shape_is_accepted`
/// in `src/replanner.rs` proves that a complete proposal carrying exactly this
/// relationship is accepted, so the published shape is not aspirational.
pub(crate) const REPLANNER_RESOLVE_EXAMPLE: &str = r#"resolve a NEEDS_REPLAN Task by adding the repair Task and pointing the blocked Task at it in the same proposal (a fragment of the full replanner object, not a complete answer): {"add_dependencies":[{"task":{"ref_kind":"EXISTING","task_id":"<blocked-task-id>"},"dependency":{"ref_kind":"NEW","proposal_id":"<new-repair-proposal_id>"}}],"resolve_needs_replan":["<blocked-task-id>"]}"#;

// ---------------------------------------------------------------------------
// Field contract.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FieldKind {
    Text,
    Integer,
    Boolean,
    /// Array whose items are a named definition.
    ArrayOf,
    /// Array of plain strings.
    TextArray,
    /// Object whose shape is a named definition.
    Object,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Field {
    pub(crate) name: &'static str,
    pub(crate) kind: FieldKind,
    pub(crate) required: bool,
    /// Closed value set for a text field (rendered as `enum`).
    pub(crate) values: &'static [&'static str],
    pub(crate) min_items: usize,
    pub(crate) max_items: usize,
    /// UTF-8 byte bound for text fields. `0` means "no bound published".
    pub(crate) max_bytes: usize,
    /// Named definition for `ArrayOf` / `Object`.
    pub(crate) def: Option<&'static str>,
    /// The host issued this value and the response must echo it verbatim.
    pub(crate) echo: bool,
    pub(crate) note: &'static str,
}

impl Default for Field {
    fn default() -> Self {
        Self {
            name: "",
            kind: FieldKind::Text,
            required: true,
            values: &[],
            min_items: 0,
            max_items: 0,
            max_bytes: 0,
            def: None,
            echo: false,
            note: "",
        }
    }
}

const fn text(name: &'static str) -> Field {
    Field {
        name,
        kind: FieldKind::Text,
        ..Field::DEFAULT
    }
}

const fn integer(name: &'static str) -> Field {
    Field {
        name,
        kind: FieldKind::Integer,
        ..Field::DEFAULT
    }
}

const fn boolean(name: &'static str) -> Field {
    Field {
        name,
        kind: FieldKind::Boolean,
        ..Field::DEFAULT
    }
}

const fn text_array(name: &'static str) -> Field {
    Field {
        name,
        kind: FieldKind::TextArray,
        ..Field::DEFAULT
    }
}

const fn array_of(name: &'static str, def: &'static str) -> Field {
    Field {
        name,
        kind: FieldKind::ArrayOf,
        def: Some(def),
        ..Field::DEFAULT
    }
}

const fn object_of(name: &'static str, def: &'static str) -> Field {
    Field {
        name,
        kind: FieldKind::Object,
        def: Some(def),
        ..Field::DEFAULT
    }
}

impl Field {
    const DEFAULT: Self = Self {
        name: "",
        kind: FieldKind::Text,
        required: true,
        values: &[],
        min_items: 0,
        max_items: 0,
        max_bytes: 0,
        def: None,
        echo: false,
        note: "",
    };

    const fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    const fn one_of(mut self, values: &'static [&'static str]) -> Self {
        self.values = values;
        self
    }

    const fn max_bytes(mut self, value: usize) -> Self {
        self.max_bytes = value;
        self
    }

    const fn item_bounds(mut self, min: usize, max: usize) -> Self {
        self.min_items = min;
        self.max_items = max;
        self
    }

    const fn echo(mut self) -> Self {
        self.echo = true;
        self
    }

    const fn note(mut self, note: &'static str) -> Self {
        self.note = note;
        self
    }
}

// ---------------------------------------------------------------------------
// Named definitions.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub(crate) enum DefBody {
    Object(&'static [Field]),
    /// `oneOf` over tagged object shapes. Each branch carries its discriminator.
    Union(&'static [&'static [Field]]),
}

#[derive(Clone, Copy)]
pub(crate) struct Def {
    pub(crate) name: &'static str,
    pub(crate) body: DefBody,
}

const WORKER_KIND_VALUES: &[&str] = &["CODEX_READONLY", "CODEX_WRITER"];

const WORKER_KIND_NOTE: &str = "V1 accepts only the production-plannable worker kinds; LOCAL_OPERATION, CODEX_REVIEWER and VERIFIER are refused by the host validator";

const OPERATION_KIND_VALUES: &[&str] = &["READ_ONLY", "LOCAL_MUTATION", "HOST_NATIVE_APPROVED"];

const REPLAY_SAFETY_VALUES: &[&str] = &["SAFE_READ_ONLY", "VERIFY_BEFORE_RETRY", "NEVER_AUTOMATIC"];

const EVIDENCE_FIELDS_READONLY: &[Field] = &[
    text("kind")
        .max_bytes(LIMIT_READONLY_EVIDENCE_FIELD_BYTES)
        .note(
            "non-empty. The host turns each evidence item into a structured observation keyed by THIS string, so for every request.verification entry of kind STRUCTURED_EVIDENCE this must be exactly that entry's requirement_id",
        ),
    text("value")
        .max_bytes(LIMIT_READONLY_EVIDENCE_FIELD_BYTES)
        .note("non-empty; the host stores this verbatim as the observation detail"),
];

const EVIDENCE_FIELDS_WORKER: &[Field] = &[
    text("kind").max_bytes(LIMIT_WORKER_EVIDENCE_KIND_BYTES),
    text("value").max_bytes(LIMIT_WORKER_EVIDENCE_VALUE_BYTES),
];

const TAG_KIND: Field = text("kind").note("discriminator");

const FILE_EXISTS_FIELDS: &[Field] = &[
    TAG_KIND.one_of(&["FILE_EXISTS"]),
    text("path").max_bytes(LIMIT_PATH_BYTES),
    boolean("must_be_file"),
];

const FILE_DIGEST_FIELDS: &[Field] = &[
    TAG_KIND.one_of(&["FILE_DIGEST"]),
    text("path").max_bytes(LIMIT_PATH_BYTES),
    text("expected_sha256").max_bytes(LIMIT_SHA256_HEX),
];

const GIT_SCOPE_FIELDS: &[Field] = &[
    TAG_KIND.one_of(&["GIT_SCOPE"]),
    text_array("allowed_changed_paths"),
    boolean("require_no_other_changes"),
];

const NO_FORBIDDEN_CHANGES_FIELDS: &[Field] = &[
    TAG_KIND.one_of(&["NO_FORBIDDEN_CHANGES"]),
    text_array("forbidden_paths"),
];

const STRUCTURED_EVIDENCE_FIELDS: &[Field] = &[
    TAG_KIND.one_of(&["STRUCTURED_EVIDENCE"]),
    text("requirement_id").max_bytes(LIMIT_EVIDENCE_REQUIREMENT_ID_BYTES),
];

const REVIEW_GATE_FIELDS: &[Field] = &[
    TAG_KIND.one_of(&["REVIEW_GATE"]),
    integer("max_blocking_findings"),
];

const VERIFICATION_SPEC_BRANCHES: &[&[Field]] = &[
    // COMMAND_EXIT exists in the serde enum but the shared verifier refuses it
    // unconditionally ("COMMAND_EXIT verification is currently unsupported"), so
    // it is deliberately NOT published: a contract that advertises a form the
    // host always refuses is worse than one that omits it.
    // COMMAND_EXIT_FIELDS,
    FILE_EXISTS_FIELDS,
    FILE_DIGEST_FIELDS,
    GIT_SCOPE_FIELDS,
    NO_FORBIDDEN_CHANGES_FIELDS,
    STRUCTURED_EVIDENCE_FIELDS,
    REVIEW_GATE_FIELDS,
];

const TASK_SCOPE_FIELDS: &[Field] = &[
    text_array("allowed_paths").item_bounds(1, LIMIT_SCOPE_PATHS_PER_KIND),
    text_array("forbidden_paths")
        .item_bounds(0, LIMIT_SCOPE_PATHS_PER_KIND)
        .optional(),
    text("operation_kind").one_of(OPERATION_KIND_VALUES),
    text("replay_safety").one_of(REPLAY_SAFETY_VALUES),
];

const PLAN_TASK_FIELDS: &[Field] = &[
    text("proposal_id").max_bytes(LIMIT_PROPOSAL_ID_BYTES),
    text("title").max_bytes(LIMIT_PLAN_TITLE_BYTES),
    text("objective").max_bytes(LIMIT_PLAN_OBJECTIVE_BYTES),
    boolean("mandatory"),
    text("worker")
        .one_of(WORKER_KIND_VALUES)
        .note(WORKER_KIND_NOTE),
    text_array("dependencies")
        .max_bytes(LIMIT_PROPOSAL_ID_BYTES)
        .item_bounds(0, LIMIT_DEPENDENCIES_PER_TASK)
        .optional(),
    object_of("scope", "task_scope"),
    array_of("verification", "verification_spec").item_bounds(1, LIMIT_VERIFICATION_PER_TASK),
];

/// Replanner task shape: identical to the Planner's except that `dependencies`
/// are tagged task refs (`{"ref_kind":"EXISTING"|"NEW", ...}`), which is what
/// `ReplanTaskProposal` actually deserializes.
const REPLAN_TASK_FIELDS: &[Field] = &[
    text("proposal_id").max_bytes(LIMIT_PROPOSAL_ID_BYTES),
    text("title").max_bytes(LIMIT_PLAN_TITLE_BYTES),
    text("objective").max_bytes(LIMIT_PLAN_OBJECTIVE_BYTES),
    boolean("mandatory"),
    text("worker")
        .one_of(WORKER_KIND_VALUES)
        .note(WORKER_KIND_NOTE),
    array_of("dependencies", "task_ref")
        .item_bounds(0, LIMIT_DEPENDENCIES_PER_TASK)
        .optional(),
    object_of("scope", "task_scope"),
    array_of("verification", "verification_spec").item_bounds(1, LIMIT_VERIFICATION_PER_TASK),
];

const PLAN_CRITERION_BINDING_FIELDS: &[Field] = &[
    text("criterion_id")
        .note("must be an exact host CompletionCriterionId from request.completion_criteria"),
    text_array("task_refs").item_bounds(1, LIMIT_PLAN_TASKS),
];

const TASK_REF_EXISTING_FIELDS: &[Field] = &[
    text("ref_kind").one_of(&["EXISTING"]),
    text("task_id").note("an exact existing durable Task UUID"),
];

const TASK_REF_NEW_FIELDS: &[Field] = &[
    text("ref_kind").one_of(&["NEW"]),
    text("proposal_id").max_bytes(LIMIT_PROPOSAL_ID_BYTES),
];

const TASK_REF_BRANCHES: &[&[Field]] = &[TASK_REF_EXISTING_FIELDS, TASK_REF_NEW_FIELDS];

const DEPENDENCY_ADDITION_FIELDS: &[Field] = &[
    object_of("task", "task_ref"),
    object_of("dependency", "task_ref"),
];

const VERIFICATION_STRENGTHENING_FIELDS: &[Field] = &[
    text("task_id"),
    array_of("add", "verification_spec").item_bounds(1, LIMIT_VERIFICATION_PER_TASK),
];

const MANDATORY_STRENGTHENING_FIELDS: &[Field] = &[text("task_id")];

const CRITERION_BINDING_STRENGTHENING_FIELDS: &[Field] = &[
    text("criterion_id"),
    array_of("add_task_refs", "task_ref").item_bounds(1, LIMIT_PLAN_TASKS),
];

const CRITERION_REPLACEMENT_FIELDS: &[Field] = &[
    text("criterion_id"),
    array_of("replacement_task_refs", "task_ref").item_bounds(1, LIMIT_PLAN_TASKS),
];

const PRISTINE_PLAN_SUPERSESSION_FIELDS: &[Field] = &[
    text("rejection_request_id"),
    array_of("criterion_rebindings", "criterion_replacement")
        .item_bounds(0, LIMIT_PLAN_DEPENDENCY_EDGES),
];

const TASK_REPLACEMENT_FIELDS: &[Field] = &[
    text("replan_request_id"),
    text("old_task_id"),
    array_of("completion_closure_task_refs", "task_ref").item_bounds(1, LIMIT_PLAN_TASKS),
    array_of("criterion_rebindings", "criterion_replacement")
        .item_bounds(0, LIMIT_PLAN_DEPENDENCY_EDGES),
];

const EXPECTED_PREIMAGE_SHA256_FIELDS: &[Field] = &[
    text("kind").one_of(&["SHA256"]),
    text("sha256").max_bytes(LIMIT_SHA256_HEX),
];

const EXPECTED_PREIMAGE_ABSENT_FIELDS: &[Field] = &[text("kind").one_of(&["ABSENT"])];

const EXPECTED_PREIMAGE_BRANCHES: &[&[Field]] = &[
    EXPECTED_PREIMAGE_SHA256_FIELDS,
    EXPECTED_PREIMAGE_ABSENT_FIELDS,
];

const WRITE_UTF8_FIELDS: &[Field] = &[
    text("kind").one_of(&["WRITE_UTF8"]),
    text("path").max_bytes(LIMIT_WRITER_PATH_BYTES),
    object_of("expected_preimage", "expected_preimage"),
    text("content").max_bytes(LIMIT_WRITE_CONTENT_BYTES),
];

const WRITER_OPERATION_BRANCHES: &[&[Field]] = &[WRITE_UTF8_FIELDS];

pub(crate) const DEFINITIONS: &[Def] = &[
    Def {
        name: "evidence_item_readonly",
        body: DefBody::Object(EVIDENCE_FIELDS_READONLY),
    },
    Def {
        name: "evidence_item_worker",
        body: DefBody::Object(EVIDENCE_FIELDS_WORKER),
    },
    Def {
        name: "verification_spec",
        body: DefBody::Union(VERIFICATION_SPEC_BRANCHES),
    },
    Def {
        name: "task_scope",
        body: DefBody::Object(TASK_SCOPE_FIELDS),
    },
    Def {
        name: "plan_task",
        body: DefBody::Object(PLAN_TASK_FIELDS),
    },
    Def {
        name: "replan_task",
        body: DefBody::Object(REPLAN_TASK_FIELDS),
    },
    Def {
        name: "plan_criterion_binding",
        body: DefBody::Object(PLAN_CRITERION_BINDING_FIELDS),
    },
    Def {
        name: "task_ref",
        body: DefBody::Union(TASK_REF_BRANCHES),
    },
    Def {
        name: "dependency_addition",
        body: DefBody::Object(DEPENDENCY_ADDITION_FIELDS),
    },
    Def {
        name: "verification_strengthening",
        body: DefBody::Object(VERIFICATION_STRENGTHENING_FIELDS),
    },
    Def {
        name: "mandatory_strengthening",
        body: DefBody::Object(MANDATORY_STRENGTHENING_FIELDS),
    },
    Def {
        name: "criterion_binding_strengthening",
        body: DefBody::Object(CRITERION_BINDING_STRENGTHENING_FIELDS),
    },
    Def {
        name: "criterion_replacement",
        body: DefBody::Object(CRITERION_REPLACEMENT_FIELDS),
    },
    Def {
        name: "pristine_plan_supersession",
        body: DefBody::Object(PRISTINE_PLAN_SUPERSESSION_FIELDS),
    },
    Def {
        name: "task_replacement",
        body: DefBody::Object(TASK_REPLACEMENT_FIELDS),
    },
    Def {
        name: "expected_preimage",
        body: DefBody::Union(EXPECTED_PREIMAGE_BRANCHES),
    },
    Def {
        name: "writer_operation",
        body: DefBody::Union(WRITER_OPERATION_BRANCHES),
    },
];

// ---------------------------------------------------------------------------
// Conditions the host enforces beyond per-field shape.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub(crate) struct Condition {
    pub(crate) code: &'static str,
    pub(crate) detail: &'static str,
}

const PLANNER_CONDITIONS: &[Condition] = &[
    Condition {
        code: "PLANNER_IDENTITY_ECHO",
        detail: "goal_id and goal_revision must equal the values the host issued for this WorkItem",
    },
    Condition {
        code: "PLANNER_PROPOSAL_ID_UNIQUE",
        detail: "proposal_id values must be unique within the plan and must not be durable Task ids",
    },
    Condition {
        code: "PLANNER_REQUIRED_CRITERIA_COVERED",
        detail: "every required request completion criterion must appear exactly once in criterion_bindings",
    },
    Condition {
        code: "PLANNER_DEPENDENCY_ACYCLIC",
        detail: "dependencies must reference proposal ids in this plan, must not self-reference, and must be acyclic",
    },
    Condition {
        code: "PLANNER_SCOPE_WITHIN_AUTHORITY",
        detail: "allowed_paths must stay inside the Goal execution root; LOCAL_MUTATION/HOST_NATIVE_APPROVED require VERIFY_BEFORE_RETRY or NEVER_AUTOMATIC, READ_ONLY requires SAFE_READ_ONLY",
    },
    Condition {
        code: "PLANNER_MANDATORY_PRESENT",
        detail: "at least one Task must be mandatory and no mandatory Task may depend on an optional Task",
    },
    Condition {
        code: "PLANNER_VERIFICATION_NON_EMPTY",
        detail: "each Task requires 1..=32 verification specifications",
    },
];

const READONLY_CONDITIONS: &[Condition] = &[
    Condition {
        code: "READONLY_EVIDENCE_KIND_IS_THE_REQUIREMENT_ID",
        detail: "the host records each evidence item as a structured observation whose requirement_id is the item's `kind`; every request.verification entry of kind STRUCTURED_EVIDENCE must therefore appear as an evidence item with `kind` equal to that entry's requirement_id, or the Task Verifier reports the requirement as missing and the Task is blocked",
    },
    Condition {
        code: "READONLY_IDENTITY_ECHO",
        detail: "goal_id, task_id, attempt_id, goal_revision and plan_revision must equal the host-issued WorkItem identity; a mismatch is READONLY_IDENTITY_MISMATCH",
    },
    Condition {
        code: "READONLY_STATUS_VOCABULARY",
        detail: "status is exactly one of candidate_complete, blocked, needs_replan, failed; the host never accepts a model-declared completion such as COMPLETED",
    },
    Condition {
        code: "READONLY_EVIDENCE_TOTAL_BOUND",
        detail: "the sum of kind plus value bytes across all evidence items must not exceed the host total bound",
    },
    Condition {
        code: "READONLY_NO_WRITE_OPERATIONS",
        detail: "an object carrying proposed_operations or any write field is refused: the read-only role has no write authority",
    },
];

const WRITER_CONDITIONS: &[Condition] = &[
    Condition {
        code: "WRITER_IDENTITY_ECHO",
        detail: "goal_id, task_id, attempt_id, goal_revision and plan_revision must equal the host-issued WorkItem identity",
    },
    Condition {
        code: "WRITER_STATUS_VOCABULARY",
        detail: "status is exactly one of candidate_complete, blocked, needs_replan, failed",
    },
    Condition {
        code: "WRITER_CANDIDATE_COMPLETE_REQUIRES_OPERATIONS",
        detail: "status candidate_complete requires 1..=32 proposed_operations; any other status requires 0",
    },
    Condition {
        code: "WRITER_PREIMAGE_EXPECTATION",
        detail: "expected_preimage is either {kind:SHA256, sha256:<64 lowercase hex>} or {kind:ABSENT}; a mismatched preimage is refused before any write",
    },
    Condition {
        code: "WRITER_OPERATION_BOUNDS",
        detail: "paths must stay inside the granted write scope, single-file content is bounded, and total write content is bounded",
    },
    Condition {
        code: "WRITER_NO_SELF_AUTHORITY",
        detail: "the model never authorizes itself: the host checks the Owner grant, scope and preimage independently of this response",
    },
];

const REVIEWER_CONDITIONS: &[Condition] = &[
    Condition {
        code: "REVIEWER_IDENTITY_ECHO",
        detail: "goal_id, task_id, attempt_id, goal_revision and plan_revision must equal the host-issued WorkItem identity",
    },
    Condition {
        code: "REVIEWER_FINDINGS_ARE_COUNTED_NOT_CLAIMED",
        detail: "blocking_findings is a non-negative integer count; the host decides whether the review gate passes",
    },
    Condition {
        code: "REVIEWER_CANNOT_COMPLETE",
        detail: "a reviewer response never completes a Task or a Goal; completion needs the host Verifier and Finalizer",
    },
];

const REPLANNER_CONDITIONS: &[Condition] = &[
    Condition {
        code: "REPLANNER_BASE_REVISION_ECHO",
        detail: "goal_id, base_goal_revision and base_plan_revision must equal the values the host issued",
    },
    Condition {
        code: "REPLANNER_ADDITIVE_ONLY",
        detail: "ordinary replanning uses only the additive fields; history is never rewritten and authority is never weakened",
    },
    Condition {
        code: "REPLANNER_RESOLVE_NEEDS_REPLAN_ARRAY",
        detail: REPLANNER_RESOLVE_NOTE,
    },
    Condition {
        code: "REPLANNER_RESOLVE_REQUIRES_NEW_DEPENDENCY",
        detail: REPLANNER_RESOLVE_DEPENDENCY_RULE,
    },
    Condition {
        code: "REPLANNER_EXCLUSIVE_PRIMITIVES",
        detail: "pristine_plan_supersession and replace_tasks may not be combined with the additive fields, with each other, or with a scope wider than the Task being replaced",
    },
    Condition {
        code: "REPLANNER_READONLY_REASSESSMENT_BARRIER",
        detail: "when the host recorded a pre-execution plan rejection with the REQUIRE_READONLY_REASSESSMENT policy and the rejected trigger is resolved, the chain must contain a new READ_ONLY Task, and every new mutating Task in the proposal must depend on that new READ_ONLY Task",
    },
    Condition {
        code: "REPLANNER_HOST_REVALIDATION",
        detail: "the host revalidates every proposal against the complete durable Goal; a rejection changes no plan revision, no history and no file",
    },
];

// ---------------------------------------------------------------------------
// Role contracts.
// ---------------------------------------------------------------------------

pub(crate) struct ResponseContract {
    pub(crate) output_contract: &'static str,
    pub(crate) role: &'static str,
    pub(crate) response: &'static [Field],
    pub(crate) conditions: &'static [Condition],
    pub(crate) notes: &'static [&'static str],
    /// Byte bound on the whole serialized response, enforced before parsing.
    pub(crate) max_response_bytes: usize,
}

const PLANNER_FIELDS: &[Field] = &[
    text("goal_id").echo(),
    integer("goal_revision").echo(),
    text("summary").max_bytes(LIMIT_PLAN_SUMMARY_BYTES),
    array_of("tasks", "plan_task").item_bounds(1, LIMIT_PLAN_TASKS),
    array_of("criterion_bindings", "plan_criterion_binding").item_bounds(0, LIMIT_PLAN_TASKS),
];

const READONLY_FIELDS: &[Field] = &[
    text("goal_id").echo(),
    text("task_id").echo(),
    text("attempt_id").echo(),
    integer("goal_revision").echo(),
    integer("plan_revision").echo(),
    text("status").one_of(READONLY_STATUS_VALUES),
    text("summary").max_bytes(LIMIT_READONLY_SUMMARY_BYTES),
    array_of("evidence", "evidence_item_readonly").item_bounds(0, LIMIT_READONLY_EVIDENCE_ITEMS),
];

const WRITER_FIELDS: &[Field] = &[
    text("goal_id").echo(),
    text("task_id").echo(),
    text("attempt_id").echo(),
    integer("goal_revision").echo(),
    integer("plan_revision").echo(),
    text("status").one_of(READONLY_STATUS_VALUES),
    text("summary").max_bytes(LIMIT_WRITER_SUMMARY_BYTES),
    array_of("evidence", "evidence_item_worker").item_bounds(0, LIMIT_WORKER_EVIDENCE_ITEMS),
    array_of("proposed_operations", "writer_operation").item_bounds(0, LIMIT_WRITER_OPERATIONS),
];

const REVIEWER_FIELDS: &[Field] = &[
    text("goal_id").echo(),
    text("task_id").echo(),
    text("attempt_id").echo(),
    integer("goal_revision").echo(),
    integer("plan_revision").echo(),
    text("summary").max_bytes(LIMIT_WRITER_SUMMARY_BYTES),
    integer("blocking_findings"),
    array_of("evidence", "evidence_item_worker").item_bounds(0, LIMIT_WORKER_EVIDENCE_ITEMS),
];

const REPLANNER_FIELDS: &[Field] = &[
    text("goal_id").echo(),
    integer("base_goal_revision").echo(),
    integer("base_plan_revision").echo(),
    text("summary").max_bytes(LIMIT_PLAN_SUMMARY_BYTES),
    array_of("add_tasks", "replan_task")
        .item_bounds(0, LIMIT_PLAN_TASKS)
        .optional(),
    array_of("add_dependencies", "dependency_addition")
        .item_bounds(0, LIMIT_PLAN_DEPENDENCY_EDGES)
        .optional(),
    array_of("strengthen_verification", "verification_strengthening")
        .item_bounds(0, LIMIT_VERIFICATION_TOTAL)
        .optional(),
    array_of("strengthen_mandatory", "mandatory_strengthening")
        .item_bounds(0, LIMIT_VERIFICATION_TOTAL)
        .optional(),
    array_of(
        "strengthen_criterion_bindings",
        "criterion_binding_strengthening",
    )
    .item_bounds(0, LIMIT_VERIFICATION_TOTAL)
    .optional(),
    text_array("resolve_needs_replan")
        .item_bounds(0, LIMIT_VERIFICATION_TOTAL)
        .note(REPLANNER_RESOLVE_NOTE)
        .optional(),
    object_of("pristine_plan_supersession", "pristine_plan_supersession").optional(),
    array_of("replace_tasks", "task_replacement")
        .item_bounds(0, LIMIT_PLAN_TASKS)
        .optional(),
];

const PLANNER_NOTES: &[&str] = &[
    "Return exactly one JSON object. Any additional property is refused.",
    "Echo goal_id and goal_revision from the host-issued WorkItem request.",
    "Never claim that work ran, and never author durable Task ids.",
];

const READONLY_NOTES: &[&str] = &[
    "Return exactly one JSON object. Any additional property is refused.",
    "The read-only role performs no writes and has no write authority.",
    "candidate_complete hands the Task to the host Verifier; it is not a completion claim.",
    "Echo every request.verification STRUCTURED_EVIDENCE requirement_id as an evidence item `kind`, with the observation in `value`. The host keys structured observations by that string.",
];

const WRITER_NOTES: &[&str] = &[
    "Return exactly one JSON object. Any additional property is refused.",
    "The writer role proposes; only the host validates the Owner grant, scope, preimage and postimage.",
    "Never write files, execute mutating commands, or claim Task/Goal completion.",
];

const REVIEWER_NOTES: &[&str] = &[
    "Return exactly one JSON object. Any additional property is refused.",
    "blocking_findings is a count, not a verdict: the host applies the review gate.",
];

const REPLANNER_NOTES: &[&str] = &[
    "Return exactly one JSON object. Any additional property is refused.",
    "Additive replanning is monotonic: add Tasks, dependencies, verification, mandatory flags, criterion bindings, or resolve needs_replan.",
    "Replacement primitives are typed and travel with explicit request ids issued by the host.",
    REPLANNER_RESOLVE_EXAMPLE,
    "A Task whose own shape was already too large, too broad, or outside its budget can never succeed on an unchanged replay: resolving it adds the repair prerequisite but the host will never re-arm that Task, so it can only be finalised through replace_tasks.",
];

pub(crate) const CONTRACTS: &[ResponseContract] = &[
    ResponseContract {
        output_contract: OUTPUT_CONTRACT_PLANNER_V1,
        role: "PLANNER",
        response: PLANNER_FIELDS,
        conditions: PLANNER_CONDITIONS,
        notes: PLANNER_NOTES,
        max_response_bytes: LIMIT_PLAN_PROPOSAL_BYTES,
    },
    ResponseContract {
        output_contract: OUTPUT_CONTRACT_READONLY_V1,
        role: "READONLY",
        response: READONLY_FIELDS,
        conditions: READONLY_CONDITIONS,
        notes: READONLY_NOTES,
        max_response_bytes: LIMIT_READONLY_RESULT_BYTES,
    },
    ResponseContract {
        output_contract: OUTPUT_CONTRACT_WRITER_V1,
        role: "WRITER",
        response: WRITER_FIELDS,
        conditions: WRITER_CONDITIONS,
        notes: WRITER_NOTES,
        max_response_bytes: LIMIT_WRITER_RESULT_BYTES,
    },
    ResponseContract {
        output_contract: OUTPUT_CONTRACT_REVIEWER_V1,
        role: "REVIEWER",
        response: REVIEWER_FIELDS,
        conditions: REVIEWER_CONDITIONS,
        notes: REVIEWER_NOTES,
        max_response_bytes: LIMIT_REVIEW_RESULT_BYTES,
    },
    ResponseContract {
        output_contract: OUTPUT_CONTRACT_REPLANNER_V1,
        role: "REPLANNER",
        response: REPLANNER_FIELDS,
        conditions: REPLANNER_CONDITIONS,
        notes: REPLANNER_NOTES,
        max_response_bytes: LIMIT_PLAN_PROPOSAL_BYTES,
    },
];

/// Contract for a persisted `output_contract` string.
///
/// The durable WorkItem stores only this string, so a WorkItem that was written
/// before this module existed is still bound to the same contract after reload.
/// An unknown string is a contract the host does not know: it maps to `None` and
/// the caller must not invent one.
pub(crate) fn contract_for_output_contract(
    output_contract: &str,
) -> Option<&'static ResponseContract> {
    CONTRACTS
        .iter()
        .find(|contract| contract.output_contract == output_contract)
}

/// Contract for a role, by the same `output_contract` the host pins per role.
pub(crate) fn contract_for_role(role: WorkRole) -> &'static ResponseContract {
    let output_contract = match role {
        WorkRole::Planner => OUTPUT_CONTRACT_PLANNER_V1,
        WorkRole::Readonly => OUTPUT_CONTRACT_READONLY_V1,
        WorkRole::Writer => OUTPUT_CONTRACT_WRITER_V1,
        WorkRole::Reviewer => OUTPUT_CONTRACT_REVIEWER_V1,
        WorkRole::Replanner => OUTPUT_CONTRACT_REPLANNER_V1,
    };
    contract_for_output_contract(output_contract)
        .expect("every WorkRole has a registered response contract")
}

pub(crate) fn required_fields(contract: &ResponseContract) -> Vec<&'static str> {
    contract
        .response
        .iter()
        .filter(|field| field.required)
        .map(|field| field.name)
        .collect()
}

pub(crate) fn optional_fields(contract: &ResponseContract) -> Vec<&'static str> {
    contract
        .response
        .iter()
        .filter(|field| !field.required)
        .map(|field| field.name)
        .collect()
}

pub(crate) fn identity_echo_fields(contract: &ResponseContract) -> Vec<&'static str> {
    contract
        .response
        .iter()
        .filter(|field| field.echo)
        .map(|field| field.name)
        .collect()
}

// ---------------------------------------------------------------------------
// JSON Schema rendering.
//
// Definitions are inlined rather than emitted under `$defs`/`$ref`: a small
// external client (or a hand-written validator) then needs no reference
// resolution, and the schema it reads is the complete contract.
// ---------------------------------------------------------------------------

fn definition(name: &str) -> Option<&'static Def> {
    DEFINITIONS.iter().find(|def| def.name == name)
}

fn field_schema(field: &Field) -> Value {
    let mut object = Map::new();
    match field.kind {
        FieldKind::Text => {
            object.insert("type".to_owned(), json!("string"));
            if !field.values.is_empty() {
                object.insert("enum".to_owned(), json!(field.values));
            }
            if field.max_bytes > 0 {
                object.insert("maxLength".to_owned(), json!(field.max_bytes));
                object.insert("x-max-bytes".to_owned(), json!(field.max_bytes));
            }
        }
        FieldKind::Integer => {
            // The host parses these into u32/u64, so the published bound is the
            // largest value every one of them can hold (H5-A round 2, R2-8).
            object.insert("type".to_owned(), json!("integer"));
            object.insert("minimum".to_owned(), json!(0));
            object.insert("maximum".to_owned(), json!(u32::MAX));
        }
        FieldKind::Boolean => {
            object.insert("type".to_owned(), json!("boolean"));
        }
        FieldKind::TextArray => {
            object.insert("type".to_owned(), json!("array"));
            let mut items = Map::new();
            items.insert("type".to_owned(), json!("string"));
            if field.max_bytes > 0 {
                items.insert("maxLength".to_owned(), json!(field.max_bytes));
                items.insert("x-max-bytes".to_owned(), json!(field.max_bytes));
            }
            object.insert("items".to_owned(), Value::Object(items));
        }
        FieldKind::ArrayOf => {
            object.insert("type".to_owned(), json!("array"));
            object.insert(
                "items".to_owned(),
                definition_schema(field.def.expect("ArrayOf field has a definition")),
            );
        }
        FieldKind::Object => {
            let rendered = definition_schema(field.def.expect("Object field has a definition"));
            if let Value::Object(map) = rendered {
                for (key, value) in map {
                    object.insert(key, value);
                }
            }
        }
    }
    if field.kind != FieldKind::Object {
        if field.min_items > 0 {
            object.insert("minItems".to_owned(), json!(field.min_items));
        }
        if field.max_items > 0 {
            object.insert("maxItems".to_owned(), json!(field.max_items));
        }
    }
    if !field.note.is_empty() {
        object.insert("description".to_owned(), json!(field.note));
    }
    Value::Object(object)
}

fn object_schema(title: &str, fields: &'static [Field]) -> Value {
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for field in fields {
        properties.insert(field.name.to_owned(), field_schema(field));
        if field.required {
            required.push(json!(field.name));
        }
    }
    json!({
        "type": "object",
        "title": title,
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    })
}

fn definition_schema(name: &str) -> Value {
    let def = definition(name).unwrap_or_else(|| panic!("unknown contract definition {name}"));
    match def.body {
        DefBody::Object(fields) => object_schema(name, fields),
        DefBody::Union(branches) => {
            let rendered: Vec<Value> = branches
                .iter()
                .enumerate()
                .map(|(index, branch)| object_schema(&format!("{name}.{index}"), branch))
                .collect();
            json!({ "oneOf": rendered })
        }
    }
}

/// The complete JSON Schema for one role's response object.
pub(crate) fn schema(contract: &ResponseContract) -> Value {
    object_schema(contract.role, contract.response)
}

/// Canonical digest of the rendered schema.
///
/// `serde_json::Map` is a `BTreeMap`, so the serialized form is key-sorted and
/// therefore canonical: the same contract always produces the same digest on
/// every platform and in every process.
pub(crate) fn schema_digest(contract: &ResponseContract) -> String {
    let rendered = schema(contract);
    let bytes = serde_json::to_vec(&rendered).expect("a rendered schema is serializable");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    format!("sha256:{}", hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub(crate) fn contract_id(contract: &ResponseContract) -> String {
    format!(
        "{RESPONSE_CONTRACT_ID}/{}/{}",
        contract.role, contract.output_contract
    )
}

/// Every role's schema, keyed by role name, for the checked-in artifacts.
#[cfg(test)]
pub(crate) fn all_schemas() -> BTreeMap<&'static str, Value> {
    CONTRACTS
        .iter()
        .map(|contract| (contract.role, schema(contract)))
        .collect()
}

/// Serialized artifact bytes for one contract (pretty, newline-terminated).
#[cfg(test)]
pub(crate) fn artifact_bytes(contract: &ResponseContract) -> Vec<u8> {
    let mut bytes =
        serde_json::to_vec_pretty(&schema(contract)).expect("a rendered schema is serializable");
    bytes.push(b'\n');
    bytes
}

/// Serialized artifact bytes for the index.
#[cfg(test)]
pub(crate) fn index_artifact_bytes() -> Vec<u8> {
    let roles: Vec<Value> = CONTRACTS
        .iter()
        .map(|contract| {
            json!({
                "role": contract.role,
                "output_contract": contract.output_contract,
                "contract_id": contract_id(contract),
                "schema_digest": schema_digest(contract),
                "file": format!("{}.v{}.json", contract.role.to_lowercase(), RESPONSE_CONTRACT_VERSION),
                "max_response_bytes": contract.max_response_bytes,
                "identity_echo_fields": identity_echo_fields(contract),
                "required_fields": required_fields(contract),
                "optional_fields": optional_fields(contract),
                "conditions": contract
                    .conditions
                    .iter()
                    .map(|condition| json!({"code": condition.code, "detail": condition.detail}))
                    .collect::<Vec<_>>(),
                "notes": contract.notes,
            })
        })
        .collect();
    let index = json!({
        "contract_id": RESPONSE_CONTRACT_ID,
        "contract_version": RESPONSE_CONTRACT_VERSION,
        "byte_semantics": BYTE_SEMANTICS,
        "generated_by": "src/external_response_contract.rs",
        "regenerate_with": "cargo test regenerate_response_contract_artifacts -- --ignored",
        "roles": roles,
    });
    let mut bytes = serde_json::to_vec_pretty(&index).expect("the index is serializable");
    bytes.push(b'\n');
    bytes
}

/// Wire name of an enum value as serde renders it.
///
/// Used by tests to prove the published vocabulary is the serde vocabulary
/// rather than a hand-copied list.
#[cfg(test)]
pub(crate) fn serde_wire_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The full worker-kind vocabulary serde accepts, with the plannable subset
/// marked by the caller. Kept here so the contract test can enumerate variants.
#[cfg(test)]
pub(crate) const ALL_WORKER_KINDS: &[WorkerKind] = &[
    WorkerKind::LocalOperation,
    WorkerKind::CodexReadonly,
    WorkerKind::CodexWriter,
    WorkerKind::CodexReviewer,
    WorkerKind::Verifier,
];

#[cfg(test)]
pub(crate) const ALL_OPERATION_KINDS: &[TaskOperationKind] = &[
    TaskOperationKind::ReadOnly,
    TaskOperationKind::LocalMutation,
    TaskOperationKind::HostNativeApproved,
];

#[cfg(test)]
pub(crate) const ALL_REPLAY_SAFETY: &[ReplaySafety] = &[
    ReplaySafety::SafeReadOnly,
    ReplaySafety::VerifyBeforeRetry,
    ReplaySafety::NeverAutomatic,
];

/// Every `VerificationSpec` kind, by its serde wire name.
#[cfg(test)]
pub(crate) fn all_verification_kinds() -> Vec<String> {
    let samples = [
        VerificationSpec::CommandExit {
            command: vec!["true".to_owned()],
            cwd: None,
            accepted_exit_codes: vec![0],
        },
        VerificationSpec::FileExists {
            path: "a".into(),
            must_be_file: true,
        },
        VerificationSpec::FileDigest {
            path: "a".into(),
            expected_sha256: "0".repeat(64),
        },
        VerificationSpec::GitScope {
            allowed_changed_paths: Vec::new(),
            require_no_other_changes: false,
        },
        VerificationSpec::NoForbiddenChanges {
            forbidden_paths: Vec::new(),
        },
        VerificationSpec::StructuredEvidence {
            requirement_id: "r".to_owned(),
        },
        VerificationSpec::ReviewGate {
            max_blocking_findings: 0,
        },
    ];
    samples
        .iter()
        .filter_map(|spec| {
            serde_json::to_value(spec)
                .ok()
                .and_then(|value| value.get("kind").and_then(Value::as_str).map(str::to_owned))
        })
        .collect()
}

#[cfg(test)]
static SCHEMA_DIGESTS: OnceLock<BTreeMap<&'static str, String>> = OnceLock::new();

/// Digest table for every role, computed once per process.
#[cfg(test)]
pub(crate) fn schema_digests() -> &'static BTreeMap<&'static str, String> {
    SCHEMA_DIGESTS.get_or_init(|| {
        CONTRACTS
            .iter()
            .map(|contract| (contract.role, schema_digest(contract)))
            .collect()
    })
}
