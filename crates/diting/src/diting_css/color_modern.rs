//! CSS Color 4/5 beyond the legacy forms `color.rs` grew up with: the full
//! named-colour table, space-separated `rgb()/hsl()` with `/ alpha`, `hwb()`,
//! `lab()/lch()/oklab()/oklch()`, `color(<space> …)`, `color-mix()`,
//! `light-dark()` and the relative-colour `from` syntax. Everything resolves to
//! an sRGB [`Color`] (out-of-gamut channels clip) — enough to paint and to
//! compare; computed-value serialisation keeps rgb().

use super::Color;

type V3 = [f32; 3];

#[derive(Clone, Copy, PartialEq)]
enum Space {
    Srgb,
    SrgbLinear,
    Hsl,
    Hwb,
    Lab,
    Lch,
    Oklab,
    Oklch,
    DisplayP3,
    A98,
    Prophoto,
    Rec2020,
    XyzD50,
    XyzD65,
}

/// A colour in some space; `c` are that space's channels (hue in degrees,
/// NaN = `none`), `a` the alpha in 0..=1.
#[derive(Clone, Copy)]
struct Col {
    space: Space,
    c: V3,
    a: f32,
}

const NAMED: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff), ("antiquewhite", 0xfaebd7), ("aqua", 0x00ffff), ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff), ("beige", 0xf5f5dc), ("bisque", 0xffe4c4), ("black", 0x000000),
    ("blanchedalmond", 0xffebcd), ("blue", 0x0000ff), ("blueviolet", 0x8a2be2), ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887), ("cadetblue", 0x5f9ea0), ("chartreuse", 0x7fff00), ("chocolate", 0xd2691e),
    ("coral", 0xff7f50), ("cornflowerblue", 0x6495ed), ("cornsilk", 0xfff8dc), ("crimson", 0xdc143c),
    ("cyan", 0x00ffff), ("darkblue", 0x00008b), ("darkcyan", 0x008b8b), ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9), ("darkgreen", 0x006400), ("darkgrey", 0xa9a9a9), ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b), ("darkolivegreen", 0x556b2f), ("darkorange", 0xff8c00), ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000), ("darksalmon", 0xe9967a), ("darkseagreen", 0x8fbc8f), ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f), ("darkslategrey", 0x2f4f4f), ("darkturquoise", 0x00ced1), ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493), ("deepskyblue", 0x00bfff), ("dimgray", 0x696969), ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff), ("firebrick", 0xb22222), ("floralwhite", 0xfffaf0), ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff), ("gainsboro", 0xdcdcdc), ("ghostwhite", 0xf8f8ff), ("gold", 0xffd700),
    ("goldenrod", 0xdaa520), ("gray", 0x808080), ("green", 0x008000), ("greenyellow", 0xadff2f),
    ("grey", 0x808080), ("honeydew", 0xf0fff0), ("hotpink", 0xff69b4), ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082), ("ivory", 0xfffff0), ("khaki", 0xf0e68c), ("lavender", 0xe6e6fa),
    ("lavenderblush", 0xfff0f5), ("lawngreen", 0x7cfc00), ("lemonchiffon", 0xfffacd), ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080), ("lightcyan", 0xe0ffff), ("lightgoldenrodyellow", 0xfafad2), ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90), ("lightgrey", 0xd3d3d3), ("lightpink", 0xffb6c1), ("lightsalmon", 0xffa07a),
    ("lightseagreen", 0x20b2aa), ("lightskyblue", 0x87cefa), ("lightslategray", 0x778899), ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de), ("lightyellow", 0xffffe0), ("lime", 0x00ff00), ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6), ("magenta", 0xff00ff), ("maroon", 0x800000), ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd), ("mediumorchid", 0xba55d3), ("mediumpurple", 0x9370db), ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee), ("mediumspringgreen", 0x00fa9a), ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585), ("midnightblue", 0x191970), ("mintcream", 0xf5fffa), ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5), ("navajowhite", 0xffdead), ("navy", 0x000080), ("oldlace", 0xfdf5e6),
    ("olive", 0x808000), ("olivedrab", 0x6b8e23), ("orange", 0xffa500), ("orangered", 0xff4500),
    ("orchid", 0xda70d6), ("palegoldenrod", 0xeee8aa), ("palegreen", 0x98fb98), ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093), ("papayawhip", 0xffefd5), ("peachpuff", 0xffdab9), ("peru", 0xcd853f),
    ("pink", 0xffc0cb), ("plum", 0xdda0dd), ("powderblue", 0xb0e0e6), ("purple", 0x800080),
    ("rebeccapurple", 0x663399), ("red", 0xff0000), ("rosybrown", 0xbc8f8f), ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513), ("salmon", 0xfa8072), ("sandybrown", 0xf4a460), ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee), ("sienna", 0xa0522d), ("silver", 0xc0c0c0), ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd), ("slategray", 0x708090), ("slategrey", 0x708090), ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f), ("steelblue", 0x4682b4), ("tan", 0xd2b48c), ("teal", 0x008080),
    ("thistle", 0xd8bfd8), ("tomato", 0xff6347), ("turquoise", 0x40e0d0), ("violet", 0xee82ee),
    ("wheat", 0xf5deb3), ("white", 0xffffff), ("whitesmoke", 0xf5f5f5), ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];

// ---------------------------------------------------------------------------
// Colour-space maths
// ---------------------------------------------------------------------------

fn mul(m: &[[f32; 3]; 3], v: V3) -> V3 {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

const SRGB_TO_XYZ: [[f32; 3]; 3] = [
    [0.412_390_8, 0.357_584, 0.180_481],
    [0.212_639, 0.715_168_7, 0.072_192_32],
    [0.019_330_82, 0.119_194_78, 0.950_532_15],
];
const XYZ_TO_SRGB: [[f32; 3]; 3] = [
    [3.240_97, -1.537_383_2, -0.498_610_76],
    [-0.969_243_65, 1.875_967_5, 0.041_555_06],
    [0.055_630_08, -0.203_976_96, 1.056_971_5],
];
const D65_TO_D50: [[f32; 3]; 3] = [
    [1.047_929_8, 0.022_946_794, -0.050_192_23],
    [0.029_627_816, 0.990_434_5, -0.017_073_825],
    [-0.009_243_058, 0.015_055_145, 0.751_874_3],
];
const D50_TO_D65: [[f32; 3]; 3] = [
    [0.955_473_5, -0.023_098_537, 0.063_259_31],
    [-0.028_369_706, 1.009_995_5, 0.021_041_399],
    [0.012_314_002, -0.020_507_696, 1.330_365_9],
];
const P3_TO_XYZ: [[f32; 3]; 3] = [
    [0.486_570_95, 0.265_667_7, 0.198_217_29],
    [0.228_974_56, 0.691_738_5, 0.079_286_91],
    [0.0, 0.045_113_38, 1.043_944_4],
];
const A98_TO_XYZ: [[f32; 3]; 3] = [
    [0.576_669_04, 0.185_558_24, 0.188_228_65],
    [0.297_344_98, 0.627_363_57, 0.075_291_46],
    [0.027_031_36, 0.070_688_85, 0.991_337_54],
];
const REC2020_TO_XYZ: [[f32; 3]; 3] = [
    [0.636_958_05, 0.144_616_9, 0.168_880_97],
    [0.262_700_2, 0.677_998_07, 0.059_301_72],
    [0.0, 0.028_072_693, 1.060_985_1],
];
const PROPHOTO_TO_XYZ_D50: [[f32; 3]; 3] = [
    [0.797_749_3, 0.135_171_93, 0.031_347_66],
    [0.288_040_5, 0.711_874_4, 0.000_085_1],
    [0.0, 0.0, 0.825_210_5],
];

fn srgb_decode(x: f32) -> f32 {
    let a = x.abs();
    let l = if a <= 0.04045 { a / 12.92 } else { ((a + 0.055) / 1.055).powf(2.4) };
    l.copysign(x)
}
fn srgb_encode(x: f32) -> f32 {
    let a = x.abs();
    let g = if a <= 0.003_130_8 { a * 12.92 } else { 1.055 * a.powf(1.0 / 2.4) - 0.055 };
    g.copysign(x)
}
fn gamma_decode(x: f32, g: f32) -> f32 {
    x.abs().powf(g).copysign(x)
}

fn none0(x: f32) -> f32 {
    if x.is_nan() { 0.0 } else { x }
}

/// Channels of `col` -> XYZ D65.
fn to_xyz_d65(col: &Col) -> V3 {
    let c = [none0(col.c[0]), none0(col.c[1]), none0(col.c[2])];
    match col.space {
        Space::Srgb => mul(&SRGB_TO_XYZ, [srgb_decode(c[0]), srgb_decode(c[1]), srgb_decode(c[2])]),
        Space::SrgbLinear => mul(&SRGB_TO_XYZ, c),
        Space::Hsl => to_xyz_d65(&Col { space: Space::Srgb, c: hsl_to_rgb(c), a: col.a }),
        Space::Hwb => to_xyz_d65(&Col { space: Space::Srgb, c: hwb_to_rgb(c), a: col.a }),
        Space::Lab | Space::Lch => {
            let lab = if col.space == Space::Lch { polar_to_rect(c) } else { c };
            let fy = (lab[0] + 16.0) / 116.0;
            let fx = lab[1] / 500.0 + fy;
            let fz = fy - lab[2] / 200.0;
            const E: f32 = 216.0 / 24389.0;
            const K: f32 = 24389.0 / 27.0;
            let f3 = |f: f32| if f * f * f > E { f * f * f } else { (116.0 * f - 16.0) / K };
            let y = if lab[0] > K * E { fy * fy * fy } else { lab[0] / K };
            let d50 = [0.96422 * f3(fx), y, 0.82521 * f3(fz)];
            mul(&D50_TO_D65, d50)
        }
        Space::Oklab | Space::Oklch => {
            let lab = if col.space == Space::Oklch { polar_to_rect(c) } else { c };
            let l_ = lab[0] + 0.396_337_78 * lab[1] + 0.215_803_76 * lab[2];
            let m_ = lab[0] - 0.105_561_346 * lab[1] - 0.063_854_17 * lab[2];
            let s_ = lab[0] - 0.089_484_18 * lab[1] - 1.291_485_5 * lab[2];
            let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
            let lin = [
                4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_93 * s,
                -1.268_438 * l + 2.609_757_4 * m - 0.341_319_4 * s,
                -0.004_196_086 * l - 0.703_418_6 * m + 1.707_614_7 * s,
            ];
            mul(&SRGB_TO_XYZ, lin)
        }
        Space::DisplayP3 => mul(&P3_TO_XYZ, [srgb_decode(c[0]), srgb_decode(c[1]), srgb_decode(c[2])]),
        Space::A98 => mul(&A98_TO_XYZ, [gamma_decode(c[0], 563.0 / 256.0), gamma_decode(c[1], 563.0 / 256.0), gamma_decode(c[2], 563.0 / 256.0)]),
        Space::Rec2020 => {
            const A: f32 = 1.099_296_8;
            const B: f32 = 0.018_053_97;
            let d = |x: f32| if x.abs() < B * 4.5 { x / 4.5 } else { ((x.abs() + A - 1.0) / A).powf(1.0 / 0.45).copysign(x) };
            mul(&REC2020_TO_XYZ, [d(c[0]), d(c[1]), d(c[2])])
        }
        Space::Prophoto => {
            let d = |x: f32| if x.abs() <= 16.0 / 512.0 { x / 16.0 } else { gamma_decode(x, 1.8) };
            mul(&D50_TO_D65, mul(&PROPHOTO_TO_XYZ_D50, [d(c[0]), d(c[1]), d(c[2])]))
        }
        Space::XyzD50 => mul(&D50_TO_D65, c),
        Space::XyzD65 => c,
    }
}

fn polar_to_rect(c: V3) -> V3 {
    let h = none0(c[2]).to_radians();
    [c[0], c[1] * h.cos(), c[1] * h.sin()]
}
fn rect_to_polar(c: V3) -> V3 {
    let chroma = (c[1] * c[1] + c[2] * c[2]).sqrt();
    let hue = if chroma < 1e-6 { f32::NAN } else { c[2].atan2(c[1]).to_degrees().rem_euclid(360.0) };
    [c[0], chroma, hue]
}

fn hsl_to_rgb(c: V3) -> V3 {
    let (h, s, l) = (c[0].rem_euclid(360.0), (c[1] / 100.0).clamp(0.0, 1.0), (c[2] / 100.0).clamp(0.0, 1.0));
    let f = |n: f32| {
        let k = (n + h / 30.0).rem_euclid(12.0);
        l - s * l.min(1.0 - l) * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0)
    };
    [f(0.0), f(8.0), f(4.0)]
}
fn rgb_to_hsl(c: V3) -> V3 {
    let (r, g, b) = (c[0], c[1], c[2]);
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let l = (mx + mn) / 2.0;
    let d = mx - mn;
    if d.abs() < 1e-6 {
        return [f32::NAN, 0.0, l * 100.0];
    }
    let s = if l == 0.0 || l == 1.0 { 0.0 } else { (mx - l) / l.min(1.0 - l) };
    let h = if mx == r { (g - b) / d + if g < b { 6.0 } else { 0.0 } } else if mx == g { (b - r) / d + 2.0 } else { (r - g) / d + 4.0 };
    [h * 60.0, s * 100.0, l * 100.0]
}
fn hwb_to_rgb(c: V3) -> V3 {
    let (mut w, mut b) = (c[1] / 100.0, c[2] / 100.0);
    if w + b >= 1.0 {
        let g = w / (w + b);
        return [g, g, g];
    }
    let rgb = hsl_to_rgb([c[0], 100.0, 50.0]);
    w = w.max(0.0);
    b = b.max(0.0);
    [rgb[0] * (1.0 - w - b) + w, rgb[1] * (1.0 - w - b) + w, rgb[2] * (1.0 - w - b) + w]
}
fn rgb_to_hwb(c: V3) -> V3 {
    let hsl = rgb_to_hsl(c);
    let (mx, mn) = (c[0].max(c[1]).max(c[2]), c[0].min(c[1]).min(c[2]));
    [hsl[0], mn * 100.0, (1.0 - mx) * 100.0]
}

/// XYZ D65 -> gamma-encoded sRGB (unclamped).
fn xyz_to_srgb(xyz: V3) -> V3 {
    let lin = mul(&XYZ_TO_SRGB, xyz);
    [srgb_encode(lin[0]), srgb_encode(lin[1]), srgb_encode(lin[2])]
}

fn to_srgb(col: &Col) -> V3 {
    match col.space {
        Space::Srgb => [none0(col.c[0]), none0(col.c[1]), none0(col.c[2])],
        Space::Hsl => hsl_to_rgb([none0(col.c[0]), none0(col.c[1]), none0(col.c[2])]),
        Space::Hwb => hwb_to_rgb([none0(col.c[0]), none0(col.c[1]), none0(col.c[2])]),
        _ => xyz_to_srgb(to_xyz_d65(col)),
    }
}

/// sRGB (gamma-encoded) -> the channels of `space` (for mixing / `from`).
fn from_srgb(space: Space, rgb: V3) -> V3 {
    match space {
        Space::Srgb => rgb,
        Space::Hsl => rgb_to_hsl(rgb),
        Space::Hwb => rgb_to_hwb(rgb),
        Space::SrgbLinear => [srgb_decode(rgb[0]), srgb_decode(rgb[1]), srgb_decode(rgb[2])],
        Space::XyzD65 => mul(&SRGB_TO_XYZ, [srgb_decode(rgb[0]), srgb_decode(rgb[1]), srgb_decode(rgb[2])]),
        Space::XyzD50 => mul(&D65_TO_D50, mul(&SRGB_TO_XYZ, [srgb_decode(rgb[0]), srgb_decode(rgb[1]), srgb_decode(rgb[2])])),
        Space::Lab | Space::Lch => {
            let xyz = mul(&D65_TO_D50, mul(&SRGB_TO_XYZ, [srgb_decode(rgb[0]), srgb_decode(rgb[1]), srgb_decode(rgb[2])]));
            const E: f32 = 216.0 / 24389.0;
            const K: f32 = 24389.0 / 27.0;
            let f = |t: f32| if t > E { t.cbrt() } else { (K * t + 16.0) / 116.0 };
            let (fx, fy, fz) = (f(xyz[0] / 0.96422), f(xyz[1]), f(xyz[2] / 0.82521));
            let lab = [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)];
            if space == Space::Lch { rect_to_polar(lab) } else { lab }
        }
        Space::Oklab | Space::Oklch => {
            let (r, g, b) = (srgb_decode(rgb[0]), srgb_decode(rgb[1]), srgb_decode(rgb[2]));
            let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
            let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
            let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
            let lab = [
                0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
                1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
                0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
            ];
            if space == Space::Oklch { rect_to_polar(lab) } else { lab }
        }
        // Wide-gamut RGB spaces are parse-only here; the sRGB values stand in.
        Space::DisplayP3 | Space::A98 | Space::Prophoto | Space::Rec2020 => rgb,
    }
}

fn to_color(col: &Col) -> Color {
    let rgb = to_srgb(col);
    let q = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color(q(rgb[0]), q(rgb[1]), q(rgb[2]), (none0(col.a).clamp(0.0, 1.0) * 255.0).round() as u8)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Split on whitespace/commas outside parentheses; a bare `/` is its own token.
fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            c if depth == 0 && (c.is_whitespace() || c == ',') => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            '/' if depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push("/".into());
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A channel token: `none`, a number, or a percentage mapped onto `pct_scale`
/// (the value 100% stands for); an angle for hue channels via `hue`.
fn channel(tok: &str, pct_scale: f32, hue: bool, vars: &[(&str, f32)]) -> Option<f32> {
    let t = tok.trim().to_ascii_lowercase();
    if t == "none" {
        return Some(f32::NAN);
    }
    if let Some(v) = vars.iter().find(|(k, _)| *k == t).map(|(_, v)| *v) {
        return Some(v);
    }
    if t.starts_with("calc(") && t.ends_with(')') {
        return eval_expr(&t[5..t.len() - 1], pct_scale, vars);
    }
    if let Some(p) = t.strip_suffix('%') {
        return p.parse::<f32>().ok().map(|p| p / 100.0 * pct_scale);
    }
    if hue {
        for (u, per) in [("deg", 1.0), ("grad", 0.9), ("rad", 180.0 / std::f32::consts::PI), ("turn", 360.0)] {
            if let Some(n) = t.strip_suffix(u) {
                return n.parse::<f32>().ok().map(|v| v * per);
            }
        }
    }
    t.parse::<f32>().ok()
}

/// `+ - * /`, parentheses, numbers (with `%` of `pct_scale`), and channel
/// keywords — the arithmetic relative-colour channel expressions use.
fn eval_expr(s: &str, pct_scale: f32, vars: &[(&str, f32)]) -> Option<f32> {
    struct P<'a> {
        b: &'a [u8],
        i: usize,
        pct: f32,
        vars: &'a [(&'a str, f32)],
    }
    impl P<'_> {
        fn ws(&mut self) {
            while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
        }
        fn expr(&mut self) -> Option<f32> {
            let mut a = self.term()?;
            loop {
                self.ws();
                match self.b.get(self.i) {
                    Some(b'+') => {
                        self.i += 1;
                        a += self.term()?;
                    }
                    Some(b'-') => {
                        self.i += 1;
                        a -= self.term()?;
                    }
                    _ => return Some(a),
                }
            }
        }
        fn term(&mut self) -> Option<f32> {
            let mut a = self.atom()?;
            loop {
                self.ws();
                match self.b.get(self.i) {
                    Some(b'*') => {
                        self.i += 1;
                        a *= self.atom()?;
                    }
                    Some(b'/') => {
                        self.i += 1;
                        a /= self.atom()?;
                    }
                    _ => return Some(a),
                }
            }
        }
        fn atom(&mut self) -> Option<f32> {
            self.ws();
            if self.b.get(self.i) == Some(&b'(') {
                self.i += 1;
                let v = self.expr()?;
                self.ws();
                if self.b.get(self.i) == Some(&b')') {
                    self.i += 1;
                    return Some(v);
                }
                return None;
            }
            let start = self.i;
            if matches!(self.b.get(self.i), Some(b'-') | Some(b'+')) {
                self.i += 1;
            }
            while self.i < self.b.len() && (self.b[self.i].is_ascii_alphanumeric() || matches!(self.b[self.i], b'.' | b'%')) {
                self.i += 1;
            }
            let tok = std::str::from_utf8(&self.b[start..self.i]).ok()?;
            let (neg, name) = match tok.strip_prefix('-') {
                Some(r) => (true, r),
                None => (false, tok.strip_prefix('+').unwrap_or(tok)),
            };
            let v = self
                .vars
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| *v)
                .or_else(|| name.strip_suffix('%').and_then(|p| p.parse::<f32>().ok()).map(|p| p / 100.0 * self.pct))
                .or_else(|| name.parse::<f32>().ok())?;
            Some(if neg { -v } else { v })
        }
    }
    let mut p = P { b: s.as_bytes(), i: 0, pct: pct_scale, vars };
    let v = p.expr()?;
    p.ws();
    (p.i == p.b.len() && v.is_finite()).then_some(v)
}

fn alpha_tok(tok: Option<&String>, vars: &[(&str, f32)]) -> Option<f32> {
    match tok {
        None => Some(1.0),
        Some(t) => channel(t, 1.0, false, vars).map(|a| if a.is_nan() { 0.0 } else { a.clamp(0.0, 1.0) }),
    }
}

/// Channel layout of a function: (space, [percent scale; 3], hue-channel index).
fn layout(name: &str) -> Option<(Space, [f32; 3], Option<usize>)> {
    Some(match name {
        "rgb" | "rgba" => (Space::Srgb, [255.0, 255.0, 255.0], None),
        "hsl" | "hsla" => (Space::Hsl, [360.0, 100.0, 100.0], Some(0)),
        "hwb" => (Space::Hwb, [360.0, 100.0, 100.0], Some(0)),
        "lab" => (Space::Lab, [100.0, 125.0, 125.0], None),
        "lch" => (Space::Lch, [100.0, 150.0, 360.0], Some(2)),
        "oklab" => (Space::Oklab, [1.0, 0.4, 0.4], None),
        "oklch" => (Space::Oklch, [1.0, 0.4, 360.0], Some(2)),
        _ => return None,
    })
}

/// Names of the three channels for the relative-colour keywords, in the
/// numeric scale the function's own channels use.
fn channel_names(name: &str) -> [&'static str; 3] {
    match name {
        "rgb" | "rgba" => ["r", "g", "b"],
        "hsl" | "hsla" => ["h", "s", "l"],
        "hwb" => ["h", "w", "b"],
        "lab" | "oklab" => ["l", "a", "b"],
        _ => ["l", "c", "h"],
    }
}

fn parse_function(name: &str, args: &str) -> Option<Col> {
    let toks = tokens(args);
    if let Some((space, scale, hue_idx)) = layout(name) {
        let mut it = toks.iter().peekable();
        let mut vars_store: Vec<(&str, f32)> = Vec::new();
        let mut base_alpha = 1.0;
        let mut rest: Vec<&String> = Vec::new();
        if it.peek().map(|t| t.eq_ignore_ascii_case("from")) == Some(true) {
            it.next();
            let base_tok = it.next()?;
            let base = parse_col(base_tok)?;
            let names = channel_names(name);
            let ch = from_srgb(space, to_srgb(&base));
            for (i, n) in names.iter().enumerate() {
                let v = if ch[i].is_nan() { 0.0 } else { ch[i] };
                // Our rgb channels live in 0..1; the keywords are 0..255.
                vars_store.push((n, if space == Space::Srgb { v * 255.0 } else { v }));
            }
            base_alpha = base.a;
        }
        rest.extend(it);
        let vars: Vec<(&str, f32)> = {
            let mut v = vars_store.clone();
            v.push(("alpha", base_alpha));
            v
        };
        let slash = rest.iter().position(|t| t.as_str() == "/");
        let (chs, al) = match slash {
            Some(i) => (&rest[..i], rest.get(i + 1).copied()),
            None => (&rest[..], None),
        };
        // Legacy comma syntax carries alpha as a 4th plain token.
        let (chs, al) = if slash.is_none() && chs.len() == 4 { (&chs[..3], Some(chs[3])) } else { (chs, al) };
        if chs.len() != 3 {
            return None;
        }
        let mut c = [0.0f32; 3];
        for i in 0..3 {
            let is_hue = hue_idx == Some(i);
            let mut v = channel(chs[i], scale[i], is_hue, &vars)?;
            // rgb() takes 0..255 -> our 0..1.
            if space == Space::Srgb && !v.is_nan() {
                v /= 255.0;
            }
            // hsl/hwb saturation & lightness percentages: keep 0..100 numbers.
            c[i] = v;
        }
        // Legacy bare numbers for hsl s/l are percentages already (0..100).
        return Some(Col { space, c, a: alpha_tok(al, &vars)? });
    }
    match name {
        "color" => {
            let space = match toks.first()?.to_ascii_lowercase().as_str() {
                "srgb" => Space::Srgb,
                "srgb-linear" => Space::SrgbLinear,
                "display-p3" => Space::DisplayP3,
                "a98-rgb" => Space::A98,
                "prophoto-rgb" => Space::Prophoto,
                "rec2020" => Space::Rec2020,
                "xyz" | "xyz-d65" => Space::XyzD65,
                "xyz-d50" => Space::XyzD50,
                _ => return None,
            };
            let rest = &toks[1..];
            let slash = rest.iter().position(|t| t == "/");
            let (chs, al) = match slash {
                Some(i) => (&rest[..i], rest.get(i + 1)),
                None => (rest, None),
            };
            if chs.len() != 3 {
                return None;
            }
            let c = [channel(&chs[0], 1.0, false, &[])?, channel(&chs[1], 1.0, false, &[])?, channel(&chs[2], 1.0, false, &[])?];
            Some(Col { space, c, a: alpha_tok(al, &[])? })
        }
        "light-dark" => {
            if toks.len() != 2 {
                return None;
            }
            // The persona renders in the light scheme.
            parse_col(&toks[0])
        }
        "color-mix" => parse_mix(args),
        _ => None,
    }
}

fn parse_mix(args: &str) -> Option<Col> {
    // "in <space> [<hue-method> hue], <color> [p%], <color> [p%]"
    let parts = split_commas(args);
    if parts.len() != 3 {
        return None;
    }
    let head = tokens(&parts[0]);
    if head.first().map(|s| s.to_ascii_lowercase()) != Some("in".into()) {
        return None;
    }
    let space = match head.get(1)?.to_ascii_lowercase().as_str() {
        "srgb" => Space::Srgb,
        "srgb-linear" => Space::SrgbLinear,
        "hsl" => Space::Hsl,
        "hwb" => Space::Hwb,
        "lab" => Space::Lab,
        "lch" => Space::Lch,
        "oklab" => Space::Oklab,
        "oklch" => Space::Oklch,
        "xyz" | "xyz-d65" => Space::XyzD65,
        "xyz-d50" => Space::XyzD50,
        // Wide-gamut RGB mixes fall back to sRGB.
        "display-p3" | "a98-rgb" | "prophoto-rgb" | "rec2020" => Space::Srgb,
        _ => return None,
    };
    let hue_method = head.get(2).map(|s| s.to_ascii_lowercase());
    let item = |s: &str| -> Option<(Col, Option<f32>)> {
        let t = tokens(s);
        let mut pct = None;
        let mut color_tok = Vec::new();
        for tok in &t {
            if let Some(p) = tok.strip_suffix('%').and_then(|p| p.parse::<f32>().ok()) {
                pct = Some(p);
            } else {
                color_tok.push(tok.clone());
            }
        }
        Some((parse_col(&color_tok.join(" "))?, pct))
    };
    let (c1, p1) = item(&parts[1])?;
    let (c2, p2) = item(&parts[2])?;
    let (mut w1, mut w2) = match (p1, p2) {
        (None, None) => (50.0, 50.0),
        (Some(a), None) => (a, 100.0 - a),
        (None, Some(b)) => (100.0 - b, b),
        (Some(a), Some(b)) => (a, b),
    };
    if w1 < 0.0 || w2 < 0.0 || w1 + w2 <= 0.0 {
        return None;
    }
    let sum = w1 + w2;
    let alpha_mult = (sum / 100.0).min(1.0);
    w1 /= sum;
    w2 /= sum;
    let ch1 = from_srgb(space, to_srgb(&c1));
    let ch2 = from_srgb(space, to_srgb(&c2));
    let hue_idx = match space {
        Space::Hsl | Space::Hwb => Some(0),
        Space::Lch | Space::Oklch => Some(2),
        _ => None,
    };
    let a = c1.a * w1 + c2.a * w2;
    let mut out = [0.0f32; 3];
    for i in 0..3 {
        let (x, y) = (ch1[i], ch2[i]);
        if hue_idx == Some(i) {
            out[i] = mix_hue(x, y, w1, w2, hue_method.as_deref());
            continue;
        }
        // none-handling: a missing channel takes the other colour's.
        let (x, y) = match (x.is_nan(), y.is_nan()) {
            (true, true) => (0.0, 0.0),
            (true, false) => (y, y),
            (false, true) => (x, x),
            _ => (x, y),
        };
        // Premultiplied interpolation for every channel but hue.
        out[i] = if a > 0.0 { (x * c1.a * w1 + y * c2.a * w2) / a } else { x * w1 + y * w2 };
    }
    Some(Col { space, c: out, a: a * alpha_mult })
}

fn mix_hue(x: f32, y: f32, w1: f32, w2: f32, method: Option<&str>) -> f32 {
    let (x, y) = match (x.is_nan(), y.is_nan()) {
        (true, true) => return f32::NAN,
        (true, false) => (y, y),
        (false, true) => (x, x),
        _ => (x, y),
    };
    let (mut a, mut b) = (x.rem_euclid(360.0), y.rem_euclid(360.0));
    match method.unwrap_or("shorter") {
        "longer" => {
            let d = b - a;
            if d > 0.0 && d < 180.0 { a += 360.0 } else if d > -180.0 && d <= 0.0 { b += 360.0 }
        }
        "increasing" => {
            if b < a { b += 360.0 }
        }
        "decreasing" => {
            if a < b { a += 360.0 }
        }
        _ => {
            let d = b - a;
            if d > 180.0 { a += 360.0 } else if d < -180.0 { b += 360.0 }
        }
    }
    (a * w1 + b * w2).rem_euclid(360.0)
}

fn split_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn parse_col(v: &str) -> Option<Col> {
    let v = v.trim();
    let lower = v.to_ascii_lowercase();
    if let Some(open) = lower.find('(') {
        if !lower.ends_with(')') {
            return None;
        }
        return parse_function(lower[..open].trim(), &v[open + 1..v.len() - 1]);
    }
    if lower == "transparent" {
        return Some(Col { space: Space::Srgb, c: [0.0; 3], a: 0.0 });
    }
    if let Some((_, rgb)) = NAMED.iter().find(|(n, _)| *n == lower) {
        let c = |sh: u32| ((rgb >> sh) & 0xff) as f32 / 255.0;
        return Some(Col { space: Space::Srgb, c: [c(16), c(8), c(0)], a: 1.0 });
    }
    super::color::parse_hex_color(&lower).map(|Color(r, g, b, a)| Col {
        space: Space::Srgb,
        c: [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0],
        a: a as f32 / 255.0,
    })
}

/// The entry `parse_color` tries first: named colours, slash-alpha and the
/// CSS Color 4/5 functions. `None` hands the string back to the legacy parser.
pub(super) fn parse(v: &str) -> Option<Color> {
    parse_col(v).map(|c| to_color(&c))
}
