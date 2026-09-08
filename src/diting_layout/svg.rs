//! svg rendering v1 (AginxOS svg zoom batch).
//!
//! The engine had zero svg code — an inline `<svg>` leaked its children as
//! flow text (the shapes were never pixels; layout probes reporting
//! "component boxes" were the only testimony). This module compiles the svg
//! subtree at collect time into a flat op list in viewBox user units, and
//! paints it into the element's replaced box at paint time. Element box size
//! comes from CSS/attributes like any replaced leaf; the viewBox→box mapping
//! IS the zoom semantics the device report asked for (pinch-zoom = scale the
//! element, the diagram scales with it).
//!
//! v1 scope (the archify artifact shape): rect / circle / ellipse / line /
//! polyline / polygon / path (M L Q C H V Z; A degrades to a line) / text /
//! g transforms (translate scale rotate matrix, flattened at compile) /
//! marker-end + marker-start triangles / presentation attributes with the
//! spec's own-CSS > own-attr > inherited priority / CSS `fill`/`stroke`
//! through the normal cascade (var() included) / stroke-dasharray /
//! viewBox xMidYMid meet. Hard edges, no antialiasing — same stance as the
//! rest of the raster painter. Out of scope: gradients, patterns, clipPath,
//! masks, filters, nested viewBox, tspan positioning, fill-rule distinction
//! (everything is even-odd).

use std::collections::HashMap;

use crate::diting_css::{Color, ComputedStyle, Display, SvgPaint};
use crate::diting_dom::tree::{DomTree, NodeId};

use super::paint::Canvas;
use super::{FontBook, Rect};

/// A compiled svg: the viewBox (None = user units map 1:1 from the element
/// box origin) plus the flattened op list in user units. Group transforms
/// are already baked into every coordinate at compile time, so paint is a
/// pure affine map away.
#[derive(Debug, Clone, Default)]
pub struct SvgRender {
    pub view_box: Option<(f32, f32, f32, f32)>,
    pub ops: Vec<SvgOp>,
}

/// One paint op, self-contained. Geometry is in viewBox user units with all
/// ancestor transforms pre-applied; colors are fully resolved (currentColor
/// folded against the element's computed `color`).
#[derive(Debug, Clone)]
pub enum SvgOp {
    /// Even-odd fill over one or more subpaths (the fill-rule distinction
    /// is out of scope; simple shapes agree under both rules).
    Fill { polys: Vec<Vec<(f32, f32)>>, color: [u8; 4] },
    /// Polyline stroke; `closed` adds the closing segment.
    Stroke {
        poly: Vec<(f32, f32)>,
        closed: bool,
        width: f32,
        dash: Option<Vec<f32>>,
        color: [u8; 4],
    },
    Text {
        content: String,
        /// Baseline origin (svg `y` IS the baseline).
        x: f32,
        y: f32,
        anchor: TextAnchor,
        font_size: f32,
        bold: bool,
        color: [u8; 4],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAnchor {
    Start,
    Middle,
    End,
}

/// Elements that carry shapes but never paint themselves (their children are
/// defs/references or metadata). Anything NOT in this list and not a shape
/// recurses like `<g>`.
fn is_non_visual(tag: &str) -> bool {
    matches!(
        tag,
        "title"
            | "desc"
            | "style"
            | "script"
            | "metadata"
            | "defs"
            | "symbol"
            | "clipPath"
            | "mask"
            | "pattern"
            | "linearGradient"
            | "radialGradient"
            | "filter"
            | "marker"
            | "animate"
            | "animateTransform"
            | "use"
    )
}

// ---------------------------------------------------------------------------
// Compile: DOM subtree → op list
// ---------------------------------------------------------------------------

/// Compile the subtree of an `<svg>` element into renderable ops.
pub fn compile_svg(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    root: NodeId,
) -> SvgRender {
    let view_box = tree
        .with_node(root, |n| n.get_attribute("viewBox").map(str::to_string))
        .flatten()
        .and_then(|v| parse_view_box(&v));
    let mut markers = HashMap::new();
    collect_markers(tree, styles, root, &mut markers);
    let mut out = SvgRender { view_box, ops: Vec::new() };
    // Root context: nothing declared = unset (SVG initial values apply at
    // draw time); font size tracks the CSS cascade through the HTML chain.
    let ctx = Ctx {
        m: IDENTITY,
        fill: None,
        stroke: None,
        stroke_w: None,
        dash: None,
        font_size: styles.get(&root).and_then(|s| s.font_size).unwrap_or(16.0),
        bold: styles.get(&root).and_then(|s| s.font_weight).is_some_and(|w| w >= 600),
        opacity: 1.0,
    };
    walk(tree, styles, root, ctx, &markers, &mut out.ops);
    out
}

/// viewBox attr parser, shared with the replaced-leaf sizing arm.
pub fn parse_view_box(v: &str) -> Option<(f32, f32, f32, f32)> {
    let nums: Vec<f32> = v
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<f32>().ok())
        .collect();
    match nums.as_slice() {
        [x, y, w, h] if *w > 0.0 && *h > 0.0 => Some((*x, *y, *w, *h)),
        _ => None,
    }
}

/// Inherited-and-transform context for the compile walk.
#[derive(Clone)]
struct Ctx {
    /// Accumulated affine [a b c d e f]: p' = (a·x + c·y + e, b·x + d·y + f).
    m: [f32; 6],
    fill: Option<SvgPaint>,
    stroke: Option<SvgPaint>,
    stroke_w: Option<f32>,
    dash: Option<Vec<f32>>,
    font_size: f32,
    bold: bool,
    /// Accumulated group `opacity` (ancestor product). SVG composites a
    /// group's opacity as a unit; multiplying per element is the flat
    /// approximation — identical whenever one factor covers the subtree,
    /// which is how authored artifacts and dimming viewers use it.
    opacity: f32,
}

const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

fn apply(m: &[f32; 6], p: (f32, f32)) -> (f32, f32) {
    (m[0] * p.0 + m[2] * p.1 + m[4], m[1] * p.0 + m[3] * p.1 + m[5])
}

/// Compose parent ∘ child (child's transform applies first).
fn mul(m: &[f32; 6], n: &[f32; 6]) -> [f32; 6] {
    [
        m[0] * n[0] + m[2] * n[1],
        m[1] * n[0] + m[3] * n[1],
        m[0] * n[2] + m[2] * n[3],
        m[1] * n[2] + m[3] * n[3],
        m[0] * n[4] + m[2] * n[5] + m[4],
        m[1] * n[4] + m[3] * n[5] + m[5],
    ]
}

/// Uniform scale factor of a transform (stroke widths and font sizes ride it).
fn scale_of(m: &[f32; 6]) -> f32 {
    ((m[0] * m[3] - m[1] * m[2]).abs()).sqrt()
}

fn attr_f(tree: &DomTree, id: NodeId, name: &str) -> Option<f32> {
    let raw = tree.with_node(id, |n| n.get_attribute(name).map(str::to_string)).flatten()?;
    num(&raw)
}

/// SVG lengths: bare numbers and `px` (other units fall back to the number —
/// good enough for the artifacts that actually carry units).
fn num(v: &str) -> Option<f32> {
    let v = v.trim().trim_end_matches("px").trim();
    v.parse::<f32>().ok().filter(|n| n.is_finite())
}

/// Attr paint: `none` = transparent, `currentColor` symbolic, else a color.
fn attr_paint(v: &str) -> Option<SvgPaint> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("none") {
        return Some(SvgPaint::Color(Color(0, 0, 0, 0)));
    }
    if v.eq_ignore_ascii_case("currentColor") {
        return Some(SvgPaint::CurrentColor);
    }
    crate::diting_css::parse_color(v).map(SvgPaint::Color)
}

fn rgba(p: SvgPaint, cs_color: Option<Color>) -> [u8; 4] {
    let c = match p {
        SvgPaint::Color(c) => c,
        SvgPaint::CurrentColor => cs_color.unwrap_or(Color(0, 0, 0, 255)),
    };
    [c.0, c.1, c.2, c.3]
}

fn walk(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    id: NodeId,
    ctx: Ctx,
    markers: &HashMap<String, MarkerDef>,
    ops: &mut Vec<SvgOp>,
) {
    let cs = styles.get(&id).cloned().unwrap_or_default();
    if cs.display == Some(Display::None) {
        return;
    }
    let Some(tag) = tree.with_node(id, |n| n.as_element().map(|e| e.local.to_string())).flatten()
    else {
        return;
    };
    if is_non_visual(&tag) {
        return;
    }

    // Transform attr composes onto the accumulated matrix.
    let tf = tree
        .with_node(id, |n| n.get_attribute("transform").map(str::to_string))
        .flatten()
        .and_then(|t| parse_transform(&t))
        .unwrap_or(IDENTITY);
    let m = mul(&ctx.m, &tf);
    let k = scale_of(&m);

    // Paint priority per spec: own CSS declaration > presentation attr >
    // inherited. The CSS layer keeps svg_* NON-inherited precisely so an
    // "own declaration" stays distinguishable here.
    let fill = cs.svg_fill.or_else(|| {
        tree.with_node(id, |n| n.get_attribute("fill").map(str::to_string))
            .flatten()
            .and_then(|v| attr_paint(&v))
    });
    let stroke = cs.svg_stroke.or_else(|| {
        tree.with_node(id, |n| n.get_attribute("stroke").map(str::to_string))
            .flatten()
            .and_then(|v| attr_paint(&v))
    });
    let stroke_w = cs
        .svg_stroke_width
        .or_else(|| attr_f(tree, id, "stroke-width"))
        .or(ctx.stroke_w);
    let dash = cs.svg_dasharray.or_else(|| {
        tree.with_node(id, |n| n.get_attribute("stroke-dasharray").map(str::to_string))
            .flatten()
            .and_then(|v| parse_dasharray(&v))
    });
    // font-size: the ComputedStyle value conflates own-CSS with inherited-CSS,
    // so the attribute is checked FIRST — spec-wrong only when one element
    // declares both (authors don't), spec-right for attr-over-inherited.
    // ctx.font_size already carries ancestor transforms' scale; multiplying
    // by k applies only this element's incremental transform.
    let font_size = attr_f(tree, id, "font-size")
        .or(cs.font_size)
        .unwrap_or(ctx.font_size)
        * k;
    let attr_bold = tree
        .with_node(id, |n| {
            n.get_attribute("font-weight").map(|v| {
                v.trim().eq_ignore_ascii_case("bold")
                    || v.trim().parse::<u16>().is_ok_and(|w| w >= 600)
            })
        })
        .flatten()
        .unwrap_or(false);
    let bold = attr_bold || cs.font_weight.is_some_and(|w| w >= 600) || ctx.bold;

    // Opacity: `opacity` composes down the subtree (accumulated in Ctx);
    // `fill-opacity`/`stroke-opacity` scale only this element's own paints.
    // All three clamp to [0, 1]; anything unparsable falls back to opaque.
    let clamp01 = |v: Option<f32>| v.filter(|o| o.is_finite()).unwrap_or(1.0).clamp(0.0, 1.0);
    let group_op = clamp01(attr_f(tree, id, "opacity"));
    let fill_op = clamp01(attr_f(tree, id, "fill-opacity"));
    let stroke_op = clamp01(attr_f(tree, id, "stroke-opacity"));

    let next = Ctx {
        m,
        fill: fill.or(ctx.fill),
        stroke: stroke.or(ctx.stroke),
        stroke_w,
        dash: dash.clone().or(ctx.dash),
        font_size,
        bold,
        opacity: ctx.opacity * group_op,
    };

    // Resolve paints for THIS element's shapes. Draw-time defaults are the
    // SVG initial values: fill = opaque black, stroke = none ("none" itself
    // folds to alpha 0 and the emitters filter it out).
    let dim_fill = |mut c: [u8; 4]| {
        c[3] = (c[3] as f32 * next.opacity * fill_op).round() as u8;
        c
    };
    let dim_stroke = |mut c: [u8; 4]| {
        c[3] = (c[3] as f32 * next.opacity * stroke_op).round() as u8;
        c
    };
    let fill_rgba = Some(
        dim_fill(next.fill.map(|p| rgba(p, cs.color)).unwrap_or([0, 0, 0, 255])),
    );
    let stroke_rgba = next
        .stroke
        .map(|p| rgba(p, cs.color))
        .map(dim_stroke);
    let sw = stroke_w.unwrap_or(1.0);

    let map_all = |pts: &[(f32, f32)]| pts.iter().map(|&p| apply(&m, p)).collect::<Vec<_>>();

    match tag.as_str() {
        "rect" => {
            let (x, y) = (attr_f(tree, id, "x").unwrap_or(0.0), attr_f(tree, id, "y").unwrap_or(0.0));
            let (w, h) = (attr_f(tree, id, "width").unwrap_or(0.0), attr_f(tree, id, "height").unwrap_or(0.0));
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            let mut rx = attr_f(tree, id, "rx").unwrap_or(0.0);
            let mut ry = attr_f(tree, id, "ry").unwrap_or(rx);
            if ry == 0.0 && rx > 0.0 {
                ry = rx;
            }
            if rx == 0.0 && ry > 0.0 {
                rx = ry;
            }
            rx = rx.clamp(0.0, w / 2.0);
            ry = ry.clamp(0.0, h / 2.0);
            let poly = rounded_rect_poly(x, y, w, h, rx, ry);
            emit_fill_stroke(map_all(&poly), true, fill_rgba, stroke_rgba, sw, dash.as_ref(), ops);
        }
        "circle" | "ellipse" => {
            let (cx, cy) = (attr_f(tree, id, "cx").unwrap_or(0.0), attr_f(tree, id, "cy").unwrap_or(0.0));
            let (rx, ry) = if tag == "circle" {
                let r = attr_f(tree, id, "r").unwrap_or(0.0);
                (r, r)
            } else {
                (
                    attr_f(tree, id, "rx").unwrap_or(0.0),
                    attr_f(tree, id, "ry").unwrap_or(0.0),
                )
            };
            if rx <= 0.0 || ry <= 0.0 {
                return;
            }
            // Unit circle mapped through the shape radii then the transform —
            // non-uniform group transforms stay correct.
            let pts: Vec<(f32, f32)> = unit_circle(48)
                .iter()
                .map(|&(ux, uy)| apply(&m, (cx + ux * rx, cy + uy * ry)))
                .collect();
            emit_fill_stroke(pts, true, fill_rgba, stroke_rgba, sw, dash.as_ref(), ops);
        }
        "line" => {
            let p1 = (attr_f(tree, id, "x1").unwrap_or(0.0), attr_f(tree, id, "y1").unwrap_or(0.0));
            let p2 = (attr_f(tree, id, "x2").unwrap_or(0.0), attr_f(tree, id, "y2").unwrap_or(0.0));
            let mapped = vec![apply(&m, p1), apply(&m, p2)];
            emit_stroke_if_any(mapped.clone(), false, stroke_rgba, sw, dash.as_ref(), ops);
            emit_markers(tree, id, markers, &[mapped], sw, ops);
        }
        "polyline" | "polygon" => {
            let closed = tag == "polygon";
            let pts = tree
                .with_node(id, |n| n.get_attribute("points").map(str::to_string))
                .flatten()
                .map(|v| parse_points(&v))
                .unwrap_or_default();
            if pts.len() < 2 {
                return;
            }
            let mapped = map_all(&pts);
            if closed {
                emit_fill_stroke(mapped.clone(), true, fill_rgba, stroke_rgba, sw, dash.as_ref(), ops);
            } else {
                emit_stroke_if_any(mapped.clone(), false, stroke_rgba, sw, dash.as_ref(), ops);
            }
            emit_markers(tree, id, markers, &[mapped], sw, ops);
        }
        "path" => {
            let d = tree
                .with_node(id, |n| n.get_attribute("d").map(str::to_string))
                .flatten()
                .unwrap_or_default();
            let subpaths = parse_path(&d, &m);
            if subpaths.is_empty() {
                return;
            }
            let all: Vec<Vec<(f32, f32)>> = subpaths.iter().map(|s| s.pts.clone()).collect();
            // A stroke-only path (open, 2-point subpaths) has no fillable
            // area — skip the degenerate Fill op so marker Fills stay
            // identifiable and the op list stays paint-only.
            if let Some(c) = fill_rgba.filter(|c| c[3] > 0) {
                if all.iter().any(|p| p.len() >= 3) {
                    ops.push(SvgOp::Fill { polys: all.clone(), color: c });
                }
            }
            for s in &subpaths {
                emit_stroke_if_any(s.pts.clone(), s.closed, stroke_rgba, sw, dash.as_ref(), ops);
            }
            emit_markers(tree, id, markers, &all, sw, ops);
        }
        "text" => {
            let (x, y) = (attr_f(tree, id, "x").unwrap_or(0.0), attr_f(tree, id, "y").unwrap_or(0.0));
            let anchor = match tree
                .with_node(id, |n| n.get_attribute("text-anchor").map(str::to_string))
                .flatten()
                .as_deref()
                .map(str::trim)
            {
                Some("middle") => TextAnchor::Middle,
                Some("end") => TextAnchor::End,
                _ => TextAnchor::Start,
            };
            let content = collect_text(tree, id);
            let Some(c) = fill_rgba.filter(|c| c[3] > 0) else {
                return;
            };
            if content.trim().is_empty() {
                return;
            }
            let (tx, ty) = apply(&m, (x, y));
            ops.push(SvgOp::Text { content, x: tx, y: ty, anchor, font_size, bold, color: c });
        }
        // g, svg (nested viewBox out of scope), unknown containers: recurse.
        _ => {
            for child in tree.children(id) {
                walk(tree, styles, child, next.clone(), markers, ops);
            }
        }
    }
}

fn unit_circle(segments: usize) -> Vec<(f32, f32)> {
    (0..segments)
        .map(|i| {
            let a = i as f32 * std::f32::consts::TAU / segments as f32;
            (a.cos(), a.sin())
        })
        .collect()
}

/// Rectangle outline with quarter-ellipse corners (8 segments each).
fn rounded_rect_poly(x: f32, y: f32, w: f32, h: f32, rx: f32, ry: f32) -> Vec<(f32, f32)> {
    if rx <= 0.0 || ry <= 0.0 {
        return vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h)];
    }
    let mut pts = Vec::new();
    let mut corner = |cx: f32, cy: f32, a0: f32| {
        for i in 0..8 {
            let a = a0 + i as f32 * (std::f32::consts::FRAC_PI_2 / 8.0);
            pts.push((cx + rx * a.cos(), cy - ry * a.sin()));
        }
    };
    corner(x + w - rx, y + ry, 0.0);
    corner(x + w - rx, y + h - ry, -std::f32::consts::FRAC_PI_2);
    corner(x + rx, y + h - ry, std::f32::consts::PI);
    corner(x + rx, y + ry, std::f32::consts::FRAC_PI_2);
    pts
}

fn emit_fill_stroke(
    poly: Vec<(f32, f32)>,
    closed: bool,
    fill: Option<[u8; 4]>,
    stroke: Option<[u8; 4]>,
    sw: f32,
    dash: Option<&Vec<f32>>,
    ops: &mut Vec<SvgOp>,
) {
    if poly.len() >= 3 {
        if let Some(c) = fill.filter(|c| c[3] > 0) {
            ops.push(SvgOp::Fill { polys: vec![poly.clone()], color: c });
        }
    }
    emit_stroke_if_any(poly, closed, stroke, sw, dash, ops);
}

fn emit_stroke_if_any(
    poly: Vec<(f32, f32)>,
    closed: bool,
    stroke: Option<[u8; 4]>,
    sw: f32,
    dash: Option<&Vec<f32>>,
    ops: &mut Vec<SvgOp>,
) {
    if let Some(c) = stroke.filter(|c| c[3] > 0 && sw > 0.0) {
        ops.push(SvgOp::Stroke { poly, closed, width: sw, dash: dash.cloned(), color: c });
    }
}

/// Concatenate the text content under a `<text>` element (tspans join inline;
/// their x/y positioning is out of scope).
fn collect_text(tree: &DomTree, id: NodeId) -> String {
    let mut out = String::new();
    for child in tree.children(id) {
        if let Some(t) = tree.with_node(child, |n| n.text_content_of_text_node().map(str::to_string)).flatten() {
            out.push_str(&t);
        } else if tree
            .with_node(child, |n| n.as_element().map(|e| e.local.to_string() == "tspan"))
            .flatten()
            .unwrap_or(false)
        {
            out.push_str(&collect_text(tree, child));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Markers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct MarkerDef {
    /// Polygon points in marker units (local marker space).
    points: Vec<(f32, f32)>,
    /// The marker-space point that anchors to the path endpoint.
    ref_pt: (f32, f32),
    fill: [u8; 4],
}

fn collect_markers(
    tree: &DomTree,
    styles: &HashMap<NodeId, ComputedStyle>,
    id: NodeId,
    out: &mut HashMap<String, MarkerDef>,
) {
    let tag = tree
        .with_node(id, |n| n.as_element().map(|e| e.local.to_string()))
        .flatten()
        .unwrap_or_default();
    if tag == "marker" {
        if let Some(mid) = tree.with_node(id, |n| n.get_attribute("id").map(str::to_string)).flatten() {
            // The marker's shape is its first polygon/path child; the
            // archify shape is a polygon "0 0, 10 3.5, 0 7".
            for child in tree.children(id) {
                let ctag = tree
                    .with_node(child, |n| n.as_element().map(|e| e.local.to_string()))
                    .flatten()
                    .unwrap_or_default();
                let pts = match ctag.as_str() {
                    "polygon" | "polyline" => tree
                        .with_node(child, |n| n.get_attribute("points").map(str::to_string))
                        .flatten()
                        .map(|v| parse_points(&v)),
                    "path" => tree
                        .with_node(child, |n| n.get_attribute("d").map(str::to_string))
                        .flatten()
                        .and_then(|v| parse_path(&v, &IDENTITY).into_iter().next().map(|s| s.pts)),
                    _ => None,
                };
                if let Some(points) = pts.filter(|p| p.len() >= 2) {
                    // Fill color: the shape's own CSS class (.m-* in archify)
                    // or attr, else wrong-but-visible black — archify always
                    // classes it.
                    let cs = styles.get(&child).cloned().unwrap_or_default();
                    let fill = cs
                        .svg_fill
                        .or_else(|| {
                            tree.with_node(child, |n| n.get_attribute("fill").map(str::to_string))
                                .flatten()
                                .and_then(|v| attr_paint(&v))
                        })
                        .map(|p| rgba(p, cs.color))
                        .unwrap_or([0, 0, 0, 255]);
                    let ref_pt = (
                        attr_f(tree, id, "refX").unwrap_or(0.0),
                        attr_f(tree, id, "refY").unwrap_or(0.0),
                    );
                    out.insert(mid, MarkerDef { points, ref_pt, fill });
                    break;
                }
            }
        }
    }
    for child in tree.children(id) {
        collect_markers(tree, styles, child, out);
    }
}

/// Emit marker geometry for marker-start / marker-end on a shape. `subpaths`
/// is the flattened geometry; the end marker orients on the LAST subpath's
/// final segment, the start marker on the FIRST subpath's first segment.
fn emit_markers(
    tree: &DomTree,
    id: NodeId,
    markers: &HashMap<String, MarkerDef>,
    subpaths: &[Vec<(f32, f32)>],
    sw: f32,
    ops: &mut Vec<SvgOp>,
) {
    let url = |name: &str| {
        tree.with_node(id, |n| n.get_attribute(name).map(str::to_string))
            .flatten()
            .and_then(|v| {
                v.trim()
                    .strip_prefix("url(#")
                    .and_then(|r| r.strip_suffix(')'))
                    .map(str::to_string)
            })
    };
    // markerUnits default = strokeWidth: one marker unit = one stroke-width
    // unit in user space.
    let k = sw.max(0.01);
    if let Some(def) = url("marker-end").and_then(|mid| markers.get(&mid)) {
        if let Some(dir) = end_direction(subpaths, false) {
            ops.push(marker_op(def, subpaths.last().and_then(|s| s.last().copied()), dir, k));
        }
    }
    if let Some(def) = url("marker-start").and_then(|mid| markers.get(&mid)) {
        if let (Some(first), Some(dir)) = (
            subpaths.first().and_then(|s| s.first().copied()),
            end_direction(subpaths, true),
        ) {
            ops.push(marker_op(def, Some(first), dir, k));
        }
    }
}

fn marker_op(def: &MarkerDef, at: Option<(f32, f32)>, dir: (f32, f32), k: f32) -> SvgOp {
    let (ex, ey) = at.unwrap_or_default();
    let len = dir.0.hypot(dir.1);
    let (ux, uy) = if len > f32::EPSILON { (dir.0 / len, dir.1 / len) } else { (1.0, 0.0) };
    // Rotate the marker polygon so +x follows `dir`, scale by k, translate
    // so ref_pt lands on the endpoint.
    let pts = def
        .points
        .iter()
        .map(|&(px, py)| {
            let dx = (px - def.ref_pt.0) * k;
            let dy = (py - def.ref_pt.1) * k;
            (ex + dx * ux - dy * uy, ey + dx * uy + dy * ux)
        })
        .collect();
    SvgOp::Fill { polys: vec![pts], color: def.fill }
}

/// Unit direction at the end (start=false) or start (true) of the geometry.
fn end_direction(subpaths: &[Vec<(f32, f32)>], start: bool) -> Option<(f32, f32)> {
    let path = if start { subpaths.first()? } else { subpaths.last()? };
    if path.len() < 2 {
        return None;
    }
    let n = path.len();
    if start {
        Some((path[1].0 - path[0].0, path[1].1 - path[0].1))
    } else {
        Some((path[n - 1].0 - path[n - 2].0, path[n - 1].1 - path[n - 2].1))
    }
}

// ---------------------------------------------------------------------------
// Parsers: transform / path / points / dasharray
// ---------------------------------------------------------------------------

fn parse_transform(v: &str) -> Option<[f32; 6]> {
    let mut acc = IDENTITY;
    let mut rest = v.trim();
    while let Some(open) = rest.find('(') {
        let close = rest.find(')')?;
        if close < open {
            break; // malformed; keep what composed so far
        }
        let name = rest[..open].trim().to_ascii_lowercase();
        let args: Vec<f32> = rest[open + 1..close]
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|t| !t.is_empty())
            .filter_map(|t| t.parse::<f32>().ok())
            .collect();
        let m = match (name.as_str(), args.as_slice()) {
            ("translate", [tx]) => [1.0, 0.0, 0.0, 1.0, *tx, 0.0],
            ("translate", [tx, ty]) => [1.0, 0.0, 0.0, 1.0, *tx, *ty],
            ("scale", [sx]) => [*sx, 0.0, 0.0, *sx, 0.0, 0.0],
            ("scale", [sx, sy]) => [*sx, 0.0, 0.0, *sy, 0.0, 0.0],
            ("rotate", [deg]) => {
                let r = deg.to_radians();
                [r.cos(), r.sin(), -r.sin(), r.cos(), 0.0, 0.0]
            }
            ("rotate", [deg, cx, cy]) => {
                let r = deg.to_radians();
                // translate(cx,cy) · R · translate(-cx,-cy)
                let rot = [r.cos(), r.sin(), -r.sin(), r.cos(), 0.0, 0.0];
                let pre = [1.0, 0.0, 0.0, 1.0, *cx, *cy];
                let post = [1.0, 0.0, 0.0, 1.0, -cx, -cy];
                mul(&mul(&pre, &rot), &post)
            }
            ("matrix", [a, b, c, d, e, f]) => [*a, *b, *c, *d, *e, *f],
            _ => IDENTITY, // unknown function: ignore, keep parsing
        };
        acc = mul(&acc, &m);
        rest = rest[close + 1..].trim_start();
    }
    Some(acc)
}

fn parse_dasharray(v: &str) -> Option<Vec<f32>> {
    let list: Option<Vec<f32>> = v
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(num)
        .collect();
    match list {
        Some(l) if !l.is_empty() && l.iter().any(|n| *n > 0.0) => Some(l),
        _ => None,
    }
}

fn parse_points(v: &str) -> Vec<(f32, f32)> {
    let nums: Vec<f32> = v
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<f32>().ok())
        .collect();
    nums.chunks_exact(2)
        .filter_map(|c| c.first().zip(c.get(1)).map(|(&a, &b)| (a, b)))
        .collect()
}

/// One flattened subpath.
struct Subpath {
    pts: Vec<(f32, f32)>,
    closed: bool,
}

enum Tok {
    Cmd(char),
    Num(f32),
}

/// Char-based lexer — path data routinely glues commands to numbers
/// ("M0,0L100,0"), which whitespace/comma splitting mangles.
fn lex_path(d: &str) -> Vec<Tok> {
    let chars: Vec<char> = d.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == ',' || c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() {
            out.push(Tok::Cmd(c));
            i += 1;
            continue;
        }
        let start = i;
        if c == '-' || c == '+' {
            i += 1;
        }
        let mut seen_dot = false;
        while i < chars.len() {
            match chars[i] {
                '0'..='9' => i += 1,
                '.' if !seen_dot => {
                    seen_dot = true;
                    i += 1;
                }
                'e' | 'E' => {
                    let mut j = i + 1;
                    if j < chars.len() && (chars[j] == '-' || chars[j] == '+') {
                        j += 1;
                    }
                    if j < chars.len() && chars[j].is_ascii_digit() {
                        i = j;
                        while i < chars.len() && chars[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                    break;
                }
                _ => break,
            }
        }
        let s: String = chars[start..i].iter().collect();
        if let Ok(v) = s.parse::<f32>() {
            out.push(Tok::Num(v));
        }
        if i == start {
            i += 1; // unparseable junk: skip one char
        }
    }
    out
}

/// Path data → flattened subpaths through the given transform (Q subdivides
/// into 12 segments, C into 16; arcs degrade to lines). Relative commands
/// resolve against the current point; H/V against the respective axis.
fn parse_path(d: &str, m: &[f32; 6]) -> Vec<Subpath> {
    let toks = lex_path(d);
    let take = |i: &mut usize| -> Option<f32> {
        if let Some(Tok::Num(v)) = toks.get(*i) {
            *i += 1;
            Some(*v)
        } else {
            None
        }
    };
    let mut subpaths: Vec<Subpath> = Vec::new();
    let mut cur = (0.0f32, 0.0f32);
    let mut start = cur;
    let mut cmd = ' ';
    let mut i = 0usize;
    while i < toks.len() {
        if let Tok::Cmd(c) = toks[i] {
            cmd = c;
            i += 1;
            if cmd == 'Z' || cmd == 'z' {
                if let Some(s) = subpaths.last_mut() {
                    s.closed = true;
                }
                cur = start;
            }
            continue;
        }
        if cmd == 'Z' || cmd == 'z' {
            break; // numbers after a lone Z: malformed, stop
        }
        let rel = cmd.is_ascii_lowercase();
        match cmd.to_ascii_uppercase() {
            'M' => {
                let (Some(x), Some(y)) = (take(&mut i), take(&mut i)) else { break };
                cur = if rel { (cur.0 + x, cur.1 + y) } else { (x, y) };
                start = cur;
                subpaths.push(Subpath { pts: vec![apply(m, cur)], closed: false });
                // Subsequent implicit pairs are LineTos.
                cmd = if rel { 'l' } else { 'L' };
            }
            'L' => {
                let (Some(x), Some(y)) = (take(&mut i), take(&mut i)) else { break };
                cur = if rel { (cur.0 + x, cur.1 + y) } else { (x, y) };
                push_pt(&mut subpaths, apply(m, cur));
            }
            'H' => {
                let Some(x) = take(&mut i) else { break };
                cur.0 = if rel { cur.0 + x } else { x };
                push_pt(&mut subpaths, apply(m, cur));
            }
            'V' => {
                let Some(y) = take(&mut i) else { break };
                cur.1 = if rel { cur.1 + y } else { y };
                push_pt(&mut subpaths, apply(m, cur));
            }
            'Q' => {
                let (Some(qx), Some(qy), Some(x), Some(y)) =
                    (take(&mut i), take(&mut i), take(&mut i), take(&mut i))
                else {
                    break;
                };
                let q = if rel { (cur.0 + qx, cur.1 + qy) } else { (qx, qy) };
                let end = if rel { (cur.0 + x, cur.1 + y) } else { (x, y) };
                // Exact quadratic→cubic control lift: C1 = P0+⅔(Q−P0),
                // C2 = P1+⅔(Q−P1) — NOT the control point itself.
                let c1 = (cur.0 + (2.0 / 3.0) * (q.0 - cur.0), cur.1 + (2.0 / 3.0) * (q.1 - cur.1));
                let c2 = (end.0 + (2.0 / 3.0) * (q.0 - end.0), end.1 + (2.0 / 3.0) * (q.1 - end.1));
                flatten_bezier(&mut subpaths, cur, c1, c2, end, 12, m);
                cur = end;
            }
            'C' => {
                let (Some(x1), Some(y1), Some(x2), Some(y2), Some(x3), Some(y3)) = (
                    take(&mut i),
                    take(&mut i),
                    take(&mut i),
                    take(&mut i),
                    take(&mut i),
                    take(&mut i),
                )
                else {
                    break;
                };
                let c1 = if rel { (cur.0 + x1, cur.1 + y1) } else { (x1, y1) };
                let c2 = if rel { (cur.0 + x2, cur.1 + y2) } else { (x2, y2) };
                let end = if rel { (cur.0 + x3, cur.1 + y3) } else { (x3, y3) };
                flatten_bezier(&mut subpaths, cur, c1, c2, end, 16, m);
                cur = end;
            }
            'A' => {
                // Arc: seven params, all dropped except the endpoint.
                for _ in 0..5 {
                    if take(&mut i).is_none() {
                        break;
                    }
                }
                let (Some(x), Some(y)) = (take(&mut i), take(&mut i)) else { break };
                cur = if rel { (cur.0 + x, cur.1 + y) } else { (x, y) };
                push_pt(&mut subpaths, apply(m, cur));
            }
            _ => break,
        }
    }
    subpaths.retain(|s| s.pts.len() >= 2);
    subpaths
}

fn push_pt(subpaths: &mut [Subpath], p: (f32, f32)) {
    if let Some(s) = subpaths.last_mut() {
        s.pts.push(p);
    }
}

/// Cubic bezier flattening.
fn flatten_bezier(
    subpaths: &mut [Subpath],
    p0: (f32, f32),
    c1: (f32, f32),
    c2: (f32, f32),
    p1: (f32, f32),
    segments: usize,
    m: &[f32; 6],
) {
    for i in 1..=segments {
        let t = i as f32 / segments as f32;
        let mt = 1.0 - t;
        let x = mt * mt * mt * p0.0
            + 3.0 * mt * mt * t * c1.0
            + 3.0 * mt * t * t * c2.0
            + t * t * t * p1.0;
        let y = mt * mt * mt * p0.1
            + 3.0 * mt * mt * t * c1.1
            + 3.0 * mt * t * t * c2.1
            + t * t * t * p1.1;
        push_pt(subpaths, apply(m, (x, y)));
    }
}

// ---------------------------------------------------------------------------
// Paint: op list → canvas, through the viewBox → element-box map
// ---------------------------------------------------------------------------

/// Paint a compiled svg into its element box on the canvas. `dx`/`dy` are
/// the band shift shared with [`super::paint::execute_band`].
pub fn paint_svg(
    render: &SvgRender,
    rect: &Rect,
    fonts: &FontBook,
    out: &mut Canvas,
    dx: f32,
    dy: f32,
) {
    if rect.width <= 0.0 || rect.height <= 0.0 || render.ops.is_empty() {
        return;
    }
    let (bx, by) = (rect.x - dx, rect.y - dy);
    // viewBox → box: xMidYMid `meet` (the SVG default): uniform scale to
    // fit, centered. No viewBox: user units are px from the box origin.
    let (s, vbx, vby, ox, oy) = match render.view_box {
        Some((vx, vy, vw, vh)) => {
            let s = (rect.width / vw).min(rect.height / vh);
            (
                s,
                vx,
                vy,
                bx + (rect.width - vw * s) / 2.0,
                by + (rect.height - vh * s) / 2.0,
            )
        }
        None => (1.0, 0.0, 0.0, bx, by),
    };
    let map = |p: (f32, f32)| (ox + (p.0 - vbx) * s, oy + (p.1 - vby) * s);

    out.push_clip(
        bx.round() as i64,
        by.round() as i64,
        (bx + rect.width).round() as i64,
        (by + rect.height).round() as i64,
    );
    for op in &render.ops {
        match op {
            SvgOp::Fill { polys, color } => {
                let mapped: Vec<Vec<(f32, f32)>> =
                    polys.iter().map(|p| p.iter().map(|&q| map(q)).collect()).collect();
                fill_even_odd(&mapped, *color, out);
            }
            SvgOp::Stroke { poly, closed, width, dash, color } => {
                let mapped: Vec<(f32, f32)> = poly.iter().map(|&q| map(q)).collect();
                stroke_polyline(
                    &mapped,
                    *closed,
                    width * s,
                    dash.as_ref().map(|d| d.iter().map(|n| n * s).collect::<Vec<_>>()).as_deref(),
                    *color,
                    out,
                );
            }
            SvgOp::Text { content, x, y, anchor, font_size, bold, color } => {
                let fs = font_size * s;
                if fs < 1.0 {
                    continue;
                }
                let (sx, sy) = map((*x, *y));
                let r = fonts.rasterize(content, fs, *bold, *color, fs * 1.2);
                if r.width == 0 || r.height == 0 {
                    continue;
                }
                let x_off = match anchor {
                    TextAnchor::Start => 0.0,
                    TextAnchor::Middle => -(r.width as f32) / 2.0,
                    TextAnchor::End => -(r.width as f32),
                };
                // svg `y` is the baseline; the tile's `baseline` field is the
                // baseline's offset from the tile top.
                out.blit_text(&r, (sx + x_off).round() as i64, (sy - r.baseline).round() as i64);
            }
        }
    }
    out.pop_clip();
}

/// Even-odd scanline fill: per pixel row, sort the polygon crossings, pair
/// them, and emit one `fill_rect` run per pair — reusing the canvas's whole
/// clip-stack machinery instead of duplicating it.
fn fill_even_odd(polys: &[Vec<(f32, f32)>], color: [u8; 4], out: &mut Canvas) {
    let mut edges: Vec<((f32, f32), (f32, f32))> = Vec::new();
    let (mut min_y, mut max_y) = (f64::MAX, f64::MIN);
    for poly in polys {
        if poly.len() < 3 {
            continue;
        }
        for i in 0..poly.len() {
            let a = poly[i];
            let b = poly[(i + 1) % poly.len()];
            if (a.1 - b.1).abs() > f32::EPSILON {
                edges.push((a, b));
                min_y = min_y.min(a.1 as f64).min(b.1 as f64);
                max_y = max_y.max(a.1 as f64).max(b.1 as f64);
            }
        }
    }
    if edges.is_empty() {
        return;
    }
    let y0 = (min_y.max(0.0).floor() as i64).max(0);
    let y1 = (max_y.ceil() as i64).min(out.height as i64);
    let mut xs: Vec<f64> = Vec::new();
    for gy in y0..y1 {
        let yc = gy as f64 + 0.5;
        xs.clear();
        for (a, b) in &edges {
            let (ay, by) = (a.1 as f64, b.1 as f64);
            let (lo, hi) = if ay < by { (ay, by) } else { (by, ay) };
            if yc >= lo && yc < hi {
                let t = (yc - ay) / (by - ay);
                xs.push(a.0 as f64 + t * (b.0 as f64 - a.0 as f64));
            }
        }
        xs.sort_by(|p, q| p.partial_cmp(q).unwrap_or(std::cmp::Ordering::Equal));
        let mut it = 0;
        while it + 1 < xs.len() {
            let run = (xs[it], xs[it + 1]);
            it += 2;
            // Pixels whose centers fall strictly inside the run.
            let px0 = ((run.0 - 0.5).floor() + 1.0).max(0.0) as i64;
            let px1 = (run.1 - 0.5).ceil().min(out.width as f64) as i64;
            if px1 > px0 {
                out.fill_rect(px0, gy, px1 - px0, 1, color);
            }
        }
    }
}

fn segments_of(poly: &[(f32, f32)], closed: bool) -> Vec<((f32, f32), (f32, f32))> {
    let mut segs: Vec<((f32, f32), (f32, f32))> = poly.windows(2).map(|w| (w[0], w[1])).collect();
    if closed && poly.len() >= 2 {
        segs.push((poly[poly.len() - 1], poly[0]));
    }
    segs
}

/// On-distance runs of a dash pattern (odd-length patterns double up per
/// spec). Distance 0 is always "on".
fn dash_on_runs(poly: &[(f32, f32)], closed: bool, pattern: &[f32]) -> Vec<(f32, f32)> {
    let mut pat: Vec<f32> = pattern.to_vec();
    if pat.len() % 2 == 1 {
        let orig = pat.clone();
        pat.extend(orig);
    }
    let total = polyline_length(poly, closed);
    let period: f32 = pat.iter().sum();
    if period <= 0.0 {
        return vec![(0.0, total)];
    }
    let mut runs = Vec::new();
    let (mut d, mut idx, mut pat_pos) = (0.0f32, 0usize, 0.0f32);
    let mut on_start: Option<f32> = None;
    for (a, b) in segments_of(poly, closed) {
        let mut remain = (b.0 - a.0).hypot(b.1 - a.1);
        while remain > f32::EPSILON {
            let chunk = remain.min(pat[idx] - pat_pos);
            if idx % 2 == 0 {
                on_start.get_or_insert(d);
            } else if let Some(s) = on_start.take() {
                runs.push((s, d));
            }
            d += chunk;
            pat_pos += chunk;
            remain -= chunk;
            if pat_pos >= pat[idx] - f32::EPSILON {
                idx = (idx + 1) % pat.len();
                pat_pos = 0.0;
            }
        }
    }
    if let Some(s) = on_start {
        runs.push((s, total));
    }
    runs
}

/// Stroke by stamping squares along the polyline every ~0.7px — works for
/// any angle, collapses to exact rect bands on axis-aligned segments.
/// Dashes carve the sample set into on-runs.
fn stroke_polyline(
    poly: &[(f32, f32)],
    closed: bool,
    width: f32,
    dash: Option<&[f32]>,
    color: [u8; 4],
    out: &mut Canvas,
) {
    if poly.len() < 2 {
        return;
    }
    let stamp_w = width.max(1.0);
    let half = stamp_w / 2.0;
    let size = stamp_w.ceil() as i64;
    let on_runs = dash.map(|pattern| dash_on_runs(poly, closed, pattern));
    let in_on = |d: f32| on_runs.as_ref().is_none_or(|runs| runs.iter().any(|&(a, b)| d >= a && d < b));

    let mut d = 0.0f32;
    for (a, b) in segments_of(poly, closed) {
        let len = (b.0 - a.0).hypot(b.1 - a.1);
        if len <= 0.0 {
            continue;
        }
        let steps = ((len / 0.7).ceil() as i32).max(1);
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let dd = d + len * t;
            if in_on(dd) {
                let cx = a.0 + (b.0 - a.0) * t - half;
                let cy = a.1 + (b.1 - a.1) * t - half;
                out.fill_rect(cx.round() as i64, cy.round() as i64, size, size, color);
            }
        }
        d += len;
    }
}

fn polyline_length(poly: &[(f32, f32)], closed: bool) -> f32 {
    segments_of(poly, closed)
        .iter()
        .map(|(a, b)| (b.0 - a.0).hypot(b.1 - a.1))
        .sum()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diting_dom::tree_sink::parse_html;

    fn compile_of(html: &str) -> SvgRender {
        let tree = parse_html(html);
        let svg_id = tree.query_selector("svg").expect("parse ok").expect("an svg element");
        let styles = super::super::compute_styles(&tree, &[]);
        compile_svg(&tree, &styles, svg_id)
    }

    #[test]
    fn viewbox_and_points_parse() {
        assert_eq!(parse_view_box("0 0 345 230"), Some((0.0, 0.0, 345.0, 230.0)));
        assert_eq!(parse_view_box("200,0,880,588"), Some((200.0, 0.0, 880.0, 588.0)));
        assert_eq!(parse_view_box("0 0 0 10"), None, "zero extent is invalid");
        let pts = parse_points("0 0, 10 3.5, 0 7");
        assert_eq!(pts, vec![(0.0, 0.0), (10.0, 3.5), (0.0, 7.0)]);
    }

    #[test]
    fn transforms_compose_and_scale() {
        let t = parse_transform("translate(10 20) scale(0.6875)").unwrap();
        let p = apply(&t, (100.0, 80.0));
        assert!((p.0 - 78.75).abs() < 1e-3 && (p.1 - 75.0).abs() < 1e-3, "{p:?}");
        assert!((scale_of(&t) - 0.6875).abs() < 1e-4, "stroke width rides the uniform scale");
        let r = parse_transform("rotate(90)").unwrap();
        let q = apply(&r, (10.0, 0.0));
        assert!((q.1 - 10.0).abs() < 1e-4 && q.0.abs() < 1e-4, "{q:?}");
    }

    #[test]
    fn path_flattens_with_transform() {
        let subpaths = parse_path("M 0 0 L 100 0 Q 100 100 0 100 Z", &IDENTITY);
        assert_eq!(subpaths.len(), 1);
        assert!(subpaths[0].closed);
        // M + L + 12 Q samples (Z does not add a point).
        assert_eq!(subpaths[0].pts.len(), 14);
        assert_eq!(subpaths[0].pts[0], (0.0, 0.0));
        assert_eq!(subpaths[0].pts[1], (100.0, 0.0));
        // Q midpoint: exact lift puts t=0.5 at (75, 75) for this control.
        let mid = subpaths[0].pts[7];
        assert!((mid.0 - 75.0).abs() < 1e-3 && (mid.1 - 75.0).abs() < 1e-3, "{mid:?}");
        // Relative form walks the same ground.
        let rel = parse_path("m 10 10 l 90 0 z", &IDENTITY);
        assert_eq!(rel[0].pts, vec![(10.0, 10.0), (100.0, 10.0)]);
        // Glued tokens (no separators before commands) lex correctly.
        let glued = parse_path("M0,0L100,0L100,50Z", &IDENTITY);
        assert_eq!(glued[0].pts, vec![(0.0, 0.0), (100.0, 0.0), (100.0, 50.0)]);
        // Arc params are consumed without derailing the following command.
        let arc = parse_path("M 0 0 A 30 30 0 0 1 60 0 L 60 30", &IDENTITY);
        assert_eq!(arc[0].pts, vec![(0.0, 0.0), (60.0, 0.0), (60.0, 30.0)]);
    }

    #[test]
    fn rect_and_paint_priority() {
        // fill attr with no CSS around → attr wins the unset chain.
        let r = compile_of(
            r##"<svg viewBox="0 0 100 100"><rect x="10" y="10" width="30" height="20" fill="#ff0000"/></svg>"##,
        );
        assert_eq!(r.view_box, Some((0.0, 0.0, 100.0, 100.0)));
        match r.ops.as_slice() {
            [SvgOp::Fill { polys, color }] => {
                assert_eq!(*color, [255, 0, 0, 255]);
                assert_eq!(polys[0][0], (10.0, 10.0));
                assert_eq!(polys[0][2], (40.0, 30.0));
            }
            other => panic!("expected one fill op: {other:?}"),
        }

        // CSS fill beats the attr (spec priority: own CSS > own attr).
        let tree = parse_html(
            r##"<svg viewBox="0 0 10 10"><rect x="0" y="0" width="5" height="5" fill="#ff0000"/></svg>"##,
        );
        let svg_id = tree.query_selector("svg").unwrap().unwrap();
        let rules = crate::diting_css::parse_stylesheet("rect { fill: #00ff00; }");
        let styles = super::super::compute_styles(&tree, &rules);
        let r = compile_svg(&tree, &styles, svg_id);
        match r.ops.as_slice() {
            [SvgOp::Fill { color, .. }] => assert_eq!(*color, [0, 255, 0, 255]),
            other => panic!("CSS fill must outrank the attr: {other:?}"),
        }

        // Inherited attr paint: a g-level fill reaches an attr-less child;
        // the child's own fill="none" wins over the inherited fill and is
        // filtered at emit (alpha 0), leaving exactly the inherited stroke.
        let r = compile_of(
            r#"<svg viewBox="0 0 10 10"><g fill="blue" stroke="red"><circle cx="5" cy="5" r="3" fill="none"/></g></svg>"#,
        );
        match r.ops.as_slice() {
            [SvgOp::Stroke { color, poly, .. }] => {
                assert_eq!(*color, [255, 0, 0, 255], "g stroke inherits to the circle");
                assert_eq!(poly.len(), 48, "circle is a 48-gon");
            }
            other => panic!("fill=none circle emits only the inherited stroke: {other:?}"),
        }
    }

    #[test]
    fn opacity_attrs_scale_alpha() {
        // Group opacity composes down the subtree and multiplies into every
        // paint's alpha; nested groups multiply.
        let r = compile_of(
            r##"<svg viewBox="0 0 10 10"><g opacity="0.5"><g opacity="0.5"><rect x="0" y="0" width="4" height="4" fill="#0000ff"/></g><rect x="5" y="0" width="4" height="4" fill="#0000ff"/></g></svg>"##,
        );
        match r.ops.as_slice() {
            [SvgOp::Fill { color: inner, .. }, SvgOp::Fill { color: outer, .. }] => {
                assert_eq!(inner[3], 64, "0.5 * 0.5 * 255 rounds to 64: {inner:?}");
                assert_eq!(outer[3], 128, "single 0.5 * 255 rounds to 128: {outer:?}");
            }
            other => panic!("two fills expected: {other:?}"),
        }

        // fill-opacity / stroke-opacity scale only their own channel, and
        // compose with the group factor.
        let r = compile_of(
            r##"<svg viewBox="0 0 10 10"><g opacity="0.5"><rect x="0" y="0" width="4" height="4" fill="#ff0000" fill-opacity="0.5" stroke="#00ff00" stroke-opacity="0.25" stroke-width="1"/></g></svg>"##,
        );
        match r.ops.as_slice() {
            [SvgOp::Fill { color, .. }, SvgOp::Stroke { color: sc, .. }] => {
                assert_eq!(color[3], 64, "0.5 group * 0.5 fill = 0.25: {color:?}");
                assert_eq!(sc[3], 32, "0.5 group * 0.25 stroke = 0.125: {sc:?}");
            }
            other => panic!("fill+stroke expected: {other:?}"),
        }

        // opacity="0" folds to alpha 0 and the emitters filter it out —
        // the dimming viewer's "hidden" state needs a real absence.
        let r = compile_of(
            r##"<svg viewBox="0 0 10 10"><rect x="0" y="0" width="4" height="4" fill="#ff0000" opacity="0"/></svg>"##,
        );
        assert!(r.ops.is_empty(), "fully transparent shape draws nothing: {:?}", r.ops);

        // Text rides fill_rgba, so a dimmed group dims its labels too.
        let r = compile_of(
            r#"<svg viewBox="0 0 10 10"><g opacity="0.12"><text x="1" y="5">hi</text></g></svg>"#,
        );
        match r.ops.as_slice() {
            [SvgOp::Text { color, .. }] => assert_eq!(color[3], 31, "0.12 * 255 rounds to 31"),
            other => panic!("one text op expected: {other:?}"),
        }

        // No opacity attrs anywhere = fully opaque, bytes of behavior
        // unchanged (255 stays 255 — the pre-existing default).
        let r = compile_of(
            r##"<svg viewBox="0 0 10 10"><rect x="0" y="0" width="4" height="4" fill="#123456"/></svg>"##,
        );
        match r.ops.as_slice() {
            [SvgOp::Fill { color, .. }] => assert_eq!(*color, [0x12, 0x34, 0x56, 255]),
            other => panic!("one fill op expected: {other:?}"),
        }
    }

    #[test]
    fn text_leak_children_compile_not_flow() {
        // A <text> child is an op, never flow content; <title>/metadata don't
        // contribute.
        let r = compile_of(
            r#"<svg viewBox="0 0 100 50"><title>meta</title><text x="4" y="20" font-size="12">Label</text></svg>"#,
        );
        match r.ops.as_slice() {
            [SvgOp::Text { content, x, y, anchor, font_size, .. }] => {
                assert_eq!(content, "Label");
                assert_eq!((*x, *y), (4.0, 20.0));
                assert_eq!(*anchor, TextAnchor::Start);
                assert!((*font_size - 12.0).abs() < 1e-4, "attr font-size beats inherited CSS");
            }
            other => panic!("expected one text op: {other:?}"),
        }
    }

    #[test]
    fn group_transform_bakes_into_geometry() {
        let r = compile_of(
            r#"<svg viewBox="0 0 200 100"><g transform="translate(10 5) scale(0.5)"><rect x="0" y="0" width="100" height="40" fill="blue"/></g></svg>"#,
        );
        match r.ops.as_slice() {
            [SvgOp::Fill { polys, .. }] => {
                let p = &polys[0];
                assert!((p[0].0 - 10.0).abs() < 1e-3 && (p[0].1 - 5.0).abs() < 1e-3, "{p:?}");
                assert!((p[2].0 - 60.0).abs() < 1e-3 && (p[2].1 - 25.0).abs() < 1e-3, "{p:?}");
            }
            other => panic!("expected one fill op: {other:?}"),
        }
    }

    #[test]
    fn scanline_fill_paints_square_and_hole() {
        let fonts = crate::diting_fonts::font_book();
        let render = SvgRender {
            view_box: None,
            ops: vec![
                // 20×20 square with a 8×8 hole at the center (even-odd).
                SvgOp::Fill {
                    polys: vec![
                        vec![(0.0, 0.0), (20.0, 0.0), (20.0, 20.0), (0.0, 20.0)],
                        vec![(6.0, 6.0), (14.0, 6.0), (14.0, 14.0), (6.0, 14.0)],
                    ],
                    color: [0, 128, 0, 255],
                },
            ],
        };
        let rect = Rect { x: 0.0, y: 0.0, width: 20.0, height: 20.0 };
        let mut canvas = Canvas::new_filled(20, 20, [255, 255, 255, 255]);
        paint_svg(&render, &rect, &fonts, &mut canvas, 0.0, 0.0);
        let px = |x: usize, y: usize| canvas.data[(y * 20 + x) * 4];
        assert_eq!(px(0, 0), 0, "corner filled (green R=0)");
        assert_eq!(px(19, 19), 0);
        assert_eq!(px(10, 10), 255, "even-odd hole stays empty");
        assert_eq!(px(3, 3), 0, "ring filled");
    }

    #[test]
    fn viewbox_meet_scales_into_box() {
        let fonts = crate::diting_fonts::font_book();
        // viewBox 200 wide → box 100 wide: scale 0.5, xMid letterboxes none
        // (ratio kept by the layout arm), yMid centers.
        let render = SvgRender {
            view_box: Some((0.0, 0.0, 200.0, 100.0)),
            ops: vec![SvgOp::Fill {
                polys: vec![vec![(0.0, 0.0), (200.0, 0.0), (200.0, 100.0), (0.0, 100.0)]],
                color: [0, 0, 0, 255],
            }],
        };
        let rect = Rect { x: 10.0, y: 20.0, width: 100.0, height: 50.0 };
        let mut canvas = Canvas::new_filled(120, 90, [255, 255, 255, 255]);
        paint_svg(&render, &rect, &fonts, &mut canvas, 0.0, 0.0);
        let px = |x: usize, y: usize| canvas.data[(y * 120 + x) * 4];
        assert_eq!(px(60, 45), 0, "box center painted");
        assert_eq!(px(60, 15), 255, "above the box untouched (letterbox)");
        assert_eq!(px(5, 45), 255, "left of the box untouched");
    }

    #[test]
    fn stroke_and_dash_paint() {
        let fonts = crate::diting_fonts::font_book();
        // Solid horizontal line: stamps collapse into a 2px band.
        let mut canvas = Canvas::new_filled(40, 10, [255, 255, 255, 255]);
        paint_svg(
            &SvgRender {
                view_box: None,
                ops: vec![SvgOp::Stroke {
                    poly: vec![(2.0, 5.0), (38.0, 5.0)],
                    closed: false,
                    width: 2.0,
                    dash: None,
                    color: [0, 0, 0, 255],
                }],
            },
            &Rect { x: 0.0, y: 0.0, width: 40.0, height: 10.0 },
            &fonts,
            &mut canvas,
            0.0,
            0.0,
        );
        let row_ink = |y: usize| (0..40).any(|x| canvas.data[(y * 40 + x) * 4] < 128);
        assert!(row_ink(4) && row_ink(5), "2px band covers rows 4-5");
        assert!(!row_ink(3) && !row_ink(6), "band edges stay sharp");

        // Dashed: 4-on 4-off leaves periodic gaps.
        let mut canvas = Canvas::new_filled(40, 10, [255, 255, 255, 255]);
        paint_svg(
            &SvgRender {
                view_box: None,
                ops: vec![SvgOp::Stroke {
                    poly: vec![(0.0, 5.0), (40.0, 5.0)],
                    closed: false,
                    width: 2.0,
                    dash: Some(vec![4.0, 4.0]),
                    color: [0, 0, 0, 255],
                }],
            },
            &Rect { x: 0.0, y: 0.0, width: 40.0, height: 10.0 },
            &fonts,
            &mut canvas,
            0.0,
            0.0,
        );
        let ink_at = |x: usize| canvas.data[(5 * 40 + x) * 4] < 128;
        assert!(ink_at(1), "first dash on");
        assert!(!ink_at(6), "first gap off");
        assert!(ink_at(9), "second dash on");
    }

    #[test]
    fn markers_orient_on_path_end() {
        let r = compile_of(
            r#"<svg viewBox="0 0 100 100"><defs><marker id="a" refX="9" refY="3.5" markerWidth="10" markerHeight="7" orient="auto"><polygon points="0 0, 10 3.5, 0 7"/></marker></defs><path d="M 0 10 L 80 10" stroke="black" marker-end="url(#a)"/></svg>"#,
        );
        // Stroke + marker fill; the marker tip sits at (80,10) + direction
        // (marker ref 9 of a 10-unit tip → tip 1 unit past the endpoint).
        let mut saw_stroke = false;
        let mut saw_marker = false;
        for op in &r.ops {
            match op {
                SvgOp::Stroke { .. } => saw_stroke = true,
                SvgOp::Fill { polys, .. } => {
                    saw_marker = true;
                    // stroke-width defaults to 1 → marker units scale 1.
                    // Tip = endpoint + (10-9, 3.5-3.5) rotated by 0° = (81, 10).
                    let tip = polys[0][1];
                    assert!((tip.0 - 81.0).abs() < 1e-3 && (tip.1 - 10.0).abs() < 1e-3, "{tip:?}");
                }
                _ => {}
            }
        }
        assert!(saw_stroke && saw_marker, "path emits stroke + marker: {:?}", r.ops);
    }
}
