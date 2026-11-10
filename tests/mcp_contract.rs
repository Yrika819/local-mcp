//! H4-A — MCP tool contract test against a **real** `local-mcp mcp` process.
//!
//! The unit tests that live beside `mcp::tools()` assert the in-process JSON. This
//! test asserts the contract a *standard MCP client* sees:
//!
//! 1. it spawns the built `local-mcp` binary as an independent server process and
//!    speaks newline-delimited JSON-RPC to it over stdio (the same transport a
//!    real client uses);
//! 2. it takes `tools/list` from that process, not from the crate;
//! 3. it validates each published `inputSchema` with a JSON Schema validator
//!    (the subset of keywords these schemas actually use is implemented below,
//!    and any *unknown* keyword fails the test instead of being ignored silently);
//! 4. it then calls `tools/call` with instances built from that schema, for both
//!    the accepted and the refused cases.
//!
//! The takeover point is `goal_start.write_scope`: it is parsed by the runtime
//! for EXTERNAL Goals, and the published schema used to omit it, so any
//! schema-validating client could never request a Writer grant.
//!
//! Isolation: every test runs against a throwaway state root supplied through
//! the host-owned `LOCAL_MCP_STATE_DIR` override and a throwaway workspace, so no
//! operator session, Goal or authorization ledger is touched.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A throwaway state root plus workspace, removed on drop.
struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    state: PathBuf,
    session_id: String,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("local-mcp-h4a-{label}-{}", uuid::Uuid::new_v4()));
        let repo = root.join("repo");
        let state = root.join("state");
        create_private_dir(&root);
        create_private_dir(&repo);
        create_private_dir(&state);
        let session_id = format!("h4a-{}", uuid::Uuid::new_v4());
        let fixture = Self {
            root,
            repo,
            state,
            session_id,
        };
        fixture.write_session();
        fixture
    }

    /// Write the Session record the server loads by ID.
    ///
    /// This is the same on-disk record `local-mcp start` produces; the harness
    /// owns this throwaway state root, so it is host state for a disposable
    /// workspace, not a forged authority over a real one.
    fn write_session(&self) {
        let sessions = self.state.join("sessions");
        create_private_dir(&sessions);
        let record = json!({
            "id": self.session_id,
            "cwd": self.repo,
            "permitted_directories": [self.repo],
        });
        let path = sessions.join(format!("{}.json", self.session_id));
        std::fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn goal_files(&self) -> Vec<PathBuf> {
        let directory = self.state.join("goals").join(&self.session_id);
        match std::fs::read_dir(directory) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                // The session Goal lock lives in the same directory; only Goal
                // documents count.
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn create_private_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path).unwrap();
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path).unwrap();
    }
}

/// A real `local-mcp mcp` child process driven over stdio.
struct McpServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpServer {
    fn spawn(fixture: &Fixture) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_local-mcp"))
            .arg("mcp")
            .current_dir(&fixture.repo)
            .env("LOCAL_MCP_STATE_DIR", &fixture.state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the built local-mcp binary must be spawnable");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut server = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let initialized = server.request("initialize", json!({"protocolVersion": "2025-06-18"}));
        assert_eq!(
            initialized["result"]["serverInfo"]["name"], "local-mcp",
            "the server must identify itself: {initialized}"
        );
        server
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_string(&request).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
        let mut response = String::new();
        let read = self.stdout.read_line(&mut response).unwrap();
        assert!(read > 0, "the server closed the transport during {method}");
        let parsed: Value = serde_json::from_str(&response)
            .unwrap_or_else(|error| panic!("non-JSON response to {method}: {error}\n{response}"));
        assert_eq!(parsed["id"], id, "response id must match the request");
        parsed
    }

    fn tools(&mut self) -> Vec<Value> {
        let response = self.request("tools/list", json!({}));
        response["result"]["tools"]
            .as_array()
            .expect("tools/list returns an array")
            .clone()
    }

    /// Call one tool and return the JSON-RPC `result` object.
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert!(
            response.get("error").is_none(),
            "tools/call {name} must not fail at the transport level: {response}"
        );
        response["result"].clone()
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("the published tool list must contain {name}"))
}

/// Goal tools report a refusal as a *successful* JSON-RPC result with
/// `isError: true` whose text is the typed `GoalApiError`.
fn goal_error_code(result: &Value) -> Option<String> {
    if result["isError"] != true {
        return None;
    }
    let text = result["content"][0]["text"].as_str()?;
    let parsed: Value = serde_json::from_str(text).ok()?;
    parsed["code"].as_str().map(str::to_owned)
}

fn goal_view(result: &Value) -> Value {
    assert_ne!(result["isError"], true, "expected success, got {result}");
    let text = result["content"][0]["text"].as_str().expect("text result");
    serde_json::from_str(text).expect("goal views are JSON")
}

fn start_arguments(session_id: &str, backend: &str) -> Value {
    json!({
        "session_id": session_id,
        "objective": "H4-A contract probe: create a durable Goal and stop.",
        "model_backend": backend,
    })
}

fn write_scope() -> Value {
    json!({
        "allowed_paths": ["src/h4a-probe.txt"],
        "allowed_operations": ["WRITE_UTF8"],
        "max_operations": 1,
        "max_bytes": 4096,
        "expires_in_seconds": 900,
    })
}

// ---------------------------------------------------------------------------
// Minimal JSON Schema validator
// ---------------------------------------------------------------------------

/// Keywords these schemas use. A schema that introduces another keyword fails
/// the contract test instead of silently passing unvalidated.
const SUPPORTED_KEYWORDS: &[&str] = &[
    "type",
    "description",
    "format",
    "required",
    "properties",
    "additionalProperties",
    "maxLength",
    "minLength",
    "minimum",
    "maximum",
    "items",
    "minItems",
    "maxItems",
    "enum",
    "default",
    "pattern",
    "title",
    "oneOf",
    "x-max-bytes",
];

fn validate_keywords(schema: &Value, path: &str, problems: &mut Vec<String>) {
    let Some(object) = schema.as_object() else {
        problems.push(format!("{path}: a schema node must be an object"));
        return;
    };
    for key in object.keys() {
        if !SUPPORTED_KEYWORDS.contains(&key.as_str()) {
            problems.push(format!("{path}: unsupported schema keyword {key}"));
        }
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (name, child) in properties {
            validate_keywords(child, &format!("{path}.{name}"), problems);
        }
    }
    if let Some(items) = object.get("items") {
        validate_keywords(items, &format!("{path}[]"), problems);
    }
    if let Some(branches) = object.get("oneOf").and_then(Value::as_array) {
        for (index, branch) in branches.iter().enumerate() {
            validate_keywords(branch, &format!("{path}.oneOf[{index}]"), problems);
        }
    }
}

/// Validate `instance` against the supported subset. Returns violations.
fn validate(schema: &Value, instance: &Value, path: &str, problems: &mut Vec<String>) {
    if let Some(expected) = schema.get("type") {
        let matches = match expected.as_str().unwrap_or_default() {
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "string" => instance.is_string(),
            "integer" => instance.as_i64().is_some() || instance.as_u64().is_some(),
            "number" => instance.is_number(),
            "boolean" => instance.is_boolean(),
            "null" => instance.is_null(),
            other => {
                problems.push(format!("{path}: unknown type {other}"));
                true
            }
        };
        if !matches {
            problems.push(format!(
                "{path}: expected type {} but saw {instance}",
                expected
            ));
            return;
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(instance)
    {
        problems.push(format!(
            "{path}: {instance} is outside the enum {allowed:?}"
        ));
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let mut matched = 0usize;
        for branch in branches {
            let mut branch_problems = Vec::new();
            validate(branch, instance, path, &mut branch_problems);
            if branch_problems.is_empty() {
                matched += 1;
            }
        }
        if matched != 1 {
            problems.push(format!(
                "{path}: {matched} oneOf branches matched, expected 1"
            ));
            return;
        }
    }
    match instance {
        Value::String(text) => {
            if let Some(minimum) = schema.get("minLength").and_then(Value::as_u64)
                && (text.chars().count() as u64) < minimum
            {
                problems.push(format!("{path}: shorter than minLength {minimum}"));
            }
            if let Some(maximum) = schema.get("maxLength").and_then(Value::as_u64)
                && (text.chars().count() as u64) > maximum
            {
                problems.push(format!("{path}: longer than maxLength {maximum}"));
            }
            // H5-B: string bounds in this contract are UTF-8 byte counts, which is
            // what the host validator measures. `x-max-bytes` is authoritative.
            if let Some(maximum) = schema.get("x-max-bytes").and_then(Value::as_u64)
                && (text.len() as u64) > maximum
            {
                problems.push(format!(
                    "{path}: {}-byte string exceeds {maximum}",
                    text.len()
                ));
            }
            if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
                match simple_pattern_matches(pattern, text) {
                    Some(true) => {}
                    Some(false) => {
                        problems.push(format!("{path}: does not match pattern {pattern}"))
                    }
                    None => problems.push(format!(
                        "{path}: pattern {pattern} is outside the audited subset"
                    )),
                }
            }
        }
        Value::Number(number) => {
            let value = number.as_f64().unwrap_or(f64::NAN);
            if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
                && value < minimum
            {
                problems.push(format!("{path}: below minimum {minimum}"));
            }
            if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
                && value > maximum
            {
                problems.push(format!("{path}: above maximum {maximum}"));
            }
        }
        Value::Array(items) => {
            if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64)
                && (items.len() as u64) < minimum
            {
                problems.push(format!("{path}: fewer than minItems {minimum}"));
            }
            if let Some(maximum) = schema.get("maxItems").and_then(Value::as_u64)
                && (items.len() as u64) > maximum
            {
                problems.push(format!("{path}: more than maxItems {maximum}"));
            }
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    validate(item_schema, item, &format!("{path}[{index}]"), problems);
                }
            }
        }
        Value::Object(object) => {
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for name in required.iter().filter_map(Value::as_str) {
                    if !object.contains_key(name) {
                        problems.push(format!("{path}: missing required property {name}"));
                    }
                }
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            for (name, value) in object {
                match properties.and_then(|properties| properties.get(name)) {
                    Some(child) => validate(child, value, &format!("{path}.{name}"), problems),
                    None => {
                        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                            problems.push(format!(
                                "{path}: property {name} is not declared and \
                                 additionalProperties is false"
                            ));
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Only the pattern shape these schemas use is audited: one anchored character
/// class with a fixed/`{n,m}` repetition, e.g. `^[0-9a-f]{64}$`. Any other shape
/// returns `None`, which is reported as a violation rather than being ignored.
fn simple_pattern_matches(pattern: &str, text: &str) -> Option<bool> {
    let body = pattern.strip_prefix('^')?.strip_suffix('$')?;
    let open = body.find('[')?;
    let close = body.find(']')?;
    let classes = &body[open + 1..close];
    if open != 0 || close + 1 >= body.len() {
        return None;
    }
    let quantifier = &body[close + 1..];
    let (minimum, maximum) = {
        let inner = quantifier.strip_prefix('{')?.strip_suffix('}')?;
        let mut parts = inner.split(',');
        let minimum: usize = parts.next()?.parse().ok()?;
        let maximum: usize = match parts.next() {
            Some(value) => value.parse().ok()?,
            None => minimum,
        };
        (minimum, maximum)
    };
    let count = text.chars().count();
    Some(
        count >= minimum
            && count <= maximum
            && text
                .chars()
                .all(|character| class_matches(classes, character)),
    )
}

fn class_matches(classes: &str, character: char) -> bool {
    let classes = classes.trim_start_matches('[').trim_end_matches(']');
    let mut characters = classes.chars().peekable();
    while let Some(start) = characters.next() {
        if characters.peek() == Some(&'-') {
            characters.next();
            if let Some(end) = characters.next()
                && start <= character
                && character <= end
            {
                return true;
            }
        } else if start == character {
            return true;
        }
    }
    false
}

/// Build the smallest instance the schema accepts. Used to prove each published
/// schema is satisfiable: an unsatisfiable schema would make the tool
/// unreachable for a schema-following client.
fn instance_for(schema: &Value, property: Option<&str>) -> Value {
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        return allowed.first().cloned().unwrap_or(Value::Null);
    }
    match schema["type"].as_str().unwrap_or("object") {
        "object" => {
            let mut object = Map::new();
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for name in required.iter().filter_map(Value::as_str) {
                    let child = &schema["properties"][name];
                    object.insert(name.to_owned(), instance_for(child, Some(name)));
                }
            }
            Value::Object(object)
        }
        "array" => {
            let minimum = schema["minItems"].as_u64().unwrap_or(0).max(1);
            let item = schema.get("items").cloned().unwrap_or(json!({}));
            Value::Array(
                (0..minimum)
                    .map(|_| instance_for(&item, property))
                    .collect(),
            )
        }
        "string" => {
            let minimum = schema["minLength"].as_u64().unwrap_or(0).max(1) as usize;
            if schema.get("format").and_then(Value::as_str) == Some("uuid") {
                return json!(uuid::Uuid::new_v4().to_string());
            }
            if let Some(pattern) = schema.get("pattern").and_then(Value::as_str)
                && pattern.contains("[0-9a-f]{64}")
            {
                return json!("a".repeat(64));
            }
            let base = property.unwrap_or("value");
            let mut text = base.repeat(minimum.div_ceil(base.len().max(1)) + 1);
            text.truncate(minimum);
            json!(text)
        }
        "integer" | "number" => {
            let minimum = schema["minimum"].as_i64().unwrap_or(1);
            json!(minimum)
        }
        "boolean" => json!(false),
        _ => Value::Null,
    }
}

fn assert_schema_accepts(schema: &Value, instance: &Value) {
    let mut problems = Vec::new();
    validate(schema, instance, "$", &mut problems);
    assert!(problems.is_empty(), "schema violations: {problems:#?}");
}

fn assert_schema_rejects(schema: &Value, instance: &Value) {
    let mut problems = Vec::new();
    validate(schema, instance, "$", &mut problems);
    assert!(
        !problems.is_empty(),
        "schema unexpectedly accepted {instance}"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The regression: `goal_start.write_scope` must be published, and a client that
/// follows the published schema must be able to submit a scope, while an
/// out-of-schema payload is refused by both the schema and the runtime.
#[test]
fn published_goal_start_schema_accepts_write_scope_that_the_runtime_accepts() {
    let fixture = Fixture::new("goal-start-scope");
    let mut server = McpServer::spawn(&fixture);
    let tools = server.tools();

    let goal_start = tool(&tools, "goal_start");
    let schema = &goal_start["inputSchema"];
    assert_eq!(
        schema["additionalProperties"], false,
        "goal_start must keep refusing undeclared properties"
    );
    assert!(
        schema["properties"].get("write_scope").is_some(),
        "goal_start must publish write_scope: without it a schema-validating \
         MCP client cannot request an EXTERNAL write scope at all"
    );

    let mut keyword_problems = Vec::new();
    validate_keywords(schema, "goal_start", &mut keyword_problems);
    assert!(keyword_problems.is_empty(), "{keyword_problems:#?}");

    // A conformant EXTERNAL call built from the published schema.
    let mut arguments = start_arguments(&fixture.session_id, "EXTERNAL");
    arguments["write_scope"] = write_scope();
    assert_schema_accepts(schema, &arguments);

    let view = goal_view(&server.call("goal_start", arguments));
    assert_eq!(view["model_backend"], "EXTERNAL");
    // The scope is recorded as a *proposal*: the authorization surface now says
    // a grant is required, and no grant exists. Nothing was authorized.
    assert_eq!(
        view["authorization"]["state"], "REQUIRED",
        "a write_scope at goal_start must leave the Goal awaiting a grant: {view}"
    );
    assert!(
        view["authorization"].get("grant_id").is_none(),
        "goal_start must never mint a grant: {view}"
    );

    // Same shape, but the required budget key is missing: the schema rejects it
    // and so does the runtime parser.
    let mut incomplete = start_arguments(&fixture.session_id, "EXTERNAL");
    let mut scope = write_scope();
    scope.as_object_mut().unwrap().remove("max_bytes");
    incomplete["write_scope"] = scope;
    assert_schema_rejects(schema, &incomplete);
    let refused = server.call("goal_start", incomplete);
    assert_eq!(
        goal_error_code(&refused).as_deref(),
        Some("INVALID_ARGUMENT"),
        "the runtime must refuse the same payload the schema refuses: {refused}"
    );

    // Undeclared key inside write_scope: `deny_unknown_fields` on the runtime
    // struct must line up with `additionalProperties: false` in the schema.
    let mut unknown = start_arguments(&fixture.session_id, "EXTERNAL");
    let mut scope = write_scope();
    scope["root"] = json!("/tmp/anywhere");
    unknown["write_scope"] = scope;
    assert_schema_rejects(schema, &unknown);
    let refused = server.call("goal_start", unknown);
    assert_eq!(
        goal_error_code(&refused).as_deref(),
        Some("INVALID_ARGUMENT"),
        "an undeclared write_scope field must be refused: {refused}"
    );

    // Scope on a CODEX Goal: schema-visible (the field is declared) but refused
    // by host policy.
    let mut codex = start_arguments(&fixture.session_id, "CODEX");
    codex["write_scope"] = write_scope();
    assert_schema_accepts(schema, &codex);
    let refused = server.call("goal_start", codex);
    assert_eq!(
        goal_error_code(&refused).as_deref(),
        Some("INVALID_ARGUMENT"),
        "write_scope must stay refused for the CODEX backend: {refused}"
    );

    // The refused calls above must not have created a Goal.
    assert_eq!(
        fixture.goal_files().len(),
        1,
        "only the accepted EXTERNAL Goal may exist: {:?}",
        fixture.goal_files()
    );
}

/// Every published schema must be self-consistent and satisfiable: no keyword
/// outside the audited subset, and a generated minimal instance that validates.
#[test]
fn every_published_tool_schema_is_well_formed_and_satisfiable() {
    let fixture = Fixture::new("tool-audit");
    let mut server = McpServer::spawn(&fixture);
    let tools = server.tools();

    let mut names = Vec::new();
    for entry in &tools {
        let name = entry["name"].as_str().expect("every tool has a name");
        names.push(name.to_owned());
        let schema = &entry["inputSchema"];
        assert_eq!(
            schema["type"], "object",
            "{name} inputSchema must be an object"
        );

        let mut problems = Vec::new();
        validate_keywords(schema, name, &mut problems);
        assert!(problems.is_empty(), "{name}: {problems:#?}");

        let instance = instance_for(schema, None);
        let mut violations = Vec::new();
        validate(schema, &instance, name, &mut violations);
        assert!(
            violations.is_empty(),
            "{name}: the minimum conformant instance is not accepted: {violations:#?}\n{instance}"
        );
    }

    for required in [
        "session_info",
        "goal_start",
        "goal_status",
        "goal_pause",
        "goal_resume",
        "goal_cancel",
        "goal_result",
        "goal_next",
        "goal_submit",
        "goal_run",
    ] {
        assert!(
            names.iter().any(|name| name == required),
            "the Goal contract must publish {required}"
        );
    }
    assert!(
        !names.contains(&"goal_write_grant".to_owned()),
        "no model-facing tool may issue a grant"
    );
}

/// `goal_start` and `goal_next`/`goal_submit` must agree about the EXTERNAL
/// workflow end to end at the schema level: the WorkItem the server hands back
/// carries the request fields `goal_submit` declares.
#[test]
fn goal_next_work_item_fields_are_covered_by_goal_submit_schema() {
    let fixture = Fixture::new("work-item-contract");
    let mut server = McpServer::spawn(&fixture);
    let tools = server.tools();

    let view = goal_view(&server.call(
        "goal_start",
        start_arguments(&fixture.session_id, "EXTERNAL"),
    ));
    let goal_id = view["goal_id"].as_str().unwrap().to_owned();

    let next = goal_view(&server.call(
        "goal_next",
        json!({"session_id": fixture.session_id, "goal_id": goal_id, "max_host_steps": 1}),
    ));
    assert_eq!(
        next["outcome"], "MODEL_REQUIRED",
        "the EXTERNAL backend must hand the client a ModelWorkItem: {next}"
    );
    let work = &next["model_work"];
    for field in [
        "work_id",
        "request_digest",
        "goal_revision",
        "output_contract",
    ] {
        assert!(
            work.get(field).is_some(),
            "the ModelWorkItem must carry {field}: {work}"
        );
    }

    // A submission built strictly from the declared goal_submit schema is
    // accepted by that schema (the response body itself is model-owned data).
    let submit_schema = &tool(&tools, "goal_submit")["inputSchema"];
    let submission = json!({
        "session_id": fixture.session_id,
        "goal_id": goal_id,
        "work_id": work["work_id"],
        "submission_id": uuid::Uuid::new_v4().to_string(),
        "expected_goal_revision": work["goal_revision"],
        "request_digest": work["request_digest"],
        "response": {},
    });
    assert_schema_accepts(submit_schema, &submission);
}

/// H5-B — the response contract a real MCP client sees on `goal_next`.
///
/// This is the executable form of finding H4-D-F1: the same real server process
/// that hands out a `ModelWorkItem` must also hand out a machine-readable
/// description of the answer it will accept, and the description must be the one
/// checked into `docs/schemas/external/`.
///
/// It goes further than reading the schema: the WorkItem is answered twice
/// against the same live server — once with the out-of-contract answer a real
/// model actually produced (`"status": "COMPLETED"`), which must be refused, and
/// once with an answer built from the published contract, which must be accepted.
#[test]
fn goal_next_publishes_a_machine_readable_response_contract() {
    let fixture = Fixture::new("response-contract");
    let mut server = McpServer::spawn(&fixture);

    let view = goal_view(&server.call(
        "goal_start",
        start_arguments(&fixture.session_id, "EXTERNAL"),
    ));
    let goal_id = view["goal_id"].as_str().unwrap().to_owned();

    let next = goal_view(&server.call(
        "goal_next",
        json!({"session_id": fixture.session_id, "goal_id": goal_id, "max_host_steps": 1}),
    ));
    assert_eq!(next["outcome"], "MODEL_REQUIRED", "{next}");
    let work = next["model_work"].clone();
    let contract = &work["response_contract"];

    // 1. The contract is identified, versioned, and digest-addressed.
    assert_eq!(
        contract["contract_id"],
        "goallatch.external.response.v2/PLANNER/planner-proposal-v1"
    );
    assert_eq!(contract["contract_version"], 2);
    assert_eq!(contract["byte_semantics"], "UTF8_BYTES");
    let digest = contract["schema_digest"]
        .as_str()
        .expect("the contract carries a digest");
    assert!(digest.starts_with("sha256:"), "{digest}");
    assert_eq!(digest.len(), 7 + 64);

    // 2. It names the identity fields the client must echo, and they are also
    //    required — an echo field the schema does not require is a contract bug.
    let required: Vec<&str> = contract["required_fields"]
        .as_array()
        .expect("required_fields")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let echo: Vec<&str> = contract["identity_echo_fields"]
        .as_array()
        .expect("identity_echo_fields")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(!echo.is_empty());
    for field in &echo {
        assert!(echo.contains(field));
        assert!(
            required.contains(field),
            "an echoed field must be required: {field}"
        );
    }

    // 3. The schema is the checked-in artifact, byte for byte.
    let artifact: Value = serde_json::from_slice(
        &std::fs::read("docs/schemas/external/planner.v2.json")
            .expect("the planner contract artifact is checked in"),
    )
    .expect("the artifact is JSON");
    assert_eq!(
        &contract["schema"], &artifact,
        "the server must publish the reviewed schema, not a different one"
    );

    // 4. The refusal vocabulary the contract publishes is the one the runtime uses.
    let decisions: Vec<&str> = contract["submission_decisions"]
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !decisions.is_empty() {
        assert!(decisions.contains(&"REJECTED_SCHEMA"));
        assert!(decisions.contains(&"COMMITTED"));
        assert!(!decisions.contains(&"COMPLETED"));
    }

    // 5. Answer the PLANNER WorkItem with the out-of-contract status a real model
    //    produced in H4, then with a contract-valid plan.
    let bogus = json!({
        "goal_id": goal_id,
        "goal_revision": work["goal_revision"],
        "status": "COMPLETED",
        "summary": "the model declares itself finished",
        "tasks": []
    });
    assert_schema_rejects(&contract["schema"], &bogus);
    let refused = goal_view(&server.call(
        "goal_submit",
        json!({
            "session_id": fixture.session_id,
            "goal_id": goal_id,
            "work_id": work["work_id"],
            "submission_id": uuid::Uuid::new_v4().to_string(),
            "expected_goal_revision": work["goal_revision"],
            "request_digest": work["request_digest"],
            "response": bogus,
        }),
    ));
    assert_eq!(
        refused["outcome"], "REJECTED_SCHEMA",
        "the host must refuse a model-declared completion: {refused}"
    );
    assert!(
        refused["code"]
            .as_str()
            .unwrap_or_default()
            .contains("PLANNER")
            || refused["code"]
                .as_str()
                .unwrap_or_default()
                .contains("SCHEMA"),
        "the refusal must be a schema refusal: {refused}"
    );

    // A fresh WorkItem is issued after the refusal, and a plan built from the
    // published contract is accepted.
    let next = goal_view(&server.call(
        "goal_next",
        json!({"session_id": fixture.session_id, "goal_id": goal_id, "max_host_steps": 1}),
    ));
    assert_eq!(next["outcome"], "MODEL_REQUIRED", "{next}");
    let work = next["model_work"].clone();
    let contract = &work["response_contract"];
    let criterion = work["request"]["completion_criteria"][0]["criterion_id"]
        .as_str()
        .expect("the planner request carries a completion criterion")
        .to_owned();
    let plan = json!({
        "goal_id": goal_id,
        "goal_revision": work["goal_revision"],
        "summary": "one bounded read-only survey of the fixture",
        "tasks": [{
            "proposal_id": "survey",
            "title": "survey the fixture",
            "objective": "list every file under the workspace root",
            "mandatory": true,
            "worker": "CODEX_READONLY",
            "dependencies": [],
            "scope": {
                "allowed_paths": ["."],
                "forbidden_paths": [],
                "operation_kind": "READ_ONLY",
                "replay_safety": "SAFE_READ_ONLY"
            },
            "verification": [{"kind": "STRUCTURED_EVIDENCE", "requirement_id": "survey"}]
        }],
        "criterion_bindings": [{"criterion_id": criterion, "task_refs": ["survey"]}]
    });
    assert_schema_accepts(&contract["schema"], &plan);
    let accepted = goal_view(&server.call(
        "goal_submit",
        json!({
            "session_id": fixture.session_id,
            "goal_id": goal_id,
            "work_id": work["work_id"],
            "submission_id": uuid::Uuid::new_v4().to_string(),
            "expected_goal_revision": work["goal_revision"],
            "request_digest": work["request_digest"],
            "response": plan,
        }),
    ));
    assert_eq!(
        accepted["outcome"], "COMMITTED",
        "a contract-valid plan must be applied: {accepted}"
    );

    // And the next WorkItem is the READONLY one, whose published status
    // vocabulary is the thing H4-D-F1 was missing.
    let next = goal_view(&server.call(
        "goal_next",
        json!({"session_id": fixture.session_id, "goal_id": goal_id, "max_host_steps": 1}),
    ));
    let readonly_contract = &next["model_work"]["response_contract"];
    assert_eq!(readonly_contract["role"], "READONLY", "{next}");
    let statuses: Vec<&str> = readonly_contract["schema"]["properties"]["status"]["enum"]
        .as_array()
        .expect("the read-only contract publishes its status enum")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        statuses,
        vec!["candidate_complete", "blocked", "needs_replan", "failed"]
    );
    assert!(
        !statuses.contains(&"COMPLETED"),
        "the published vocabulary must not admit a model-declared completion"
    );
}

/// Isolation evidence for H4-D: two server processes with different state roots
/// cannot see each other's Sessions, Goals or authorization ledgers.
#[test]
fn isolated_state_roots_do_not_share_sessions_goals_or_ledgers() {
    let first = Fixture::new("isolation-a");
    let second = Fixture::new("isolation-b");
    {
        let mut server = McpServer::spawn(&first);
        let view =
            goal_view(&server.call("goal_start", start_arguments(&first.session_id, "EXTERNAL")));
        assert_eq!(
            view["session_id"], first.session_id,
            "the Goal is bound to its own session"
        );
        assert_eq!(first.goal_files().len(), 1);
        assert!(
            second.goal_files().is_empty(),
            "the other state root must stay untouched"
        );
    }
    // The second root knows nothing about the first root's session, so the Goal
    // API cannot even resolve it.
    let mut other = McpServer::spawn(&second);
    let response = other.request(
        "tools/call",
        json!({"name": "goal_status", "arguments": {"session_id": first.session_id}}),
    );
    assert!(
        response.get("error").is_some(),
        "a Session from another state root must not resolve: {response}"
    );
    assert!(
        second.goal_files().is_empty(),
        "no Goal may appear in the isolated root"
    );
}

/// Read the bounded stdout of the child into a string. Kept small and explicit
/// so a transport change on either side is a compile-time or assertion failure
/// rather than a silently hanging test.
#[allow(dead_code)]
fn read_all(mut reader: impl Read) -> String {
    let mut buffer = String::new();
    let _ = reader.read_to_string(&mut buffer);
    buffer
}
