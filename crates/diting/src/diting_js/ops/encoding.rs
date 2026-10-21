// Text-encoding bridges (god-file ratchet split from mod.rs, pure move).
use super::*;

/// Canonical (lowercased) WHATWG name for a TextDecoder label, or "" if the
/// label is unknown (the JS constructor turns "" into a RangeError).
#[diting_op]
#[string]
pub(crate) fn op_encoding_for_label(#[string] label: &str) -> String {
    crate::diting_net::label_name(label).unwrap_or_default()
}

/// Decode bytes with a legacy/explicit encoding via encoding_rs. Returns
/// {"ok":true,"v":<string>} or {"ok":false} (unknown label, or a fatal decode
/// error). The UTF-8 non-fatal common case is handled in JS without this op.
#[diting_op]
#[string]
pub(crate) fn op_text_decode(#[string] label: &str, #[buffer] bytes: &[u8], fatal: bool, ignore_bom: bool) -> String {
    match crate::diting_net::decode_with_label(label, bytes, fatal, ignore_bom) {
        Some(s) => serde_json::json!({ "ok": true, "v": s }).to_string(),
        None => "{\"ok\":false}".to_string(),
    }
}

/// Re-encode a URL query component using a non-UTF-8 document encoding override
/// (the WHATWG "encoding override"). `query` is the already-UTF-8-decoded query
/// string; `label` the target charset; `special` whether the URL has a special
/// scheme (adds `'` to the percent-encode set). Returns the encoded query, or
/// the input unchanged if the label is unknown. Only called by the JS anchor
/// path when the document is non-UTF-8, so the UTF-8 hot path never reaches it.
#[diting_op]
#[string]
pub(crate) fn op_url_encode_query(#[string] query: &str, #[string] label: &str, special: bool) -> String {
    crate::diting_net::url_encode_query(query, label, special).unwrap_or_else(|| query.to_string())
}
