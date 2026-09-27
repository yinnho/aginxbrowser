//! 批240 / obscura#1100: SVGGeometryElement measurement — the engine behind
//! `getTotalLength` / `getPointAtLength` (split from svg.rs so the parent
//! rides under the layering audit's god-file ratchet cap; text/wrap.rs
//! precedent). Pure geometry on user-unit path data: no DOM, no styles.

pub(super) enum Tok {
    Cmd(char),
    Num(f32),
}

/// Char-based lexer — path data routinely glues commands to numbers
/// ("M0,0L100,0"), which whitespace/comma splitting mangles.
pub(super) fn lex_path(d: &str) -> Vec<Tok> {
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

// ---------------------------------------------------------------------------
// 批240 / obscura#1100: SVGGeometryElement measurement (getTotalLength /
// getPointAtLength). Chrome reads these off SkPathMeasure (blink
// Path::length → SkPathMeasure(resScale 1)), a chord-subdivision measure —
// so neither the analytic perimeter nor a kappa-bezier approximation
// reproduces the numbers: circle r=25 reads 156.0674 (not 2πr=157.0796),
// rounded 100×50 rx=10 reads 282.427. The implementation mirrors what the
// measure actually does, per shape family:
//
// - ovals (circle/ellipse/rounded-rect corners): Skia stores them as
//   rational quadratics (conics, weight √2/2 — an EXACT quarter ellipse) and
//   measures by recursive halving with the Chebyshev curvature gate
//   (tolerance 0.5), accumulating endpoint-to-endpoint chords. Ported
//   verbatim below (`compute_conic`); validated against Chrome to ~1e-5.
// - explicit `d` beziers: Chrome's own measure runs finer than the
//   published algorithm (160.7157 vs its chord value 160.5687 for probe p2),
//   so the exact chord table is unreproducible — GL8 analytic quadrature
//   lands within 0.02 of Chrome on every probe, the tighter bound.
// ---------------------------------------------------------------------------

/// One measurable path segment. Arcs in `d` degrade to endpoint lines (the
/// paint model's own v1 posture); ovals arrive as Skia's exact conic
/// quarters with their precomputed chord spans.
enum MeasSeg {
    Line([f32; 4]),
    Cubic([f32; 8]),
    Conic {
        /// p0, control, p1 (x,y pairs).
        pts: [f32; 6],
        /// Rational weight (√2/2 for a quarter ellipse).
        w: f32,
        /// Skia's accepted chord spans, in order (see [`compute_conic`]).
        spans: Vec<ConicSpan>,
    },
}

/// One accepted span of the conic chord measure: end parameter (Skia's
/// integer t-space rescaled to 0..1) and the chord's own length.
struct ConicSpan {
    t1: f32,
    len: f32,
}

/// 8-node Gauss–Legendre nodes/weights on [-1, 1] — published constants;
/// f32 can't hold the digits but rounding them would drift the quadrature.
#[allow(clippy::excessive_precision)]
const GL8_X: [f32; 8] = [
    -0.9602898564975363, -0.7966664774136267, -0.5255324099163290, -0.1834346424956498,
    0.1834346424956498, 0.5255324099163290, 0.7966664774136267, 0.9602898564975363,
];
#[allow(clippy::excessive_precision)]
const GL8_W: [f32; 8] = [
    0.1012285362903763, 0.2223810344533745, 0.3137066458778873, 0.3626837833783620,
    0.3626837833783620, 0.3137066458778873, 0.2223810344533745, 0.1012285362903763,
];

/// The kappa constant is gone with the cubic approximation: ovals arrive as
/// exact rational quadratics instead. Skia's measure parameters, verbatim
/// from SkContourMeasure.cpp:
const K_MAX_T: i32 = 0x3FFF_FFFF;
/// CHEAP_DIST_LIMIT (SK_Scalar1/2) at the default resScale 1.
const CHEAP_DIST_LIMIT: f32 = 0.5;
const MAX_RECURSION_DEPTH: i32 = 8;
/// Rational weight of an exact quarter ellipse (cos 45°).
const QUARTER_W: f32 = std::f32::consts::FRAC_1_SQRT_2;

fn t_to_f(t: i32) -> f32 {
    t as f32 * (1.0 / K_MAX_T as f32)
}

/// `tspan_big_enough`: tspan >> 10 != 0.
fn tspan_big_enough(tspan: i32) -> bool {
    (tspan >> 10) != 0
}

/// Rational-quadratic evaluation (SkConic::evalAt): B(t) with basis weights
/// (u², 2uw t, t²) normalized by their sum.
fn conic_eval(p: &[f32; 6], w: f32, t: f32) -> (f32, f32) {
    let u = 1.0 - t;
    let (b0, b1, b2) = (u * u, 2.0 * u * t * w, t * t);
    let s = b0 + b1 + b2;
    (
        (b0 * p[0] + b1 * p[2] + b2 * p[4]) / s,
        (b0 * p[1] + b1 * p[3] + b2 * p[5]) / s,
    )
}

impl MeasSeg {
    fn eval(&self, t: f32) -> (f32, f32) {
        match self {
            MeasSeg::Line([x0, y0, x1, y1]) => (x0 + (x1 - x0) * t, y0 + (y1 - y0) * t),
            MeasSeg::Cubic(p) => {
                let (p0, p1, p2, p3) = ((p[0], p[1]), (p[2], p[3]), (p[4], p[5]), (p[6], p[7]));
                let mt = 1.0 - t;
                let (a, b, c, d) = (
                    mt * mt * mt,
                    3.0 * mt * mt * t,
                    3.0 * mt * t * t,
                    t * t * t,
                );
                (
                    a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0,
                    a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1,
                )
            }
            MeasSeg::Conic { pts, w, .. } => conic_eval(pts, *w, t),
        }
    }

    /// Cubic derivative for the GL8 integrand (conics never integrate —
    /// their spans carry the measured length already).
    fn derivative(&self, t: f32) -> (f32, f32) {
        match self {
            MeasSeg::Line([x0, y0, x1, y1]) => (x1 - x0, y1 - y0),
            MeasSeg::Cubic(p) => {
                let (p0, p1, p2, p3) = ((p[0], p[1]), (p[2], p[3]), (p[4], p[5]), (p[6], p[7]));
                let mt = 1.0 - t;
                (
                    3.0 * mt * mt * (p1.0 - p0.0)
                        + 6.0 * mt * t * (p2.0 - p1.0)
                        + 3.0 * t * t * (p3.0 - p2.0),
                    3.0 * mt * mt * (p1.1 - p0.1)
                        + 6.0 * mt * t * (p2.1 - p1.1)
                        + 3.0 * t * t * (p3.1 - p2.1),
                )
            }
            MeasSeg::Conic { .. } => (0.0, 0.0),
        }
    }

    /// Arc length: lines and conics sum exact chords, cubics integrate via
    /// 8-node Gauss–Legendre quadrature of ∫|B'(t)|dt (see the section doc —
    /// Chrome's own cubic measure runs finer than its published chord
    /// algorithm, so the analytic integral is the tighter match).
    fn length(&self) -> f32 {
        match self {
            MeasSeg::Line([x0, y0, x1, y1]) => ((x1 - x0).hypot(y1 - y0)).max(0.0),
            MeasSeg::Cubic(_) => {
                let mut sum = 0.0f32;
                for (x, w) in GL8_X.iter().zip(GL8_W.iter()) {
                    let t = 0.5 * (x + 1.0);
                    let (dx, dy) = self.derivative(t);
                    sum += w * dx.hypot(dy);
                }
                (sum * 0.5).max(0.0)
            }
            MeasSeg::Conic { spans, .. } => spans.iter().map(|s| s.len).sum(),
        }
    }

    /// Knot table for arc-length inversion: (t, local length) pairs, first
    /// entry (0, 0), last entry t = 1. Lines are exact with two knots;
    /// cubics sample uniformly and rescale so the last knot equals the GL8
    /// exact length; conics use Skia's own span boundaries, whose cumulative
    /// chords ARE the measured length.
    fn knots(&self) -> Vec<(f32, f32)> {
        match self {
            MeasSeg::Line(..) => vec![(0.0, 0.0), (1.0, self.length())],
            MeasSeg::Cubic(_) => {
                let mut ks = Vec::with_capacity(NKNOTS + 1);
                ks.push((0.0, 0.0));
                let mut prev = self.eval(0.0);
                let mut acc = 0.0f32;
                for k in 1..=NKNOTS {
                    let t = k as f32 / NKNOTS as f32;
                    let p = self.eval(t);
                    acc += (p.0 - prev.0).hypot(p.1 - prev.1);
                    ks.push((t, acc));
                    prev = p;
                }
                if acc > 0.0 {
                    let scale = self.length() / acc;
                    for e in ks.iter_mut().skip(1) {
                        e.1 *= scale;
                    }
                }
                ks
            }
            MeasSeg::Conic { spans, .. } => {
                let mut ks = Vec::with_capacity(spans.len() + 1);
                ks.push((0.0, 0.0));
                let mut cum = 0.0f32;
                for s in spans {
                    cum += s.len;
                    ks.push((s.t1, cum));
                }
                ks
            }
        }
    }
}

/// A measured path: segments plus knot tables for arc-length inversion.
pub(crate) struct PathMeasure {
    segs: Vec<MeasSeg>,
    /// Absolute arc length at the START of each segment.
    starts: Vec<f32>,
    /// Per-segment knot tables ([`MeasSeg::knots`]) for distance→t inversion.
    knots: Vec<Vec<(f32, f32)>>,
    total: f32,
}

const NKNOTS: usize = 24;

impl PathMeasure {
    pub(crate) fn total(&self) -> f32 {
        self.total
    }

    /// Point at arc length `target` (Chrome clamps: <0 → start, >total →
    /// end). Within a segment the distance maps to t proportionally across
    /// the knot intervals and the curve is EVALUATED at that t — Skia's
    /// distanceToSegment + compute_pos_tan semantics, so returned points sit
    /// on the curve, not on the measuring chords.
    pub(crate) fn point_at(&self, target: f32) -> (f32, f32) {
        if self.segs.is_empty() {
            return (0.0, 0.0);
        }
        let target = target.clamp(0.0, self.total);
        // Find the segment owning this arc length.
        let mut seg_idx = self.segs.len() - 1;
        for (i, s) in self.starts.iter().enumerate() {
            if *s > target {
                seg_idx = i.saturating_sub(1);
                break;
            }
        }
        let ks = &self.knots[seg_idx];
        let into = target - self.starts[seg_idx];
        // Find the knot interval, then interpolate t and evaluate the curve
        // at it (evaluating beats lerp-ing the sample points).
        let mut k = 0usize;
        while k + 1 < ks.len() && ks[k + 1].1 < into {
            k += 1;
        }
        let (t0, l0) = ks[k];
        let (t1, l1) = ks[(k + 1).min(ks.len() - 1)];
        let frac = if l1 > l0 { ((into - l0) / (l1 - l0)).clamp(0.0, 1.0) } else { 0.0 };
        self.segs[seg_idx].eval((t0 + (t1 - t0) * frac).clamp(0.0, 1.0))
    }
}

/// `d` attribute → measurement segments (identity transform — getTotalLength
/// is defined in user units, before viewBox/element transforms). Mirrors
/// [`parse_path`]'s command handling; Q lifts to the exact equivalent cubic,
/// arcs degrade to endpoint lines, Z closes with a straight line.
fn parse_path_measure(d: &str) -> Vec<MeasSeg> {
    let toks = lex_path(d);
    let take = |i: &mut usize| -> Option<f32> {
        if let Some(Tok::Num(v)) = toks.get(*i) {
            *i += 1;
            Some(*v)
        } else {
            None
        }
    };
    let mut segs: Vec<MeasSeg> = Vec::new();
    let line = |segs: &mut Vec<MeasSeg>, a: (f32, f32), b: (f32, f32)| {
        if a != b {
            segs.push(MeasSeg::Line([a.0, a.1, b.0, b.1]));
        }
    };
    let mut cur = (0.0f32, 0.0f32);
    let mut start = cur;
    let mut cmd = ' ';
    let mut i = 0usize;
    while i < toks.len() {
        if let Tok::Cmd(c) = toks[i] {
            cmd = c;
            i += 1;
            if cmd == 'Z' || cmd == 'z' {
                line(&mut segs, cur, start);
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
                // Moveto draws nothing — it just relocates (subsequent
                // implicit pairs become LineTos via the cmd swap below).
                cur = if rel { (cur.0 + x, cur.1 + y) } else { (x, y) };
                start = cur;
                cmd = if rel { 'l' } else { 'L' };
            }
            'L' => {
                let (Some(x), Some(y)) = (take(&mut i), take(&mut i)) else { break };
                let prev = cur;
                cur = if rel { (cur.0 + x, cur.1 + y) } else { (x, y) };
                line(&mut segs, prev, cur);
            }
            'H' => {
                let Some(x) = take(&mut i) else { break };
                let prev = cur;
                cur.0 = if rel { cur.0 + x } else { x };
                line(&mut segs, prev, cur);
            }
            'V' => {
                let Some(y) = take(&mut i) else { break };
                let prev = cur;
                cur.1 = if rel { cur.1 + y } else { y };
                line(&mut segs, prev, cur);
            }
            'Q' => {
                let (Some(qx), Some(qy), Some(x), Some(y)) =
                    (take(&mut i), take(&mut i), take(&mut i), take(&mut i))
                else {
                    break;
                };
                let q = if rel { (cur.0 + qx, cur.1 + qy) } else { (qx, qy) };
                let end = if rel { (cur.0 + x, cur.1 + y) } else { (x, y) };
                // Exact quadratic→cubic control lift (same as parse_path).
                let c1 = (cur.0 + (2.0 / 3.0) * (q.0 - cur.0), cur.1 + (2.0 / 3.0) * (q.1 - cur.1));
                let c2 = (end.0 + (2.0 / 3.0) * (q.0 - end.0), end.1 + (2.0 / 3.0) * (q.1 - end.1));
                segs.push(MeasSeg::Cubic([cur.0, cur.1, c1.0, c1.1, c2.0, c2.1, end.0, end.1]));
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
                segs.push(MeasSeg::Cubic([cur.0, cur.1, c1.0, c1.1, c2.0, c2.1, end.0, end.1]));
                cur = end;
            }
            'A' => {
                // Seven params, all dropped except the endpoint (the paint
                // model's arc posture).
                for _ in 0..5 {
                    if take(&mut i).is_none() {
                        break;
                    }
                }
                let (Some(x), Some(y)) = (take(&mut i), take(&mut i)) else { break };
                let prev = cur;
                cur = if rel { (cur.0 + x, cur.1 + y) } else { (x, y) };
                line(&mut segs, prev, cur);
            }
            _ => break,
        }
    }
    segs
}

/// Skia's conic measure, verbatim: recurse by halving the INTEGER t-domain
/// ([0, kMaxTValue]), gating on recursion depth, tspan size, and the
/// Chebyshev deviation of the curve's t-midpoint from the chord midpoint;
/// accepted spans accumulate endpoint-to-endpoint CHORD lengths. Evaluating
/// the original conic at the half-t (instead of chopping it) yields the same
/// point sequence Skia's rational de Casteljau chop produces.
#[allow(clippy::too_many_arguments)] // Skia's own signature, verbatim
fn compute_conic(
    pts: &[f32; 6],
    w: f32,
    mint: i32,
    minpt: (f32, f32),
    maxt: i32,
    maxpt: (f32, f32),
    depth: i32,
    spans: &mut Vec<ConicSpan>,
) {
    let halft = (mint + maxt) >> 1;
    let halfpt = conic_eval(pts, w, t_to_f(halft));
    let mid_chord = ((minpt.0 + maxpt.0) * 0.5, (minpt.1 + maxpt.1) * 0.5);
    let too_curvy = (halfpt.0 - mid_chord.0)
        .abs()
        .max((halfpt.1 - mid_chord.1).abs())
        > CHEAP_DIST_LIMIT;
    if depth < MAX_RECURSION_DEPTH && tspan_big_enough(maxt - mint) && too_curvy {
        compute_conic(pts, w, mint, minpt, halft, halfpt, depth + 1, spans);
        compute_conic(pts, w, halft, halfpt, maxt, maxpt, depth + 1, spans);
    } else {
        spans.push(ConicSpan {
            t1: t_to_f(maxt),
            len: (maxpt.0 - minpt.0).hypot(maxpt.1 - minpt.1),
        });
    }
}

/// One measured conic segment: p0 → p1 with control c (a quarter-ellipse
/// arc when c is the corner point and w = √2/2 — the exact rational form).
fn conic_seg(p0: (f32, f32), c: (f32, f32), p1: (f32, f32)) -> MeasSeg {
    let pts = [p0.0, p0.1, c.0, c.1, p1.0, p1.1];
    let mut spans = Vec::new();
    compute_conic(&pts, QUARTER_W, 0, p0, K_MAX_T, p1, 0, &mut spans);
    MeasSeg::Conic { pts, w: QUARTER_W, spans }
}

/// Build a [`PathMeasure`] for a geometry element: `tag` + a raw attribute
/// reader. `None` for non-geometry tags (the caller reports 0 like Chrome's
/// missing-method posture).
pub(crate) fn build_path_measure(
    tag: &str,
    attr: &dyn Fn(&str) -> Option<String>,
) -> Option<PathMeasure> {
    let f = |name: &str, default: f32| -> f32 {
        attr(name)
            .and_then(|v| v.trim().parse::<f32>().ok())
            .unwrap_or(default)
    };
    let segs: Vec<MeasSeg> = match tag {
        "path" => parse_path_measure(attr("d").as_deref().unwrap_or("")),
        "line" => {
            let (x1, y1) = (f("x1", 0.0), f("y1", 0.0));
            let (x2, y2) = (f("x2", 0.0), f("y2", 0.0));
            vec![MeasSeg::Line([x1, y1, x2, y2])]
        }
        "rect" => {
            let (x, y, w, h) = (f("x", 0.0), f("y", 0.0), f("width", 0.0), f("height", 0.0));
            let mut rx = f("rx", 0.0);
            let mut ry = f("ry", 0.0);
            if rx <= 0.0 && ry > 0.0 {
                rx = ry;
            }
            if ry <= 0.0 && rx > 0.0 {
                ry = rx;
            }
            rx = rx.clamp(0.0, w / 2.0);
            ry = ry.clamp(0.0, h / 2.0);
            let mut segs = Vec::new();
            let push_line = |segs: &mut Vec<MeasSeg>, a: (f32, f32), b: (f32, f32)| {
                if a != b {
                    segs.push(MeasSeg::Line([a.0, a.1, b.0, b.1]));
                }
            };
            if rx <= 0.0 || ry <= 0.0 {
                // Square corners: 4 lines (degenerate edges drop like Chrome).
                push_line(&mut segs, (x, y), (x + w, y));
                push_line(&mut segs, (x + w, y), (x + w, y + h));
                push_line(&mut segs, (x + w, y + h), (x, y + h));
                push_line(&mut segs, (x, y + h), (x, y));
            } else {
                // Top edge → TR corner → right edge → BR corner → bottom →
                // BL corner → left → TL corner (closes at the start point).
                // Corner arcs are exact quarter-ellipse conics (Skia's own
                // rRect form) with the control at the cut-corner point.
                segs.push(MeasSeg::Line([x + rx, y, x + w - rx, y]));
                segs.push(conic_seg(
                    (x + w - rx, y),
                    (x + w - rx, y + ry),
                    (x + w, y + ry),
                ));
                segs.push(MeasSeg::Line([x + w, y + ry, x + w, y + h - ry]));
                segs.push(conic_seg(
                    (x + w, y + h - ry),
                    (x + w - rx, y + h - ry),
                    (x + w - rx, y + h),
                ));
                segs.push(MeasSeg::Line([x + w - rx, y + h, x + rx, y + h]));
                segs.push(conic_seg(
                    (x + rx, y + h),
                    (x + rx, y + h - ry),
                    (x, y + h - ry),
                ));
                segs.push(MeasSeg::Line([x, y + h - ry, x, y + ry]));
                segs.push(conic_seg((x, y + ry), (x + rx, y + ry), (x + rx, y)));
            }
            segs
        }
        "circle" => {
            let (cx, cy, r) = (f("cx", 0.0), f("cy", 0.0), f("r", 0.0));
            circle_ish(cx, cy, r, r)
        }
        "ellipse" => {
            let (cx, cy) = (f("cx", 0.0), f("cy", 0.0));
            circle_ish(cx, cy, f("rx", 0.0), f("ry", 0.0))
        }
        "polyline" | "polygon" => {
            let pts = poly_points(attr("points").as_deref().unwrap_or(""));
            let mut segs: Vec<MeasSeg> = Vec::new();
            for w in pts.windows(2) {
                segs.push(MeasSeg::Line([w[0].0, w[0].1, w[1].0, w[1].1]));
            }
            if tag == "polygon" {
                if let (Some(first), Some(last)) = (pts.first().copied(), pts.last().copied()) {
                    if first != last {
                        segs.push(MeasSeg::Line([last.0, last.1, first.0, first.1]));
                    }
                }
            }
            segs
        }
        _ => return None,
    };
    Some(finish_measure(segs))
}

/// circle/ellipse → 4 quarter-ellipse conics starting at (cx+rx, cy) and
/// sweeping clockwise on screen (right → down → left → up — Skia's oval
/// walk), controls at the (±rx, ±ry) corner points.
fn circle_ish(cx: f32, cy: f32, rx: f32, ry: f32) -> Vec<MeasSeg> {
    if rx <= 0.0 || ry <= 0.0 {
        return Vec::new();
    }
    vec![
        conic_seg((cx + rx, cy), (cx + rx, cy + ry), (cx, cy + ry)),
        conic_seg((cx, cy + ry), (cx - rx, cy + ry), (cx - rx, cy)),
        conic_seg((cx - rx, cy), (cx - rx, cy - ry), (cx, cy - ry)),
        conic_seg((cx, cy - ry), (cx + rx, cy - ry), (cx + rx, cy)),
    ]
}

/// Parse `points` (polyline/polygon): whitespace/comma-separated coordinate
/// pairs; malformed tails drop like the paint parser.
fn poly_points(raw: &str) -> Vec<(f32, f32)> {
    let nums: Vec<f32> = raw
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<f32>().ok())
        .collect();
    nums.chunks(2).filter_map(|c| c.first().zip(c.get(1)).map(|(a, b)| (*a, *b))).collect()
}

/// Precompute the knot tables and totals. Per segment the knots come from
/// [`MeasSeg::knots`] — exact for lines and conics (span boundaries), uniform
/// samples rescaled to the GL8 exact length for cubics (the chord
/// underestimate distributes uniformly, so inversion stays monotone and the
/// total is integration-exact). The last knot's t is pinned to 1: a
/// depth-capped conic recursion can accept its final span before the full
/// t-domain, but the curve's endpoint is eval(1) regardless.
fn finish_measure(segs: Vec<MeasSeg>) -> PathMeasure {
    let mut starts = Vec::with_capacity(segs.len());
    let mut knots = Vec::with_capacity(segs.len());
    let mut total = 0.0f32;
    for seg in &segs {
        starts.push(total);
        let mut ks = seg.knots();
        if let Some(last) = ks.last_mut() {
            last.0 = 1.0;
        }
        knots.push(ks);
        total += seg.length();
    }
    PathMeasure { segs, starts, knots, total }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // 批240 / obscura#1100: getTotalLength/getPointAtLength — every constant
    // below is Chrome's own reading of the same shape (headless --dump-dom
    // probe, 2026-09-28). Chrome measures through SkPathMeasure: ovals as
    // exact quarter-ellipse conics with recursive chord subdivision (ported
    // verbatim — these land within ~1e-5), explicit beziers analytically
    // close to Chrome's own (finer-than-published) measure (within 0.02).
    // -----------------------------------------------------------------------

    fn measure(tag: &str, attrs: &[(&str, &str)]) -> PathMeasure {
        build_path_measure(tag, &|a| {
            attrs
                .iter()
                .find(|(k, _)| *k == a)
                .map(|(_, v)| v.to_string())
        })
        .expect("geometry tag")
    }

    #[test]
    fn path_measure_matches_chrome_totals() {
        let close = |got: f32, want: f64, name: &str| {
            assert!(
                (got as f64 - want).abs() < 0.02,
                "{name}: got {got}, Chrome {want}"
            );
        };
        // Straight path and the closing-Z double-back (Z re-walks to start).
        close(measure("path", &[("d", "M0,0 L100,0")]).total(), 100.0, "p1");
        close(measure("path", &[("d", "M0,0 L100,0 Z")]).total(), 200.0, "p4");
        // Cubic and quadratic beziers — Q rides the exact ⅔ lift.
        close(
            measure("path", &[("d", "M0,0 C50,100 100,0 100,100")]).total(),
            160.71572875976562,
            "p2",
        );
        close(
            measure("path", &[("d", "M0,0 Q50,100 100,0")]).total(),
            147.89434814453125,
            "p3",
        );
        // Basic shapes as Chrome's conic chord measure: circle r=25 reads
        // 156.067 (NOT 2πr=157.080 — 16 accepted chords of 22.5°), ellipse
        // 158.159, square rect 300, rounded 282.427 (220 straight + 4 conic
        // quarter corners).
        close(measure("circle", &[("r", "25")]).total(), 156.0674285888672, "circle");
        close(measure("ellipse", &[("rx", "30"), ("ry", "20")]).total(), 158.15943908691406, "ellipse");
        close(measure("rect", &[("width", "100"), ("height", "50")]).total(), 300.0, "rect");
        close(
            measure("rect", &[("width", "100"), ("height", "50"), ("rx", "10")]).total(),
            282.427001953125,
            "rounded rect",
        );
        close(
            measure("line", &[("x1", "0"), ("y1", "0"), ("x2", "30"), ("y2", "40")]).total(),
            50.0,
            "line",
        );
        close(
            measure("polygon", &[("points", "0,0 100,0 50,80")]).total(),
            288.67962646484375,
            "polygon",
        );
        // Empty d and M-only paths draw nothing.
        assert_eq!(measure("path", &[("d", "")]).total(), 0.0);
        assert_eq!(measure("path", &[("d", "M10,10 M20,20")]).total(), 0.0);
    }

    #[test]
    fn path_measure_point_at_matches_chrome_semantics() {
        let p2 = measure("path", &[("d", "M0,0 C50,100 100,0 100,100")]);
        // Chrome: zero=(0,0), mid=(58.0898,49.7112), end=over=(100,100),
        // negative clamps to the start.
        let (zx, zy) = p2.point_at(0.0);
        assert!(zx.abs() < 1e-3 && zy.abs() < 1e-3, "start {zx},{zy}");
        let (mx, my) = p2.point_at(p2.total() / 2.0);
        // Chrome: mid=(58.0898,49.7112). Our inversion is analytic (GL8),
        // Chrome's runs on its own chord table, so t lands ~0.15px apart on
        // this 100px probe — same point on the curve to animation precision.
        assert!(
            (mx - 58.0898).abs() < 0.2 && (my - 49.7112).abs() < 0.1,
            "mid {mx},{my}"
        );
        let (ex, ey) = p2.point_at(p2.total());
        assert!((ex - 100.0).abs() < 1e-2 && (ey - 100.0).abs() < 1e-2, "end {ex},{ey}");
        let (ox, oy) = p2.point_at(p2.total() + 1000.0);
        assert!((ox - 100.0).abs() < 1e-2 && (oy - 100.0).abs() < 1e-2, "over-clamps to end");
        let (nx, ny) = p2.point_at(-5.0);
        assert!(nx.abs() < 1e-3 && ny.abs() < 1e-3, "negative clamps to start");
        // Empty path: (0,0), no panic.
        let empty = measure("path", &[("d", "")]);
        assert_eq!(empty.point_at(50.0), (0.0, 0.0));
    }

    #[test]
    fn path_measure_relative_and_implicit_commands() {
        // Relative walk + glued tokens measure the same ground as absolute.
        let abs = measure("path", &[("d", "M0,0L100,0L100,50")]).total();
        let rel = measure("path", &[("d", "m0,0l100,0l0,50")]).total();
        assert!((abs - 150.0).abs() < 1e-3 && (rel - abs).abs() < 1e-3, "abs {abs} rel {rel}");
        // Implicit LineTo pairs after M, H/V mixed in.
        let imp = measure("path", &[("d", "M0,0 100,0 100,50")]).total();
        assert!((imp - 150.0).abs() < 1e-3, "implicit {imp}");
        let hv = measure("path", &[("d", "M10,10 H50 V30 h10")]).total();
        // 40 (H50) + 20 (V30) + 10 (h10).
        assert!((hv - 70.0).abs() < 1e-3, "hv {hv}");
        // Arc params consume without derailing (degrades to endpoint line:
        // 0,0 → 60,0 = 60, then 60,0 → 60,30 = 30).
        let arc = measure("path", &[("d", "M 0 0 A 30 30 0 0 1 60 0 L 60 30")]).total();
        assert!((arc - 90.0).abs() < 1e-3, "arc {arc}");
    }
}
