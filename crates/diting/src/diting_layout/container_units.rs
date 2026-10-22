//! Container-query unit baking (god-file ratchet split from mod.rs, pure move).


/// Rewrite container-query lengths (`10cqw`, `2.5cqh`, ...) against the
/// answering container's border box into px, so the ordinary declaration
/// parser never sees the units. Non-container tokens (including hex colors
/// and URLs containing digit+letter runs) pass through byte-identical.
pub(super) fn bake_container_units(text: &str, w: f32, h: f32) -> String {
    fn push_px(out: &mut String, px: f32) {
        if px.fract() == 0.0 && px.abs() < 1e15 {
            out.push_str(&format!("{}px", px as i64));
        } else {
            out.push_str(&format!("{:.3}", px));
            out.push_str("px");
        }
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let num_start = c.is_ascii_digit()
            || (c == '-' && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()))
            || (c == '.' && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()));
        if !num_start {
            out.push(c);
            i += 1;
            continue;
        }
        let start = i;
        if chars[i] == '-' {
            i += 1;
        }
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
            i += 1;
        }
        let ustart = i;
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit: String = chars[ustart..i].iter().collect();
        match unit.as_str() {
            "cqw" | "cqi" => {
                let v: f32 = chars[start..ustart].iter().collect::<String>().parse().unwrap_or(0.0);
                push_px(&mut out, v * w / 100.0);
            }
            "cqh" | "cqb" => {
                let v: f32 = chars[start..ustart].iter().collect::<String>().parse().unwrap_or(0.0);
                push_px(&mut out, v * h / 100.0);
            }
            _ => out.extend(chars[start..i].iter()),
        }
    }
    out
}
