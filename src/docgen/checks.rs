//! The composition audit — archify's post-render quality gates
//! (check-render-output.mjs + geometry.mjs) recast in integer tenths.
//!
//! The reference re-parses its emitted SVG to audit it; our adapters still
//! hold the placed geometry (routes, labels, frames), so the audit runs in
//! process over that. Same checks, same thresholds, same exemptions:
//!
//! - proper crossings and ambiguous corridors are exempted for pairs that
//!   share a semantic endpoint (their fan-out is real topology);
//! - a route may cross a composition frame but must not borrow its side as
//!   a corridor — border runs are an error in every profile (rounded
//!   corners are trimmed so a corner touch is not a run);
//! - label clearance measures each label against the OTHER relationships'
//!   segments (its own relationship is exempt): 2px at standard, 4px at
//!   showcase;
//! - route rhythm flags micro segments (<8px) anywhere and interior
//!   segments <16px; bend and stretch budgets are metrics, not issues;
//! - desktop readability projects the smallest node-label font through the
//!   930px reader width and floors it at 6px.
//!
//! The quality profile changes the report, never the artifact bytes:
//! standard rates findings as warnings (border runs excepted), showcase
//! rates everything as errors — so `render_markdown(quality:"showcase")`
//! is the delivery gate an agent checks before shipping a document.

use serde_json::{json, Value};

use super::graph::{Pt, Rect};
use super::tx;

/// Label clearance floor per profile (reference: 2px standard / 4px showcase).
const LABEL_CLEARANCE_STANDARD: i32 = 20;
const LABEL_CLEARANCE_SHOWCASE: i32 = 40;
/// Corridor overlap floor (reference minOverlapPx: 8px).
const CORRIDOR_MIN_OVERLAP: i32 = 80;
/// Rhythm floors (reference): micro 8px anywhere, 16px for interior runs.
const MICRO_SEGMENT: i32 = 80;
const INTERIOR_SEGMENT: i32 = 160;
/// Suggested budgets — metrics only, never issues (reference parity).
const MAX_BENDS: usize = 2;
const MAX_STRETCH_PCT: i64 = 135;
/// Desktop readability (reference): 1440×900 viewport, 960px reader minus
/// 30px of chrome; node text must still project to 6px.
const READER_WIDTH: i32 = 9300;
const MIN_PROJECTED_FONT: i32 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    Standard,
    Showcase,
}

impl Quality {
    pub fn by_name(name: &str) -> Option<Quality> {
        match name {
            "standard" => Some(Quality::Standard),
            "showcase" => Some(Quality::Showcase),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Quality::Standard => "standard",
            Quality::Showcase => "showcase",
        }
    }

    fn label_clearance(self) -> i32 {
        match self {
            Quality::Standard => LABEL_CLEARANCE_STANDARD,
            Quality::Showcase => LABEL_CLEARANCE_SHOWCASE,
        }
    }
}

/// One routed relationship, in the adapter's placed terms.
pub struct AuditRoute {
    /// Display name ("md->parse", or the authored edge id).
    pub name: String,
    /// Semantic endpoints, for the shared-endpoint exemptions.
    pub from: String,
    pub to: String,
    pub points: Vec<Pt>,
    pub label: Option<Rect>,
}

/// A composition frame a route may cross but never borrow.
pub struct AuditFrame {
    pub kind: &'static str,
    pub label: String,
    pub rect: Rect,
    /// Corner radius (tenths): border segments are trimmed by it.
    pub radius: i32,
}

/// Desktop-readability inputs: the measured canvas and the smallest primary
/// node-label font the adapter emitted.
pub struct Readability {
    pub view_box_w: i32,
    pub min_label_font: i32,
}

pub struct AuditScene<'a> {
    pub routes: &'a [AuditRoute],
    pub frames: &'a [AuditFrame],
    pub readability: Option<Readability>,
}

#[derive(Default, Debug)]
pub struct Composition {
    crossings: Vec<(String, String, Pt)>,
    corridors: Vec<(String, String, i32, Pt, Pt)>,
    border_runs: Vec<(String, String, &'static str, String, i32)>,
    /// (label's route, other route, clearance², mask-hidden length).
    label_clearances: Vec<(String, String, i64, i32)>,
    /// (route, code, segment index, is_interior, length).
    rhythm: Vec<(String, &'static str, usize, bool, i32)>,
    metrics: Metrics,
    readability: Option<(i32, i32, bool)>,
}

#[derive(Default, Debug)]
struct Metrics {
    max_bends: usize,
    routes_over_bends: usize,
    max_stretch_pct: Option<i64>,
    routes_over_stretch: usize,
    min_segment: Option<i32>,
    short_segments: usize,
    micro_segments: usize,
    min_label_clearance: Option<i64>,
}

/// A segment, normalized so `.0 <= .1` on the varying axis.
struct Seg {
    a: Pt,
    b: Pt,
    horizontal: bool,
}

fn segments(points: &[Pt]) -> impl Iterator<Item = Seg> + '_ {
    points.windows(2).map(|w| {
        let (a, b) = (w[0], w[1]);
        Seg {
            a,
            b,
            horizontal: a.1 == b.1,
        }
    })
}

impl Seg {
    fn len(&self) -> i32 {
        (self.b.0 - self.a.0).abs() + (self.b.1 - self.a.1).abs()
    }

    /// Colinear overlap with `other`, if both ride the same line: returns
    /// (length, start, end) on the shared axis.
    fn axis_overlap(&self, other: &Seg) -> Option<(i32, Pt, Pt)> {
        let overlap = |(a_lo, a_hi): (i32, i32), (b_lo, b_hi): (i32, i32)| {
            let lo = a_lo.max(b_lo);
            let hi = a_hi.min(b_hi);
            (hi > lo).then_some((lo, hi))
        };
        if self.horizontal && other.horizontal && self.a.1 == other.a.1 {
            let (lo, hi) = overlap(
                (self.a.0, self.b.0),
                (other.a.0, other.b.0),
            )?;
            Some((hi - lo, (lo, self.a.1), (hi, self.a.1)))
        } else if !self.horizontal && !other.horizontal && self.a.0 == other.a.0 {
            let (lo, hi) = overlap(
                (self.a.1, self.b.1),
                (other.a.1, other.b.1),
            )?;
            Some((hi - lo, (self.a.0, lo), (self.a.0, hi)))
        } else {
            None
        }
    }

    /// Proper crossing with an orthogonal segment: each passes strictly
    /// through the other's interior (touches are not crossings).
    fn crosses(&self, other: &Seg) -> Option<Pt> {
        let (h, v) = if self.horizontal && !other.horizontal {
            (self, other)
        } else if !self.horizontal && other.horizontal {
            (other, self)
        } else {
            return None;
        };
        let h_ys = (h.a.1, h.b.1);
        debug_assert_eq!(h_ys.0, h_ys.1);
        let v_xs = (v.a.0, v.b.0);
        debug_assert_eq!(v_xs.0, v_xs.1);
        let (v_lo, v_hi) = (v.a.1.min(v.b.1), v.a.1.max(v.b.1));
        let (h_lo, h_hi) = (h.a.0.min(h.b.0), h.a.0.max(h.b.0));
        (h_ys.0 > v_lo && h_ys.0 < v_hi && v_xs.0 > h_lo && v_xs.0 < h_hi)
            .then_some((v_xs.0, h_ys.0))
    }

    /// Overlap length with a rect's interior — how much of the segment the
    /// label mask would hide.
    fn rect_intersection(&self, r: &Rect) -> i32 {
        if self.horizontal {
            if self.a.1 < r.y || self.a.1 > r.bottom() {
                return 0;
            }
            (self.b.0.min(r.right()) - self.a.0.max(r.x)).max(0)
        } else {
            if self.a.0 < r.x || self.a.0 > r.right() {
                return 0;
            }
            (self.b.1.min(r.bottom()) - self.a.1.max(r.y)).max(0)
        }
    }

    /// Clearance² to a rect: 0 when they intersect, else the smallest of
    /// endpoint-to-rect and corner-to-segment distances, all squared so the
    /// threshold comparisons stay exact in integers.
    fn rect_clearance_sq(&self, r: &Rect) -> i64 {
        if self.rect_intersection(r) > 0 {
            return 0;
        }
        let point_rect_sq = |p: Pt| -> i64 {
            let dx = (r.x - p.0).max(0).max(p.0 - r.right());
            let dy = (r.y - p.1).max(0).max(p.1 - r.bottom());
            dx as i64 * dx as i64 + dy as i64 * dy as i64
        };
        let point_seg_sq = |p: Pt| -> i64 {
            // Routes are orthogonal; clamp on the varying axis.
            let (dx, dy) = if self.horizontal {
                let lo = self.a.0.min(self.b.0);
                let hi = self.a.0.max(self.b.0);
                ((p.0 - lo).max(0).min(hi - lo).abs(), p.1 - self.a.1)
            } else {
                let lo = self.a.1.min(self.b.1);
                let hi = self.a.1.max(self.b.1);
                (p.0 - self.a.0, (p.1 - lo).max(0).min(hi - lo).abs())
            };
            // The clamp above yields 0 only inside [lo, hi]; outside, the
            // distance is the raw offset.
            let (dx, dy) = if self.horizontal {
                let lo = self.a.0.min(self.b.0);
                let hi = self.a.0.max(self.b.0);
                (
                    if p.0 < lo { lo - p.0 } else if p.0 > hi { p.0 - hi } else { 0 },
                    dy,
                )
            } else {
                let lo = self.a.1.min(self.b.1);
                let hi = self.a.1.max(self.b.1);
                (
                    dx,
                    if p.1 < lo { lo - p.1 } else if p.1 > hi { p.1 - hi } else { 0 },
                )
            };
            dx as i64 * dx as i64 + dy as i64 * dy as i64
        };
        let mut best = point_rect_sq(self.a).min(point_rect_sq(self.b));
        for corner in [
            (r.x, r.y),
            (r.right(), r.y),
            (r.right(), r.bottom()),
            (r.x, r.bottom()),
        ] {
            best = best.min(point_seg_sq(corner));
        }
        best
    }
}

/// The frame's four border segments, trimmed by the corner radius.
fn border_segments(frame: &AuditFrame) -> [(Seg, &'static str); 4] {
    let r = &frame.rect;
    let rad = frame.radius.clamp(0, r.w / 2).clamp(0, r.h / 2);
    let (left, right, top, bottom) = (r.x, r.right(), r.y, r.bottom());
    [
        (Seg { a: (left + rad, top), b: (right - rad, top), horizontal: true }, "top"),
        (Seg { a: (right, top + rad), b: (right, bottom - rad), horizontal: false }, "right"),
        (Seg { a: (right - rad, bottom), b: (left + rad, bottom), horizontal: true }, "bottom"),
        (Seg { a: (left, bottom - rad), b: (left, top + rad), horizontal: false }, "left"),
    ]
}

/// Two tenths-squared values, rounded once at the leaf.
fn sq_to_tenths(sq: i64) -> i32 {
    (sq as f64).sqrt().round() as i32
}

/// Run the audit. All iteration is over adapter order (document/canonical),
/// so the same geometry reports the same findings every time.
pub fn audit(scene: &AuditScene) -> Composition {
    let mut out = Composition::default();
    let routes = scene.routes;

    // Crossings and corridors, exempting shared semantic endpoints.
    for (i, left) in routes.iter().enumerate() {
        for right in routes.iter().skip(i + 1) {
            let shares_endpoint = [left.from.as_str(), left.to.as_str()]
                .iter()
                .any(|id| *id == right.from || *id == right.to);
            if shares_endpoint {
                continue;
            }
            let mut crossing = None;
            for l in left.points.windows(2) {
                for r in right.points.windows(2) {
                    let ls = Seg { a: l[0], b: l[1], horizontal: l[0].1 == l[1].1 };
                    let rs = Seg { a: r[0], b: r[1], horizontal: r[0].1 == r[1].1 };
                    if let Some(p) = ls.crosses(&rs) {
                        crossing = Some(p);
                        break;
                    }
                }
                if crossing.is_some() {
                    break;
                }
            }
            if let Some(p) = crossing {
                out.crossings
                    .push((left.name.clone(), right.name.clone(), p));
            }
            let mut longest: Option<(i32, Pt, Pt)> = None;
            for l in left.points.windows(2) {
                for r in right.points.windows(2) {
                    let ls = Seg { a: l[0], b: l[1], horizontal: l[0].1 == l[1].1 };
                    let rs = Seg { a: r[0], b: r[1], horizontal: r[0].1 == r[1].1 };
                    if let Some(hit) = ls.axis_overlap(&rs) {
                        if longest.is_none_or(|best| hit.0 > best.0) {
                            longest = Some(hit);
                        }
                    }
                }
            }
            if let Some((len, from, to)) = longest {
                if len >= CORRIDOR_MIN_OVERLAP {
                    out.corridors
                        .push((left.name.clone(), right.name.clone(), len, from, to));
                }
            }
        }
    }

    // Border runs: any positive colinear overlap with a trimmed frame side.
    for route in routes {
        for frame in scene.frames {
            for (border, side) in border_segments(frame) {
                let mut total = 0;
                for seg in segments(&route.points) {
                    if let Some((len, _, _)) = seg.axis_overlap(&border) {
                        total += len;
                    }
                }
                if total > 0 {
                    out.border_runs.push((
                        route.name.clone(),
                        frame.kind.to_string(),
                        side,
                        frame.label.clone(),
                        total,
                    ));
                }
            }
        }
    }

    // Label clearance: each label against every OTHER relationship. All
    // measurements feed the min metric; near-misses (within the showcase
    // cap) are kept for the profile-thresholded report.
    for (i, labeled) in routes.iter().enumerate() {
        let Some(label) = labeled.label else { continue };
        for (j, other) in routes.iter().enumerate() {
            if i == j {
                continue;
            }
            let mut nearest: Option<(i64, i32)> = None;
            for seg in segments(&other.points) {
                let sq = seg.rect_clearance_sq(&label);
                let hidden = seg.rect_intersection(&label);
                if nearest.is_none_or(|(best, _)| sq < best) {
                    nearest = Some((sq, hidden));
                }
            }
            if let Some((sq, hidden)) = nearest {
                out.metrics.min_label_clearance = Some(
                    out.metrics
                        .min_label_clearance
                        .map_or(sq, |best| best.min(sq)),
                );
                let cap = LABEL_CLEARANCE_SHOWCASE as i64;
                if sq < cap * cap {
                    out.label_clearances
                        .push((labeled.name.clone(), other.name.clone(), sq, hidden));
                }
            }
        }
    }

    // Rhythm issues and budgets.
    for route in routes {
        let seg_count = route.points.len().saturating_sub(1);
        let mut route_len = 0i64;
        for (idx, seg) in segments(&route.points).enumerate() {
            let len = seg.len();
            if len <= 0 {
                continue;
            }
            route_len += len as i64;
            out.metrics.min_segment = Some(out.metrics.min_segment.map_or(len, |b| b.min(len)));
            let interior = idx > 0 && idx + 1 < seg_count;
            if len < MICRO_SEGMENT {
                out.metrics.micro_segments += 1;
                out.rhythm
                    .push((route.name.clone(), "micro-segment", idx, interior, len));
            } else if interior && len < INTERIOR_SEGMENT {
                out.rhythm.push((
                    route.name.clone(),
                    "short-interior-segment",
                    idx,
                    interior,
                    len,
                ));
            }
            if len < INTERIOR_SEGMENT {
                out.metrics.short_segments += 1;
            }
        }
        let bends = route.points.len().saturating_sub(2);
        out.metrics.max_bends = out.metrics.max_bends.max(bends);
        if bends > MAX_BENDS {
            out.metrics.routes_over_bends += 1;
        }
        let direct = (route.points[route.points.len() - 1].0 - route.points[0].0).abs() as i64
            + (route.points[route.points.len() - 1].1 - route.points[0].1).abs() as i64;
        if direct > 0 {
            let stretch = route_len * 100 / direct;
            out.metrics.max_stretch_pct = Some(
                out.metrics
                    .max_stretch_pct
                    .map_or(stretch, |b| b.max(stretch)),
            );
            if stretch > MAX_STRETCH_PCT {
                out.metrics.routes_over_stretch += 1;
            }
        }
    }

    // Desktop readability.
    if let Some(r) = &scene.readability {
        if r.view_box_w > 0 && r.min_label_font > 0 {
            let projected = if r.view_box_w <= READER_WIDTH {
                r.min_label_font
            } else {
                r.min_label_font * READER_WIDTH / r.view_box_w
            };
            out.readability = Some((r.min_label_font, projected, projected < MIN_PROJECTED_FONT));
        }
    }
    out
}

impl Composition {
    /// Issues at error severity under this profile — the delivery bar.
    fn errors(&self, quality: Quality) -> usize {
        let border = self.border_runs.len();
        match quality {
            Quality::Standard => border,
            Quality::Showcase => {
                border
                    + self.crossings.len()
                    + self.corridors.len()
                    + self.rhythm.len()
                    + self.label_hits(quality).len()
                    + usize::from(self.readability_is_hit())
            }
        }
    }

    fn label_hits(&self, quality: Quality) -> Vec<&(String, String, i64, i32)> {
        let threshold = quality.label_clearance() as i64;
        self.label_clearances
            .iter()
            .filter(|(_, _, sq, _)| *sq < threshold * threshold)
            .collect()
    }

    fn readability_is_hit(&self) -> bool {
        self.readability
            .is_some_and(|(_, _, hit)| hit)
    }

    /// The receipt-facing report. `status` fails only on error-severity
    /// findings: standard fails on border runs alone, showcase on everything.
    pub fn report(&self, quality: Quality) -> Value {
        let mut issues: Vec<Value> = Vec::new();
        let mut warn = |code: &str, message: String, error: bool| {
            issues.push(json!({
                "severity": if error { "error" } else { "warning" },
                "code": code,
                "message": message,
            }));
        };
        let error_like = quality == Quality::Showcase;
        for (a, b, p) in &self.crossings {
            warn(
                "composition/proper-crossing",
                format!("{} crosses {} at ({}, {})", a, b, tx(p.0), tx(p.1)),
                error_like,
            );
        }
        for (a, b, len, from, to) in &self.corridors {
            warn(
                "composition/ambiguous-corridor",
                format!(
                    "{} shares a {}px corridor with {} from ({}, {}) to ({}, {})",
                    a,
                    tx(*len),
                    b,
                    tx(from.0),
                    tx(from.1),
                    tx(to.0),
                    tx(to.1)
                ),
                error_like,
            );
        }
        for (route, kind, side, label, len) in &self.border_runs {
            warn(
                "composition/border-run",
                format!(
                    "{} follows {} \"{}\" {} border for {}px",
                    route,
                    kind,
                    label,
                    side,
                    tx(*len)
                ),
                true,
            );
        }
        for hit in self.label_hits(quality) {
            warn(
                "composition/label-clearance",
                format!(
                    "label of {} is {}px from {} (minimum {}px)",
                    hit.0,
                    tx(sq_to_tenths(hit.2)),
                    hit.1,
                    tx(quality.label_clearance())
                ),
                error_like,
            );
        }
        for (route, code, idx, interior, len) in &self.rhythm {
            let floor = if *code == "micro-segment" {
                MICRO_SEGMENT
            } else {
                INTERIOR_SEGMENT
            };
            warn(
                &format!("composition/{}", code),
                format!(
                    "{} has a {}px {} segment #{} (floor {}px)",
                    route,
                    tx(*len),
                    if *interior { "interior" } else { "endpoint" },
                    idx,
                    tx(floor)
                ),
                error_like,
            );
        }
        if self.readability_is_hit() {
            if let Some((font, projected, _)) = self.readability {
                warn(
                    "composition/desktop-readability",
                    format!(
                        "node text projects to {}px at the {}px reader width (floor {}px)",
                        tx(projected),
                        tx(READER_WIDTH),
                        tx(MIN_PROJECTED_FONT)
                    ),
                    error_like,
                );
                let _ = font;
            }
        }
        let m = &self.metrics;
        let metrics = json!({
            "crossings": self.crossings.len(),
            "ambiguousCorridors": self.corridors.len(),
            "borderRuns": self.border_runs.len(),
            "labelClearanceIssues": self.label_hits(quality).len(),
            "minLabelClearancePx": self.metrics.min_label_clearance.map(|sq| tx(sq_to_tenths(sq))),
            "maxBends": m.max_bends,
            "routesOverSuggestedBends": m.routes_over_bends,
            "maxStretchPct": m.max_stretch_pct,
            "routesOverSuggestedStretch": m.routes_over_stretch,
            "minSegmentPx": m.min_segment.map(tx),
            "shortSegmentCount": m.short_segments,
            "microSegmentCount": m.micro_segments,
            "minProjectedNodeTextPx": self.readability.map(|(_, projected, _)| tx(projected)),
        });
        let errors = self.errors(quality);
        json!({
            "profile": quality.name(),
            "status": if self.clean(quality) { "pass" } else { "fail" },
            "errors": errors,
            "warnings": issues.len() - errors,
            "metrics": metrics,
            "issues": issues,
        })
    }

    /// The gate contract: no error-severity findings at this profile.
    pub fn clean(&self, quality: Quality) -> bool {
        self.errors(quality) == 0
    }

    /// Total findings at this profile (warnings included), for receipt
    /// check lines.
    pub fn findings(&self, quality: Quality) -> usize {
        self.report(quality)["issues"].as_array().map_or(0, |a| a.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(name: &str, from: &str, to: &str, points: &[Pt], label: Option<Rect>) -> AuditRoute {
        AuditRoute {
            name: name.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            points: points.to_vec(),
            label,
        }
    }

    fn frame(rect: Rect, radius: i32) -> AuditFrame {
        AuditFrame {
            kind: "group",
            label: "Core".to_string(),
            rect,
            radius,
        }
    }

    // 1000×1000px frame, 100px corner radius.
    const R: Rect = Rect { x: 0, y: 0, w: 10000, h: 10000 };

    #[test]
    fn crossing_is_detected_and_shared_endpoints_are_exempt() {
        let disjoint = [
            route("a->b", "a", "b", &[(0, 0), (1000, 0)], None),
            route("c->d", "c", "d", &[(500, -500), (500, 500)], None),
        ];
        let comp = audit(&AuditScene { routes: &disjoint, frames: &[], readability: None });
        assert_eq!(comp.crossings.len(), 1, "h × v interior crossing");
        // Same endpoints: the fan-out is topology, not a defect.
        let shared = [
            route("a->b", "a", "b", &[(0, 0), (1000, 0)], None),
            route("b->a", "a", "b", &[(500, -500), (500, 500)], None),
        ];
        let comp = audit(&AuditScene { routes: &shared, frames: &[], readability: None });
        assert!(comp.crossings.is_empty());
        assert!(comp.clean(Quality::Showcase));
    }

    #[test]
    fn touching_segments_are_not_proper_crossings() {
        // The vertical segment ends exactly on the horizontal one.
        let touching = [
            route("a->b", "a", "b", &[(0, 0), (1000, 0)], None),
            route("c->d", "c", "d", &[(500, -500), (500, 0)], None),
        ];
        let comp = audit(&AuditScene { routes: &touching, frames: &[], readability: None });
        assert!(comp.crossings.is_empty());
    }

    #[test]
    fn corridor_needs_the_minimum_overlap() {
        let colinear = [
            route("a->b", "a", "b", &[(0, 0), (2000, 0)], None),
            route("c->d", "c", "d", &[(1200, 0), (3000, 0)], None),
        ];
        let comp = audit(&AuditScene { routes: &colinear, frames: &[], readability: None });
        assert_eq!(comp.corridors.len(), 1, "800px of shared corridor");
        let short = [
            route("a->b", "a", "b", &[(0, 0), (2000, 0)], None),
            route("c->d", "c", "d", &[(1950, 0), (3000, 0)], None),
        ];
        let comp = audit(&AuditScene { routes: &short, frames: &[], readability: None });
        assert!(comp.corridors.is_empty(), "50px < 80px floor");
    }

    #[test]
    fn border_run_trims_rounded_corners() {
        // A route riding the top border inside the straight middle: a run.
        let scene = AuditScene {
            routes: &[route("a->b", "a", "b", &[(2000, 0), (8000, 0)], None)],
            frames: &[frame(R, 1000)],
            readability: None,
        };
        let comp = audit(&scene);
        assert_eq!(comp.border_runs.len(), 1);
        assert_eq!(comp.border_runs[0].2, "top");
        // The same route shifted to 2px above the border: not a run.
        let scene = AuditScene {
            routes: &[route("a->b", "a", "b", &[(2000, -20), (8000, -20)], None)],
            frames: &[frame(R, 1000)],
            readability: None,
        };
        assert!(audit(&scene).border_runs.is_empty());
        // Crossing the border perpendicularly is legitimate.
        let scene = AuditScene {
            routes: &[route("a->b", "a", "b", &[(5000, -2000), (5000, 2000)], None)],
            frames: &[frame(R, 1000)],
            readability: None,
        };
        assert!(audit(&scene).border_runs.is_empty());
        // Within the trimmed corner zone only (left of x=1000): the corner
        // is not a corridor.
        let scene = AuditScene {
            routes: &[route("a->b", "a", "b", &[(100, 0), (900, 0)], None)],
            frames: &[frame(R, 1000)],
            readability: None,
        };
        assert!(audit(&scene).border_runs.is_empty());
    }

    #[test]
    fn border_runs_are_errors_in_every_profile() {
        let scene = AuditScene {
            routes: &[route("a->b", "a", "b", &[(2000, 0), (8000, 0)], None)],
            frames: &[frame(R, 0)],
            readability: None,
        };
        let comp = audit(&scene);
        assert!(!comp.clean(Quality::Standard), "hard failure, standard too");
        assert_eq!(comp.report(Quality::Standard)["status"], "fail");
        assert_eq!(
            comp.report(Quality::Standard)["issues"][0]["severity"],
            "error"
        );
    }

    #[test]
    fn label_clearance_threshold_follows_the_profile() {
        // Label 30px (300 tenths) below a foreign route: above the 2px
        // standard bar, below the 4px showcase bar.
        let label = Rect { x: 0, y: 30, w: 400, h: 100 };
        let scene = AuditScene {
            routes: &[
                route("a->b", "a", "b", &[(0, 0), (1000, 0)], Some(label)),
                route("c->d", "c", "d", &[(0, 0), (1000, 0)], None),
            ],
            frames: &[],
            readability: None,
        };
        let comp = audit(&scene);
        assert!(comp.label_hits(Quality::Standard).is_empty());
        assert_eq!(comp.label_hits(Quality::Showcase).len(), 1);
        // Its own relationship is exempt.
        let own = AuditScene {
            routes: &[route("a->b", "a", "b", &[(0, 0), (1000, 0)], Some(Rect { x: 0, y: 5, w: 400, h: 100 }))],
            frames: &[],
            readability: None,
        };
        assert!(audit(&own).label_hits(Quality::Showcase).is_empty());
    }

    #[test]
    fn rhythm_flags_micro_and_short_interior_segments() {
        // 3-segment route: the 12px middle run is a short interior segment
        // (the 60px tail is an endpoint, under 16px but allowed).
        let zig = [route("a->b", "a", "b", &[(0, 0), (600, 0), (600, 120), (1200, 120)], None)];
        let comp = audit(&AuditScene { routes: &zig, frames: &[], readability: None });
        assert_eq!(comp.rhythm.len(), 1);
        assert_eq!(comp.rhythm[0].1, "short-interior-segment");
        assert_eq!(comp.metrics.max_bends, 2);
        // A 5px segment anywhere is a micro segment.
        let tiny = [route("a->b", "a", "b", &[(0, 0), (50, 0), (50, 600), (1200, 600)], None)];
        let comp = audit(&AuditScene { routes: &tiny, frames: &[], readability: None });
        assert_eq!(comp.rhythm.len(), 1);
        assert_eq!(comp.rhythm[0].1, "micro-segment");
    }

    #[test]
    fn readability_projects_the_reader_width() {
        let wide = AuditScene {
            routes: &[route("a->b", "a", "b", &[(0, 0), (2000, 0)], None)],
            frames: &[],
            readability: Some(Readability { view_box_w: 18600, min_label_font: 80 }),
        };
        let comp = audit(&wide);
        assert_eq!(comp.report(Quality::Showcase)["metrics"]["minProjectedNodeTextPx"], "4");
        assert!(comp.readability_is_hit(), "8px font halves to 4px on a 2× canvas");
        let narrow = AuditScene {
            routes: &[route("a->b", "a", "b", &[(0, 0), (2000, 0)], None)],
            frames: &[],
            readability: Some(Readability { view_box_w: 9300, min_label_font: 80 }),
        };
        assert!(!audit(&narrow).readability_is_hit(), "reader-width canvas: no shrink");
    }

    #[test]
    fn empty_scene_is_clean_and_report_is_deterministic() {
        let scene = AuditScene { routes: &[], frames: &[], readability: None };
        let comp = audit(&scene);
        assert!(comp.clean(Quality::Showcase));
        let r1 = comp.report(Quality::Showcase);
        let r2 = comp.report(Quality::Showcase);
        assert_eq!(r1, r2);
        assert_eq!(r1["status"], "pass");
    }
}
