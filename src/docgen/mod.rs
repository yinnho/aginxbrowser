//! docgen — agent supplies markdown, the engine emits deterministic HTML.
//!
//! The product contract: the LLM never writes HTML. Prose rides a plain
//! shell; diagrams ride ```archify fenced blocks carrying zero-coordinate
//! typed JSON, which the family adapters turn into SVG with fixed-column
//! arithmetic. Same input in, same bytes out — the receipt carries the
//! sha256 so a caller can verify determinism themselves.
//!
//! Diagram vocabulary and layout constants are adapted from archify (MIT);
//! this module owns the zero-coordinate contract and the deterministic
//! emission.

pub mod graph;
pub mod sequence;
pub mod shell;
pub mod spec;
pub mod workflow;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Format tenths-of-a-pixel as a compact decimal: integer when whole, one
/// place when not. `{:.1}` on an exact x/10 rounds correctly, so bytes are
/// stable.
pub(crate) fn tx(t: i32) -> String {
    if t % 10 == 0 {
        format!("{}", t / 10)
    } else {
        format!("{:.1}", t as f64 / 10.0)
    }
}

pub struct RenderOutcome {
    pub html: String,
    pub receipt: Value,
}

/// Render a markdown document into a self-contained HTML artifact plus a
/// receipt (checks, diagnostics, sha256 of the artifact bytes).
pub fn render(markdown: &str) -> RenderOutcome {
    let doc = shell::render(markdown);

    let rendered: Vec<&shell::FenceOutcome> = doc.fences.iter().filter(|f| f.ok).collect();
    let failed: Vec<&shell::FenceOutcome> = doc.fences.iter().filter(|f| !f.ok).collect();

    let mut checks = Vec::new();
    if !rendered.is_empty() {
        checks.push(format!(
            "{} diagram{} rendered deterministically",
            rendered.len(),
            if rendered.len() == 1 { "" } else { "s" }
        ));
    }
    if doc.fences.is_empty() {
        checks.push("no archify fences found — prose-only document".to_string());
    }
    if failed.is_empty() && !doc.fences.is_empty() {
        checks.push("all fences parsed and validated clean".to_string());
    }

    let diagnostics: Vec<String> = failed
        .iter()
        .flat_map(|f| {
            f.detail
                .iter()
                .map(move |d| format!("archify fence #{}: {}", f.index + 1, d))
        })
        .collect();

    let diagrams: Vec<Value> = doc
        .fences
        .iter()
        .filter(|f| f.ok)
        .map(|f| {
            json!({
                "index": f.index,
                "type": f.kind,
                "title": f.title,
                "facts": f.detail,
            })
        })
        .collect();

    let mut hasher = Sha256::new();
    hasher.update(doc.html.as_bytes());
    let sha256 = format!("{:x}", hasher.finalize());

    let receipt = json!({
        "bytes": doc.html.len(),
        "diagrams": diagrams,
        "checks": checks,
        "diagnostics": diagnostics,
        "sha256": sha256,
    });
    RenderOutcome {
        html: doc.html,
        receipt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Deploy flow\n\nProse before.\n\n```archify\n{\"sequence\":{\"title\":\"Cache miss\",\"participants\":[\n  {\"id\":\"user\",\"type\":\"external\",\"label\":\"User\"},\n  {\"id\":\"api\",\"type\":\"backend\",\"label\":\"API\"},\n  {\"id\":\"redis\",\"type\":\"database\",\"label\":\"Redis\",\"sublabel\":\"cache\"}],\n \"messages\":[\n  {\"from\":\"user\",\"to\":\"api\",\"label\":\"GET /x\",\"variant\":\"emphasis\"},\n  {\"from\":\"api\",\"to\":\"redis\",\"label\":\"read cache\"},\n  {\"from\":\"redis\",\"to\":\"api\",\"label\":\"miss\",\"variant\":\"return\"}]}}\n```\n\nProse after.\n";

    #[test]
    fn same_input_same_sha256() {
        let a = render(DOC);
        let b = render(DOC);
        assert_eq!(a.html, b.html, "artifact bytes must be identical");
        assert_eq!(
            a.receipt["sha256"], b.receipt["sha256"],
            "receipt hashes must match"
        );
        // The receipt hash is over the artifact itself.
        let mut h = Sha256::new();
        h.update(a.html.as_bytes());
        assert_eq!(a.receipt["sha256"], format!("{:x}", h.finalize()));
    }

    #[test]
    fn receipt_counts_and_structural_facts() {
        let r = render(DOC);
        assert_eq!(r.receipt["diagrams"].as_array().unwrap().len(), 1);
        assert_eq!(r.receipt["diagnostics"].as_array().unwrap().len(), 0);
        let facts = r.receipt["diagrams"][0]["facts"].as_array().unwrap();
        assert!(facts.iter().any(|f| f.as_str().unwrap().contains("participants=3")));
        assert!(facts.iter().any(|f| f.as_str().unwrap().contains("messages=3")));
        // Structural assertion: the SVG carries the nodes and edges.
        assert!(r.html.contains("data-participant-id=\"redis\""));
        assert!(r.html.contains("data-from=\"api\" data-to=\"redis\""));
        assert!(r.html.contains("GET /x"));
    }

    #[test]
    fn broken_diagram_lands_in_diagnostics_not_a_crash() {
        let md = "# T\n\n```archify\n{\"sequence\":{\"title\":\"X\"}}\n```\n";
        let r = render(md);
        // `participants` is a required field, so this dies at the JSON layer
        // with one parse diagnostic naming the field.
        assert_eq!(r.receipt["diagnostics"].as_array().unwrap().len(), 1);
        assert!(r.receipt["diagnostics"][0]
            .as_str()
            .unwrap()
            .contains("participants"));
        assert!(r.receipt["diagnostics"][0]
            .as_str()
            .unwrap()
            .contains("fence #1"));
        // Document still emits, with the fence as a code block.
        assert!(r.html.contains("language-archify"));
    }

    #[test]
    fn workflow_fence_round_trips_through_the_shell() {
        let md = "# Deploy\n\n```archify\n{\"workflow\":{\"title\":\"Release\",\"lanes\":[\n  {\"id\":\"dev\",\"label\":\"Dev\"},\n  {\"id\":\"ops\",\"label\":\"Ops\",\"variant\":\"exception\"}],\n \"nodes\":[\n  {\"id\":\"build\",\"lane\":\"dev\",\"col\":0,\"label\":\"Build\",\"type\":\"backend\"},\n  {\"id\":\"ship\",\"lane\":\"dev\",\"col\":1,\"label\":\"Ship\",\"type\":\"backend\"},\n  {\"id\":\"rollback\",\"lane\":\"ops\",\"col\":1,\"label\":\"Rollback\",\"type\":\"security\"}],\n \"edges\":[\n  {\"from\":\"build\",\"to\":\"ship\",\"label\":\"ci pass\"},\n  {\"from\":\"ship\",\"to\":\"rollback\",\"label\":\"500s\",\"variant\":\"security\"}]}}\n```\n";
        let a = render(md);
        let b = render(md);
        assert_eq!(a.html, b.html, "same input, same bytes");
        assert_eq!(a.receipt["diagnostics"].as_array().unwrap().len(), 0);
        assert_eq!(a.receipt["diagrams"][0]["type"], "workflow");
        let facts = a.receipt["diagrams"][0]["facts"].as_array().unwrap();
        assert!(facts.iter().any(|f| f.as_str().unwrap().contains("lanes=2")));
        assert!(facts.iter().any(|f| f.as_str().unwrap().contains("edges=2")));
        assert!(facts.iter().any(|f| f.as_str().unwrap().starts_with("viewBox=")));
        // Structural assertions on the artifact itself.
        assert!(a.html.contains("data-diagram-type=\"workflow\""));
        assert!(a.html.contains("data-lane-id=\"ops\""));
        assert!(a.html.contains("data-node-id=\"rollback\""));
        assert!(a.html.contains("data-from=\"ship\" data-to=\"rollback\""));
    }

    #[test]
    fn geometry_is_bounded_by_viewbox() {
        let r = render(DOC);
        let vb = r.receipt["diagrams"][0]["facts"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|f| {
                let s = f.as_str()?;
                s.strip_prefix("viewBox=")
            })
            .unwrap()
            .to_string();
        let (w, h): (i32, i32) = {
            let mut parts = vb.trim_end_matches(char::is_alphabetic).split('x');
            let w: i32 = parts.next().unwrap().parse().unwrap();
            let h: i32 = parts.next().unwrap().parse().unwrap();
            (w, h)
        };
        // 3 participants → width = 105 + 2*108 + 43 + 40 = 404; 3 messages →
        // height = 142 + 30 + 2*30 + 40 + 48 = 320.
        assert_eq!((w, h), (404, 320));
        // Every path anchor (lifelines, arrows, arrowheads) starts with
        // `d="M <x> <y> ...` — each anchor's x stays within the viewBox.
        let svg = r.html.split("<svg").nth(1).unwrap().split("</svg>").next().unwrap();
        let anchors = svg.split("d=\"M ").skip(1).count();
        assert!(anchors >= 5, "lifelines + arrows + heads, got {anchors}");
        for seg in svg.split("d=\"M ").skip(1) {
            let attrs = seg.split('"').next().unwrap();
            let x: f64 = attrs
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap_or_else(|_| panic!("bad anchor start: {attrs:?}"));
            assert!(x >= 0.0 && x <= w as f64, "x {x} escapes {w}");
        }
    }
}
