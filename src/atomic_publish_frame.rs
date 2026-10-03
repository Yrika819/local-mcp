//! Wire format shared by the host and the host-owned `atomic-publish` helper.
//!
//! This is a single source of truth on purpose. The host encodes a request and the
//! helper decodes it; if the two could drift, a mismatch would either be misread as a
//! refusal (safe but wrong) or, worse, be read as a different target than the host
//! validated. Keeping one implementation makes that impossible by construction.
//!
//! All integers are little-endian and every field is length-prefixed, so the framing is
//! unambiguous: two distinct requests can never share a byte stream. Paths are carried
//! as UTF-8 here, and the host refuses to build a frame for a path it cannot encode
//! exactly rather than silently lossy-converting it.

use std::io;

pub(crate) const MAGIC: &[u8] = b"local-mcp/atomic-publish/v1\n";

/// Which preimage the commit requires. `0` is absence, `1` is a SHA-256 digest.
///
/// The kind travels as a distinct byte rather than being inferred from a digest string,
/// so `ABSENT` and `SHA256` can never be confused for one another.
/// The preimage kind byte travels as a distinct value rather than being inferred from
/// a digest string, so `ABSENT` and `SHA256` can never be confused for one another.
#[allow(
    dead_code,
    reason = "the host binary encodes and the helper binary decodes"
)]
pub(crate) const PREIMAGE_ABSENT: u8 = 0;
#[allow(
    dead_code,
    reason = "the host binary encodes and the helper binary decodes"
)]
pub(crate) const PREIMAGE_SHA256: u8 = 1;

/// Encoding is host-side; decoding is helper-side. Both live in this one module so the
/// two binaries that include it cannot drift, which means each sees half the API as
/// unused. That is a per-binary false positive, not dead code.
#[allow(
    dead_code,
    reason = "the host binary encodes and the helper binary decodes"
)]
pub(crate) struct EncodedRequest<'a> {
    pub parent: &'a str,
    pub target: &'a str,
    pub preimage_kind: u8,
    pub preimage_digest: &'a str,
    pub request_id: &'a str,
    pub content: &'a [u8],
}

#[allow(
    dead_code,
    reason = "the host binary encodes and the helper binary decodes"
)]
pub(crate) struct DecodedRequest {
    pub parent: String,
    pub target: String,
    pub preimage_kind: u8,
    pub preimage_digest: String,
    pub request_id: String,
    pub content: Vec<u8>,
}

#[allow(
    dead_code,
    reason = "the host binary encodes and the helper binary decodes"
)]
pub(crate) fn encode(request: EncodedRequest<'_>) -> Vec<u8> {
    let EncodedRequest {
        parent,
        target,
        preimage_kind,
        preimage_digest,
        request_id,
        content,
    } = request;
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    push_bytes(&mut out, parent.as_bytes());
    push_bytes(&mut out, target.as_bytes());
    out.push(preimage_kind);
    push_bytes(&mut out, preimage_digest.as_bytes());
    push_bytes(&mut out, request_id.as_bytes());
    out.extend_from_slice(&(content.len() as u64).to_le_bytes());
    out.extend_from_slice(content);
    out
}

fn push_bytes(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value);
}

#[allow(
    dead_code,
    reason = "the host binary encodes and the helper binary decodes"
)]
fn take<'a>(input: &mut &'a [u8], len: usize) -> io::Result<&'a [u8]> {
    if input.len() < len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "atomic-publish frame is truncated",
        ));
    }
    let (head, tail) = input.split_at(len);
    *input = tail;
    Ok(head)
}

#[allow(
    dead_code,
    reason = "the host binary encodes and the helper binary decodes"
)]
fn take_bytes<'a>(input: &mut &'a [u8]) -> io::Result<&'a [u8]> {
    let len =
        u32::from_le_bytes(take(input, 4)?.try_into().expect("a 4-byte slice is a u32")) as usize;
    take(input, len)
}

#[allow(
    dead_code,
    reason = "the host binary encodes and the helper binary decodes"
)]
pub(crate) fn decode(mut input: &[u8]) -> io::Result<DecodedRequest> {
    let magic = take(&mut input, MAGIC.len())?;
    if magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "atomic-publish frame has an unknown version",
        ));
    }
    let parent = take_bytes(&mut input)?.to_vec();
    let target = take_bytes(&mut input)?.to_vec();
    let preimage_kind = take(&mut input, 1)?[0];
    let preimage_digest = take_bytes(&mut input)?.to_vec();
    let request_id = take_bytes(&mut input)?.to_vec();
    let content_len = u64::from_le_bytes(
        take(&mut input, 8)?
            .try_into()
            .expect("an 8-byte slice is a u64"),
    ) as usize;
    let content = take(&mut input, content_len)?.to_vec();
    if !input.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "atomic-publish frame has trailing bytes",
        ));
    }
    if !matches!(preimage_kind, PREIMAGE_ABSENT | PREIMAGE_SHA256) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "atomic-publish frame has an unknown preimage kind",
        ));
    }
    let text = |bytes: Vec<u8>, label: &'static str| -> io::Result<String> {
        String::from_utf8(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, label))
    };
    Ok(DecodedRequest {
        parent: text(parent, "atomic-publish parent is not valid UTF-8")?,
        target: text(target, "atomic-publish target is not valid UTF-8")?,
        preimage_kind,
        preimage_digest: text(preimage_digest, "atomic-publish digest is not valid UTF-8")?,
        request_id: text(request_id, "atomic-publish request id is not valid UTF-8")?,
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(request_id: &str, target: &str, content: &[u8]) -> Vec<u8> {
        encode(EncodedRequest {
            parent: "/p",
            target,
            preimage_kind: PREIMAGE_SHA256,
            preimage_digest: &"ab".repeat(32),
            request_id,
            content,
        })
    }

    #[test]
    fn a_frame_round_trips_exactly() {
        let bytes = encoded("req-1", "/p/a.txt", b"hello");
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.parent, "/p");
        assert_eq!(decoded.target, "/p/a.txt");
        assert_eq!(decoded.preimage_kind, PREIMAGE_SHA256);
        assert_eq!(decoded.preimage_digest, "ab".repeat(32));
        assert_eq!(decoded.request_id, "req-1");
        assert_eq!(decoded.content, b"hello");
    }

    #[test]
    fn framing_is_unambiguous_across_field_boundaries() {
        // Naive concatenation would make these two byte-identical.
        assert_ne!(
            encoded("xy", "/p/a.txt", b""),
            encoded("x", "/p/a.txt", b"")
        );
        assert_ne!(
            encoded("r", "/p/ab.txt", b""),
            encoded("r", "/p/a.txt", b"")
        );
        assert_eq!(
            decode(&encoded("xy", "/p/a.txt", b"")).unwrap().request_id,
            "xy"
        );
        assert_eq!(
            decode(&encoded("x", "/p/a.txt", b"")).unwrap().request_id,
            "x"
        );
    }

    #[test]
    fn truncated_trailing_and_unversioned_frames_are_rejected() {
        let bytes = encoded("req", "/p/a.txt", b"body");
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        let mut wrong_magic = bytes.clone();
        wrong_magic[0] = b'X';
        assert!(decode(&wrong_magic).is_err());
    }

    #[test]
    fn an_unknown_preimage_kind_is_rejected_rather_than_guessed() {
        let mut bytes = encoded("req", "/p/a.txt", b"");
        let magic_len = MAGIC.len();
        // The kind byte sits after magic + parent + target segments.
        let mut offset = magic_len;
        for _ in 0..2 {
            let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4 + len;
        }
        bytes[offset] = 9;
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn content_with_nul_and_high_bytes_survives() {
        let content = vec![0u8, 0xff, 0x00, 0x41];
        let bytes = encoded("req", "/p/a.txt", &content);
        assert_eq!(decode(&bytes).unwrap().content, content);
    }

    #[test]
    fn absent_and_sha256_preimages_stay_distinguishable() {
        let absent = encode(EncodedRequest {
            parent: "/p",
            target: "/p/a.txt",
            preimage_kind: PREIMAGE_ABSENT,
            preimage_digest: "",
            request_id: "req",
            content: b"",
        });
        assert_eq!(decode(&absent).unwrap().preimage_kind, PREIMAGE_ABSENT);
        assert_ne!(absent, encoded("req", "/p/a.txt", b""));
    }
}
