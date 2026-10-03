//! End-to-end coverage for the host-owned atomic publication helper as it is actually
//! invoked in production: through the platform sandbox, with the validated parent as
//! the only writable root.
//!
//! These tests exist to protect the property that motivated the design. The commit is
//! atomic, but only because it still runs *inside* the sandbox. If a future change moved
//! publication out of the sandbox, these tests would show the containment disappearing
//! rather than silently trading it away for correctness.

#[cfg(unix)]
use std::path::{Path, PathBuf};

use uuid::Uuid;

#[cfg(unix)]
fn scratch(label: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("local-mcp-publish-int-{label}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// The Nix build sandbox cannot host a nested sandbox profile.
#[cfg(unix)]
fn nested_sandbox_unavailable() -> bool {
    std::env::var_os("NIX_BUILD_TOP").is_some()
}

#[cfg(unix)]
mod unix_tests {
    use super::*;

    use crate::atomic_publish_frame::{self, EncodedRequest, PREIMAGE_ABSENT, PREIMAGE_SHA256};
    use crate::sandbox;

    fn helper() -> PathBuf {
        std::env::current_exe()
            .unwrap()
            .parent()
            .map(|dir| dir.join("atomic-publish"))
            .filter(|path| path.is_file())
            .or_else(|| {
                std::env::current_exe()
                    .unwrap()
                    .parent()
                    .and_then(|dir| dir.parent())
                    .map(|dir| dir.join("atomic-publish"))
            })
            .filter(|path| path.is_file())
            .expect("the atomic-publish helper must be built alongside the test binary")
    }

    async fn publish(
        parent: &Path,
        target: &Path,
        kind: u8,
        digest: &str,
        request_id: &str,
        content: &[u8],
    ) -> sandbox::Output {
        let frame = atomic_publish_frame::encode(EncodedRequest {
            parent: &parent.to_string_lossy(),
            target: &target.to_string_lossy(),
            preimage_kind: kind,
            preimage_digest: digest,
            request_id,
            content,
        });
        sandbox::run(
            &[helper().to_string_lossy().into_owned()],
            parent,
            std::slice::from_ref(&parent.to_path_buf()),
            Some(&frame),
        )
        .await
        .unwrap()
    }

    fn sha256(bytes: &[u8]) -> String {
        crate::workspace_publish::sha256_hex(bytes)
    }

    #[tokio::test]
    async fn the_sandboxed_helper_creates_and_replaces_atomically() {
        if nested_sandbox_unavailable() {
            return;
        }
        let root = scratch("roundtrip");
        let target = root.join("a.txt");

        let created = publish(&root, &target, PREIMAGE_ABSENT, "", "req-1", b"new\n").await;
        assert_eq!(created.status, 0, "{}", created.stderr);
        assert_eq!(std::fs::read(&target).unwrap(), b"new\n");

        let replaced = publish(
            &root,
            &target,
            PREIMAGE_SHA256,
            &sha256(b"new\n"),
            "req-2",
            b"replaced\n",
        )
        .await;
        assert_eq!(replaced.status, 0, "{}", replaced.stderr);
        assert_eq!(std::fs::read(&target).unwrap(), b"replaced\n");

        let debris: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".staged"))
            .collect();
        assert!(
            debris.is_empty(),
            "sandboxed publication left staged debris"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn the_sandboxed_helper_refuses_a_stale_preimage_without_mutating() {
        if nested_sandbox_unavailable() {
            return;
        }
        let root = scratch("stale");
        let target = root.join("a.txt");
        std::fs::write(&target, b"external\n").unwrap();

        let result = publish(
            &root,
            &target,
            PREIMAGE_SHA256,
            &sha256(b"something-else\n"),
            "req-3",
            b"writer\n",
        )
        .await;

        assert_ne!(result.status, 0, "a stale preimage must fail the commit");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"external\n",
            "the external content must survive a refused commit"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The containment property that the whole design exists to preserve: publication
    /// gains atomicity without gaining reach. A destination outside the writable root
    /// must be refused even when the helper is invoked directly.
    #[tokio::test]
    async fn the_helper_cannot_publish_outside_its_writable_root() {
        if nested_sandbox_unavailable() {
            return;
        }
        let root = scratch("escape");
        let allowed = root.join("allowed");
        let outside = root.join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let victim = outside.join("victim.txt");
        std::fs::write(&victim, b"must not change\n").unwrap();

        let result = publish(
            &allowed,
            &victim,
            PREIMAGE_SHA256,
            &sha256(b"must not change\n"),
            "req-4",
            b"overwritten\n",
        )
        .await;

        assert_ne!(
            result.status, 0,
            "publication outside the writable root must be refused"
        );
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"must not change\n",
            "the sandbox boundary must still contain the commit"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn an_executable_target_keeps_its_mode_across_the_sandboxed_commit() {
        use std::os::unix::fs::PermissionsExt;

        if nested_sandbox_unavailable() {
            return;
        }
        let root = scratch("mode");
        let target = root.join("run.sh");
        std::fs::write(&target, b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

        let result = publish(
            &root,
            &target,
            PREIMAGE_SHA256,
            &sha256(b"#!/bin/sh\necho hi\n"),
            "req-5",
            b"#!/bin/sh\necho bye\n",
        )
        .await;

        assert_eq!(result.status, 0, "{}", result.stderr);
        let mode = std::fs::symlink_metadata(&target)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            mode, 0o755,
            "executable bit lost through the sandboxed commit"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn a_malformed_request_is_rejected_without_touching_the_destination() {
        if nested_sandbox_unavailable() {
            return;
        }
        let root = scratch("malformed");
        let target = root.join("a.txt");
        std::fs::write(&target, b"untouched\n").unwrap();

        let output = sandbox::run(
            &[helper().to_string_lossy().into_owned()],
            &root,
            std::slice::from_ref(&root.to_path_buf()),
            Some(b"not a framed request"),
        )
        .await
        .unwrap();

        assert_ne!(output.status, 0, "a malformed frame must be rejected");
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched\n");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
