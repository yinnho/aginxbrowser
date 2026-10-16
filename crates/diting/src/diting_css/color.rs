//! Color + linear-gradient parsing (god-file ratchet split from mod.rs, pure move).

use super::*;

/// Named colors + #rgb/#rrggbbaa hex (the forms real sheets overwhelmingly use).
pub fn parse_color(v: &str) -> Option<Color> {
    let v = v.trim().to_ascii_lowercase();
    // rgb()/rgba() — channels as 0-255 or %, alpha 0-1 (batch 4a: the most
    // common authored background format on real pages, and the one the
    // paint cross-checks style backgrounds with).
    if let Some(rest) = v
        .strip_prefix("rgb(")
        .or_else(|| v.strip_prefix("rgba("))
        .and_then(|r| r.strip_suffix(')'))
    {
        let nums: Option<Vec<f32>> = rest
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(|tok| {
                if let Some(pct) = tok.strip_suffix('%') {
                    pct.css_f32().map(|p| p * 255.0 / 100.0)
                } else {
                    tok.css_f32()
                }
            })
            .collect();
        let nums = nums?;
        if nums.len() != 3 && nums.len() != 4 {
            return None;
        }
        let chan = |x: f32| x.round().clamp(0.0, 255.0) as u8;
        let a = if nums.len() == 4 { (nums[3].clamp(0.0, 1.0) * 255.0).round() as u8 } else { 255 };
        return Some(Color(chan(nums[0]), chan(nums[1]), chan(nums[2]), a));
    }
    // hsl()/hsla() (#136, obscura #1085): hue is a bare number, deg, or
    // turn; s/l are percentages (a bare number reads on the same 0-100
    // scale, like Chrome); alpha is 0-1 or %. Comma and space separated.
    if let Some(rest) = v
        .strip_prefix("hsl(")
        .or_else(|| v.strip_prefix("hsla("))
        .and_then(|r| r.strip_suffix(')'))
    {
        let toks: Vec<&str> = rest
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .collect();
        if toks.len() != 3 && toks.len() != 4 {
            return None;
        }
        let hue = if let Some(n) = toks[0].strip_suffix("turn") {
            n.css_f32().map(|x| x * 360.0)
        } else {
            toks[0].strip_suffix("deg").unwrap_or(toks[0]).css_f32()
        }?;
        let frac = |tok: &str| {
            tok.strip_suffix('%')
                .and_then(|p| p.css_f32())
                .or_else(|| tok.css_f32())
                .map(|p| (p / 100.0).clamp(0.0, 1.0))
        };
        let sat = frac(toks[1])?;
        let lig = frac(toks[2])?;
        let a = if toks.len() == 4 {
            toks[3]
                .strip_suffix('%')
                .and_then(|p| p.css_f32().map(|p| p / 100.0))
                .or_else(|| toks[3].css_f32())
                .map(|a| a.clamp(0.0, 1.0))?
        } else {
            1.0
        };
        let hue = ((hue % 360.0) + 360.0) % 360.0;
        return Some(hsl_to_color(hue, sat, lig, a));
    }
    let named = match v.as_str() {
        "black" => Color(0, 0, 0, 255),
        "white" => Color(255, 255, 255, 255),
        "red" => Color(255, 0, 0, 255),
        "green" => Color(0, 128, 0, 255),
        "lime" => Color(0, 255, 0, 255),
        "blue" => Color(0, 0, 255, 255),
        "gray" | "grey" => Color(128, 128, 128, 255),
        "silver" => Color(192, 192, 192, 255),
        "transparent" => Color(0, 0, 0, 0),
        _ => return parse_hex_color(&v),
    };
    Some(named)
}

/// CSS HSL → sRGB (the chroma ladder, CSS Color 3 §4.2.1/§4.2.5): h in
/// [0,360), s/l/a already normalized to [0,1]. Feeds parse_color's
/// hsl()/hsla() arm (#136).
pub(super) fn hsl_to_color(h: f32, s: f32, l: f32, a: f32) -> Color {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let ch = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    Color(ch(r), ch(g), ch(b), (a * 255.0).round() as u8)
}

pub(super) fn parse_hex_color(v: &str) -> Option<Color> {
    let hex = v.strip_prefix('#')?;
    let (r, g, b, a) = match hex.len() {
        3 => {
            let n: Vec<u8> = hex.chars().filter_map(|c| c.to_digit(16).map(|d| d as u8)).collect();
            if n.len() != 3 {
                return None;
            }
            (
                n[0] * 17,
                n[1] * 17,
                n[2] * 17,
                255,
            )
        }
        6 | 8 => {
            let bytes = (0..hex.len())
                .step_by(2)
                .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
                .collect::<Vec<u8>>();
            if bytes.len() != hex.len() / 2 {
                return None;
            }
            match bytes.as_slice() {
                [r, g, b] => (*r, *g, *b, 255),
                [r, g, b, a] => (*r, *g, *b, *a),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(Color(r, g, b, a))
}

/// One parsed CSS `linear-gradient(...)`: stops as (0..1 position, color),
/// ascending by position, plus the CSS angle in degrees (0 = to top,
/// clockwise; 180 = the `linear-gradient(a, b)` default, to bottom).
/// Authored for the native PPTX exporter (`gradFill`), promoted to a
/// shared parse when the diting paint layer grew its own background-image
/// consumer — one grammar, two backends.
pub struct LinearGradient {
    pub stops: Vec<(f32, Color)>,
    pub css_deg: f32,
}

/// Parse a `linear-gradient(<angle>, <color> [pos%], ...)` value. Angle
/// forms: bare `135deg` or `to top/right/bottom/left` (corners rejected,
/// v1). Missing stop positions distribute evenly (all missing) or
/// interpolate between the nearest known neighbors (some missing), per the
/// CSS rule. Anything else — `none`, `url(...)`, radial/repeating — is
/// None so callers keep their fallback fill.
pub fn parse_linear_gradient(raw: &str) -> Option<LinearGradient> {
    let v = raw.trim().to_ascii_lowercase();
    let rest = v.strip_prefix("linear-gradient(")?.strip_suffix(')')?;
    // Top-level comma split (rgb()/rgba() carry commas of their own).
    let mut parts: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in rest.chars() {
        match ch {
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    parts.push(cur);
    // Two parts is the floor (`red, blue` with no angle spec); an angle
    // that starves the stop list falls through to the colors.len() gate.
    if parts.len() < 2 {
        return None;
    }

    // First part: angle spec or the first stop.
    let mut css_deg = 180.0; // CSS default: to bottom
    let mut stop_parts: &[String] = &parts;
    let first = parts[0].trim();
    if let Some(deg) = first.strip_suffix("deg") {
        css_deg = deg.trim().parse().ok()?;
        stop_parts = &parts[1..];
    } else if let Some(dir) = first.strip_prefix("to ") {
        css_deg = match dir.trim() {
            "top" => 0.0,
            "right" => 90.0,
            "bottom" => 180.0,
            "left" => 270.0,
            _ => return None, // corners: v1 doesn't map diagonal keywords
        };
        stop_parts = &parts[1..];
    }

    // Stops: "<color> [pos%]" — color first (may contain spaces inside
    // parens), an optional trailing percent position.
    let mut colors: Vec<Color> = Vec::new();
    let mut positions: Vec<Option<f32>> = Vec::new();
    for p in stop_parts {
        let p = p.trim();
        if p.is_empty() {
            return None;
        }
        // Whole part is a bare color (no position).
        if let Some(c) = parse_color(p) {
            colors.push(c);
            positions.push(None);
            continue;
        }
        let (head, tail) = p.rsplit_once(' ')?;
        let c = parse_color(head.trim())?;
        let pos = tail.trim().strip_suffix('%')?.trim().css_f32()? / 100.0;
        colors.push(c);
        positions.push(Some(pos));
    }
    if colors.len() < 2 {
        return None;
    }

    // Missing positions distribute evenly (all missing), or interpolate
    // between the nearest known neighbors (some missing) — the CSS
    // interpolation rule, to slide fidelity.
    let n = colors.len();
    let mut pos: Vec<f32> = vec![0.0; n];
    if positions.iter().all(|p| p.is_none()) {
        for (i, slot) in pos.iter_mut().enumerate() {
            *slot = i as f32 / (n - 1) as f32;
        }
    } else {
        positions[0] = positions[0].or(Some(0.0));
        positions[n - 1] = positions[n - 1].or(Some(1.0));
        for i in 0..n {
            if positions[i].is_none() {
                let (mut lo, mut hi) = (i, i);
                while lo > 0 && positions[lo].is_none() {
                    lo -= 1;
                }
                while hi + 1 < n && positions[hi].is_none() {
                    hi += 1;
                }
                let a = positions[lo].unwrap_or(0.0);
                let b = positions[hi].unwrap_or(1.0);
                pos[i] = a + (b - a) * (i - lo) as f32 / (hi - lo).max(1) as f32;
            } else {
                pos[i] = positions[i].unwrap_or(0.0);
            }
        }
    }

    let mut stops: Vec<(f32, Color)> = pos.into_iter().zip(colors).collect();
    // Out-of-order authored stops clamp up to their predecessor at raster
    // time per CSS; a stable sort is the equivalent here (equal positions
    // keep the hard-line transition).
    stops.sort_by(|a, b| a.0.total_cmp(&b.0));
    Some(LinearGradient { stops, css_deg })
}
