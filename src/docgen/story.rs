//! The flow story layer (motion batch 2): the diagram itself becomes the
//! animation, not just the page around it. Each fence carries a storyboard
//! — caption beats derived from its own nodes or messages — and the shell
//! bakes two artifacts from it at generation time:
//!
//! * per-element CSS animation rules, addressed through the same data
//!   attributes the viewer already hooks (`data-node-id`, `data-from`…), so
//!   the adapters' svg bytes never move;
//! * a timed caption strip under the figure — subtitles are page content,
//!   per the standing direction.
//!
//! One clock drives everything: a beat's node lands as its caption shows,
//! its outgoing edges draw just after. Narration rides the same clock later
//! — the receipt carries the beat times, so muxing voice (the only
//! sanctioned ffmpeg step) stays mechanical.
//!
//! Every keyframe declares `from` only (neutral `to`), so the animation
//! fills back to the element's underlying value — after it ends, the
//! viewer's `opacity` presentation attribute stays authoritative and focus
//! dimming keeps working on motion artifacts.

use std::collections::HashMap;

use super::spec::{
    ArchitectureSpec, DataflowSpec, LifecycleSpec, SequenceSpec, WorkflowSpec, text_units,
};

/// The story clock starts once the figure's entrance grow has settled.
const T0: f32 = 0.90;
/// A beat's outgoing edges start drawing after its node has landed.
const EDGE_OFFSET: f32 = 0.35;
/// Arrowheads and edge labels ride slightly behind the drawing line.
const EDGE_TAIL: f32 = 0.20;
/// The draw keyframe's dash budget: any path in a v1 diagram is shorter, so
/// one keyframe serves every edge (short paths just finish drawing early).
const DRAW_DASH: u32 = 4000;

pub struct Beat {
    pub id: String,
    pub caption: String,
    /// Seconds after page load when the beat lands (its node pops, its
    /// caption shows). Two-decimal by construction.
    pub at: f32,
    /// How long the caption stays — narration-paced.
    pub dur: f32,
}

pub struct StoryEdge {
    pub from: String,
    pub to: String,
    /// Dashed variants (dashed/return) fade in: a dasharray override would
    /// erase their authored pattern.
    pub dashed: bool,
    pub at: f32,
}

pub enum StoryFamily {
    /// workflow/dataflow/lifecycle/architecture: node beats plus per-edge
    /// draw rules over the flat `path[data-from]` emission.
    Grid,
    /// sequence: participants fade in on a quick ladder, then each message
    /// group animates at its beat time (messages ARE the beats).
    Sequence { participants: Vec<String> },
}

pub struct Story {
    pub beats: Vec<Beat>,
    pub edges: Vec<StoryEdge>,
    pub family: StoryFamily,
    /// Total story time, seconds — the mux step's timeline length.
    pub duration: f32,
}

/// Narration pace: comfortable Chinese reads ~4-4.5 chars/s (a fullwidth
/// char is 2 text units → ~9 units/s), plus a lead-in. Clamped so a terse
/// label still breathes and a wall of text cannot stall the flow; quantized
/// to 0.05s so beat times stay two-decimal under accumulation.
fn beat_duration(caption: &str) -> f32 {
    let d = (text_units(caption) as f32 / 9.0 + 0.8).clamp(1.3, 3.4);
    (d * 20.0).round() / 20.0
}

/// Caption text: label plus sublabel, the narration line for the beat.
fn caption(label: &str, sub: Option<&str>) -> String {
    match sub.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => format!("{}，{}", label.trim(), s),
        None => label.trim().to_string(),
    }
}

/// The shared grid-family timing: nodes land on narration-paced beats,
/// edges fire from their source beat.
fn grid_story(nodes: Vec<(String, String)>, edges: Vec<(String, String, bool)>) -> Story {
    let mut at = T0;
    let mut beats = Vec::with_capacity(nodes.len());
    for (id, cap) in nodes {
        let dur = beat_duration(&cap);
        beats.push(Beat { id, caption: cap, at, dur });
        at += dur;
    }
    let at_of = |from: &str| {
        beats
            .iter()
            .find(|b| b.id == from)
            .map_or(T0, |b| b.at)
    };
    let edges = edges
        .into_iter()
        .map(|(from, to, dashed)| StoryEdge { at: at_of(&from) + EDGE_OFFSET, from, to, dashed })
        .collect();
    Story { beats, edges, family: StoryFamily::Grid, duration: at + 0.5 }
}

pub fn workflow(spec: &WorkflowSpec) -> Story {
    let lane_index: HashMap<&str, usize> = spec
        .lanes
        .iter()
        .enumerate()
        .map(|(i, l)| (l.id.as_str(), i))
        .collect();
    // Story order reads left to right across columns, top lane first — the
    // narrative direction, deliberately coarser than the adapter's canonical
    // (lane, col) paint order.
    let mut nodes: Vec<_> = spec.nodes.iter().collect();
    nodes.sort_by(|a, b| {
        a.col.cmp(&b.col)
            .then(lane_index.get(a.lane.as_str()).cmp(&lane_index.get(b.lane.as_str())))
            .then(a.id.cmp(&b.id))
    });
    let beats_in = nodes
        .into_iter()
        .map(|n| (n.id.clone(), caption(&n.label, n.sublabel.as_deref())))
        .collect();
    let edges = spec
        .edges
        .iter()
        .map(|e| {
            (
                e.from.clone(),
                e.to.clone(),
                matches!(e.variant.as_deref(), Some("dashed") | Some("return")),
            )
        })
        .collect();
    grid_story(beats_in, edges)
}

pub fn dataflow(spec: &DataflowSpec) -> Story {
    let mut nodes: Vec<_> = spec.nodes.iter().collect();
    nodes.sort_by(|a, b| a.stage.cmp(&b.stage).then(a.row.cmp(&b.row)).then(a.id.cmp(&b.id)));
    let beats_in = nodes
        .into_iter()
        .map(|n| (n.id.clone(), caption(&n.label, n.sublabel.as_deref())))
        .collect();
    let edges = spec
        .flows
        .iter()
        .map(|f| {
            (
                f.from.clone(),
                f.to.clone(),
                matches!(f.variant.as_deref(), Some("dashed") | Some("return")),
            )
        })
        .collect();
    grid_story(beats_in, edges)
}

pub fn lifecycle(spec: &LifecycleSpec) -> Story {
    let lane_index: HashMap<&str, usize> = spec
        .lanes
        .iter()
        .enumerate()
        .map(|(i, l)| (l.id.as_str(), i))
        .collect();
    let mut nodes: Vec<_> = spec.states.iter().collect();
    nodes.sort_by(|a, b| {
        a.col.cmp(&b.col)
            .then(lane_index.get(a.lane.as_str()).cmp(&lane_index.get(b.lane.as_str())))
            .then(a.id.cmp(&b.id))
    });
    let beats_in = nodes
        .into_iter()
        .map(|s| (s.id.clone(), caption(&s.label, s.sublabel.as_deref())))
        .collect();
    let edges = spec
        .transitions
        .iter()
        .map(|t| {
            (
                t.from.clone(),
                t.to.clone(),
                matches!(t.variant.as_deref(), Some("dashed") | Some("return")),
            )
        })
        .collect();
    grid_story(beats_in, edges)
}

pub fn architecture(spec: &ArchitectureSpec) -> Story {
    let mut nodes: Vec<_> = spec.components.iter().collect();
    nodes.sort_by(|a, b| a.row.cmp(&b.row).then(a.col.cmp(&b.col)).then(a.id.cmp(&b.id)));
    let beats_in = nodes
        .into_iter()
        .map(|c| (c.id.clone(), caption(&c.label, c.sublabel.as_deref())))
        .collect();
    let edges = spec
        .connections
        .iter()
        .map(|c| {
            (
                c.from.clone(),
                c.to.clone(),
                matches!(c.variant.as_deref(), Some("dashed") | Some("return")),
            )
        })
        .collect();
    grid_story(beats_in, edges)
}

pub fn sequence(spec: &SequenceSpec) -> Story {
    let label_of = |id: &str| {
        spec.participants
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.label.trim().to_string())
            .unwrap_or_else(|| id.to_string())
    };
    let mut at = T0;
    let mut beats = Vec::with_capacity(spec.messages.len());
    for (i, m) in spec.messages.iter().enumerate() {
        let cap = if m.label.trim().is_empty() {
            format!("{} → {}", label_of(&m.from), label_of(&m.to))
        } else {
            format!(
                "{} → {}：{}",
                label_of(&m.from),
                label_of(&m.to),
                m.label.trim()
            )
        };
        let dur = beat_duration(&cap);
        beats.push(Beat { id: i.to_string(), caption: cap, at, dur });
        at += dur;
    }
    Story {
        beats,
        edges: Vec::new(),
        family: StoryFamily::Sequence {
            participants: spec.participants.iter().map(|p| p.id.clone()).collect(),
        },
        duration: at + 0.5,
    }
}

/// Attribute-selector-safe ids: validated specs already guarantee this shape
/// ([A-Za-z][A-Za-z0-9_-]*); the guard keeps emission robust even if a future
/// family loosens validation — an unsafe id simply doesn't animate.
fn css_id_ok(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

const EASE_BACK: &str = "cubic-bezier(.34,1.56,.64,1)";
const EASE_OUT: &str = "cubic-bezier(.33,1,.68,1)";

/// The per-fence CSS: scoped to `figure[data-diagram-index]` so multiple
/// diagrams in one document never collide. Keyframes live in the motion
/// stylesheet (motion.rs); these rules only place elements on the clock.
pub fn story_css(index: usize, story: &Story) -> String {
    let fig = format!("figure[data-diagram-index=\"{index}\"]");
    let mut css = String::with_capacity(512);
    let s2 = |x: f32| format!("{x:.2}");
    match &story.family {
        StoryFamily::Grid => {
            for b in &story.beats {
                if !css_id_ok(&b.id) {
                    continue;
                }
                css.push_str(&format!(
                    "{fig} [data-node-id=\"{id}\"]{{animation:agx-node .55s {EASE_BACK} both;animation-delay:{at}s}}",
                    id = b.id,
                    at = s2(b.at)
                ));
            }
        }
        StoryFamily::Sequence { participants } => {
            // Participants are the stage, not the story: a quick fade ladder
            // so the heads and lifelines are up before the first message.
            for (i, id) in participants.iter().enumerate() {
                if !css_id_ok(id) {
                    continue;
                }
                css.push_str(&format!(
                    "{fig} [data-participant-id=\"{id}\"]{{animation:agx-in .4s both;animation-delay:{at}s}}",
                    at = s2(T0 + i as f32 * 0.12)
                ));
            }
            for (k, b) in story.beats.iter().enumerate() {
                css.push_str(&format!(
                    "{fig} g[data-message-index=\"{k}\"]{{animation:agx-in .45s both;animation-delay:{at}s}}",
                    at = s2(b.at)
                ));
            }
        }
    }
    for e in &story.edges {
        if !(css_id_ok(&e.from) && css_id_ok(&e.to)) {
            continue;
        }
        if e.dashed {
            css.push_str(&format!(
                "{fig} path[data-from=\"{f}\"][data-to=\"{t}\"]{{animation:agx-in .5s both;animation-delay:{at}s}}",
                f = e.from,
                t = e.to,
                at = s2(e.at)
            ));
        } else {
            // Draw-in: the dash budget rides the keyframe's `from`; the
            // declared dasharray keeps the solid path one unbroken dash
            // while the offset drains.
            css.push_str(&format!(
                "{fig} path[data-from=\"{f}\"][data-to=\"{t}\"]{{stroke-dasharray:{DRAW_DASH};animation:agx-draw .6s {EASE_OUT} both;animation-delay:{at}s}}",
                f = e.from,
                t = e.to,
                at = s2(e.at)
            ));
        }
        // The explicit arrowhead follows the edge path as a sibling; edge
        // labels are grouped elements carrying the same pair. Both land just
        // behind the drawing line so an arrow never floats unattached.
        css.push_str(&format!(
            "{fig} path[data-from=\"{f}\"][data-to=\"{t}\"]+path,{fig} g[data-from=\"{f}\"][data-to=\"{t}\"]{{animation:agx-in .45s both;animation-delay:{at}s}}",
            f = e.from,
            t = e.to,
            at = s2(e.at + EDGE_TAIL)
        ));
    }
    css
}

/// The caption strip under the figure: one span per beat, timed by inline
/// longhands (name/fill/mode live in the motion stylesheet). Under
/// prefers-reduced-motion the strip becomes a static transcript.
pub fn captions_html(story: &Story) -> String {
    let mut s = String::from("<div class=\"agx-caps\">");
    for b in &story.beats {
        s.push_str(&format!(
            "<span class=\"agx-cap\" style=\"animation-duration:{dur:.2}s;animation-delay:{at:.2}s\">{cap}</span>",
            dur = b.dur,
            at = b.at,
            cap = super::shell::html_escape(&b.caption)
        ));
    }
    s.push_str("</div>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflow_spec() -> WorkflowSpec {
        serde_json::from_str(
            r#"{"title":"订单履约","lanes":[
                {"id":"shop","label":"店铺"},
                {"id":"wms","label":"仓配"}],
               "nodes":[
                {"id":"order","lane":"shop","col":0,"label":"订单接收","sublabel":"分钟级下单","type":"frontend"},
                {"id":"risk","lane":"shop","col":1,"label":"风控校验","type":"security"},
                {"id":"split","lane":"wms","col":2,"label":"波次拆分","type":"backend"},
                {"id":"ship","lane":"wms","col":4,"label":"干线发运","type":"cloud"}],
               "edges":[
                {"from":"order","to":"risk","label":"下单"},
                {"from":"risk","to":"split","label":"放行"},
                {"from":"ship","to":"order","label":"回传单号","variant":"return"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn workflow_beats_read_left_to_right_with_narration_pacing() {
        let st = workflow(&workflow_spec());
        let ids: Vec<&str> = st.beats.iter().map(|b| b.id.as_str()).collect();
        // Column order regardless of lane/document order.
        assert_eq!(ids, vec!["order", "risk", "split", "ship"]);
        assert_eq!(st.beats[0].caption, "订单接收，分钟级下单");
        assert_eq!(st.beats[1].caption, "风控校验");
        // Times are monotone, two-decimal, narration-paced.
        assert_eq!(st.beats[0].at, 0.90);
        assert!(st.beats[1].at > st.beats[0].at);
        for w in st.beats.windows(2) {
            assert_eq!(w[1].at, w[0].at + w[0].dur);
        }
        assert_eq!(st.duration, st.beats.last().unwrap().at + st.beats.last().unwrap().dur + 0.5);
    }

    #[test]
    fn edges_fire_from_their_source_beat_and_dashed_ones_fade() {
        let st = workflow(&workflow_spec());
        let ship_at = st.beats.iter().find(|b| b.id == "ship").unwrap().at;
        let ret = st.edges.iter().find(|e| e.from == "ship").unwrap();
        assert!(ret.dashed, "return variant must fade, not draw");
        assert_eq!(ret.at, ship_at + 0.35);
        let fwd = st.edges.iter().find(|e| e.from == "order").unwrap();
        assert!(!fwd.dashed);
        assert_eq!(fwd.at, st.beats[0].at + 0.35);
    }

    #[test]
    fn css_scopes_rules_per_figure_and_per_element() {
        let st = workflow(&workflow_spec());
        let css = story_css(2, &st);
        assert!(css.contains("figure[data-diagram-index=\"2\"] [data-node-id=\"order\"]"));
        assert!(css.contains("animation:agx-node .55s cubic-bezier(.34,1.56,.64,1) both"));
        // Solid edge draws; its arrowhead and label group trail it.
        assert!(css.contains(
            "path[data-from=\"order\"][data-to=\"risk\"]{stroke-dasharray:4000;animation:agx-draw"
        ));
        assert!(css.contains("path[data-from=\"order\"][data-to=\"risk\"]+path,"));
        assert!(css.contains("g[data-from=\"order\"][data-to=\"risk\"]{animation:agx-in .45s both"));
        // The dashed return edge never takes the dasharray override.
        let ret_rule = css
            .split("path[data-from=\"ship\"][data-to=\"order\"]{")
            .nth(1)
            .unwrap()
            .split('}')
            .next()
            .unwrap();
        assert!(ret_rule.contains("agx-in"), "{ret_rule}");
        assert!(!ret_rule.contains("dasharray"), "{ret_rule}");
    }

    #[test]
    fn sequence_story_beats_are_messages_and_participants_ladder() {
        let spec: SequenceSpec = serde_json::from_str(
            r#"{"title":"Ping","participants":[
                {"id":"a","type":"frontend","label":"Client"},
                {"id":"b","type":"backend","label":"Server"}],
               "messages":[{"from":"a","to":"b","label":"ping"}]}"#,
        )
        .unwrap();
        let st = sequence(&spec);
        assert_eq!(st.beats.len(), 1);
        assert_eq!(st.beats[0].caption, "Client → Server：ping");
        assert!(st.edges.is_empty());
        let css = story_css(0, &st);
        assert!(css.contains("[data-participant-id=\"a\"]{animation:agx-in .4s both;animation-delay:0.90s}"));
        assert!(css.contains("[data-participant-id=\"b\"]{animation:agx-in .4s both;animation-delay:1.02s}"));
        assert!(css.contains("g[data-message-index=\"0\"]{animation:agx-in .45s both;animation-delay:0.90s}"));
    }

    #[test]
    fn caption_strip_carries_inline_timing_and_escapes() {
        let spec = workflow_spec();
        let st = workflow(&spec);
        let html = captions_html(&st);
        assert!(html.starts_with("<div class=\"agx-caps\">"));
        assert!(html.contains(
            "style=\"animation-duration:3.00s;animation-delay:0.90s\">订单接收，分钟级下单</span>"
        ), "{html}");
        assert!(html.ends_with("</span></div>"));
        // Markup in a caption can't break out of the span.
        let hostile: SequenceSpec = serde_json::from_str(
            r#"{"title":"X","participants":[
                {"id":"a","type":"frontend","label":"<b>"},
                {"id":"b","type":"backend","label":"B"}],
               "messages":[{"from":"a","to":"b","label":"<i>hi</i>"}]}"#,
        )
        .unwrap();
        let h = captions_html(&sequence(&hostile));
        assert!(h.contains("&lt;b&gt; → B：&lt;i&gt;hi&lt;/i&gt;"), "{h}");
        assert!(!h.contains("<b>"));
    }

    #[test]
    fn hostile_ids_are_skipped_not_injected() {
        // Validated specs cannot produce these; if one ever slips through,
        // the rule is dropped rather than injected into the selector.
        let st = grid_story(
            vec![("x\"]{}".to_string(), "cap".to_string())],
            vec![("ok1".to_string(), "ok2".to_string(), false)],
        );
        let css = story_css(0, &st);
        assert!(!css.contains("[data-node-id=\"x"));
        // Clean endpoints still animate — an unknown source falls back to T0.
        assert!(css.contains("[data-from=\"ok1\"][data-to=\"ok2\"]"));
    }
}
