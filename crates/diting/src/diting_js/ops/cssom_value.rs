//! getComputedStyle value spelling (CSSOM serialization of used/computed values) (god-file ratchet split from mod.rs, pure move).

#[cfg(feature = "screenshot")]
use super::*;

/// Chrome's UA-sheet display for the tags whose CSSOM value differs from the
/// Block bucket the layout engine gives them. Layout intentionally has no
/// table/list box types, so `td` lays out as a block — but getComputedStyle
/// must still answer like a browser (obscura #771: a table's box type was
/// unreadable over CDP). Only consulted while `display_from_ua` is set.
#[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
pub(super) fn ua_display_cssom(tag: Option<&str>) -> Option<&'static str> {
    match tag? {
        "table" => Some("table"),
        "tr" => Some("table-row"),
        "td" | "th" => Some("table-cell"),
        "thead" | "tbody" | "tfoot" => Some("table-row-group"),
        "col" => Some("table-column"),
        "colgroup" => Some("table-column-group"),
        "caption" => Some("table-caption"),
        "li" => Some("list-item"),
        _ => None,
    }
}

/// Chrome's computed spelling for a timing function (shared by the
/// animation-timing-function and transition-timing-function arms).
#[cfg(feature = "screenshot")]
pub(super) fn easing_cssom(e: crate::diting_css::Easing) -> String {
    use crate::diting_css::Easing;
    match e {
        Easing::Linear => "linear".into(),
        Easing::CubicBezier(0.25, 0.1, 0.25, 1.0) => "ease".into(),
        Easing::CubicBezier(0.42, 0.0, 1.0, 1.0) => "ease-in".into(),
        Easing::CubicBezier(0.0, 0.0, 0.58, 1.0) => "ease-out".into(),
        Easing::CubicBezier(0.42, 0.0, 0.58, 1.0) => "ease-in-out".into(),
        Easing::CubicBezier(a, b, c, d) => format!(
            "cubic-bezier({}, {}, {}, {})",
            format_number(a),
            format_number(b),
            format_number(c),
            format_number(d)
        ),
    }
}

/// `", lower-roman"`-style suffix for counter()/counters() re-serialization;
/// decimal is the initial value so Chrome omits it.
#[cfg(feature = "screenshot")]
pub(super) fn counter_style_suffix(style: crate::diting_css::CounterStyle) -> String {
    use crate::diting_css::CounterStyle;
    match style {
        CounterStyle::Decimal => String::new(),
        CounterStyle::DecimalLeadingZero => ", decimal-leading-zero".into(),
        CounterStyle::LowerAlpha => ", lower-alpha".into(),
        CounterStyle::UpperAlpha => ", upper-alpha".into(),
        CounterStyle::LowerRoman => ", lower-roman".into(),
        CounterStyle::UpperRoman => ", upper-roman".into(),
    }
}

/// Serialize one property of a cascaded [`ComputedStyle`] in Chrome's
/// computed-value spelling, for the `computed_style` op (getComputedStyle's
/// cascade layer). `None` means "not in the table" — the JS caller falls
/// through to its inline/dimension/default chain, so initial values never
/// shadow a caller that knows better. `cb_inline` is the element's
/// containing-block inline size when the op had geometry (#242): percentage
/// padding/margin arms resolve to Chrome's used px; without it (boxless
/// elements) they keep the declared spelling.
#[cfg(feature = "screenshot")]
pub(super) fn computed_style_value(
    s: &crate::diting_css::ComputedStyle,
    prop: &str,
    cssom_tag: Option<&str>,
    cb_inline: Option<f32>,
) -> Option<String> {
    use crate::diting_css::*;
    let color = |c: &Color| {
        if c.3 == 255 {
            format!("rgb({}, {}, {})", c.0, c.1, c.2)
        } else {
            let a = format!("{:.3}", c.3 as f32 / 255.0);
            let a = a.trim_end_matches('0').trim_end_matches('.');
            format!("rgba({}, {}, {}, {})", c.0, c.1, c.2, if a.is_empty() { "0" } else { a })
        }
    };
    match prop {
        "position" => Some(
            match s.position {
                Some(PositionMode::Relative) => "relative",
                Some(PositionMode::Absolute) => "absolute",
                Some(PositionMode::Fixed) => "fixed",
                Some(PositionMode::Sticky) => "sticky",
                _ => "static",
            }
            .into(),
        ),
        "display" => {
            if s.display_from_ua {
                if let Some(ua) = ua_display_cssom(cssom_tag) {
                    return Some(ua.to_string());
                }
            }
            Some(
                match s.display {
                    Some(Display::Inline) => "inline",
                    Some(Display::InlineBlock) => "inline-block",
                    Some(Display::Flex) => "flex",
                    Some(Display::Grid) => "grid",
                    Some(Display::Table) => "table",
                    Some(Display::TableRow) => "table-row",
                    Some(Display::TableCell) => "table-cell",
                    Some(Display::None) => "none",
                    _ => "block",
                }
                .into(),
            )
        },
        "z-index" => Some(match s.z_index {
            Some(z) => z.to_string(),
            None => "auto".into(),
        }),
        "border-collapse" => Some(
            match s.border_collapse {
                Some(BorderCollapse::Collapse) => "collapse",
                _ => "separate",
            }
            .into(),
        ),
        "table-layout" => Some(
            match s.table_layout {
                Some(TableLayout::Fixed) => "fixed",
                _ => "auto",
            }
            .into(),
        ),
        // Declared values only: undeclared cells keep the JS caller's own
        // default chain (the UA middle behavior at the alignment site isn't
        // a declaration). Chrome's computed value: lengths are absolute px,
        // percentages keep their % shape.
        "vertical-align" => s
            .vertical_align
            .map(|va| match va {
                VerticalAlign::Top => "top".to_string(),
                VerticalAlign::Middle => "middle".to_string(),
                VerticalAlign::Bottom => "bottom".to_string(),
                VerticalAlign::Baseline => "baseline".to_string(),
                VerticalAlign::Sub => "sub".to_string(),
                VerticalAlign::Super => "super".to_string(),
                VerticalAlign::Length(px) => format!("{px}px"),
                VerticalAlign::Percent(p) => format!("{p}%"),
            }),
        // Not inherited (the paint-time propagation to inline descendants is
        // a layout concern): an element reports its OWN declared/UA set.
        "text-decoration-line" | "text-decoration" => Some(match s.text_decoration_line {
            Some(d) if !d.is_empty() => [
                if d.underline { "underline" } else { "" },
                if d.overline { "overline" } else { "" },
                if d.line_through { "line-through" } else { "" },
            ]
            .into_iter()
            .filter(|k| !k.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
            _ => "none".to_string(),
        }),
        "float" => Some(
            match s.float_side {
                Some(FloatSide::Left) => "left",
                Some(FloatSide::Right) => "right",
                None => "none",
            }
            .into(),
        ),
        "clear" => Some(
            match s.clear_side {
                Some(ClearSide::Left) => "left",
                Some(ClearSide::Right) => "right",
                Some(ClearSide::Both) => "both",
                Some(ClearSide::InlineStart) => "inline-start",
                Some(ClearSide::InlineEnd) => "inline-end",
                None => "none",
            }
            .into(),
        ),
        // Chrome's computed value is the declared keyword itself (not a used
        // value), so the CSS initial surfaces as content-box.
        // Chrome's initial computed content is "normal"; declared strings
        // re-serialize quoted, attr()/counter()/counters()/quotes keywords
        // keep their functional shape. On pseudos the resolved plain string
        // surfaces instead (documented divergence).
        "content" => Some(match &s.content {
            Some(ContentValue::Str(t)) => {
                format!("\"{}\"", t.replace('\\', "\\\\").replace('"', "\\\""))
            },
            Some(ContentValue::Attr(name)) => format!("attr({})", name),
            Some(ContentValue::Counter { name, style }) => {
                format!("counter({}{})", name, counter_style_suffix(*style))
            },
            Some(ContentValue::Counters { name, sep, style }) => format!(
                "counters({}, \"{}\"{})",
                name,
                sep.replace('\\', "\\\\").replace('"', "\\\""),
                counter_style_suffix(*style)
            ),
            Some(ContentValue::OpenQuote) => "open-quote".into(),
            Some(ContentValue::CloseQuote) => "close-quote".into(),
            Some(ContentValue::NoQuote { close }) => {
                if *close { "no-close-quote".into() } else { "no-open-quote".into() }
            },
            Some(ContentValue::List(parts)) => parts
                .iter()
                .map(|p| match p {
                    ContentValue::Str(t) => {
                        format!("\"{}\"", t.replace('\\', "\\\\").replace('"', "\\\""))
                    },
                    ContentValue::Attr(name) => format!("attr({})", name),
                    ContentValue::Counter { name, style } => {
                        format!("counter({}{})", name, counter_style_suffix(*style))
                    },
                    ContentValue::Counters { name, sep, style } => format!(
                        "counters({}, \"{}\"{})",
                        name,
                        sep.replace('\\', "\\\\").replace('"', "\\\""),
                        counter_style_suffix(*style)
                    ),
                    ContentValue::OpenQuote => "open-quote".into(),
                    ContentValue::CloseQuote => "close-quote".into(),
                    ContentValue::NoQuote { close } => {
                        if *close { "no-close-quote" } else { "no-open-quote" }.into()
                    },
                    ContentValue::List(_) => "normal".into(),
                })
                .collect::<Vec<_>>()
                .join(" "),
            None => "normal".into(),
        }),
        "box-sizing" => Some(
            match s.box_sizing {
                Some(BoxSizing::BorderBox) => "border-box",
                _ => "content-box",
            }
            .into(),
        ),
        // Same declared-keyword convention: `text` or the CSS initial's
        // border-box. The -webkit- alias reads through here too.
        "background-clip" => Some(if s.background_clip_text { "text" } else { "border-box" }.into()),
        "overflow" => {
            let (x, y) = s.resolved_overflow();
            Some(if x == y {
                crate::diting_css::overflow_name(x).into()
            } else {
                format!("{} {}", crate::diting_css::overflow_name(x), crate::diting_css::overflow_name(y))
            })
        }
        "overflow-x" => {
            let (x, _) = s.resolved_overflow();
            Some(crate::diting_css::overflow_name(x).into())
        }
        "overflow-y" => {
            let (_, y) = s.resolved_overflow();
            Some(crate::diting_css::overflow_name(y).into())
        }
        "font-size" => s.font_size.map(|f| format!("{}px", f)),
        "font-weight" => s.font_weight.map(|w| w.to_string()),
        "text-align" => Some(
            match s.text_align {
                Some(TextAlign::Center) => "center",
                Some(TextAlign::Right) => "right",
                // #188-3: start/end stay LOGICAL in the computed style —
                // Chrome reports them verbatim (measured: undeclared
                // reports "start" too, the css-text-3 initial value);
                // consume-time resolution flips the painted side, not
                // this face.
                Some(TextAlign::Start) | None => "start",
                Some(TextAlign::End) => "end",
                Some(TextAlign::Left) => "left",
                // #214: parsed/reported verbatim; the stretch itself is not
                // painted (consumed as start by layout).
                Some(TextAlign::Justify) => "justify",
            }
            .into(),
        ),
        "text-align-last" => Some(
            match s.text_align_last {
                Some(crate::diting_css::TextAlignLast::Start) => "start",
                Some(crate::diting_css::TextAlignLast::End) => "end",
                Some(crate::diting_css::TextAlignLast::Left) => "left",
                Some(crate::diting_css::TextAlignLast::Right) => "right",
                Some(crate::diting_css::TextAlignLast::Center) => "center",
                Some(crate::diting_css::TextAlignLast::Justify) => "justify",
                Some(crate::diting_css::TextAlignLast::Auto) | None => "auto",
            }
            .into(),
        ),
        "direction" => Some(
            match s.direction {
                Some(crate::diting_css::TextDirection::Rtl) => "rtl",
                _ => "ltr",
            }
            .into(),
        ),
        "line-height" => Some(
            match s.line_height {
                Some(LineHeightSpec::Number(n)) => format_number(n),
                Some(LineHeightSpec::Px(v)) => format!("{}px", format_number(v)),
                _ => "normal".into(),
            },
        ),
        "word-spacing" => Some(
            match s.word_spacing {
                Some(v) => format!("{}px", format_number(v)),
                None => "normal".into(),
            },
        ),
        "font-variant-caps" => Some(
            match s.font_variant_caps {
                Some(true) => "small-caps".into(),
                _ => "normal".into(),
            },
        ),
        // Declared-only (`color` posture): unset returns None so the JS
        // caller's default chain serves the initial — and the innerText walk
        // can tell a declared `none` (resets inheritance) from unset (keeps
        // the ancestor's transform). Same divergence-from-Chrome posture as
        // the other inherited props here (white-space).
        "text-transform" => s.text_transform.map(|t| match t {
            TextTransform::Uppercase => "uppercase".into(),
            TextTransform::Lowercase => "lowercase".into(),
            TextTransform::Capitalize => "capitalize".into(),
            TextTransform::None => "none".into(),
        }),
        "font-variant" => Some(
            match s.font_variant_caps {
                Some(true) => "small-caps".into(),
                _ => "normal".into(),
            },
        ),
        "container-type" => Some(
            match s.container_type {
                crate::diting_css::ContainerType::InlineSize => "inline-size",
                crate::diting_css::ContainerType::Size => "size",
                _ => "normal",
            }
            .into(),
        ),
        "container-name" => Some(s.container_name.clone().unwrap_or_else(|| "none".into())),
        "white-space" => Some(
            match s.white_space {
                Some(crate::diting_css::WhiteSpace::Nowrap) => "nowrap",
                Some(crate::diting_css::WhiteSpace::Pre) => "pre",
                Some(crate::diting_css::WhiteSpace::PreWrap) => "pre-wrap",
                Some(crate::diting_css::WhiteSpace::PreLine) => "pre-line",
                Some(crate::diting_css::WhiteSpace::BreakSpaces) => "break-spaces",
                _ => "normal",
            }
            .into(),
        ),
        "text-overflow" => Some(
            match s.text_overflow {
                Some(crate::diting_css::TextOverflow::Ellipsis) => "ellipsis",
                _ => "clip",
            }
            .into(),
        ),
        // #271: both spellings answer from the one slot; Chrome serializes
        // the number bare ("2") and the initial as "none".
        "line-clamp" | "-webkit-line-clamp" => Some(
            match s.line_clamp {
                Some(n) => n.to_string().into(),
                None => "none".into(),
            },
        ),
        "color" => s.color.as_ref().map(&color),
        "background-color" => s.background_color.as_ref().map(&color),
        // Raw author token stream (var() already substituted by the cascade);
        // unset stays absent so the JS initial (`none`) serves it.
        "background-image" => s.background_image.clone(),
        // Same posture as background-image: the author's stack, verbatim.
        "font-family" => s.font_family.clone(),
        // Padding/margin longhands: Chrome reports the used px — percentage
        // and calc sides resolve against the containing-block inline size
        // the op supplies (#242); without geometry the declared spelling
        // stays (the old posture — resolving needs a containing block this
        // layer has no geometry for). Unset is 0.
        "padding-top" => Some(side_css_used(&s.padding.top, cb_inline)),
        "padding-right" => Some(side_css_used(&s.padding.right, cb_inline)),
        "padding-bottom" => Some(side_css_used(&s.padding.bottom, cb_inline)),
        "padding-left" => Some(side_css_used(&s.padding.left, cb_inline)),
        // Border widths ride the same path so the iframe viewport math in
        // bootstrap can peel the UA 2px border off the measured border box.
        // Chrome answers 0px whenever the style is none/hidden — the width
        // keyword stays declared but the used border is gone (`border: none`
        // on a webtop iframe must not shrink its document's viewport).
        "border-top-width" => Some(if s.border_style.is_some() { side_css(&s.border_width.top) } else { "0px".into() }),
        "border-right-width" => Some(if s.border_style.is_some() { side_css(&s.border_width.right) } else { "0px".into() }),
        "border-bottom-width" => Some(if s.border_style.is_some() { side_css(&s.border_width.bottom) } else { "0px".into() }),
        "border-left-width" => Some(if s.border_style.is_some() { side_css(&s.border_width.left) } else { "0px".into() }),
        // #207: the style/color longhands were absent, so the JS mask served
        // "" where Chrome answers the keyword / resolved color. Style is the
        // declared keyword (uniform across sides — per-side styles are a later
        // batch) with `none` for the initial; color is currentcolor resolved
        // Chrome-style (declared border color, else the element's color, else
        // black).
        "border-top-style" | "border-right-style" | "border-bottom-style" | "border-left-style" => Some(
            match s.border_style {
                Some(BorderStyle::Solid) => "solid",
                Some(BorderStyle::Dashed) => "dashed",
                Some(BorderStyle::Dotted) => "dotted",
                Some(BorderStyle::Double) => "double",
                None => "none",
            }
            .into(),
        ),
        "border-top-color" | "border-right-color" | "border-bottom-color" | "border-left-color" => Some(
            match s.border_color.or(s.color) {
                Some(c) => color(&c),
                None => "rgb(0, 0, 0)".into(),
            },
        ),
        "margin-top" => Some(side_css_used(&s.margin.top, cb_inline)),
        "margin-right" => Some(side_css_used(&s.margin.right, cb_inline)),
        "margin-bottom" => Some(side_css_used(&s.margin.bottom, cb_inline)),
        "margin-left" => Some(side_css_used(&s.margin.left, cb_inline)),
        // #244: logical box longhands serve from the same slots as their
        // physical twins — horizontal-tb maps block→top/bottom, inline→
        // left/right. Our direction face doesn't mirror inline flow, so
        // inline-start=left is the self-consistent mapping (Chrome under
        // rtl maps inline-start to right; that flips only when our inline
        // layout mirrors).
        "margin-block-start" => Some(side_css_used(&s.margin.top, cb_inline)),
        "margin-block-end" => Some(side_css_used(&s.margin.bottom, cb_inline)),
        "margin-inline-start" => Some(side_css_used(&s.margin.left, cb_inline)),
        "margin-inline-end" => Some(side_css_used(&s.margin.right, cb_inline)),
        "padding-block-start" => Some(side_css_used(&s.padding.top, cb_inline)),
        "padding-block-end" => Some(side_css_used(&s.padding.bottom, cb_inline)),
        "padding-inline-start" => Some(side_css_used(&s.padding.left, cb_inline)),
        "padding-inline-end" => Some(side_css_used(&s.padding.right, cb_inline)),
        // #244: transform-origin in Chrome's computed form — keywords stay
        // normalized percentages and the z component pins 0px, so the
        // initial value reads "50% 50% 0px" (the issue's Chrome ground
        // truth); declared px survives verbatim.
        "transform-origin" => Some({
            let spell = |l: &Length| match l {
                Length::Px(v) => format!("{}px", format_number(*v)),
                Length::Percent(p) => format!("{}%", format_number(*p)),
                _ => "50%".to_string(),
            };
            let (x, y) = s
                .transform_origin
                .unwrap_or((Length::Percent(50.0), Length::Percent(50.0)));
            format!("{} {} 0px", spell(&x), spell(&y))
        }),
        // Chrome's computed border-radius collapses equal corners (up to the
        // shortest form that round-trips); elliptical corners serialize with
        // the slash form. Percent stays percent — resolving against the box
        // is a used value this layer has no geometry for, and "12%" is closer
        // to truth than the JS mask's "0px".
        "border-radius" => Some({
            let spell = |l: &Length| match l {
                Length::Px(v) => format!("{}px", format_number(*v)),
                Length::Percent(p) => format!("{}%", format_number(*p)),
                // Keyword lengths never parse into a corner radius; the
                // fallback just keeps the arm total.
                _ => "0px".to_string(),
            };
            match &s.corner_radii {
                Some([(tl_x, tl_y), (tr_x, tr_y), (br_x, br_y), (bl_x, bl_y)]) => {
                    let x = format!(
                        "{} {} {} {}",
                        spell(tl_x),
                        spell(tr_x),
                        spell(br_x),
                        spell(bl_x)
                    );
                    let y = format!(
                        "{} {} {} {}",
                        spell(tl_y),
                        spell(tr_y),
                        spell(br_y),
                        spell(bl_y)
                    );
                    if x == y {
                        // Circular corners: collapse like Chrome (a b / a b /
                        // a b c d → a / a b / a b c d).
                        let parts: Vec<&str> = x.split(' ').collect();
                        let [a, b, c, d] = [parts[0], parts[1], parts[2], parts[3]];
                        let s = if c == a && d == b {
                            if a == b { a.to_string() } else { format!("{a} {b}") }
                        } else {
                            format!("{a} {b} {c} {d}")
                        };
                        s
                    } else {
                        format!("{x} / {y}")
                    }
                }
                None => "0px".to_string(),
            }
        }),
        // Chrome's computed box-shadow: one entry per layer (first-declared
        // first), each "inset? color dx dy blur spread"; unset serializes
        // as "none".
        "box-shadow" => Some(match &s.box_shadow {
            None => "none".to_string(),
            Some(layers) => layers
                .iter()
                .map(|l| {
                    let c = color(&l.color);
                    let inset = if l.inset { "inset " } else { "" };
                    format!(
                        "{inset}{} {}px {}px {}px {}px",
                        c,
                        format_number(l.dx),
                        format_number(l.dy),
                        format_number(l.blur),
                        format_number(l.spread)
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
        }),
        // Chrome's computed backdrop-filter: "blur(<length>)" or "none".
        "backdrop-filter" => Some(match s.backdrop_blur {
            None => "none".to_string(),
            Some(px) => format!("blur({}px)", format_number(px)),
        }),
        // Chrome's computed text-shadow: "color dx dy blur" per layer
        // (first-declared first); unset serializes as "none".
        "text-shadow" => Some(match &s.text_shadow {
            None => "none".to_string(),
            Some(layers) => layers
                .iter()
                .map(|l| {
                    format!(
                        "{} {}px {}px {}px",
                        color(&l.color),
                        format_number(l.dx),
                        format_number(l.dy),
                        format_number(l.blur)
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
        }),
        "flex-direction" => Some(
            match s.flex_direction {
                Some(FlexDirection::RowReverse) => "row-reverse",
                Some(FlexDirection::Column) => "column",
                Some(FlexDirection::ColumnReverse) => "column-reverse",
                _ => "row",
            }
            .into(),
        ),
        "flex-wrap" => Some(
            match s.flex_wrap {
                Some(FlexWrapMode::Wrap) => "wrap",
                _ => "nowrap",
            }
            .into(),
        ),
        "justify-content" => Some(
            match s.justify_content {
                Some(JustifyMode::FlexStart) => "flex-start",
                Some(JustifyMode::Center) => "center",
                Some(JustifyMode::FlexEnd) => "flex-end",
                Some(JustifyMode::SpaceBetween) => "space-between",
                Some(JustifyMode::SpaceAround) => "space-around",
                Some(JustifyMode::SpaceEvenly) => "space-evenly",
                // Unset computes to `normal` in Chrome; only an author
                // declaration yields a keyword (obscura #771 wrong-value rows).
                None => "normal",
            }
            .into(),
        ),
        "align-items" => Some(
            match s.align_items {
                Some(AlignMode::Stretch) => "stretch",
                Some(AlignMode::FlexStart) => "flex-start",
                Some(AlignMode::Center) => "center",
                Some(AlignMode::FlexEnd) => "flex-end",
                None => "normal",
            }
            .into(),
        ),
        // Cross-axis line/track distribution (blitz#1059): same unset→
        // "normal" face the neighbors answer.
        "align-content" => Some(
            match s.align_content {
                Some(AlignContentMode::FlexStart) => "flex-start",
                Some(AlignContentMode::Center) => "center",
                Some(AlignContentMode::FlexEnd) => "flex-end",
                Some(AlignContentMode::SpaceBetween) => "space-between",
                Some(AlignContentMode::SpaceAround) => "space-around",
                Some(AlignContentMode::SpaceEvenly) => "space-evenly",
                Some(AlignContentMode::Stretch) => "stretch",
                None => "normal",
            }
            .into(),
        ),
        // Animation batch A: opacity is non-inherited with initial 1; Chrome
        // spells the computed number bare ("1", "0.5781"). Undeclared still
        // answers here — "1" IS the initial value, so no caller chain can
        // know better.
        "opacity" => Some(format_number(s.opacity.unwrap_or(1.0))),
        // #120 zoom: Chrome reports the COMPUTED factor — inherited, so a
        // zoomed wrapper's descendants all answer the ancestor's number.
        // Always a bare number (initial 1); the geometry scaling itself
        // already lives in the px values this table reports.
        "zoom" => Some(format_number(s.zoom.unwrap_or(1.0))),
        // Animation batch B / affine batch: the Transform2D serializes as
        // its full CSS matrix (a b c d tx ty) — rotate/skew/matrix join the
        // same composition now. No transform (or `none`) computes to "none";
        // a percentage translate resolves against the element's own border
        // box — a used value this snapshot layer has no box for — so those
        // stay absent and the JS inline chain answers instead of a wrong
        // matrix (GSAP writes inline, so tween state always reflects).
        "transform" => match &s.transform {
            None => Some("none".into()),
            Some(t) => match (t.tx, t.ty) {
                (Length::Px(tx), Length::Px(ty)) => Some(format!(
                    "matrix({}, {}, {}, {}, {}, {})",
                    format_number(t.a),
                    format_number(t.b),
                    format_number(t.c),
                    format_number(t.d),
                    format_number(tx),
                    format_number(ty)
                )),
                _ => None,
            },
        },
        // CSS Transforms L2 individual properties (#140) — reflected as
        // declared, exactly like Chrome (the computed `transform` matrix
        // above does NOT fold them in; they compose at use time only).
        "translate" => match s.translate_prop {
            None => Some("none".into()),
            Some((Length::Px(x), Length::Px(0.0))) => Some(format!("{}px", format_number(x))),
            Some((Length::Px(x), Length::Px(y))) => {
                Some(format!("{}px {}px", format_number(x), format_number(y)))
            }
            _ => None, // percent slots: no border box at this snapshot layer
        },
        "rotate" => match s.rotate_prop {
            None => Some("none".into()),
            Some(deg) => Some(format!("{}deg", format_number(deg))),
        },
        "scale" => match s.scale_prop {
            None => Some("none".into()),
            Some((x, y)) if (x - y).abs() < f32::EPSILON => Some(format_number(x)),
            Some((x, y)) => Some(format!("{} {}", format_number(x), format_number(y))),
        },
        // CSS animation longhands resolved from the shorthand's AnimationSpec
        // (the engine stores only the shorthand — parse_animation_shorthand).
        // Chrome spells durations in bare seconds ("0.1s"); direction and
        // play-state are unmodeled in the sampler, so they report their
        // initials ("normal", "running") like every other unmodeled longhand.
        "animation-name" => Some(
            s.animation
                .as_ref()
                .map(|a| a.name.clone())
                .unwrap_or_else(|| "none".into()),
        ),
        "animation-duration" => Some(format!(
            "{}s",
            format_number(s.animation.as_ref().map(|a| a.duration).unwrap_or(0.0))
        )),
        "animation-delay" => Some(format!(
            "{}s",
            format_number(s.animation.as_ref().map(|a| a.delay).unwrap_or(0.0))
        )),
        "animation-timing-function" => Some(easing_cssom(
            s.animation
                .as_ref()
                .map(|a| a.easing)
                .unwrap_or(Easing::CubicBezier(0.25, 0.1, 0.25, 1.0)),
        )),
        "animation-fill-mode" => Some(
            match s.animation.as_ref().map(|a| a.fill_forwards) {
                Some(true) => "forwards",
                _ => "none",
            }
            .into(),
        ),
        "animation-iteration-count" => Some(match s.animation.as_ref().map(|a| a.iterations) {
            Some(v) if v.is_infinite() => "infinite".into(),
            Some(v) => format_number(v),
            None => "1".into(),
        }),
        "animation-direction" => Some("normal".into()),
        "animation-play-state" => Some("running".into()),
        // CSS transition longhands from the shorthand's TransitionSpec. Chrome
        // spells an unset transition-property "all" (the initial), and the
        // spec table's "none" keyword only appears when declared.
        "transition-property" => Some(
            s.transition
                .as_ref()
                .and_then(|t| t.property.clone())
                .unwrap_or_else(|| "all".into()),
        ),
        "transition-duration" => Some(format!(
            "{}s",
            format_number(s.transition.as_ref().map(|t| t.duration).unwrap_or(0.0))
        )),
        "transition-delay" => Some(format!(
            "{}s",
            format_number(s.transition.as_ref().map(|t| t.delay).unwrap_or(0.0))
        )),
        "transition-timing-function" => Some(easing_cssom(
            s.transition
                .as_ref()
                .map(|t| t.easing)
                .unwrap_or(Easing::CubicBezier(0.25, 0.1, 0.25, 1.0)),
        )),
        _ => None,
    }
}

/// Minimal float spelling: integral values print without a fraction (Chrome
/// reports `24px`, not `24.00px`).
#[cfg(feature = "screenshot")]
pub(super) fn format_number(v: f32) -> String {
    if (v - v.round()).abs() < f32::EPSILON {
        format!("{}", v as i64)
    } else {
        format!("{}", v)
    }
}

/// One side of a padding/margin shorthand in Chrome's computed spelling.
/// Unset sides are `0px` (the CSS initial for both properties).
#[cfg(feature = "screenshot")]
/// #242: used-value spelling for one padding/margin side. Chrome answers
/// used px from getComputedStyle once layout has run: a percentage side
/// resolves against the containing-block inline size, calc folds its two
/// halves. Without a containing block (boxless elements, pseudo cascades
/// on a boxless host) the declared spelling stays.
pub(super) fn side_css_used(l: &Option<crate::diting_css::Length>, cb_inline: Option<f32>) -> String {
    use crate::diting_css::Length;
    match (l, cb_inline) {
        (Some(Length::Percent(p)), Some(cb)) => format!("{}px", format_number(p * cb / 100.0)),
        (Some(Length::Calc { percent, px }), Some(cb)) => {
            format!("{}px", format_number(percent * cb / 100.0 + px))
        }
        _ => side_css(l),
    }
}

/// The initial containing block's inline size: html spans the viewport, so
/// take the topmost ancestor that laid out (skips #document, which has no
/// rect).
#[cfg(feature = "screenshot")]
pub(super) fn icb_inline(
    dom: &DomTree,
    rects: &std::collections::HashMap<NodeId, [f32; 4]>,
    nid: NodeId,
) -> Option<f32> {
    let mut cur = dom.with_node(nid, |n| n.parent).flatten();
    let mut html = None;
    while let Some(p) = cur {
        if rects.contains_key(&p) {
            html = Some(p);
        }
        cur = dom.with_node(p, |n| n.parent).flatten();
    }
    Some(rects.get(&html?)?[2])
}

/// #242: the inline size of `nid`'s containing block, per CSS box rules:
/// in-flow boxes (static/relative/sticky) measure the DOM parent's content
/// box; absolutely positioned boxes measure the nearest positioned
/// ancestor's padding box; fixed boxes measure the initial containing
/// block (the html element's border box tracks the viewport). Ancestor
/// percentage insets resolve recursively (depth-capped; keyword/em insets
/// count 0 — em would need font metrics). `None` when geometry is missing
/// or the chain runs out — callers keep the declared spelling then.
#[cfg(feature = "screenshot")]
pub(super) fn cb_inline_for(
    dom: &DomTree,
    rects: &std::collections::HashMap<NodeId, [f32; 4]>,
    styles: &std::collections::HashMap<NodeId, crate::diting_css::ComputedStyle>,
    nid: NodeId,
    depth: u8,
) -> Option<f32> {
    use crate::diting_css::PositionMode;
    if depth == 0 {
        return None;
    }
    let own = styles.get(&nid)?;
    match own.position {
        Some(PositionMode::Fixed) => icb_inline(dom, rects, nid),
        Some(PositionMode::Absolute) => {
            let mut cur = dom.with_node(nid, |n| n.parent).flatten();
            while let Some(p) = cur {
                if let Some(st) = styles.get(&p) {
                    if matches!(
                        st.position,
                        Some(PositionMode::Relative)
                            | Some(PositionMode::Absolute)
                            | Some(PositionMode::Fixed)
                            | Some(PositionMode::Sticky)
                    ) {
                        // Padding box: subtract the border, keep the padding.
                        let w = rects.get(&p)?[2];
                        let border_lr = if st.border_style.is_some() {
                            used_px(&st.border_width.left) + used_px(&st.border_width.right)
                        } else {
                            0.0
                        };
                        return Some(w - border_lr);
                    }
                }
                cur = dom.with_node(p, |n| n.parent).flatten();
            }
            // No positioned ancestor: the ICB, same as fixed.
            icb_inline(dom, rects, nid)
        }
        _ => {
            let p = dom.with_node(nid, |n| n.parent).flatten()?;
            content_inline_for(dom, rects, styles, p, depth)
        }
    }
}

/// The content-box inline size of `nid` (border box minus border minus
/// padding, percentage insets resolved against nid's own containing block).
#[cfg(feature = "screenshot")]
pub(super) fn content_inline_for(
    dom: &DomTree,
    rects: &std::collections::HashMap<NodeId, [f32; 4]>,
    styles: &std::collections::HashMap<NodeId, crate::diting_css::ComputedStyle>,
    nid: NodeId,
    depth: u8,
) -> Option<f32> {
    let st = styles.get(&nid)?;
    let w = rects.get(&nid)?[2];
    let border_lr = if st.border_style.is_some() {
        used_px(&st.border_width.left) + used_px(&st.border_width.right)
    } else {
        0.0
    };
    // nid's own percentage padding resolves against ITS containing block;
    // at the top of the tree that CB is the ICB ≈ the element's own width
    // (html spans the viewport), so the recursion's None collapses to w.
    let cb = cb_inline_for(dom, rects, styles, nid, depth - 1).unwrap_or(w);
    let side = |l: &Option<crate::diting_css::Length>| match l {
        Some(crate::diting_css::Length::Px(v)) => *v,
        Some(crate::diting_css::Length::Percent(p)) => p * cb / 100.0,
        Some(crate::diting_css::Length::Calc { percent, px }) => percent * cb / 100.0 + px,
        _ => 0.0,
    };
    Some(w - border_lr - side(&st.padding.left) - side(&st.padding.right))
}

/// Px value of a border side (borders cannot be percentages; keywords are
/// width keywords that never reach a border arm — 0 for both).
// #244 helpers sit behind the same screenshot gate as their only caller
// (`computed_style_value`) — ungated they break the bare `cargo build`.
#[cfg(feature = "screenshot")]
pub(super) fn used_px(l: &Option<crate::diting_css::Length>) -> f32 {
    match l {
        Some(crate::diting_css::Length::Px(v)) => *v,
        _ => 0.0,
    }
}

#[cfg(feature = "screenshot")]
pub(super) fn side_css(l: &Option<crate::diting_css::Length>) -> String {
    use crate::diting_css::Length;
    match l {
        None => "0px".to_string(),
        Some(Length::Px(v)) => format!("{}px", format_number(*v)),
        Some(Length::Percent(p)) => format!("{}%", format_number(*p)),
        Some(Length::Calc { percent, px }) => {
            format!("calc({}% + {}px)", format_number(*percent), format_number(*px))
        }
        // `auto` is margin-only (the centering idiom); padding rejects it
        // at parse time so it can't reach this arm from there. min/max/
        // fit-content are width keywords and likewise never reach a
        // padding/margin side — the fallback just keeps the arm total.
        Some(Length::Auto) => "auto".to_string(),
        Some(Length::MinContent | Length::MaxContent | Length::FitContent) => "0px".to_string(),
    }
}
