// #222 mix-blend-mode — the compositing half. The W3C
// compositing-and-blending formulas plus the offscreen group plumbing
// execute_band drives: `redirect` swaps painting into a transparent twin
// canvas (clip + transform stacks carried), `restore` composites the
// group back onto the saved backdrop. Extracted from paint.rs as one
// cohesive domain unit — the ratchet counts paint.rs production lines.
use super::Canvas;
use crate::diting_css::MixBlendMode;

// --- mix-blend-mode (#222) — the W3C compositing-and-blending formulas.
// Separable modes run in exact u32 arithmetic; color-dodge and color-burn
// in particular MUST divide in integers with round-to-nearest, never in
// float: a float reciprocal lands off-by-one through gradient transition
// bands (takumi #1791 measured ~1.7% of ramp pixels off). Products stay
// ≤ 255·255·2, far inside u32. ---

/// One separable-mode blend step: `cb` backdrop channel, `cs` source
/// channel, both straight 0..255. Non-separable modes don't route here —
/// they need the whole RGB triple ([`blend_rgb`]).
pub(super) fn blend_channel(mode: MixBlendMode, cb: u32, cs: u32) -> u32 {
    match mode {
        MixBlendMode::Normal => cs,
        MixBlendMode::Multiply => (cb * cs + 127) / 255,
        MixBlendMode::Screen => 255 - ((255 - cb) * (255 - cs) + 127) / 255,
        MixBlendMode::Darken => cb.min(cs),
        MixBlendMode::Lighten => cb.max(cs),
        // Spec: Cb==0 → 0; Cs==255 → 1; else min(1, Cb/(1−Cs)). The
        // round-to-nearest divide is (num·2 + den) / (den·2) in u32.
        MixBlendMode::ColorDodge => {
            if cb == 0 {
                0
            } else if cs == 255 {
                255
            } else {
                let den = 255 - cs;
                255.min((cb * 510 + den) / (2 * den))
            }
        }
        // Spec: Cb==255 → 255; Cs==0 → 0; else 1 − min(1, (1−Cb)/Cs).
        MixBlendMode::ColorBurn => {
            if cb == 255 {
                255
            } else if cs == 0 {
                0
            } else {
                let inv = 255.min(((255 - cb) * 510 + cs) / (2 * cs));
                255 - inv
            }
        }
        // Overlay: multiply below Cb 0.5, screen above — the threshold
        // `2·cb <= 255` is `cb <= 127` (127.5 is unrepresentable).
        MixBlendMode::Overlay => {
            if 2 * cb <= 255 {
                (2 * cb * cs + 127) / 255
            } else {
                255 - ((2 * (255 - cb) * (255 - cs) + 127) / 255)
            }
        }
        // Hard-light: overlay with the operands swapped (Cs threshold).
        MixBlendMode::HardLight => {
            if 2 * cs <= 255 {
                (2 * cb * cs + 127) / 255
            } else {
                255 - ((2 * (255 - cb) * (255 - cs) + 127) / 255)
            }
        }
        MixBlendMode::Difference => cb.abs_diff(cs),
        MixBlendMode::Exclusion => {
            let p = (2 * cb * cs + 127) / 255;
            (cb + cs).saturating_sub(p)
        }
        // Soft-light needs the sqrt piecewise D(x) — f32 (IEEE-deterministic
        // ops, no division drift concern: takumi #1791's error was the
        // approximate reciprocal, not f32 division).
        MixBlendMode::SoftLight => {
            let cbf = f32::from(cb as u8) / 255.0;
            let csf = f32::from(cs as u8) / 255.0;
            let b = if csf <= 0.5 {
                cbf - (1.0 - 2.0 * csf) * cbf * (1.0 - cbf)
            } else {
                let d = if cbf <= 0.25 {
                    ((16.0 * cbf - 12.0) * cbf + 4.0) * cbf
                } else {
                    cbf.sqrt()
                };
                cbf + (2.0 * csf - 1.0) * (d - cbf)
            };
            (b.clamp(0.0, 1.0) * 255.0).round() as u32
        }
        // Non-separable modes never route here.
        MixBlendMode::Hue
        | MixBlendMode::Saturation
        | MixBlendMode::Color
        | MixBlendMode::Luminosity => cs,
    }
}

/// Luminance (compositing spec §5.1) on a straight 0..1 triple.
fn blend_lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Clip a color back into 0..1 keeping its luminance (spec ClipColor).
fn blend_clip_color(mut c: [f32; 3]) -> [f32; 3] {
    let l = blend_lum(c);
    let n = c.iter().copied().fold(f32::MAX, f32::min);
    let x = c.iter().copied().fold(f32::MIN, f32::max);
    if n < 0.0 {
        for v in c.iter_mut() {
            *v = l + (*v - l) * l / (l - n);
        }
    }
    if x > 1.0 {
        for v in c.iter_mut() {
            *v = l + (*v - l) * (1.0 - l) / (x - l);
        }
    }
    c
}

/// Shift a color's luminance to `l` (spec SetLum).
fn blend_set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - blend_lum(c);
    blend_clip_color([c[0] + d, c[1] + d, c[2] + d])
}

/// Force a color's saturation to `s` (spec SetSat) — saturation rides the
/// min/mid/max channel ordering.
fn blend_set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    let mut idx = [0usize, 1, 2];
    idx.sort_by(|&a, &b| c[a].total_cmp(&c[b]));
    let (min, mid, max) = (idx[0], idx[1], idx[2]);
    let mut out = [0.0f32; 3];
    if c[max] > c[min] {
        out[mid] = (c[mid] - c[min]) * s / (c[max] - c[min]);
        out[max] = s;
    }
    out
}

/// The full per-pixel blend step: backdrop and source straight RGB triples
/// in 0..255, blended per the mode. Separable modes go channel-wise; the
/// four non-separable modes work on the luminosity/saturation triples in
/// f32 (0..1) and return rounded u32s.
fn blend_rgb(mode: MixBlendMode, cb: [u32; 3], cs: [u32; 3]) -> [u32; 3] {
    match mode {
        MixBlendMode::Hue
        | MixBlendMode::Saturation
        | MixBlendMode::Color
        | MixBlendMode::Luminosity => {
            let b = cb.map(|v| f32::from(v as u8) / 255.0);
            let s = cs.map(|v| f32::from(v as u8) / 255.0);
            let sat = |c: [f32; 3]| c.iter().copied().fold(f32::MIN, f32::max) - c.iter().copied().fold(f32::MAX, f32::min);
            let out = match mode {
                MixBlendMode::Hue => blend_set_lum(blend_set_sat(s, sat(b)), blend_lum(b)),
                MixBlendMode::Saturation => blend_set_lum(blend_set_sat(b, sat(s)), blend_lum(b)),
                MixBlendMode::Color => blend_set_lum(s, blend_lum(b)),
                _ => blend_set_lum(b, blend_lum(s)),
            };
            out.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u32)
        }
        _ => [
            blend_channel(mode, cb[0], cs[0]),
            blend_channel(mode, cb[1], cs[1]),
            blend_channel(mode, cb[2], cs[2]),
        ],
    }
}

/// Composite one blend group onto the backdrop (#222): per pixel the
/// compositing spec's blending formula
///   Co·αo = αs·(1−αb)·Cs + αs·αb·B(Cb,Cs) + (1−αs)·αb·Cb,
///   αo = αs + αb·(1−αs)
/// evaluated at 255³ scale in u32 so the single divide is exact
/// (round-to-nearest). STORAGE IS PREMULTIPLIED on these canvases — `over`
/// writes src·α into a transparent destination — so both sides
/// un-premultiply to straight before the blend step, and the result goes
/// back premultiplied. An opaque destination (every page canvas in
/// practice) makes the round trip the identity. Group pixels with αs = 0
/// leave the backdrop untouched, so the group's own clip state (inherited
/// at BlendBegin) already bounds the write.
fn blend_group(dst: &mut Canvas, group: &Canvas, mode: MixBlendMode) {
    let n = dst.data.len().min(group.data.len()) / 4;
    for p in 0..n {
        let i = p * 4;
        let sa = group.data[i + 3] as u32;
        if sa == 0 {
            continue;
        }
        let ba = dst.data[i + 3] as u32;
        // Un-premultiply: round(c·255/α), identity at α=255.
        let straight = |c: u32, a: u32| -> u32 {
            if a >= 255 {
                c
            } else {
                (c * 510 + a) / (2 * a)
            }
        };
        let cb = [
            straight(dst.data[i] as u32, ba),
            straight(dst.data[i + 1] as u32, ba),
            straight(dst.data[i + 2] as u32, ba),
        ];
        let cs = [
            straight(group.data[i] as u32, sa),
            straight(group.data[i + 1] as u32, sa),
            straight(group.data[i + 2] as u32, sa),
        ];
        let b = blend_rgb(mode, cb, cs);
        let ao_num = sa * 255 + ba * (255 - sa);
        for ch in 0..3 {
            let prem = sa * (255 - ba) * cs[ch]
                + sa * ba * b[ch]
                + (255 - sa) * ba * cb[ch];
            // Premultiplied storage: prem sits at 255² scale.
            dst.data[i + ch] = ((prem + 32512) / 65025) as u8;
        }
        dst.data[i + 3] = ((ao_num + 127) / 255) as u8;
    }
}


/// Open a blend group (#222): redirect painting into an offscreen
/// transparent canvas — same size (the coordinate space is shared), with
/// the CURRENT clip and transform stacks copied so the group's content
/// lands exactly where it would have painted un-blended.
pub(super) fn redirect(out: &mut Canvas, stack: &mut Vec<(Canvas, MixBlendMode)>, mode: MixBlendMode) {
    let mut group = Canvas::new_transparent(out.width, out.height);
    group.clip = out.clip.clone();
    group.xf_stack = out.xf_stack.clone();
    let saved = std::mem::replace(out, group);
    stack.push((saved, mode));
}

/// Close the innermost blend group: composite it onto the saved backdrop
/// canvas and put that canvas back as the paint target.
pub(super) fn restore(out: &mut Canvas, stack: &mut Vec<(Canvas, MixBlendMode)>) {
    if let Some((saved, mode)) = stack.pop() {
        let group = std::mem::replace(out, saved);
        blend_group(out, &group, mode);
    }
}
