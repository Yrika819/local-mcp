//! Setup Preview V1 — a read-only, presenter-only report of which pinned,
//! already-published release a later Setup would install, and what the
//! Doctor V1 contract says about the local host.
//!
//! Hard boundaries (see `docs/SETUP_PREVIEW_V1_DESIGN.md`, closed 2026-11-04):
//!
//! - no install, update, download, extraction, staging, or execution of any
//!   kind; `--archive` only hashes the file, `--manifest` only reads it;
//! - no network I/O; the pinned release is embedded or operator-supplied;
//! - no writes of any kind (stdout/stderr are the only outputs);
//! - Doctor Core V1 is reused through its internal API (`doctor::collect_report`),
//!   never through an external process; its outcomes are rendered verbatim and
//!   are never an installation authorization;
//! - artifact verification is three-way explicit: manifest present / pin
//!   consistent / archive bytes hashed this run. `VERIFIED_ARCHIVE` is shown
//!   only when bytes were hashed in this run.
//!
//! Exit codes (this module's own contract; 1/2 remain Doctor's):
//! `0` rendered (says nothing about install safety), `64` usage/input error,
//! `70` internal error. "Unverifiable" is rendered, never an error.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    path::PathBuf,
};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::doctor;

pub(crate) const PREVIEW_SCHEMA_VERSION: &str = "1.0.0";
pub(crate) const PREVIEW_CONTRACT_VERSION: u8 = 1;

const PINNED_VERSION: &str = "0.1.2";
const SUPPORTED_DOCTOR_SCHEMA_VERSION: &str = "1.0.0";
const SUPPORTED_DOCTOR_CONTRACT_VERSION: u8 = 1;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const IO_BUFFER: usize = 1024 * 1024;
const HUMAN_OUTPUT_LIMIT: usize = 64 * 1024;
const JSON_OUTPUT_LIMIT: usize = 1024 * 1024;
const MAX_ECHO_CHARS: usize = 96;

// ---------------------------------------------------------------------------
// CLI surface
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = false)]
pub(crate) struct SetupArgs {
    /// Subcommand; bare `local-mcp setup` prints help and exits 64.
    #[command(subcommand)]
    command: Option<SetupCommand>,
}

#[derive(Subcommand, Debug)]
enum SetupCommand {
    /// Show which pinned release a setup would install (read-only).
    Preview(SetupPreviewOptions),
}

#[derive(Args, Debug)]
pub(crate) struct SetupPreviewOptions {
    /// Emit one versioned JSON envelope instead of plain text.
    #[arg(long)]
    json: bool,
    /// Operator-supplied release manifest (JSON, at most 1 MiB).
    /// Defaults to the embedded, pinned v0.1.2 manifest.
    #[arg(long, value_name = "PATH")]
    manifest: Option<PathBuf>,
    /// Local archive to hash for byte verification (at most 256 MiB).
    /// Only hashed; never extracted or executed.
    #[arg(long, value_name = "PATH")]
    archive: Option<PathBuf>,
}

/// Entry point for `local-mcp setup …`. Returns the process exit code.
pub(crate) fn run(args: SetupArgs) -> u8 {
    match args.command {
        None => {
            const SETUP_HELP: &str = "local-mcp setup — inspect which pinned release a setup would install (read-only)\n\nUSAGE:\n    local-mcp setup <COMMAND>\n\nCOMMANDS:\n    preview    Show which pinned release a setup would install (read-only)\n\nFLAGS:\n    --help        Print the setup help text\n\n'local-mcp setup' never installs anything. `local-mcp setup preview` only\nreports the pinned release, the Doctor V1 diagnostic, and what a hypothetical\nsetup would require. A Doctor PASS is not an installation authorization.\n";
            let mut stderr = io::stderr().lock();
            if stderr.write_all(SETUP_HELP.as_bytes()).is_err() {
                return ExitCode::InternalError.value();
            }
            ExitCode::UsageError.value()
        }
        Some(SetupCommand::Preview(options)) => execute(options),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExitCode {
    Rendered,
    UsageError,
    InternalError,
}

impl ExitCode {
    const fn value(self) -> u8 {
        match self {
            Self::Rendered => 0,
            Self::UsageError => 64,
            Self::InternalError => 70,
        }
    }
}

// ---------------------------------------------------------------------------
// Pinned release table (the embedded v0.1.2 pin)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub(crate) enum Target {
    LinuxX86_64,
    LinuxAarch64,
    MacosX86_64,
    MacosAarch64,
    WindowsX86_64,
    WindowsAarch64,
}

struct TargetFacts {
    key: &'static str,
    asset_name: &'static str,
    sha256: &'static str,
    required_helpers: &'static [&'static str],
    classification: &'static str,
    experimental: bool,
}

impl Target {
    const ALL: [Target; 6] = [
        Target::LinuxX86_64,
        Target::LinuxAarch64,
        Target::MacosX86_64,
        Target::MacosAarch64,
        Target::WindowsX86_64,
        Target::WindowsAarch64,
    ];

    fn facts(self) -> TargetFacts {
        // Values are the `SHA256SUMS` published with the v0.1.2 GitHub
        // release (verified against the release page on 2026-11-04).
        match self {
            Target::LinuxX86_64 => TargetFacts {
                key: "linux-x86_64",
                asset_name: "local-mcp-v0.1.2-linux-x86_64.tar.gz",
                sha256: "34bfccf8a6bc1a258b3d841234d445f6e7db9db0fc9a5895dddffa95affe43e6",
                required_helpers: &["local-mcp", "codex-linux-sandbox", "atomic-publish"],
                classification: "supported",
                experimental: false,
            },
            Target::LinuxAarch64 => TargetFacts {
                key: "linux-aarch64",
                asset_name: "local-mcp-v0.1.2-linux-aarch64.tar.gz",
                sha256: "f048eddfd17b9fe9bdccaee7d05418d31282d5642c2a709c27738fba58c9ece0",
                required_helpers: &["local-mcp", "codex-linux-sandbox", "atomic-publish"],
                classification: "experimental",
                experimental: true,
            },
            Target::MacosX86_64 => TargetFacts {
                key: "macos-x86_64",
                asset_name: "local-mcp-v0.1.2-macos-x86_64.tar.gz",
                sha256: "12d24cb82501153527422f54639e3f5332c7d84b741dbdbb4b9a5a7dd6c8d9a8",
                required_helpers: &["local-mcp", "atomic-publish"],
                classification: "supported",
                experimental: false,
            },
            Target::MacosAarch64 => TargetFacts {
                key: "macos-aarch64",
                asset_name: "local-mcp-v0.1.2-macos-aarch64.tar.gz",
                sha256: "21400b2fb61e865484055e470ca78f3d289882fda335ed6e5e4cbc37eef8a787",
                required_helpers: &["local-mcp", "atomic-publish"],
                classification: "ci_target",
                experimental: false,
            },
            Target::WindowsX86_64 => TargetFacts {
                key: "windows-x86_64",
                asset_name: "local-mcp-v0.1.2-windows-x86_64.zip",
                sha256: "f5d2e3e20242956ca34b1fe2d9de479ca1a38fad84b99ddb619204cf0cd4342a",
                required_helpers: &["local-mcp.exe", "atomic-publish.exe"],
                classification: "experimental",
                experimental: true,
            },
            Target::WindowsAarch64 => TargetFacts {
                key: "windows-aarch64",
                asset_name: "local-mcp-v0.1.2-windows-aarch64.zip",
                sha256: "4288ba681bd47fdfc8c08d923c5870bd1693c2dadac17f975d661024b522841c",
                required_helpers: &["local-mcp.exe", "atomic-publish.exe"],
                classification: "experimental",
                experimental: true,
            },
        }
    }

    fn detect() -> Option<Target> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Some(Target::LinuxX86_64),
            ("linux", "aarch64") => Some(Target::LinuxAarch64),
            ("macos", "x86_64") => Some(Target::MacosX86_64),
            ("macos", "aarch64") => Some(Target::MacosAarch64),
            ("windows", "x86_64") => Some(Target::WindowsX86_64),
            ("windows", "aarch64") => Some(Target::WindowsAarch64),
            _ => None,
        }
    }

    fn key(self) -> &'static str {
        self.facts().key
    }
}

// ---------------------------------------------------------------------------
// Manifest parsing (bounded, duplicate-key rejecting)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    fn as_object(&self) -> Option<&[(String, JsonValue)]> {
        match self {
            JsonValue::Object(entries) => Some(entries),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::String(value) => Some(value),
            _ => None,
        }
    }

    fn get<'a>(&'a self, key: &str) -> Option<&'a JsonValue> {
        self.as_object()
            .and_then(|entries| entries.iter().find(|(candidate, _)| candidate == key))
            .map(|(_, value)| value)
    }
}

struct JsonVisitor;

impl<'de> serde::de::Visitor<'de> for JsonVisitor {
    type Value = JsonValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        JsonValue::deserialize(deserializer)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(JsonValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        match serde_json::Number::from_f64(value) {
            Some(number) => Ok(JsonValue::Number(number)),
            None => Err(E::custom("non-finite number")),
        }
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(JsonValue::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(JsonValue::String(value))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(JsonValue::Array(items))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut entries: Vec<(String, JsonValue)> = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, JsonValue>()? {
            if entries.iter().any(|(existing, _)| *existing == key) {
                return Err(serde::de::Error::custom(format!("duplicate key: {key}")));
            }
            entries.push((key, value));
        }
        Ok(JsonValue::Object(entries))
    }
}

impl<'de> Deserialize<'de> for JsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(JsonVisitor)
    }
}

fn parse_json_strict(bytes: &[u8]) -> Result<JsonValue, ()> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = JsonValue::deserialize(&mut deserializer).map_err(|_| ())?;
    // Reject trailing data after the top-level value.
    deserializer.end().map_err(|_| ())?;
    Ok(value)
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Characters that can rewrite, hide, or reorder human terminal output. JSON
/// serialization already escapes C0 controls, but the human error path must not
/// echo raw escape or bidi/format sequences supplied by an operator manifest.
fn is_display_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

fn sanitize_for_error(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        if is_display_control(character) {
            for escaped in character.escape_default() {
                output.push(escaped);
            }
        } else {
            output.push(character);
        }
    }
    output
}

fn truncate_for_error(value: &str) -> String {
    let sanitized = sanitize_for_error(value);
    let truncated: String = sanitized.chars().take(MAX_ECHO_CHARS).collect();
    if sanitized.chars().count() > MAX_ECHO_CHARS {
        format!("{truncated}… (truncated)")
    } else {
        truncated
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ManifestAsset {
    name: String,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Manifest {
    version: String,
    assets: BTreeMap<String, ManifestAsset>,
}

fn manifest_from_json(value: JsonValue) -> Result<Manifest, Failure> {
    let root_keys: Vec<&str> = value
        .as_object()
        .ok_or_else(|| Failure::manifest_invalid("root", "top-level value must be an object"))?
        .iter()
        .map(|(key, _)| key.as_str())
        .collect();
    for expected in ["schema_version", "version", "assets"] {
        if value.get(expected).is_none() {
            return Err(Failure::manifest_invalid(
                expected,
                "required key is missing",
            ));
        }
    }
    for key in root_keys {
        if !matches!(key, "schema_version" | "version" | "assets") {
            return Err(Failure::manifest_invalid(
                "manifest",
                &format!("unexpected key: {}", truncate_for_error(key)),
            ));
        }
    }
    let schema_version = value
        .get("schema_version")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| Failure::manifest_invalid("schema_version", "must be a string"))?;
    if schema_version != "1.0.0" {
        return Err(Failure::manifest_invalid(
            "schema_version",
            &format!(
                "unsupported schema_version: {}",
                truncate_for_error(schema_version)
            ),
        ));
    }
    let version = value
        .get("version")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| Failure::manifest_invalid("version", "must be a string"))?;
    if version.is_empty() || version.chars().count() > 64 {
        return Err(Failure::manifest_invalid(
            "version",
            "must be 1-64 characters",
        ));
    }
    let assets_value = value
        .get("assets")
        .ok_or_else(|| Failure::manifest_invalid("assets", "required key is missing"))?;
    let asset_entries = assets_value
        .as_object()
        .ok_or_else(|| Failure::manifest_invalid("assets", "must be an object"))?;
    let mut assets = BTreeMap::new();
    for (target_key, asset_value) in asset_entries {
        if Target::ALL.iter().all(|target| target.key() != target_key) {
            return Err(Failure::manifest_invalid(
                "assets",
                &format!("unexpected target: {}", truncate_for_error(target_key)),
            ));
        }
        let asset_keys: Vec<&str> = asset_value
            .as_object()
            .ok_or_else(|| {
                Failure::manifest_invalid(&format!("assets.{target_key}"), "must be an object")
            })?
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        for expected in ["name", "sha256"] {
            if asset_value.get(expected).is_none() {
                return Err(Failure::manifest_invalid(
                    &format!("assets.{target_key}.{expected}"),
                    "required key is missing",
                ));
            }
        }
        for key in asset_keys {
            if !matches!(key, "name" | "sha256") {
                return Err(Failure::manifest_invalid(
                    &format!("assets.{target_key}"),
                    &format!("unexpected key: {}", truncate_for_error(key)),
                ));
            }
        }
        let name = asset_value
            .get("name")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                Failure::manifest_invalid(&format!("assets.{target_key}.name"), "must be a string")
            })?;
        let sha256 = asset_value
            .get("sha256")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                Failure::manifest_invalid(
                    &format!("assets.{target_key}.sha256"),
                    "must be a string",
                )
            })?;
        if name.is_empty() || name.chars().count() > 256 || name.contains(['/', '\\']) {
            return Err(Failure::manifest_invalid(
                &format!("assets.{target_key}.name"),
                "must be 1-256 characters without path separators",
            ));
        }
        if !is_sha256_hex(sha256) {
            return Err(Failure::manifest_invalid(
                &format!("assets.{target_key}.sha256"),
                "must be 64 lowercase hex characters",
            ));
        }
        assets.insert(
            target_key.clone(),
            ManifestAsset {
                name: name.to_owned(),
                sha256: sha256.to_owned(),
            },
        );
    }
    for target in Target::ALL {
        if !assets.contains_key(target.key()) {
            return Err(Failure::manifest_invalid(
                &format!("assets.{}", target.key()),
                "required target is missing from the manifest",
            ));
        }
    }
    Ok(Manifest {
        version: version.to_owned(),
        assets,
    })
}

/// Consistency check between a parsed manifest and the embedded pin.
/// Returns the first mismatch as a `PinMismatch` failure naming the key.
fn check_pin(manifest: &Manifest) -> Result<(), Failure> {
    if manifest.version != PINNED_VERSION {
        return Err(Failure::pin_mismatch(
            "version",
            PINNED_VERSION,
            &manifest.version,
        ));
    }
    for target in Target::ALL {
        let facts = target.facts();
        let asset = manifest
            .assets
            .get(facts.key)
            .expect("manifest validated to contain all targets");
        if asset.name != facts.asset_name {
            return Err(Failure::pin_mismatch(
                &format!("assets.{}.name", facts.key),
                facts.asset_name,
                &asset.name,
            ));
        }
        if asset.sha256 != facts.sha256 {
            return Err(Failure::pin_mismatch(
                &format!("assets.{}.sha256", facts.key),
                facts.sha256,
                &asset.sha256,
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Failure model (fail-closed)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FailureKind {
    ManifestInvalid,
    PinMismatch,
    UnsupportedPlatform,
    ArchiveMismatch,
    ArchiveUnreadable,
    MetadataChanged,
    DoctorUnparseable,
    DoctorUnsupported,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Failure {
    pub(crate) kind: FailureKind,
    pub(crate) check: String,
    pub(crate) expected: String,
    pub(crate) actual: String,
    pub(crate) message: String,
}

impl Failure {
    fn manifest_invalid(check: &str, message: &str) -> Self {
        Self {
            kind: FailureKind::ManifestInvalid,
            check: format!("manifest.{check}"),
            expected: "a valid, complete v0.1.2 release manifest".to_owned(),
            actual: truncate_for_error(message),
            message: "The release manifest is missing, unreadable, or invalid.".to_owned(),
        }
    }

    fn pin_mismatch(check: &str, expected: &str, actual: &str) -> Self {
        Self {
            kind: FailureKind::PinMismatch,
            check: check.to_owned(),
            expected: truncate_for_error(expected),
            actual: truncate_for_error(actual),
            message: "The release manifest disagrees with the embedded v0.1.2 pin.".to_owned(),
        }
    }

    fn unsupported_platform(detected: &str, supported: &str) -> Self {
        Self {
            kind: FailureKind::UnsupportedPlatform,
            check: "platform".to_owned(),
            expected: supported.to_owned(),
            actual: detected.to_owned(),
            message: "This platform has no pinned release target.".to_owned(),
        }
    }

    fn archive_mismatch(expected: &str, actual: &str) -> Self {
        Self {
            kind: FailureKind::ArchiveMismatch,
            check: "archive.sha256".to_owned(),
            expected: expected.to_owned(),
            actual: actual.to_owned(),
            message: "The archive's SHA-256 does not match the pinned release digest.".to_owned(),
        }
    }

    fn archive_unreadable(detail: &str) -> Self {
        Self {
            kind: FailureKind::ArchiveUnreadable,
            check: "archive".to_owned(),
            expected: format!("a readable regular file of at most {MAX_ARCHIVE_BYTES} bytes"),
            actual: truncate_for_error(detail),
            message: "The archive could not be hashed.".to_owned(),
        }
    }

    fn metadata_changed(first: &str, second: &str) -> Self {
        Self {
            kind: FailureKind::MetadataChanged,
            check: "manifest.double_read".to_owned(),
            expected: format!("identical bytes on both reads (first sha256: {first})"),
            actual: format!("second sha256: {second}"),
            message: "Release metadata changed between reads; refusing to render.".to_owned(),
        }
    }

    fn doctor_unparseable() -> Self {
        Self {
            kind: FailureKind::DoctorUnparseable,
            check: "doctor".to_owned(),
            expected: "a parseable Doctor Core V1 report".to_owned(),
            actual: "the diagnostic worker failed".to_owned(),
            message: "Doctor could not produce a report; no diagnostics are fabricated.".to_owned(),
        }
    }

    fn doctor_unsupported(check: &str, actual: &str) -> Self {
        Self {
            kind: FailureKind::DoctorUnsupported,
            check: format!("doctor.{check}"),
            expected: format!(
                "Doctor schema {SUPPORTED_DOCTOR_SCHEMA_VERSION} / diagnostic_contract_version {SUPPORTED_DOCTOR_CONTRACT_VERSION}"
            ),
            actual: truncate_for_error(actual),
            message: "The embedded Doctor report has an unsupported contract version; refusing to render."
                .to_owned(),
        }
    }

    /// Exit code for this failure class: 64 for usage/input, 70 for internal.
    fn exit_code(&self) -> u8 {
        match self.kind {
            FailureKind::DoctorUnparseable | FailureKind::DoctorUnsupported => 70,
            _ => 64,
        }
    }
}

/// The Preview consumes the typed Doctor report rather than parsing a payload,
/// but it still refuses to embed a report whose contract version it does not
/// know (Design §5.1 / N-8). The doctor identity strings must be present.
fn validate_doctor_contract(
    schema_version: &str,
    diagnostic_contract_version: u8,
    doctor_version: &str,
    goallatch_version: &str,
) -> Result<(), Failure> {
    if schema_version != SUPPORTED_DOCTOR_SCHEMA_VERSION {
        return Err(Failure::doctor_unsupported(
            "schema_version",
            schema_version,
        ));
    }
    if diagnostic_contract_version != SUPPORTED_DOCTOR_CONTRACT_VERSION {
        return Err(Failure::doctor_unsupported(
            "diagnostic_contract_version",
            &diagnostic_contract_version.to_string(),
        ));
    }
    if doctor_version.is_empty() {
        return Err(Failure::doctor_unsupported(
            "doctor_version",
            "empty doctor identity",
        ));
    }
    if goallatch_version.is_empty() {
        return Err(Failure::doctor_unsupported(
            "goallatch_version",
            "empty product identity",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Bounded file reads and hashing
// ---------------------------------------------------------------------------

fn read_file_bounded(path: &std::path::Path, limit: u64) -> Result<Vec<u8>, String> {
    let metadata = fs::metadata(path).map_err(|error| error.kind().to_string())?;
    if !metadata.is_file() {
        return Err("not a regular file".to_owned());
    }
    if metadata.len() > limit {
        return Err(format!("larger than the {limit}-byte limit"));
    }
    let mut file = fs::File::open(path).map_err(|error| error.kind().to_string())?;
    let mut buffer = Vec::with_capacity(metadata.len().min(limit) as usize + 1);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut chunk)
            .map_err(|error| error.kind().to_string())?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() as u64 > limit {
            return Err(format!("larger than the {limit}-byte limit"));
        }
    }
    Ok(buffer)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    to_hex(&digest)
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// Hash an archive in a single bounded streaming pass. The file is never
/// extracted, opened for members, or executed.
fn hash_archive(path: &std::path::Path) -> Result<String, Failure> {
    hash_archive_with_limit(path, MAX_ARCHIVE_BYTES)
}

/// Bounded variant of [`hash_archive`]; the limit parameter exists so tests
/// can exercise the cap without allocating 256 MiB fixtures.
fn hash_archive_with_limit(path: &std::path::Path, limit: u64) -> Result<String, Failure> {
    let metadata = fs::metadata(path)
        .map_err(|error| Failure::archive_unreadable(&error.kind().to_string()))?;
    if !metadata.is_file() {
        return Err(Failure::archive_unreadable("not a regular file"));
    }
    if metadata.len() > limit {
        return Err(Failure::archive_unreadable(&format!(
            "larger than the {limit}-byte limit"
        )));
    }
    let mut file = fs::File::open(path)
        .map_err(|error| Failure::archive_unreadable(&error.kind().to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; IO_BUFFER.min(1 << 20)];
    let mut total: u64 = 0;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| Failure::archive_unreadable(&error.kind().to_string()))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > limit {
            return Err(Failure::archive_unreadable(&format!(
                "larger than the {limit}-byte limit"
            )));
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

// ---------------------------------------------------------------------------
// Install-root trust evaluation (V1: never PASS)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum InstallRootTrustStatus {
    Unknown,
    Untrusted,
}

impl InstallRootTrustStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::Untrusted => "UNTRUSTED",
        }
    }
}

/// Synthetic trust metadata for the install-root evaluation. V1 never
/// supplies real metadata from the CLI; the evaluator is exercised by unit
/// tests and always yields non-PASS statuses. PASS is reserved for future
/// Security V3 machinery and is intentionally unrepresentable here.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct InstallRootTrustMeta {
    pub(crate) world_writable: Option<bool>,
    pub(crate) replaced_after_check: Option<bool>,
    pub(crate) reparse_or_symlink: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InstallRootTrust {
    pub(crate) status: InstallRootTrustStatus,
    pub(crate) reason_code: &'static str,
}

/// Evaluate install-root trust metadata. Fail-closed: any negative evidence
/// yields UNTRUSTED; absent evidence yields UNKNOWN. PASS is never returned.
pub(crate) fn evaluate_install_root_trust(meta: Option<InstallRootTrustMeta>) -> InstallRootTrust {
    match meta {
        None => InstallRootTrust {
            status: InstallRootTrustStatus::Unknown,
            reason_code: "INSTALL_ROOT_NOT_EVALUATED",
        },
        Some(meta) => {
            if meta.world_writable == Some(true)
                || meta.replaced_after_check == Some(true)
                || meta.reparse_or_symlink == Some(true)
            {
                InstallRootTrust {
                    status: InstallRootTrustStatus::Untrusted,
                    reason_code: "INSTALL_ROOT_NEGATIVE_EVIDENCE",
                }
            } else {
                InstallRootTrust {
                    status: InstallRootTrustStatus::Unknown,
                    reason_code: "INSTALL_ROOT_UNVERIFIED",
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JSON envelope
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PlatformSection {
    os: &'static str,
    arch: &'static str,
    target: &'static str,
    supported: bool,
    experimental: bool,
    classification: &'static str,
}

#[derive(Serialize)]
struct ReleaseSection {
    version: &'static str,
    target: &'static str,
    asset: &'static str,
    sha256: &'static str,
    experimental: bool,
}

#[derive(Serialize)]
struct ArtifactVerificationSection {
    status: &'static str,
    manifest_present: bool,
    pin_consistent: bool,
    archive_present: bool,
    archive_sha256: Option<String>,
    expected_sha256: &'static str,
    detail: &'static str,
}

#[derive(Serialize)]
struct InstallContractSection {
    required_helpers: &'static [&'static str],
    summary: &'static str,
}

#[derive(Serialize)]
struct EvidenceSection {
    doctor_embedded: bool,
    manifest_source: &'static str,
    double_read_consistent: Option<bool>,
    archive_hashed_in_run: bool,
}

#[derive(Serialize)]
struct InstallRootTrustSection {
    status: &'static str,
    reason_code: &'static str,
}

#[derive(Serialize)]
struct PreviewEnvelope<'a> {
    schema_version: &'static str,
    preview_contract_version: u8,
    goallatch_version: &'static str,
    platform: PlatformSection,
    release: ReleaseSection,
    artifact_verification: ArtifactVerificationSection,
    install_contract: InstallContractSection,
    doctor: &'a doctor::DoctorReport,
    overall_preview_result: &'static str,
    evidence: EvidenceSection,
    install_root_trust: InstallRootTrustSection,
    residual_warnings: Vec<String>,
}

#[derive(Serialize)]
struct FailureSection {
    check: String,
    expected: String,
    actual: String,
    message: String,
}

#[derive(Serialize)]
struct FailureEnvelope {
    schema_version: &'static str,
    preview_contract_version: u8,
    goallatch_version: &'static str,
    platform: Option<PlatformSection>,
    overall_preview_result: &'static str,
    failure: FailureSection,
    residual_warnings: Vec<String>,
}

fn residual_warnings(target: Target, artifact_verified: bool) -> Vec<String> {
    let facts = target.facts();
    let mut warnings = Vec::new();
    warnings.push("A Doctor PASS is not an installation authorization.".to_owned());
    warnings.push(
        "Doctor payload authenticity is not established; schema conformance is shape-only."
            .to_owned(),
    );
    if artifact_verified {
        warnings.push(
            "Archive SHA-256 matches the pin for the bytes read in this run; this does not prove the file will remain unchanged or is safe to install."
                .to_owned(),
        );
    } else {
        warnings.push(
            "Archive bytes were not verified in this run; supply --archive PATH to check a local file (never extracted or executed)."
                .to_owned(),
        );
    }
    warnings.push(
        "Helper check-to-open (TOCTOU) and ancestor-directory redirection remain unmitigated (Security V2 residual risks)."
            .to_owned(),
    );
    match facts.classification {
        "experimental" => warnings.push(format!(
            "The {0} target is Experimental; it has no Unix-equivalent process sandbox or is not closure-validated.",
            facts.key
        )),
        "ci_target" => warnings.push(
            "macOS Apple Silicon is a CI target and is not claimed as locally release-closure validated."
                .to_owned(),
        ),
        _ => {}
    }
    warnings
}

fn platform_section(target: Target) -> PlatformSection {
    let facts = target.facts();
    let (os, arch) = match target {
        Target::LinuxX86_64 | Target::LinuxAarch64 => (
            "linux",
            if target == Target::LinuxX86_64 {
                "x86_64"
            } else {
                "aarch64"
            },
        ),
        Target::MacosX86_64 | Target::MacosAarch64 => (
            "macos",
            if target == Target::MacosX86_64 {
                "x86_64"
            } else {
                "aarch64"
            },
        ),
        Target::WindowsX86_64 | Target::WindowsAarch64 => (
            "windows",
            if target == Target::WindowsX86_64 {
                "x86_64"
            } else {
                "aarch64"
            },
        ),
    };
    PlatformSection {
        os,
        arch,
        target: facts.key,
        // Every Target in this table is a known release target; the
        // unsupported-platform case is a failure screen with no platform
        // section. Experimental/CI-only caveats are carried separately, matching
        // the design example (Windows: supported=true, experimental=true).
        supported: true,
        experimental: facts.experimental,
        classification: facts.classification,
    }
}

/// V1 never establishes install-root trust (`PASS` is intentionally
/// unrepresentable), so `RENDERED_VERIFIED` is not emitted even when the archive
/// digest matched. This is the N-10 rule; the authoritative artifact fact stays
/// in `artifact_verification.status`.
fn overall_preview_result(required_blockers: usize, required_unknowns: usize) -> &'static str {
    if required_blockers > 0 {
        "DOCTOR_BLOCK"
    } else if required_unknowns > 0 {
        "DOCTOR_REQUIRED_UNKNOWN"
    } else {
        "RENDERED_UNVERIFIED"
    }
}

struct PreviewOutcome {
    target: Target,
    artifact: ArtifactOutcome,
    manifest_source: &'static str,
    doctor_report: doctor::DoctorReport,
    install_root_trust: InstallRootTrust,
}

#[derive(Debug)]
enum ArtifactOutcome {
    VerifiedArchive { sha256: String },
    PinnedUnverified,
}

impl ArtifactOutcome {
    fn verified(&self) -> bool {
        matches!(self, Self::VerifiedArchive { .. })
    }

    fn status(&self) -> &'static str {
        match self {
            Self::VerifiedArchive { .. } => "VERIFIED_ARCHIVE",
            Self::PinnedUnverified => "PINNED_UNVERIFIED",
        }
    }

    fn archive_sha256(&self) -> Option<String> {
        match self {
            Self::VerifiedArchive { sha256 } => Some(sha256.clone()),
            Self::PinnedUnverified => None,
        }
    }
}

fn load_manifest(
    options: &SetupPreviewOptions,
) -> Result<(Manifest, &'static str, Option<Vec<u8>>), Failure> {
    let Some(path) = options.manifest.as_deref() else {
        // Embedded pin: all six targets, verified against the release.
        let assets = Target::ALL
            .iter()
            .map(|target| {
                let facts = target.facts();
                (
                    facts.key.to_owned(),
                    ManifestAsset {
                        name: facts.asset_name.to_owned(),
                        sha256: facts.sha256.to_owned(),
                    },
                )
            })
            .collect();
        return Ok((
            Manifest {
                version: PINNED_VERSION.to_owned(),
                assets,
            },
            "embedded",
            None,
        ));
    };

    // First read (load time). The bytes are retained for the pre-render
    // byte comparison in `verify_manifest_unchanged` (Design §6.2 / N-13).
    let first = read_file_bounded(path, MAX_MANIFEST_BYTES)
        .map_err(|detail| Failure::manifest_invalid("read", &detail))?;
    let parsed = parse_json_strict(&first)
        .map_err(|()| Failure::manifest_invalid("parse", "not valid JSON (or duplicate keys)"))?;
    let manifest = manifest_from_json(parsed)?;
    check_pin(&manifest)?;
    Ok((manifest, "operator", Some(first)))
}

/// Re-read an operator-supplied manifest immediately before output assembly and
/// byte-compare it with the load-time read. The embedded pin is an immutable
/// constant and is not re-read.
fn verify_manifest_unchanged(
    options: &SetupPreviewOptions,
    first: Option<&[u8]>,
) -> Result<Option<bool>, Failure> {
    let (Some(path), Some(first)) = (options.manifest.as_deref(), first) else {
        return Ok(None);
    };
    let second = read_file_bounded(path, MAX_MANIFEST_BYTES)
        .map_err(|detail| Failure::manifest_invalid("re-read", &detail))?;
    if second != first {
        return Err(Failure::metadata_changed(
            &sha256_hex(first)[..16],
            &sha256_hex(&second)[..16],
        ));
    }
    Ok(Some(true))
}

/// Decide the artifact verification outcome for a computed digest against the
/// pinned expectation. Factored out so unit tests can exercise both branches
/// without the real v0.1.2 asset.
fn archive_outcome(digest: &str, expected: &str) -> Result<ArtifactOutcome, Failure> {
    if digest != expected {
        return Err(Failure::archive_mismatch(expected, digest));
    }
    Ok(ArtifactOutcome::VerifiedArchive {
        sha256: digest.to_owned(),
    })
}

fn verify_archive(
    options: &SetupPreviewOptions,
    target: Target,
) -> Result<ArtifactOutcome, Failure> {
    let Some(path) = options.archive.as_deref() else {
        return Ok(ArtifactOutcome::PinnedUnverified);
    };
    let digest = hash_archive(path)?;
    archive_outcome(&digest, target.facts().sha256)
}

// ---------------------------------------------------------------------------
// Human rendering (fixed layout; see Design §7.1)
// ---------------------------------------------------------------------------

fn render_human(outcome: &PreviewOutcome) -> String {
    use std::fmt::Write as _;

    let facts = outcome.target.facts();
    let trust = &outcome.install_root_trust;
    let doctor_report = &outcome.doctor_report;
    let summary = &doctor_report.summary;

    let mut out = String::new();
    let _ = writeln!(out, "local-mcp setup preview");
    let _ = writeln!(out, "DRY RUN — no installation will be performed");
    let _ = writeln!(
        out,
        "No files will be modified; the archive is never extracted or executed."
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Platform: {} {} ({}; target {})",
        os_label(outcome.target),
        arch_label(outcome.target),
        facts.classification,
        facts.key
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "Pinned release: v{PINNED_VERSION}");
    let _ = writeln!(out, "  asset: {}", facts.asset_name);
    let _ = writeln!(out, "  sha256: {}", facts.sha256);
    match &outcome.artifact {
        ArtifactOutcome::VerifiedArchive { sha256 } => {
            let _ = writeln!(
                out,
                "  artifact verification: VERIFIED_ARCHIVE — bytes hashed in this run; sha256 matches the pin"
            );
            let _ = writeln!(out, "  archive sha256: {sha256}");
        }
        ArtifactOutcome::PinnedUnverified => {
            let _ = writeln!(
                out,
                "  artifact verification: PINNED_UNVERIFIED — archive bytes were NOT checked in this run"
            );
            let _ = writeln!(
                out,
                "  (supply --archive PATH to hash a local file; the archive is never extracted or executed)"
            );
        }
    }
    let _ = writeln!(out, "  manifest source: {}", outcome.manifest_source);
    let _ = writeln!(out, "  pin consistent: yes");
    let _ = writeln!(out);
    let _ = writeln!(out, "Install Contract (this platform):");
    let _ = writeln!(
        out,
        "  required helpers: {}",
        facts.required_helpers.join(", ")
    );
    let _ = writeln!(
        out,
        "  note: helpers must be installed together next to the executable; final entries that are symlinks/reparse points are refused (Security V2)"
    );
    if outcome.target == Target::LinuxX86_64 || outcome.target == Target::LinuxAarch64 {
        let _ = writeln!(
            out,
            "  Linux requires upstream Bubblewrap 0.12.0+ and user/PID/network namespaces with seccomp"
        );
    }
    if facts.experimental {
        let _ = writeln!(
            out,
            "  Experimental platform: see the v0.1.2 release notes before relying on it"
        );
    }
    if facts.classification == "ci_target" {
        let _ = writeln!(
            out,
            "  macOS Apple Silicon is a CI target; it is not claimed as locally release-closure validated"
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Doctor (embedded; verbatim outcomes — NOT an installation authorization):"
    );
    let _ = writeln!(
        out,
        "  outcome: {} (required: {}) · advisory: {} · exit-code implication: {}",
        summary.status.as_str(),
        summary.required_status.as_str(),
        summary.advisory_status.as_str(),
        summary.exit_code.value()
    );
    let _ = writeln!(
        out,
        "  counts: PASS {} · WARN {} · BLOCK {} · UNKNOWN {} ({} checks)",
        summary.counts.pass,
        summary.counts.warn,
        summary.counts.block,
        summary.counts.unknown,
        summary.checks_total
    );
    let _ = writeln!(
        out,
        "  required blockers: {} · required unknowns: {}",
        summary.required_blockers, summary.required_unknowns
    );
    let _ = writeln!(
        out,
        "  severity legend: PASS=established; WARN=advisory; BLOCK=required failure; UNKNOWN=not established"
    );
    let mut listed = 0usize;
    for check in &doctor_report.checks {
        let flagged = matches!(
            check.status,
            doctor::DiagnosticStatus::Block | doctor::DiagnosticStatus::Unknown
        );
        if flagged {
            let requirement = if check.requirement == doctor::Requirement::Required {
                "required"
            } else {
                "advisory"
            };
            let _ = writeln!(
                out,
                "  - [{requirement}] {}: {} — {}",
                check.id,
                check.status.as_str(),
                check.reason_code.as_str()
            );
            listed += 1;
        }
    }
    if listed == 0 {
        let _ = writeln!(out, "  - no BLOCK or UNKNOWN checks");
    }
    let _ = writeln!(
        out,
        "  authenticity: Doctor payload authenticity is not established (schema validation is shape-only)"
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "Confirmed items:");
    let _ = writeln!(out, "  - release version pinned: {PINNED_VERSION}");
    let _ = writeln!(
        out,
        "  - asset name and SHA-256 pin recorded for this target"
    );
    let _ = writeln!(out, "  - platform recognized: {}", facts.key);
    if outcome.artifact.verified() {
        let _ = writeln!(
            out,
            "  - archive bytes hashed in this run; digest matches the pin"
        );
    }
    let _ = writeln!(out, "Unconfirmed items:");
    if !outcome.artifact.verified() {
        let _ = writeln!(
            out,
            "  - archive bytes were NOT verified (no --archive supplied or hash unavailable)"
        );
    }
    let _ = writeln!(
        out,
        "  - install-root trust: {} ({})",
        trust.status.as_str(),
        trust.reason_code
    );
    let _ = writeln!(
        out,
        "  - helper presence/layout of a future install location (not evaluated)"
    );
    let _ = writeln!(
        out,
        "  - Doctor payload authenticity (unverifiable by this preview)"
    );
    let _ = writeln!(
        out,
        "  - runtime behavior, sandbox closure, and client compatibility on this host"
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "If a real setup were to proceed (hypothetical; nothing below is executed):"
    );
    let _ = writeln!(
        out,
        "  1. the operator downloads the archive and verifies it against SHA256SUMS;"
    );
    let _ = writeln!(
        out,
        "  2. the archive is extracted and all required helpers are installed together;"
    );
    let _ = writeln!(
        out,
        "  3. install-root ownership/ancestry is established (fail-closed otherwise);"
    );
    let _ = writeln!(
        out,
        "  4. a fresh, explicit Owner approval is given at execution time;"
    );
    let _ = writeln!(
        out,
        "  5. every precondition is re-checked immediately before any mutation."
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "Security restrictions and residual risks:");
    for warning in residual_warnings(outcome.target, outcome.artifact.verified()) {
        let _ = writeln!(out, "  - {warning}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "A Doctor PASS is not an installation authorization.");
    out
}

const fn os_label(target: Target) -> &'static str {
    match target {
        Target::LinuxX86_64 | Target::LinuxAarch64 => "Linux",
        Target::MacosX86_64 | Target::MacosAarch64 => "macOS",
        Target::WindowsX86_64 | Target::WindowsAarch64 => "Windows",
    }
}

const fn arch_label(target: Target) -> &'static str {
    match target {
        Target::LinuxX86_64 | Target::MacosX86_64 | Target::WindowsX86_64 => "x86_64",
        Target::LinuxAarch64 | Target::MacosAarch64 | Target::WindowsAarch64 => "aarch64",
    }
}

// ---------------------------------------------------------------------------
// Envelope assembly and failure rendering
// ---------------------------------------------------------------------------

fn build_failure_envelope(failure: &Failure, platform: Option<Target>) -> FailureEnvelope {
    let goallatch_version = env!("CARGO_PKG_VERSION");
    FailureEnvelope {
        schema_version: PREVIEW_SCHEMA_VERSION,
        preview_contract_version: PREVIEW_CONTRACT_VERSION,
        goallatch_version,
        platform: platform.map(platform_section),
        overall_preview_result: "FAILED",
        failure: FailureSection {
            check: failure.check.clone(),
            expected: failure.expected.clone(),
            actual: failure.actual.clone(),
            message: failure.message.clone(),
        },
        residual_warnings: vec![
            "A Doctor PASS is not an installation authorization.".to_owned(),
            "No installation was performed and no files were modified.".to_owned(),
        ],
    }
}

fn render_failure_human(failure: &Failure) -> String {
    let mut out = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(
        out,
        "local-mcp setup preview — FAILED (no installation performed)"
    );
    let _ = writeln!(out, "check: {}", failure.check);
    let _ = writeln!(out, "expected: {}", failure.expected);
    let _ = writeln!(out, "actual: {}", failure.actual);
    let _ = writeln!(out, "{}", failure.message);
    let _ = writeln!(
        out,
        "No installation was performed and no files were modified."
    );
    let _ = writeln!(out, "A Doctor PASS is not an installation authorization.");
    out
}

fn write_stdout(bytes: &[u8]) -> Result<(), ()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes).map_err(|_| ())?;
    stdout.flush().map_err(|_| ())
}

fn write_stderr(text: &str) -> Result<(), ()> {
    let mut stderr = io::stderr().lock();
    stderr.write_all(text.as_bytes()).map_err(|_| ())
}

fn emit_failure(failure: Failure, options: &SetupPreviewOptions) -> u8 {
    let platform = Target::detect();
    if options.json {
        let envelope = build_failure_envelope(&failure, platform);
        match serde_json::to_vec_pretty(&envelope) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                if bytes.len() > JSON_OUTPUT_LIMIT {
                    let _ = write_stderr("preview failure envelope exceeded the output bound\n");
                    return ExitCode::InternalError.value();
                }
                if write_stdout(&bytes).is_err() {
                    return ExitCode::InternalError.value();
                }
                return failure.exit_code();
            }
            Err(_) => {
                let _ = write_stderr("preview failure envelope could not be serialized\n");
                return ExitCode::InternalError.value();
            }
        }
    } else if write_stderr(&render_failure_human(&failure)).is_err() {
        return ExitCode::InternalError.value();
    }
    failure.exit_code()
}

fn execute(options: SetupPreviewOptions) -> u8 {
    let op_platform = Target::detect();

    let target = match op_platform {
        Some(target) => target,
        None => {
            let detected = format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH);
            let supported = Target::ALL
                .iter()
                .map(|target| target.key())
                .collect::<Vec<_>>()
                .join(", ");
            return emit_failure(
                Failure::unsupported_platform(&detected, &supported),
                &options,
            );
        }
    };

    let (_manifest, manifest_source, manifest_first_bytes) = match load_manifest(&options) {
        Ok(loaded) => loaded,
        Err(failure) => return emit_failure(failure, &options),
    };
    // The embedded-pin consistency check ran inside load_manifest. The
    // operator-manifest double-read is repeated immediately before rendering
    // below (Design §6.2 / N-13).

    let artifact = match verify_archive(&options, target) {
        Ok(artifact) => artifact,
        Err(failure) => return emit_failure(failure, &options),
    };

    let doctor_report = match doctor::collect_report(doctor::headless_options()) {
        Ok(report) => report,
        Err(()) => return emit_failure(Failure::doctor_unparseable(), &options),
    };
    if let Err(failure) = validate_doctor_contract(
        doctor_report.schema_version,
        doctor_report.diagnostic_contract_version,
        doctor_report.doctor_version,
        doctor_report.goallatch_version,
    ) {
        return emit_failure(failure, &options);
    }

    let double_read_consistent =
        match verify_manifest_unchanged(&options, manifest_first_bytes.as_deref()) {
            Ok(consistent) => consistent,
            Err(failure) => return emit_failure(failure, &options),
        };

    let install_root_trust = evaluate_install_root_trust(None);
    let facts = target.facts();
    let artifact_verified = artifact.verified();
    let goallatch_version = env!("CARGO_PKG_VERSION");

    if options.json {
        let envelope = PreviewEnvelope {
            schema_version: PREVIEW_SCHEMA_VERSION,
            preview_contract_version: PREVIEW_CONTRACT_VERSION,
            goallatch_version,
            platform: platform_section(target),
            release: ReleaseSection {
                version: PINNED_VERSION,
                target: facts.key,
                asset: facts.asset_name,
                sha256: facts.sha256,
                experimental: facts.experimental,
            },
            artifact_verification: ArtifactVerificationSection {
                status: artifact.status(),
                manifest_present: true,
                pin_consistent: true,
                archive_present: artifact_verified,
                archive_sha256: artifact.archive_sha256(),
                expected_sha256: facts.sha256,
                detail: if artifact_verified {
                    "archive bytes hashed in this run; digest matches the pin"
                } else {
                    "archive bytes not verified in this run"
                },
            },
            install_contract: InstallContractSection {
                required_helpers: facts.required_helpers,
                summary: "helpers must be installed together next to the executable; symlinks/reparse points are refused; no install is performed by this preview",
            },
            doctor: &doctor_report,
            overall_preview_result: overall_preview_result(
                doctor_report.summary.required_blockers,
                doctor_report.summary.required_unknowns,
            ),
            evidence: EvidenceSection {
                doctor_embedded: true,
                manifest_source,
                double_read_consistent,
                archive_hashed_in_run: artifact_verified,
            },
            install_root_trust: InstallRootTrustSection {
                status: install_root_trust.status.as_str(),
                reason_code: install_root_trust.reason_code,
            },
            residual_warnings: residual_warnings(target, artifact_verified),
        };
        match serde_json::to_vec_pretty(&envelope) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                if bytes.len() > JSON_OUTPUT_LIMIT {
                    let _ = write_stderr("preview JSON exceeded the 1 MiB output bound\n");
                    return ExitCode::InternalError.value();
                }
                if write_stdout(&bytes).is_err() {
                    return ExitCode::InternalError.value();
                }
                return ExitCode::Rendered.value();
            }
            Err(_) => {
                let _ = write_stderr("preview JSON could not be serialized\n");
                return ExitCode::InternalError.value();
            }
        }
    }

    let outcome = PreviewOutcome {
        target,
        artifact,
        manifest_source,
        doctor_report,
        install_root_trust,
    };
    let output = render_human(&outcome);
    if output.len() > HUMAN_OUTPUT_LIMIT {
        let _ = write_stderr("preview output exceeded the 64 KiB bound\n");
        return ExitCode::InternalError.value();
    }
    if write_stdout(output.as_bytes()).is_err() {
        return ExitCode::InternalError.value();
    }
    ExitCode::Rendered.value()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MANIFEST: &str = r#"{
        "schema_version": "1.0.0",
        "version": "0.1.2",
        "assets": {
            "linux-x86_64": { "name": "local-mcp-v0.1.2-linux-x86_64.tar.gz", "sha256": "34bfccf8a6bc1a258b3d841234d445f6e7db9db0fc9a5895dddffa95affe43e6" },
            "linux-aarch64": { "name": "local-mcp-v0.1.2-linux-aarch64.tar.gz", "sha256": "f048eddfd17b9fe9bdccaee7d05418d31282d5642c2a709c27738fba58c9ece0" },
            "macos-x86_64": { "name": "local-mcp-v0.1.2-macos-x86_64.tar.gz", "sha256": "12d24cb82501153527422f54639e3f5332c7d84b741dbdbb4b9a5a7dd6c8d9a8" },
            "macos-aarch64": { "name": "local-mcp-v0.1.2-macos-aarch64.tar.gz", "sha256": "21400b2fb61e865484055e470ca78f3d289882fda335ed6e5e4cbc37eef8a787" },
            "windows-x86_64": { "name": "local-mcp-v0.1.2-windows-x86_64.zip", "sha256": "f5d2e3e20242956ca34b1fe2d9de479ca1a38fad84b99ddb619204cf0cd4342a" },
            "windows-aarch64": { "name": "local-mcp-v0.1.2-windows-aarch64.zip", "sha256": "4288ba681bd47fdfc8c08d923c5870bd1693c2dadac17f975d661024b522841c" }
        }
    }"#;

    fn parse_sample() -> Manifest {
        let value = parse_json_strict(SAMPLE_MANIFEST.as_bytes()).expect("sample manifest parses");
        manifest_from_json(value).expect("sample manifest is valid")
    }

    /// Redundancy is deliberate: editing the embedded pin without updating
    /// this table (or vice versa) fails the build, so the pin cannot drift
    /// silently. The values are the v0.1.2 `SHA256SUMS` published with the
    /// release (verified against the release on 2026-11-04).
    #[test]
    fn embedded_pin_matches_the_published_v0_1_2_sha256sums() {
        let expected: [(&str, &str, &str); 6] = [
            (
                "linux-x86_64",
                "local-mcp-v0.1.2-linux-x86_64.tar.gz",
                "34bfccf8a6bc1a258b3d841234d445f6e7db9db0fc9a5895dddffa95affe43e6",
            ),
            (
                "linux-aarch64",
                "local-mcp-v0.1.2-linux-aarch64.tar.gz",
                "f048eddfd17b9fe9bdccaee7d05418d31282d5642c2a709c27738fba58c9ece0",
            ),
            (
                "macos-x86_64",
                "local-mcp-v0.1.2-macos-x86_64.tar.gz",
                "12d24cb82501153527422f54639e3f5332c7d84b741dbdbb4b9a5a7dd6c8d9a8",
            ),
            (
                "macos-aarch64",
                "local-mcp-v0.1.2-macos-aarch64.tar.gz",
                "21400b2fb61e865484055e470ca78f3d289882fda335ed6e5e4cbc37eef8a787",
            ),
            (
                "windows-x86_64",
                "local-mcp-v0.1.2-windows-x86_64.zip",
                "f5d2e3e20242956ca34b1fe2d9de479ca1a38fad84b99ddb619204cf0cd4342a",
            ),
            (
                "windows-aarch64",
                "local-mcp-v0.1.2-windows-aarch64.zip",
                "4288ba681bd47fdfc8c08d923c5870bd1693c2dadac17f975d661024b522841c",
            ),
        ];
        for (key, name, sha256) in expected {
            let target = Target::ALL
                .iter()
                .copied()
                .find(|candidate| candidate.key() == key)
                .expect("target exists in the pin table");
            let facts = target.facts();
            assert_eq!(facts.asset_name, name, "asset name for {key}");
            assert_eq!(facts.sha256, sha256, "sha256 for {key}");
        }
    }

    #[test]
    fn embedded_pin_asset_names_follow_the_release_convention() {
        for target in Target::ALL {
            let facts = target.facts();
            let expected = if facts.key.starts_with("windows") {
                format!("local-mcp-v{PINNED_VERSION}-{}.zip", facts.key)
            } else {
                format!("local-mcp-v{PINNED_VERSION}-{}.tar.gz", facts.key)
            };
            assert_eq!(facts.asset_name, expected);
            assert!(is_sha256_hex(facts.sha256));
        }
    }

    #[test]
    fn documented_resource_limits_match_the_closed_contract() {
        assert_eq!(MAX_MANIFEST_BYTES, 1024 * 1024);
        assert_eq!(MAX_ARCHIVE_BYTES, 256 * 1024 * 1024);
        assert_eq!(HUMAN_OUTPUT_LIMIT, 64 * 1024);
        assert_eq!(JSON_OUTPUT_LIMIT, 1024 * 1024);
        assert_eq!(PREVIEW_SCHEMA_VERSION, "1.0.0");
        assert_eq!(PREVIEW_CONTRACT_VERSION, 1);
    }

    #[test]
    fn every_known_target_is_a_supported_preview_target() {
        for target in Target::ALL {
            let section = platform_section(target);
            assert!(
                section.supported,
                "{} must be a supported preview target",
                target.key()
            );
            assert_eq!(section.experimental, target.facts().experimental);
            assert_eq!(section.classification, target.facts().classification);
        }
    }

    #[test]
    fn overall_result_never_claims_verified_without_install_root_trust() {
        assert_eq!(overall_preview_result(0, 0), "RENDERED_UNVERIFIED");
        assert_eq!(overall_preview_result(1, 0), "DOCTOR_BLOCK");
        assert_eq!(overall_preview_result(0, 1), "DOCTOR_REQUIRED_UNKNOWN");
        assert_eq!(overall_preview_result(1, 1), "DOCTOR_BLOCK");
    }

    #[test]
    fn doctor_contract_versions_are_checked() {
        validate_doctor_contract("1.0.0", 1, "1.0.0", "0.1.2")
            .expect("the current Doctor V1 contract is supported");
        for (schema, contract, doctor, product) in [
            ("99.0.0", 1, "1.0.0", "0.1.2"),
            ("1.0.0", 2, "1.0.0", "0.1.2"),
            ("1.0.0", 1, "", "0.1.2"),
            ("1.0.0", 1, "1.0.0", ""),
        ] {
            let failure = validate_doctor_contract(schema, contract, doctor, product)
                .expect_err("unsupported Doctor identity must be refused");
            assert_eq!(failure.kind, FailureKind::DoctorUnsupported);
            assert_eq!(failure.exit_code(), 70);
        }
    }

    #[test]
    fn error_text_escapes_terminal_and_bidi_controls() {
        let failure = Failure::pin_mismatch(
            "version",
            PINNED_VERSION,
            "0.1.2\u{1b}[2J\u{202e}FAKE\nnext",
        );
        let rendered = render_failure_human(&failure);
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{202e}'));
        assert!(rendered.contains("\\u{1b}"));
        assert!(rendered.contains("\\u{202e}"));
        assert!(rendered.contains("A Doctor PASS is not an installation authorization."));
    }

    #[test]
    fn manifest_double_read_detects_changed_bytes() {
        let root = std::env::temp_dir().join(format!(
            "goallatch-preview-double-read-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create test-owned double-read root");
        let path = root.join("manifest.json");
        std::fs::write(&path, SAMPLE_MANIFEST).expect("write fixture manifest");

        let options = SetupPreviewOptions {
            json: false,
            manifest: Some(path.clone()),
            archive: None,
        };
        let first = std::fs::read(&path).expect("read fixture manifest");
        assert_eq!(
            verify_manifest_unchanged(&options, Some(&first)).expect("unchanged bytes pass"),
            Some(true)
        );

        std::fs::write(&path, br#"{"changed":true}"#).expect("replace fixture manifest");
        let error = verify_manifest_unchanged(&options, Some(&first)).unwrap_err();
        assert_eq!(error.kind, FailureKind::MetadataChanged);
        assert_eq!(error.exit_code(), 64);
        assert!(error.expected.contains(&sha256_hex(&first)[..16]));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sample_manifest_parses_and_matches_the_pin() {
        let manifest = parse_sample();
        assert_eq!(manifest.version, PINNED_VERSION);
        check_pin(&manifest).expect("sample manifest matches the embedded pin");
    }

    #[test]
    fn manifest_with_duplicate_keys_is_rejected() {
        let duplicated = SAMPLE_MANIFEST.replace(
            "\"version\": \"0.1.2\",",
            "\"version\": \"0.1.2\", \"version\": \"0.2.0\",",
        );
        assert!(parse_json_strict(duplicated.as_bytes()).is_err());
    }

    #[test]
    fn manifest_with_trailing_data_is_rejected() {
        let mut bytes = SAMPLE_MANIFEST.as_bytes().to_vec();
        bytes.extend_from_slice(b" {}");
        assert!(parse_json_strict(&bytes).is_err());
    }

    #[test]
    fn manifest_with_invalid_utf8_is_rejected() {
        let bytes = [0xff, 0xfe, 0x00, 0x01];
        assert!(parse_json_strict(&bytes).is_err());
    }

    #[test]
    fn manifest_missing_target_is_rejected() {
        let value = serde_json::from_str::<serde_json::Value>(SAMPLE_MANIFEST).unwrap();
        let mut assets = value["assets"].as_object().unwrap().clone();
        assets.remove("windows-x86_64");
        let mut root = value.as_object().unwrap().clone();
        root.insert("assets".to_owned(), serde_json::Value::Object(assets));
        let text = serde_json::to_string(&serde_json::Value::Object(root)).unwrap();
        let parsed = parse_json_strict(text.as_bytes()).expect("valid JSON syntax");
        let error = manifest_from_json(parsed).unwrap_err();
        assert_eq!(error.kind, FailureKind::ManifestInvalid);
        assert!(error.check.contains("windows-x86_64"));
    }

    #[test]
    fn manifest_with_unexpected_target_is_rejected() {
        let value = serde_json::from_str::<serde_json::Value>(SAMPLE_MANIFEST).unwrap();
        let mut assets = value["assets"].as_object().unwrap().clone();
        assets.insert(
            "riscv64-unknown-linux-gnu".to_owned(),
            serde_json::json!({
                "name": "local-mcp-v0.1.2-riscv64.tar.gz",
                "sha256": "34bfccf8a6bc1a258b3d841234d445f6e7db9db0fc9a5895dddffa95affe43e6"
            }),
        );
        let mut root = value.as_object().unwrap().clone();
        root.insert("assets".to_owned(), serde_json::Value::Object(assets));
        let text = serde_json::to_string(&serde_json::Value::Object(root)).unwrap();
        let parsed = parse_json_strict(text.as_bytes()).unwrap();
        let error = manifest_from_json(parsed).unwrap_err();
        assert_eq!(error.kind, FailureKind::ManifestInvalid);
        assert!(error.actual.contains("unexpected target"));
    }

    #[test]
    fn manifest_with_malformed_sha256_is_rejected() {
        let tampered = SAMPLE_MANIFEST.replace(
            "34bfccf8a6bc1a258b3d841234d445f6e7db9db0fc9a5895dddffa95affe43e6",
            "not-a-sha256",
        );
        let parsed = parse_json_strict(tampered.as_bytes()).unwrap();
        let error = manifest_from_json(parsed).unwrap_err();
        assert_eq!(error.kind, FailureKind::ManifestInvalid);
        assert!(error.check.contains("sha256"));
    }

    #[test]
    fn manifest_with_wrong_schema_version_is_rejected() {
        let tampered = SAMPLE_MANIFEST.replace("\"1.0.0\"", "\"99.0.0\"");
        let parsed = parse_json_strict(tampered.as_bytes()).unwrap();
        let error = manifest_from_json(parsed).unwrap_err();
        assert_eq!(error.kind, FailureKind::ManifestInvalid);
        assert!(error.check.contains("schema_version"));
    }

    #[test]
    fn pin_mismatch_version_is_detected() {
        let manifest = Manifest {
            version: "0.2.0".to_owned(),
            assets: parse_sample().assets,
        };
        let error = check_pin(&manifest).unwrap_err();
        assert_eq!(error.kind, FailureKind::PinMismatch);
        assert_eq!(error.check, "version");
    }

    #[test]
    fn pin_mismatch_asset_name_is_detected() {
        let mut manifest = parse_sample();
        manifest.assets.get_mut("linux-x86_64").unwrap().name =
            "local-mcp-v0.9.9-linux-x86_64.tar.gz".to_owned();
        let error = check_pin(&manifest).unwrap_err();
        assert_eq!(error.kind, FailureKind::PinMismatch);
        assert!(error.check.ends_with(".name"));
    }

    #[test]
    fn pin_mismatch_sha256_is_detected() {
        let mut manifest = parse_sample();
        manifest.assets.get_mut("linux-x86_64").unwrap().sha256 =
            "00bfccf8a6bc1a258b3d841234d445f6e7db9db0fc9a5895dddffa95affe43e6".to_owned();
        let error = check_pin(&manifest).unwrap_err();
        assert_eq!(error.kind, FailureKind::PinMismatch);
        assert!(error.check.ends_with(".sha256"));
    }

    #[test]
    fn install_root_trust_is_never_pass() {
        let none = evaluate_install_root_trust(None);
        assert_eq!(none.status, InstallRootTrustStatus::Unknown);
        assert_eq!(none.reason_code, "INSTALL_ROOT_NOT_EVALUATED");

        for meta in [
            InstallRootTrustMeta {
                world_writable: Some(true),
                ..InstallRootTrustMeta::default()
            },
            InstallRootTrustMeta {
                replaced_after_check: Some(true),
                ..InstallRootTrustMeta::default()
            },
            InstallRootTrustMeta {
                reparse_or_symlink: Some(true),
                ..InstallRootTrustMeta::default()
            },
        ] {
            let trust = evaluate_install_root_trust(Some(meta));
            assert_eq!(trust.status, InstallRootTrustStatus::Untrusted);
        }

        let benign = evaluate_install_root_trust(Some(InstallRootTrustMeta {
            world_writable: Some(false),
            replaced_after_check: Some(false),
            reparse_or_symlink: Some(false),
        }));
        assert_eq!(benign.status, InstallRootTrustStatus::Unknown);
    }

    #[test]
    fn sha256_hex_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn read_file_bounded_returns_bytes_and_refuses_directories_and_oversize() {
        let root = std::env::temp_dir().join(format!(
            "goallatch-preview-read-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create test-owned read root");

        let file = root.join("payload.bin");
        std::fs::write(&file, b"abc").expect("write fixture");

        let bytes = read_file_bounded(&file, 1024).expect("small regular file reads");
        assert_eq!(bytes, b"abc");

        let oversize = read_file_bounded(&file, 2);
        assert!(oversize.is_err(), "limit below file size must be refused");

        let directory = read_file_bounded(&root, 1024);
        assert!(directory.is_err(), "a directory is not a regular file");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hash_archive_streams_and_bounds() {
        let root = std::env::temp_dir().join(format!(
            "goallatch-preview-hash-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create test-owned hash root");
        let file = root.join("fixture.bin");
        std::fs::write(&file, b"abc").expect("write fixture");
        let digest = hash_archive_with_limit(&file, 1024).expect("small fixture hashes");
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let oversized = hash_archive_with_limit(&file, 2);
        assert!(oversized.is_err(), "oversize fixture must be refused");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn archive_outcome_states_are_exact() {
        let verified = archive_outcome(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .expect("matching digest verifies");
        assert_eq!(verified.status(), "VERIFIED_ARCHIVE");

        let mismatch = archive_outcome(
            "00bfccf8a6bc1a258b3d841234d445f6e7db9db0fc9a5895dddffa95affe43e6",
            "34bfccf8a6bc1a258b3d841234d445f6e7db9db0fc9a5895dddffa95affe43e6",
        );
        let error = mismatch.unwrap_err();
        assert_eq!(error.kind, FailureKind::ArchiveMismatch);
        assert_eq!(error.check, "archive.sha256");
    }

    #[test]
    fn metadata_changed_fails_closed() {
        let failure = Failure::metadata_changed("1111111111111111", "2222222222222222");
        assert_eq!(failure.kind, FailureKind::MetadataChanged);
        assert_eq!(failure.exit_code(), 64);
        assert!(failure.expected.contains("1111111111111111"));
        assert!(failure.actual.contains("2222222222222222"));
    }

    #[test]
    fn failure_exit_codes_are_partitioned() {
        assert_eq!(Failure::doctor_unparseable().exit_code(), 70);
        assert_eq!(
            Failure::doctor_unsupported("schema_version", "99.0.0").exit_code(),
            70
        );
        assert_eq!(Failure::manifest_invalid("x", "y").exit_code(), 64);
        assert_eq!(
            Failure::unsupported_platform("riscv64/linux", "linux-x86_64").exit_code(),
            64
        );
    }
}
