//! The motion layer (motion preset batch): declarative CSS animation baked
//! into the markdown shell at generation time, so the artifact plays its
//! entrance choreography in any browser with zero scripts — the MP4 frame
//! pump is not involved (video stays an optional later step for voiceover
//! muxing; subtitles are page content here, not burned-in frames).
//!
//! The easing math is GSAP's, absorbed as the public cubic-bezier
//! equivalents rather than a library: power2/3.out and expo.out for the
//! rises, back.out(1.70158) ≈ cubic-bezier(.34, 1.56, .64, 1) for the
//! diagram grow-in overshoot. Stagger is a pure CSS nth-child delay
//! ladder over the body's children, so the DOM stays unpolluted (one
//! wrapper class, no per-element inline hooks) and the bytes stay
//! deterministic. The artifact without `motion` is byte-for-byte the
//! static document; with it, nothing scripts — the file itself animates.

/// The unit a heading splits into (the SplitText move, done at generation
/// time): one span per CJK char or latin word, trailing whitespace folded
/// into the preceding unit so span boundaries only ever coincide with
/// whitespace — line breaking matches the un-split text.
fn motion_units(text: &str) -> Vec<String> {
    let mut units: Vec<String> = Vec::new();
    let mut cur = String::new(); // open latin run (the first may carry leading ws)
    let mut ws = String::new(); // whitespace seen since the last unit closed
    for ch in text.chars() {
        if ch.is_whitespace() {
            ws.push(ch);
            continue;
        }
        let cjk = matches!(ch, '\u{4E00}'..='\u{9FFF}' | '\u{3000}'..='\u{303F}' | '\u{FF00}'..='\u{FFEF}');
        if !ws.is_empty() {
            if !cur.is_empty() {
                // Word boundary: the whitespace trails the open latin run.
                cur.push_str(&ws);
                units.push(std::mem::take(&mut cur));
            } else if let Some(last) = units.last_mut() {
                // After a closed unit (a CJK char): trails it.
                last.push_str(&ws);
            } else {
                // A leading run opens the first unit so no span is empty.
                cur.push_str(&ws);
            }
            ws.clear();
        }
        if cjk {
            // A CJK char is its own unit: close whatever was open first.
            if !cur.is_empty() {
                units.push(std::mem::take(&mut cur));
            }
            units.push(ch.to_string());
        } else {
            cur.push(ch);
        }
    }
    if !ws.is_empty() {
        // Trailing document whitespace rides whatever came last.
        if !cur.is_empty() {
            cur.push_str(&ws);
        } else if let Some(last) = units.last_mut() {
            last.push_str(&ws);
        }
    }
    if !cur.is_empty() {
        units.push(cur);
    }
    units
}

/// Split one heading's text into animated spans with staggered delays
/// (expo-fast rise, one unit every 30ms after `base`). The caller routes
/// this through a pulldown-cmark `Event::Html` so the standard heading
/// markup carries it inline.
pub fn split_heading_html(text: &str, base_secs: f32) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    for (i, unit) in motion_units(text).into_iter().enumerate() {
        let d = base_secs + i as f32 * 0.03;
        out.push_str(&format!(
            "<span class=\"agx-ch\" style=\"animation-delay:{d:.2}s\">{}</span>",
            super::shell::html_escape(&unit),
        ));
    }
    out
}

/// The motion stylesheet: keyframes plus the nth-child delay ladders.
/// `rise` = power2.out up-fade for prose blocks, `char` = expo.out for
/// heading units (em-based so it scales with the heading size), `grow` =
/// back-eased scale for diagram figures. The ladders stagger the body's
/// direct children (40 rungs, 70ms apart, clamped tail) and list items
/// within each list (20 rungs, 50ms apart) — pure CSS, so the DOM carries
/// no per-element hooks and the bytes stay deterministic.
///
/// The prefers-reduced-motion resets carry `!important` because the story
/// layer's per-element rules are generated at render time with specificity
/// the static stylesheet cannot know in advance (the edge rules alone sit
/// at three attribute selectors plus a sibling combinator) and they append
/// after this block — a plain reset would lose the cascade tie or the
/// specificity race outright. An accessibility override that must beat
/// generated rules regardless of their shape is the canonical !important.
/// The caption strip additionally flips to `display:block` so the static
/// transcript reads one beat per line instead of a run-on inline smear.
pub fn motion_css() -> String {
    let mut css = String::from(
        "@keyframes agx-rise{from{opacity:0;transform:translateY(10px)}}\
@keyframes agx-char{from{opacity:0;transform:translateY(.45em)}}\
@keyframes agx-grow{from{opacity:0;transform:scale(.965)}}\
@keyframes agx-node{from{opacity:0;transform:scale(.9)}}\
@keyframes agx-in{from{opacity:0}}\
@keyframes agx-draw{from{opacity:0;stroke-dashoffset:4000}}\
@keyframes agx-cap{0%{opacity:0;transform:translateY(6px)}12%,84%{opacity:1;transform:none}100%{opacity:0}}\
.agx-motion>*{animation:agx-rise .55s cubic-bezier(.33,1,.68,1) both}\
.agx-motion>h1,.agx-motion>h2,.agx-motion>h3{animation:none}\
.agx-motion>figure{animation:agx-grow .65s cubic-bezier(.34,1.56,.64,1) both}\
.agx-ch{display:inline;animation:agx-char .5s cubic-bezier(.16,1,.3,1) both}\
.agx-motion li{animation:agx-rise .45s cubic-bezier(.33,1,.68,1) both}\
.agx-motion figure svg [data-node-id],.agx-motion figure svg [data-participant-id]{transform-box:fill-box;transform-origin:center}\
.agx-caps{position:relative;min-height:2.5em;margin:.55rem 0 0}\
.agx-cap{position:absolute;left:0;right:0;top:0;text-align:center;font-size:24px;font-weight:600;line-height:1.5;opacity:0;animation-name:agx-cap;animation-fill-mode:both;animation-timing-function:ease-out}\
@media(prefers-reduced-motion:reduce){.agx-motion>*,.agx-ch,.agx-motion li,.agx-motion figure [data-node-id],.agx-motion figure [data-participant-id],.agx-motion figure [data-message-index],.agx-motion figure path[data-from],.agx-motion figure path[data-from]+path,.agx-motion figure g[data-from]{animation:none!important}.agx-motion .agx-cap{animation:none!important;opacity:1;position:static;display:block}}",
    );
    // Body-child ladder: `> *` (0-1-0) sets the animation, the nth-child
    // rules (0-2-0) win on the delay property they override.
    for k in 1..=40 {
        css.push_str(&format!(
            ".agx-motion>*:nth-child({k}){{animation-delay:{:.2}s}}",
            0.05 + k as f32 * 0.07
        ));
    }
    css.push_str(".agx-motion>*:nth-child(n+41){animation-delay:2.85s}");
    for k in 1..=20 {
        css.push_str(&format!(
            ".agx-motion li:nth-child({k}){{animation-delay:{:.2}s}}",
            k as f32 * 0.05
        ));
    }
    css.push_str(".agx-motion li:nth-child(n+21){animation-delay:1s}");
    css
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_split_cjk_per_char_latin_per_word() {
        // Latin runs carry their trailing space; an interior space after a
        // CJK char folds into that char's unit — span boundaries only ever
        // coincide with whitespace, so line breaking is unchanged.
        let u = motion_units("Hello 世界 foo");
        assert_eq!(u, vec!["Hello ", "世", "界 ", "foo"]);
        let u2 = motion_units("one two");
        assert_eq!(u2, vec!["one ", "two"]);
        let u3 = motion_units("标题A");
        assert_eq!(u3, vec!["标", "题", "A"]);
    }

    #[test]
    fn css_carries_all_three_keyframes_and_the_ladder_selectors() {
        let css = motion_css();
        for needle in [
            "@keyframes agx-rise",
            "@keyframes agx-char",
            "@keyframes agx-grow",
            ".agx-motion>*",
            ".agx-motion>figure",
            ".agx-ch",
            ".agx-motion li",
            "cubic-bezier(.34,1.56,.64,1)",
            // The stagger ladders and their clamped tails.
            ".agx-motion>*:nth-child(1){animation-delay:0.12s}",
            ".agx-motion>*:nth-child(40)",
            ".agx-motion>*:nth-child(n+41)",
            ".agx-motion li:nth-child(3){animation-delay:0.15s}",
            ".agx-motion li:nth-child(n+21)",
            "prefers-reduced-motion",
            // The reduced-motion contract: resets must beat the generated
            // story rules (hence !important), cover the arrowhead sibling,
            // and stack the caption transcript one beat per line.
            "animation:none!important",
            "path[data-from]+path",
            "position:static;display:block",
        ] {
            assert!(css.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn leading_whitespace_opens_the_first_unit() {
        let u = motion_units("  x");
        assert_eq!(u, vec!["  x"]);
        assert!(!u.is_empty());
    }

    #[test]
    fn heading_spans_carry_escalating_delays_and_escape() {
        let html = split_heading_html("a<b", 0.1);
        // The punctuation rides the latin run; escaped markup can't break
        // out of the span.
        assert!(html.contains("animation-delay:0.10s\">a&lt;b</span>"), "{html}");
        // CJK chars split per glyph with the 30ms step.
        let zh = split_heading_html("标题", 0.0);
        assert!(zh.contains("animation-delay:0.00s\">标</span>"), "{zh}");
        assert!(zh.contains("animation-delay:0.03s\">题</span>"), "{zh}");
    }
}
