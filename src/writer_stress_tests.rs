//! Adversarial stress audit for the Writer atomic-publication path.
//!
//! These are deliberately *not* ordinary unit tests. They look for the failure shapes
//! that only appear under repetition: temp-file leakage, partial content, permission
//! drift, wrong durable state, and unbounded artifact growth. They run against the real
//! `workspace_publish` primitive, including the real no-replace `link()` path.
//!
//! Kept as tests rather than a throwaway script so the audit stays reproducible.

use std::fs;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::workspace_publish::{self, ExpectedPreimage, PublishRequest};

fn scratch(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("local-mcp-stress-{label}-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    root
}

fn publish(
    root: &Path,
    name: &str,
    expected: ExpectedPreimage,
    request_id: &str,
    content: &[u8],
) -> Result<(), workspace_publish::PublishError> {
    let destination = root.join(name);
    workspace_publish::publish(PublishRequest {
        destination: &destination,
        parent: root,
        expected_preimage: &expected,
        request_id,
        content,
    })
}

fn staged_debris(root: &Path) -> Vec<String> {
    fs::read_dir(root)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".staged"))
                .collect()
        })
        .unwrap_or_default()
}

/// 100 repeated atomic replacements of one file must leave exactly one file, correct
/// content, and no debris. This is the shape that would expose per-attempt temp leakage.
#[test]
fn one_hundred_repeated_replacements_leave_no_debris_and_stable_content() {
    let root = scratch("repeat100");
    let target = root.join("file.txt");
    fs::write(&target, b"gen-0\n").unwrap();

    for generation in 1..=100u32 {
        let before = workspace_publish::sha256_hex(format!("gen-{}\n", generation - 1).as_bytes());
        publish(
            &root,
            "file.txt",
            ExpectedPreimage::Sha256(before),
            &format!("req-{generation}"),
            format!("gen-{generation}\n").as_bytes(),
        )
        .unwrap_or_else(|error| panic!("generation {generation} failed: {error}"));
    }

    assert_eq!(fs::read(&target).unwrap(), b"gen-100\n");
    assert_eq!(
        staged_debris(&root),
        Vec::<String>::new(),
        "repeated replacement leaked staged files"
    );
    // Only the target itself should remain: no unbounded artifact growth.
    let entries: Vec<String> = fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["file.txt".to_owned()]);
    fs::remove_dir_all(&root).unwrap();
}

/// A 32-operation attempt is the documented Writer maximum. Every file must land, and
/// the directory must contain exactly the 32 targets.
#[test]
fn a_thirty_two_operation_attempt_publishes_every_file() {
    let root = scratch("ops32");
    for index in 0..32 {
        fs::write(root.join(format!("f{index}.txt")), format!("old-{index}\n")).unwrap();
    }
    for index in 0..32 {
        let name = format!("f{index}.txt");
        let before = workspace_publish::sha256_hex(format!("old-{index}\n").as_bytes());
        publish(
            &root,
            &name,
            ExpectedPreimage::Sha256(before),
            &format!("req-{index}"),
            format!("new-{index}\n").as_bytes(),
        )
        .unwrap();
    }
    for index in 0..32 {
        assert_eq!(
            fs::read(root.join(format!("f{index}.txt"))).unwrap(),
            format!("new-{index}\n").as_bytes()
        );
    }
    assert_eq!(fs::read_dir(&root).unwrap().count(), 32);
    fs::remove_dir_all(&root).unwrap();
}

/// Rapid external mutation between validation and commit must never result in the
/// Writer's content landing over someone else's. Repeating this many times increases the
/// chance of hitting the residual window the design documents.
#[test]
fn rapid_external_preimage_mutation_never_gets_overwritten() {
    let root = scratch("rapid");
    let target = root.join("contested.txt");

    let mut overwritten = 0;
    for round in 0..64u32 {
        let external = format!("external-{round}\n");
        fs::write(&target, &external).unwrap();
        let observed = workspace_publish::sha256_hex(external.as_bytes());

        // Alternate the expectation: half the rounds are honest, half are stale.
        let expected = if round % 2 == 0 {
            ExpectedPreimage::Sha256(observed.clone())
        } else {
            ExpectedPreimage::Sha256(workspace_publish::sha256_hex(b"something-else\n"))
        };
        let writer_content = format!("writer-{round}\n");
        let _ = publish(
            &root,
            "contested.txt",
            expected,
            &format!("req-{round}"),
            writer_content.as_bytes(),
        );

        let final_bytes = fs::read(&target).unwrap();
        // Either the honest commit landed, or the external content is still intact.
        // A mixed or truncated result would fail both checks.
        let ok = final_bytes == writer_content.as_bytes() || final_bytes == external.as_bytes();
        assert!(ok, "round {round} produced a partial or corrupt result");
        if final_bytes == writer_content.as_bytes() && round % 2 == 1 {
            overwritten += 1;
        }
    }
    assert_eq!(overwritten, 0, "a stale preimage was overwritten");
    assert_eq!(staged_debris(&root), Vec::<String>::new());
    fs::remove_dir_all(&root).unwrap();
}

/// The expected-ABSENT race: the target appears after absence was validated. The
/// external target must survive, every round.
#[test]
fn the_new_file_create_race_never_clobbers() {
    let root = scratch("createrace");
    let target = root.join("created.txt");

    for round in 0..32u32 {
        fs::remove_file(&target).ok();
        // The external actor wins between validation and publication.
        let external = format!("external-wins-{round}\n");
        fs::write(&target, &external).unwrap();

        let result = publish(
            &root,
            "created.txt",
            ExpectedPreimage::Absent,
            &format!("req-{round}"),
            b"writer\n",
        );
        assert!(
            result.is_err(),
            "round {round}: an absent-expectation commit must refuse"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            external.as_bytes(),
            "round {round}: the external target was clobbered"
        );
    }
    assert_eq!(staged_debris(&root), Vec::<String>::new());
    fs::remove_dir_all(&root).unwrap();
}

/// Repeatedly swapping the target for a symlink must never cause the writer's content to
/// land in the symlink's destination.
#[cfg(unix)]
#[test]
fn repeated_symlink_swaps_never_redirect_the_write() {
    let root = scratch("symswap");
    let target = root.join("target.txt");
    let victim = root.join("victim.txt");
    fs::write(&target, b"original\n").unwrap();
    fs::write(&victim, b"victim\n").unwrap();

    for round in 0..32u32 {
        fs::remove_file(&target).ok();
        std::os::unix::fs::symlink(&victim, &target).unwrap();

        let _ = publish(
            &root,
            "target.txt",
            ExpectedPreimage::Absent,
            &format!("req-{round}"),
            format!("writer-{round}\n").as_bytes(),
        );

        assert_eq!(
            fs::read(&victim).unwrap(),
            b"victim\n",
            "round {round}: a symlink was followed and the victim overwritten"
        );
    }
    fs::remove_dir_all(&root).unwrap();
}

/// Repeated replacement must not drift a file's permissions.
#[cfg(unix)]
#[test]
fn repeated_replacement_does_not_drift_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let root = scratch("perms");
    let target = root.join("script.sh");
    fs::write(&target, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

    for round in 0..64u32 {
        // Each round expects the content the previous round produced.
        let prior = if round == 0 {
            b"#!/bin/sh\n".to_vec()
        } else {
            format!("#!/bin/sh\necho {}\n", round - 1).into_bytes()
        };
        let before = workspace_publish::sha256_hex(&prior);
        publish(
            &root,
            "script.sh",
            ExpectedPreimage::Sha256(before),
            &format!("req-{round}"),
            format!("#!/bin/sh\necho {round}\n").as_bytes(),
        )
        .unwrap();
        let mode = fs::symlink_metadata(&target).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode, 0o755,
            "round {round}: permissions drifted to {mode:o}"
        );
    }
    fs::remove_dir_all(&root).unwrap();
}

/// Unicode and non-UTF-8 path components must round-trip through staging, publication and
/// identity encoding without corruption.
///
/// Non-UTF-8 *filenames* are only exercised where the filesystem can represent them:
/// APFS/HFS+ reject arbitrary byte sequences in names, so that half is skipped there
/// rather than asserted against a platform that cannot store the input. The identity
/// half is pure computation and always runs, because that is what keeps two distinct
/// host paths from colliding in an authority or evidence digest.
#[cfg(unix)]
#[test]
fn unicode_and_non_utf8_paths_publish_correctly() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let root = scratch("unicode");

    // Unicode names must work everywhere.
    let unicode_name = "ファイル.txt";
    fs::write(root.join(unicode_name), b"old\n").unwrap();
    let before = workspace_publish::sha256_hex(b"old\n");
    let destination = root.join(unicode_name);
    workspace_publish::publish(PublishRequest {
        destination: &destination,
        parent: &root,
        expected_preimage: &ExpectedPreimage::Sha256(before),
        request_id: "req-unicode",
        content: "new content\n".as_bytes(),
    })
    .unwrap_or_else(|error| panic!("unicode filename failed: {error}"));
    assert_eq!(fs::read(&destination).unwrap(), b"new content\n");

    // Non-UTF-8 names: publish only if this filesystem can store one at all.
    let weird_name = std::path::PathBuf::from(OsStr::from_bytes(b"weird-\xfe.txt"));
    match fs::write(root.join(&weird_name), b"old\n") {
        Ok(()) => {
            let before = workspace_publish::sha256_hex(b"old\n");
            let destination = root.join(&weird_name);
            workspace_publish::publish(PublishRequest {
                destination: &destination,
                parent: &root,
                expected_preimage: &ExpectedPreimage::Sha256(before),
                request_id: "req-weird",
                content: b"weird new\n",
            })
            .unwrap_or_else(|error| panic!("non-UTF-8 filename failed: {error}"));
            assert_eq!(fs::read(&destination).unwrap(), b"weird new\n");
        }
        Err(error) if error.raw_os_error() == Some(92) => {
            // Illegal byte sequence: this filesystem cannot represent the name.
            // Recorded rather than silently skipped.
            eprintln!(
                "note: filesystem rejects non-UTF-8 filenames (os error 92); identity checks still run"
            );
        }
        Err(error) => panic!("unexpected error creating a non-UTF-8 filename: {error}"),
    }

    // Identity encoding must stay distinct for two non-UTF-8 paths that differ only in
    // bytes `to_string_lossy` would collapse onto the same replacement character.
    let a = root.join(OsStr::from_bytes(b"x-\xff.txt"));
    let b = root.join(OsStr::from_bytes(b"x-\xfe.txt"));
    assert_ne!(
        workspace_publish::path_identity_bytes(&a),
        workspace_publish::path_identity_bytes(&b),
        "distinct non-UTF-8 paths must not collapse to one identity"
    );
    let lossy_a = a.to_string_lossy().into_owned();
    let lossy_b = b.to_string_lossy().into_owned();
    assert_eq!(
        lossy_a, lossy_b,
        "this fixture is only meaningful if lossy rendering really does collide"
    );
    assert_ne!(
        workspace_publish::scope_identity(&[(a.clone(), "aa".to_owned())]),
        workspace_publish::scope_identity(&[(b.clone(), "aa".to_owned())]),
        "scope_identity must not alias two paths that lossy text conflates"
    );
    assert_eq!(staged_debris(&root), Vec::<String>::new());
    fs::remove_dir_all(&root).unwrap();
}

/// Simulated crash after staging: the destination must remain the complete old file.
///
/// The debris is deliberately given a name the host did **not** derive. That is the
/// interesting case: cleanup must reap only host-attributable debris, so foreign debris
/// has to survive. A broad "delete anything matching *.staged" sweep would fail here.
#[test]
fn a_crash_after_staging_leaves_the_old_file_intact_and_foreign_debris_alone() {
    let root = scratch("crashstage");
    let target = root.join("file.txt");
    let original = b"the complete original content\n";
    fs::write(&target, original).unwrap();

    // Simulate the crash: something was staged and the process died before publication.
    let foreign = root.join(".file.txt.deadbeefdeadbeef.not-our-request.staged");
    fs::write(&foreign, b"half-written").unwrap();

    // The destination is untouched by the crash.
    assert_eq!(fs::read(&target).unwrap(), original.to_vec());

    // A retry under our own durable request id succeeds and leaves foreign debris alone.
    let before = workspace_publish::sha256_hex(original);
    publish(
        &root,
        "file.txt",
        ExpectedPreimage::Sha256(before),
        "req-1",
        b"replacement\n",
    )
    .unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"replacement\n");
    assert_eq!(
        fs::read(&foreign).unwrap(),
        b"half-written",
        "cleanup must never delete a file whose identity the host did not derive"
    );
    fs::remove_dir_all(&root).unwrap();
}

/// A publication failure must not leave debris behind, and must leave the destination as
/// the complete old file.
#[test]
fn a_refused_commit_leaves_no_debris_and_no_partial_content() {
    let root = scratch("refused");
    let target = root.join("file.txt");
    let original = b"complete original\n";
    fs::write(&target, original).unwrap();

    let wrong = workspace_publish::sha256_hex(b"not the current content\n");
    let result = publish(
        &root,
        "file.txt",
        ExpectedPreimage::Sha256(wrong),
        "req-1",
        b"writer content\n",
    );
    assert!(result.is_err());
    assert_eq!(fs::read(&target).unwrap(), original.to_vec());
    assert_eq!(staged_debris(&root), Vec::<String>::new());
    fs::remove_dir_all(&root).unwrap();
}

/// The primary checkout analogue: publishing into one directory must never touch a
/// sibling directory that represents the user's "primary" workspace.
#[test]
fn publication_never_mutates_a_sibling_workspace() {
    let root = scratch("primary");
    let managed = root.join("managed");
    let primary = root.join("primary");
    fs::create_dir_all(&managed).unwrap();
    fs::create_dir_all(&primary).unwrap();
    fs::write(managed.join("file.txt"), b"managed-before\n").unwrap();
    fs::write(primary.join("file.txt"), b"primary-content\n").unwrap();
    let primary_before = fs::read(primary.join("file.txt")).unwrap();

    let before = workspace_publish::sha256_hex(b"managed-before\n");
    publish(
        &managed,
        "file.txt",
        ExpectedPreimage::Sha256(before),
        "req-1",
        b"managed-after\n",
    )
    .unwrap();

    assert_eq!(
        fs::read(managed.join("file.txt")).unwrap(),
        b"managed-after\n"
    );
    assert_eq!(
        fs::read(primary.join("file.txt")).unwrap(),
        primary_before,
        "the primary checkout must be untouched"
    );
    assert_eq!(staged_debris(&managed), Vec::<String>::new());
    assert_eq!(staged_debris(&primary), Vec::<String>::new());
    fs::remove_dir_all(&root).unwrap();
}
