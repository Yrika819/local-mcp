//! Draft 2020-12 conformance gate for the Doctor Core V1 JSON contract.
//!
//! This binary is a CI tool, never a build input of the runtime package. It
//! validates four separate things and reports them separately, because a
//! schema-valid document is not automatically a truthful one:
//!
//! 1. the schema document itself compiles as a Draft 2020-12 schema;
//! 2. the checked-in synthetic fixtures are schema-valid;
//! 3. deliberately corrupted documents are rejected (a negative control that
//!    proves the gate is actually wired to the schema);
//! 4. the JSON a real `local-mcp doctor --json` produces is schema-valid and
//!    internally consistent, and stays inside the producer's own output
//!    profile (null/boolean observations, no paths, bounded size).
//!
//! Schema conformance cannot prove that a string is redacted. Step 4 therefore
//! checks the producer profile directly instead of trusting the schema's
//! permissive `observed` union.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

const EXIT_FAILED: i32 = 1;

const FIXTURES: [&str; 4] = [
    "doctor-v1-pass.json",
    "doctor-v1-warn.json",
    "doctor-v1-block.json",
    "doctor-v1-unknown.json",
];

const CAPABILITIES: [&str; 5] = [
    "mcp_transport",
    "session_management",
    "sandboxed_execution",
    "atomic_publication",
    "goal_model_execution",
];

/// Matches `JSON_OUTPUT_LIMIT` in `src/doctor.rs`. Kept as a separate literal on
/// purpose: the validator is an independent check, so it must not import the
/// value under test.
const MAX_REPORT_BYTES: usize = 64 * 1024;

fn main() {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("doctor-schema-validation=FAILED: {error}");
            std::process::exit(EXIT_FAILED);
        }
    };
    match run(&args) {
        Ok(report) => {
            for line in report {
                println!("{line}");
            }
            println!("doctor-schema-validation=OK");
        }
        Err(errors) => {
            for error in errors {
                eprintln!("error: {error}");
            }
            eprintln!("doctor-schema-validation=FAILED");
            std::process::exit(EXIT_FAILED);
        }
    }
}

struct Args {
    schema: PathBuf,
    fixtures: Option<PathBuf>,
    producer: Option<PathBuf>,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut schema = None;
        let mut fixtures = None;
        let mut producer = None;
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let mut value = |name: &str| -> Result<String, String> {
                args.next()
                    .ok_or_else(|| format!("{name} requires a value"))
            };
            match arg.as_str() {
                "--schema" => schema = Some(PathBuf::from(value("--schema")?)),
                "--fixtures" => fixtures = Some(PathBuf::from(value("--fixtures")?)),
                "--producer" => producer = Some(PathBuf::from(value("--producer")?)),
                other => return Err(format!("unrecognized argument: {other}")),
            }
        }
        Ok(Self {
            schema: schema.ok_or("--schema is required")?,
            fixtures,
            producer,
        })
    }
}

fn run(args: &Args) -> Result<Vec<String>, Vec<String>> {
    let mut failures = Vec::new();
    let mut report = Vec::new();

    let schema_text = std::fs::read_to_string(&args.schema)
        .map_err(|error| vec![format!("cannot read {}: {error}", args.schema.display())])?;
    let schema: Value = serde_json::from_str(&schema_text)
        .map_err(|error| vec![format!("{} is not JSON: {error}", args.schema.display())])?;

    // --- 1. The schema document itself -------------------------------------
    match check_schema_document(&schema) {
        Ok(line) => report.push(line),
        Err(error) => failures.push(error),
    }
    let validator = match jsonschema::validator_for(&schema) {
        Ok(validator) => Some(validator),
        Err(error) => {
            failures.push(format!("schema does not compile as Draft 2020-12: {error}"));
            None
        }
    };

    // --- 2. Synthetic fixtures --------------------------------------------
    let mut valid_fixture: Option<Value> = None;
    if let Some(directory) = &args.fixtures {
        for name in FIXTURES {
            let path = directory.join(name);
            match read_json(&path) {
                Ok(document) => {
                    if let Some(validator) = &validator {
                        match collect_errors(validator, &document) {
                            Ok(()) => {
                                report.push(format!("fixture {name}: schema-valid (synthetic)"));
                                if name == FIXTURES[0] {
                                    valid_fixture = Some(document);
                                }
                            }
                            Err(errors) => failures.push(format!(
                                "fixture {name} is not schema-valid: {}",
                                errors.join("; ")
                            )),
                        }
                    }
                }
                Err(error) => failures.push(error),
            }
        }
    }

    // --- 3. Negative control ----------------------------------------------
    if let (Some(validator), Some(fixture)) = (&validator, &valid_fixture) {
        match negative_control(validator, fixture) {
            Ok(count) => report.push(format!(
                "negative control: {count} corrupted documents rejected"
            )),
            Err(error) => failures.push(error),
        }
    }

    // --- 4. Producer output ------------------------------------------------
    if let Some(producer) = &args.producer {
        match check_producer(validator.as_ref(), producer) {
            Ok(lines) => report.extend(lines),
            Err(errors) => failures.extend(errors),
        }
    }

    if failures.is_empty() {
        Ok(report)
    } else {
        Err(failures)
    }
}

fn check_schema_document(schema: &Value) -> Result<String, String> {
    let object = schema
        .as_object()
        .ok_or("the schema document is not a JSON object")?;
    let draft = object
        .get("$schema")
        .and_then(Value::as_str)
        .ok_or("the schema has no $schema declaration")?;
    if draft != "https://json-schema.org/draft/2020-12/schema" {
        return Err(format!("unexpected $schema declaration: {draft}"));
    }
    for key in ["$defs", "required", "properties"] {
        if !object.contains_key(key) {
            return Err(format!("the schema is missing a top-level {key}"));
        }
    }
    // The crate has no public meta-schema entry point (jsonschema 0.26), so the
    // meta-validation gate is proven with a negative probe rather than assumed:
    // a schema whose `type` is not a string must fail to compile.
    let probe = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": 42
    });
    if jsonschema::validator_for(&probe).is_ok() {
        return Err(
            "the Draft 2020-12 compiler accepted a schema with a non-string type".to_owned(),
        );
    }
    Ok(format!(
        "schema document: compiles as Draft 2020-12 ({draft})"
    ))
}

fn negative_control(validator: &jsonschema::Validator, fixture: &Value) -> Result<usize, String> {
    let mutations: Vec<(&str, Value)> = vec![
        ("missing required top-level field", {
            let mut document = fixture.clone();
            document
                .as_object_mut()
                .ok_or("fixture is not an object")?
                .remove("summary");
            document
        }),
        ("unknown schema_version", {
            let mut document = fixture.clone();
            document["schema_version"] = Value::String("2.0.0".to_owned());
            document
        }),
        ("unknown diagnostic status", {
            let mut document = fixture.clone();
            document["checks"][0]["status"] = Value::String("SKIP".to_owned());
            document
        }),
        ("exit code outside the contract", {
            let mut document = fixture.clone();
            document["summary"]["exit_code"] = Value::from(3);
            document
        }),
        ("empty check list", {
            let mut document = fixture.clone();
            document["checks"] = Value::Array(Vec::new());
            document
        }),
        ("reason code that violates the pattern", {
            let mut document = fixture.clone();
            document["checks"][0]["reason_code"] = Value::String("lowercase".to_owned());
            document
        }),
        ("redaction flag cleared", {
            let mut document = fixture.clone();
            document["installation"]["current_exe"]["path_redacted"] = Value::Bool(false);
            document
        }),
        ("unknown operating system", {
            let mut document = fixture.clone();
            document["platform"]["operating_system"] = Value::String("solaris".to_owned());
            document
        }),
    ];

    let mut count = 0;
    for (description, document) in mutations {
        if collect_errors(validator, &document).is_ok() {
            return Err(format!(
                "negative control failed: the schema accepted a document with {description}"
            ));
        }
        count += 1;
    }
    Ok(count)
}

fn check_producer(
    validator: Option<&jsonschema::Validator>,
    producer: &Path,
) -> Result<Vec<String>, Vec<String>> {
    let output = Command::new(producer)
        .args(["doctor", "--json"])
        .output()
        .map_err(|error| vec![format!("cannot run {}: {error}", producer.display())])?;
    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| vec![format!("producer stdout is not UTF-8: {error}")])?;
    if !output.stderr.is_empty() {
        return Err(vec![format!(
            "producer wrote {} bytes to stderr for a successful JSON report",
            output.stderr.len()
        )]);
    }
    if stdout.len() > MAX_REPORT_BYTES {
        return Err(vec![format!(
            "producer report is {} bytes, above the {MAX_REPORT_BYTES} byte limit",
            stdout.len()
        )]);
    }
    let document: Value = serde_json::from_str(&stdout).map_err(|error| {
        vec![format!(
            "producer stdout is not exactly one JSON value: {error}"
        )]
    })?;
    document
        .as_object()
        .ok_or_else(|| vec!["producer report is not a JSON object".to_owned()])?;

    let mut failures = Vec::new();
    let mut report = Vec::new();
    if let Some(validator) = validator {
        match collect_errors(validator, &document) {
            Ok(()) => report.push("producer report: schema-valid".to_owned()),
            Err(errors) => failures.push(format!(
                "producer report is not schema-valid: {}",
                errors.join("; ")
            )),
        }
    }
    match semantic_checks(&document, output.status.code()) {
        Ok(count) => report.push(format!(
            "producer report: {count} semantic invariants hold (exit 0/1/2/64/70, \
             counts, capability aggregation, redaction, probe honesty)"
        )),
        Err(errors) => failures.extend(errors),
    }
    if failures.is_empty() {
        Ok(report)
    } else {
        Err(failures)
    }
}

fn semantic_checks(document: &Value, process_exit: Option<i32>) -> Result<usize, Vec<String>> {
    let mut checks = 0;
    let mut failures = Vec::new();

    let mut expect = |condition: bool, message: String| {
        checks += 1;
        if !condition {
            failures.push(message);
        }
    };

    let summary = &document["summary"];
    let check_list = document["checks"].as_array().cloned().unwrap_or_default();
    let windows = document["platform"]["operating_system"] == "windows";

    // Counts, totals and the exit-code mapping are recomputed from `checks`
    // instead of being read from `summary`.
    let count = |status: &str| {
        check_list
            .iter()
            .filter(|check| check["status"] == status)
            .count()
    };
    expect(
        summary["counts"]["PASS"] == count("PASS"),
        "summary.counts.PASS does not match the checks array".to_owned(),
    );
    expect(
        summary["counts"]["WARN"] == count("WARN"),
        "summary.counts.WARN does not match the checks array".to_owned(),
    );
    expect(
        summary["counts"]["BLOCK"] == count("BLOCK"),
        "summary.counts.BLOCK does not match the checks array".to_owned(),
    );
    expect(
        summary["counts"]["UNKNOWN"] == count("UNKNOWN"),
        "summary.counts.UNKNOWN does not match the checks array".to_owned(),
    );
    expect(
        summary["checks_total"] == check_list.len(),
        "summary.checks_total does not match the checks array".to_owned(),
    );

    let applicable_required: Vec<&str> = check_list
        .iter()
        .filter(|check| {
            check["requirement"] == "required" && check["applicability"] == "applicable"
        })
        .filter_map(|check| check["status"].as_str())
        .collect();
    let applicable_optional: Vec<&str> = check_list
        .iter()
        .filter(|check| {
            check["requirement"] == "optional" && check["applicability"] == "applicable"
        })
        .filter_map(|check| check["status"].as_str())
        .collect();
    let runtime: Vec<&str> = check_list
        .iter()
        .filter(|check| {
            matches!(
                check["evidence_type"].as_str(),
                Some("runtime_read_only" | "not_run")
            )
        })
        .filter_map(|check| check["status"].as_str())
        .collect();

    let required_status = aggregate(&applicable_required);
    let advisory_status = aggregate(&applicable_optional);
    let runtime_status = aggregate(&runtime);
    let blockers = applicable_required
        .iter()
        .filter(|s| **s == "BLOCK")
        .count();
    let unknowns = applicable_required
        .iter()
        .filter(|s| **s == "UNKNOWN")
        .count();

    expect(
        summary["required_status"] == required_status,
        format!("summary.required_status is not {required_status}"),
    );
    expect(
        summary["advisory_status"] == advisory_status,
        format!("summary.advisory_status is not {advisory_status}"),
    );
    expect(
        summary["runtime_status"] == runtime_status,
        format!("summary.runtime_status is not {runtime_status}"),
    );
    expect(
        summary["status"] == required_status,
        "summary.status must equal the required aggregate".to_owned(),
    );
    expect(
        summary["required_blockers"] == blockers,
        "summary.required_blockers is not the recomputed count".to_owned(),
    );
    expect(
        summary["required_unknowns"] == unknowns,
        "summary.required_unknowns is not the recomputed count".to_owned(),
    );
    let expected_exit = if blockers > 0 {
        1
    } else if unknowns > 0 || applicable_required.is_empty() {
        2
    } else {
        0
    };
    expect(
        summary["exit_code"] == expected_exit,
        format!("summary.exit_code is not the recomputed {expected_exit}"),
    );
    expect(
        process_exit == Some(expected_exit),
        format!("process exit {process_exit:?} does not match the reported {expected_exit}"),
    );

    // Requirement/applicability agreement and the per-capability aggregation.
    for (capability, expected_requirement) in [
        ("mcp_transport", "required"),
        ("session_management", "required"),
        (
            "sandboxed_execution",
            if windows {
                "not_applicable"
            } else {
                "required"
            },
        ),
        ("atomic_publication", "required"),
        ("goal_model_execution", "optional"),
    ] {
        expect(
            document["capabilities"][capability]["requirement"] == expected_requirement,
            format!("capability {capability} has the wrong requirement"),
        );
    }
    expect(
        document["capabilities"].as_object().is_some_and(|map| {
            CAPABILITIES
                .iter()
                .all(|name| map.get(*name).is_some_and(Value::is_object))
        }),
        "capabilities is missing one of the five contract capabilities".to_owned(),
    );

    for capability in CAPABILITIES {
        let reported = &document["capabilities"][capability];
        let requirement = reported["requirement"].as_str().unwrap_or("");
        if requirement == "not_applicable" {
            expect(
                reported["status"] == "UNKNOWN"
                    && reported["formal_status"] == "UNKNOWN"
                    && reported["runtime_status"] == "UNKNOWN",
                format!("capability {capability} must be UNKNOWN when not applicable"),
            );
            continue;
        }
        let selected: Vec<&Value> = check_list
            .iter()
            .filter(|check| {
                check["capability"] == capability && check["applicability"] == "applicable"
            })
            .collect();
        let statuses = |filter: &dyn Fn(&Value) -> bool| -> Vec<String> {
            selected
                .iter()
                .filter(|check| filter(check))
                .filter_map(|check| check["status"].as_str().map(str::to_owned))
                .collect()
        };
        let all: Vec<String> = statuses(&|_| true);
        let formal = statuses(&|check| {
            matches!(
                check["evidence_type"].as_str(),
                Some("static_observation" | "formal_contract")
            )
        });
        let runtime = statuses(&|check| {
            matches!(
                check["evidence_type"].as_str(),
                Some("runtime_read_only" | "not_run")
            )
        });
        expect(
            reported["status"] == aggregate_owned(&all),
            format!("capability {capability} status is not worst-wins over its checks"),
        );
        expect(
            reported["formal_status"] == aggregate_owned(&formal),
            format!("capability {capability} formal_status is not worst-wins over its checks"),
        );
        expect(
            reported["runtime_status"] == aggregate_owned(&runtime),
            format!("capability {capability} runtime_status is not worst-wins over its checks"),
        );
        let expected_reasons: BTreeSet<&str> = selected
            .iter()
            .filter_map(|check| check["reason_code"].as_str())
            .collect();
        let reported_reasons: BTreeSet<&str> = reported["reason_codes"]
            .as_array()
            .map(|codes| codes.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        expect(
            reported_reasons == expected_reasons,
            format!("capability {capability} reason_codes do not match its checks"),
        );
    }

    // An unrun probe is never evidence, and an evidence type of `not_run` can
    // never carry a PASS or WARN.
    for (path, probe) in [
        (
            "bubblewrap",
            &document["dependencies"]["bubblewrap"]["version_probe"],
        ),
        (
            "codex_cli",
            &document["dependencies"]["codex_cli"]["version_probe"],
        ),
    ] {
        expect(
            probe["attempted"] != false
                || (probe["status"] == "UNKNOWN" && probe["version"].is_null()),
            format!("{path} probe was not attempted but does not report UNKNOWN/null"),
        );
        expect(
            probe["requested"] != false || probe["attempted"] == false,
            format!("{path} probe reports an attempt without being requested"),
        );
    }
    for check in &check_list {
        expect(
            check["evidence_type"] != "not_run" || check["status"] == "UNKNOWN",
            format!(
                "check {} has not_run evidence but status {}",
                check["id"], check["status"]
            ),
        );
        expect(
            check["observed"].is_null() || check["observed"].is_boolean(),
            format!(
                "check {} emits a non-boolean observation that the pinned producer never produces",
                check["id"]
            ),
        );
    }
    expect(
        document["installation"]["current_exe"]["path_redacted"] == true,
        "installation.current_exe.path_redacted is not true".to_owned(),
    );
    expect(
        document["dependencies"]["codex_cli"]["path_redacted"] == true,
        "dependencies.codex_cli.path_redacted is not true".to_owned(),
    );

    let mut strings = Vec::new();
    collect_strings(document, &mut strings);
    for value in strings {
        if value.chars().count() > 512 {
            failures.push("a reported string exceeds the 512 character bound".to_owned());
        }
        if looks_like_a_path(&value) {
            failures.push(format!(
                "a reported string looks like a local path or environment value: {value}"
            ));
        }
    }
    checks += 1;

    if failures.is_empty() {
        Ok(checks)
    } else {
        Err(failures)
    }
}

fn looks_like_a_path(value: &str) -> bool {
    if value.contains(":\\\\") || value.contains("\\\\?\\") {
        return true;
    }
    for marker in ["/home/", "/Users/", "/root/", "/tmp/", "\\\\Users\\\\"] {
        if value.contains(marker) {
            return true;
        }
    }
    // A Windows drive prefix such as `C:\` or a POSIX absolute path with a
    // deeper component. Bare `/` is allowed because messages use it.
    let bytes = value.as_bytes();
    if bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' {
        return true;
    }
    value.starts_with("/home") || value.starts_with("/usr/local") || value.starts_with("/opt/")
}

fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_strings(item, out);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                out.push(key.clone());
                collect_strings(item, out);
            }
        }
        _ => {}
    }
}

fn aggregate(statuses: &[&str]) -> &'static str {
    let mut result = "UNKNOWN";
    let mut best: Option<u8> = None;
    for status in statuses {
        let (label, weight) = match *status {
            "BLOCK" => ("BLOCK", 3),
            "UNKNOWN" => ("UNKNOWN", 2),
            "WARN" => ("WARN", 1),
            _ => ("PASS", 0),
        };
        if best.is_none_or(|current| weight > current) {
            best = Some(weight);
            result = label;
        }
    }
    result
}

fn aggregate_owned(statuses: &[String]) -> Value {
    let borrowed: Vec<&str> = statuses.iter().map(String::as_str).collect();
    Value::String(aggregate(&borrowed).to_owned())
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{} is not JSON: {error}", path.display()))
}

fn collect_errors(validator: &jsonschema::Validator, document: &Value) -> Result<(), Vec<String>> {
    let errors: Vec<String> = validator
        .iter_errors(document)
        .map(|error| format!("{}: {error}", error.instance_path))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
