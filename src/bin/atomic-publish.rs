//! Host-owned helper that performs one atomic workspace publication.
//!
//! This is a Cargo binary target shipped next to `local-mcp`, exactly like the existing
//! `codex-linux-sandbox` helper. It exists so the commit can run **inside** the platform
//! sandbox with the validated parent as the only writable root, instead of being
//! performed by the unrestricted host process. Performing it in-process would have
//! replaced a kernel-enforced containment boundary with in-process correctness, which
//! is strictly more authority than the Writer had before this branch.
//!
//! It is deliberately *not* model-selectable: the host resolves its path from its own
//! executable directory and passes a structured, framed request on stdin. The model
//! never chooses this program, never supplies its arguments, and never supplies any
//! authority-bearing field.
//!
//! # Wire format
//!
//! All integers are little-endian, and every field is length-prefixed so the framing is
//! unambiguous and no two distinct requests share a byte stream.
//!
//! ```text
//! magic      : b"local-mcp/atomic-publish/v1\n"
//! parent     : u32 len | bytes
//! target     : u32 len | bytes
//! preimage   : u8 kind                  (0 = ABSENT, 1 = SHA256)
//!               kind == 1 => u32 len | lowercase hex digest
//! request id : u32 len | bytes
//! content    : u64 len | bytes
//! ```

#[path = "../workspace_publish.rs"]
mod workspace_publish;

#[path = "../atomic_publish_frame.rs"]
mod atomic_publish_frame;

mod frame {
    pub(crate) use crate::atomic_publish_frame::{
        DecodedRequest as Request, EncodedRequest, PREIMAGE_ABSENT, PREIMAGE_SHA256, decode, encode,
    };
}

fn main() {
    let mut input = Vec::new();
    if let Err(error) = std::io::Read::read_to_end(&mut std::io::stdin(), &mut input) {
        eprintln!("atomic-publish cannot read its request: {error}");
        std::process::exit(64);
    }
    let request = match frame::decode(&input) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("atomic-publish rejected its request: {error}");
            std::process::exit(65);
        }
    };

    let expected = match request.preimage_kind {
        frame::PREIMAGE_ABSENT => workspace_publish::ExpectedPreimage::Absent,
        frame::PREIMAGE_SHA256 => {
            workspace_publish::ExpectedPreimage::Sha256(request.preimage_digest)
        }
        other => {
            eprintln!("atomic-publish rejected an unknown preimage kind: {other}");
            std::process::exit(65);
        }
    };

    let parent = std::path::PathBuf::from(&request.parent);
    let target = std::path::PathBuf::from(&request.target);

    match workspace_publish::publish(workspace_publish::PublishRequest {
        destination: &target,
        parent: &parent,
        expected_preimage: &expected,
        request_id: &request.request_id,
        content: &request.content,
    }) {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            // The exit status distinguishes a refusal, where the destination is provably
            // untouched, from a publication whose outcome this process cannot prove.
            // Recovery keys its decision on the target's observed digest, never on
            // this code, but a precise code keeps the host's error honest.
            eprintln!("atomic-publish failed: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod frame_tests {
    use super::atomic_publish_frame::{self, EncodedRequest, PREIMAGE_ABSENT, PREIMAGE_SHA256};

    #[test]
    fn a_frame_round_trips_exactly() {
        let encoded = atomic_publish_frame::encode(EncodedRequest {
            parent: "/p",
            target: "/p/a.txt",
            preimage_kind: PREIMAGE_SHA256,
            preimage_digest: &"ab".repeat(32),
            request_id: "req-1",
            content: b"hello",
        });
        let decoded = atomic_publish_frame::decode(&encoded).unwrap();
        assert_eq!(decoded.parent, "/p");
        assert_eq!(decoded.target, "/p/a.txt");
        assert_eq!(decoded.preimage_kind, PREIMAGE_SHA256);
        assert_eq!(decoded.preimage_digest, "ab".repeat(32));
        assert_eq!(decoded.request_id, "req-1");
        assert_eq!(decoded.content, b"hello");
    }

    #[test]
    fn a_frame_for_a_missing_helper_input_is_rejected() {
        assert!(atomic_publish_frame::decode(b"").is_err());
    }
}
