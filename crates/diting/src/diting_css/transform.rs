//! Transform / transform-origin parsing (god-file ratchet split from mod.rs, pure move).

use super::*;

/// Parse `transform-origin` (#244): `[ <length-percentage> | left | center |
/// right ] || [ <length-percentage> | top | center | bottom ]`, keywords
/// normalizing to percentages (left/top → 0%, center → 50%, right/bottom →
/// 100%). A bare x component defaults y to center. More than two components
/// (the 3D z form) invalidates the declaration.
pub(super) fn parse_transform_origin(v: &str, fonts: &FontCtx) -> Option<(Length, Length)> {
    let len = |val: &str| -> Option<Length> {
        let t = val.trim();
        if let Some(l) = eval_math_call(t, fonts) {
            return Some(l);
        }
        parse_css_length(t).map(|l| resolve_len(l, fonts))
    };
    let one = |tok: &str| -> Option<Length> {
        match tok.trim().to_ascii_lowercase().as_str() {
            "left" | "top" => Some(Length::Percent(0.0)),
            "center" => Some(Length::Percent(50.0)),
            "right" | "bottom" => Some(Length::Percent(100.0)),
            _ => len(tok),
        }
    };
    let toks: Vec<&str> = v.split_whitespace().collect();
    if toks.is_empty() || toks.len() > 2 {
        return None;
    }
    let is_y_kw = |t: &str| matches!(t, "top" | "bottom");
    match toks.as_slice() {
        [a] => {
            // A lone y-keyword ("top") means x=center; anything else is x
            // with y defaulting to center.
            if is_y_kw(&a.to_ascii_lowercase()) {
                Some((Length::Percent(50.0), one(a)?))
            } else {
                Some((one(a)?, Length::Percent(50.0)))
            }
        }
        // Two components: a y-keyword fixes its own axis wherever it sits
        // ("top 10px" is x=10px y=0%); otherwise positional x-then-y.
        [a, b] if is_y_kw(&a.to_ascii_lowercase()) => Some((one(b)?, one(a)?)),
        [a, b] => Some((one(a)?, one(b)?)),
        _ => None,
    }
}

/// Parse a `transform` declaration into one 2D affine [`Transform2D`]
/// (transform batch; obscura #740 lineage, widened twice). The translate
/// family (translate/translate3d — z parses and drops, 3D flattens to 2D
/// here — /translateX/translateY) and the scale family accumulate exactly;
/// rotate/skewX/skewY/matrix join the same composition. The GSAP tween
/// output this exists for writes deg/rad angles and full `matrix(…)` forms.
/// `none` and anything unparsable yield None — spec: one unknown function
/// invalidates the whole declaration, so the element renders untransformed.
/// component keeps symbolic lengths; the transition register op reuses this
/// so a before/after snapshot matches the sampler's affine.
/// Standalone callers (svg presentation attributes, @supports probes, the
/// transition register) parse without a viewport — they fold against the
/// default pair. The cascade threads the page's real viewport through
/// `parse_transform_with_vp` so `translateX(100vw)` — the deck/carousel
/// idiom — resolves to actual pixels.
pub(crate) fn parse_transform(v: &str) -> Option<Transform2D> {
    parse_transform_with_vp(v, DEFAULT_VIEWPORT.0, DEFAULT_VIEWPORT.1)
}

pub(crate) fn parse_transform_with_vp(v: &str, vw: f32, vh: f32) -> Option<Transform2D> {
    let v = v.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return None; // `none` is the initial value → no transform
    }
    let one = |s: &str| -> Option<Length> {
        let s = s.trim();
        if let Some(num) = s.strip_suffix('%') {
            num.trim().css_f32().map(Length::Percent)
        } else if let Some(num) = s.strip_suffix("px") {
            num.trim().css_f32().map(Length::Px)
        } else if let Some(num) = s.strip_suffix("vw") {
            // Viewport units fold to px at parse time (batch 162 resolve_len
            // posture) — the translate slots are px/percent only.
            num.trim().css_f32().map(|n| Length::Px(n * vw / 100.0))
        } else if let Some(num) = s.strip_suffix("vh") {
            num.trim().css_f32().map(|n| Length::Px(n * vh / 100.0))
        } else {
            // `0` is a legal bare length; other unitless values are not.
            (s == "0").then_some(Length::Px(0.0))
        }
    };
    // Percentage/calc translate slots can't ride a rotated linear part (the
    // vector map mixes axes) — only a fully resolved px composes there.
    let px_only = |l: Length| match l {
        Length::Px(v) => Some(v),
        _ => None,
    };
    let num = |s: &str| -> Option<f32> {
        let n = s.trim().css_f32()?;
        n.is_finite().then_some(n)
    };
    // Angle in CSS units: deg (the default suffix real sheets write), rad,
    // turn, grad. Bare nonzero numbers are invalid CSS (Chrome rejects).
    let angle = parse_angle_deg;
    let mut t = Transform2D {
        a: 1.0, b: 0.0, c: 0.0, d: 1.0,
        tx: Length::Px(0.0),
        ty: Length::Px(0.0),
    };
    // Walking left to right, each new function sits INNERMOST (CSS composes
    // the list first-to-last as outermost-to-innermost): M_new = M_old ∘ F.
    // Linear parts multiply; a translate maps through the linear part
    // accumulated so far (exactly the translate-scaled-by-prior-scale rule
    // the axis-aligned slice pinned).
    let compose_linear = |t: &mut Transform2D, fa: f32, fb: f32, fc: f32, fd: f32| {
        let (a, b, c, d) = (t.a, t.b, t.c, t.d);
        t.a = a * fa + c * fb;
        t.b = b * fa + d * fb;
        t.c = a * fc + c * fd;
        t.d = b * fc + d * fd;
    };
    let compose_translate = |t: &mut Transform2D, x: Length, y: Length| -> Option<()> {
        if t.is_axis_aligned() {
            // Diagonal keeps the slots independent (the historical
            // mul_len/add_len semantics, percent included).
            t.tx = add_len(mul_len(x, t.a), t.tx)?;
            t.ty = add_len(mul_len(y, t.d), t.ty)?;
        } else {
            // Full vector map: both slots must be px.
            let (px, py) = (px_only(x)?, px_only(y)?);
            t.tx = add_len(Length::Px(t.a * px + t.c * py), t.tx)?;
            t.ty = add_len(Length::Px(t.b * px + t.d * py), t.ty)?;
        }
        Some(())
    };
    let mut rest = v;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let (name, after) = rest.split_once('(')?;
        let (args, tail) = after.split_once(')')?;
        let name = name.trim();
        let parts: Vec<&str> = args.split(',').collect();
        match name {
            "translate" | "translate3d" => {
                let (a, b) = match parts.as_slice() {
                    [x] => (one(x)?, Length::Px(0.0)), // translate(x) == translate(x, 0)
                    [x, y] => (one(x)?, one(y)?),
                    [x, y, z] => {
                        // 3-arg form is translate3d; z must parse as a
                        // length (GSAP writes "0px") and is then dropped —
                        // this engine has no z axis.
                        one(z)?;
                        (one(x)?, one(y)?)
                    }
                    _ => return None,
                };
                compose_translate(&mut t, a, b)?;
            }
            "translateX" => {
                let [x] = parts.as_slice() else { return None };
                let a = one(x)?;
                if t.is_axis_aligned() {
                    compose_translate(&mut t, a, Length::Px(0.0))?;
                } else {
                    let v = px_only(a)?;
                    t.tx = add_len(Length::Px(t.a * v), t.tx)?;
                    t.ty = add_len(Length::Px(t.b * v), t.ty)?;
                }
            }
            "translateY" => {
                let [y] = parts.as_slice() else { return None };
                let b = one(y)?;
                if t.is_axis_aligned() {
                    compose_translate(&mut t, Length::Px(0.0), b)?;
                } else {
                    let v = px_only(b)?;
                    t.tx = add_len(Length::Px(t.c * v), t.tx)?;
                    t.ty = add_len(Length::Px(t.d * v), t.ty)?;
                }
            }
            "scale" => {
                let (fsx, fsy) = match parts.as_slice() {
                    [x] => {
                        let s = num(x)?;
                        (s, s) // scale(s) == scale(s, s)
                    }
                    [x, y] => (num(x)?, num(y)?),
                    _ => return None,
                };
                compose_linear(&mut t, fsx, 0.0, 0.0, fsy);
            }
            "scaleX" | "scaleY" => {
                let [x] = parts.as_slice() else { return None };
                let s = num(x)?;
                if name == "scaleX" {
                    compose_linear(&mut t, s, 0.0, 0.0, 1.0);
                } else {
                    compose_linear(&mut t, 1.0, 0.0, 0.0, s);
                }
            }
            "rotate" => {
                let [arg] = parts.as_slice() else { return None };
                let rad = angle(arg)?.to_radians();
                let (s, c) = (rad.sin(), rad.cos());
                compose_linear(&mut t, c, s, -s, c);
            }
            "skewX" => {
                let [arg] = parts.as_slice() else { return None };
                compose_linear(&mut t, 1.0, 0.0, angle(arg)?.to_radians().tan(), 1.0);
            }
            "skewY" => {
                let [arg] = parts.as_slice() else { return None };
                compose_linear(&mut t, 1.0, angle(arg)?.to_radians().tan(), 0.0, 1.0);
            }
            "matrix" => {
                let [fa, fb, fc, fd, fe, ff] = parts.as_slice() else { return None };
                let (fa, fb, fc, fd) = (num(fa)?, num(fb)?, num(fc)?, num(fd)?);
                let (fe, ff) = (num(fe)?, num(ff)?);
                // e/f are plain numbers (px in matrix units) that map through
                // the OLD accumulated linear — F rides innermost, so
                // (M∘F).t = L_M·t_F + t_M — which means the translate must
                // compose BEFORE the linear update, while t still holds L_M.
                compose_translate(&mut t, Length::Px(fe), Length::Px(ff));
                compose_linear(&mut t, fa, fb, fc, fd);
            }
            _ => return None,
        }
        rest = tail;
    }
    Some(t)
}
