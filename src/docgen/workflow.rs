//! Workflow family adapter: swimlane vocabulary feeding the graph engine.
//!
//! The engine (graph.rs) owns geometry; this module owns semantics — lanes,
//! rank columns, phases, groups — and translates them into the constraint,
//! corridor, and scene inputs the router consumes. Layout arithmetic is
//! archify's readable-v2 workflow compiler (MIT) recast in integer tenths:
//! constraint propagation over the column lattice, three post-pass shifts,
//! measured lane geometry, and the bounded feedback loop that re-solves with
//! a wider rank or lane gap when a route cannot fit.
//!
//! v1 divergences from the reference (kept deliberately): no absolute pins
//! (via/channelX/channelY/labelAt), no authored node sizes, no bias, no role
//! exemption, and no label-channel preference — a labeled direct edge always
//! widens its rank gap instead of detouring through a lane-gap channel.

use std::collections::{BTreeMap, HashMap};

use super::graph::{
    anchor, default_sides, label_point, label_rect, plan_route, polyline_d, solve_columns,
    ColConstraint, Feedback, PlacedRoute, Pt, Rect, RouteError, RouteKind, RouteRequest,
    RouteScene, Side, COLUMN_COUNT, PX,
};
use super::spec::{
    text_units, Lane, NodeGroup, Phase, WorkflowEdge, WorkflowNode, WorkflowSpec,
    workflow_node_height,
};
use super::tx;

// ---------------------------------------------------------------------------
// constants (archify px × 10)
// ---------------------------------------------------------------------------

const LANE_X: i32 = 400;
const LANE_Y: i32 = 520;
const LANE_W_MIN: i32 = 6400;
const LANE_TITLE_H: i32 = 300;
const LANE_GAP_BASE: i32 = 200;
/// Node boxes: fixed 92px wide; height comes from `workflow_node_height`.
const NODE_W: i32 = 920;
/// Static rank clearance for a facing direct edge (28px).
const DIRECT_CLEARANCE: i32 = 280;
/// Same-lane column clearance for vertically overlapping nodes (8px).
const PAIR_CLEARANCE: i32 = 80;
/// A labeled direct edge demands room for its label mask, not just 28px.
const LABEL_MASK_PAD: i32 = 80;
/// Font fitting (preferred/minimum, tenths): label 11/9, sublabel 8/6, tag 7/6.
const LABEL_PREFERRED: i32 = 110;
const LABEL_MIN: i32 = 90;
const SUB_PREFERRED: i32 = 80;
const SUB_MIN: i32 = 60;
const TAG_PREFERRED: i32 = 70;
const TAG_MIN: i32 = 60;
/// 0.6px advance per text unit per px of font size.
const TEXT_WIDTH_FACTOR: i32 = 6;
/// Space the fitted label line reserves inside the node box.
const LABEL_RESERVE: i32 = 240;
/// Edge label width: max(30, units·4.8 + 10) px.
fn edge_label_width(label: &str) -> i32 {
    (text_units(label) as i32 * 48 + 100).max(300)
}
/// Phase span: column pad 46px, label minimum units·5.6+8 px.
const PHASE_PAD: i32 = 460;
/// Group frame: column pad 50px, label minimum units·5.6+20 px, node inset 4px.
const GROUP_PAD: i32 = 500;
const GROUP_NODE_INSET: i32 = 40;
const GROUP_FRAME_TOP_INSET: i32 = 80;
const GROUP_FRAME_BOTTOM_INSET: i32 = 40;
const GROUP_LABEL_BASELINE_OFFSET: i32 = -20;
const GROUP_LABEL_X_INSET: i32 = 100;
const GROUP_LABEL_UNITS: i32 = 56;
/// First-rank left inset: extent floor 46px, inset 8px (lane origin is 40px).
const FIRST_EXTENT_FLOOR: i32 = 460;
const LEFT_INSET: i32 = 80;
/// Lane header label: advance 6.2/unit, clearance 2px, width pad 30px.
const LANE_HEADER_ORIGIN: i32 = 540; // laneX 400 + 14px label inset
const LANE_HEADER_UNITS: i32 = 62;
const LANE_HEADER_CLEARANCE: i32 = 20;
const LANE_LABEL_PAD: i32 = 300;
/// Rightmost floor: colXs[last] + 50; laneW adds −laneX+8 on top.
const RIGHTMOST_PAD: i32 = 500;
const RIGHTMOST_INSET: i32 = 80;
/// Content floors for the measured-left shift: 16px for phases, 44px groups.
const CONTENT_FLOOR: i32 = 160;
const GROUP_CONTENT_FLOOR: i32 = 440;
/// Vertical rhythm: base content height max(74, extent·2+8), lane = title+content.
const BASE_CONTENT_MIN: i32 = 740;
const BASE_CONTENT_PAD: i32 = 80;
/// Group reserves: minimum top offset 11px over a label, 9px otherwise; the
/// frame keeps 1px below its nodes.
const GROUP_TOP_OVER_LABEL: i32 = 110;
const GROUP_TOP_CLEAR: i32 = 90;
const GROUP_BOTTOM_MARGIN: i32 = 10;
/// Legend: item gap 7px, swatch 14×9, swatch→text 3px, row 16px, title 48px.
const LEGEND_ITEM_GAP: i32 = 70;
const LEGEND_SWATCH_W: i32 = 140;
const LEGEND_SWATCH_H: i32 = 90;
const LEGEND_SWATCH_TEXT_GAP: i32 = 30;
const LEGEND_ROW_H: i32 = 160;
const LEGEND_TITLE_W: i32 = 480;
const LEGEND_X: i32 = 200;
const LEGEND_Y_PAD: i32 = 440;
const LEGEND_UNITS: i32 = 62;
/// Canvas: lanes margin 40+16 px, bottom auto pad 124px, viewBox margins 16/18.
const CANVAS_RIGHT_PAD: i32 = 160;
const AUTO_HEIGHT_PAD: i32 = 1240;
const VB_RIGHT_MARGIN: i32 = 160;
const VB_BOTTOM_MARGIN: i32 = 180;
/// Bounded feedback rounds (archify: 3 after the initial attempt).
const MAX_FEEDBACK_ROUNDS: usize = 3;

/// Legend catalog order (archify's workflow catalog, not the kind registry).
const LEGEND_CATALOG: [(&str, &str); 7] = [
    ("frontend", "Frontend"),
    ("backend", "Backend"),
    ("security", "Security"),
    ("messagebus", "Message bus"),
    ("database", "Database"),
    ("cloud", "Cloud"),
    ("external", "External"),
];

#[derive(Debug)]
pub struct RenderedWorkflow {
    pub svg: String,
    pub title: String,
    /// ViewBox in px.
    pub view_box: [i32; 2],
    pub lanes: usize,
    pub nodes: usize,
    pub edges: usize,
}

/// The canonicalized document: every collection in its deterministic order.
/// Nodes sort (lane, col, id), edges (id, from, to, label, route), phases
/// (fromCol, toCol, id), groups (lane, fromCol, toCol, id); lanes keep
/// document order — the author's stacking IS the spec.
struct Canon<'a> {
    title: &'a str,
    lanes: &'a [Lane],
    nodes: Vec<&'a WorkflowNode>,
    edges: Vec<&'a WorkflowEdge>,
    phases: Vec<&'a Phase>,
    groups: Vec<&'a NodeGroup>,
    lane_index: HashMap<&'a str, usize>,
    node_index: HashMap<&'a str, usize>,
}

fn canon<'a>(spec: &'a WorkflowSpec) -> Canon<'a> {
    let lane_index: HashMap<&'a str, usize> = spec
        .lanes
        .iter()
        .enumerate()
        .map(|(i, l)| (l.id.as_str(), i))
        .collect();
    let mut nodes: Vec<&WorkflowNode> = spec.nodes.iter().collect();
    nodes.sort_by(|a, b| {
        lane_index
            .get(a.lane.as_str())
            .cmp(&lane_index.get(b.lane.as_str()))
            .then(a.col.cmp(&b.col))
            .then(a.id.cmp(&b.id))
    });
    let mut edges: Vec<&WorkflowEdge> = spec.edges.iter().collect();
    edges.sort_by(|a, b| {
        a.id.as_deref().unwrap_or("")
            .cmp(b.id.as_deref().unwrap_or(""))
            .then(a.from.cmp(&b.from))
            .then(a.to.cmp(&b.to))
            .then(
                a.label
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.label.as_deref().unwrap_or("")),
            )
            .then(a.route.as_deref().unwrap_or("").cmp(b.route.as_deref().unwrap_or("")))
    });
    let mut phases: Vec<&Phase> = spec.phases.iter().collect();
    phases.sort_by(|a, b| {
        a.from_col
            .cmp(&b.from_col)
            .then(a.to_col.cmp(&b.to_col))
            .then(a.id.cmp(&b.id))
    });
    let mut groups: Vec<&NodeGroup> = spec.groups.iter().collect();
    groups.sort_by(|a, b| {
        lane_index
            .get(a.lane.as_str())
            .cmp(&lane_index.get(b.lane.as_str()))
            .then(a.from_col.cmp(&b.from_col))
            .then(a.to_col.cmp(&b.to_col))
            .then(a.id.cmp(&b.id))
    });
    let node_index: HashMap<&'a str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    Canon {
        title: spec.title.trim(),
        lanes: &spec.lanes,
        nodes,
        edges,
        phases,
        groups,
        lane_index,
        node_index,
    }
}

enum CompileError {
    /// The engine asked for a wider gap; re-solve.
    Feedback(Feedback),
    Problems(Vec<String>),
}

/// Render a validated workflow spec. Infeasibility the bounded feedback loop
/// cannot repair comes back as problems; the caller falls the fence back to a
/// code block.
pub fn render_workflow(spec: &WorkflowSpec) -> Result<RenderedWorkflow, Vec<String>> {
    let doc = canon(spec);
    let mut rank_gaps: BTreeMap<(usize, usize), i32> = BTreeMap::new();
    let mut lane_gap_min = 0;
    for round in 0..=MAX_FEEDBACK_ROUNDS {
        match compile_once(&doc, &rank_gaps, lane_gap_min) {
            Ok(done) => return Ok(done),
            Err(CompileError::Feedback(Feedback::RankGap {
                from_col,
                to_col,
                minimum,
            })) => {
                // Accept the request only if it strictly grows the ask;
                // a repeat request means the loop is wedged.
                let entry = rank_gaps.entry((from_col, to_col)).or_insert(i32::MIN);
                if minimum > *entry {
                    *entry = minimum;
                } else {
                    return Err(dead_end(round, &doc));
                }
            }
            Err(CompileError::Feedback(Feedback::LaneGap { minimum })) => {
                if minimum > lane_gap_min {
                    lane_gap_min = minimum;
                } else {
                    return Err(dead_end(round, &doc));
                }
            }
            Err(CompileError::Problems(p)) => return Err(p),
        }
    }
    Err(dead_end(MAX_FEEDBACK_ROUNDS, &doc))
}

fn dead_end(round: usize, doc: &Canon) -> Vec<String> {
    vec![format!(
        "no feasible automatic route after {} feedback round{} across {} edges — \
         spread the offending endpoints across more columns or shorten the longest label",
        round,
        if round == 1 { "" } else { "s" },
        doc.edges.len()
    )]
}

// ---------------------------------------------------------------------------
// one compile attempt
// ---------------------------------------------------------------------------

struct Laid {
    col_xs: [i32; COLUMN_COUNT],
    lane_w: i32,
    lane_tops: Vec<i32>,
    lane_heights: Vec<i32>,
    lane_gap: i32,
    rects: Vec<Rect>,
}

fn compile_once(
    doc: &Canon,
    rank_gaps: &BTreeMap<(usize, usize), i32>,
    lane_gap_min: i32,
) -> Result<RenderedWorkflow, CompileError> {
    let col_xs = solve_columns(&constraints(doc, rank_gaps));
    let col_xs = post_pass_shifts(doc, col_xs);
    let laid = vertical_math(doc, col_xs, lane_gap_min);

    // Legend: kinds present, in catalog order, packed into rows.
    let legend_entries: Vec<(usize, i32)> = LEGEND_CATALOG
        .iter()
        .enumerate()
        .filter(|(_, (kind, _))| doc.nodes.iter().any(|n| n.kind == *kind))
        .map(|(i, (_, label))| (i, text_units(label) as i32))
        .collect();
    let one_row_min = LEGEND_TITLE_W
        + 80
        + legend_entries
            .iter()
            .map(|&(_, units)| LEGEND_SWATCH_W + LEGEND_SWATCH_TEXT_GAP + units * LEGEND_UNITS)
            .sum::<i32>()
        + LEGEND_ITEM_GAP * legend_entries.len().saturating_sub(1) as i32;
    let required_width = LANE_X + laid.lane_w + CANVAS_RIGHT_PAD;
    let minimum_canvas_width = required_width.max(one_row_min + 400);
    let legend_packing_width = minimum_canvas_width - 400;
    let legend_rows = pack_legend_rows(&legend_entries, legend_packing_width);
    let legend_extra_height = (legend_rows.len().saturating_sub(1)) as i32 * LEGEND_ROW_H;
    let last_lane_bottom = laid
        .lane_tops
        .last()
        .zip(laid.lane_heights.last())
        .map_or(LANE_Y, |(&t, &h)| t + h);
    let legend_y = last_lane_bottom + LEGEND_Y_PAD + legend_extra_height;
    let auto_height = LANE_Y
        + laid.lane_heights.iter().sum::<i32>()
        + (doc.lanes.len().saturating_sub(1)) as i32 * laid.lane_gap
        + AUTO_HEIGHT_PAD
        + legend_extra_height;

    // Route every edge in canonical order, accumulating the placed world.
    let mut placed: Vec<(usize, usize, Vec<Pt>, Option<Rect>)> = Vec::new();
    let ports = port_spread(doc, &laid);
    for (ei, edge) in doc.edges.iter().enumerate() {
        let &from_idx = doc
            .node_index
            .get(edge.from.as_str())
            .expect("validated: from resolves");
        let &to_idx = doc
            .node_index
            .get(edge.to.as_str())
            .expect("validated: to resolves");
        let from_lane = doc.lane_index[doc.nodes[from_idx].lane.as_str()];
        let to_lane = doc.lane_index[doc.nodes[to_idx].lane.as_str()];
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
            placed: &scene_placed,
            canvas: (minimum_canvas_width, auto_height),
        };
        let same_lane = from_lane == to_lane;
        let gap_a = laid.lane_tops[from_lane] + laid.lane_heights[from_lane];
        let gap_b = laid.lane_tops[to_lane];
        let label_width = edge.label.as_deref().map(edge_label_width);
        let req = RouteRequest {
            from_idx,
            to_idx,
            from: &laid.rects[from_idx],
            to: &laid.rects[to_idx],
            from_col: doc.nodes[from_idx].col,
            to_col: doc.nodes[to_idx].col,
            forward: doc.nodes[to_idx].col > doc.nodes[from_idx].col,
            cross_lane: !same_lane,
            lane_gap: laid.lane_gap,
            label_width,
            from_side_authored: edge.from_side.as_deref().and_then(Side::parse),
            to_side_authored: edge.to_side.as_deref().and_then(Side::parse),
            route: match edge.route.as_deref() {
                Some(r) if r != "auto" => RouteKind::Preset(r),
                _ => RouteKind::Auto,
            },
            corridors: super::graph::Corridors {
                lane_gap_y: if same_lane {
                    laid.lane_tops[from_lane] - 160
                } else {
                    gap_a + (gap_b - gap_a) / 2
                },
                top_y: 80.max(laid.lane_tops[from_lane].min(laid.lane_tops[to_lane]) - 160),
                bottom_y: (laid.lane_tops[from_lane] + laid.lane_heights[from_lane])
                    .max(laid.lane_tops[to_lane] + laid.lane_heights[to_lane])
                    + 160,
                outside_left_x: LANE_X - 200,
                outside_right_x: LANE_X + laid.lane_w + 120,
            },
            col_xs: laid.col_xs,
            primary_ports: ports.get(&ei).copied(),
        };
        match plan_route(&req, &scene) {
            Ok(planned) => {
                let label = label_width.map(|w| label_rect(w, label_point(&planned.points)));
                placed.push((from_idx, to_idx, planned.points, label));
            }
            Err(RouteError::Feedback(f)) => return Err(CompileError::Feedback(f)),
            Err(RouteError::Exhausted { families }) => {
                return Err(CompileError::Problems(vec![format!(
                    "edge \"{}\" has no feasible automatic route (families tried: {}) — \
                     spread its endpoints across columns or shorten its label",
                    edge_name(edge),
                    families.join(", ")
                )]))
            }
            Err(RouteError::PresetConflict { preset }) => {
                return Err(CompileError::Problems(vec![format!(
                    "edge \"{}\" requests route \"{}\" but no side pair honors it under \
                     the readability constraints — try \"auto\"",
                    edge_name(edge),
                    preset
                )]))
            }
        }
    }

    // Measured bounds → final viewBox.
    let bounds = measured_bounds(doc, &laid, &placed, &legend_rows, legend_y);
    if bounds.0 < 0 || bounds.1 < 0 {
        return Err(CompileError::Problems(vec![
            "workflow geometry extends above or left of the viewBox origin — the solver \
             budget is exhausted; simplify the densest lane"
                .to_string(),
        ]));
    }
    let view_box = [
        minimum_canvas_width.max(bounds.2 + VB_RIGHT_MARGIN),
        auto_height.max(bounds.3 + VB_BOTTOM_MARGIN),
    ];

    let svg = emit_svg(doc, &laid, &placed, view_box, &legend_rows, legend_y);
    Ok(RenderedWorkflow {
        svg,
        title: doc.title.to_string(),
        view_box: [view_box[0] / 10, view_box[1] / 10],
        lanes: doc.lanes.len(),
        nodes: doc.nodes.len(),
        edges: doc.edges.len(),
    })
}

fn edge_name(edge: &WorkflowEdge) -> String {
    edge.id
        .clone()
        .unwrap_or_else(|| format!("{}->{}", edge.from, edge.to))
}

// ---------------------------------------------------------------------------
// constraint assembly
// ---------------------------------------------------------------------------

fn constraints(doc: &Canon, rank_gaps: &BTreeMap<(usize, usize), i32>) -> Vec<ColConstraint> {
    let mut out = Vec::new();
    for (&(from, to), &minimum) in rank_gaps {
        out.push(ColConstraint { from, to, minimum });
    }

    // Same-lane nodes in different columns that overlap vertically.
    for (i, &a) in doc.nodes.iter().enumerate() {
        for &b in &doc.nodes[i + 1..] {
            if a.lane != b.lane || a.col == b.col {
                continue;
            }
            if !vertical_overlap(a, b) {
                continue;
            }
            out.push(ColConstraint {
                from: a.col.min(b.col) as usize,
                to: a.col.max(b.col) as usize,
                minimum: NODE_W + PAIR_CLEARANCE,
            });
        }
    }

    // Facing direct edges (auto/straight, same yOffset) demand route room;
    // a label demands room for its mask (the reference defers this to
    // feedback iteration 2 — we merge it statically, same fixed point).
    for edge in &doc.edges {
        let &from_idx = doc
            .node_index
            .get(edge.from.as_str())
            .expect("validated: from resolves");
        let &to_idx = doc
            .node_index
            .get(edge.to.as_str())
            .expect("validated: to resolves");
        let (a, b) = (doc.nodes[from_idx], doc.nodes[to_idx]);
        if a.lane != b.lane || a.col == b.col {
            continue;
        }
        if !matches!(edge.route.as_deref(), None | Some("auto") | Some("straight")) {
            continue;
        }
        if a.y_offset.unwrap_or(0) != b.y_offset.unwrap_or(0) {
            continue;
        }
        let labeled = edge
            .label
            .as_deref()
            .map(edge_label_width)
            .map(|w| (w + LABEL_MASK_PAD).max(DIRECT_CLEARANCE))
            .unwrap_or(DIRECT_CLEARANCE);
        out.push(ColConstraint {
            from: a.col.min(b.col) as usize,
            to: a.col.max(b.col) as usize,
            minimum: NODE_W + labeled,
        });
    }

    for phase in &doc.phases {
        let minimum_width = text_units(&phase.label) as i32 * GROUP_LABEL_UNITS + 80;
        push_span_constraint(&mut out, phase.from_col, phase.to_col, minimum_width, 920);
    }
    for group in &doc.groups {
        let minimum_width = text_units(&group.label) as i32 * GROUP_LABEL_UNITS + 200;
        push_span_constraint(&mut out, group.from_col, group.to_col, minimum_width, 1000);
    }
    out
}

fn vertical_overlap(a: &WorkflowNode, b: &WorkflowNode) -> bool {
    let da = a.y_offset.unwrap_or(0);
    let db = b.y_offset.unwrap_or(0);
    (da - db).abs() * PX
        < (workflow_node_height(a) + workflow_node_height(b)) * PX / 2 + PAIR_CLEARANCE
}

/// Same-column spans push the next column; multi-column spans demand their
/// own label width over the natural lattice span.
fn push_span_constraint(
    out: &mut Vec<ColConstraint>,
    from_col: u8,
    to_col: u8,
    minimum_width: i32,
    node_w: i32,
) {
    if from_col == to_col {
        if (to_col as usize) < COLUMN_COUNT - 1 {
            out.push(ColConstraint {
                from: to_col as usize,
                to: to_col as usize + 1,
                minimum: 1200 + (minimum_width - node_w).max(0),
            });
        }
    } else {
        out.push(ColConstraint {
            from: from_col as usize,
            to: to_col as usize,
            minimum: (minimum_width - node_w).max(0),
        });
    }
}

// ---------------------------------------------------------------------------
// post-pass shifts and vertical math
// ---------------------------------------------------------------------------

fn post_pass_shifts(doc: &Canon, mut col_xs: [i32; COLUMN_COUNT]) -> [i32; COLUMN_COUNT] {
    // 1. First rank must clear the lane's left edge.
    let first_extent = doc
        .nodes
        .iter()
        .filter(|n| n.col == 0)
        .map(|_| NODE_W / 2)
        .fold(FIRST_EXTENT_FLOOR, i32::max);
    let left_shift = (LANE_X + LEFT_INSET + first_extent - col_xs[0]).max(0);
    for c in col_xs.iter_mut() {
        *c += left_shift;
    }

    // 2. Nodes with top-side endpoints must clear their lane header label.
    let mut header_shift = 0;
    for edge in &doc.edges {
        for (id, side) in [
            (&edge.from, edge.from_side.as_deref()),
            (&edge.to, edge.to_side.as_deref()),
        ] {
            if side != Some("top") {
                continue;
            }
            let &i = doc
                .node_index
                .get(id.as_str())
                .expect("validated: endpoint resolves");
            let node = doc.nodes[i];
            let lane_pos = doc.lane_index[node.lane.as_str()];
            let header_right = lane_header_right(&doc.lanes[lane_pos], lane_pos);
            header_shift =
                header_shift.max(header_right + LANE_HEADER_CLEARANCE - col_xs[node.col as usize]);
        }
    }
    for c in col_xs.iter_mut() {
        *c += header_shift;
    }

    // 3. Phases and groups must keep their frames inside the left content floor.
    let mut content_shift = 0;
    for phase in &doc.phases {
        let (left, _) = phase_span(phase, &col_xs);
        content_shift = content_shift.max(CONTENT_FLOOR - left);
    }
    for group in &doc.groups {
        let (left, _) = group_bounds(doc, group, &col_xs);
        content_shift = content_shift.max(GROUP_CONTENT_FLOOR - left);
    }
    for c in col_xs.iter_mut() {
        *c += content_shift;
    }
    col_xs
}

fn lane_header_right(lane: &Lane, lane_pos: usize) -> i32 {
    let prefix = if lane.variant.as_deref() == Some("exception") {
        "EX".to_string()
    } else {
        format!("{:02}", lane_pos + 1)
    };
    LANE_HEADER_ORIGIN
        + text_units(&format!("{prefix} / {}", lane.label)) as i32 * LANE_HEADER_UNITS
}

/// Phase span: (left, width) — centered between columns over the natural
/// spread, minimum the label width; same-column phases hang off the column.
fn phase_span(phase: &Phase, col_xs: &[i32; COLUMN_COUNT]) -> (i32, i32) {
    let natural = col_xs[phase.to_col as usize] - col_xs[phase.from_col as usize] + NODE_W;
    let width = natural.max(text_units(&phase.label) as i32 * GROUP_LABEL_UNITS + 80);
    let left = if phase.from_col == phase.to_col {
        col_xs[phase.from_col as usize] - PHASE_PAD
    } else {
        (col_xs[phase.from_col as usize] + col_xs[phase.to_col as usize] - width) / 2
    };
    (left, width)
}

/// Group frame bounds: (left, width) — column pad, label minimum, node inset.
fn group_bounds(doc: &Canon, group: &NodeGroup, col_xs: &[i32; COLUMN_COUNT]) -> (i32, i32) {
    let start = col_xs[group.from_col as usize] - GROUP_PAD;
    let end = col_xs[group.to_col as usize] + GROUP_PAD;
    let natural = end - start;
    let minimum = text_units(&group.label) as i32 * GROUP_LABEL_UNITS + 200;
    let width = natural.max(minimum);
    let mut left = if group.from_col == group.to_col && width > natural {
        start
    } else {
        (start + end - width) / 2
    };
    let mut right = left + width;
    for node in &doc.nodes {
        if node.lane != group.lane || node.col < group.from_col || node.col > group.to_col {
            continue;
        }
        let cx = col_xs[node.col as usize];
        left = left.min(cx - NODE_W / 2 - GROUP_NODE_INSET);
        right = right.max(cx + NODE_W / 2 + GROUP_NODE_INSET);
    }
    (left, right - left)
}

fn vertical_math(doc: &Canon, col_xs: [i32; COLUMN_COUNT], lane_gap_min: i32) -> Laid {
    let mut rightmost = col_xs[COLUMN_COUNT - 1] + RIGHTMOST_PAD;
    for node in &doc.nodes {
        rightmost = rightmost.max(col_xs[node.col as usize] + NODE_W / 2);
    }
    for group in &doc.groups {
        let (left, width) = group_bounds(doc, group, &col_xs);
        rightmost = rightmost.max(left + width);
    }
    let lane_label_width = doc
        .lanes
        .iter()
        .enumerate()
        .map(|(i, lane)| lane_header_right(lane, i) - LANE_X + LANE_LABEL_PAD)
        .max()
        .unwrap_or(0);
    let lane_w = LANE_W_MIN
        .max(rightmost - LANE_X + RIGHTMOST_INSET)
        .max(lane_label_width);

    let max_extent = doc
        .nodes
        .iter()
        .map(|n| workflow_node_height(n) * PX / 2 + (n.y_offset.unwrap_or(0) * PX).abs())
        .max()
        .unwrap_or(0);
    let base_content_h = BASE_CONTENT_MIN.max(max_extent * 2 + BASE_CONTENT_PAD);
    let lane_h = LANE_TITLE_H + base_content_h;

    // Group reserves per lane: the frame label must clear its nodes, and the
    // frame must close below them.
    let mut headers = vec![0i32; doc.lanes.len()];
    let mut footers = vec![0i32; doc.lanes.len()];
    for group in &doc.groups {
        let lane = doc.lane_index[group.lane.as_str()];
        let (gx, _) = group_bounds(doc, group, &col_xs);
        let label_left = gx + GROUP_LABEL_X_INSET;
        let label_right = label_left + text_units(&group.label) as i32 * GROUP_LABEL_UNITS;
        for node in &doc.nodes {
            if node.lane != group.lane || node.col < group.from_col || node.col > group.to_col {
                continue;
            }
            let cx = col_xs[node.col as usize];
            let overlaps_label = cx + NODE_W / 2 > label_left && cx - NODE_W / 2 < label_right;
            let h = workflow_node_height(node) * PX;
            let top_offset = (base_content_h - h) / 2 + node.y_offset.unwrap_or(0) * PX;
            let minimum_top = if overlaps_label {
                GROUP_TOP_OVER_LABEL
            } else {
                GROUP_TOP_CLEAR
            };
            headers[lane] = headers[lane].max((minimum_top - top_offset).max(0));
            let bottom_margin = base_content_h - GROUP_FRAME_BOTTOM_INSET - top_offset - h;
            footers[lane] = footers[lane].max((GROUP_BOTTOM_MARGIN - bottom_margin).max(0));
        }
    }
    let lane_heights: Vec<i32> = (0..doc.lanes.len())
        .map(|i| lane_h + headers[i] + footers[i])
        .collect();
    let lane_gap = LANE_GAP_BASE.max(lane_gap_min);
    let mut lane_tops = Vec::with_capacity(doc.lanes.len());
    let mut top = LANE_Y;
    for (i, _) in doc.lanes.iter().enumerate() {
        lane_tops.push(top);
        top += lane_heights[i] + lane_gap;
    }

    let rects: Vec<Rect> = doc
        .nodes
        .iter()
        .map(|node| {
            let h = workflow_node_height(node) * PX;
            let lane = doc.lane_index[node.lane.as_str()];
            let y = lane_tops[lane]
                + LANE_TITLE_H
                + headers[lane]
                + (base_content_h - h) / 2
                + node.y_offset.unwrap_or(0) * PX;
            Rect {
                x: col_xs[node.col as usize] - NODE_W / 2,
                y,
                w: NODE_W,
                h,
            }
        })
        .collect();
    Laid {
        col_xs,
        lane_w,
        lane_tops,
        lane_heights,
        lane_gap,
        rects,
    }
}

// ---------------------------------------------------------------------------
// port spread
// ---------------------------------------------------------------------------

/// Fan-out ports: when ≥2 auto edges share a node side, spread their anchors
/// so parallel edges leave from distinct points (gutter 16px, spacing ≤14px).
fn port_spread(doc: &Canon, laid: &Laid) -> HashMap<usize, (Pt, Pt)> {
    struct Item {
        edge: usize,
        from_endpoint: bool,
        node: usize,
        key: String,
    }
    let mut groups: HashMap<(usize, Side), Vec<Item>> = HashMap::new();
    for (ei, edge) in doc.edges.iter().enumerate() {
        if edge.route.as_deref().is_some_and(|r| r != "auto") {
            continue;
        }
        let &from_idx = doc
            .node_index
            .get(edge.from.as_str())
            .expect("validated: from resolves");
        let &to_idx = doc
            .node_index
            .get(edge.to.as_str())
            .expect("validated: to resolves");
        let (fs, ts) = default_sides(&laid.rects[from_idx], &laid.rects[to_idx]);
        let fs = edge.from_side.as_deref().and_then(Side::parse).unwrap_or(fs);
        let ts = edge.to_side.as_deref().and_then(Side::parse).unwrap_or(ts);
        let key = format!(
            "{}|{}|{}|{}",
            edge.id.as_deref().unwrap_or(""),
            edge.from,
            edge.to,
            edge.label.as_deref().unwrap_or("")
        );
        groups.entry((from_idx, fs)).or_default().push(Item {
            edge: ei,
            from_endpoint: true,
            node: from_idx,
            key: key.clone(),
        });
        groups.entry((to_idx, ts)).or_default().push(Item {
            edge: ei,
            from_endpoint: false,
            node: to_idx,
            key,
        });
    }

    let mut spread: HashMap<(usize, bool), Pt> = HashMap::new();
    for ((node_idx, side), mut items) in groups {
        if items.len() < 2 {
            continue;
        }
        let horizontal_side = side.horizontal();
        items.sort_by(|a, b| {
            let counterpart = |item: &Item| {
                let other = if item.from_endpoint {
                    doc.edges[item.edge].to.as_str()
                } else {
                    doc.edges[item.edge].from.as_str()
                };
                let idx = doc.node_index.get(other).copied().unwrap_or(item.node);
                if horizontal_side {
                    laid.rects[idx].cy()
                } else {
                    laid.rects[idx].cx()
                }
            };
            counterpart(a)
                .cmp(&counterpart(b))
                .then_with(|| a.key.cmp(&b.key))
        });
        let rect = &laid.rects[node_idx];
        let extent = if horizontal_side { rect.h } else { rect.w };
        let usable = (extent - 320).max(0);
        let spacing = (usable / (items.len() as i32 - 1)).min(140);
        if spacing <= 0 {
            continue;
        }
        for (i, item) in items.iter().enumerate() {
            let offset = (2 * i as i32 - (items.len() as i32 - 1)) * spacing / 2;
            let mut point = anchor(rect, side);
            if horizontal_side {
                point.1 += offset;
            } else {
                point.0 += offset;
            }
            spread.insert((item.edge, item.from_endpoint), point);
        }
    }

    let mut ports: HashMap<usize, (Pt, Pt)> = HashMap::new();
    for ((edge, is_from), point) in spread {
        let entry = ports.entry(edge).or_insert((point, point));
        if is_from {
            entry.0 = point;
        } else {
            entry.1 = point;
        }
    }
    ports
}

// ---------------------------------------------------------------------------
// measured bounds, legend packing
// ---------------------------------------------------------------------------

/// (left, top, right, bottom) over every drawn thing.
fn measured_bounds(
    doc: &Canon,
    laid: &Laid,
    placed: &[(usize, usize, Vec<Pt>, Option<Rect>)],
    legend_rows: &[Vec<(i32, i32, usize)>],
    legend_y: i32,
) -> (i32, i32, i32, i32) {
    let mut left = LANE_X;
    let mut top = 270;
    let mut right = LANE_X + laid.lane_w;
    let mut bottom = legend_y + 180;
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
    for phase in &doc.phases {
        let (x, width) = phase_span(phase, &laid.col_xs);
        include(x, 270, &mut left, &mut top, &mut right, &mut bottom);
        include(x + width, 430, &mut left, &mut top, &mut right, &mut bottom);
    }
    for group in &doc.groups {
        let lane = doc.lane_index[group.lane.as_str()];
        let (gx, gw) = group_bounds(doc, group, &laid.col_xs);
        let fy = laid.lane_tops[lane] + LANE_TITLE_H + GROUP_FRAME_TOP_INSET;
        let fb = laid.lane_tops[lane] + laid.lane_heights[lane] - GROUP_FRAME_BOTTOM_INSET;
        include(gx, fy, &mut left, &mut top, &mut right, &mut bottom);
        include(gx + gw, fb, &mut left, &mut top, &mut right, &mut bottom);
        // The label mask rides the frame's top-left corner.
        include(
            gx + GROUP_LABEL_X_INSET,
            fy + GROUP_LABEL_BASELINE_OFFSET - 100,
            &mut left,
            &mut top,
            &mut right,
            &mut bottom,
        );
        include(
            gx + GROUP_LABEL_X_INSET + text_units(&group.label) as i32 * GROUP_LABEL_UNITS,
            fy + GROUP_LABEL_BASELINE_OFFSET + 40,
            &mut left,
            &mut top,
            &mut right,
            &mut bottom,
        );
    }
    for (row, entries) in legend_rows.iter().enumerate() {
        let baseline = legend_y - (legend_rows.len() as i32 - 1 - row as i32) * LEGEND_ROW_H;
        for &(x, w, _) in entries {
            include(x, baseline - 100, &mut left, &mut top, &mut right, &mut bottom);
            include(
                x + w,
                baseline + 40,
                &mut left,
                &mut top,
                &mut right,
                &mut bottom,
            );
        }
    }
    (left, top, right, bottom)
}

/// Pack legend entries into rows that fit `width`; each entry lands as
/// (x, width, catalog index).
fn pack_legend_rows(entries: &[(usize, i32)], width: i32) -> Vec<Vec<(i32, i32, usize)>> {
    let mut rows: Vec<Vec<(i32, i32, usize)>> = vec![Vec::new()];
    let mut cursor = LEGEND_X + LEGEND_TITLE_W + 80;
    for &(idx, units) in entries {
        let w = LEGEND_SWATCH_W + LEGEND_SWATCH_TEXT_GAP + units * LEGEND_UNITS;
        if cursor + w > LEGEND_X + width {
            rows.push(Vec::new());
            cursor = LEGEND_X;
        }
        rows.last_mut().expect("seeded row").push((cursor, w, idx));
        cursor += w + LEGEND_ITEM_GAP;
    }
    rows
}

// ---------------------------------------------------------------------------
// SVG emission
// ---------------------------------------------------------------------------

fn kind_palette(kind: &str) -> (&'static str, &'static str, &'static str) {
    match kind {
        "frontend" => ("#dbeafe", "#2563eb", "#1e40af"),
        "backend" => ("#dcfce7", "#16a34a", "#166534"),
        "database" => ("#fef3c7", "#d97706", "#92400e"),
        "cloud" => ("#e0e7ff", "#4f46e5", "#3730a3"),
        "security" => ("#fee2e2", "#dc2626", "#991b1b"),
        "messagebus" => ("#f3e8ff", "#9333ea", "#6b21a8"),
        _ => ("#f4f4f5", "#71717a", "#3f3f46"),
    }
}

/// (stroke, width, dash, label fill) per edge variant — the sequence table.
fn variant_style(variant: &str) -> (&'static str, i32, Option<&'static str>, &'static str) {
    match variant {
        "emphasis" => ("#18181b", 18, None, "#18181b"),
        "security" => ("#dc2626", 14, None, "#dc2626"),
        "dashed" => ("#9333ea", 14, Some("6 4"), "#9333ea"),
        "return" => ("#71717a", 14, Some("3 5"), "#71717a"),
        _ => ("#52525b", 14, None, "#52525b"),
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
    legend_rows: &[Vec<(i32, i32, usize)>],
    legend_y: i32,
) -> String {
    let mut s = String::with_capacity(8192);
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {} {}\" role=\"img\" aria-label=\"{}\">",
        tx(view_box[0]),
        tx(view_box[1]),
        esc(doc.title)
    ));

    // Lanes.
    for (i, lane) in doc.lanes.iter().enumerate() {
        let y = laid.lane_tops[i];
        let h = laid.lane_heights[i];
        let prefix = if lane.variant.as_deref() == Some("exception") {
            "EX".to_string()
        } else {
            format!("{:02}", i + 1)
        };
        let exception = lane.variant.as_deref() == Some("exception");
        s.push_str(&format!(
            "<rect data-lane-id=\"{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"10\" fill=\"#fafafa\" stroke=\"{}\" stroke-width=\"1\"/>",
            esc(&lane.id),
            tx(LANE_X),
            tx(y),
            tx(laid.lane_w),
            tx(h),
            if exception { "#dc2626" } else { "#d4d4d8" }
        ));
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"10\" font-weight=\"600\" fill=\"{}\">{} / {}</text>",
            tx(LANE_X + 140),
            tx(y + 220),
            if exception { "#dc2626" } else { "#52525b" },
            esc(&prefix),
            esc(&lane.label)
        ));
    }

    // Phases.
    for phase in &doc.phases {
        let (x, width) = phase_span(phase, &laid.col_xs);
        let accent = if phase.variant.as_deref() == Some("security") {
            "#dc2626"
        } else {
            "#52525b"
        };
        s.push_str(&format!(
            "<line x1=\"{}\" y1=\"35\" x2=\"{}\" y2=\"35\" stroke=\"#71717a\" stroke-width=\"1.1\"/>",
            tx(x),
            tx(x + width)
        ));
        s.push_str(&format!(
            "<rect x=\"{}\" y=\"27\" width=\"{}\" height=\"16\" rx=\"4\" fill=\"#ffffff\"/>",
            tx(x),
            tx(width)
        ));
        s.push_str(&format!(
            "<text x=\"{}\" y=\"39\" font-size=\"8\" font-weight=\"600\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
            tx(x + width / 2),
            accent,
            esc(&phase.label)
        ));
    }

    // Groups.
    for group in &doc.groups {
        let lane = doc.lane_index[group.lane.as_str()];
        let (gx, gw) = group_bounds(doc, group, &laid.col_xs);
        let fy = laid.lane_tops[lane] + LANE_TITLE_H + GROUP_FRAME_TOP_INSET;
        let gh = laid.lane_heights[lane] - LANE_TITLE_H - GROUP_FRAME_TOP_INSET
            - GROUP_FRAME_BOTTOM_INSET;
        let security = group.variant.as_deref() == Some("security");
        s.push_str(&format!(
            "<rect data-group-id=\"{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"9\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1\"/>",
            esc(&group.id),
            tx(gx),
            tx(fy),
            tx(gw),
            tx(gh),
            if security { "#fef2f2" } else { "#ffffff" },
            if security { "#dc2626" } else { "#a1a1aa" }
        ));
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"7\" font-weight=\"600\" fill=\"{}\">{}</text>",
            tx(gx + GROUP_LABEL_X_INSET),
            tx(fy + GROUP_LABEL_BASELINE_OFFSET),
            if security { "#dc2626" } else { "#52525b" },
            esc(&group.label)
        ));
    }

    // Edge paths + arrowheads.
    for (i, edge) in doc.edges.iter().enumerate() {
        let (_, _, points, _) = &placed[i];
        let variant = edge.variant.as_deref().unwrap_or("default");
        let (stroke, stroke_w, dash, _) = variant_style(variant);
        let dash_attr = dash.map_or(String::new(), |d| format!(" stroke-dasharray=\"{d}\""));
        s.push_str(&format!(
            "<path data-from=\"{}\" data-to=\"{}\" d=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{}\"{}/>",
            esc(&edge.from),
            esc(&edge.to),
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
        let (fill, stroke, text_fill) = kind_palette(&node.kind);
        s.push_str(&format!("<g data-node-id=\"{}\">", esc(&node.id)));
        s.push_str(&format!(
            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"6\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1.5\"/>",
            tx(rect.x),
            tx(rect.y),
            tx(rect.w),
            tx(rect.h),
            fill,
            stroke
        ));
        let label_font =
            fitted_font(&node.label, NODE_W - LABEL_RESERVE, LABEL_PREFERRED, LABEL_MIN);
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"{}\" font-weight=\"600\" fill=\"#18181b\" text-anchor=\"middle\">{}</text>",
            tx(rect.cx()),
            tx(rect.y + 210),
            tx(label_font),
            esc(&node.label)
        ));
        if let Some(sub) = node.sublabel.as_deref().filter(|s| !s.trim().is_empty()) {
            let sub_font = fitted_font(sub, NODE_W, SUB_PREFERRED, SUB_MIN);
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"#71717a\" text-anchor=\"middle\">{}</text>",
                tx(rect.cx()),
                tx(rect.y + 380),
                tx(sub_font),
                esc(sub)
            ));
        }
        if let Some(tag) = node.tag.as_deref().filter(|t| !t.trim().is_empty()) {
            let tag_font = fitted_font(tag, NODE_W, TAG_PREFERRED, TAG_MIN);
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
                tx(rect.cx()),
                tx(rect.y + rect.h - 120),
                tx(tag_font),
                text_fill,
                esc(tag)
            ));
        }
        s.push_str("</g>");
    }

    // Edge labels.
    for (i, edge) in doc.edges.iter().enumerate() {
        let Some(label) = &edge.label else { continue };
        let (_, _, points, lrect) = &placed[i];
        let Some(lr) = lrect else { continue };
        let at = label_point(points);
        let (_, _, _, label_fill) = variant_style(edge.variant.as_deref().unwrap_or("default"));
        s.push_str(&format!(
            "<g data-from=\"{}\" data-to=\"{}\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"14\" rx=\"3\" fill=\"#ffffff\"/><text x=\"{}\" y=\"{}\" font-size=\"8\" fill=\"{}\" text-anchor=\"middle\">{}</text></g>",
            esc(&edge.from),
            esc(&edge.to),
            tx(lr.x),
            tx(lr.y),
            tx(lr.w),
            tx(at.0),
            tx(at.1),
            label_fill,
            esc(label)
        ));
    }

    // Legend.
    if legend_rows.iter().any(|r| !r.is_empty()) {
        for (row, entries) in legend_rows.iter().enumerate() {
            let baseline = legend_y - (legend_rows.len() as i32 - 1 - row as i32) * LEGEND_ROW_H;
            if row == 0 {
                s.push_str(&format!(
                    "<text x=\"{}\" y=\"{}\" font-size=\"7\" font-weight=\"600\" fill=\"#52525b\">Legend</text>",
                    tx(LEGEND_X),
                    tx(baseline)
                ));
            }
            for &(x, _w, idx) in entries {
                let (kind, label) = LEGEND_CATALOG[idx];
                let (fill, stroke, _) = kind_palette(kind);
                s.push_str(&format!(
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"2\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1\"/><text x=\"{}\" y=\"{}\" font-size=\"7\" fill=\"#52525b\">{}</text>",
                    tx(x),
                    tx(baseline - 80),
                    tx(LEGEND_SWATCH_W),
                    tx(LEGEND_SWATCH_H),
                    fill,
                    stroke,
                    tx(x + LEGEND_SWATCH_W + LEGEND_SWATCH_TEXT_GAP),
                    tx(baseline),
                    esc(label)
                ));
            }
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

    const SPEC_JSON: &str = r#"{
        "title": "Checkout",
        "lanes": [
            {"id": "web", "label": "Web"},
            {"id": "svc", "label": "Services"}
        ],
        "nodes": [
            {"id": "cart", "lane": "web", "col": 0, "label": "Cart", "type": "frontend"},
            {"id": "pay", "lane": "web", "col": 1, "label": "Pay", "type": "frontend"},
            {"id": "orders", "lane": "svc", "col": 2, "label": "Orders", "type": "backend"},
            {"id": "db", "lane": "svc", "col": 3, "label": "DB", "type": "database"}
        ],
        "edges": [
            {"from": "cart", "to": "pay", "label": "checkout"},
            {"from": "pay", "to": "orders", "label": "charge"},
            {"from": "orders", "to": "db", "label": "insert", "variant": "dashed"}
        ]
    }"#;

    fn spec() -> WorkflowSpec {
        let s: WorkflowSpec = serde_json::from_str(SPEC_JSON).unwrap();
        assert!(super::super::spec::validate_workflow(&s).is_empty());
        s
    }

    #[test]
    fn two_lane_pipeline_renders_deterministically() {
        let a = render_workflow(&spec()).unwrap();
        let b = render_workflow(&spec()).unwrap();
        assert_eq!(a.svg, b.svg, "same input, same bytes");
        assert_eq!((a.lanes, a.nodes, a.edges), (2, 4, 3));
        assert!(a.svg.contains("data-lane-id=\"web\""));
        assert!(a.svg.contains("data-node-id=\"cart\""));
        assert!(a.svg.contains("data-from=\"pay\" data-to=\"orders\""));
        assert!(a.svg.contains("stroke-dasharray=\"6 4\""));
        // Labeled direct edges widen their rank gaps, so the canvas exceeds
        // the 640px lane minimum: 40 + laneW + 16.
        assert!(a.view_box[0] >= 696, "got {}", a.view_box[0]);
        // Legend lists the three kinds present.
        assert!(a.svg.contains(">Legend<"));
        assert!(a.svg.contains("Frontend"));
        assert!(a.svg.contains("Database"));
        assert!(!a.svg.contains("Message bus"));
    }

    #[test]
    fn shuffled_collections_render_identical_bytes() {
        // Same logical document, nodes/edges/phases listed in a different
        // order — the canonical sorts must erase the difference.
        let shuffled: WorkflowSpec = serde_json::from_str(
            r#"{
                "title": "Checkout",
                "lanes": [
                    {"id": "web", "label": "Web"},
                    {"id": "svc", "label": "Services"}
                ],
                "nodes": [
                    {"id": "db", "lane": "svc", "col": 3, "label": "DB", "type": "database"},
                    {"id": "orders", "lane": "svc", "col": 2, "label": "Orders", "type": "backend"},
                    {"id": "pay", "lane": "web", "col": 1, "label": "Pay", "type": "frontend"},
                    {"id": "cart", "lane": "web", "col": 0, "label": "Cart", "type": "frontend"}
                ],
                "edges": [
                    {"from": "orders", "to": "db", "label": "insert", "variant": "dashed"},
                    {"from": "pay", "to": "orders", "label": "charge"},
                    {"from": "cart", "to": "pay", "label": "checkout"}
                ]
            }"#,
        )
        .unwrap();
        assert!(super::super::spec::validate_workflow(&shuffled).is_empty());
        assert_eq!(
            render_workflow(&spec()).unwrap().svg,
            render_workflow(&shuffled).unwrap().svg
        );
    }

    #[test]
    fn backward_edge_detours_instead_of_crossing_nodes() {
        let mut s = spec();
        s.edges.push(WorkflowEdge {
            id: None,
            from: "db".to_string(),
            to: "cart".to_string(),
            label: Some("audit".to_string()),
            variant: Some("return".to_string()),
            route: None,
            from_side: None,
            to_side: None,
        });
        let rendered = render_workflow(&s).unwrap();
        let path = rendered
            .svg
            .split("data-from=\"db\" data-to=\"cart\"")
            .nth(1)
            .unwrap()
            .split("/>")
            .next()
            .unwrap();
        // A multi-column backward edge is never a 2-point straight line —
        // it detours through a corridor.
        assert!(
            path.matches('L').count() >= 2,
            "backward route too straight: {path}"
        );
        assert!(rendered.svg.contains("stroke-dasharray=\"3 5\""));
    }

    #[test]
    fn up_channel_preset_draws_the_channel_above_the_nodes() {
        let s: WorkflowSpec = serde_json::from_str(
            r#"{
                "title": "Retry",
                "lanes": [{"id": "svc", "label": "Services"}],
                "nodes": [
                    {"id": "a", "lane": "svc", "col": 0, "label": "A", "type": "backend"},
                    {"id": "b", "lane": "svc", "col": 1, "label": "B", "type": "backend"}
                ],
                "edges": [
                    {"from": "a", "to": "b", "label": "work"},
                    {"from": "b", "to": "a", "route": "up-channel", "variant": "return"}
                ]
            }"#,
        )
        .unwrap();
        let rendered = render_workflow(&s).unwrap();
        // Node tops sit at y=93 (lane 52 + title 30 + centering); the channel
        // runs 28px above them at y=65.
        assert!(
            rendered.svg.contains(" 65 L "),
            "channel leg missing: {}",
            rendered.svg
        );
    }
}
