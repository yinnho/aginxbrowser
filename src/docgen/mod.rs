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

pub mod architecture;
pub mod checks;
pub mod dataflow;
pub mod graph;
#[cfg(test)]
mod gate;
pub mod lifecycle;
pub mod motion;
pub mod sequence;
pub mod shell;
pub mod sigil;
pub mod spec;
pub mod theme;
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

/// Render with the default (light) theme, static posture — the historical
/// shorthand; production rides [`render_with_quality`].
#[cfg(test)]
pub fn render(markdown: &str) -> RenderOutcome {
    render_with_theme(markdown, &theme::LIGHT)
}

/// Render with an explicit theme, static posture — the historical
/// shorthand; production rides [`render_with_quality`].
#[cfg(test)]
pub fn render_with_theme(markdown: &str, theme: &'static theme::Theme) -> RenderOutcome {
    render_with_quality(markdown, theme, checks::Quality::Standard, false)
}

/// Render with an explicit theme and quality profile. The profile grades
/// the receipt, never the artifact: the composition audit runs in process
/// over the placed geometry and `quality` only sets how its findings are
/// severity-rated (border runs fail every profile; showcase fails on any
/// finding). The emitted bytes are identical across profiles. `motion`
/// (motion preset batch) bakes the declarative entrance choreography in —
/// CSS keyframes and delay ladders, zero scripts; off, the bytes are the
/// historical static document.
pub fn render_with_quality(
    markdown: &str,
    theme: &'static theme::Theme,
    quality: checks::Quality,
    motion: bool,
) -> RenderOutcome {
    let doc = shell::render(markdown, theme, motion);

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
    if motion {
        checks.push(
            "entrance motion baked in — CSS keyframes with nth-child delay ladders, zero scripts"
                .to_string(),
        );
    }
    if failed.is_empty() && !doc.fences.is_empty() {
        checks.push("all fences parsed and validated clean".to_string());
    }
    let repaired_count: usize = doc
        .fences
        .iter()
        .filter(|f| f.ok)
        .map(|f| f.repairs.len())
        .sum();
    if repaired_count > 0 {
        checks.push(format!(
            "{} route preset{} self-repaired and disclosed (diagrams[].repairs)",
            repaired_count,
            if repaired_count == 1 { "" } else { "s" }
        ));
    }
    let audited: Vec<(&shell::FenceOutcome, usize)> = doc
        .fences
        .iter()
        .filter_map(|f| f.composition.as_ref().map(|c| (f, c.findings(quality))))
        .collect();
    let findings: usize = audited.iter().map(|(_, n)| n).sum();
    if !audited.is_empty() {
        if findings == 0 {
            checks.push(format!(
                "composition audit ({}): clean across {} diagram{}",
                quality.name(),
                audited.len(),
                if audited.len() == 1 { "" } else { "s" }
            ));
        } else {
            checks.push(format!(
                "composition audit ({}): {} finding{} across {} diagram{} (diagrams[].composition)",
                quality.name(),
                findings,
                if findings == 1 { "" } else { "s" },
                audited.len(),
                if audited.len() == 1 { "" } else { "s" }
            ));
        }
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
            let mut v = json!({
                "index": f.index,
                "type": f.kind,
                "title": f.title,
                "facts": f.detail,
            });
            if !f.repairs.is_empty() {
                v["repairs"] = serde_json::to_value(&f.repairs).unwrap_or(Value::Null);
            }
            if !f.views.is_empty() {
                // The viewer tabs this artifact carries, so a caller can
                // verify what is focusable without loading the document.
                v["views"] = serde_json::to_value(&f.views).unwrap_or(Value::Null);
            }
            if let Some(c) = &f.composition {
                v["composition"] = c.report(quality);
            }
            v
        })
        .collect();

    let mut hasher = Sha256::new();
    hasher.update(doc.html.as_bytes());
    let sha256 = format!("{:x}", hasher.finalize());

    let receipt = json!({
        "bytes": doc.html.len(),
        "preset": theme.preset,
        "theme": theme.name,
        "quality": quality.name(),
        "motion": motion,
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
    use super::theme::DARK;

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
    fn dataflow_fence_round_trips_through_the_shell() {
        let md = "# ETL\n\n```archify\n{\"dataflow\":{\"title\":\"Pipeline\",\"stages\":[\n  {\"label\":\"In\"},{\"label\":\"Out\"}],\n \"nodes\":[\n  {\"id\":\"src\",\"type\":\"external\",\"label\":\"Source\",\"stage\":0,\"row\":0},\n  {\"id\":\"sink\",\"type\":\"database\",\"label\":\"Store\",\"stage\":1,\"row\":1}],\n \"flows\":[{\"from\":\"src\",\"to\":\"sink\",\"label\":\"rows\"}]}}\n```\n";
        let a = render(md);
        let b = render(md);
        assert_eq!(a.html, b.html, "same input, same bytes");
        assert_eq!(a.receipt["diagnostics"].as_array().unwrap().len(), 0);
        assert_eq!(a.receipt["diagrams"][0]["type"], "dataflow");
        let facts = a.receipt["diagrams"][0]["facts"].as_array().unwrap();
        assert!(facts.iter().any(|f| f.as_str().unwrap().contains("stages=2")));
        assert!(facts.iter().any(|f| f.as_str().unwrap().contains("flows=1")));
        assert!(facts.iter().any(|f| f.as_str().unwrap().starts_with("viewBox=")));
        // Structural assertions on the artifact itself.
        assert!(a.html.contains("data-diagram-type=\"dataflow\""));
        assert!(a.html.contains("data-stage=\"0\""));
        assert!(a.html.contains("data-node-id=\"sink\""));
        assert!(a.html.contains("data-from=\"src\" data-to=\"sink\""));
    }

    #[test]
    fn lifecycle_fence_round_trips_through_the_shell() {
        let md = "# Orders\n\n```archify\n{\"lifecycle\":{\"title\":\"Order\",\"lanes\":[\n  {\"id\":\"main\",\"label\":\"Fulfillment\"}],\n \"states\":[\n  {\"id\":\"new\",\"type\":\"start\",\"label\":\"New\",\"lane\":\"main\",\"col\":0},\n  {\"id\":\"done\",\"type\":\"success\",\"label\":\"Done\",\"lane\":\"main\",\"col\":2}],\n \"transitions\":[{\"from\":\"new\",\"to\":\"done\",\"label\":\"ship\"}]}}\n```\n";
        let a = render(md);
        let b = render(md);
        assert_eq!(a.html, b.html, "same input, same bytes");
        assert_eq!(a.receipt["diagnostics"].as_array().unwrap().len(), 0);
        assert_eq!(a.receipt["diagrams"][0]["type"], "lifecycle");
        let facts = a.receipt["diagrams"][0]["facts"].as_array().unwrap();
        assert!(facts.iter().any(|f| f.as_str().unwrap().contains("states=2")));
        assert!(facts
            .iter()
            .any(|f| f.as_str().unwrap().contains("transitions=1")));
        assert!(facts.iter().any(|f| f.as_str().unwrap().starts_with("viewBox=")));
        // Structural assertions on the artifact itself.
        assert!(a.html.contains("data-diagram-type=\"lifecycle\""));
        assert!(a.html.contains("data-band=\"phase\""));
        assert!(a.html.contains("01 / Fulfillment"));
        assert!(a.html.contains("data-node-id=\"done\""));
        assert!(a.html.contains("data-from=\"new\" data-to=\"done\""));
    }

    #[test]
    fn architecture_fence_round_trips_through_the_shell() {
        let md = "# Site\n\n```archify\n{\"architecture\":{\"title\":\"Site\",\"components\":[\n  {\"id\":\"web\",\"type\":\"frontend\",\"label\":\"Web\",\"row\":0,\"col\":0},\n  {\"id\":\"api\",\"type\":\"backend\",\"label\":\"API\",\"row\":1,\"col\":1}],\n \"boundaries\":[{\"kind\":\"region\",\"label\":\"VPC\",\"wraps\":[\"api\"]}],\n \"connections\":[{\"from\":\"web\",\"to\":\"api\",\"label\":\"https\"}]}}\n```\n";
        let a = render(md);
        let b = render(md);
        assert_eq!(a.html, b.html, "same input, same bytes");
        assert_eq!(a.receipt["diagnostics"].as_array().unwrap().len(), 0);
        assert_eq!(a.receipt["diagrams"][0]["type"], "architecture");
        let facts = a.receipt["diagrams"][0]["facts"].as_array().unwrap();
        assert!(facts
            .iter()
            .any(|f| f.as_str().unwrap().contains("components=2")));
        assert!(facts
            .iter()
            .any(|f| f.as_str().unwrap().contains("boundaries=1")));
        assert!(facts
            .iter()
            .any(|f| f.as_str().unwrap().contains("connections=1")));
        assert!(facts.iter().any(|f| f.as_str().unwrap().starts_with("viewBox=")));
        // Structural assertions on the artifact itself.
        assert!(a.html.contains("data-diagram-type=\"architecture\""));
        assert!(a.html.contains("data-boundary-label=\"VPC\""));
        assert!(a.html.contains("data-node-id=\"api\""));
        assert!(a.html.contains("data-from=\"web\" data-to=\"api\""));
    }

    #[test]
    fn receipt_lists_guided_views() {
        let md = "# T\n\n```archify\n{\"sequence\":{\"title\":\"Ping\",\"participants\":[{\"id\":\"a\",\"type\":\"frontend\",\"label\":\"A\"},{\"id\":\"b\",\"type\":\"backend\",\"label\":\"B\"}],\"messages\":[{\"from\":\"a\",\"to\":\"b\",\"label\":\"ping\"}]},\"views\":[{\"id\":\"v\",\"label\":\"Both\",\"nodes\":[\"a\",\"b\"]}]}\n```\n";
        let r = render(md);
        assert_eq!(r.receipt["diagnostics"].as_array().unwrap().len(), 0);
        let views = r.receipt["diagrams"][0]["views"].as_array().unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0]["id"], "v");
        assert_eq!(views[0]["label"], "Both");
        assert_eq!(views[0]["nodes"][1], "b");
        // The artifact carries the same tabs (id + label in the strip).
        assert!(r
            .html
            .contains("data-view-id=\"v\" aria-selected=\"false\">Both</button>"));
        // Without views the receipt key stays absent.
        let plain = render("# T\n\n```archify\n{\"sequence\":{\"title\":\"Ping\",\"participants\":[{\"id\":\"a\",\"type\":\"frontend\",\"label\":\"A\"},{\"id\":\"b\",\"type\":\"backend\",\"label\":\"B\"}],\"messages\":[{\"from\":\"a\",\"to\":\"b\",\"label\":\"ping\"}]}}\n```\n");
        assert!(plain.receipt["diagrams"][0].get("views").is_none());
    }

    #[test]
    fn motion_round_trips_the_receipt_and_changes_the_bytes() {
        let statik = render(DOC);
        let moved = render_with_quality(DOC, &theme::LIGHT, checks::Quality::Standard, true);
        // The receipt names the axis so a cached artifact is never mistaken
        // for the other posture.
        assert_eq!(statik.receipt["motion"], false);
        assert_eq!(moved.receipt["motion"], true);
        assert_ne!(statik.html, moved.html);
        assert_eq!(
            moved.html,
            render_with_quality(DOC, &theme::LIGHT, checks::Quality::Standard, true).html
        );
        assert!(moved.html.contains("<div class=\"agx-motion\">"));
        assert!(moved.html.contains("@keyframes agx-rise"));
        assert!(moved
            .receipt["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c.as_str().unwrap_or("").contains("entrance motion baked in")));
        assert!(!statik.html.contains("agx-motion"));
    }

    #[test]
    fn theme_swaps_the_artifact_deterministically() {
        let light = render(DOC);
        let dark = render_with_theme(DOC, &DARK);
        // Same input, different theme: different bytes, both deterministic.
        assert_ne!(light.html, dark.html);
        assert_eq!(dark.html, render_with_theme(DOC, &DARK).html);
        assert_eq!(light.receipt["theme"], "light");
        assert_eq!(dark.receipt["theme"], "dark");
        // The shell carries the theme as provenance and bakes its colors.
        assert!(dark.html.contains("<html data-theme=\"dark\" data-preset=\"classic\">"));
        assert!(dark.html.contains("background:#09090b"));
        assert!(light.html.contains("<html data-theme=\"light\" data-preset=\"classic\">"));
        assert!(light.html.contains("background:#ffffff"));
        // Diagram ink follows the theme, not just the prose shell.
        assert!(dark.html.contains("#f4f4f5"), "dark ink must appear in SVG");
        // Geometry is theme-invariant: same viewBox facts either way.
        let vb = |r: &RenderOutcome| {
            r.receipt["diagrams"][0]["facts"]
                .as_array()
                .unwrap()
                .iter()
                .find_map(|f| f.as_str().and_then(|s| s.strip_prefix("viewBox=")))
                .unwrap()
                .to_string()
        };
        assert_eq!(vb(&light), vb(&dark));
    }

    #[test]
    fn preset_swaps_the_palette_orthogonally_to_theme() {
        use super::theme;
        let classic = render_with_theme(DOC, &theme::DARK);
        let flow = render_with_theme(DOC, &theme::SIGNAL_FLOW_DARK);
        // Same mode, different preset family: different bytes, deterministic.
        assert_ne!(classic.html, flow.html);
        assert_eq!(flow.html, render_with_theme(DOC, &theme::SIGNAL_FLOW_DARK).html);
        assert_eq!(classic.receipt["preset"], "classic");
        assert_eq!(flow.receipt["preset"], "signal-flow");
        assert_eq!(flow.receipt["theme"], "dark");
        assert!(flow.html.contains("<html data-theme=\"dark\" data-preset=\"signal-flow\">"));
        // The preset palette really flows into the SVG.
        assert!(flow.html.contains(theme::SIGNAL_FLOW_DARK.ink));
        assert!(!flow.html.contains(theme::DARK.ink));
        // And every node now carries a semantic sigil stamp.
        assert!(flow.html.contains("data-sigil=\""));
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
