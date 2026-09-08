//! Architecture family adapter: the deployment view over a row/col grid.
//!
//! Components are (row, col) cells on a lattice whose knobs (cols, cell
//! size, gaps) the author may tune; every coordinate is derived — the
//! zero-coordinate rule wins over the reference's free-placement escape
//! hatch, so authored `pos`/`size` are deliberately not part of the
//! contract. Boundaries are containment frames computed from `wraps`
//! member lists and painted behind everything; inter-boundary connections
//! cross them freely. Routing goes through the shared graph engine with
//! per-edge local corridors. The grid is fixed, so router feedback is
//! terminal — a diagnostic, never a loop — and an infeasible authored
//! preset walks the family's substitution ladder first (verified
//! substitutes only, every substitution disclosed).
//!
//! v1 divergences from the reference (kept deliberately): no boundary
//! label masking/push-up when a member sits under the label, no
//! title-composition convergence, no deployment-ownership nesting.

use std::collections::HashMap;

use super::graph::{
    label_point, label_rect, plan_grid_edge, polyline_d, Corridors, PlacedRoute, Pt, Rect,
    RouteError, RouteKind, RouteRepair, RouteRequest, RouteScene, Side, COLUMN_COUNT,
};
use super::spec::{text_units, ArchBoundary, ArchComponent, ArchConnection, ArchitectureSpec, ArchLayout};
use super::theme::Theme;
use super::tx;

// ---------------------------------------------------------------------------
// constants (archify px × 10)
// ---------------------------------------------------------------------------

const ORIGIN_X: i32 = 400;
const ORIGIN_Y: i32 = 800;
const DEFAULT_CELL_W: i32 = 1300;
const DEFAULT_CELL_H: i32 = 640;
const DEFAULT_GAP_X: i32 = 300;
const DEFAULT_GAP_Y: i32 = 400;
/// Boundary frame: 30px pad at the sides and below, 30px above (min 22px),
/// plus 20px of label room at the bottom.
const BOUNDARY_PAD: i32 = 300;
const BOUNDARY_TOP_PAD_MIN: i32 = 220;
const BOUNDARY_BOTTOM_EXTRA: i32 = 200;
/// Per-edge local corridors: 28px above the higher endpoint, 34px below the
/// deeper bottom, 36px outside each side.
const TOP_CLEAR: i32 = 280;
const BOTTOM_CLEAR: i32 = 340;
const SIDE_CLEAR: i32 = 360;
/// Connection label: w = max(32, units·4.9 + 12) px, single line.
const LABEL_WIDTH_FACTOR: i32 = 49;
const LABEL_WIDTH_PAD: i32 = 120;
const LABEL_WIDTH_MIN: i32 = 320;
/// Fonts (tenths): component label 10/8, sublabel/tag 7/6; connection
/// label 8. 0.6px advance per text unit per px of font size.
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
const CANVAS_MARGIN: i32 = 400;
const LEGEND_DROP: i32 = 440;

/// Legend catalog: component kinds present, in catalog order.
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
pub struct RenderedArchitecture {
    pub svg: String,
    pub title: String,
    /// ViewBox in px.
    pub view_box: [i32; 2],
    pub components: usize,
    pub boundaries: usize,
    pub connections: usize,
    /// Route presets the engine substituted, in canonical connection order.
    pub repairs: Vec<RouteRepair>,
}

/// The canonicalized document: components sort (row, col, id), connections
/// sort (id, from, to, label); boundaries keep document order for wrap
/// resolution and are re-sorted by computed frame in the layout.
struct Canon<'a> {
    title: &'a str,
    components: Vec<&'a ArchComponent>,
    boundaries: &'a [ArchBoundary],
    connections: Vec<&'a ArchConnection>,
    component_index: HashMap<&'a str, usize>,
}

fn canon<'a>(spec: &'a ArchitectureSpec) -> Canon<'a> {
    let mut components: Vec<&ArchComponent> = spec.components.iter().collect();
    components.sort_by(|a, b| {
        a.row
            .cmp(&b.row)
            .then(a.col.cmp(&b.col))
            .then(a.id.cmp(&b.id))
    });
    let mut connections: Vec<&ArchConnection> = spec.connections.iter().collect();
    connections.sort_by(|a, b| {
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
    });
    let component_index: HashMap<&'a str, usize> = components
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.as_str(), i))
        .collect();
    Canon {
        title: spec.title.trim(),
        components,
        boundaries: &spec.boundaries,
        connections,
        component_index,
    }
}

/// Render a validated architecture spec. Infeasibility is terminal (the
/// grid is fixed): the connection names its dead end and the caller falls
/// the fence back to a code block.
pub fn render_architecture(
    spec: &ArchitectureSpec,
    theme: &'static Theme,
) -> Result<RenderedArchitecture, Vec<String>> {
    let doc = canon(spec);
    let laid = layout(&doc, &spec.layout);

    let mut placed: Vec<(usize, usize, Vec<Pt>, Option<Rect>)> = Vec::new();
    let mut repairs: Vec<RouteRepair> = Vec::new();
    for connection in &doc.connections {
        let &from_idx = doc
            .component_index
            .get(connection.from.as_str())
            .expect("validated: from resolves");
        let &to_idx = doc
            .component_index
            .get(connection.to.as_str())
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
            // Boundaries are background frames, not obstacles: crossing a
            // region reads as traversing it, which is the point.
            obstacles: &[],
            placed: &scene_placed,
            canvas: (laid.content_w, laid.content_h),
        };
        let label_w = connection
            .label
            .as_deref()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                (text_units(l) as i32 * LABEL_WIDTH_FACTOR + LABEL_WIDTH_PAD).max(LABEL_WIDTH_MIN)
            });
        let (from, to) = (&laid.rects[from_idx], &laid.rects[to_idx]);
        let req = RouteRequest {
            from_idx,
            to_idx,
            from,
            to,
            // col_xs is diagnostics-only (feedback is terminal here), so
            // clamp into the lattice view.
            from_col: doc.components[from_idx].col.min(5),
            to_col: doc.components[to_idx].col.min(5),
            forward: doc.components[to_idx].col > doc.components[from_idx].col,
            cross_lane: false,
            lane_gap: 200,
            label_width: label_w,
            from_side_authored: connection.from_side.as_deref().and_then(Side::parse),
            to_side_authored: connection.to_side.as_deref().and_then(Side::parse),
            route: match connection.route.as_deref() {
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
            col_xs: grid_col_xs(laid.pitch_x),
            primary_ports: None,
        };
        match plan_grid_edge(req, &scene, ladder) {
            Ok((planned, substituted)) => {
                if let Some((requested, sub)) = substituted {
                    repairs.push(RouteRepair {
                        edge: connection_name(connection),
                        requested: requested.to_string(),
                        substituted: sub.to_string(),
                    });
                }
                let label = label_w.map(|w| label_rect(w, label_point(&planned.points)));
                placed.push((from_idx, to_idx, planned.points, label));
            }
            Err(RouteError::PresetConflict { preset }) => {
                return Err(vec![format!(
                    "connection \"{}\" requests route \"{preset}\" but no verified substitute \
                     fits — move its components to other cells, or loosen the route",
                    connection_name(connection)
                )])
            }
            Err(_) => {
                return Err(vec![format!(
                    "connection \"{}\" has no feasible route between its components — spread \
                     them across cells, or shorten the label",
                    connection_name(connection)
                )])
            }
        }
    }

    let bounds = measured_bounds(&laid, &placed);
    if bounds.0 < 0 || bounds.1 < 0 {
        return Err(vec![
            "architecture geometry extends above or left of the viewBox origin — the grid origin \
             or a boundary pad underflowed"
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
        .filter(|(_, (kind, _))| doc.components.iter().any(|c| c.kind == *kind))
        .map(|(i, _)| i)
        .collect();

    let svg = emit_svg(&doc, &laid, &placed, view_box, &legend, theme);
    Ok(RenderedArchitecture {
        svg,
        title: doc.title.to_string(),
        view_box: [view_box[0] / 10, view_box[1] / 10],
        components: doc.components.len(),
        boundaries: doc.boundaries.len(),
        connections: doc.connections.len(),
        repairs,
    })
}

/// Semantic substitution ladder for an infeasible preset: keep the
/// connection's reading (an L stays an L, just turned) while dropping the
/// exact shape that cannot be honored. First verified substitute wins.
fn ladder(route: &str) -> &'static [&'static str] {
    match route {
        "straight" => &["auto"],
        "orthogonal-h" => &["auto", "orthogonal-v"],
        "orthogonal-v" => &["auto", "orthogonal-h"],
        _ => &[],
    }
}

fn connection_name(connection: &ArchConnection) -> String {
    connection
        .id
        .clone()
        .unwrap_or_else(|| format!("{}->{}", connection.from, connection.to))
}

// ---------------------------------------------------------------------------
// grid layout
// ---------------------------------------------------------------------------

fn grid_col_xs(pitch_x: i32) -> [i32; COLUMN_COUNT] {
    let mut xs = [0i32; COLUMN_COUNT];
    for (i, x) in xs.iter_mut().enumerate() {
        *x = ORIGIN_X + i as i32 * pitch_x;
    }
    xs
}

struct LaidBoundary {
    rect: Rect,
    label: String,
    security: bool,
}

struct Laid {
    pitch_x: i32,
    rects: Vec<Rect>,
    boundaries: Vec<LaidBoundary>,
    content_w: i32,
    content_h: i32,
    legend_baseline: i32,
}

fn layout(doc: &Canon, spec_layout: &Option<ArchLayout>) -> Laid {
    // Authored knobs are px (×10 into tenths); the defaults above are already
    // tenths.
    let cell_w = spec_layout
        .as_ref()
        .and_then(|l| l.cell_w)
        .map_or(DEFAULT_CELL_W, |px| px * 10);
    let cell_h = spec_layout
        .as_ref()
        .and_then(|l| l.cell_h)
        .map_or(DEFAULT_CELL_H, |px| px * 10);
    let gap_x = spec_layout
        .as_ref()
        .and_then(|l| l.gap_x)
        .map_or(DEFAULT_GAP_X, |px| px * 10);
    let gap_y = spec_layout
        .as_ref()
        .and_then(|l| l.gap_y)
        .map_or(DEFAULT_GAP_Y, |px| px * 10);
    let pitch_x = cell_w + gap_x;
    let pitch_y = cell_h + gap_y;
    let rects: Vec<Rect> = doc
        .components
        .iter()
        .map(|c| Rect {
            x: ORIGIN_X + c.col as i32 * pitch_x,
            y: ORIGIN_Y + c.row as i32 * pitch_y,
            w: cell_w,
            h: cell_h,
        })
        .collect();

    // Boundary frames wrap their members, label room included.
    let mut boundaries: Vec<LaidBoundary> = Vec::new();
    for boundary in doc.boundaries {
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for id in &boundary.wraps {
            if let Some(&idx) = doc.component_index.get(id.as_str()) {
                let r = &rects[idx];
                min_x = min_x.min(r.x);
                min_y = min_y.min(r.y);
                max_x = max_x.max(r.right());
                max_y = max_y.max(r.bottom());
            }
        }
        if min_x == i32::MAX {
            continue; // validated away; skip defensively
        }
        let pad = boundary.pad.unwrap_or(BOUNDARY_PAD / 10) * 10;
        let top_pad = pad.max(BOUNDARY_TOP_PAD_MIN);
        boundaries.push(LaidBoundary {
            rect: Rect {
                x: min_x - pad,
                y: min_y - top_pad,
                w: max_x - min_x + 2 * pad,
                h: max_y - min_y + top_pad + BOUNDARY_BOTTOM_EXTRA,
            },
            label: boundary.label.trim().to_string(),
            security: boundary.kind == "security-group",
        });
    }
    boundaries.sort_by(|a, b| {
        a.rect
            .x
            .cmp(&b.rect.x)
            .then(a.rect.y.cmp(&b.rect.y))
            .then(a.label.cmp(&b.label))
    });

    // Provisional canvas from the lattice; the final viewBox is measured
    // after routing.
    let content_right = boundaries
        .iter()
        .map(|b| b.rect.right())
        .chain(rects.iter().map(Rect::right))
        .max()
        .unwrap_or(ORIGIN_X + cell_w);
    let content_bottom = boundaries
        .iter()
        .map(|b| b.rect.bottom())
        .chain(rects.iter().map(Rect::bottom))
        .max()
        .unwrap_or(ORIGIN_Y + cell_h);
    let legend_baseline = content_bottom + LEGEND_DROP;
    Laid {
        pitch_x,
        rects,
        boundaries,
        content_w: content_right + CANVAS_MARGIN,
        content_h: legend_baseline + LEGEND_DROP,
        legend_baseline,
    }
}

/// (left, top, right, bottom) over components, boundaries, and placed
/// routes/labels.
fn measured_bounds(
    laid: &Laid,
    placed: &[(usize, usize, Vec<Pt>, Option<Rect>)],
) -> (i32, i32, i32, i32) {
    let mut left = i32::MAX;
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
    for boundary in &laid.boundaries {
        include(
            boundary.rect.x,
            boundary.rect.y,
            &mut left,
            &mut top,
            &mut right,
            &mut bottom,
        );
        include(
            boundary.rect.right(),
            boundary.rect.bottom(),
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
    if left == i32::MAX {
        (ORIGIN_X, ORIGIN_Y, ORIGIN_X, ORIGIN_Y)
    } else {
        (left, top, right, bottom)
    }
}

// ---------------------------------------------------------------------------
// SVG emission
// ---------------------------------------------------------------------------

/// (stroke, width, dash, label fill) per connection variant — the sequence
/// table minus `return`, which is sequence/workflow vocabulary. Sizes and
/// dash patterns are this adapter's typography; colors come from the theme.
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

    // Boundary frames (behind everything: connections cross them freely).
    for boundary in &laid.boundaries {
        let (fill, stroke) = if boundary.security {
            (theme.panel_danger, theme.danger)
        } else {
            (theme.panel, theme.guide)
        };
        let dash_attr = if boundary.security {
            ""
        } else {
            " stroke-dasharray=\"6 4\""
        };
        s.push_str(&format!(
            "<rect data-boundary-label=\"{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"9\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1\"{}/>",
            esc(&boundary.label),
            tx(boundary.rect.x),
            tx(boundary.rect.y),
            tx(boundary.rect.w),
            tx(boundary.rect.h),
            fill,
            stroke,
            dash_attr
        ));
    }

    // Connection paths + arrowheads.
    for (i, connection) in doc.connections.iter().enumerate() {
        let (_, _, points, _) = &placed[i];
        let (stroke, stroke_w, dash, _) =
            variant_style(theme, connection.variant.as_deref().unwrap_or("default"));
        let dash_attr = dash.map_or(String::new(), |d| format!(" stroke-dasharray=\"{d}\""));
        s.push_str(&format!(
            "<path data-from=\"{}\" data-to=\"{}\" d=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{}\"{}/>",
            esc(&connection.from),
            esc(&connection.to),
            polyline_d(points),
            stroke,
            tx(stroke_w),
            dash_attr
        ));
        s.push_str(&arrowhead(points, stroke));
    }

    // Components.
    for (i, component) in doc.components.iter().enumerate() {
        let rect = &laid.rects[i];
        let component_colors = theme.node(&component.kind);
        s.push_str(&format!("<g data-node-id=\"{}\">", esc(&component.id)));
        s.push_str(&format!(
            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"6\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1.5\"/>",
            tx(rect.x),
            tx(rect.y),
            tx(rect.w),
            tx(rect.h),
            component_colors.fill,
            component_colors.stroke
        ));
        let label_font = fitted_font(&component.label, rect.w - 160, LABEL_PREFERRED, LABEL_MIN);
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"{}\" font-weight=\"600\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
            tx(rect.cx()),
            tx(rect.y + 340),
            tx(label_font),
            theme.ink,
            esc(&component.label)
        ));
        if let Some(sub) = component.sublabel.as_deref().filter(|s| !s.trim().is_empty()) {
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
        if let Some(tag) = component.tag.as_deref().filter(|t| !t.trim().is_empty()) {
            let tag_font = fitted_font(tag, rect.w, SUB_PREFERRED, SUB_MIN);
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"{}\" text-anchor=\"middle\">{}</text>",
                tx(rect.cx()),
                tx(rect.y + rect.h - 100),
                tx(tag_font),
                component_colors.text,
                esc(tag)
            ));
        }
        s.push_str("</g>");
    }

    // Connection labels (on top: white mask + label).
    for (i, connection) in doc.connections.iter().enumerate() {
        let Some(label) = connection.label.as_deref().filter(|l| !l.trim().is_empty()) else {
            continue;
        };
        let (_, _, points, _) = &placed[i];
        let at = label_point(points);
        let w = (text_units(label) as i32 * LABEL_WIDTH_FACTOR + LABEL_WIDTH_PAD).max(LABEL_WIDTH_MIN);
        let (_, _, _, label_fill) =
            variant_style(theme, connection.variant.as_deref().unwrap_or("default"));
        s.push_str(&format!(
            "<g data-from=\"{}\" data-to=\"{}\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"16\" rx=\"3\" fill=\"{}\"/><text x=\"{}\" y=\"{}\" font-size=\"8\" fill=\"{}\" text-anchor=\"middle\">{}</text></g>",
            esc(&connection.from),
            esc(&connection.to),
            tx(at.0 - w / 2),
            tx(at.1 - 110),
            tx(w),
            theme.panel_alt,
            tx(at.0),
            tx(at.1 + 30),
            label_fill,
            esc(label)
        ));
    }

    // Boundary labels last, so lines never strike them out.
    for boundary in &laid.boundaries {
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"7\" font-weight=\"600\" fill=\"{}\">{}</text>",
            tx(boundary.rect.x + 100),
            tx(boundary.rect.y + 160),
            if boundary.security { theme.danger } else { theme.ink_soft },
            esc(&boundary.label)
        ));
    }

    // Legend: component kinds present, as node swatches.
    if !legend.is_empty() {
        let mut x = LEGEND_X;
        for &idx in legend {
            let (kind, label) = LEGEND_CATALOG[idx];
            let swatch = theme.node(kind);
            s.push_str(&format!(
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"2\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1\"/><text x=\"{}\" y=\"{}\" font-size=\"7\" fill=\"{}\">{}</text>",
                tx(x),
                tx(laid.legend_baseline - 80),
                tx(LEGEND_SWATCH_W),
                tx(LEGEND_SWATCH_H),
                swatch.fill,
                swatch.stroke,
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
        "title": "Deployment",
        "components": [
            {"id": "web", "type": "frontend", "label": "Web", "row": 0, "col": 0},
            {"id": "cdn", "type": "cloud", "label": "CDN", "row": 0, "col": 1},
            {"id": "db", "type": "database", "label": "DB", "row": 0, "col": 2},
            {"id": "api", "type": "backend", "label": "API", "row": 1, "col": 2}
        ],
        "boundaries": [
            {"kind": "security-group", "label": "Edge", "wraps": ["web"]},
            {"kind": "region", "label": "VPC", "wraps": ["api", "db"]}
        ],
        "connections": [
            {"from": "web", "to": "cdn", "route": "orthogonal-h"},
            {"from": "web", "to": "api", "label": "https"},
            {"from": "cdn", "to": "api", "label": "cache miss"},
            {"from": "api", "to": "db", "label": "sql", "variant": "dashed"}
        ]
    }"#;

    fn spec() -> ArchitectureSpec {
        let s: ArchitectureSpec = serde_json::from_str(SPEC_JSON).unwrap();
        assert!(super::super::spec::validate_architecture(&s).is_empty());
        s
    }

    #[test]
    fn deployment_grid_renders_deterministically() {
        let a = render_architecture(&spec(), &LIGHT).unwrap();
        let b = render_architecture(&spec(), &LIGHT).unwrap();
        assert_eq!(a.svg, b.svg, "same input, same bytes");
        assert_eq!((a.components, a.boundaries, a.connections), (4, 2, 4));
        assert!(a.repairs.is_empty(), "{:?}", a.repairs);
        // Boundaries paint behind, carry their label, and the security one
        // is solid red while the region is dashed.
        assert!(a.svg.contains("data-boundary-label=\"VPC\""));
        assert!(a.svg.contains("data-boundary-label=\"Edge\""));
        assert!(a.svg.contains("stroke-dasharray=\"6 4\""));
        assert!(a.svg.contains("data-node-id=\"web\""));
        assert!(a.svg.contains("data-from=\"web\" data-to=\"api\""));
        // VPC right edge 520px + 40px margin → 560px wide; content bottom
        // 268px + legend 44+44 → 356px tall.
        assert_eq!(a.view_box, [560, 356], "{:?}", a.view_box);
        // Legend lists the four kinds present, not the whole catalog.
        assert!(a.svg.contains(">Frontend<"));
        assert!(a.svg.contains(">Backend<"));
        assert!(a.svg.contains(">Database<"));
        assert!(a.svg.contains(">Cloud<"));
        assert!(!a.svg.contains(">Message bus<"));
    }

    #[test]
    fn shuffled_collections_render_identical_bytes() {
        let shuffled: ArchitectureSpec = serde_json::from_str(
            r#"{
                "title": "Deployment",
                "components": [
                    {"id": "api", "type": "backend", "label": "API", "row": 1, "col": 2},
                    {"id": "db", "type": "database", "label": "DB", "row": 0, "col": 2},
                    {"id": "cdn", "type": "cloud", "label": "CDN", "row": 0, "col": 1},
                    {"id": "web", "type": "frontend", "label": "Web", "row": 0, "col": 0}
                ],
                "boundaries": [
                    {"kind": "region", "label": "VPC", "wraps": ["api", "db"]},
                    {"kind": "security-group", "label": "Edge", "wraps": ["web"]}
                ],
                "connections": [
                    {"from": "api", "to": "db", "label": "sql", "variant": "dashed"},
                    {"from": "cdn", "to": "api", "label": "cache miss"},
                    {"from": "web", "to": "api", "label": "https"},
                    {"from": "web", "to": "cdn", "route": "orthogonal-h"}
                ]
            }"#,
        )
        .unwrap();
        assert!(super::super::spec::validate_architecture(&shuffled).is_empty());
        assert_eq!(
            render_architecture(&spec(), &LIGHT).unwrap().svg,
            render_architecture(&shuffled, &LIGHT).unwrap().svg
        );
    }

    #[test]
    fn orthogonal_h_honors_the_single_bend() {
        // web → cdn are row neighbors: the orthogonal-h preset collapses to
        // one straight horizontal segment (no intermediate bend points).
        let rendered = render_architecture(&spec(), &LIGHT).unwrap();
        let path = rendered
            .svg
            .split("data-from=\"web\" data-to=\"cdn\"")
            .nth(1)
            .unwrap()
            .split("/>")
            .next()
            .unwrap();
        assert!(
            path.contains("M 170 112 L 200 112"),
            "orthogonal-h shape missing: {path}"
        );
        assert!(rendered.repairs.is_empty());
    }
}
