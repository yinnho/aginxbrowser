//! Lifecycle family adapter: the state machine over three fixed bands.
//!
//! Lane id "main" owns the top phase band (columns 0..=4), "terminal" the
//! bottom outcome band (0..=2); every other lane shares the middle event
//! band (0..=2), separated only by yOffset. Bands are dashed reading
//! guides, not containers — transitions cross them freely. Layout
//! arithmetic is archify's lifecycle renderer (MIT) recast in integer
//! tenths; routing goes through the shared graph engine with per-edge
//! local corridors. The bands are fixed, so router feedback is terminal —
//! a diagnostic, never a loop — and an infeasible authored preset walks
//! the family's substitution ladder first (verified substitutes only,
//! every substitution disclosed).
//!
//! v1 divergences from the reference (kept deliberately): no authored
//! state sizes (band geometry owns them), no authored meta.viewBox (the
//! canvas is measured), and transition labels fit by width formula instead
//! of width-overflow diagnostics.

use std::collections::HashMap;

use super::graph::{
    label_point, label_rect, plan_grid_edge, polyline_d, Corridors, PlacedRoute, Pt, Rect,
    RouteError, RouteKind, RouteRepair, RouteRequest, RouteScene, Side, COLUMN_COUNT, PX,
};
use super::spec::{
    lifecycle_band, text_units, LifecycleBand, LifecycleLane, LifecycleSpec, LifecycleState,
    LifecycleTransition,
};
use super::theme::Theme;
use super::tx;

// ---------------------------------------------------------------------------
// constants (archify px × 10)
// ---------------------------------------------------------------------------

/// Band column origins: phase starts at column 0 of the wide grid, event
/// and outcome share the centered three-column grid.
const PHASE_X0: i32 = 940;
const EVENT_X0: i32 = 4020;
const COL_PITCH: i32 = 1540;
/// Dashed band rails and their titles ("01 / …") at fixed heights.
const RAIL_YS: [i32; 3] = [1120, 2640, 4360];
const RAIL_X: i32 = 720;
/// The phase spine: a solid line riding the phase-state bottoms.
const PHASE_SPINE_TAIL: i32 = 380;
const PHASE_SPINE_MIN_TAIL: i32 = 500;
/// Per-edge local corridors: 28px above the higher endpoint, 34px below the
/// deeper bottom, 36px outside each side.
const TOP_CLEAR: i32 = 280;
const BOTTOM_CLEAR: i32 = 340;
const SIDE_CLEAR: i32 = 360;
/// Transition label: w = max(32, units·4.9 + 12) px; a note adds a line.
const LABEL_WIDTH_FACTOR: i32 = 49;
const LABEL_WIDTH_PAD: i32 = 120;
const LABEL_WIDTH_MIN: i32 = 320;
/// Fonts (tenths): state label 10/8, sublabel/step/tag 7/6; transition
/// label 8, note 6. 0.6px advance per text unit per px of font size.
const LABEL_PREFERRED: i32 = 100;
const LABEL_MIN: i32 = 80;
const SUB_PREFERRED: i32 = 70;
const SUB_MIN: i32 = 60;
const TEXT_WIDTH_FACTOR: i32 = 6;
/// Legend: 14×9 swatch, 3px to the text, 24px between items.
const LEGEND_SWATCH_W: i32 = 140;
const LEGEND_SWATCH_H: i32 = 90;
const LEGEND_SWATCH_TEXT_GAP: i32 = 30;
const LEGEND_ITEM_GAP: i32 = 240;
const LEGEND_X: i32 = 160;
/// Canvas: rails inset 72px each side; the width floor keeps the full
/// phase grid reachable; the height floor keeps the outcome band plus the
/// legend reserve.
const CANVAS_MARGIN: i32 = 880;
const CANVAS_BOTTOM_PAD: i32 = 1220;
const LEGEND_BASELINE_FROM_BOTTOM: i32 = 1000;

/// Legend catalog: state kinds present, in kind order.
const LEGEND_CATALOG: [(&str, &str); 8] = [
    ("start", "Start"),
    ("active", "Active"),
    ("waiting", "Waiting"),
    ("decision", "Decision"),
    ("success", "Success"),
    ("failure", "Failure"),
    ("neutral", "Neutral"),
    ("external", "External"),
];

#[derive(Debug)]
pub struct RenderedLifecycle {
    pub svg: String,
    pub title: String,
    /// ViewBox in px.
    pub view_box: [i32; 2],
    pub lanes: usize,
    pub states: usize,
    pub transitions: usize,
    /// Route presets the engine substituted, in canonical transition order.
    pub repairs: Vec<RouteRepair>,
}

/// The canonicalized document: states sort (band, col, yOffset, id) with
/// band order Phase < Event < Outcome, transitions sort
/// (id, from, to, label, note); lanes keep document order.
struct Canon<'a> {
    title: &'a str,
    lanes: &'a [LifecycleLane],
    states: Vec<&'a LifecycleState>,
    transitions: Vec<&'a LifecycleTransition>,
    state_index: HashMap<&'a str, usize>,
}

fn band_rank(lane: &str) -> u8 {
    match lifecycle_band(lane) {
        LifecycleBand::Phase => 0,
        LifecycleBand::Event => 1,
        LifecycleBand::Outcome => 2,
    }
}

fn canon<'a>(spec: &'a LifecycleSpec) -> Canon<'a> {
    let mut states: Vec<&LifecycleState> = spec.states.iter().collect();
    states.sort_by(|a, b| {
        band_rank(&a.lane)
            .cmp(&band_rank(&b.lane))
            .then(a.col.cmp(&b.col))
            .then(a.y_offset.unwrap_or(0).cmp(&b.y_offset.unwrap_or(0)))
            .then(a.id.cmp(&b.id))
    });
    let mut transitions: Vec<&LifecycleTransition> = spec.transitions.iter().collect();
    transitions.sort_by(|a, b| {
        a.id.as_deref()
            .unwrap_or("")
            .cmp(b.id.as_deref().unwrap_or(""))
            .then(a.from.cmp(&b.from))
            .then(a.to.cmp(&b.to))
            .then(
                a.label
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.label.as_deref().unwrap_or("")),
            )
            .then(
                a.note
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.note.as_deref().unwrap_or("")),
            )
    });
    let state_index: HashMap<&'a str, usize> = states
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();
    Canon {
        title: spec.title.trim(),
        lanes: &spec.lanes,
        states,
        transitions,
        state_index,
    }
}

/// Column center for a band cell (tenths).
fn col_x(band: LifecycleBand, col: u8) -> i32 {
    match band {
        LifecycleBand::Phase => PHASE_X0 + col as i32 * COL_PITCH,
        LifecycleBand::Event | LifecycleBand::Outcome => EVENT_X0 + col as i32 * COL_PITCH,
    }
}

/// Render a validated lifecycle spec. Infeasibility is terminal (the bands
/// are fixed): the transition names its dead end and the caller falls the
/// fence back to a code block.
pub fn render_lifecycle(
    spec: &LifecycleSpec,
    theme: &'static Theme,
) -> Result<RenderedLifecycle, Vec<String>> {
    let doc = canon(spec);
    let mut laid = layout(&doc);

    let mut placed: Vec<(usize, usize, Vec<Pt>, Option<Rect>)> = Vec::new();
    let mut repairs: Vec<RouteRepair> = Vec::new();
    for transition in &doc.transitions {
        let &from_idx = doc
            .state_index
            .get(transition.from.as_str())
            .expect("validated: from resolves");
        let &to_idx = doc
            .state_index
            .get(transition.to.as_str())
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
        let label_w = transition_label_width(
            transition.label.as_deref(),
            transition.note.as_deref(),
        );
        let (from, to) = (&laid.rects[from_idx], &laid.rects[to_idx]);
        let req = RouteRequest {
            from_idx,
            to_idx,
            from,
            to,
            from_col: doc.states[from_idx].col,
            to_col: doc.states[to_idx].col,
            forward: doc.states[to_idx].col > doc.states[from_idx].col,
            cross_lane: false,
            lane_gap: 200,
            label_width: label_w,
            from_side_authored: transition.from_side.as_deref().and_then(Side::parse),
            to_side_authored: transition.to_side.as_deref().and_then(Side::parse),
            route: match transition.route.as_deref() {
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
            col_xs: grid_col_xs(),
            primary_ports: None,
        };
        match plan_grid_edge(req, &scene, ladder) {
            Ok((planned, substituted)) => {
                if let Some((requested, sub)) = substituted {
                    repairs.push(RouteRepair {
                        edge: transition_name(transition),
                        requested: requested.to_string(),
                        substituted: sub.to_string(),
                    });
                }
                let label = label_w.map(|w| label_rect(w, label_point(&planned.points)));
                placed.push((from_idx, to_idx, planned.points, label));
            }
            Err(RouteError::PresetConflict { preset }) => {
                return Err(vec![format!(
                    "transition \"{}\" requests route \"{preset}\" but no verified substitute \
                     fits — move its states to another column or band, or loosen the route",
                    transition_name(transition)
                )])
            }
            Err(_) => {
                return Err(vec![format!(
                    "transition \"{}\" has no feasible route between its states — spread them \
                     across columns or bands, or shorten the label",
                    transition_name(transition)
                )])
            }
        }
    }

    let bounds = measured_bounds(&laid, &placed);
    if bounds.0 < 0 || bounds.1 < 0 {
        return Err(vec![
            "lifecycle geometry extends above or left of the viewBox origin — pull the yOffset \
             stack back toward its band"
                .to_string(),
        ]);
    }
    // Final canvas: the lattice floors, grown by whatever the routes
    // actually reach.
    laid.content_w = laid.content_w.max(bounds.2 + CANVAS_MARGIN);
    laid.content_h = laid.content_h.max(bounds.3 + CANVAS_MARGIN);
    laid.legend_baseline = laid.content_h - LEGEND_BASELINE_FROM_BOTTOM;
    let view_box = [laid.content_w, laid.content_h];

    let legend: Vec<usize> = LEGEND_CATALOG
        .iter()
        .enumerate()
        .filter(|(_, (kind, _))| doc.states.iter().any(|s| s.kind == *kind))
        .map(|(i, _)| i)
        .collect();

    let svg = emit_svg(&doc, &laid, &placed, view_box, &legend, theme);
    Ok(RenderedLifecycle {
        svg,
        title: doc.title.to_string(),
        view_box: [view_box[0] / 10, view_box[1] / 10],
        lanes: doc.lanes.len(),
        states: doc.states.len(),
        transitions: doc.transitions.len(),
        repairs,
    })
}

/// Semantic substitution ladder for an infeasible preset: keep the
/// transition's reading (a dive stays low, a sideways exit stays sideways)
/// while dropping the exact shape that cannot be honored. First verified
/// substitute wins.
fn ladder(route: &str) -> &'static [&'static str] {
    match route {
        "straight" => &["auto"],
        "drop" => &["bottom-channel", "top-channel"],
        "bottom-channel" => &["top-channel"],
        "top-channel" => &["bottom-channel"],
        "left-channel" => &["right-channel", "auto"],
        "right-channel" => &["left-channel", "auto"],
        _ => &[],
    }
}

fn transition_name(transition: &LifecycleTransition) -> String {
    transition
        .id
        .clone()
        .unwrap_or_else(|| format!("{}->{}", transition.from, transition.to))
}

fn transition_label_width(label: Option<&str>, note: Option<&str>) -> Option<i32> {
    let has_label = label.is_some_and(|l| !l.trim().is_empty());
    let has_note = note.is_some_and(|n| !n.trim().is_empty());
    if !has_label && !has_note {
        return None;
    }
    let units = text_units(label.unwrap_or("")).max(text_units(note.unwrap_or("")));
    Some((units as i32 * LABEL_WIDTH_FACTOR + LABEL_WIDTH_PAD).max(LABEL_WIDTH_MIN))
}

// ---------------------------------------------------------------------------
// fixed-band layout
// ---------------------------------------------------------------------------

fn grid_col_xs() -> [i32; COLUMN_COUNT] {
    let mut xs = [0i32; COLUMN_COUNT];
    for (i, x) in xs.iter_mut().enumerate() {
        *x = EVENT_X0 + i as i32 * COL_PITCH;
    }
    xs
}

struct Laid {
    rects: Vec<Rect>,
    /// Band titles for the three rails.
    band_titles: [String; 3],
    /// Phase spine extent, when the main lane has states.
    spine: Option<(i32, i32)>,
    content_w: i32,
    content_h: i32,
    legend_baseline: i32,
}

fn state_rect(state: &LifecycleState) -> Rect {
    let band = lifecycle_band(&state.lane);
    let (band_y, w, h, _) = band.geometry();
    Rect {
        x: col_x(band, state.col) - w * PX / 2,
        y: band_y * PX - h * PX / 2 + state.y_offset.unwrap_or(0) * PX,
        w: w * PX,
        h: h * PX,
    }
}

/// Lattice floors: the phase grid's right edge and the outcome band's
/// bottom, always reachable even when no state sits there.
fn floor_right() -> i32 {
    col_x(LifecycleBand::Phase, 4) + 590
}

fn floor_bottom() -> i32 {
    let g = LifecycleBand::Outcome.geometry();
    (g.0 + g.2 / 2) * PX
}

fn layout(doc: &Canon) -> Laid {
    let rects: Vec<Rect> = doc.states.iter().map(|&s| state_rect(s)).collect();
    let main_title = doc
        .lanes
        .iter()
        .find(|l| l.id == "main")
        .map(|l| l.label.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Lifecycle phases".to_string());
    let event_titles: Vec<&str> = doc
        .lanes
        .iter()
        .filter(|l| lifecycle_band(&l.id) == LifecycleBand::Event)
        .map(|l| l.label.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let event_title = if event_titles.is_empty() {
        "Interruptions + recovery".to_string()
    } else {
        event_titles.join(" + ")
    };
    let outcome_title = doc
        .lanes
        .iter()
        .find(|l| l.id == "terminal")
        .map(|l| l.label.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Outcomes".to_string());
    let band_titles = [main_title, event_title, outcome_title];
    let spine = doc
        .states
        .iter()
        .filter(|s| s.lane == "main")
        .map(|s| s.col)
        .min()
        .zip(
            doc.states
                .iter()
                .filter(|s| s.lane == "main")
                .map(|s| s.col)
                .max(),
        )
        .map(|(min_c, max_c)| {
            let start = col_x(LifecycleBand::Phase, min_c) + 590;
            let end = (col_x(LifecycleBand::Phase, max_c) + PHASE_SPINE_TAIL)
                .max(start + PHASE_SPINE_MIN_TAIL);
            (start, end)
        });
    // Provisional canvas from the lattice floors; the final viewBox is
    // measured after routing.
    let content_w = floor_right() + CANVAS_MARGIN;
    let content_h = floor_bottom() + CANVAS_BOTTOM_PAD;
    Laid {
        rects,
        band_titles,
        spine,
        content_w,
        content_h,
        legend_baseline: content_h - LEGEND_BASELINE_FROM_BOTTOM,
    }
}

/// (left, top, right, bottom) over states and placed routes/labels; rails
/// are decoration inside the canvas and never extend it.
fn measured_bounds(
    laid: &Laid,
    placed: &[(usize, usize, Vec<Pt>, Option<Rect>)],
) -> (i32, i32, i32, i32) {
    let mut left = RAIL_X;
    let mut top = i32::MAX;
    let mut right = i32::MIN;
    let mut bottom = i32::MIN;
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
    if top == i32::MAX {
        (left, RAIL_YS[0], right, RAIL_YS[2])
    } else {
        (left, top, right, bottom)
    }
}

// ---------------------------------------------------------------------------
// SVG emission
// ---------------------------------------------------------------------------

/// (fill, stroke) per state kind — the reference palette mapped onto the
/// theme's kind slots (start rides frontend, active cloud, waiting database,
/// decision messagebus, success backend, failure security; the unlabeled
/// fallback is the plain slot). External keeps its dashed border.
fn state_palette(theme: &Theme, kind: &str) -> (&'static str, &'static str) {
    let slot = match kind {
        "start" => "frontend",
        "active" => "cloud",
        "waiting" => "database",
        "decision" => "messagebus",
        "success" => "backend",
        "failure" => "security",
        "neutral" => "neutral",
        _ => "plain",
    };
    let colors = theme.node(slot);
    (colors.fill, colors.stroke)
}

/// (stroke, width, dash, label fill) per transition variant — emphasis
/// rides heavier (2px vs 1.1px) exactly like the reference. Sizes and dash
/// patterns are this adapter's typography; colors come from the theme.
fn variant_style(theme: &Theme, variant: &str) -> (&'static str, i32, Option<&'static str>, &'static str) {
    match variant {
        "emphasis" => (theme.ink, 20, None, theme.ink),
        "security" => (theme.danger, 11, None, theme.danger),
        "dashed" => (theme.skip, 11, Some("6 4"), theme.skip),
        _ => (theme.ink_soft, 11, None, theme.ink_soft),
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

    // Band rails + titles (dashed guides, behind everything).
    for (bi, &rail_y) in RAIL_YS.iter().enumerate() {
        s.push_str(&format!(
            "<line data-band=\"{}\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{}\" stroke-width=\"0.8\" stroke-dasharray=\"3 8\"/>",
            ["phase", "event", "outcome"][bi],
            tx(RAIL_X),
            tx(rail_y),
            tx(view_box[0] - RAIL_X),
            tx(rail_y),
            theme.guide
        ));
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"10\" font-weight=\"600\" fill=\"{}\">{:02} / {}</text>",
            tx(RAIL_X),
            tx(rail_y - 120),
            theme.ink_soft,
            bi + 1,
            esc(&laid.band_titles[bi])
        ));
    }

    // Phase spine along the phase-state bottoms.
    if let Some((start, end)) = laid.spine {
        s.push_str(&format!(
            "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{}\" stroke-width=\"2.2\"/>",
            tx(start),
            tx(1260 + 310),
            tx(end),
            tx(1260 + 310),
            theme.ink
        ));
    }

    // Transition paths + arrowheads.
    for (i, transition) in doc.transitions.iter().enumerate() {
        let (_, _, points, _) = &placed[i];
        let (stroke, stroke_w, dash, _) =
            variant_style(theme, transition.variant.as_deref().unwrap_or("default"));
        let dash_attr = dash.map_or(String::new(), |d| format!(" stroke-dasharray=\"{d}\""));
        s.push_str(&format!(
            "<path data-from=\"{}\" data-to=\"{}\" d=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{}\"{}/>",
            esc(&transition.from),
            esc(&transition.to),
            polyline_d(points),
            stroke,
            tx(stroke_w),
            dash_attr
        ));
        s.push_str(&arrowhead(points, stroke));
    }

    // States.
    for (i, state) in doc.states.iter().enumerate() {
        let rect = &laid.rects[i];
        let (fill, stroke) = state_palette(theme, &state.kind);
        let external = state.kind == "external";
        let dash_attr = if external {
            " stroke-dasharray=\"4 3\""
        } else {
            ""
        };
        s.push_str(&format!("<g data-node-id=\"{}\">", esc(&state.id)));
        s.push_str(&format!(
            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"7\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1.5\"{}/>",
            tx(rect.x),
            tx(rect.y),
            tx(rect.w),
            tx(rect.h),
            fill,
            stroke,
            dash_attr
        ));
        if let Some(step) = state.step.as_deref().filter(|s| !s.trim().is_empty()) {
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"7\" font-weight=\"700\" fill=\"{}\">{}</text>",
                tx(rect.x + 100),
                tx(rect.y + 140),
                theme.ink_muted,
                esc(step)
            ));
        }
        let label_font = fitted_font(&state.label, rect.w - 160, LABEL_PREFERRED, LABEL_MIN);
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"{}\" font-weight=\"600\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
            tx(rect.cx()),
            tx(rect.y + 340),
            tx(label_font),
            theme.ink,
            esc(&state.label)
        ));
        if let Some(sub) = state.sublabel.as_deref().filter(|s| !s.trim().is_empty()) {
            let sub_font = fitted_font(sub, rect.w, SUB_PREFERRED, SUB_MIN);
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
                tx(rect.cx()),
                tx(rect.y + 450),
                tx(sub_font),
                theme.ink_muted,
                esc(sub)
            ));
        }
        if let Some(tag) = state.tag.as_deref().filter(|t| !t.trim().is_empty()) {
            let tag_font = fitted_font(tag, rect.w, SUB_PREFERRED, SUB_MIN);
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
                tx(rect.cx()),
                tx(rect.y + rect.h - 100),
                tx(tag_font),
                theme.ink_muted,
                esc(tag)
            ));
        }
        s.push_str("</g>");
    }

    // Transition labels (on top: white mask + label, note beneath).
    for (i, transition) in doc.transitions.iter().enumerate() {
        let Some(w) = transition_label_width(
            transition.label.as_deref(),
            transition.note.as_deref(),
        ) else {
            continue;
        };
        let (_, _, points, _) = &placed[i];
        let at = label_point(points);
        let has_label = transition
            .label
            .as_deref()
            .is_some_and(|l| !l.trim().is_empty());
        let has_note = transition
            .note
            .as_deref()
            .is_some_and(|n| !n.trim().is_empty());
        let (_, _, _, label_fill) =
            variant_style(theme, transition.variant.as_deref().unwrap_or("default"));
        let h = if has_note { 300 } else { 160 };
        let label_line = has_label.then(|| {
            format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"8\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
                tx(at.0),
                tx(at.1 + 30),
                label_fill,
                esc(transition.label.as_deref().unwrap_or(""))
            )
        });
        let note_line = has_note.then(|| {
            format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"6\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
                tx(at.0),
                tx(at.1 + 180),
                theme.ink_muted,
                esc(transition.note.as_deref().unwrap_or(""))
            )
        });
        s.push_str(&format!(
            "<g data-from=\"{}\" data-to=\"{}\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"3\" fill=\"{}\"/>{}{}</g>",
            esc(&transition.from),
            esc(&transition.to),
            tx(at.0 - w / 2),
            tx(at.1 - 110),
            tx(w),
            tx(h),
            theme.panel_alt,
            label_line.as_deref().unwrap_or(""),
            note_line.as_deref().unwrap_or("")
        ));
    }

    // Legend: state kinds present, as node swatches.
    if !legend.is_empty() {
        let mut x = LEGEND_X;
        for &idx in legend {
            let (kind, label) = LEGEND_CATALOG[idx];
            let (fill, stroke) = state_palette(theme, kind);
            let dash_attr = if kind == "external" {
                " stroke-dasharray=\"4 3\""
            } else {
                ""
            };
            s.push_str(&format!(
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"2\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1\"{}/><text x=\"{}\" y=\"{}\" font-size=\"7\" fill=\"{}\">{}</text>",
                tx(x),
                tx(laid.legend_baseline - 80),
                tx(LEGEND_SWATCH_W),
                tx(LEGEND_SWATCH_H),
                fill,
                stroke,
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
        "title": "Deploy",
        "lanes": [
            {"id": "main", "label": "Release phases"},
            {"id": "ops", "label": "Ops"},
            {"id": "terminal", "label": "End states"}
        ],
        "states": [
            {"id": "p1", "type": "start", "label": "Build", "lane": "main", "col": 0, "step": "01"},
            {"id": "e1", "type": "waiting", "label": "Canary", "lane": "ops", "col": 1, "sublabel": "5% traffic"},
            {"id": "o1", "type": "success", "label": "Live", "lane": "terminal", "col": 1}
        ],
        "transitions": [
            {"from": "p1", "to": "e1", "label": "promote"},
            {"from": "e1", "to": "o1", "route": "drop", "label": "pass", "note": "after 15m"}
        ]
    }"#;

    fn spec() -> LifecycleSpec {
        let s: LifecycleSpec = serde_json::from_str(SPEC_JSON).unwrap();
        assert!(super::super::spec::validate_lifecycle(&s).is_empty());
        s
    }

    #[test]
    fn three_band_lifecycle_renders_deterministically() {
        let a = render_lifecycle(&spec(), &LIGHT).unwrap();
        let b = render_lifecycle(&spec(), &LIGHT).unwrap();
        assert_eq!(a.svg, b.svg, "same input, same bytes");
        assert_eq!((a.lanes, a.states, a.transitions), (3, 3, 2));
        assert!(a.repairs.is_empty(), "{:?}", a.repairs);
        // Rails: dashed guides with lane-derived titles.
        assert!(a.svg.contains("stroke-dasharray=\"3 8\""));
        assert!(a.svg.contains("01 / Release phases"));
        assert!(a.svg.contains("02 / Ops"));
        assert!(a.svg.contains("03 / End states"));
        assert!(a.svg.contains("data-node-id=\"e1\""));
        assert!(a.svg.contains("data-from=\"p1\" data-to=\"e1\""));
        // The phase spine rides the phase bottoms (157px).
        assert!(a.svg.contains("y1=\"157\""));
        // Legend lists the kinds present, not the whole catalog.
        assert!(a.svg.contains(">Start<"));
        assert!(a.svg.contains(">Waiting<"));
        assert!(a.svg.contains(">Success<"));
        assert!(!a.svg.contains(">Decision<"));
    }

    #[test]
    fn shuffled_collections_render_identical_bytes() {
        let shuffled: LifecycleSpec = serde_json::from_str(
            r#"{
                "title": "Deploy",
                "lanes": [
                    {"id": "terminal", "label": "End states"},
                    {"id": "main", "label": "Release phases"},
                    {"id": "ops", "label": "Ops"}
                ],
                "states": [
                    {"id": "o1", "type": "success", "label": "Live", "lane": "terminal", "col": 1},
                    {"id": "e1", "type": "waiting", "label": "Canary", "lane": "ops", "col": 1, "sublabel": "5% traffic"},
                    {"id": "p1", "type": "start", "label": "Build", "lane": "main", "col": 0, "step": "01"}
                ],
                "transitions": [
                    {"from": "e1", "to": "o1", "route": "drop", "label": "pass", "note": "after 15m"},
                    {"from": "p1", "to": "e1", "label": "promote"}
                ]
            }"#,
        )
        .unwrap();
        assert!(super::super::spec::validate_lifecycle(&shuffled).is_empty());
        // Lane order is authorial (band titles/numbering), so only the
        // states/transitions shuffle must erase — the doc orders differ,
        // compare against the same lane order instead.
        let same_lanes: LifecycleSpec = serde_json::from_str(
            r#"{
                "title": "Deploy",
                "lanes": [
                    {"id": "main", "label": "Release phases"},
                    {"id": "ops", "label": "Ops"},
                    {"id": "terminal", "label": "End states"}
                ],
                "states": [
                    {"id": "o1", "type": "success", "label": "Live", "lane": "terminal", "col": 1},
                    {"id": "e1", "type": "waiting", "label": "Canary", "lane": "ops", "col": 1, "sublabel": "5% traffic"},
                    {"id": "p1", "type": "start", "label": "Build", "lane": "main", "col": 0, "step": "01"}
                ],
                "transitions": [
                    {"from": "e1", "to": "o1", "route": "drop", "label": "pass", "note": "after 15m"},
                    {"from": "p1", "to": "e1", "label": "promote"}
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(
            render_lifecycle(&spec(), &LIGHT).unwrap().svg,
            render_lifecycle(&same_lanes, &LIGHT).unwrap().svg
        );
    }

    #[test]
    fn drop_between_bands_folds_to_the_vertical_dive() {
        // e1 (event col1) → o1 (outcome col1) share a column center (556),
        // so the drop preset's corridor run collapses and the dive renders
        // as one straight vertical — no repair needed.
        let rendered = render_lifecycle(&spec(), &LIGHT).unwrap();
        let path = rendered
            .svg
            .split("data-from=\"e1\" data-to=\"o1\"")
            .nth(1)
            .unwrap()
            .split("/>")
            .next()
            .unwrap();
        assert!(
            path.contains("M 556 307 L 556 421"),
            "drop dive missing: {path}"
        );
        assert!(rendered.repairs.is_empty());
    }
}
