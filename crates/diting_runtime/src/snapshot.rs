//! Our snapshot container — the three-segment format the campaign plan
//! promised ([v8 blob][sidecar][u64 v8-len]), plus the external reference
//! table snapshots and restores must agree on.
//!
//! Batch 5 of the crate's roadmap. The segment skeleton is deno_core's
//! (`runtime/snapshot.rs` serialize/deconstruct — law learned, code ours):
//! a trailing fixed-width length lets `deconstruct` split without scanning
//! either segment. Two deliberate divergences, both ours to keep:
//!
//! - the tail length is a **fixed u64 LE**, where deno writes `usize` — a
//!   blob produced on a 64-bit host stays loadable on a 32-bit target
//!   (the aginxos musl port lives in this repo's future).
//! - the **sidecar is ours**: a u32 version word first, v1 carrying no
//!   further state. The promise table, the settle face, the op stubs all
//!   live in the V8 blob itself; the sidecar exists for the runtime state
//!   that ISN'T heap state, and grows field by field when such state
//!   appears. A version mismatch fails loudly instead of decoding
//!   nonsense (deno's snapshot_format.rs makes the same trade).
//!
//! Creation and restore (the V8-touching half) live in [`crate::vm`] next
//! to the prelude they bake; this module stays pure bytes plus the one
//! table both sides must share.

use std::borrow::Cow;

/// Sidecar layout version. Bump when fields are appended; loaders reject
/// other versions loudly.
pub const SIDECAR_VERSION: u32 = 1;

/// Width of the trailing v8-blob length, fixed forever (see module doc).
const V8LEN: usize = std::mem::size_of::<u64>();

/// The external references table — every raw native callback baked into a
/// snapshot, in the SAME order at bake and restore. V8 stores snapshotted
/// callbacks by INDEX into this table and resolves them when the blob
/// deserializes (deno_core's create_external_references is the same law:
/// it too builds its Vec per call). Order is load-bearing: appending is
/// safe, reordering or removing breaks every existing blob. The function
/// is the single source of truth — call sites never build their own vec.
///
/// No Rust `static`: the union carries raw pointers, so it isn't `Sync`,
/// and it doesn't need to be — the Cow moves into `CreateParams`, which
/// keeps the backing storage alive in the isolate annex for as long as
/// V8 may read it.
///
/// SAFETY: the safety contract is with V8, not Rust — entries must stay
/// valid and stable for the process lifetime. Function pointers into our
/// own text segment are exactly that.
pub(crate) fn external_references() -> Cow<'static, [v8::ExternalReference]> {
    vec![v8::ExternalReference {
        function: crate::vm::spawn_callback,
    }]
    .into()
}

/// The v1 sidecar: version word only.
pub fn sidecar_v1() -> Vec<u8> {
    SIDECAR_VERSION.to_le_bytes().to_vec()
}

/// What came out of a container.
#[derive(Debug, PartialEq, Eq)]
pub struct Deconstructed<'a> {
    pub v8_blob: &'a [u8],
    pub sidecar: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// Fewer bytes than the tail length plus a sidecar version word.
    TooShort,
    /// The tail length points past the buffer — not our format.
    BadSplit { v8_len: u64 },
    /// A sidecar from a different layout generation.
    BadVersion { expected: u32, found: u32 },
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::TooShort => write!(f, "snapshot too short to split"),
            SnapshotError::BadSplit { v8_len } => {
                write!(f, "snapshot tail length {v8_len} points past the buffer")
            }
            SnapshotError::BadVersion { expected, found } => write!(
                f,
                "snapshot sidecar version mismatch: expected {expected}, found {found}"
            ),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Pack a container: `[v8 blob][sidecar][u64 LE = v8 blob len]`.
pub fn serialize(v8_data: &[u8], sidecar: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v8_data.len() + sidecar.len() + V8LEN);
    out.extend_from_slice(v8_data);
    out.extend_from_slice(sidecar);
    out.extend_from_slice(&(v8_data.len() as u64).to_le_bytes());
    out
}

/// Split a container, validating the layout and the sidecar version. The
/// v8 blob's own integrity is V8's business at restore time (it fails
/// loudly on garbage); this only guarantees the split is honest.
pub fn deconstruct(bytes: &[u8]) -> Result<Deconstructed<'_>, SnapshotError> {
    if bytes.len() < V8LEN + std::mem::size_of::<u32>() {
        return Err(SnapshotError::TooShort);
    }
    let tail = &bytes[bytes.len() - V8LEN..];
    let v8_len = u64::from_le_bytes(tail.try_into().unwrap());
    let body_end = bytes.len() - V8LEN;
    if v8_len as usize > body_end {
        return Err(SnapshotError::BadSplit { v8_len });
    }
    let v8_blob = &bytes[..v8_len as usize];
    let sidecar = &bytes[v8_len as usize..body_end];
    if sidecar.len() < std::mem::size_of::<u32>() {
        return Err(SnapshotError::TooShort);
    }
    let found = u32::from_le_bytes(sidecar[..4].try_into().unwrap());
    if found != SIDECAR_VERSION {
        return Err(SnapshotError::BadVersion {
            expected: SIDECAR_VERSION,
            found,
        });
    }
    Ok(Deconstructed { v8_blob, sidecar })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_returns_exact_segments() {
        let v8 = b"pretend this is a v8 blob \x00\x01\x02";
        let bytes = serialize(v8, &sidecar_v1());
        let parts = deconstruct(&bytes).unwrap();
        assert_eq!(parts.v8_blob, v8);
        assert_eq!(parts.sidecar, &sidecar_v1());
    }

    #[test]
    fn empty_v8_segment_is_representable() {
        // A degenerate but honest container: zero-length v8 segment.
        let bytes = serialize(&[], &sidecar_v1());
        let parts = deconstruct(&bytes).unwrap();
        assert!(parts.v8_blob.is_empty());
        assert_eq!(parts.sidecar.len(), 4);
    }

    #[test]
    fn too_short_is_rejected() {
        assert_eq!(deconstruct(&[]), Err(SnapshotError::TooShort));
        assert_eq!(
            deconstruct(&[0u8; 11]),
            Err(SnapshotError::TooShort),
            "11 bytes cannot hold an 8-byte tail plus a version word"
        );
    }

    #[test]
    fn tail_pointing_past_the_buffer_is_rejected() {
        let mut bytes = serialize(b"v8data", &sidecar_v1());
        // Overwrite the tail with an impossible length.
        let n = bytes.len();
        bytes[n - 8..].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            deconstruct(&bytes),
            Err(SnapshotError::BadSplit { v8_len: u64::MAX })
        );
    }

    #[test]
    fn wrong_sidecar_version_is_rejected_loudly() {
        let sidecar = 999u32.to_le_bytes();
        let bytes = serialize(b"v8data", &sidecar);
        assert_eq!(
            deconstruct(&bytes),
            Err(SnapshotError::BadVersion {
                expected: SIDECAR_VERSION,
                found: 999
            })
        );
    }

    #[test]
    fn sidecar_without_a_version_word_is_rejected() {
        // v8_len lands exactly at body_end: zero-length sidecar.
        let bytes = serialize(b"v8data", &[]);
        assert_eq!(deconstruct(&bytes), Err(SnapshotError::TooShort));
    }
}
