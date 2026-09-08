//! Graph layout engine core — the mechanism half of docgen's mechanism/
//! policy split. Family adapters (workflow today; architecture, dataflow,
//! lifecycle later) supply vocabulary and constraints; this module owns
//! everything geometric: rectangle predicates, the rank-column constraint
//! solver, orthogonal route planning over a fixed candidate vocabulary with
//! a deterministic cost order, and edge-label placement.
//!
//! Adapted from archify's readable-v2 workflow compiler (MIT): the same
//! candidate families, feasibility gates, cost priority, and bounded
//! outside-right escalation — rewritten in integer tenths-of-a-pixel
//! arithmetic. Integers make every predicate exact, so the epsilon fences
//! the reference implementation needs around float drift do not exist here;
//! the one irrational quantity (Euclidean distance) is rounded once at the
//! leaves. No clocks, no randomness, no hash-iteration order: same input,
//! same bytes.

/// Tenths of a pixel per pixel.
pub const PX: i32 = 10;

/// Six ranks, flowing left to right.
pub const COLUMN_COUNT: usize = 6;
const BASELINE_PITCH: i32 = 1200;
const COLUMN_START: i32 = 940;

/// Hard rhythm: a two-point route is at least 28px; endpoint stubs 8px;
/// interior segments 16px.
const DIRECT_MIN: i32 = 280;
const STUB_MIN: i32 = 80;
const INTERIOR_MIN: i32 = 160;
/// Outward stub length feeding corridor candidates.
const STUB_LEN: i32 = 160;
/// What the feedback request asks the solver for — archify demands 32px on
/// re-solve, one notch above the 28px static clearance the adapter's
/// constraint assembly applies.
const RANK_GAP_CLEARANCE: i32 = 320;
/// Lane clearance a cross-lane route demands.
const LANE_GAP_MIN: i32 = 320;
/// Label-vs-route proximity threshold in the cost vector.
const LABEL_ROUTE_THRESHOLD: i32 = 80;
/// Feasibility clearance against already-placed labels.
const PLACED_LABEL_CLEARANCE: i32 = 40;
/// Overlap tolerance for label rectangles (may overlap this much).
const LABEL_TOLERANCE: i32 = -20;
/// Outside-right escalation bounds.
const ESCALATE_PROBES: usize = 7;
const ESCALATE_BISECT: usize = 24;

pub type Pt = (i32, i32);

fn coord(p: Pt, axis: usize) -> i32 {
    if axis == 0 { p.0 } else { p.1 }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn cx(&self) -> i32 {
        self.x + self.w / 2
    }
    pub fn cy(&self) -> i32 {
        self.y + self.h / 2
    }
}

/// Correctly-rounded integer hypotenuse — the only irrational quantity in
/// the engine; IEEE sqrt is correctly rounded everywhere we ship.
fn hypot(dx: i32, dy: i32) -> i32 {
    let dx = dx as i64;
    let dy = dy as i64;
    (((dx * dx + dy * dy) as f64).sqrt() + 0.5) as i32
}

// ---------------------------------------------------------------------------
// sides
// ---------------------------------------------------------------------------

/// Declared in archify's side iteration order (right, bottom, left, top) so
/// derived ordinals tie-break identically.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum Side {
    Right,
    Bottom,
    Left,
    Top,
}

pub const SIDE_ORDER: [Side; 4] = [Side::Right, Side::Bottom, Side::Left, Side::Top];

impl Side {
    pub fn parse(s: &str) -> Option<Side> {
        match s {
            "right" => Some(Side::Right),
            "bottom" => Some(Side::Bottom),
            "left" => Some(Side::Left),
            "top" => Some(Side::Top),
            _ => None,
        }
    }

    pub fn horizontal(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }

    /// Unit vector pointing away from the node.
    fn outward(self) -> (i32, i32) {
        match self {
            Side::Right => (1, 0),
            Side::Bottom => (0, 1),
            Side::Left => (-1, 0),
            Side::Top => (0, -1),
        }
    }
}

/// The point where an edge attaches to a box on the given side.
pub fn anchor(rect: &Rect, side: Side) -> Pt {
    match side {
        Side::Left => (rect.x, rect.cy()),
        Side::Right => (rect.right(), rect.cy()),
        Side::Top => (rect.cx(), rect.y),
        Side::Bottom => (rect.cx(), rect.bottom()),
    }
}

fn outward_stub(pt: Pt, side: Side) -> Pt {
    let (dx, dy) = side.outward();
    (pt.0 + dx * STUB_LEN, pt.1 + dy * STUB_LEN)
}

/// Natural sides by relative position: leave on the side facing the target,
/// arrive on the side facing the source.
pub fn default_sides(from: &Rect, to: &Rect) -> (Side, Side) {
    let from_side = if to.cx() < from.cx() {
        Side::Left
    } else if to.cx() > from.cx() {
        Side::Right
    } else if to.cy() > from.cy() {
        Side::Bottom
    } else {
        Side::Top
    };
    let to_side = if to.cx() < from.cx() {
        Side::Right
    } else if to.cx() > from.cx() {
        Side::Left
    } else if to.cy() > from.cy() {
        Side::Top
    } else {
        Side::Bottom
    };
    (from_side, to_side)
}

// ---------------------------------------------------------------------------
// predicates
// ---------------------------------------------------------------------------

/// True when the rectangles are closer than `gap` on both axes. Negative
/// gaps allow that much overlap.
pub fn rects_overlap(a: &Rect, b: &Rect, gap: i32) -> bool {
    !(a.right() + gap <= b.x
        || b.right() + gap <= a.x
        || a.bottom() + gap <= b.y
        || b.bottom() + gap <= a.y)
}

/// Axis-aligned segment vs rect inflated by `gap`. Routes only ever hold
/// orthogonal segments, so a closed-form test replaces general
/// segment-intersection machinery.
pub fn segment_intersects_rect(a: Pt, b: Pt, rect: &Rect, gap: i32) -> bool {
    let (bx1, bx2, by1, by2) = (
        rect.x - gap,
        rect.right() + gap,
        rect.y - gap,
        rect.bottom() + gap,
    );
    if a.0 == b.0 {
        let (y1, y2) = if a.1 <= b.1 { (a.1, b.1) } else { (b.1, a.1) };
        a.0 >= bx1 && a.0 <= bx2 && y1 <= by2 && y2 >= by1
    } else if a.1 == b.1 {
        let (x1, x2) = if a.0 <= b.0 { (a.0, b.0) } else { (b.0, a.0) };
        a.1 >= by1 && a.1 <= by2 && x1 <= bx2 && x2 >= bx1
    } else {
        // Non-orthogonal segments are a router bug; be conservative.
        debug_assert!(false, "route segment must be orthogonal");
        true
    }
}

fn point_rect_distance(p: Pt, rect: &Rect) -> i32 {
    let dx = (rect.x - p.0).max(0).max(p.0 - rect.right());
    let dy = (rect.y - p.1).max(0).max(p.1 - rect.bottom());
    hypot(dx, dy)
}

fn point_segment_distance(p: Pt, a: Pt, b: Pt) -> i32 {
    if a.0 == b.0 {
        let (y1, y2) = if a.1 <= b.1 { (a.1, b.1) } else { (b.1, a.1) };
        let py = p.1.clamp(y1, y2);
        hypot(p.0 - a.0, p.1 - py)
    } else {
        let (x1, x2) = if a.0 <= b.0 { (a.0, b.0) } else { (b.0, a.0) };
        let px = p.0.clamp(x1, x2);
        hypot(p.0 - px, p.1 - a.1)
    }
}

/// Gap between an orthogonal segment and a rect: 0 when intersecting, else
/// the narrowest separation between them.
pub fn segment_rect_clearance(a: Pt, b: Pt, rect: &Rect) -> i32 {
    if segment_intersects_rect(a, b, rect, 0) {
        return 0;
    }
    let corners = [
        (rect.x, rect.y),
        (rect.right(), rect.y),
        (rect.right(), rect.bottom()),
        (rect.x, rect.bottom()),
    ];
    let mut best = point_rect_distance(a, rect).min(point_rect_distance(b, rect));
    for c in corners {
        best = best.min(point_segment_distance(c, a, b));
    }
    best
}

/// Do two orthogonal segments cross at a proper interior point of both?
pub fn proper_axis_crossing(a: Pt, b: Pt, c: Pt, d: Pt) -> bool {
    let first_h = a.1 == b.1;
    let second_h = c.1 == d.1;
    if first_h == second_h {
        return false;
    }
    let (h1, h2, v1, v2) = if first_h { (a, b, c, d) } else { (c, d, a, b) };
    let (x, y) = (v1.0, h1.1);
    x > h1.0.min(h2.0) + 1
        && x < h1.0.max(h2.0) - 1
        && y > v1.1.min(v2.1) + 1
        && y < v1.1.max(v2.1) - 1
}

/// Length of the shared corridor when both segments run collinearly.
pub fn axis_overlap_length(a: Pt, b: Pt, c: Pt, d: Pt) -> i32 {
    let horizontal = a.1 == b.1 && c.1 == d.1 && a.1 == c.1;
    let vertical = a.0 == b.0 && c.0 == d.0 && a.0 == c.0;
    if !horizontal && !vertical {
        return 0;
    }
    let axis = if horizontal { 0 } else { 1 };
    let overlap = |p: Pt, q: Pt| {
        let lo = coord(p, axis).min(coord(q, axis));
        let hi = coord(p, axis).max(coord(q, axis));
        (lo, hi)
    };
    let (alo, ahi) = overlap(a, b);
    let (blo, bhi) = overlap(c, d);
    (ahi.min(bhi) - alo.max(blo)).max(0)
}

// ---------------------------------------------------------------------------
// route normalization and rhythm
// ---------------------------------------------------------------------------

/// Drop duplicate consecutive points and fold collinear runs.
pub fn normalize_route_points(points: &[Pt]) -> Vec<Pt> {
    let mut out: Vec<Pt> = Vec::with_capacity(points.len());
    for &p in points {
        if out.last() == Some(&p) {
            continue;
        }
        while out.len() >= 2 {
            let a = out[out.len() - 2];
            let b = out[out.len() - 1];
            // i64: outside-right escalation probes reach ~80k tenths, and the
            // fold products scale as delta² — i32 overflows there.
            let cross = (b.0 - a.0) as i64 * (p.1 - b.1) as i64
                - (b.1 - a.1) as i64 * (p.0 - b.0) as i64;
            let forward = (b.0 - a.0) as i64 * (p.0 - b.0) as i64
                + (b.1 - a.1) as i64 * (p.1 - b.1) as i64;
            if cross == 0 && forward >= 0 {
                out.pop();
            } else {
                break;
            }
        }
        out.push(p);
    }
    out
}

pub fn orthogonal(points: &[Pt]) -> bool {
    points.windows(2).all(|w| (w[0].0 == w[1].0) != (w[0].1 == w[1].1))
}

fn endpoint_side_ok(points: &[Pt], seg: usize, side: Side, is_source: bool) -> bool {
    let (a, b) = (points[seg], points[seg + 1]);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (along, across) = if side.horizontal() { (dx, dy) } else { (dy, dx) };
    // The source stub leaves along the side's outward direction; the target
    // stub arrives moving against it, into the box.
    let sign = if is_source { 1 } else { -1 };
    let outward_along = if side.horizontal() {
        side.outward().0 * sign
    } else {
        side.outward().1 * sign
    };
    across == 0 && along * outward_along > 0
}

pub fn route_honors_endpoint_sides(points: &[Pt], from_side: Side, to_side: Side) -> bool {
    if points.len() < 2 {
        return false;
    }
    endpoint_side_ok(points, 0, from_side, true)
        && endpoint_side_ok(points, points.len() - 2, to_side, false)
}

pub fn route_meets_hard_rhythm(points: &[Pt]) -> bool {
    if points.len() == 2 {
        return hypot(points[1].0 - points[0].0, points[1].1 - points[0].1) >= DIRECT_MIN;
    }
    for i in 0..points.len() - 1 {
        let len = (points[i + 1].0 - points[i].0).abs() + (points[i + 1].1 - points[i].1).abs();
        let endpoint = i == 0 || i == points.len() - 2;
        if len < if endpoint { STUB_MIN } else { INTERIOR_MIN } {
            return false;
        }
    }
    true
}

/// Interior segments must not re-enter the box they left; all but the last
/// must not clip the box they are about to enter.
pub fn route_clears_endpoint_nodes(points: &[Pt], from: &Rect, to: &Rect) -> bool {
    let last = points.len() - 2;
    for i in 0..=last {
        if i > 0 && segment_intersects_rect(points[i], points[i + 1], from, 0) {
            return false;
        }
        if i < last && segment_intersects_rect(points[i], points[i + 1], to, 0) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// rank-column solver
// ---------------------------------------------------------------------------

pub struct ColConstraint {
    pub from: usize,
    pub to: usize,
    pub minimum: i32,
}

/// Forward-pass constraint propagation over the rank lattice: seed columns
/// at the baseline pitch, then push each column right of the largest
/// constraint demanding it.
pub fn solve_columns(constraints: &[ColConstraint]) -> [i32; COLUMN_COUNT] {
    let mut cols = [0i32; COLUMN_COUNT];
    for (i, c) in cols.iter_mut().enumerate() {
        *c = COLUMN_START + i as i32 * BASELINE_PITCH;
    }
    let mut ordered: Vec<&ColConstraint> = constraints
        .iter()
        .filter(|c| c.from < c.to && c.to < COLUMN_COUNT)
        .collect();
    ordered.sort_by(|a, b| {
        a.to.cmp(&b.to)
            .then(a.from.cmp(&b.from))
            .then(a.minimum.cmp(&b.minimum))
    });
    for to in 1..COLUMN_COUNT {
        for c in &ordered {
            if c.to != to {
                continue;
            }
            let candidate = cols[c.from] + c.minimum;
            if candidate > cols[to] {
                cols[to] = candidate;
            }
        }
    }
    // A pushed column drags its successors: keep every gap at least the
    // baseline pitch so the lattice stays uniformly spaced.
    for i in 1..COLUMN_COUNT {
        if cols[i] < cols[i - 1] + BASELINE_PITCH {
            cols[i] = cols[i - 1] + BASELINE_PITCH;
        }
    }
    cols
}

// ---------------------------------------------------------------------------
// label placement
// ---------------------------------------------------------------------------

/// Where an edge label sits: on the best segment — horizontal beats longer
/// beats earlier — centered; a horizontal segment lifts the label 10px, a
/// vertical one keeps it dead center.
pub fn label_point(points: &[Pt]) -> Pt {
    let mut segments: Vec<(bool, i32, usize)> = points[..points.len() - 1]
        .iter()
        .enumerate()
        .map(|(i, &p)| {
            let q = points[i + 1];
            (p.1 == q.1, (q.0 - p.0).abs() + (q.1 - p.1).abs(), i)
        })
        .collect();
    segments.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    let seg = segments[0].2;
    let (a, b) = (points[seg], points[seg + 1]);
    let mid = ((a.0 + b.0) / 2, (a.1 + b.1) / 2);
    if a.0 == b.0 {
        mid
    } else {
        (mid.0, mid.1 - 100)
    }
}

pub fn label_rect(width: i32, at: Pt) -> Rect {
    Rect {
        x: at.0 - width / 2,
        y: at.1 - 100,
        w: width,
        h: 140,
    }
}

// ---------------------------------------------------------------------------
// router
// ---------------------------------------------------------------------------

/// An edge already routed in this compile — the accumulated world a new
/// candidate must clear.
pub struct PlacedRoute {
    pub points: Vec<Pt>,
    pub label: Option<Rect>,
    /// Shares an endpoint with the edge being planned (interaction metrics
    /// skip these).
    pub shares_endpoint: bool,
}

/// Corridor positions, in scene coordinates, supplied by the adapter.
pub struct Corridors {
    pub lane_gap_y: i32,
    pub top_y: i32,
    pub bottom_y: i32,
    pub outside_left_x: i32,
    pub outside_right_x: i32,
}

pub struct RouteScene<'a> {
    /// All node rectangles; the request names its endpoints by index.
    pub nodes: &'a [Rect],
    /// Band obstacles (phase strips in the workflow family): horizontal
    /// runs and labels must clear them; vertical legs may cross.
    pub obstacles: &'a [Rect],
    pub placed: &'a [PlacedRoute],
    /// (minimum canvas width, current canvas height) for growth costs and
    /// the origin-fits check.
    pub canvas: (i32, i32),
}

pub enum RouteKind<'a> {
    Auto,
    Preset(&'a str),
}

/// Engine self-repair channel: infeasibility that re-solving with a wider
/// gap could fix. The adapter catches this, records the constraint, and
/// recompiles — bounded rounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    RankGap {
        from_col: usize,
        to_col: usize,
        minimum: i32,
    },
    LaneGap {
        minimum: i32,
    },
}

#[derive(Debug)]
pub enum RouteError {
    Feedback(Feedback),
    /// No feasible route even after escalation.
    Exhausted {
        families: Vec<&'static str>,
    },
    /// A preset route cannot be honored under the readability constraints.
    PresetConflict {
        preset: &'static str,
    },
}

pub struct PlannedRoute {
    pub points: Vec<Pt>,
}

/// A disclosed engine self-repair: the authored preset was geometrically
/// infeasible, and this verified substitute (planned through the same
/// readability gates as any route) took its place.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RouteRepair {
    pub edge: String,
    pub requested: String,
    pub substituted: String,
}

/// Plan one edge for a fixed-grid family (dataflow/lifecycle/architecture):
/// try the authored preset, and when it is geometrically infeasible walk the
/// family's semantic substitution ladder — every substitute planned through
/// the same feasibility gates, first verified wins. The caller names the
/// edge and builds the disclosure. Feedback is terminal here (a fixed grid
/// has no gap to widen), so it surfaces as the error.
#[allow(clippy::type_complexity)] // the (requested, substituted) disclosure pair
pub fn plan_grid_edge(
    mut req: RouteRequest,
    scene: &RouteScene,
    ladder: fn(&str) -> &'static [&'static str],
) -> Result<(PlannedRoute, Option<(&'static str, &'static str)>), RouteError> {
    let authored = match req.route {
        RouteKind::Preset(name) => name,
        RouteKind::Auto => return plan_route(&req, scene).map(|p| (p, None)),
    };
    // Intern into the preset vocabulary: validation guarantees the name is
    // one of PRESETS, and the disclosure/error carry 'static.
    let authored: &'static str = PRESETS
        .iter()
        .find(|p| **p == authored)
        .copied()
        .unwrap_or("auto");
    if let Ok(planned) = plan_route(&req, scene) {
        return Ok((planned, None));
    }
    for sub in ladder(authored) {
        req.route = if *sub == "auto" {
            RouteKind::Auto
        } else {
            RouteKind::Preset(sub)
        };
        if let Ok(planned) = plan_route(&req, scene) {
            return Ok((planned, Some((authored, sub))));
        }
    }
    Err(RouteError::PresetConflict { preset: authored })
}

/// Field order IS the cost priority (archify's
/// READABLE_CANDIDATE_COST_PRIORITY, minus the legacy-visual-continuity
/// term — we have no legacy to stay continuous with).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CandidateCost {
    pub forward_reverse_px: i64,
    pub proper_crossings: i64,
    pub shared_corridor_px: i64,
    pub label_route_deficit: i64,
    pub interior_28_deficit: i64,
    pub bend_count: i64,
    pub stretch_milli: i64,
    pub canvas_growth_px: i64,
    pub port_displacement_milli: i64,
    pub ordinal: i64,
}

const FAMILIES: [&str; 9] = [
    "facing-straight",
    "horizontal-then-vertical",
    "vertical-then-horizontal",
    "lane-gap-corridor",
    "column-gap-corridor",
    "outside-left",
    "outside-right",
    "top-corridor",
    "bottom-corridor",
];

/// Route preset names across the family vocabularies. The channel names
/// beyond the workflow six are aliases riding the same adapter-supplied
/// corridors (`top-channel` is `up-channel` under the dataflow/lifecycle
/// naming; `left`/`right`/`vertical-channel` are vertical runs);
/// `orthogonal-h/v` are the architecture doglegs.
const PRESETS: [&str; 12] = [
    "straight",
    "drop",
    "outside-right",
    "return-left",
    "bottom-channel",
    "up-channel",
    "top-channel",
    "vertical-channel",
    "left-channel",
    "right-channel",
    "orthogonal-h",
    "orthogonal-v",
];

pub struct RouteRequest<'a> {
    pub from_idx: usize,
    pub to_idx: usize,
    pub from: &'a Rect,
    pub to: &'a Rect,
    pub from_col: u8,
    pub to_col: u8,
    /// True when a backwards x-run should cost (forward edges only).
    pub forward: bool,
    /// Crosses lanes (feedback diagnostics).
    pub cross_lane: bool,
    /// Current lane gap (feedback diagnostics).
    pub lane_gap: i32,
    /// Measured label width, when the edge carries a label.
    pub label_width: Option<i32>,
    pub from_side_authored: Option<Side>,
    pub to_side_authored: Option<Side>,
    pub route: RouteKind<'a>,
    pub corridors: Corridors,
    /// Column x positions, for feedback diagnostics.
    pub col_xs: [i32; COLUMN_COUNT],
    /// Spread ports for the primary side pair, from the adapter's fan-out
    /// pass.
    pub primary_ports: Option<(Pt, Pt)>,
}

struct Scored {
    points: Vec<Pt>,
    cost: CandidateCost,
}

/// Plan one edge. Deterministic: candidates enumerate in a fixed family and
/// side order, costs compare lexicographically, and the ordinal breaks
/// remaining ties.
pub fn plan_route(req: &RouteRequest, scene: &RouteScene) -> Result<PlannedRoute, RouteError> {
    let natural = default_sides(req.from, req.to);
    let authored = (
        req.from_side_authored.unwrap_or(natural.0),
        req.to_side_authored.unwrap_or(natural.1),
    );

    // Side pairs: the primary (natural or authored) pair first, then the
    // full cross product, honoring authored sides. Deduplicated in order.
    let mut pairs: Vec<(Side, Side)> = vec![authored];
    for &f in &SIDE_ORDER {
        for &t in &SIDE_ORDER {
            if req.from_side_authored.is_some_and(|s| s != f) {
                continue;
            }
            if req.to_side_authored.is_some_and(|s| s != t) {
                continue;
            }
            pairs.push((f, t));
        }
    }
    let mut seen: Vec<(Side, Side)> = Vec::new();
    pairs.retain(|p| {
        if seen.contains(p) {
            false
        } else {
            seen.push(*p);
            true
        }
    });

    match &req.route {
        RouteKind::Auto => plan_auto(req, scene, &pairs, natural),
        RouteKind::Preset(name) => plan_preset(req, scene, &pairs, name),
    }
}

fn plan_auto(
    req: &RouteRequest,
    scene: &RouteScene,
    pairs: &[(Side, Side)],
    natural: (Side, Side),
) -> Result<PlannedRoute, RouteError> {
    let mut plans: Vec<Scored> = Vec::new();
    let mut feedback: Vec<(Feedback, usize)> = Vec::new();

    for (pair_ordinal, &(from_side, to_side)) in pairs.iter().enumerate() {
        let primary = pair_ordinal == 0;
        let start = if primary {
            req.primary_ports.map_or_else(|| anchor(req.from, from_side), |p| p.0)
        } else {
            anchor(req.from, from_side)
        };
        let end = if primary {
            req.primary_ports.map_or_else(|| anchor(req.to, to_side), |p| p.1)
        } else {
            anchor(req.to, to_side)
        };
        let natural_start = anchor(req.from, natural.0);
        let natural_end = anchor(req.to, natural.1);

        let scored = candidate_set(
            req,
            scene,
            start,
            end,
            from_side,
            to_side,
            pair_ordinal,
            natural_start,
            natural_end,
        );
        if scored.is_empty() {
            // No stock candidate fits this pair — escalate the outside-right
            // corridor, then classify the dead end for the feedback loop.
            if let Some(points) = escalate_outside_right(req, scene, start, end, from_side, to_side) {
                let ordinal = pair_ordinal as i64 * 9 + 6; // outside-right family slot
                plans.push(Scored {
                    cost: candidate_cost(req, scene, &points, ordinal, natural_start, natural_end),
                    points,
                });
            } else if let RouteError::Feedback(f) =
                diagnose_dead_end(req, start, end, from_side, to_side)
            {
                feedback.push((f, pair_ordinal));
            }
        } else {
            plans.extend(scored);
        }
    }

    plans.sort_by(|a, b| a.cost.cmp(&b.cost));
    if let Some(best) = plans.into_iter().next() {
        return Ok(PlannedRoute {
            points: best.points,
        });
    }
    feedback.sort_by_key(|(f, ordinal)| (feedback_priority(f), *ordinal));
    if let Some((f, _)) = feedback.into_iter().next() {
        return Err(RouteError::Feedback(f));
    }
    Err(RouteError::Exhausted {
        families: FAMILIES.to_vec(),
    })
}

fn feedback_priority(f: &Feedback) -> u8 {
    match f {
        Feedback::RankGap { .. } => 0,
        Feedback::LaneGap { .. } => 1,
    }
}

fn plan_preset(
    req: &RouteRequest,
    scene: &RouteScene,
    pairs: &[(Side, Side)],
    name: &str,
) -> Result<PlannedRoute, RouteError> {
    let natural_start = anchor(req.from, default_sides(req.from, req.to).0);
    let natural_end = anchor(req.to, default_sides(req.from, req.to).1);
    let mut plans: Vec<Scored> = Vec::new();
    for (ordinal, &(from_side, to_side)) in pairs.iter().enumerate() {
        let start = anchor(req.from, from_side);
        let end = anchor(req.to, to_side);
        let via = preset_via(req, name, start, end);
        let mut all = vec![start];
        all.extend(via);
        all.push(end);
        let points = normalize_route_points(&all);
        if feasible(req, scene, &points, from_side, to_side) {
            plans.push(Scored {
                cost: candidate_cost(req, scene, &points, ordinal as i64, natural_start, natural_end),
                points,
            });
        }
    }
    plans.sort_by(|a, b| a.cost.cmp(&b.cost));
    if let Some(best) = plans.into_iter().next() {
        return Ok(PlannedRoute {
            points: best.points,
        });
    }
    let preset: &'static str = PRESETS.iter().find(|p| **p == name).copied().unwrap_or("auto");
    Err(RouteError::PresetConflict { preset })
}

/// Preset vias. The channels ride the adapter's corridors, not local
/// arithmetic: policy owns where a channel runs, the mechanism only
/// enforces readability.
fn preset_via(req: &RouteRequest, name: &str, start: Pt, end: Pt) -> Vec<Pt> {
    match name {
        "straight" => Vec::new(),
        "drop" => vec![(start.0, req.corridors.lane_gap_y), (end.0, req.corridors.lane_gap_y)],
        "outside-right" => vec![
            (req.corridors.outside_right_x, start.1),
            (req.corridors.outside_right_x, end.1),
        ],
        "return-left" => {
            let x = req.from.x.min(req.to.x) - 280;
            vec![(x, start.1), (x, end.1)]
        }
        "bottom-channel" => vec![
            (start.0, req.corridors.bottom_y),
            (end.0, req.corridors.bottom_y),
        ],
        "up-channel" | "top-channel" => vec![
            (start.0, req.corridors.top_y),
            (end.0, req.corridors.top_y),
        ],
        "vertical-channel" => {
            let x = (start.0 + end.0) / 2;
            vec![(x, start.1), (x, end.1)]
        }
        "left-channel" => vec![
            (req.corridors.outside_left_x, start.1),
            (req.corridors.outside_left_x, end.1),
        ],
        "right-channel" => vec![
            (req.corridors.outside_right_x, start.1),
            (req.corridors.outside_right_x, end.1),
        ],
        "orthogonal-h" => vec![(end.0, start.1)],
        "orthogonal-v" => vec![(start.0, end.1)],
        _ => Vec::new(),
    }
}

/// The nine automatic candidate families for one (start, end, sides) triple.
#[allow(clippy::too_many_arguments)]
fn candidate_set(
    req: &RouteRequest,
    scene: &RouteScene,
    start: Pt,
    end: Pt,
    from_side: Side,
    to_side: Side,
    pair_ordinal: usize,
    natural_start: Pt,
    natural_end: Pt,
) -> Vec<Scored> {
    let mid_x = (start.0 + end.0) / 2;
    let via_y = |y: i32| {
        let ss = outward_stub(start, from_side);
        let es = outward_stub(end, to_side);
        vec![ss, (ss.0, y), (es.0, y), es]
    };
    let via_x = |x: i32| {
        let ss = outward_stub(start, from_side);
        let es = outward_stub(end, to_side);
        vec![ss, (x, ss.1), (x, es.1), es]
    };
    let c = &req.corridors;
    let vias: Vec<Vec<Pt>> = vec![
        Vec::new(),
        vec![(end.0, start.1)],
        vec![(start.0, end.1)],
        via_y(c.lane_gap_y),
        via_x(mid_x),
        via_x(c.outside_left_x),
        via_x(c.outside_right_x),
        via_y(c.top_y),
        via_y(c.bottom_y),
    ];
    let base = pair_ordinal as i64 * 9;
    let mut scored = Vec::new();
    for (family, via) in vias.into_iter().enumerate() {
        let mut all = Vec::with_capacity(via.len() + 2);
        all.push(start);
        all.extend(via);
        all.push(end);
        let points = normalize_route_points(&all);
        if !feasible(req, scene, &points, from_side, to_side) {
            continue;
        }
        scored.push(Scored {
            cost: candidate_cost(
                req,
                scene,
                &points,
                base + family as i64,
                natural_start,
                natural_end,
            ),
            points,
        });
    }
    scored
}

fn feasible(req: &RouteRequest, scene: &RouteScene, points: &[Pt], from_side: Side, to_side: Side) -> bool {
    orthogonal(points)
        && route_honors_endpoint_sides(points, from_side, to_side)
        && route_meets_hard_rhythm(points)
        && route_clears_endpoint_nodes(points, req.from, req.to)
        && route_clears_unrelated_nodes(req, scene, points)
        && route_clears_obstacles(scene, points)
        && route_label_clears_nodes(req, scene, points)
        && route_clears_placed_labels(req, scene, points)
        && points.iter().all(|&p| p.0 >= 0 && p.1 >= 0)
}

fn route_clears_unrelated_nodes(req: &RouteRequest, scene: &RouteScene, points: &[Pt]) -> bool {
    for (i, node) in scene.nodes.iter().enumerate() {
        if i == req.from_idx || i == req.to_idx {
            continue;
        }
        for w in points.windows(2) {
            if segment_intersects_rect(w[0], w[1], node, 20) {
                return false;
            }
        }
    }
    true
}

/// Band obstacles block horizontal runs but not vertical legs: a leg
/// crossing a phase strip reads as passing through the phase, while a
/// channel running inside the strip reads as belonging to it.
fn route_clears_obstacles(scene: &RouteScene, points: &[Pt]) -> bool {
    for band in scene.obstacles {
        for w in points.windows(2) {
            if w[0].1 != w[1].1 {
                continue;
            }
            let seg = Rect {
                x: w[0].0.min(w[1].0),
                y: w[0].1,
                w: (w[1].0 - w[0].0).abs(),
                h: 0,
            };
            if rects_overlap(&seg, band, 20) {
                return false;
            }
        }
    }
    true
}

fn route_label_clears_nodes(req: &RouteRequest, scene: &RouteScene, points: &[Pt]) -> bool {
    let Some(w) = req.label_width else {
        return true;
    };
    let rect = label_rect(w, label_point(points));
    scene.nodes.iter().all(|n| !rects_overlap(&rect, n, LABEL_TOLERANCE))
        && scene
            .obstacles
            .iter()
            .all(|o| !rects_overlap(&rect, o, LABEL_TOLERANCE))
}

fn route_clears_placed_labels(req: &RouteRequest, scene: &RouteScene, points: &[Pt]) -> bool {
    let Some(w) = req.label_width else {
        return true;
    };
    let rect = label_rect(w, label_point(points));
    for placed in scene.placed {
        if let Some(other) = &placed.label {
            if rects_overlap(&rect, other, LABEL_TOLERANCE) {
                return false;
            }
            for w2 in points.windows(2) {
                if segment_rect_clearance(w2[0], w2[1], other) < PLACED_LABEL_CLEARANCE {
                    return false;
                }
            }
        }
        for w2 in placed.points.windows(2) {
            if segment_rect_clearance(w2[0], w2[1], &rect) < PLACED_LABEL_CLEARANCE {
                return false;
            }
        }
    }
    true
}

fn candidate_cost(
    req: &RouteRequest,
    scene: &RouteScene,
    points: &[Pt],
    ordinal: i64,
    natural_start: Pt,
    natural_end: Pt,
) -> CandidateCost {
    let mut crossings = 0i64;
    let mut corridor = 0i64;
    let mut label_deficit = 0i64;
    for placed in scene.placed {
        if placed.shares_endpoint {
            continue;
        }
        for w in points.windows(2) {
            for v in placed.points.windows(2) {
                if proper_axis_crossing(w[0], w[1], v[0], v[1]) {
                    crossings += 1;
                }
                corridor += axis_overlap_length(w[0], w[1], v[0], v[1]) as i64;
            }
        }
    }
    if let Some(w) = req.label_width {
        let rect = label_rect(w, label_point(points));
        for placed in scene.placed {
            for v in placed.points.windows(2) {
                label_deficit += (LABEL_ROUTE_THRESHOLD - segment_rect_clearance(v[0], v[1], &rect)).max(0) as i64;
            }
            if let Some(other) = &placed.label {
                for w2 in points.windows(2) {
                    label_deficit +=
                        (LABEL_ROUTE_THRESHOLD - segment_rect_clearance(w2[0], w2[1], other)).max(0) as i64;
                }
            }
        }
    }

    let lengths: Vec<i32> = points[..points.len() - 1]
        .iter()
        .enumerate()
        .map(|(i, &p)| (points[i + 1].0 - p.0).abs() + (points[i + 1].1 - p.1).abs())
        .collect();
    let route_len: i64 = lengths.iter().map(|&l| l as i64).sum();
    let direct_len = ((points[points.len() - 1].0 - points[0].0).abs()
        + (points[points.len() - 1].1 - points[0].1).abs()) as i64;
    let mut forward_reverse = 0i64;
    if req.forward {
        for i in 0..points.len() - 1 {
            forward_reverse += (points[i].0 - points[i + 1].0).max(0) as i64;
        }
    }
    let interior_deficit: i64 = if lengths.len() > 2 {
        lengths[1..lengths.len() - 1]
            .iter()
            .map(|&l| (280 - l).max(0) as i64)
            .sum()
    } else {
        0
    };
    let min_x = points.iter().map(|p| p.0).min().unwrap_or(0);
    let max_x = points.iter().map(|p| p.0).max().unwrap_or(0);
    let min_y = points.iter().map(|p| p.1).min().unwrap_or(0);
    let max_y = points.iter().map(|p| p.1).max().unwrap_or(0);
    let canvas_growth = (-(min_x as i64)).max(0)
        + ((max_x as i64) - scene.canvas.0 as i64).max(0)
        + (-(min_y as i64)).max(0)
        + ((max_y as i64) - scene.canvas.1 as i64).max(0);
    let port_displacement = (points[0].0 - natural_start.0).abs() as i64
        + (points[0].1 - natural_start.1).abs() as i64
        + (points[points.len() - 1].0 - natural_end.0).abs() as i64
        + (points[points.len() - 1].1 - natural_end.1).abs() as i64;
    CandidateCost {
        forward_reverse_px: forward_reverse,
        proper_crossings: crossings,
        shared_corridor_px: corridor,
        label_route_deficit: label_deficit,
        interior_28_deficit: interior_deficit,
        bend_count: (points.len() as i64 - 2).max(0),
        stretch_milli: if direct_len > 0 {
            route_len * 1000 / direct_len
        } else {
            1000
        },
        canvas_growth_px: canvas_growth,
        port_displacement_milli: port_displacement * 1000,
        ordinal,
    }
}

/// When no stock candidate fits, push the outside-right corridor right in
/// doubling probes, then binary-search back to the tightest feasible x.
fn escalate_outside_right(
    req: &RouteRequest,
    scene: &RouteScene,
    start: Pt,
    end: Pt,
    from_side: Side,
    to_side: Side,
) -> Option<Vec<Pt>> {
    let base_x = req.corridors.outside_right_x;
    let build = |x: i32| {
        let ss = outward_stub(start, from_side);
        let es = outward_stub(end, to_side);
        normalize_route_points(&[start, ss, (x, ss.1), (x, es.1), es, end])
    };
    let candidate_label = req.label_width.map(|w| label_rect(w, label_point(&build(base_x))));

    // Rightward deficit: how far labels/segments reach past the corridor.
    let mut min_x = base_x;
    if let Some(rect) = &candidate_label {
        for node in scene.nodes {
            if !rects_overlap(rect, node, LABEL_TOLERANCE) {
                continue;
            }
            let deficit = node.right() - 20 - rect.x;
            if deficit > 0 {
                min_x = min_x.max(base_x + deficit * 2);
            }
        }
        for placed in scene.placed {
            if let Some(other) = &placed.label {
                if rects_overlap(rect, other, LABEL_TOLERANCE) {
                    let deficit = other.right() - 20 - rect.x;
                    if deficit > 0 {
                        min_x = min_x.max(base_x + deficit * 2);
                    }
                }
            }
            for v in placed.points.windows(2) {
                let clearance = segment_rect_clearance(v[0], v[1], rect);
                if clearance < PLACED_LABEL_CLEARANCE {
                    let deficit = v[0].0.max(v[1].0) + 40 - rect.x;
                    if deficit > 0 {
                        min_x = min_x.max(base_x + deficit * 2);
                    }
                }
            }
        }
    }
    let mut rightmost = base_x;
    for node in scene.nodes {
        rightmost = rightmost.max(node.right());
    }
    for placed in scene.placed {
        for &p in &placed.points {
            rightmost = rightmost.max(p.0);
        }
        if let Some(other) = &placed.label {
            rightmost = rightmost.max(other.right());
        }
    }
    let mut growth = (320)
        .max(candidate_label.map_or(0, |r| r.w))
        .max(rightmost + 160 - min_x);
    let mut last_infeasible = base_x;
    for _ in 0..ESCALATE_PROBES {
        if min_x > base_x {
            let expanded = build(min_x);
            if feasible(req, scene, &expanded, from_side, to_side) {
                let mut feasible_x = min_x;
                let mut feasible_points = expanded;
                let mut infeasible_x = last_infeasible;
                for _ in 0..ESCALATE_BISECT {
                    if feasible_x - infeasible_x <= 1 {
                        break;
                    }
                    let mid = (infeasible_x + feasible_x) / 2;
                    if mid >= feasible_x {
                        break;
                    }
                    let points = build(mid);
                    if feasible(req, scene, &points, from_side, to_side) {
                        feasible_x = mid;
                        feasible_points = points;
                    } else {
                        infeasible_x = mid;
                    }
                }
                return Some(feasible_points);
            }
            last_infeasible = min_x;
        }
        min_x += growth;
        growth *= 2;
    }
    None
}

/// Classify a dead end into re-solvable feedback when the geometry says a
/// wider rank or lane gap would have admitted the stock candidates.
fn diagnose_dead_end(
    req: &RouteRequest,
    start: Pt,
    end: Pt,
    from_side: Side,
    to_side: Side,
) -> RouteError {
    let horizontally_facing = (from_side == Side::Right && to_side == Side::Left && end.0 > start.0)
        || (from_side == Side::Left && to_side == Side::Right && start.0 > end.0);
    if horizontally_facing && req.from_col != req.to_col {
        let from_col = req.from_col.min(req.to_col) as usize;
        let to_col = req.from_col.max(req.to_col) as usize;
        let required = req.from.w / 2 + RANK_GAP_CLEARANCE + req.to.w / 2;
        let actual = req.col_xs[to_col] - req.col_xs[from_col];
        if actual + 1 < required {
            return RouteError::Feedback(Feedback::RankGap {
                from_col,
                to_col,
                minimum: required,
            });
        }
    }
    if req.cross_lane && req.lane_gap < LANE_GAP_MIN {
        return RouteError::Feedback(Feedback::LaneGap {
            minimum: LANE_GAP_MIN,
        });
    }
    RouteError::Exhausted {
        families: FAMILIES.to_vec(),
    }
}

/// SVG path data for an orthogonal polyline.
pub fn polyline_d(points: &[Pt]) -> String {
    let mut s = String::with_capacity(points.len() * 16);
    for (i, &(x, y)) in points.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(if i == 0 { "M " } else { "L " });
        s.push_str(&super::tx(x));
        s.push(' ');
        s.push_str(&super::tx(y));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn rects_overlap_gap_semantics() {
        let a = rect(0, 0, 100, 100);
        let b = rect(140, 0, 100, 100);
        assert!(!rects_overlap(&a, &b, 0), "40px apart is clear");
        assert!(rects_overlap(&a, &b, 50), "within the 50px gap");
        let touching = rect(100, 0, 100, 100);
        assert!(!rects_overlap(&a, &touching, 0), "edge-adjacent is clear");
        // Negative gap is a tolerance: an intruder 20px deep is clear under a
        // 30px tolerance, flagged under a 10px one.
        let intruder = rect(80, 0, 100, 100);
        assert!(!rects_overlap(&a, &intruder, -30));
        assert!(rects_overlap(&a, &intruder, -10));
    }

    #[test]
    fn normalize_folds_collinear_runs() {
        let folded = normalize_route_points(&[(0, 0), (50, 0), (100, 0), (100, 50), (100, 100)]);
        assert_eq!(folded, vec![(0, 0), (100, 0), (100, 100)]);
        let dedup = normalize_route_points(&[(0, 0), (0, 0), (0, 100)]);
        assert_eq!(dedup, vec![(0, 0), (0, 100)]);
        // A fold-back (U-turn) is preserved.
        assert_eq!(
            normalize_route_points(&[(0, 0), (100, 0), (50, 0)]),
            vec![(0, 0), (100, 0), (50, 0)]
        );
    }

    #[test]
    fn rhythm_boundaries() {
        assert!(route_meets_hard_rhythm(&[(0, 0), (280, 0)]));
        assert!(!route_meets_hard_rhythm(&[(0, 0), (279, 0)]));
        assert!(route_meets_hard_rhythm(&[(0, 0), (80, 0), (80, 200)]));
        assert!(!route_meets_hard_rhythm(&[(0, 0), (79, 0), (79, 200)]));
        assert!(!route_meets_hard_rhythm(&[(0, 0), (100, 0), (100, 115), (200, 115)]));
    }

    #[test]
    fn endpoint_sides_source_out_target_in() {
        // Source leaves rightward, target arrives moving left-to-right into
        // its left side.
        assert!(route_honors_endpoint_sides(
            &[(0, 0), (400, 0)],
            Side::Right,
            Side::Left
        ));
        // Target stub moving the wrong way (outward from target's left).
        assert!(!route_honors_endpoint_sides(
            &[(400, 0), (0, 0)],
            Side::Right,
            Side::Left
        ));
        // Source must not slew vertically on its stub.
        assert!(!route_honors_endpoint_sides(
            &[(0, 0), (0, 400)],
            Side::Right,
            Side::Left
        ));
    }

    #[test]
    fn crossing_and_corridor_metrics() {
        assert!(proper_axis_crossing((0, 0), (100, 0), (50, -50), (50, 50)));
        assert!(!proper_axis_crossing((0, 0), (100, 0), (50, 0), (50, 50)));
        assert!(!proper_axis_crossing((0, 0), (100, 0), (150, -50), (150, 50)));
        assert_eq!(axis_overlap_length((0, 0), (100, 0), (50, 0), (150, 0)), 50);
        assert_eq!(axis_overlap_length((0, 0), (100, 0), (0, 10), (100, 10)), 0);
    }

    #[test]
    fn solver_propagates_constraints_forward() {
        let cols = solve_columns(&[]);
        assert_eq!(cols[0], 940);
        assert_eq!(cols[5], 940 + 5 * 1200);
        // A wide node pair in columns 1..=3 pushes everything after it.
        let cols = solve_columns(&[ColConstraint {
            from: 1,
            to: 3,
            minimum: 4000,
        }]);
        assert_eq!(cols[1], 940 + 1200);
        assert_eq!(cols[3], (940 + 1200 + 4000).max(940 + 3 * 1200));
        assert_eq!(cols[5], cols[3] + 2 * 1200);
        // Constraints never move a column left of the lattice seed.
        let cols = solve_columns(&[ColConstraint {
            from: 2,
            to: 4,
            minimum: 100,
        }]);
        assert_eq!(cols[4], 940 + 4 * 1200);
    }

    #[test]
    fn label_prefers_longest_horizontal_segment() {
        // Dogleg with a long horizontal run and a short vertical one.
        let p = label_point(&[(0, 0), (500, 0), (500, 100)]);
        assert_eq!(p, (250, -100));
        // A short horizontal run still beats a longer vertical one.
        let p = label_point(&[(0, 0), (0, 500), (300, 500)]);
        assert_eq!(p, (150, 400));
        // An all-vertical route keeps its label dead center.
        assert_eq!(label_point(&[(0, 0), (0, 500)]), (0, 250));
        // Two-point route lifts the label 10px above the line.
        assert_eq!(label_point(&[(0, 0), (400, 0)]), (200, -100));
    }

    #[test]
    fn simple_adjacent_route_is_straight_and_deterministic() {
        let from = rect(0, 0, 920, 520);
        let to = rect(2000, 0, 920, 520);
        let scene = RouteScene {
            nodes: &[from, to],
            obstacles: &[],
            placed: &[],
            canvas: (4000, 2000),
        };
        let req = RouteRequest {
            from_idx: 0,
            to_idx: 1,
            from: &from,
            to: &to,
            from_col: 0,
            to_col: 1,
            forward: true,
            cross_lane: false,
            lane_gap: 200,
            label_width: None,
            from_side_authored: None,
            to_side_authored: None,
            route: RouteKind::Auto,
            corridors: Corridors {
                lane_gap_y: -160,
                top_y: 80,
                bottom_y: 1000,
                outside_left_x: -200,
                outside_right_x: 3200,
            },
            col_xs: solve_columns(&[]),
            primary_ports: None,
        };
        let a = plan_route(&req, &scene).unwrap();
        let b = plan_route(&req, &scene).unwrap();
        assert_eq!(a.points, b.points, "identical calls, identical route");
        assert_eq!(a.points, vec![(920, 260), (2000, 260)], "straight facing");
    }

    #[test]
    fn label_clearance_pushes_off_the_straight_line() {
        // A third node sits dead center between the endpoints; the straight
        // candidate is infeasible, so the router must dogleg around it.
        let from = rect(0, 0, 920, 520);
        let to = rect(3000, 0, 920, 520);
        let obstacle = rect(1800, 100, 320, 320);
        let scene = RouteScene {
            nodes: &[from, to, obstacle],
            obstacles: &[],
            placed: &[],
            canvas: (5000, 2000),
        };
        let req = RouteRequest {
            from_idx: 0,
            to_idx: 1,
            from: &from,
            to: &to,
            from_col: 0,
            to_col: 2,
            forward: true,
            cross_lane: false,
            lane_gap: 200,
            label_width: None,
            from_side_authored: None,
            to_side_authored: None,
            route: RouteKind::Auto,
            corridors: Corridors {
                lane_gap_y: -160,
                top_y: 80,
                bottom_y: 1000,
                outside_left_x: -200,
                outside_right_x: 4200,
            },
            col_xs: solve_columns(&[]),
            primary_ports: None,
        };
        let planned = plan_route(&req, &scene).unwrap();
        assert!(planned.points.len() > 2, "must dogleg: {:?}", planned.points);
        // And the chosen route actually clears the obstacle.
        for w in planned.points.windows(2) {
            assert!(
                !segment_intersects_rect(w[0], w[1], &obstacle, 20),
                "route clips obstacle: {:?}",
                planned.points
            );
        }
        assert!(orthogonal(&planned.points));
        assert!(route_meets_hard_rhythm(&planned.points));
    }

    #[test]
    fn preset_routes_honor_their_shape() {
        let from = rect(400, 400, 920, 520);
        let to = rect(3000, 1000, 920, 520);
        let scene = RouteScene {
            nodes: &[from, to],
            obstacles: &[],
            placed: &[],
            canvas: (5000, 2000),
        };
        let mk = |route| RouteRequest {
            from_idx: 0,
            to_idx: 1,
            from: &from,
            to: &to,
            from_col: 0,
            to_col: 2,
            forward: false,
            cross_lane: false,
            lane_gap: 200,
            label_width: None,
            from_side_authored: None,
            to_side_authored: None,
            route,
            corridors: Corridors {
                lane_gap_y: 960,
                top_y: 80,
                bottom_y: 1600,
                outside_left_x: 200,
                outside_right_x: 4200,
            },
            col_xs: solve_columns(&[]),
            primary_ports: None,
        };
        // return-left detours through x = min(from.x, to.x) - 28.
        let planned = plan_route(&mk(RouteKind::Preset("return-left")), &scene).unwrap();
        assert!(
            planned.points.iter().any(|&p| p.0 == 120),
            "return-left channel missing: {:?}",
            planned.points
        );
        // Channels ride the corridors the adapter supplies — top_y above the
        // node tops, bottom_y below the node bottoms, or the run crosses a
        // node and the gate (correctly) refuses.
        let planned = plan_route(&mk(RouteKind::Preset("bottom-channel")), &scene).unwrap();
        assert!(
            planned.points.iter().any(|&p| p.1 == 1600),
            "bottom channel corridor missing: {:?}",
            planned.points
        );
        let planned = plan_route(&mk(RouteKind::Preset("up-channel")), &scene).unwrap();
        assert!(
            planned.points.iter().any(|&p| p.1 == 80),
            "up channel corridor missing: {:?}",
            planned.points
        );
    }

    #[test]
    fn band_obstacles_block_horizontal_runs_not_vertical_legs() {
        // A phase band sits between two facing nodes at the height both the
        // straight line and the top corridor would run; every surviving
        // route must cross it vertically, never run inside it.
        let from = rect(0, 0, 920, 520);
        let to = rect(3000, 0, 920, 520);
        let band = rect(1000, 100, 1200, 300);
        let scene = RouteScene {
            nodes: &[from, to],
            obstacles: &[band],
            placed: &[],
            canvas: (5000, 2000),
        };
        let req = RouteRequest {
            from_idx: 0,
            to_idx: 1,
            from: &from,
            to: &to,
            from_col: 0,
            to_col: 2,
            forward: true,
            cross_lane: false,
            lane_gap: 200,
            label_width: None,
            from_side_authored: None,
            to_side_authored: None,
            route: RouteKind::Auto,
            corridors: Corridors {
                lane_gap_y: -160,
                top_y: 200, // deliberately inside the band
                bottom_y: 1000,
                outside_left_x: -200,
                outside_right_x: 4200,
            },
            col_xs: solve_columns(&[]),
            primary_ports: None,
        };
        let planned = plan_route(&req, &scene).unwrap();
        for w in planned.points.windows(2) {
            if w[0].1 == w[1].1 {
                let seg = Rect {
                    x: w[0].0.min(w[1].0),
                    y: w[0].1,
                    w: (w[1].0 - w[0].0).abs(),
                    h: 0,
                };
                assert!(
                    !rects_overlap(&seg, &band, 20),
                    "horizontal run inside the band: {:?}",
                    planned.points
                );
            }
        }
    }

    #[test]
    fn labels_may_not_park_on_a_band() {
        let from = rect(0, 0, 920, 520);
        let to = rect(3000, 0, 920, 520);
        let band = rect(1000, 100, 1200, 300);
        let scene = RouteScene {
            nodes: &[from, to],
            obstacles: &[band],
            placed: &[],
            canvas: (5000, 2000),
        };
        let mut req = RouteRequest {
            from_idx: 0,
            to_idx: 1,
            from: &from,
            to: &to,
            from_col: 0,
            to_col: 2,
            forward: true,
            cross_lane: false,
            lane_gap: 200,
            label_width: Some(436),
            from_side_authored: None,
            to_side_authored: None,
            route: RouteKind::Auto,
            corridors: Corridors {
                lane_gap_y: -160,
                top_y: 80,
                bottom_y: 1000,
                outside_left_x: -200,
                outside_right_x: 4200,
            },
            col_xs: solve_columns(&[]),
            primary_ports: None,
        };
        // A route running 5px below the band is clear, but its lifted label
        // rect lands inside the band — infeasible with a label, fine
        // without one.
        let below_band = [(0, 450), (3900, 450)];
        assert!(!feasible(&req, &scene, &below_band, Side::Right, Side::Left));
        req.label_width = None;
        assert!(feasible(&req, &scene, &below_band, Side::Right, Side::Left));
    }
}
