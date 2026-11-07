//! G0-A Windows Authority Audit — cross-cutting regression tests.
//!
//! These tests pin the authority contracts audited in
//! `docs/WINDOWS_AUTHORITY_AUDIT_G0A.md`:
//!
//! * host-native `execute` refuses a cwd outside the session's permitted
//!   directories **before** any approval channel is consulted;
//! * a missing approval channel fails closed instead of running the command;
//! * `write_file`'s path-authority gate refuses creation outside the permitted
//!   roots before the host-native approval request;
//! * `side_effect_state: UNKNOWN` is the deliberate fail-safe verdict for
//!   commands the host cannot prove read-only, and it locks the budget rather
//!   than granting a retry;
//! * `operation_validated` reports structured `git_stage_paths` validation only,
//!   so `false` on `read_only_command` / `unstructured` operations is expected
//!   and must never be read as "the operation was not validated".
//!
//! The approval wire protocol itself (allow / deny / invalid / missing /
//! disconnect) is covered by the tests inside `src/approvals.rs`.

use std::path::PathBuf;

#[cfg(windows)]
use serde_json::json;
use uuid::Uuid;

use crate::config::{self, PathIntent, Session};
#[cfg(windows)]
use crate::execution;
use crate::fallback::{
    self, Budget, FailureClass, LifecycleEvidence, SideEffectClass, SideEffectState,
};
use crate::sandbox::CommandStart;

fn audit_session(label: &str) -> (Session, PathBuf) {
    let cwd = std::env::temp_dir().join(format!("local-mcp-g0a-{label}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    (
        Session {
            id: format!("g0a-{label}-{}", Uuid::new_v4()),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
        },
        cwd,
    )
}

fn outside_root(label: &str) -> PathBuf {
    let outside =
        std::env::temp_dir().join(format!("local-mcp-g0a-outside-{label}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&outside).unwrap();
    outside
}

/// The cwd gate runs before the approval request: an out-of-bounds cwd must be
/// refused by path authority even though this session has no approval channel
/// at all. If approval were consulted first, the error would instead be the
/// "session is not running" approval failure.
#[cfg(windows)]
#[tokio::test]
async fn execute_outside_permitted_directories_is_refused_before_approval() {
    let (session, root) = audit_session("outside-cwd");
    let outside = outside_root("cwd");
    let args = json!({"command": ["git", "status"], "cwd": outside.to_string_lossy()});
    let error = match execution::execute(&args, &session).await {
        Err(error) => error,
        Ok(_) => panic!("execute outside the permitted directories must be refused"),
    };
    let message = format!("{error:#}");
    assert!(
        message.contains("outside the session's permitted directories"),
        "expected a path-authority refusal, got: {message}"
    );
    assert!(
        !message.contains("is not running"),
        "path authority must refuse before any approval request: {message}"
    );
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(outside).unwrap();
}

/// A host-native `execute` with no approval channel must fail closed: the
/// command never starts and the caller sees the approval-channel error.
#[cfg(windows)]
#[tokio::test]
async fn execute_without_approval_channel_fails_closed() {
    let (session, root) = audit_session("no-channel");
    let args = json!({"command": ["git", "status"]});
    let error = match execution::execute(&args, &session).await {
        Err(error) => error,
        Ok(_) => panic!("execute without an approval channel must fail closed"),
    };
    let message = format!("{error:#}");
    assert!(
        message.contains("is not running"),
        "a missing approval channel must fail closed before any host-native work: {message}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

/// `write_file` resolves its target through `validate_path_authority` before it
/// requests host-native approval, so a creation outside the permitted roots is
/// refused by path authority alone (fail closed, zero mutation).
#[test]
fn write_file_path_authority_refuses_creation_outside_permitted_roots() {
    let (session, root) = audit_session("outside-write");
    let outside = outside_root("write");
    let target = outside.join("must-not-appear.txt");
    let error =
        config::validate_path_authority(&session, &target, PathIntent::CreateFile).unwrap_err();
    assert!(
        format!("{error:#}").contains("outside the session's permitted directories"),
        "write_file target outside the permitted roots must be refused"
    );
    assert!(
        !target.exists(),
        "the refused write must not create anything"
    );
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(outside).unwrap();
}

/// `side_effect_state: UNKNOWN` is not a bug report and not a bypass: it is the
/// fail-safe verdict for a command the host cannot classify, and it locks the
/// fallback budget instead of granting another attempt.
#[test]
fn unclassified_host_commands_map_to_unknown_and_lock_the_budget() {
    let class =
        fallback::infer_side_effect_class(&["rustc".to_owned(), "--version".to_owned()], None);
    assert_eq!(class, SideEffectClass::Unknown);
    let state = fallback::infer_side_effect_state(
        class,
        LifecycleEvidence::completed_with_start_proof(CommandStart::Proven),
        FailureClass::Success,
        None,
    );
    assert_eq!(state, SideEffectState::Unknown);
    let budget = Budget::from_operation(None).after_state(state);
    assert!(
        budget.locked,
        "an UNKNOWN verdict must lock the budget rather than grant a retry"
    );
}

/// `git status` can refresh the index stat cache, so it is only an observation
/// with `--no-optional-locks`. This is the exact shape that reported
/// `side_effect_state: UNKNOWN` during dogfooding, and it is by design.
#[test]
fn git_status_is_an_observation_only_with_no_optional_locks() {
    let refreshing = fallback::infer_side_effect_class(
        &[
            "git".to_owned(),
            "status".to_owned(),
            "--porcelain".to_owned(),
        ],
        None,
    );
    assert_eq!(
        refreshing,
        SideEffectClass::Unknown,
        "plain `git status` may refresh the index and stays unproven"
    );
    let quiet = fallback::infer_side_effect_class(
        &[
            "git".to_owned(),
            "--no-optional-locks".to_owned(),
            "status".to_owned(),
            "--porcelain".to_owned(),
        ],
        None,
    );
    assert_eq!(
        quiet,
        SideEffectClass::None,
        "`git --no-optional-locks status` is a provable observation"
    );
}

/// A provable observation reports `CONFIRMED_NOT_PERFORMED`, contrasting with
/// the UNKNOWN verdict above so the two payload states stay distinguishable.
#[test]
fn classified_read_only_commands_are_confirmed_not_performed() {
    let class = fallback::infer_side_effect_class(
        &["git".to_owned(), "rev-parse".to_owned(), "HEAD".to_owned()],
        None,
    );
    assert_eq!(class, SideEffectClass::None);
    let state = fallback::infer_side_effect_state(
        class,
        LifecycleEvidence::completed_with_start_proof(CommandStart::Proven),
        FailureClass::Success,
        None,
    );
    assert_eq!(state, SideEffectState::ConfirmedNotPerformed);
}
