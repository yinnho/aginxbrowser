//! Typed diagram specs — the zero-coordinate JSON contract.
//!
//! The agent never writes coordinates. A sequence diagram is participants
//! plus an ordered message list; the adapter derives every x/y from fixed
//! column arithmetic and automatic row stacking (the "technology first"
//! doctrine: geometry belongs to the engine, semantics to the author).
//!
//! Vocabulary (participant kinds, message variants) is adapted from archify
//! (MIT), whose sequence renderer is the reference implementation — minus
//! its explicit `y`/`from`/`to` coordinates, which we compute instead.

use serde::Deserialize;

/// The seven participant kinds archify defines. Order matters only for
/// error messages; lookups are by string.
pub const PARTICIPANT_KINDS: [&str; 7] = [
    "frontend",
    "backend",
    "database",
    "cloud",
    "security",
    "messagebus",
    "external",
];

/// Message arrow variants. `return` renders dashed and muted; `dashed` is
/// the async/trace idiom; `security` tints red; `emphasis` draws heavier.
pub const MESSAGE_VARIANTS: [&str; 5] = ["default", "emphasis", "security", "dashed", "return"];

/// A ```archify fence body: one typed diagram. `diagram_type` routes to the
/// family adapter; v1 implements `sequence`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagramSpec {
    #[serde(default)]
    pub diagram_type: Option<String>,
    pub sequence: Option<SequenceSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequenceSpec {
    pub title: String,
    pub participants: Vec<Participant>,
    pub messages: Vec<Message>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub label: String,
    #[serde(default)]
    pub sublabel: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub from: String,
    pub to: String,
    pub label: String,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Validate a parsed spec into an ordered problem list. Empty = clean.
/// Deterministic order: structure first, then participants in order, then
/// messages in order — so the same input always yields the same receipt.
pub fn validate_sequence(spec: &SequenceSpec) -> Vec<String> {
    let mut problems = Vec::new();
    if spec.title.trim().is_empty() {
        problems.push("title must not be empty".to_string());
    }
    if spec.participants.len() < 2 {
        problems.push(format!(
            "a sequence diagram needs at least 2 participants, got {}",
            spec.participants.len()
        ));
    }
    if spec.messages.is_empty() {
        problems.push("a sequence diagram needs at least 1 message".to_string());
    }

    let mut seen = std::collections::HashSet::new();
    for p in &spec.participants {
        if !valid_id(&p.id) {
            problems.push(format!(
                "participant id \"{}\" must match [a-zA-Z][a-zA-Z0-9_-]*",
                p.id
            ));
        }
        if !seen.insert(p.id.as_str()) {
            problems.push(format!("participant id \"{}\" is used twice", p.id));
        }
        if !PARTICIPANT_KINDS.contains(&p.kind.as_str()) {
            problems.push(format!(
                "participant \"{}\" has unknown type \"{}\" (one of: {})",
                p.id,
                p.kind,
                PARTICIPANT_KINDS.join(", ")
            ));
        }
        if p.label.trim().is_empty() {
            problems.push(format!("participant \"{}\" must have a label", p.id));
        }
    }

    for m in &spec.messages {
        if !seen.contains(m.from.as_str()) {
            problems.push(format!(
                "message \"{}\" references unknown source \"{}\"",
                m.label, m.from
            ));
        }
        if !seen.contains(m.to.as_str()) {
            problems.push(format!(
                "message \"{}\" references unknown target \"{}\"",
                m.label, m.to
            ));
        }
        if m.from == m.to {
            problems.push(format!(
                "message \"{}\" is a self-call — route it through another participant or fold it into a note (self-loops are not laid out yet)",
                m.label
            ));
        }
        if m.label.trim().is_empty() {
            problems.push("message labels must not be empty".to_string());
        }
        if let Some(v) = &m.variant {
            if !MESSAGE_VARIANTS.contains(&v.as_str()) {
                problems.push(format!(
                    "message \"{}\" has unknown variant \"{}\" (one of: {})",
                    m.label,
                    v,
                    MESSAGE_VARIANTS.join(", ")
                ));
            }
        }
    }
    problems
}

/// Text width in abstract units: fullwidth/East-Asian glyphs count 2, the
/// rest 1, variation selectors count 0. Same model as archify's textUnits —
/// a monospace-ish advance estimate good enough for shrink-to-fit decisions.
pub fn text_units(text: &str) -> usize {
    let mut units = 0usize;
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i] as u32;
        if (0xfe00..=0xfe0f).contains(&c) {
            i += 1;
            continue;
        }
        units += if is_fullwidth(c) { 2 } else { 1 };
        i += 1;
    }
    units.max(1)
}

/// East-Asian wide/emoji code point ranges (archify's FULLWIDTH_RE, plus the
/// surrogate-pair extensions it reaches via codePointAt).
fn is_fullwidth(c: u32) -> bool {
    matches!(c,
        0x1100..=0x115F
        | 0x231A..=0x231B
        | 0x2329..=0x232A
        | 0x23E9..=0x23EC
        | 0x23F0 | 0x23F3
        | 0x25FD..=0x25FE
        | 0x2614..=0x2615
        | 0x2630..=0x2637
        | 0x2648..=0x2653
        | 0x267F | 0x268A..=0x268F
        | 0x2693 | 0x26A1
        | 0x26AA..=0x26AB
        | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5
        | 0x26CE | 0x26D4 | 0x26EA
        | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA | 0x26FD
        | 0x2705 | 0x270A..=0x270B
        | 0x2728 | 0x274C | 0x274E
        | 0x2753..=0x2755 | 0x2757
        | 0x2795..=0x2797 | 0x27B0 | 0x27BF
        | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55
        | 0x2E80..=0xA4CF
        | 0xA960..=0xA97C
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF01..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x16FE0..=0x18DFF
        | 0x1AFF0..=0x1AFFF
        | 0x1B000..=0x1B2FF
        | 0x1F000..=0x1FAFF
        | 0x20000..=0x3FFFD)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_party_spec() -> SequenceSpec {
        serde_json::from_str(
            r#"{
                "title": "T",
                "participants": [
                    {"id": "a", "type": "frontend", "label": "A"},
                    {"id": "b", "type": "backend", "label": "B"}
                ],
                "messages": [
                    {"from": "a", "to": "b", "label": "hi"}
                ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn minimal_spec_validates_clean() {
        assert!(validate_sequence(&two_party_spec()).is_empty());
    }

    #[test]
    fn validation_reports_in_document_order() {
        let bad: SequenceSpec = serde_json::from_str(
            r#"{
                "title": "",
                "participants": [
                    {"id": "1bad", "type": "nope", "label": ""},
                    {"id": "b", "type": "backend", "label": "B"}
                ],
                "messages": [
                    {"from": "ghost", "to": "b", "label": "", "variant": "nope"},
                    {"from": "b", "to": "b", "label": "self"}
                ]
            }"#,
        )
        .unwrap();
        let problems = validate_sequence(&bad);
        assert_eq!(problems.len(), 8, "{problems:?}");
        assert!(problems[0].contains("title"));
        assert!(problems[1].contains("1bad"));
        assert!(problems[2].contains("nope"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let bad = r#"{"title":"T","participants":[{"id":"a","type":"frontend","label":"A"},
            {"id":"b","type":"backend","label":"B"}],
            "messages":[{"from":"a","to":"b","label":"hi","y":160}]}"#;
        assert!(serde_json::from_str::<SequenceSpec>(bad).is_err());
    }

    #[test]
    fn text_units_counts_fullwidth_as_two() {
        assert_eq!(text_units("abc"), 3);
        assert_eq!(text_units("读取缓存"), 8);
        assert_eq!(text_units("GET /dashboard"), 14);
        // Variation selectors are width-zero.
        assert_eq!(text_units("a\u{FE0F}"), 1);
    }
}
