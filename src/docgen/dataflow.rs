//! Dataflow family adapter: the stage pipeline over a fixed (stage, row)
//! lattice.
//!
//! Stages (2..=5) are vertical composition frames at a fixed pitch; nodes
//! are (stage, row) cells with yOffset as the one stacking escape hatch;
//! flows connect them. Layout arithmetic is archify's dataflow renderer
//! (MIT) recast in integer tenths; routing goes through the shared graph
//! engine with per-edge local corridors. A fixed grid has no gap to widen,
//! so router feedback is terminal here — a diagnostic, never a loop — and
//! an infeasible authored preset walks the family's substitution ladder
//! first (verified substitutes only, every substitution disclosed).
//!
//! v1 divergences from the reference (kept deliberately): no authored node
//! sizes, no authored meta.viewBox (the canvas is measured), no port
//! fan-out spreading, and flow labels fit by width formula instead of
//! width-overflow diagnostics.

use std::collections::HashMap;

use super::graph::{
    label_point, label_rect, plan_grid_edge, polyline_d, Corridors, PlacedRoute, Pt, Rect,
    RouteError, RouteKind, RouteRepair, RouteRequest, RouteScene, Side, COLUMN_COUNT, PX,
};
use super::spec::{text_units, DataflowFlow, DataflowNode, DataflowSpec};
use super::theme::Theme;
use super::tx;

// ---------------------------------------------------------------------------
// constants (archify px × 10)
// ---------------------------------------------------------------------------

/// Frame top; stage titles ride at y=68 inside it.
const STAGE_Y: i32 = 460;
const STAGE_TITLE_Y: i32 = 680;
/// Frames span stageX ± 84px, rounded 10px.
const STAGE_FRAME_PAD: i32 = 840;
const STAGE_FRAME_RX: i32 = 100;
const LEFT_X: i32 = 1000;
const COL_GAP: i32 = 2150;
const NODE_W: i32 = 1120;
const NODE_H: i32 = 580;
const ROW_YS: [i32; 5] = [1280, 2420, 3560, 4700, 5840];
/// Frame bottom = deepest node bottom + 40px; the legend rides 26px lower.
const FRAME_BOTTOM_PAD: i32 = 400;
const LEGEND_DROP: i32 = 260;
const LEGEND_BASELINE_PAD: i32 = 200;
const CANVAS_MARGIN: i32 = 160;
/// Per-edge local corridors — the reference's channel arithmetic: 24px above
/// the higher endpoint, 26px below the deeper bottom, 20px outside each side.
const TOP_CLEAR: i32 = 240;
const BOTTOM_CLEAR: i32 = 260;
const SIDE_CLEAR: i32 = 200;
/// Flow label: w = max(34, units·4.9 + 12) px; a classification adds a line.
const LABEL_WIDTH_FACTOR: i32 = 49;
const LABEL_WIDTH_PAD: i32 = 120;
const LABEL_WIDTH_MIN: i32 = 340;
/// Fonts (tenths): node label 10/8, sublabel 7/6, tag 7/6; flow label 8,
/// classification 6. 0.6px advance per text unit per px of font size.
const LABEL_PREFERRED: i32 = 100;
const LABEL_MIN: i32 = 80;
const SUB_PREFERRED: i32 = 70;
const SUB_MIN: i32 = 60;
const TEXT_WIDTH_FACTOR: i32 = 6;
/// Legend: 20px line swatch, 3px to the text, 24px between items.
const LEGEND_SWATCH_W: i32 = 200;
const LEGEND_SWATCH_TEXT_GAP: i32 = 30;
const LEGEND_ITEM_GAP: i32 = 240;
const LEGEND_X: i32 = 160;

/// Legend catalog: flow variants present, in variant order.
const LEGEND_CATALOG: [(&str, &str); 4] = [
    ("default", "Default"),
    ("emphasis", "Emphasis"),
    ("security", "Security"),
    ("dashed", "Dashed"),
];

#[derive(Debug)]
pub struct RenderedDataflow {
    pub svg: String,
    pub title: String,
    /// ViewBox in px.
    pub view_box: [i32; 2],
    pub stages: usize,
    pub nodes: usize,
    pub flows: usize,
    /// Route presets the engine substituted, in canonical flow order.
    pub repairs: Vec<RouteRepair>,
}

/// The canonicalized document: nodes sort (stage, row, yOffset, id), flows
/// sort (id, from, to, label); stages keep document order — array index IS
/// the stage number.
struct Canon<'a> {
    title: &'a str,
    stages: &'a [super::spec::DataflowStage],
    nodes: Vec<&'a DataflowNode>,
    flows: Vec<&'a DataflowFlow>,
    node_index: HashMap<&'a str, usize>,
}

fn canon<'a>(spec: &'a DataflowSpec) -> Canon<'a> {
    let mut nodes: Vec<&DataflowNode> = spec.nodes.iter().collect();
    nodes.sort_by(|a, b| {
        a.stage
            .cmp(&b.stage)
            .then(a.row.cmp(&b.row))
            .then(a.y_offset.unwrap_or(0).cmp(&b.y_offset.unwrap_or(0)))
            .then(a.id.cmp(&b.id))
    });
    let mut flows: Vec<&DataflowFlow> = spec.flows.iter().collect();
    flows.sort_by(|a, b| {
        a.id.as_deref()
            .unwrap_or("")
            .cmp(b.id.as_deref().unwrap_or(""))
            .then(a.from.cmp(&b.from))
            .then(a.to.cmp(&b.to))
            .then(a.label.cmp(&b.label))
    });
    let node_index: HashMap<&'a str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    Canon {
        title: spec.title.trim(),
        stages: &spec.stages,
        nodes,
        flows,
        node_index,
    }
}

/// Render a validated dataflow spec. Infeasibility is terminal (no feedback
/// loop on a fixed grid): the flow names its dead end and the caller falls
/// the fence back to a code block.
pub fn render_dataflow(
    spec: &DataflowSpec,
    theme: &'static Theme,
) -> Result<RenderedDataflow, Vec<String>> {
    let doc = canon(spec);
    let laid = layout(&doc);

    let mut placed: Vec<(usize, usize, Vec<Pt>, Option<Rect>)> = Vec::new();
    let mut repairs: Vec<RouteRepair> = Vec::new();
    for flow in &doc.flows {
        let &from_idx = doc
            .node_index
            .get(flow.from.as_str())
            .expect("validated: from resolves");
        let &to_idx = doc
            .node_index
            .get(flow.to.as_str())
            .expect("validated: to resolves");
        let scene_placed: Vec<PlacedRoute> = placed
            .iter()
            .map(|(pf, pt, points, label)| PlacedRoute {
                points: points.clone(),
                label: *label,
                shares_endpoint: *pf == from_idx
                    || *pf == to_idx
                    || *pt == from_idx
                    || *pt == to_idx,
            })
            .collect();
        let scene = RouteScene {
            nodes: &laid.rects,
            obstacles: &[],
            placed: &scene_placed,
            canvas: (laid.content_w, laid.content_h),
        };
        let label_w = flow_label_width(&flow.label, flow.classification.as_deref());
        let (from, to) = (&laid.rects[from_idx], &laid.rects[to_idx]);
        let req = RouteRequest {
            from_idx,
            to_idx,
            from,
            to,
            from_col: doc.nodes[from_idx].stage,
            to_col: doc.nodes[to_idx].stage,
            forward: doc.nodes[to_idx].stage > doc.nodes[from_idx].stage,
            cross_lane: false,
            lane_gap: 200,
            label_width: Some(label_w),
            from_side_authored: flow.from_side.as_deref().and_then(Side::parse),
            to_side_authored: flow.to_side.as_deref().and_then(Side::parse),
            route: match flow.route.as_deref() {
                Some(r) if r != "auto" => RouteKind::Preset(r),
                _ => RouteKind::Auto,
            },
            corridors: Corridors {
                lane_gap_y: (from.cy() + to.cy()) / 2,
                top_y: from.y.min(to.y) - TOP_CLEAR,
                bottom_y: from.bottom().max(to.bottom()) + BOTTOM_CLEAR,
                outside_left_x: from.x.min(to.x) - SIDE_CLEAR,
                outside_right_x: from.right().max(to.right()) + SIDE_CLEAR,
            },
            col_xs: stage_col_xs(),
            primary_ports: None,
        };
        match plan_grid_edge(req, &scene, ladder) {
            Ok((planned, substituted)) => {
                if let Some((requested, sub)) = substituted {
                    repairs.push(RouteRepair {
                        edge: flow_name(flow),
                        requested: requested.to_string(),
                        substituted: sub.to_string(),
                    });
                }
                let label = label_rect(label_w, label_point(&planned.points));
                placed.push((from_idx, to_idx, planned.points, Some(label)));
            }
            Err(RouteError::PresetConflict { preset }) => {
                return Err(vec![format!(
                    "flow \"{}\" requests route \"{preset}\" but no verified substitute fits \
                     — spread its endpoints across stages or rows, or loosen the route",
                    flow_name(flow)
                )])
            }
            Err(_) => {
                return Err(vec![format!(
                    "flow \"{}\" has no feasible route between its endpoints — spread them \
                     across stages or rows, or shorten the label",
                    flow_name(flow)
                )])
            }
        }
    }

    let bounds = measured_bounds(&laid, &placed);
    if bounds.0 < 0 || bounds.1 < 0 {
        return Err(vec![
            "dataflow geometry extends above or left of the viewBox origin — pull the yOffset \
             stack back toward its row"
                .to_string(),
        ]);
    }
    let view_box = [
        laid.content_w.max(bounds.2 + CANVAS_MARGIN),
        laid.content_h.max(bounds.3 + CANVAS_MARGIN),
    ];

    let legend: Vec<usize> = LEGEND_CATALOG
        .iter()
        .enumerate()
        .filter(|(_, (variant, _))| {
            doc.flows
                .iter()
                .any(|f| f.variant.as_deref().unwrap_or("default") == *variant)
        })
        .map(|(i, _)| i)
        .collect();

    let svg = emit_svg(&doc, &laid, &placed, view_box, &legend, theme);
    Ok(RenderedDataflow {
        svg,
        title: doc.title.to_string(),
        view_box: [view_box[0] / 10, view_box[1] / 10],
        stages: doc.stages.len(),
        nodes: doc.nodes.len(),
        flows: doc.flows.len(),
        repairs,
    })
}

/// Semantic substitution ladder for an infeasible preset: keep the flow's
/// reading (a rise stays a channel, a dive stays low) while dropping the
/// exact shape that cannot be honored. First verified substitute wins.
fn ladder(route: &str) -> &'static [&'static str] {
    match route {
        "straight" => &["auto"],
        "vertical-channel" => &["bottom-channel", "top-channel", "auto"],
        "bottom-channel" => &["top-channel"],
        "top-channel" => &["bottom-channel"],
        _ => &[],
    }
}

fn flow_name(flow: &DataflowFlow) -> String {
    flow.id
        .clone()
        .unwrap_or_else(|| format!("{}->{}", flow.from, flow.to))
}

fn flow_label_width(label: &str, classification: Option<&str>) -> i32 {
    let units = text_units(label).max(classification.map_or(0, text_units));
    (units as i32 * LABEL_WIDTH_FACTOR + LABEL_WIDTH_PAD).max(LABEL_WIDTH_MIN)
}

// ---------------------------------------------------------------------------
// fixed-grid layout
// ---------------------------------------------------------------------------

fn stage_x(i: usize) -> i32 {
    LEFT_X + i as i32 * COL_GAP
}

fn stage_col_xs() -> [i32; COLUMN_COUNT] {
    let mut xs = [0i32; COLUMN_COUNT];
    for (i, x) in xs.iter_mut().enumerate() {
        *x = stage_x(i);
    }
    xs
}

struct Laid {
    rects: Vec<Rect>,
    content_w: i32,
    content_h: i32,
    frame_bottom: i32,
    legend_baseline: i32,
}

fn layout(doc: &Canon) -> Laid {
    let rects: Vec<Rect> = doc
        .nodes
        .iter()
        .map(|n| Rect {
            x: stage_x(n.stage as usize) - NODE_W / 2,
            y: ROW_YS[n.row as usize] + n.y_offset.unwrap_or(0) * PX,
            w: NODE_W,
            h: NODE_H,
        })
        .collect();
    let last_stage = doc.stages.len().saturating_sub(1);
    let content_w = stage_x(last_stage) + STAGE_FRAME_PAD + CANVAS_MARGIN;
    let deepest = rects
        .iter()
        .map(|r| r.bottom())
        .max()
        .unwrap_or(STAGE_Y + 4000);
    let frame_bottom = deepest + FRAME_BOTTOM_PAD;
    let legend_baseline = frame_bottom + LEGEND_DROP;
    let content_h = legend_baseline + LEGEND_BASELINE_PAD;
    Laid {
        rects,
        content_w,
        content_h,
        frame_bottom,
        legend_baseline,
    }
}

/// (left, top, right, bottom) over every drawn thing.
fn measured_bounds(
    laid: &Laid,
    placed: &[(usize, usize, Vec<Pt>, Option<Rect>)],
) -> (i32, i32, i32, i32) {
    let mut left = LEFT_X - STAGE_FRAME_PAD;
    let mut top = STAGE_Y;
    let mut right = laid.content_w - CANVAS_MARGIN;
    // The frames are drawn content: seed with their edges, not the canvas
    // floor (the viewBox add of CANVAS_MARGIN would double-count it).
    let mut bottom = laid.frame_bottom;
    let include = |x: i32, y: i32, l: &mut i32, t: &mut i32, r: &mut i32, b: &mut i32| {
        *l = (*l).min(x);
        *t = (*t).min(y);
        *r = (*r).max(x);
        *b = (*b).max(y);
    };
    for rect in &laid.rects {
        include(rect.x, rect.y, &mut left, &mut top, &mut right, &mut bottom);
        include(
            rect.right(),
            rect.bottom(),
            &mut left,
            &mut top,
            &mut right,
            &mut bottom,
        );
    }
    for (_, _, points, label) in placed {
        for &p in points {
            include(p.0, p.1, &mut left, &mut top, &mut right, &mut bottom);
        }
        if let Some(lr) = label {
            include(lr.x, lr.y, &mut left, &mut top, &mut right, &mut bottom);
            include(
                lr.right(),
                lr.bottom(),
                &mut left,
                &mut top,
                &mut right,
                &mut bottom,
            );
        }
    }
    (left, top, right, bottom)
}

// ---------------------------------------------------------------------------
// SVG emission
// ---------------------------------------------------------------------------

/// (stroke, width, dash, label fill) per flow variant — emphasis rides
/// heavier (1.8px vs 1.4px) exactly like the reference. Sizes and dash
/// patterns are this adapter's typography; colors come from the theme.
fn variant_style(theme: &Theme, variant: &str) -> (&'static str, i32, Option<&'static str>, &'static str) {
    match variant {
        "emphasis" => (theme.ink, 18, None, theme.ink),
        "security" => (theme.danger, 14, None, theme.danger),
        "dashed" => (theme.skip, 14, Some("6 4"), theme.skip),
        _ => (theme.ink_soft, 14, None, theme.ink_soft),
    }
}

fn fitted_font(text: &str, available: i32, preferred: i32, minimum: i32) -> i32 {
    let units = text_units(text) as i32;
    let by_width = available * 10 / (units * TEXT_WIDTH_FACTOR);
    preferred.min(by_width).max(minimum)
}

fn esc(s: &str) -> String {
    let flat = s.replace(['\n', '\r', '\t'], " ");
    flat.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn emit_svg(
    doc: &Canon,
    laid: &Laid,
    placed: &[(usize, usize, Vec<Pt>, Option<Rect>)],
    view_box: [i32; 2],
    legend: &[usize],
    theme: &'static Theme,
) -> String {
    let mut s = String::with_capacity(8192);
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {} {}\" role=\"img\" aria-label=\"{}\">",
        tx(view_box[0]),
        tx(view_box[1]),
        esc(doc.title)
    ));

    // Stage frames + titles (behind everything: frames contain their nodes,
    // so flows paint over the frame, never around it).
    for (i, stage) in doc.stages.iter().enumerate() {
        let x = stage_x(i);
        s.push_str(&format!(
            "<rect data-stage=\"{i}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"{}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1\"/>",
            tx(x - STAGE_FRAME_PAD),
            tx(STAGE_Y),
            tx(STAGE_FRAME_PAD * 2),
            tx(laid.frame_bottom - STAGE_Y),
            tx(STAGE_FRAME_RX),
            theme.panel,
            theme.frame
        ));
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"9\" font-weight=\"600\" fill=\"{}\" text-anchor=\"middle\">{:02} / {}</text>",
            tx(x),
            tx(STAGE_TITLE_Y),
            theme.ink_soft,
            i + 1,
            esc(stage.label.trim())
        ));
    }

    // Flow paths + arrowheads.
    for (i, flow) in doc.flows.iter().enumerate() {
        let (_, _, points, _) = &placed[i];
        let (stroke, stroke_w, dash, _) = variant_style(theme, flow.variant.as_deref().unwrap_or("default"));
        let dash_attr = dash.map_or(String::new(), |d| format!(" stroke-dasharray=\"{d}\""));
        s.push_str(&format!(
            "<path data-from=\"{}\" data-to=\"{}\" d=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{}\"{}/>",
            esc(&flow.from),
            esc(&flow.to),
            polyline_d(points),
            stroke,
            tx(stroke_w),
            dash_attr
        ));
        s.push_str(&arrowhead(points, stroke));
    }

    // Nodes.
    for (i, node) in doc.nodes.iter().enumerate() {
        let rect = &laid.rects[i];
        let node_colors = theme.node(&node.kind);
        s.push_str(&format!("<g data-node-id=\"{}\">", esc(&node.id)));
        s.push_str(&format!(
            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"6\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1.5\"/>",
            tx(rect.x),
            tx(rect.y),
            tx(rect.w),
            tx(rect.h),
            node_colors.fill,
            node_colors.stroke
        ));
        let label_font = fitted_font(&node.label, NODE_W - 160, LABEL_PREFERRED, LABEL_MIN);
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"{}\" font-weight=\"600\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
            tx(rect.cx()),
            tx(rect.y + 210),
            tx(label_font),
            theme.ink,
            esc(&node.label)
        ));
        if let Some(sub) = node.sublabel.as_deref().filter(|s| !s.trim().is_empty()) {
            let sub_font = fitted_font(sub, NODE_W, SUB_PREFERRED, SUB_MIN);
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
                tx(rect.cx()),
                tx(rect.y + 370),
                tx(sub_font),
                theme.ink_muted,
                esc(sub)
            ));
        }
        if let Some(tag) = node.tag.as_deref().filter(|t| !t.trim().is_empty()) {
            let tag_font = fitted_font(tag, NODE_W, SUB_PREFERRED, SUB_MIN);
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
                tx(rect.cx()),
                tx(rect.y + NODE_H - 110),
                tx(tag_font),
                node_colors.text,
                esc(tag)
            ));
        }
        s.push_str("</g>");
    }

    // Flow labels (on top: white mask + label, classification beneath).
    for (i, flow) in doc.flows.iter().enumerate() {
        let (_, _, points, _) = &placed[i];
        let at = label_point(points);
        let w = flow_label_width(&flow.label, flow.classification.as_deref());
        let has_class = flow
            .classification
            .as_deref()
            .is_some_and(|c| !c.trim().is_empty());
        let (_, _, _, label_fill) = variant_style(theme, flow.variant.as_deref().unwrap_or("default"));
        let h = if has_class { 300 } else { 160 };
        let class_line = has_class.then(|| {
            format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"6\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
                tx(at.0),
                tx(at.1 + 180),
                theme.ink_muted,
                esc(flow.classification.as_deref().unwrap_or(""))
            )
        });
        s.push_str(&format!(
            "<g data-from=\"{}\" data-to=\"{}\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"3\" fill=\"{}\"/><text x=\"{}\" y=\"{}\" font-size=\"8\" fill=\"{}\" text-anchor=\"middle\">{}</text>{}</g>",
            esc(&flow.from),
            esc(&flow.to),
            tx(at.0 - w / 2),
            tx(at.1 - 110),
            tx(w),
            tx(h),
            theme.panel_alt,
            tx(at.0),
            tx(at.1 + 30),
            label_fill,
            esc(&flow.label),
            class_line.as_deref().unwrap_or("")
        ));
    }

    // Legend: flow variants present, as line swatches.
    if !legend.is_empty() {
        let mut x = LEGEND_X;
        for &idx in legend {
            let (variant, label) = LEGEND_CATALOG[idx];
            let (stroke, width, dash, _) = variant_style(theme, variant);
            let dash_attr = dash.map_or(String::new(), |d| format!(" stroke-dasharray=\"{d}\""));
            s.push_str(&format!(
                "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{}\" stroke-width=\"{}\"{}/><text x=\"{}\" y=\"{}\" font-size=\"7\" fill=\"{}\">{}</text>",
                tx(x),
                tx(laid.legend_baseline - 30),
                tx(x + LEGEND_SWATCH_W),
                tx(laid.legend_baseline - 30),
                stroke,
                tx(width),
                dash_attr,
                tx(x + LEGEND_SWATCH_W + LEGEND_SWATCH_TEXT_GAP),
                tx(laid.legend_baseline),
                theme.ink_soft,
                esc(label)
            ));
            x += LEGEND_SWATCH_W
                + LEGEND_SWATCH_TEXT_GAP
                + text_units(label) as i32 * 62
                + LEGEND_ITEM_GAP;
        }
    }
    s.push_str("</svg>");
    s
}

/// Explicit triangle at the route's end, aimed along its last segment
/// (diting svg v1 draws paths, not marker refs).
fn arrowhead(points: &[Pt], fill: &str) -> String {
    if points.len() < 2 {
        return String::new();
    }
    let tip = points[points.len() - 1];
    let prev = points[points.len() - 2];
    let (dx, dy) = (tip.0 - prev.0, tip.1 - prev.1);
    let len = (dx.abs() + dy.abs()).max(1);
    let (ux, uy) = (dx / len, dy / len);
    // 6px back along the segment, ±2.5px across.
    let base_x = tip.0 - ux * 60;
    let base_y = tip.1 - uy * 60;
    let (px, py) = (-uy, ux);
    format!(
        "<path d=\"M {} {} L {} {} L {} {} Z\" fill=\"{}\"/>",
        tx(tip.0),
        tx(tip.1),
        tx(base_x + px * 25),
        tx(base_y + py * 25),
        tx(base_x - px * 25),
        tx(base_y - py * 25),
        fill
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::theme::LIGHT;

    const SPEC_JSON: &str = r#"{
        "title": "Pipeline",
        "stages": [
            {"label": "Ingest"},
            {"label": "Process"},
            {"label": "Serve"}
        ],
        "nodes": [
            {"id": "edge", "type": "frontend", "label": "Edge", "stage": 0, "row": 0},
            {"id": "queue", "type": "messagebus", "label": "Queue", "stage": 1, "row": 1, "tag": "Kafka"},
            {"id": "worker", "type": "backend", "label": "Worker", "stage": 1, "row": 3},
            {"id": "cache", "type": "database", "label": "Cache", "stage": 2, "row": 2, "sublabel": "redis"}
        ],
        "flows": [
            {"from": "edge", "to": "queue", "label": "events"},
            {"from": "queue", "to": "worker", "label": "consume", "variant": "dashed"},
            {"from": "worker", "to": "cache", "label": "get/set", "variant": "emphasis"}
        ]
    }"#;

    fn spec() -> DataflowSpec {
        let s: DataflowSpec = serde_json::from_str(SPEC_JSON).unwrap();
        assert!(super::super::spec::validate_dataflow(&s).is_empty());
        s
    }

    #[test]
    fn stage_pipeline_renders_deterministically() {
        let a = render_dataflow(&spec(), &LIGHT).unwrap();
        let b = render_dataflow(&spec(), &LIGHT).unwrap();
        assert_eq!(a.svg, b.svg, "same input, same bytes");
        assert_eq!((a.stages, a.nodes, a.flows), (3, 4, 3));
        assert!(a.repairs.is_empty(), "{:?}", a.repairs);
        assert!(a.svg.contains("data-stage=\"0\""));
        assert!(a.svg.contains("01 / Ingest"));
        assert!(a.svg.contains("data-node-id=\"queue\""));
        assert!(a.svg.contains("data-from=\"edge\" data-to=\"queue\""));
        assert!(a.svg.contains("stroke-dasharray=\"6 4\""));
        // 3 stages → 100 + 2*215 + 84 + 16 = 630px wide; rows to 3 → 470 +
        // 58 + 40 + 26 + 20 = 614px tall.
        assert_eq!(a.view_box, [630, 614], "{:?}", a.view_box);
        // Legend lists the variants present (default implied, dashed,
        // emphasis) but not security.
        assert!(a.svg.contains(">Dashed<"));
        assert!(a.svg.contains(">Emphasis<"));
        assert!(!a.svg.contains(">Security<"));
    }

    #[test]
    fn shuffled_collections_render_identical_bytes() {
        let shuffled: DataflowSpec = serde_json::from_str(
            r#"{
                "title": "Pipeline",
                "stages": [
                    {"label": "Ingest"},
                    {"label": "Process"},
                    {"label": "Serve"}
                ],
                "nodes": [
                    {"id": "cache", "type": "database", "label": "Cache", "stage": 2, "row": 2, "sublabel": "redis"},
                    {"id": "worker", "type": "backend", "label": "Worker", "stage": 1, "row": 3},
                    {"id": "queue", "type": "messagebus", "label": "Queue", "stage": 1, "row": 1, "tag": "Kafka"},
                    {"id": "edge", "type": "frontend", "label": "Edge", "stage": 0, "row": 0}
                ],
                "flows": [
                    {"from": "worker", "to": "cache", "label": "get/set", "variant": "emphasis"},
                    {"from": "edge", "to": "queue", "label": "events"},
                    {"from": "queue", "to": "worker", "label": "consume", "variant": "dashed"}
                ]
            }"#,
        )
        .unwrap();
        assert!(super::super::spec::validate_dataflow(&shuffled).is_empty());
        assert_eq!(
            render_dataflow(&spec(), &LIGHT).unwrap().svg,
            render_dataflow(&shuffled, &LIGHT).unwrap().svg
        );
    }

    #[test]
    fn stacked_cell_vertical_channel_is_repaired_and_disclosed() {
        // Two nodes sharing stage 0 row 2, stacked 66px apart (the minimum
        // legal clearance): every vertical-channel shape degenerates to an
        // 8px straight run or a U-turn, so the ladder walks to auto — the
        // sideways dogleg around the stack.
        let s: DataflowSpec = serde_json::from_str(
            r#"{
                "title": "Stack",
                "stages": [{"label": "One"}, {"label": "Two"}],
                "nodes": [
                    {"id": "a", "type": "backend", "label": "A", "stage": 0, "row": 2},
                    {"id": "b", "type": "backend", "label": "B", "stage": 0, "row": 2, "yOffset": 66}
                ],
                "flows": [
                    {"from": "a", "to": "b", "label": "fan out", "route": "vertical-channel"}
                ]
            }"#,
        )
        .unwrap();
        assert!(super::super::spec::validate_dataflow(&s).is_empty());
        let rendered = render_dataflow(&s, &LIGHT).unwrap();
        assert_eq!(rendered.repairs.len(), 1, "{:?}", rendered.repairs);
        assert_eq!(rendered.repairs[0].edge, "a->b");
        assert_eq!(rendered.repairs[0].requested, "vertical-channel");
        assert_eq!(rendered.repairs[0].substituted, "auto");
        // The substitute really is a dogleg, not the degenerate straight.
        let path = rendered
            .svg
            .split("data-from=\"a\" data-to=\"b\"")
            .nth(1)
            .unwrap()
            .split("/>")
            .next()
            .unwrap();
        assert!(path.matches('L').count() >= 2, "route too straight: {path}");
    }

    #[test]
    fn top_channel_blocked_by_a_raised_node_substitutes_bottom_channel() {
        // A middle node pulled up 80px sits in the top channel's path, so
        // the run below the stack takes over — disclosed.
        let s: DataflowSpec = serde_json::from_str(
            r#"{
                "title": "Wide",
                "stages": [{"label": "One"}, {"label": "Two"}, {"label": "Three"}],
                "nodes": [
                    {"id": "a", "type": "backend", "label": "A", "stage": 0, "row": 2},
                    {"id": "m", "type": "database", "label": "M", "stage": 1, "row": 2, "yOffset": -80},
                    {"id": "b", "type": "backend", "label": "B", "stage": 2, "row": 2}
                ],
                "flows": [
                    {"from": "a", "to": "b", "label": "wide", "route": "top-channel"}
                ]
            }"#,
        )
        .unwrap();
        assert!(super::super::spec::validate_dataflow(&s).is_empty());
        let rendered = render_dataflow(&s, &LIGHT).unwrap();
        assert_eq!(rendered.repairs.len(), 1, "{:?}", rendered.repairs);
        assert_eq!(rendered.repairs[0].requested, "top-channel");
        assert_eq!(rendered.repairs[0].substituted, "bottom-channel");
        // The bottom run rides 26px under the row-2 bottoms (414px).
        let path = rendered
            .svg
            .split("data-from=\"a\" data-to=\"b\"")
            .nth(1)
            .unwrap()
            .split("/>")
            .next()
            .unwrap();
        assert!(
            path.contains("L 100 440 L 530 440"),
            "bottom channel run missing: {path}"
        );
    }
}
