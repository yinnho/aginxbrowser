//! Sequence family adapter: typed zero-coordinate spec → deterministic SVG.
//!
//! Layout is fixed-column arithmetic (archify's constants, owned here):
//! participant centers sit on a 108px lattice, messages stack one row per
//! entry at a 30px rhythm. Every coordinate is computed in integer tenths
//! of a pixel so the output bytes are identical on every platform — the
//! golden test hashes them.
//!
//! Styling is plain presentation attributes, no `<style>` block: an inline
//! SVG's `<style>` leaks document-wide in HTML, and the built-in renderer
//! (diting svg v1) draws attributes without a CSS pass. Arrowheads are
//! explicit triangles, not `<marker>` references, for the same reason.

use super::sigil;
use super::spec::{text_units, SequenceSpec};
use super::theme::Theme;

// Tenths of a pixel. PX = /10.
const TOP_Y: i32 = 720;
const PARTICIPANT_W: i32 = 860;
const PARTICIPANT_H: i32 = 540;
const LIFELINE_TOP: i32 = 1420;
const SIDE_MARGIN: i32 = 620;
const COL_GAP: i32 = 1080;
const RIGHT_MARGIN: i32 = 400;
const ROW_STEP: i32 = 300;
const BOTTOM_PAD: i32 = 480;
const ARROW_INSET: i32 = 70;
const HEAD_LEN: i32 = 80;
const LABEL_H: i32 = 160;
const TITLE_Y: i32 = 340;
/// Fitted-font geometry shared with archify's text-fit: 0.6px advance per
/// unit per px of size, 8px reserved inside the box.
const TEXT_WIDTH_FACTOR: i32 = 6; // tenths of px per unit per tenth-of-px size
const TEXT_PADDING: i32 = 80;
const LABEL_PREFERRED: i32 = 110;
const LABEL_MIN: i32 = 80;
const SUBLABEL_PREFERRED: i32 = 70;
const SUBLABEL_MIN: i32 = 60;

#[derive(Debug)]
pub struct RenderedSequence {
    pub svg: String,
    pub title: String,
    pub view_box: [i32; 2],
    pub participants: usize,
    pub messages: usize,
    /// Composition audit over the placed geometry (profile-independent).
    pub composition: super::checks::Composition,
}

/// A participant's center-x on the column lattice.
fn cx(index: usize) -> i32 {
    SIDE_MARGIN + PARTICIPANT_W / 2 + index as i32 * COL_GAP
}

fn message_y(index: usize) -> i32 {
    LIFELINE_TOP + 300 + index as i32 * ROW_STEP
}

/// Largest font size (tenths) at or below `preferred` fitting `text` in
/// `available` tenths, floored at `minimum` — archify's fittedNodeFontSize.
fn fitted_font(text: &str, available: i32, preferred: i32, minimum: i32) -> i32 {
    let units = text_units(text) as i32;
    let by_width = available * 10 / (units * TEXT_WIDTH_FACTOR);
    preferred.min(by_width).max(minimum)
}

/// Does `text` still overflow `available` at its legible minimum?
fn overflows_at_minimum(text: &str, available: i32, minimum: i32) -> bool {
    let units = text_units(text) as i32;
    units * TEXT_WIDTH_FACTOR * minimum > available * 10
}

/// (stroke, width, dash, text fill) per message variant — sizes and dash
/// patterns are this adapter's typography; the colors come from the theme.
fn variant_style(theme: &Theme, variant: &str) -> (&'static str, i32, Option<&'static str>, &'static str) {
    match variant {
        "emphasis" => (theme.ink, 18, None, theme.ink),
        "security" => (theme.danger, 14, None, theme.danger),
        "dashed" => (theme.skip, 14, Some("6 4"), theme.skip),
        "return" => (theme.ink_muted, 14, Some("3 5"), theme.ink_muted),
        _ => (theme.ink_soft, 14, None, theme.edge_label),
    }
}

/// Format tenths as a compact decimal: integer when whole, one place when
/// not. `{:.1}` on an exact x/10 rounds correctly, so bytes are stable.
use super::tx;

/// XML-escape text content and attribute values; collapse newlines/tabs so
/// a label never smuggles structure into single-line `<text>`.
fn esc(s: &str) -> String {
    let flat = s.replace(['\n', '\r', '\t'], " ");
    flat.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render a validated spec. Geometry-level failures (labels no shrink can
/// rescue) come back as problems for the receipt; the caller falls the
/// fence back to a code block.
pub fn render_sequence(
    spec: &SequenceSpec,
    theme: &'static Theme,
) -> Result<RenderedSequence, Vec<String>> {
    let mut problems = Vec::new();
    let available = PARTICIPANT_W - TEXT_PADDING;
    for p in &spec.participants {
        if overflows_at_minimum(&p.label, available, LABEL_MIN) {
            problems.push(format!(
                "participant \"{}\" label \"{}\" cannot fit the {}px box even at the {}px legible minimum — shorten it",
                p.id,
                p.label,
                PARTICIPANT_W / 10,
                LABEL_MIN / 10
            ));
        }
        if let Some(sub) = &p.sublabel {
            if overflows_at_minimum(sub, available, SUBLABEL_MIN) {
                problems.push(format!(
                    "participant \"{}\" sublabel \"{}\" cannot fit the {}px box at the {}px minimum — shorten it",
                    p.id,
                    sub,
                    PARTICIPANT_W / 10,
                    SUBLABEL_MIN / 10
                ));
            }
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }

    let n = spec.participants.len();
    let width = cx(n - 1) + PARTICIPANT_W / 2 + RIGHT_MARGIN;
    let lifeline_bottom = message_y(spec.messages.len() - 1) + 400;
    let height = lifeline_bottom + BOTTOM_PAD;

    let mut svg = String::with_capacity(4096);
    // Audit records: one horizontal run per message row, collected during
    // emission so the audit and the drawn geometry cannot drift.
    let mut audit_routes: Vec<super::checks::AuditRoute> = Vec::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {} {}\" role=\"img\" aria-label=\"{}\">",
        tx(width),
        tx(height),
        esc(&spec.title)
    ));

    // Title band, then lifelines, messages, and participants painted on top
    // (archify's paint order — participant boxes must cover their lifeline
    // stubs).
    svg.push_str(&format!(
        "<text x=\"{}\" y=\"{}\" font-size=\"13\" font-weight=\"600\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
        tx(width / 2),
        tx(TITLE_Y),
        theme.ink,
        esc(spec.title.trim())
    ));

    for (i, _) in spec.participants.iter().enumerate() {
        let x = cx(i);
        svg.push_str(&format!(
            "<path d=\"M {} {} L {} {}\" stroke=\"{}\" stroke-width=\"0.8\" stroke-dasharray=\"3 7\" fill=\"none\"/>",
            tx(x),
            tx(LIFELINE_TOP),
            tx(x),
            tx(lifeline_bottom),
            theme.guide
        ));
    }

    for (i, m) in spec.messages.iter().enumerate() {
        let from = spec
            .participants
            .iter()
            .position(|p| p.id == m.from)
            .expect("validated: from resolves");
        let to = spec
            .participants
            .iter()
            .position(|p| p.id == m.to)
            .expect("validated: to resolves");
        let (from_x, to_x) = (cx(from), cx(to));
        let dir = if to_x > from_x { 1 } else { -1 };
        let y = message_y(i);
        let start = from_x + dir * ARROW_INSET;
        let end = to_x - dir * ARROW_INSET;
        let head_base = end - dir * HEAD_LEN;
        let variant = m.variant.as_deref().unwrap_or("default");
        let (stroke, stroke_w, dash, text_fill) = variant_style(theme, variant);

        svg.push_str(&format!(
            "<g data-message-index=\"{}\" data-from=\"{}\" data-to=\"{}\" data-variant=\"{}\">",
            i,
            esc(&m.from),
            esc(&m.to),
            variant
        ));
        svg.push_str(&format!(
            "<path d=\"M {} {} L {} {}\" stroke=\"{}\" stroke-width=\"{}\"{} fill=\"none\"/>",
            tx(start),
            tx(y),
            tx(head_base),
            tx(y),
            stroke,
            tx(stroke_w),
            dash.map(|d| format!(" stroke-dasharray=\"{d}\"")).unwrap_or_default()
        ));
        // Explicit arrowhead (diting svg v1 draws paths, not marker refs).
        svg.push_str(&format!(
            "<path d=\"M {} {} L {} {} L {} {} Z\" fill=\"{}\"/>",
            tx(end),
            tx(y),
            tx(head_base),
            tx(y - 40),
            tx(head_base),
            tx(y + 40),
            stroke
        ));

        // Label: white mask so long labels stay readable across lifelines.
        let units = text_units(&m.label) as i32;
        let label_w = (340).max(units * 52 + 120);
        let center = (start + end) / 2;
        audit_routes.push(super::checks::AuditRoute {
            name: if m.label.trim().is_empty() {
                format!("{}->{}", m.from, m.to)
            } else {
                m.label.trim().to_string()
            },
            from: m.from.clone(),
            to: m.to.clone(),
            points: vec![(start, y), (end, y)],
            label: Some(super::graph::Rect {
                x: center - label_w / 2,
                y: y - 200,
                w: label_w,
                h: LABEL_H,
            }),
        });
        svg.push_str(&format!(
            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"3\" fill=\"{}\" fill-opacity=\"0.85\"/>",
            tx(center - label_w / 2),
            tx(y - 200),
            tx(label_w),
            tx(LABEL_H),
            theme.panel_alt
        ));
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"9\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
            tx(center),
            tx(y - 100),
            text_fill,
            esc(m.label.trim())
        ));
        if let Some(note) = &m.note {
            svg.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"7\" fill=\"{}\">{}</text>",
                tx(start.min(end) + 120),
                tx(y + 180),
                theme.ink_muted,
                esc(note.trim())
            ));
        }
        svg.push_str("</g>");
    }

    for (i, p) in spec.participants.iter().enumerate() {
        let x = cx(i);
        let node = theme.node(&p.kind);
        let label_font = fitted_font(&p.label, available, LABEL_PREFERRED, LABEL_MIN);
        svg.push_str(&format!(
            "<g data-participant-id=\"{}\" data-kind=\"{}\">",
            esc(&p.id),
            esc(&p.kind)
        ));
        svg.push_str(&format!(
            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"6\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1.5\"/>",
            tx(x - PARTICIPANT_W / 2),
            tx(TOP_Y),
            tx(PARTICIPANT_W),
            tx(PARTICIPANT_H),
            node.fill,
            node.stroke
        ));
        svg.push_str(&sigil::sigil(
            &p.kind,
            x - PARTICIPANT_W / 2 + 60,
            TOP_Y + 60,
            theme,
        ));
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"{}\" font-weight=\"600\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
            tx(x),
            tx(TOP_Y + 220),
            tx(label_font),
            theme.ink,
            esc(p.label.trim())
        ));
        if let Some(sub) = &p.sublabel {
            let sub_font = fitted_font(sub, available, SUBLABEL_PREFERRED, SUBLABEL_MIN);
            svg.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
                tx(x),
                tx(TOP_Y + 390),
                tx(sub_font),
                theme.ink_muted,
                esc(sub.trim())
            ));
        }
        svg.push_str("</g>");
    }

    svg.push_str("</svg>");

    // Composition audit: message rows have unique ys, so crossings and
    // corridors are structurally absent — this contributes label clearance
    // and readability. Readability mirrors the emission's fitted-font call
    // for the primary participant label.
    let min_label_font = spec
        .participants
        .iter()
        .map(|p| fitted_font(&p.label, available, LABEL_PREFERRED, LABEL_MIN))
        .min();
    let composition = super::checks::audit(&super::checks::AuditScene {
        routes: &audit_routes,
        frames: &[],
        readability: min_label_font.map(|f| super::checks::Readability {
            view_box_w: width,
            min_label_font: f,
        }),
    });

    Ok(RenderedSequence {
        svg,
        title: spec.title.trim().to_string(),
        view_box: [width / 10, height / 10],
        participants: spec.participants.len(),
        messages: spec.messages.len(),
        composition,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::theme::LIGHT;

    fn spec(json: &str) -> SequenceSpec {
        let s: SequenceSpec = serde_json::from_str(json).unwrap();
        assert!(super::super::spec::validate_sequence(&s).is_empty());
        s
    }

    fn minimal() -> SequenceSpec {
        spec(
            r#"{"title":"Ping","participants":[
                {"id":"a","type":"frontend","label":"Client"},
                {"id":"b","type":"backend","label":"Server"}],
               "messages":[{"from":"a","to":"b","label":"ping"},
                           {"from":"b","to":"a","label":"pong","variant":"return"}]}"#,
        )
    }

    #[test]
    fn geometry_is_fixed_column_fixed_row() {
        let r = render_sequence(&minimal(), &LIGHT).unwrap();
        // 2 participants: width = cx(1) + W/2 + 40 = 213 + 43 + 40 = 296.
        // 2 messages: height = 142 + 30 + 30 + 40 + 48 = 290.
        assert_eq!(r.view_box, [296, 290]);
        // Lifelines exist for both; 2 message groups; 2 participant groups.
        assert_eq!(r.svg.matches("stroke-dasharray=\"3 7\"").count(), 2);
        assert_eq!(r.svg.matches("data-message-index").count(), 2);
        assert_eq!(r.svg.matches("data-participant-id").count(), 2);
        // Right-going first message: start 7px right of a's center (105),
        // line stops one head-length (8px) short of the inset target.
        assert!(
            r.svg.contains("d=\"M 112 172 L 198 172\""),
            "first arrow wrong"
        );
    }

    #[test]
    fn left_going_message_flips_insets() {
        let r = render_sequence(&minimal(), &LIGHT).unwrap();
        // pong goes b→a: start = 213-7 = 206, head base = 105+7+8 = 120.
        assert!(r.svg.contains("M 206 202 L 120 202"), "left arrow missing");
    }

    #[test]
    fn identical_input_produces_identical_bytes() {
        let a = render_sequence(&minimal(), &LIGHT).unwrap().svg;
        let b = render_sequence(&minimal(), &LIGHT).unwrap().svg;
        assert_eq!(a, b);
        assert_eq!(a.matches("<svg").count(), 1);
    }

    #[test]
    fn long_labels_shrink_then_reject() {
        // 16 units at 8px minimum * 0.6 = 76.8px fits 78px; 17 does not.
        let fits = spec(&format!(
            r#"{{"title":"T","participants":[
                {{"id":"a","type":"frontend","label":"{}"}},
                {{"id":"b","type":"backend","label":"B"}}],
               "messages":[{{"from":"a","to":"b","label":"x"}}]}}"#,
            "x".repeat(16)
        ));
        assert!(render_sequence(&fits, &LIGHT).is_ok());
        let rejects = spec(&format!(
            r#"{{"title":"T","participants":[
                {{"id":"a","type":"frontend","label":"{}"}},
                {{"id":"b","type":"backend","label":"B"}}],
               "messages":[{{"from":"a","to":"b","label":"x"}}]}}"#,
            "x".repeat(17)
        ));
        let problems = render_sequence(&rejects, &LIGHT).unwrap_err();
        assert!(problems[0].contains("cannot fit"), "{problems:?}");
    }

    #[test]
    fn cjk_labels_measure_double_width() {
        // 4 CJK chars = 8 units: at min 8px → 38.4px, fits.
        let s = spec(
            r#"{"title":"缓存","participants":[
                {"id":"a","type":"frontend","label":"读取缓存"},
                {"id":"b","type":"database","label":"Postgres"}],
               "messages":[{"from":"a","to":"b","label":"查询"}]}"#,
        );
        let r = render_sequence(&s, &LIGHT).unwrap();
        assert!(r.svg.contains("读取缓存"));
        assert!(r.svg.contains("查询"));
    }

    #[test]
    fn variants_carry_their_strokes() {
        let s = spec(
            r#"{"title":"V","participants":[
                {"id":"a","type":"frontend","label":"A"},
                {"id":"b","type":"backend","label":"B"}],
               "messages":[
                   {"from":"a","to":"b","label":"e","variant":"emphasis"},
                   {"from":"a","to":"b","label":"s","variant":"security"},
                   {"from":"a","to":"b","label":"d","variant":"dashed"},
                   {"from":"b","to":"a","label":"r","variant":"return"}]}"#,
        );
        let svg = render_sequence(&s, &LIGHT).unwrap().svg;
        assert!(svg.contains("stroke-width=\"1.8\""));
        assert!(svg.contains("#dc2626"));
        assert!(svg.contains("stroke-dasharray=\"6 4\""));
        assert!(svg.contains("stroke-dasharray=\"3 5\""));
    }
}
