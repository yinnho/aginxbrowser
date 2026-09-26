// Colocated contract suite — split out of the god file (ratchet).
use super::*;

// ---- @container parsing (moli#282) ----

#[test]
fn container_rules_parse_out_of_the_plain_pool() {
    let (plain, _kf, containers) = parse_stylesheet_full(
        ".a { color: red } \
         @container (min-width: 200px) { .b { color: blue } } \
         @container side (width >= 300px) { .c { color: green } } \
         @media (min-width: 1px) { @container (max-width: 50px) { .e { color: pink } } }",
        (1000.0, 800.0),
        CssMediaType::Screen,
        &MediaOverrides::default(),
    );
    assert_eq!(plain.len(), 1, "only the base rule stays in the plain pool");
    assert_eq!(plain[0].selector, ".a");
    assert_eq!(containers.len(), 3, "@container nested in a PASSING @media still lands");
    let [unnamed, named, in_media] = &containers[..] else { panic!() };

    assert_eq!(unnamed.name, None);
    let cond = unnamed.condition.as_ref().unwrap();
    assert_eq!(cond.0.len(), 1);
    assert_eq!(cond.0[0].cmp, ContainerCmp::Ge);
    assert_eq!(cond.0[0].axis, ContainerAxis::Width);
    assert_eq!(cond.0[0].px, 200.0);
    assert_eq!(unnamed.rules.len(), 1);
    assert_eq!(unnamed.rules[0].selector, ".b");

    assert_eq!(named.name.as_deref(), Some("side"));
    let cond = named.condition.as_ref().unwrap();
    assert_eq!(cond.0[0].cmp, ContainerCmp::Ge, "reversed operands flip the comparison");
    assert_eq!(cond.0[0].px, 300.0);

    let cond = in_media.condition.as_ref().unwrap();
    assert_eq!(cond.0[0].cmp, ContainerCmp::Le);
    assert_eq!(cond.0[0].px, 50.0);
}

#[test]
fn container_unparseable_condition_drops_the_whole_rule() {
    // v1: size-range triples and style queries do not parse — the arm
    // must NEVER match, so the rule leaves no trace (an "always match"
    // fallback would style things the author never asked for).
    let (plain, _kf, containers) = parse_stylesheet_full(
        "@container (400px <= width <= 600px) { .d { color: black } } \
         @container style(--accent: yes) { .f { color: white } } \
         @container (width: 123px) { .g { color: gray } }",
        (1000.0, 800.0),
        CssMediaType::Screen,
        &MediaOverrides::default(),
    );
    assert!(plain.is_empty());
    assert_eq!(containers.len(), 1, "triple and style query drop; the colon form survives");
    let cond = containers[0].condition.as_ref().unwrap();
    assert_eq!(cond.0[0].cmp, ContainerCmp::Eq);
    assert_eq!(cond.0[0].px, 123.0);
}

#[test]
fn container_nested_and_composes_conditions() {
    let (_plain, _kf, containers) = parse_stylesheet_full(
        "@container (min-width: 100px) { \
           @container (max-width: 400px) { .h { color: red } } \
         }",
        (1000.0, 800.0),
        CssMediaType::Screen,
        &MediaOverrides::default(),
    );
    assert_eq!(containers.len(), 1, "the nested spelling flattens to one rule");
    let cond = containers[0].condition.as_ref().unwrap();
    assert_eq!(cond.0.len(), 2, "outer and inner conditions AND-compose");
    assert!(cond.matches(250.0, 600.0));
    assert!(!cond.matches(50.0, 600.0), "below the outer min");
    assert!(!cond.matches(500.0, 600.0), "above the inner max");
}

#[test]
fn container_condition_matches_and_semantics() {
    let cond = parse_container_condition("(min-width: 100px) and (max-height: 50px)").unwrap();
    assert_eq!(cond.0.len(), 2);
    assert!(cond.matches(100.0, 50.0), "boundaries are inclusive");
    assert!(!cond.matches(99.0, 50.0));
    assert!(!cond.matches(100.0, 51.0));
    let eq = parse_container_condition("(width: 200px)").unwrap();
    assert!(eq.matches(200.0, 999.0));
    assert!(!eq.matches(201.0, 999.0));
    // A bare `(width)` query or an empty condition is unparseable here.
    assert!(parse_container_condition("(width)").is_none());
    assert!(parse_container_condition("").is_none());
}

// ---- stylesheet parsing ----

#[test]
fn grid_areas_minmax_and_shorthand_parse() {
    // Vector 2022's scaffold: minmax + rem tracks, the rows/columns
    // shorthand, the areas matrix, and named-area item placement.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-template-columns: 11.5rem minmax(0, 60rem) 16rem");
    assert_eq!(
        s.grid_template_columns,
        Some(vec![
            GridTrack::Px(184.0),
            GridTrack::MinMax { min: TrackSize::Px(0.0), max: TrackSize::Px(960.0) },
            GridTrack::Px(256.0),
        ]),
    );

    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-template: min-content 1fr min-content / auto minmax(0, 1fr)");
    assert_eq!(s.grid_template_rows, Some(vec![GridTrack::Auto, GridTrack::Fr(1.0), GridTrack::Auto]));
    assert_eq!(
        s.grid_template_columns,
        Some(vec![GridTrack::Auto, GridTrack::MinMax { min: TrackSize::Px(0.0), max: TrackSize::Fr(1.0) }]),
    );

    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-template-areas: 'siteNotice siteNotice' 'columnStart pageContent' 'footer footer'");
    assert_eq!(
        s.grid_template_areas,
        Some(vec![
            vec!["siteNotice".into(), "siteNotice".into()],
            vec!["columnStart".into(), "pageContent".into()],
            vec!["footer".into(), "footer".into()],
        ]),
    );
    // Non-rectangular matrices are invalid → the whole declaration drops.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-template-areas: 'a b' 'a'");
    assert_eq!(s.grid_template_areas, None);

    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-area: pageContent");
    assert_eq!(s.grid_area, Some("pageContent".into()));
    // The 4-value numeric form is not this batch; it must not half-parse.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-area: 2 / 1 / 3 / 2");
    assert_eq!(s.grid_area, None);
}

#[test]
fn grid_repeat_expands_inline() {
    // The landing-page idiom: repeat(4, 1fr). Before this batch the
    // token failed to parse, the whole declaration dropped, and the
    // grid collapsed to a single auto column.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-template-columns: repeat(4, 1fr)");
    assert_eq!(
        s.grid_template_columns,
        Some(vec![GridTrack::Fr(1.0); 4]),
    );

    // Multi-track lists and minmax nested inside repeat.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-template-columns: 100px repeat(2, minmax(0, 1fr) 2fr) auto");
    assert_eq!(
        s.grid_template_columns,
        Some(vec![
            GridTrack::Px(100.0),
            GridTrack::MinMax { min: TrackSize::Px(0.0), max: TrackSize::Fr(1.0) },
            GridTrack::Fr(2.0),
            GridTrack::MinMax { min: TrackSize::Px(0.0), max: TrackSize::Fr(1.0) },
            GridTrack::Fr(2.0),
            GridTrack::Auto,
        ]),
    );

    // auto-fill needs container geometry the cascade doesn't have:
    // reject the declaration rather than half-parse.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-template-columns: repeat(auto-fill, 100px)");
    assert_eq!(s.grid_template_columns, None);
}

#[test]
fn grid_percent_tracks_parse() {
    // The dashboard idiom: `grid-template-columns: 25% 1fr`. Before this
    // batch the % token was rejected, the whole declaration dropped, and
    // the grid collapsed to one auto column.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-template-columns: 25% 1fr");
    assert_eq!(
        s.grid_template_columns,
        Some(vec![GridTrack::Percent(25.0), GridTrack::Fr(1.0)]),
    );

    // Percent rides through repeat and minmax like any other sizing.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-template-columns: repeat(2, 50%)");
    assert_eq!(s.grid_template_columns, Some(vec![GridTrack::Percent(50.0); 2]));

    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-template-columns: minmax(10%, 1fr) 200px");
    assert_eq!(
        s.grid_template_columns,
        Some(vec![
            GridTrack::MinMax { min: TrackSize::Percent(10.0), max: TrackSize::Fr(1.0) },
            GridTrack::Px(200.0),
        ]),
    );

    // A malformed token still aborts the whole declaration — no half-parse.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "grid-template-columns: 25% nonsense");
    assert_eq!(s.grid_template_columns, None);
}


#[test]
fn display_inline_block_parses_and_serializes() {
    let mut s = ComputedStyle::default();
    assert!(
        apply_declarations(&mut s, "display: inline-block"),
        "inline-block must not be dropped by the cascade"
    );
    assert_eq!(s.display, Some(Display::InlineBlock));
    // @supports already claimed inline-block; now the claim is truthful.
    assert!(supports_declaration("display", "inline-block"));
}

/// #107: Fusion's .next-input is `display: inline-flex` and sizes its bare
/// input children purely from flex layout. The declaration used to be
/// dropped wholesale, zeroing 32 inputs on the Tmall publish page.
#[test]
fn display_inline_flex_and_grid_parse() {
    let mut s = ComputedStyle::default();
    assert!(
        apply_declarations(&mut s, "display: inline-flex"),
        "inline-flex must not be dropped by the cascade (#107)"
    );
    assert_eq!(s.display, Some(Display::Flex));
    assert!(
        apply_declarations(&mut s, "display: inline-grid"),
        "inline-grid must not be dropped by the cascade (#107)"
    );
    assert_eq!(s.display, Some(Display::Grid));
}

/// inline-table is the value Fusion's `.next-input` actually declares
/// (`display: inline-table; width: 200px`): it used to hit the drop arm, the
/// span collapsed to plain inline, and `.next-input input { width: 100% }`
/// resolved against nothing — 32 zero-width inputs on the Tmall publish
/// page. It maps onto the table container mode like inline-flex rides Flex.
#[test]
fn display_inline_table_parses() {
    let mut s = ComputedStyle::default();
    assert!(
        apply_declarations(&mut s, "display: inline-table"),
        "inline-table must not be dropped by the cascade (tmall .next-input)"
    );
    assert_eq!(s.display, Some(Display::Table));
    // The probe table must claim exactly what apply_one accepts — and must
    // NOT claim `contents`, which has no box-skipping implementation
    // behind it (claiming support while dropping the declaration is the
    // worse lie for an @supports feature branch).
    assert!(supports_declaration("display", "inline-table"));
    assert!(supports_declaration("display", "inline-flex"));
    assert!(supports_declaration("display", "table-cell"));
    assert!(!supports_declaration("display", "contents"));
    assert!(!supports_declaration("display", "inline-tablee"));
}

// ---- animation batch A/B: opacity + transform ----

/// The exact inline strings GSAP's tween engine writes on a diting-driven
/// page (spike-captured ground truth): from() initial, mid-tween, and the
/// completion write. All three must land in Transform2D or the frame
/// snaps to end-state mid-animation (the 300-identical-frames bug).
#[test]
fn parse_transform_gsap_ground_truth() {
    let t = parse_transform("translate3d(-185.3719px, 0px, 0px)").unwrap();
    assert_eq!(
        t,
        Transform2D {
            a: 1.0, b: 0.0, c: 0.0, d: 1.0,
            tx: Length::Px(-185.3719),
            ty: Length::Px(0.0),
        }
    );
    let t = parse_transform("translate(-200px, 0px)").unwrap();
    assert_eq!(t.tx, Length::Px(-200.0));
    // Completion write uses bare 0 lengths.
    let t = parse_transform("translate(0, 0)").unwrap();
    assert_eq!((t.tx, t.ty), (Length::Px(0.0), Length::Px(0.0)));
    // translate3d's z must parse (finite) even though 3D flattens away.
    assert!(parse_transform("translate3d(10px, 20px, 30px)").is_some());
    assert!(parse_transform("translate3d(10px, 20px, bad)").is_none());
}

/// Scale family plus the composition-order contract: walking the list
/// left-to-right, each new function is innermost, so a translate added
/// AFTER a scale composes scaled — `scale(2) translate(100px)` maps
/// p → p·2 + 200, while `translate(100px) scale(2)` maps p → p·2 + 100.
#[test]
fn parse_transform_scales_and_composition_order() {
    let t = parse_transform("scale(0.5)").unwrap();
    assert_eq!((t.a, t.d), (0.5, 0.5));
    let t = parse_transform("scale(2, 3)").unwrap();
    assert_eq!((t.a, t.d), (2.0, 3.0));
    let t = parse_transform("scaleX(2)").unwrap();
    assert_eq!((t.a, t.d), (2.0, 1.0));
    let t = parse_transform("scaleY(4)").unwrap();
    assert_eq!((t.a, t.d), (1.0, 4.0));

    let t = parse_transform("scale(2) translate(100px)").unwrap();
    assert_eq!(t.tx, Length::Px(200.0));
    assert_eq!(t.a, 2.0);
    let t = parse_transform("translate(100px) scale(2)").unwrap();
    assert_eq!(t.tx, Length::Px(100.0));
    assert_eq!(t.a, 2.0);

    // Scales multiply through a percent translate: w·0.5·2 == Percent(50·2).
    let t = parse_transform("scale(2) translate(50%)").unwrap();
    assert_eq!(t.tx, Length::Percent(100.0));

    let t = parse_transform("translateX(12px) translateY(34px)").unwrap();
    assert_eq!((t.tx, t.ty), (Length::Px(12.0), Length::Px(34.0)));
}

/// Spec rule: one unknown function invalidates the whole declaration —
/// functions this engine has no axis for (3D), bad arity, and unitless
/// nonzero angles yield None (the element renders untransformed), as do
/// `none` (the initial value) and garbage.
#[test]
fn parse_transform_rejects_unknown_functions() {
    assert!(parse_transform("translateZ(10px)").is_none());
    assert!(parse_transform("rotate3d(1, 1, 1, 45deg)").is_none());
    assert!(parse_transform("matrix(1, 2)").is_none());
    assert!(parse_transform("rotate(45)").is_none(), "unitless nonzero angle is invalid CSS");
    assert!(parse_transform("scale(banana)").is_none());
    assert!(parse_transform("none").is_none());
    assert!(parse_transform("").is_none());
    assert!(parse_transform("translate(50%, 10%)").is_some());
    assert!(parse_transform("translate(100px) rotate(10deg)").is_some(), "rotate joins the composition now");
}

/// rotate/skew/matrix vectors, checked against the CSS matrix
/// convention x' = a·x + c·y + e, y' = b·x + d·y + f: rotate(θ) is
/// (cos, sin, −sin, cos), skewX bends y into x (c = tan), and a
/// translate AFTER a rotation composes rotated (the vector maps
/// through the accumulated linear part).
#[test]
fn parse_transform_rotate_skew_matrix_ground_truth() {
    let close = |t: &Transform2D, want: (f32, f32, f32, f32)| {
        let got = (t.a, t.b, t.c, t.d);
        let ok = [got.0, got.1, got.2, got.3]
            .into_iter()
            .zip([want.0, want.1, want.2, want.3])
            .all(|(g, w)| (g - w).abs() < 1e-5);
        assert!(ok, "got {got:?} want {want:?}");
    };

    let t = parse_transform("rotate(90deg)").unwrap();
    close(&t, (0.0, 1.0, -1.0, 0.0));
    assert!(!t.is_axis_aligned());
    let t = parse_transform("rotate(45deg)").unwrap();
    close(&t, (
        std::f32::consts::FRAC_1_SQRT_2,
        std::f32::consts::FRAC_1_SQRT_2,
        -std::f32::consts::FRAC_1_SQRT_2,
        std::f32::consts::FRAC_1_SQRT_2,
    ));
    // Angle units: 0.25 turn and ~π/2 rad are both 90°.
    let t = parse_transform("rotate(0.25turn)").unwrap();
    close(&t, (0.0, 1.0, -1.0, 0.0));
    let t = parse_transform("rotate(1.5707963rad)").unwrap();
    close(&t, (0.0, 1.0, -1.0, 0.0));

    let t = parse_transform("skewX(45deg)").unwrap();
    close(&t, (1.0, 0.0, 1.0, 1.0));
    let t = parse_transform("skewY(45deg)").unwrap();
    close(&t, (1.0, 1.0, 0.0, 1.0));

    let t = parse_transform("matrix(1, 0, 0, 1, 10, 20)").unwrap();
    close(&t, (1.0, 0.0, 0.0, 1.0));
    assert_eq!((t.tx, t.ty), (Length::Px(10.0), Length::Px(20.0)));
    // e/f are numbers that map through the OLD linear: matrix(2,0,0,2,10,0)
    // keeps tx=10 (Chrome maps the origin to (10,0), not (20,0)).
    let t = parse_transform("matrix(2, 0, 0, 2, 10, 0)").unwrap();
    assert_eq!(t.tx, Length::Px(10.0));
    close(&t, (2.0, 0.0, 0.0, 2.0));

    // A translate AFTER a rotation composes rotated; BEFORE it, the
    // slots ride untouched (rotate never touches the translate). The
    // rotated tx carries cos(90°) f32 noise (≈ −4.4e-7), which is the
    // same "0" every downstream rasterizer sees — snap it in the assert.
    let t = parse_transform("rotate(90deg) translate(10px)").unwrap();
    let (Length::Px(tx), Length::Px(ty)) = (t.tx, t.ty) else {
        panic!("rotate+translate must stay px-resolved");
    };
    assert!((tx - 0.0).abs() < 1e-5, "cos(90°) noise collapses to 0: {tx}");
    assert_eq!(ty, 10.0);
    let t = parse_transform("translate(10px) rotate(90deg)").unwrap();
    assert_eq!((t.tx, t.ty), (Length::Px(10.0), Length::Px(0.0)));

    // Percent translate survives only while the accumulated linear is
    // diagonal: an outermost percent is fine, a percent under rotation
    // can't compose (the vector map mixes axes) and dies with the list.
    let t = parse_transform("translate(50%) rotate(10deg)").unwrap();
    assert_eq!(t.tx, Length::Percent(50.0));
    assert!(parse_transform("rotate(10deg) translate(50%)").is_none());

    // The diagonal cases keep the pre-baked geometry path.
    assert!(parse_transform("translate(10px, 20px)").unwrap().is_axis_aligned());
    assert!(parse_transform("scale(2)").unwrap().is_axis_aligned());
}

/// opacity parses through the same declaration pipeline the cascade uses,
/// clamps into [0,1], and rejects non-finite/out-of-range values.
#[test]
fn opacity_parses_clamps_and_validates() {
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "opacity: 0.5781"));
    assert_eq!(s.opacity, Some(0.5781));
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "opacity: 0"));
    assert_eq!(s.opacity, Some(0.0));
    // Out-of-range clamps toward the nearest bound (Chrome clamps used
    // values; parse keeps the clamp so paint never sees alpha > 1).
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "opacity: 2"));
    assert_eq!(s.opacity, Some(1.0));
    assert!(!supports_declaration("opacity", "1.5x"));
    assert!(supports_declaration("opacity", "0.25"));
    // Undeclared stays None — the caller's default chain answers "1".
    assert_eq!(ComputedStyle::default().opacity, None);
}

#[test]
fn vector_2022_minified_media_rule_reaches_computed_style() {
    // Byte-for-byte excerpt of the live load.php payload (minified, no
    // spaces after colons, the grid rule media-gated at 1120px). The
    // scaffold only applies on wide viewports — narrow ones must not
    // see it either.
    let css = concat!(
        "@media screen and (min-width:1680px){.mw-page-container{padding-left:3.25rem}}",
        "@media screen and (min-width:1120px){.mw-page-container-inner{",
        "display:grid;column-gap:24px;",
        "grid-template:min-content 1fr min-content / 12.25rem minmax(0,1fr);",
        "grid-template-areas:'siteNotice siteNotice' 'columnStart pageContent' 'footer footer'}}",
        ".mw-body .vector-page-titlebar{grid-area:titlebar}",
    );
    let rules = parse_stylesheet_for(css, (1440.0, 900.0), CssMediaType::Screen);
    let inner = rules
        .iter()
        .find(|r| r.selector == ".mw-page-container-inner")
        .expect("media-gated grid rule must survive parsing at 1440px");
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, &inner.declarations);
    assert_eq!(
        s.grid_template_areas,
        Some(vec![
            vec!["siteNotice".into(), "siteNotice".into()],
            vec!["columnStart".into(), "pageContent".into()],
            vec!["footer".into(), "footer".into()],
        ]),
    );
    assert_eq!(
        s.grid_template_columns,
        Some(vec![
            GridTrack::Px(196.0),
            GridTrack::MinMax { min: TrackSize::Px(0.0), max: TrackSize::Fr(1.0) },
        ]),
    );
    let titlebar = rules.iter().find(|r| r.selector == ".mw-body .vector-page-titlebar").unwrap();
    let mut t = ComputedStyle::default();
    apply_declarations(&mut t, &titlebar.declarations);
    assert_eq!(t.grid_area, Some("titlebar".into()));

    // Same sheet below the breakpoint: the scaffold stays out.
    let narrow = parse_stylesheet_for(css, (375.0, 700.0), CssMediaType::Screen);
    assert!(!narrow.iter().any(|r| r.selector == ".mw-page-container-inner"));
}

#[test]
fn float_and_clear_parse_into_style() {
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "float: left");
    assert_eq!(s.float_side, Some(FloatSide::Left));
    assert_eq!(s.clear_side, None);

    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "float: right; clear: both");
    assert_eq!(s.float_side, Some(FloatSide::Right));
    assert_eq!(s.clear_side, Some(ClearSide::Both));

    // Explicit initial values compute to None.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "float: none; clear: none");
    assert_eq!(s.float_side, None);
    assert_eq!(s.clear_side, None);

    // Logical keywords (CSS Logical Properties) parse; layout resolves
    // them against LTR (inline-start = left).
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "clear: inline-start");
    assert_eq!(s.clear_side, Some(ClearSide::InlineStart));
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "clear: inline-end");
    assert_eq!(s.clear_side, Some(ClearSide::InlineEnd));

    // Invalid values drop the declaration (style stays default).
    let mut s = ComputedStyle::default();
    assert!(!apply_declarations(&mut s, "float: top"));
    assert!(!apply_declarations(&mut s, "clear: all"));
    assert_eq!(s, ComputedStyle::default());
}

#[test]
fn float_clear_probe_matches_grammar() {
    assert!(supports_declaration("float", "left"));
    assert!(supports_declaration("float", "right"));
    assert!(supports_declaration("float", "none"));
    assert!(!supports_declaration("float", "top"));
    assert!(supports_declaration("clear", "both"));
    assert!(supports_declaration("clear", "inline-start"), "logical keyword (LTR-resolved)");
    assert!(supports_declaration("clear", "inline-end"));
    // `0` passes the probe's shared unitless-zero gate (the same wildcard
    // every keyword property gets); the value parser itself declines it.
}

#[test]
fn parse_color_rgb_function_forms() {
    assert_eq!(parse_color("rgb(198, 40, 40)"), Some(Color(198, 40, 40, 255)));
    assert_eq!(parse_color("rgb(198 40 40)"), Some(Color(198, 40, 40, 255)), "space-separated");
    assert_eq!(parse_color("rgba(198,40,40,0.5)"), Some(Color(198, 40, 40, 128)));
    assert_eq!(parse_color("RGB(50%, 0%, 0%)"), Some(Color(128, 0, 0, 255)), "percent + case-insensitive");
    assert_eq!(parse_color("rgb(1,2)"), None, "wrong arity");
}

#[test]
fn linear_gradient_135deg_two_stops() {
    let g = parse_linear_gradient("linear-gradient(135deg, #2c3e50 0%, #fd79a8 100%)").expect("parses");
    assert_eq!(g.css_deg, 135.0);
    assert_eq!(g.stops, vec![(0.0, Color(0x2c, 0x3e, 0x50, 255)), (1.0, Color(0xfd, 0x79, 0xa8, 255))]);
}

#[test]
fn linear_gradient_direction_keywords_and_default() {
    assert_eq!(parse_linear_gradient("linear-gradient(to right, red, blue)").unwrap().css_deg, 90.0);
    assert_eq!(parse_linear_gradient("linear-gradient(to top, red, blue)").unwrap().css_deg, 0.0);
    assert_eq!(parse_linear_gradient("linear-gradient(red, blue)").unwrap().css_deg, 180.0, "no spec = to bottom");
    assert!(parse_linear_gradient("linear-gradient(to top left, red, blue)").is_none(),
        "corner keywords stay unparsed (v1), callers keep their fallback");
}

#[test]
fn linear_gradient_missing_positions_distribute_evenly() {
    let g = parse_linear_gradient("linear-gradient(red, lime, blue)").expect("parses");
    assert_eq!(g.stops[0].0, 0.0);
    assert_eq!(g.stops[1].0, 0.5);
    assert_eq!(g.stops[2].0, 1.0);
}

#[test]
fn linear_gradient_rgb_function_stops() {
    let g = parse_linear_gradient("linear-gradient(90deg, rgb(93, 58, 176) 0%, rgba(213, 0, 114, 0.5) 100%)")
        .expect("parses");
    assert_eq!(g.stops[0].1, Color(93, 58, 176, 255));
    assert_eq!(g.stops[1].1, Color(213, 0, 114, 128));
}

#[test]
fn linear_gradient_out_of_order_stops_sort() {
    let g = parse_linear_gradient("linear-gradient(90deg, red 100%, blue 0%)").expect("parses");
    // Authored reversed; raster needs ascending, equal-position hard
    // lines survive as-is.
    assert!(g.stops[0].0 <= g.stops[1].0);
    assert_eq!(g.stops[0].1, Color(0, 0, 255, 255));
}

#[test]
fn non_gradients_rejected() {
    assert!(parse_linear_gradient("none").is_none());
    assert!(parse_linear_gradient("url(https://x/y.png)").is_none());
    assert!(parse_linear_gradient("radial-gradient(red, blue)").is_none());
    assert!(parse_linear_gradient("linear-gradient(90deg, red)").is_none());
}

/// The `background` shorthand is how real sheets declare gradients —
/// the longhand-only v1 left those declarations dropped on the floor.
#[test]
fn background_shorthand_expands_gradient_and_color() {
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "background: linear-gradient(135deg, red, blue)"));
    assert_eq!(
        s.background_image.as_deref(),
        Some("linear-gradient(135deg, red, blue)"),
        "the whole gradient function is one paren-aware token"
    );
    assert_eq!(s.background_color, None, "no color token in the shorthand");

    // Full layer: color + gradient + repeat, order-free.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "background: #fff linear-gradient(to right, red, blue) no-repeat"));
    assert_eq!(s.background_color, Some(Color(255, 255, 255, 255)));
    assert_eq!(s.background_image.as_deref(), Some("linear-gradient(to right, red, blue)"));

    // Shorthand RESETS the image longhand (CSS semantics): a plain
    // color later in the cascade clears an earlier gradient.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "background-image: linear-gradient(red, blue)");
    assert!(apply_declarations(&mut s, "background: #f0f0f0"));
    assert_eq!(s.background_image, None);
    assert_eq!(s.background_color, Some(Color(240, 240, 240, 255)));

    // ... and the color longhand: `background: none` after
    // `background-color: red` must land transparent (the classic
    // button/link reset pattern), while a later longhand still wins.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "background-color: red");
    assert!(apply_declarations(&mut s, "background: none"));
    assert_eq!(s.background_color, None, "shorthand resets color too");
    assert_eq!(s.background_image, None);

    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "background: none"));
    apply_declarations(&mut s, "background-color: red");
    assert_eq!(s.background_color, Some(Color(255, 0, 0, 255)));

    // Function colors stay inside their token; rgb() shorthand still
    // lands as a color (the pre-shorthand-rework behavior).
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "background: rgb(20, 60, 200)"));
    assert_eq!(s.background_color, Some(Color(20, 60, 200, 255)));
    assert_eq!(s.background_image, None);
}

#[test]
fn border_shorthand_and_longhands_parse() {
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "border: 6px solid rgb(20,60,200)");
    assert_eq!(s.border_width.top, Some(Length::Px(6.0)));
    assert_eq!(s.border_width.left, Some(Length::Px(6.0)));
    assert_eq!(s.border_style, Some(BorderStyle::Solid));
    assert_eq!(s.border_color, Some(Color(20, 60, 200, 255)));

    // Order-free shorthand + width keywords.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "border: red thin dashed");
    assert_eq!(s.border_width.left, Some(Length::Px(1.0)));
    assert_eq!(s.border_color, Some(Color(255, 0, 0, 255)));
    assert_eq!(s.border_style, Some(BorderStyle::Dashed));

    // Longhands, incl. the 2-value side expansion.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "border-width: 2px 4px; border-style: solid; border-color: blue");
    assert_eq!(s.border_width.top, Some(Length::Px(2.0)));
    assert_eq!(s.border_width.left, Some(Length::Px(4.0)));
    assert_eq!(s.border_style, Some(BorderStyle::Solid));
    assert_eq!(s.border_color, Some(Color(0, 0, 255, 255)));

    // `none` computes any widths away (CSS initial style).
    apply_declarations(&mut s, "border-style: none");
    assert_eq!(s.border_style, None);

    // Per-side width longhands narrow one side of a shorthand border
    // without touching the rest (blitz#837 repro shape: 0 kills the top
    // side only). Unitless zero is the canonical form there.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "border: 1px solid #ff0000");
    assert!(apply_declarations(&mut s, "border-top-width: 0"));
    assert_eq!(s.border_width.top, Some(Length::Px(0.0)));
    assert_eq!(s.border_width.right, Some(Length::Px(1.0)));
    assert_eq!(s.border_width.bottom, Some(Length::Px(1.0)));
    assert_eq!(s.border_width.left, Some(Length::Px(1.0)));
    assert!(apply_declarations(&mut s, "border-left-width: thick"));
    assert_eq!(s.border_width.left, Some(Length::Px(5.0)));
    assert!(!apply_declarations(&mut s, "border-right-width: bogus"));

    // Garbage token drops the whole shorthand declaration.
    let mut s = ComputedStyle::default();
    assert!(!apply_declarations(&mut s, "border: solid wat"));
    assert_eq!(s.border_style, None);
}

#[test]
fn parse_rules_comments_and_nested_braces() {
    let css = r#"/* header */
        a { color: red; }
        div { background: url("a{b}.png"); color: blue; }
    "#;
    let rules = parse_stylesheet(css);
    assert_eq!(rules.len(), 2, "{rules:?}");
    assert_eq!(rules[0].selector, "a");
    assert_eq!(rules[0].declarations, "color: red;");
    assert_eq!(rules[1].selector, "div");
    // Braces inside quoted strings must not confuse the block counter.
    assert!(rules[1].declarations.contains(r#"url("a{b}.png")"#), "{rules:?}");
}

#[test]
fn stray_close_brace_resyncs_parsing() {
    // Upstream: remoteok.com ships an unbalanced top-level `}` mid-sheet;
    // browsers recover and keep the rest of the sheet usable.
    let css = "a { color: red; } } p { color: blue; }";
    let rules = parse_stylesheet(css);
    assert_eq!(rules.len(), 2, "{rules:?}");
    assert_eq!(rules[1].selector, "p");
}

#[test]
fn media_queries_gate_inner_rules() {
    let css = r#"
        @media (min-width: 768px) { .desktop { display: flex; } }
        @media (max-width: 500px) { .mobile { display: none; } }
        @media screen and (min-width: 100px) { .both { color: red; } }
        @media print { .paper { color: blue; } }
    "#;

    let desktop = parse_stylesheet_for(css, (1280.0, 720.0), CssMediaType::Screen);
    let sels: Vec<&str> = desktop.iter().map(|r| r.selector.as_str()).collect();
    assert!(sels.contains(&".desktop"), "{sels:?}");
    assert!(sels.contains(&".both"), "{sels:?}");
    assert!(!sels.contains(&".mobile"), "{sels:?}");
    assert!(!sels.contains(&".paper"), "{sels:?}");

    let narrow = parse_stylesheet_for(css, (400.0, 720.0), CssMediaType::Screen);
    let sels: Vec<&str> = narrow.iter().map(|r| r.selector.as_str()).collect();
    assert!(sels.contains(&".mobile"), "{sels:?}");
    assert!(!sels.contains(&".desktop"), "{sels:?}");

    let print = parse_stylesheet_for(css, (1280.0, 720.0), CssMediaType::Print);
    let sels: Vec<&str> = print.iter().map(|r| r.selector.as_str()).collect();
    assert!(sels.contains(&".paper"), "{sels:?}");
    // A bare feature query implies media type `all`, so it applies under
    // print too — that's spec behavior, lock it as such.
    assert!(sels.contains(&".desktop"), "bare feature = all: {sels:?}");
    assert!(!sels.contains(&".both"), "`screen and …` must not apply in print: {sels:?}");
}

#[test]
fn media_list_is_or_and_function_commas_survive() {
    assert!(media_query_applies("not print, (min-width: 10px)", (1280.0, 720.0), CssMediaType::Screen));
    assert!(!media_query_applies("(min-width: 99999px)", (1280.0, 720.0), CssMediaType::Screen));
    // Comma inside a function is not a list separator.
    assert!(media_query_applies("(width: 1280px)", (1280.0, 720.0), CssMediaType::Screen));
}

/// Issue #29: `Emulation.setEmulatedMedia` features replace the
/// prefers-* truth tables wholesale — matching arms must start (and
/// stop) applying through the override path.
#[test]
fn emulated_media_features_flip_media_arms() {
    let css = r#"
        @media (prefers-color-scheme: dark) { .dark { color: blue; } }
        @media (prefers-reduced-motion: reduce) { .rm { display: none; } }
    "#;
    let base = parse_stylesheet_for(css, (1280.0, 720.0), CssMediaType::Screen);
    assert!(base.is_empty(), "no emulation → persona defaults: {base:?}");

    let emu = MediaOverrides {
        features: vec![
            ("prefers-color-scheme".into(), "dark".into()),
            ("prefers-reduced-motion".into(), "reduce".into()),
        ],
    };
    let rules = parse_stylesheet_timed_with(css, (1280.0, 720.0), CssMediaType::Screen, &emu).0;
    let sels: Vec<&str> = rules.iter().map(|r| r.selector.as_str()).collect();
    assert!(sels.contains(&".dark"), "{sels:?}");
    assert!(sels.contains(&".rm"), "{sels:?}");

    // An emulated value only satisfies the matching value, not any value.
    let dark = MediaOverrides { features: vec![("prefers-color-scheme".into(), "dark".into())] };
    let rules = parse_stylesheet_timed_with(
        "@media (prefers-color-scheme: light) { .light { color: red; } }",
        (1280.0, 720.0),
        CssMediaType::Screen,
        &dark,
    )
    .0;
    assert!(rules.is_empty(), "emulated dark ≠ light: {rules:?}");
}

/// Issue #29: emulating the media TYPE keeps `all` arms and drops
/// `screen` arms, while feature overrides still apply under print.
#[test]
fn emulated_print_media_type_and_all_token() {
    let css = r#"
        @media print { .paper { color: black; } }
        @media screen { .ui { color: blue; } }
        @media all { .every { color: green; } }
        @media (prefers-reduced-motion: no-preference) { .anim { color: red; } }
    "#;
    let emu = MediaOverrides { features: vec![("prefers-reduced-motion".into(), "reduce".into())] };
    let rules = parse_stylesheet_timed_with(css, (1280.0, 720.0), CssMediaType::Print, &emu).0;
    let sels: Vec<&str> = rules.iter().map(|r| r.selector.as_str()).collect();
    assert!(sels.contains(&".paper"), "{sels:?}");
    assert!(!sels.contains(&".ui"), "{sels:?}");
    assert!(sels.contains(&".every"), "`all` applies under print: {sels:?}");
    assert!(!sels.contains(&".anim"), "emulated reduce ≠ no-preference: {sels:?}");
}

#[test]
fn supports_conditions_evaluate() {
    let css = r#"
        @supports (display: grid) { .g { display: grid; } }
        @supports not (display: nonexistent-thing) { .n { color: red; } }
        @supports ((display: grid) and (display: flex)) { .af { color: blue; } }
        @supports (display: totally-bogus-value-is-still-a-declaration-probe-false) { .x { color: green; } }
    "#;
    let rules = parse_stylesheet(css);
    let sels: Vec<&str> = rules.iter().map(|r| r.selector.as_str()).collect();
    assert!(sels.contains(&".g"), "{sels:?}");
    assert!(sels.contains(&".n"), "{sels:?}");
    assert!(sels.contains(&".af"), "{sels:?}");
    assert!(!sels.contains(&".x"), "{sels:?}");
}

#[test]
fn layer_bodies_flatten_and_font_face_dropped() {
    let css = r#"
        @layer base { .in-layer { color: red; } }
        @font-face { font-family: X; src: url(x.ttf); }
        @import url(other.css);
        .top { color: blue; }
    "#;
    let rules = parse_stylesheet(css);
    let sels: Vec<&str> = rules.iter().map(|r| r.selector.as_str()).collect();
    assert!(sels.contains(&".in-layer"), "{sels:?}");
    assert!(sels.contains(&".top"), "{sels:?}");
    assert_eq!(rules.len(), 2, "@font-face/@import must drop: {sels:?}");
}

#[test]
fn keyframes_and_import_do_not_bleed_into_next_selector() {
    let css = "@keyframes spin { from { opacity: 0; } } .after { color: red; }";
    let rules = parse_stylesheet(css);
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0].selector, ".after");
}

#[test]
fn collect_selector_attr_names_extracts_bracket_idents() {
    let rules = vec![
        ParsedRule { selector: "[data-x]".into(), declarations: String::new() },
        ParsedRule { selector: ".a[aria-expanded=\"true\"]:hover".into(), declarations: String::new() },
        ParsedRule { selector: "[TabIndex]".into(), declarations: String::new() },
        ParsedRule { selector: "[lang|=en]".into(), declarations: String::new() },
        ParsedRule { selector: ".plain > #id".into(), declarations: String::new() },
    ];
    let names = collect_selector_attr_names(&rules);
    assert!(names.contains("data-x"), "{names:?}");
    assert!(names.contains("aria-expanded"), "{names:?}");
    assert!(names.contains("tabindex"), "lowercased: {names:?}");
    assert!(names.contains("lang"), "{names:?}");
    assert!(!names.contains("true"), "value is not a name: {names:?}");
    assert_eq!(names.len(), 4, "{names:?}");
    assert!(collect_selector_attr_names(&[]).is_empty());
}

// ---- declarations & computed style ----

#[test]
fn declaration_splitting_handles_quotes_and_nested_blocks() {
    let decls = r#"content: "a;b"; background: url(x(1;2).png); width: 4px; & :hover { color: red; }"#;
    let parsed = split_declarations(decls);
    let names: Vec<&str> = parsed.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["content", "background", "width"], "{parsed:?}");
    // The nested block is one dropped chunk (unparseable as declarations).
}

#[test]
fn shorthand_expansion_four_value_forms() {
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "margin: 1px 2px 3px 4px; padding: 8px");
    assert_eq!(s.margin.top, Some(Length::Px(1.0)));
    assert_eq!(s.margin.right, Some(Length::Px(2.0)));
    assert_eq!(s.margin.bottom, Some(Length::Px(3.0)));
    assert_eq!(s.margin.left, Some(Length::Px(4.0)));
    assert_eq!(s.padding.top, Some(Length::Px(8.0)));
    assert_eq!(s.padding.left, Some(Length::Px(8.0)));

    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "margin: 5px 6px");
    assert_eq!((s.margin.top, s.margin.bottom), (Some(Length::Px(5.0)), Some(Length::Px(5.0))));
    assert_eq!((s.margin.right, s.margin.left), (Some(Length::Px(6.0)), Some(Length::Px(6.0))));

    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "margin: 7px 8px 9px");
    assert_eq!(s.margin.bottom, Some(Length::Px(9.0)));
    assert_eq!(s.margin.left, Some(Length::Px(8.0)), "3-value form mirrors right to left");
}

#[test]
fn colors_parse_named_hex_short_hex() {
    assert_eq!(parse_color("red"), Some(Color(255, 0, 0, 255)));
    assert_eq!(parse_color("#00ff00"), Some(Color(0, 255, 0, 255)));
    assert_eq!(parse_color("#f00"), Some(Color(255, 0, 0, 255)));
    assert_eq!(parse_color("#ff000080").map(|c| c.3), Some(128));
    // rgb()/rgba() absorbed in batch 4a (was "out of slice scope").
    assert_eq!(parse_color("rgb(1,2,3)"), Some(Color(1, 2, 3, 255)));
}

#[test]
fn unitless_nonzero_lengths_rejected() {
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "margin-top: 0; margin-bottom: 12; padding-left: 3px");
    assert_eq!(s.margin.top, Some(Length::Px(0.0)), "zero is a valid unitless length");
    assert_eq!(s.margin.bottom, None, "nonzero unitless length is invalid CSS");
    assert_eq!(s.padding.left, Some(Length::Px(3.0)));
}

// ---- cascade ----

#[test]
fn cascade_specificity_then_source_order_then_inline() {
    let tree = diting_dom::tree_sink::parse_html(
        r#"<p id="main" class="intro" style="color: green">x</p>"#,
    );
    let node = tree.get_element_by_id("main").unwrap();

    let rules = vec![
        ParsedRule { selector: "p".into(), declarations: "color: #111111".into() },
        ParsedRule { selector: ".intro".into(), declarations: "color: #222222".into() },
        ParsedRule { selector: "#main".into(), declarations: "color: #333333".into() },
    ];
    // Compile through our own diting_dom selectors for specificity truth.
    let matched: Vec<(&ParsedRule, u32)> = rules
        .iter()
        .filter_map(|rule| {
            tree.compile_rule_selector(&rule.selector)
                .map(|compiled| (rule, compiled.specificity()))
        })
        .collect();
    assert_eq!(matched.len(), 3);

    let computed = cascade_element("p", &tree, node, &matched, None, Some("background-color: #abcdef"), DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(
        computed.color,
        Some(Color(0x33, 0x33, 0x33, 0xff)),
        "id specificity beats class beats tag: {computed:?}"
    );
    assert_eq!(
        computed.background_color,
        Some(Color(0xab, 0xcd, 0xef, 0xff)),
        "inline style applies last"
    );
}

/// transform viewport units (the deck/carousel idiom): `translateX(100vw)`
/// at an 800px viewport is 800px — before this the whole declaration was
/// dropped and every slide of such a deck rendered un-transformed. The
/// unitless entry point keeps folding against DEFAULT_VIEWPORT.
#[test]
fn parse_transform_folds_vw_vh_against_viewport() {
    let t = parse_transform_with_vp("translateX(100vw)", 800.0, 600.0).expect("vw parses");
    assert_eq!(t.tx, Length::Px(800.0));
    let t = parse_transform_with_vp("translateY(50vh)", 800.0, 600.0).expect("vh parses");
    assert_eq!(t.ty, Length::Px(300.0));
    let t = parse_transform_with_vp("translate(25vw, 10px)", 800.0, 600.0).expect("two-arg translate");
    assert_eq!((t.tx, t.ty), (Length::Px(200.0), Length::Px(10.0)));
    let t = parse_transform("translateX(100vw)").expect("default-viewport path parses");
    assert_eq!(t.tx, Length::Px(DEFAULT_VIEWPORT.0));
}

/// CSS 2.1 §6.4.1: author `!important` beats inline *normal*, and inline
/// `!important` beats author `!important` back. The deck case is the
/// first half — a stylesheet `transform: none !important` (in an
/// `@media print` un-stack block) must clear the inline
/// `translateX(100vw)` the carousel script wrote; without importance
/// handling the raw value reached the parser with the suffix attached
/// and the declaration was silently dropped.
#[test]
fn cascade_important_beats_inline_and_important_inline_wins() {
    let tree = diting_dom::tree_sink::parse_html(
        r#"<p id="main" class="slide">x</p>"#,
    );
    let node = tree.get_element_by_id("main").unwrap();
    let rules = vec![ParsedRule {
        selector: ".slide".into(),
        declarations: "margin-top: 1px !important; transform: none !important".into(),
    }];
    let matched: Vec<(&ParsedRule, u32)> = rules
        .iter()
        .filter_map(|rule| {
            tree.compile_rule_selector(&rule.selector)
                .map(|compiled| (rule, compiled.specificity()))
        })
        .collect();

    let computed = cascade_element(
        "p", &tree, node, &matched, None,
        Some("margin-top: 9px; transform: translateY(100vh)"),
        DEFAULT_ROOT_FONT_SIZE, (800.0, 600.0),
    );
    assert_eq!(
        computed.margin.top, Some(Length::Px(1.0)),
        "author !important beats inline normal"
    );
    assert_eq!(
        computed.transform, None,
        "transform:none !important clears the inline transform"
    );

    // And the top tier: inline !important flips it back (case-insensitive
    // suffix — strip_important matches lowercased).
    let computed = cascade_element(
        "p", &tree, node, &matched, None,
        Some("margin-top: 4px !IMPORTANT"),
        DEFAULT_ROOT_FONT_SIZE, (800.0, 600.0),
    );
    assert_eq!(
        computed.margin.top, Some(Length::Px(4.0)),
        "inline !important beats author !important"
    );
}

#[test]
fn inheritance_flows_from_parent_and_author_overrides() {
    let tree =
        diting_dom::tree_sink::parse_html(r#"<section><p>x</p><em>y</em></section>"#);
    let section = tree.query_selector("section").unwrap().unwrap();
    let em = tree.query_selector("em").unwrap().unwrap();

    let parent = ComputedStyle {
        color: Some(Color(51, 51, 51, 255)),
        font_size: Some(18.0),
        text_align: Some(TextAlign::Center),
        ..Default::default()
    };
    // No matched author rules for <em>: pure inheritance + UA defaults.
    let child = cascade_element("em", &tree, em, &[], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(child.color, parent.color, "color inherits");
    assert_eq!(child.font_size, parent.font_size, "font-size inherits");
    assert_eq!(child.text_align, parent.text_align, "text-align inherits");
    assert_eq!(child.display, Some(Display::Inline), "UA default for <em>");

    // Author rule on the child overrides the inherited color only.
    let rule = ParsedRule { selector: "em".into(), declarations: "color: red".into() };
    let spec = tree.compile_rule_selector("em").unwrap().specificity();
    let overridden = cascade_element("em", &tree, em, &[(&rule, spec)], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(overridden.color, Some(Color(255, 0, 0, 255)));
    assert_eq!(overridden.font_size, parent.font_size, "unmentioned props still inherit");

    // Section itself: block UA default even with no author CSS.
    let block = cascade_element("section", &tree, section, &[], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(block.display, Some(Display::Block));
}

#[test]
fn th_centers_by_default_and_beats_an_inherited_align() {
    let tree = diting_dom::tree_sink::parse_html(
        r#"<table><tr><th>x</th><td><em>z</em></td></tr></table>"#,
    );
    let th = tree.query_selector("th").unwrap().unwrap();
    let td = tree.query_selector("td").unwrap().unwrap();
    let em = tree.query_selector("em").unwrap().unwrap();

    let th_cs = cascade_element("th", &tree, th, &[], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(th_cs.text_align, Some(TextAlign::Center), "UA center on th");
    let td_cs = cascade_element("td", &tree, td, &[], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(td_cs.text_align, None, "td stays start-aligned");

    // The element's own UA declaration beats an inherited value: a th
    // stays centered inside a right-aligned ancestor, plain tags inherit.
    let parent = ComputedStyle { text_align: Some(TextAlign::Right), ..Default::default() };
    let centered = cascade_element("th", &tree, th, &[], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(centered.text_align, Some(TextAlign::Center));
    let right = cascade_element("em", &tree, em, &[], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(right.text_align, Some(TextAlign::Right));
}

#[test]
fn table_height_attribute_is_a_presentational_hint() {
    let tree = diting_dom::tree_sink::parse_html(
        r#"<table><tr height="30"><td height="55">x</td><td style="height:10px">y</td></tr></table>"#,
    );
    let td1 = tree.query_selector("td").unwrap().unwrap();
    let td2 = tree.query_selector("td[style]").unwrap().unwrap();
    let tr = tree.query_selector("tr").unwrap().unwrap();

    let cs = cascade_element("td", &tree, td1, &[], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(cs.height, Some(Length::Px(55.0)), "attr height lands as px");
    // Inline style applies after the hint and wins.
    let authored = cascade_element("td", &tree, td2, &[], None, Some("height:10px"), DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(authored.height, Some(Length::Px(10.0)));
    let tr_cs = cascade_element("tr", &tree, tr, &[], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(tr_cs.height, Some(Length::Px(30.0)));
    // Non-table tags ignore the attribute entirely.
    let plain = cascade_element("div", &tree, td1, &[], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(plain.height, None, "height attr is table-cell/row only");
}

#[test]
fn var_substitutes_from_custom_properties_and_inherits() {
    let tree =
        diting_dom::tree_sink::parse_html(r#"<section><p>x</p><span>y</span></section>"#);
    let section = tree.query_selector("section").unwrap().unwrap();
    let span = tree.query_selector("span").unwrap().unwrap();

    let root_rule = ParsedRule {
        selector: "section".into(),
        declarations: "--main-color: #ff0000; color: var(--main-color)".into(),
    };
    let spec = tree.compile_rule_selector("section").unwrap().specificity();
    let parent = cascade_element("section", &tree, section, &[(&root_rule, spec)], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(parent.color, Some(Color(255, 0, 0, 255)), "var() resolves against same-element custom property");
    assert_eq!(parent.custom.get("--main-color").map(String::as_str), Some("#ff0000"));

    // Child with no rules: custom map inherits (and would feed its own var()s).
    let child = cascade_element("span", &tree, span, &[], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(child.custom.get("--main-color").map(String::as_str), Some("#ff0000"), "custom properties inherit");

    // Child rule USING the inherited custom property.
    let use_rule = ParsedRule { selector: "span".into(), declarations: "color: var(--main-color)".into() };
    let use_spec = tree.compile_rule_selector("span").unwrap().specificity();
    let styled = cascade_element("span", &tree, span, &[(&use_rule, use_spec)], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(styled.color, Some(Color(255, 0, 0, 255)), "var() resolves against inherited custom property");
}

#[test]
fn var_fallback_and_unresolved_behavior() {
    let tree = diting_dom::tree_sink::parse_html(r#"<p>x</p>"#);
    let p = tree.query_selector("p").unwrap().unwrap();

    let rule = ParsedRule {
        selector: "p".into(),
        declarations: "color: var(--missing, rgb(0, 0, 255)); background-color: var(--missing)".into(),
    };
    let spec = tree.compile_rule_selector("p").unwrap().specificity();

    let parent = ComputedStyle { color: Some(Color(1, 2, 3, 255)), ..Default::default() };
    let cs = cascade_element("p", &tree, p, &[(&rule, spec)], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    // Fallback may contain commas (rgb) — split_top_comma keeps it whole.
    assert_eq!(cs.color, Some(Color(0, 0, 255, 255)), "fallback (with commas) substitutes");
    // Unresolved without fallback: declaration dropped, inherited value survives (IACVT approximation).
    assert_eq!(cs.background_color, None, "unresolved var() drops the declaration");
}

#[test]
fn var_in_dimensions_and_chained_custom_properties() {
    let tree = diting_dom::tree_sink::parse_html(r#"<div>x</div>"#);
    let div = tree.query_selector("div").unwrap().unwrap();

    let rule = ParsedRule {
        selector: "div".into(),
        declarations:
            "--w: 100px; width: var(--w); --a: var(--b); --b: 10px; padding-top: var(--a)".into(),
    };
    let spec = tree.compile_rule_selector("div").unwrap().specificity();
    let cs = cascade_element("div", &tree, div, &[(&rule, spec)], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(cs.width, Some(Length::Px(100.0)), "var() resolves in dimensions");
    // --a references --b (declared earlier in the list): stored raw and
    // substituted when `padding-top` uses --a, so chains resolve.
    assert_eq!(cs.padding.top, Some(Length::Px(10.0)), "chained custom properties resolve at use time");
}

#[test]
fn custom_property_names_are_case_sensitive() {
    let mut s = ComputedStyle::default();
    let fonts = FontCtx::default();
    assert!(apply_declarations_with(&mut s, "--Main: 5px; width: var(--Main)", &fonts));
    assert_eq!(s.width, Some(Length::Px(5.0)), "--Main round-trips case-sensitively");

    let mut s2 = ComputedStyle::default();
    apply_declarations_with(&mut s2, "--Main: 5px; width: var(--main, 9px)", &fonts);
    assert_eq!(s2.width, Some(Length::Px(9.0)), "--main does NOT match --Main (case-sensitive)");
}

// ---- line-height + UA box defaults (§49 parity fixes) ----

#[test]
fn line_height_parses_all_forms_and_inherits_as_number() {
    let tree = diting_dom::tree_sink::parse_html(r#"<div id="outer"><div id="inner">x</div></div>"#);
    let outer = tree.query_selector("#outer").unwrap().unwrap();
    let inner = tree.query_selector("#inner").unwrap().unwrap();

    let rule = ParsedRule {
        selector: "#outer".into(),
        declarations: "line-height: 1.6".into(),
    };
    let spec = tree.compile_rule_selector("#outer").unwrap().specificity();
    let parent = cascade_element("div", &tree, outer, &[(&rule, spec)], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(parent.line_height, Some(LineHeightSpec::Number(1.6)));

    // The child inherits the NUMBER (spec computed value) — it scales
    // against whichever font-size the text actually uses.
    let child = cascade_element("div", &tree, inner, &[], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(child.line_height, Some(LineHeightSpec::Number(1.6)));

    // Length forms collapse to absolute px (em against own font-size).
    let px_rule = ParsedRule {
        selector: "#inner".into(),
        declarations: "font-size: 20px; line-height: 1.5em".into(),
    };
    let spec2 = tree.compile_rule_selector("#inner").unwrap().specificity();
    let sized = cascade_element("div", &tree, inner, &[(&px_rule, spec2)], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(sized.line_height, Some(LineHeightSpec::Px(30.0)));
    assert_eq!(sized.font_size, Some(20.0));

    // `normal` and percentages fold to their canonical forms.
    let mut s = ComputedStyle::default();
    assert!(apply_one(&mut s, "line-height", "normal", &FontCtx::default()));
    assert_eq!(s.line_height, Some(LineHeightSpec::Normal));
    assert!(apply_one(&mut s, "line-height", "150%", &FontCtx::default()));
    assert_eq!(s.line_height, Some(LineHeightSpec::Number(1.5)));
    assert!(!apply_one(&mut s, "line-height", "nonsense", &FontCtx::default()));
    // Unitless numbers are legal ONLY here: `1.6` parses, `1.6px`-shaped
    // junk and negative values don't.
    assert!(apply_one(&mut s, "line-height", "1.6", &FontCtx::default()));
    assert!(!apply_one(&mut s, "line-height", "-1.2", &FontCtx::default()));
}

#[test]
fn font_shorthand_grammar() {
    let f = FontCtx::default();
    // Bare size+family: weight defaults 400, no line-height given.
    let p = parse_font_shorthand("20px monospace", &f).unwrap();
    assert_eq!(p.weight, 400);
    assert_eq!(p.size_px, Some(20.0));
    assert_eq!(p.line_height, None);
    assert_eq!(p.family, "monospace");

    // Front-walk consumes style+weight in any order; em size folds
    // against the context; em line-height folds against the NEW size.
    let p = parse_font_shorthand("italic bold 1.5em/1.5em serif", &f).unwrap();
    assert_eq!(p.weight, 700);
    assert_eq!(p.size_px, Some(24.0));
    assert_eq!(p.line_height, Some(LineHeightSpec::Px(36.0)));
    assert_eq!(p.family, "serif");

    // Attached /line-height (px), quoted family stays verbatim.
    let p = parse_font_shorthand("12px/18px \"Times New Roman\", serif", &f).unwrap();
    assert_eq!(p.line_height, Some(LineHeightSpec::Px(18.0)));
    assert_eq!(p.family, "\"Times New Roman\", serif");

    // Detached slash forms + % lh behaves like a multiplier.
    assert_eq!(
        parse_font_shorthand("20px / 1.5 serif", &f).unwrap().line_height,
        Some(LineHeightSpec::Number(1.5))
    );
    assert_eq!(
        parse_font_shorthand("20px /1.5 serif", &f).unwrap().line_height,
        Some(LineHeightSpec::Number(1.5))
    );
    assert_eq!(
        parse_font_shorthand("20px/120% serif", &f).unwrap().line_height,
        Some(LineHeightSpec::Number(1.2))
    );

    // System font keywords and global keywords are not modeled.
    for kw in ["caption", "status-bar", "inherit", "initial"] {
        assert!(parse_font_shorthand(kw, &f).is_none(), "{kw}");
    }
    // Size and family are both mandatory; a bad size kills the whole
    // shorthand (no partial application).
    assert!(parse_font_shorthand("20px", &f).is_none());
    assert!(parse_font_shorthand("20orpx serif", &f).is_none());

    // apply_one wiring: size/family/weight land, and line-height resets
    // to normal when the shorthand carries none (shorthand semantics).
    let mut s = ComputedStyle::default();
    assert!(apply_one(&mut s, "font", "bold 20px monospace", &f));
    assert_eq!(s.font_size, Some(20.0));
    assert_eq!(s.font_family.as_deref(), Some("monospace"));
    assert_eq!(s.font_weight, Some(700));
    assert_eq!(s.line_height, Some(LineHeightSpec::Normal));
    assert!(!apply_one(&mut s, "font", "caption", &f));
}

#[test]
fn text_decoration_grammar() {
    let f = FontCtx::default();
    let mut s = ComputedStyle::default();
    assert!(apply_one(&mut s, "text-decoration", "underline", &f));
    assert_eq!(
        s.text_decoration_line,
        Some(TextDecorations { underline: true, overline: false, line_through: false })
    );
    // Multi-line longhand unions.
    assert!(apply_one(&mut s, "text-decoration-line", "underline line-through", &f));
    assert_eq!(
        s.text_decoration_line,
        Some(TextDecorations { underline: true, overline: false, line_through: true })
    );
    // Shorthand style/color/thickness legs parse-and-drop.
    assert!(apply_one(&mut s, "text-decoration", "underline wavy red 2px", &f));
    assert_eq!(
        s.text_decoration_line,
        Some(TextDecorations { underline: true, overline: false, line_through: false })
    );
    // `none` is a real declaration (kills UA decorations); overline alone
    // is legal; garbage invalidates the whole declaration.
    assert!(apply_one(&mut s, "text-decoration", "none", &f));
    assert_eq!(s.text_decoration_line, Some(TextDecorations::default()));
    assert!(apply_one(&mut s, "text-decoration-line", "overline", &f));
    assert_eq!(
        s.text_decoration_line,
        Some(TextDecorations { underline: false, overline: true, line_through: false })
    );
    assert!(!apply_one(&mut s, "text-decoration", "sideways", &f));
    assert!(!apply_one(&mut s, "text-decoration-line", "wavy", &f));
    assert!(!apply_one(&mut s, "text-decoration", "solid", &f));
}

#[test]
fn ua_text_decoration_tags() {
    let u = Some(TextDecorations { underline: true, ..Default::default() });
    let lt = Some(TextDecorations { line_through: true, ..Default::default() });
    let deco_of = |html: &str, sel: &str, tag: &str| {
        let tree = diting_dom::tree_sink::parse_html(html);
        let id = tree.query_selector(sel).unwrap().unwrap();
        cascade_element(tag, &tree, id, &[], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0))
            .text_decoration_line
    };
    assert_eq!(deco_of("<body><u>x</u></body>", "u", "u"), u);
    assert_eq!(deco_of("<body><ins>x</ins></body>", "ins", "ins"), u);
    assert_eq!(deco_of("<body><s>x</s></body>", "s", "s"), lt);
    assert_eq!(deco_of("<body><del>x</del></body>", "del", "del"), lt);
    assert_eq!(deco_of("<body><strike>x</strike></body>", "strike", "strike"), lt);
    // The link gate: href → underline (any-link); bare <a> stays plain.
    assert_eq!(deco_of(r#"<body><a href="https://x">l</a></body>"#, "a", "a"), u);
    assert_eq!(deco_of("<body><a>n</a></body>", "a", "a"), None);
    // Author `none` on a u kills the UA underline.
    let tree = diting_dom::tree_sink::parse_html(r#"<body><u style="text-decoration:none">x</u></body>"#);
    let id = tree.query_selector("u").unwrap().unwrap();
    let inline: Option<String> =
        tree.with_node(id, |n| n.get_attribute("style").map(str::to_string)).flatten();
    assert_eq!(
        cascade_element("u", &tree, id, &[], None, inline.as_deref(), DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0))
            .text_decoration_line,
        Some(TextDecorations::default())
    );
}

#[test]
fn ua_sub_sup_baseline_shift_declares() {
    let va_of = |html: &str, sel: &str, tag: &str| {
        let tree = diting_dom::tree_sink::parse_html(html);
        let id = tree.query_selector(sel).unwrap().unwrap();
        cascade_element(tag, &tree, id, &[], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0))
            .vertical_align
    };
    assert_eq!(va_of("<body><sup>x</sup></body>", "sup", "sup"), Some(VerticalAlign::Super));
    assert_eq!(va_of("<body><sub>x</sub></body>", "sub", "sub"), Some(VerticalAlign::Sub));
    // Plain inline and font-size:smaller cousin stay baseline-aligned.
    assert_eq!(va_of("<body><span>x</span></body>", "span", "span"), None);
    assert_eq!(va_of("<body><big>x</big></body>", "big", "big"), None);

    // Author declarations override the UA shift.
    let tree = diting_dom::tree_sink::parse_html(r#"<body><sup style="vertical-align:baseline">x</sup></body>"#);
    let id = tree.query_selector("sup").unwrap().unwrap();
    let inline: Option<String> =
        tree.with_node(id, |n| n.get_attribute("style").map(str::to_string)).flatten();
    assert_eq!(
        cascade_element("sup", &tree, id, &[], None, inline.as_deref(), DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0))
            .vertical_align,
        Some(VerticalAlign::Baseline)
    );

    // Keyword surface: all six spell through the same slot.
    let mut s = ComputedStyle::default();
    for (kw, want) in [
        ("baseline", VerticalAlign::Baseline),
        ("sub", VerticalAlign::Sub),
        ("super", VerticalAlign::Super),
        ("top", VerticalAlign::Top),
        ("middle", VerticalAlign::Middle),
        ("bottom", VerticalAlign::Bottom),
    ] {
        apply_declarations(&mut s, &format!("vertical-align: {kw}"));
        assert_eq!(s.vertical_align, Some(want), "keyword {kw}");
    }
    // Length/percentage shifts model through the same slot: px lands
    // absolute, em folds against the element's own font-size (the test
    // cascade's own fs = DEFAULT_ROOT_FONT_SIZE), % keeps its shape for
    // the layout side, `0` is the unitless zero, junk is rejected.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "vertical-align: 3px"));
    assert_eq!(s.vertical_align, Some(VerticalAlign::Length(3.0)));
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "vertical-align: -10px"));
    assert_eq!(s.vertical_align, Some(VerticalAlign::Length(-10.0)));
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "vertical-align: 0.5em"));
    assert_eq!(s.vertical_align, Some(VerticalAlign::Length(0.5 * DEFAULT_ROOT_FONT_SIZE)));
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "vertical-align: 0"));
    assert_eq!(s.vertical_align, Some(VerticalAlign::Length(0.0)));
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "vertical-align: 50%"));
    assert_eq!(s.vertical_align, Some(VerticalAlign::Percent(50.0)));
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "vertical-align: 2vw"));
    assert_eq!(
        s.vertical_align,
        Some(VerticalAlign::Length(2.0 * DEFAULT_VIEWPORT.0 / 100.0))
    );
}

// ---- batch 162: viewport units (vw/vh) + inset shorthand ----

#[test]
fn vw_vh_resolve_against_viewport() {
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "width: 50vw; height: 50vh"));
    assert_eq!(s.width, Some(Length::Px(0.5 * DEFAULT_VIEWPORT.0)));
    assert_eq!(s.height, Some(Length::Px(0.5 * DEFAULT_VIEWPORT.1)));

    let vp = FontCtx { own: 16.0, root: 16.0, viewport_w: 800.0, viewport_h: 600.0 };
    let mut s = ComputedStyle::default();
    assert!(apply_declarations_with(&mut s, "width: 50vw; margin: 10vw", &vp));
    assert_eq!(s.width, Some(Length::Px(400.0)), "50vw of 800");
    assert_eq!(s.margin.top, Some(Length::Px(80.0)), "10vw of 800");
}

#[test]
fn vw_inside_calc_resolves_per_viewport() {
    let wide = FontCtx { own: 16.0, root: 16.0, viewport_w: 800.0, viewport_h: 600.0 };
    let mut s = ComputedStyle::default();
    assert!(apply_declarations_with(&mut s, "width: calc(100vw - 20px)", &wide));
    assert_eq!(s.width, Some(Length::Px(780.0)));
    // Same declaration re-resolved under a different viewport must differ —
    // this is what bake-at-parse could never give us.
    let narrow = FontCtx { own: 16.0, root: 16.0, viewport_w: 400.0, viewport_h: 600.0 };
    let mut s2 = ComputedStyle::default();
    assert!(apply_declarations_with(&mut s2, "width: calc(100vw - 20px)", &narrow));
    assert_eq!(s2.width, Some(Length::Px(380.0)));
}

#[test]
fn vw_threads_through_compute_styles_viewport() {
    let tree =
        diting_dom::tree_sink::parse_html(r#"<div style="width: 50vw; font-size: 2vh">x</div>"#);
    let d = tree.query_selector("div").unwrap().unwrap();
    let at_800 = crate::diting_layout::compute_styles(&tree, &[], (800.0, 600.0));
    let at_400 = crate::diting_layout::compute_styles(&tree, &[], (400.0, 600.0));
    assert_eq!(at_800[&d].width, Some(Length::Px(400.0)));
    assert_eq!(at_400[&d].width, Some(Length::Px(200.0)));
    // font-size folds in the cascade pre-pass, like em/% before it.
    assert_eq!(at_800[&d].font_size, Some(12.0), "2vh of 600");
}

#[test]
fn inset_shorthand_expansion_and_auto_reset() {
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "inset: 8px"));
    assert_eq!(
        (s.top, s.right, s.bottom, s.left),
        (
            Some(Length::Px(8.0)),
            Some(Length::Px(8.0)),
            Some(Length::Px(8.0)),
            Some(Length::Px(8.0))
        )
    );
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "inset: 1px 2px"));
    assert_eq!(
        (s.top, s.right, s.bottom, s.left),
        (
            Some(Length::Px(1.0)),
            Some(Length::Px(2.0)),
            Some(Length::Px(1.0)),
            Some(Length::Px(2.0))
        )
    );
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "inset: 1px 2px 3px"));
    assert_eq!(
        (s.top, s.right, s.bottom, s.left),
        (
            Some(Length::Px(1.0)),
            Some(Length::Px(2.0)),
            Some(Length::Px(3.0)),
            Some(Length::Px(2.0))
        )
    );
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "inset: 1px 2px 3px 4px"));
    assert_eq!(
        (s.top, s.right, s.bottom, s.left),
        (
            Some(Length::Px(1.0)),
            Some(Length::Px(2.0)),
            Some(Length::Px(3.0)),
            Some(Length::Px(4.0))
        )
    );
    // auto is the property initial here: None slots, not Length::Auto.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "inset: 0 auto"));
    assert_eq!(s.top, Some(Length::Px(0.0)));
    assert_eq!(s.right, None, "auto folds to the None initial");
    // inset: auto resets all four, overriding earlier singles.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "top: 5px; left: 6px; inset: auto"));
    assert_eq!((s.top, s.right, s.bottom, s.left), (None, None, None, None));
}

#[test]
fn ua_box_defaults_match_browser_sheet() {
    let tree = diting_dom::tree_sink::parse_html(
        r#"<body><h1>t</h1><p>p</p><ul><li>l</li></ul></body>"#,
    );
    let body = tree.query_selector("body").unwrap().unwrap();
    let h1 = tree.query_selector("h1").unwrap().unwrap();
    let p = tree.query_selector("p").unwrap().unwrap();
    let ul = tree.query_selector("ul").unwrap().unwrap();

    let body_style = cascade_element("body", &tree, body, &[], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    for side in [body_style.margin.top, body_style.margin.right, body_style.margin.bottom, body_style.margin.left] {
        assert_eq!(side, Some(Length::Px(8.0)), "body 8px UA margin");
    }

    // h1: 2em font-size + bold + .67em margins AGAINST ITS OWN 2em size.
    let h1_style = cascade_element("h1", &tree, h1, &[], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(h1_style.font_size, Some(32.0), "h1 UA size 2em of 16");
    assert_eq!(h1_style.font_weight, Some(700), "h1 UA bold");
    let em67 = Length::Px((0.67f32 * 32.0 * 100.0).round() / 100.0);
    assert_eq!(h1_style.margin.top, Some(em67), "h1 .67em of its own 2em size");
    assert_eq!(h1_style.margin.bottom, Some(em67));

    let p_style = cascade_element("p", &tree, p, &[], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(p_style.margin.top, Some(Length::Px(16.0)), "p 1em block margin");

    let ul_style = cascade_element("ul", &tree, ul, &[], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(ul_style.margin.top, Some(Length::Px(16.0)), "ul 1em block margin");
    assert_eq!(ul_style.padding.left, Some(Length::Px(40.0)), "ul 40px inline-start padding");

    // Author margin overrides per side.
    let rule = ParsedRule { selector: "p".into(), declarations: "margin-top: 0".into() };
    let spec = tree.compile_rule_selector("p").unwrap().specificity();
    let authored = cascade_element("p", &tree, p, &[(&rule, spec)], None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(authored.margin.top, Some(Length::Px(0.0)), "author margin-top wins");
    assert_eq!(authored.margin.bottom, Some(Length::Px(16.0)), "UA bottom margin survives");
}

// ---- batch 2e: em/rem/% lengths ----

#[test]
fn em_rem_and_percent_parse_and_resolve() {
    let mut s = ComputedStyle::default();
    let fonts = FontCtx { own: 20.0, root: 32.0, viewport_w: DEFAULT_VIEWPORT.0, viewport_h: DEFAULT_VIEWPORT.1 };
    apply_declarations_with(&mut s, "width: 10em; height: 2.5rem; margin: 5% 1px", &fonts);
    assert_eq!(s.width, Some(Length::Px(200.0)), "em folds against own fs");
    assert_eq!(s.height, Some(Length::Px(80.0)), "rem folds against root fs");
    assert_eq!(s.margin.top, Some(Length::Percent(5.0)), "% stays symbolic");
    assert_eq!(s.margin.left, Some(Length::Px(1.0)));
}

#[test]
fn font_size_em_percent_against_parent_rem_against_root() {
    let tree = diting_dom::tree_sink::parse_html(r#"<div><p>x</p></div>"#);
    let p = tree.query_selector("p").unwrap().unwrap();
    let parent = ComputedStyle { font_size: Some(20.0), ..Default::default() };

    // 1.5em of 20 → 30; 150% of 20 → 30; 1.25rem of root 24 → 30.
    let mk = |decl: &str, root: f32| {
        cascade_element(
            "p", &tree, p,
            &[(&ParsedRule { selector: "p".into(), declarations: decl.into() }, 1)],
            Some(&parent), None, root, (1280.0, 720.0),
        )
    };
    assert_eq!(mk("font-size: 1.5em", 16.0).font_size, Some(30.0), "em against parent");
    assert_eq!(mk("font-size: 150%", 16.0).font_size, Some(30.0), "% against parent");
    assert_eq!(mk("font-size: 1.25rem", 24.0).font_size, Some(30.0), "rem against root");
    assert_eq!(mk("color: red", 16.0).font_size, Some(20.0), "inherits parent");
}

#[test]
fn font_size_relative_keywords_larger_smaller() {
    // inline `font-size: smaller/larger` used to be dropped as an
    // unparseable keyword (element kept the inherited 16px). They fold
    // against the parent with the 1.2 ladder — same treatment the UA
    // sheet gives the <small>/<big> tags.
    let tree = diting_dom::tree_sink::parse_html(r#"<div><p>x</p></div>"#);
    let p = tree.query_selector("p").unwrap().unwrap();
    let parent = ComputedStyle { font_size: Some(20.0), ..Default::default() };
    let mk = |decl: &str, parent: &ComputedStyle| {
        cascade_element(
            "p", &tree, p, &[],
            Some(parent), Some(decl), DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0),
        )
    };
    assert_eq!(
        mk("font-size: smaller", &parent).font_size,
        Some(20.0 / 1.2),
        "smaller = parent / 1.2"
    );
    assert_eq!(
        mk("font-size: larger", &parent).font_size,
        Some(24.0),
        "larger = parent * 1.2"
    );
    let default = ComputedStyle::default();
    assert_eq!(
        mk("font-size: smaller", &default).font_size,
        Some(16.0 / 1.2),
        "16px base: 13.33"
    );
    assert_eq!(
        mk("font-size: larger", &default).font_size,
        Some(19.2),
        "16px base: 19.2"
    );
    // Compound declarations still work — the keyword is one token among
    // the rest of the block.
    let cs = cascade_element(
        "p", &tree, p, &[],
        Some(&default), Some("color: red; font-size: larger; width: 10em"), DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0),
    );
    assert_eq!(cs.font_size, Some(19.2));
    assert_eq!(cs.width, Some(Length::Px(192.0)), "width em folds against the keyword result");
}

#[test]
fn font_size_computes_before_em_lengths_same_block() {
    // Declaration order inside one block must not matter: font-size is
    // computed first, then width's em folds against it (CSS spec order).
    let tree = diting_dom::tree_sink::parse_html("<div><p>x</p></div>");
    let p = tree.query_selector("p").unwrap().unwrap();
    let cs = cascade_element(
        "p", &tree, p,
        &[(&ParsedRule { selector: "p".into(), declarations: "width: 10em; font-size: 24px".into() }, 1)],
        None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0),
    );
    assert_eq!(cs.font_size, Some(24.0));
    assert_eq!(cs.width, Some(Length::Px(240.0)), "10em of the same block's font-size");
}

#[test]
fn cascade_font_size_wins_by_specificity_not_prepass_order() {
    // The pre-pass must respect cascade order: the id rule's font-size
    // beats the later-parsed class rule, and width's em folds against
    // the WINNER.
    let tree = diting_dom::tree_sink::parse_html(r#"<p id="m" class="c">x</p>"#);
    let p = tree.get_element_by_id("m").unwrap();
    let rules = vec![
        ParsedRule { selector: "p.c".into(), declarations: "font-size: 10px; width: 2em".into() },
        ParsedRule { selector: "#m".into(), declarations: "font-size: 30px".into() },
    ];
    let matched: Vec<(&ParsedRule, u32)> = rules
        .iter()
        .filter_map(|r| tree.compile_rule_selector(&r.selector).map(|c| (r, c.specificity())))
        .collect();
    let cs = cascade_element("p", &tree, p, &matched, None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(cs.font_size, Some(30.0), "id specificity wins font-size");
    assert_eq!(cs.width, Some(Length::Px(60.0)), "2em of 30, not of 10");
}

#[test]
fn end_to_end_stylesheet_through_dom_matching() {
    // The batch's headline integration: parse a real-shaped sheet with
    // this module, match its selectors with OUR diting_dom engine
    // (including :where() from batch −1), cascade the winners.
    let html = r#"<html><body>
        <nav class="menu"><a href="/x">link</a></nav>
        <article><p class="lead">text</p></article>
    </body></html>"#;
    let tree = diting_dom::tree_sink::parse_html(html);

    let sheet = r#"
        body { margin: 0; }
        .menu a { text-align: center; }
        :where(article) { padding: 12px; }
        article .lead { font-weight: bold; }
    "#;
    let rules = parse_stylesheet(sheet);
    assert_eq!(rules.len(), 4, "{rules:?}");

    let lead = tree.query_selector(".lead").unwrap().unwrap();
    let matched: Vec<(&ParsedRule, u32)> = rules
        .iter()
        .enumerate()
        .filter_map(|(order, rule)| {
            // Match against the element per querySelector semantics.
            let hits = tree.query_selector_all_from(
                tree.document(),
                &rule.selector,
            ).ok()?;
            if order == usize::MAX || !hits.contains(&lead) {
                return None;
            }
            let compiled = tree.compile_rule_selector(&rule.selector)?;
            Some((rule, compiled.specificity()))
        })
        .collect();
    // `.menu a` does not hit .lead; `article .lead` does. `:where(article)`
    // matches the <article> element, not its child .lead.
    assert_eq!(matched.len(), 1, "{matched:?}");

    let computed = cascade_element("p", &tree, lead, &matched, None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(computed.font_weight, Some(700), "author bold applies");
    assert_eq!(computed.text_align, None, ".menu a never touched .lead");

    // The :where rule applies to the article element itself.
    let article = tree.query_selector("article").unwrap().unwrap();
    let art_matched: Vec<(&ParsedRule, u32)> = rules
        .iter()
        .filter_map(|rule| {
            let hits = tree
                .query_selector_all_from(tree.document(), &rule.selector)
                .ok()?;
            if !hits.contains(&article) {
                return None;
            }
            let compiled = tree.compile_rule_selector(&rule.selector)?;
            Some((rule, compiled.specificity()))
        })
        .collect();
    assert_eq!(art_matched.len(), 1, "{art_matched:?}");
    let art_computed = cascade_element("article", &tree, article, &art_matched, None, None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(art_computed.padding.top, Some(Length::Px(12.0)), ":where(article) padding applies");
}

// ---- calc() (obscura#767) ----

#[test]
fn calc_folds_to_px_percent_or_mixed() {
    let f = FontCtx::default();
    // Pure px arithmetic folds at parse time — never reaches layout.
    assert_eq!(eval_calc("100px + 50px", &f), Some(Length::Px(150.0)));
    assert_eq!(eval_calc("calc(60px - 10.5px)", &f), Some(Length::Px(49.5)));
    assert_eq!(eval_calc("2 * 25px", &f), Some(Length::Px(50.0)));
    assert_eq!(eval_calc("100px / 4", &f), Some(Length::Px(25.0)));
    // Percent-only folds to Percent.
    assert_eq!(eval_calc("100% - 50%", &f), Some(Length::Percent(50.0)));
    assert_eq!(eval_calc("50%", &f), Some(Length::Percent(50.0)));
    // Mixed stays symbolic: percent in CSS 0-100 scale, px exact.
    assert_eq!(
        eval_calc("100% - 30px", &f),
        Some(Length::Calc { percent: 100.0, px: -30.0 })
    );
    assert_eq!(
        eval_calc("50% + 10px", &f),
        Some(Length::Calc { percent: 50.0, px: 10.0 })
    );
    // em folds against the font context like bare declarations.
    let big = FontCtx { own: 20.0, root: 16.0, viewport_w: DEFAULT_VIEWPORT.0, viewport_h: DEFAULT_VIEWPORT.1 };
    assert_eq!(eval_calc("1em + 4px", &big), Some(Length::Px(24.0)));
    // Parentheses and nested calc() group.
    assert_eq!(
        eval_calc("(100% - 30px) / 2", &f),
        Some(Length::Calc { percent: 50.0, px: -15.0 })
    );
    assert_eq!(
        eval_calc("CALC(100% - calc(10px + 5px))", &f),
        Some(Length::Calc { percent: 100.0, px: -15.0 })
    );
    // Signed operand right after the operator lexes as one token.
    assert_eq!(
        eval_calc("10px + -2px", &f),
        Some(Length::Px(8.0))
    );
}

#[test]
fn calc_rejects_invalid_grammar() {
    let f = FontCtx::default();
    // A bare number is not a length without a unit context.
    assert_eq!(eval_calc("5", &f), None);
    // + / - between a number and a length is invalid CSS.
    assert_eq!(eval_calc("100px + 5", &f), None);
    assert_eq!(eval_calc("5 + 100px", &f), None);
    // Division by zero.
    assert_eq!(eval_calc("100px / 0", &f), None);
    // Trailing garbage after a complete expression.
    assert_eq!(eval_calc("100px + 5px )", &f), None);
    assert_eq!(eval_calc("100px + 5px extra", &f), None);
    // Unbalanced parens.
    assert_eq!(eval_calc("(100px", &f), None);
    assert_eq!(eval_calc("100px)", &f), None);
    // * / with two length operands is invalid.
    assert_eq!(eval_calc("10px * 2px", &f), None);
    assert_eq!(eval_calc("10px / 2px", &f), None);
}

#[test]
fn calc_width_min_max_and_sides_parse() {
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "width: calc(100% - 30px)"));
    assert_eq!(s.width, Some(Length::Calc { percent: 100.0, px: -30.0 }));
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "min-width: calc(50% + 10px); max-width: calc(100% - 2em)"));
    assert_eq!(s.min_width, Some(Length::Calc { percent: 50.0, px: 10.0 }));
    assert_eq!(s.max_width, Some(Length::Calc { percent: 100.0, px: -32.0 }));

    // expand_sides keeps calc() tokens whole across the whitespace split.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "padding: calc(100% - 30px) 5px"));
    assert_eq!(s.padding.top, Some(Length::Calc { percent: 100.0, px: -30.0 }));
    assert_eq!(s.padding.right, Some(Length::Px(5.0)));
    assert_eq!(s.padding.bottom, Some(Length::Calc { percent: 100.0, px: -30.0 }));
    assert_eq!(s.padding.left, Some(Length::Px(5.0)));

    // Inline styles share the grammar.
    let mut s = ComputedStyle::default();
    assert!(apply_inline_declarations(&mut s, "margin-left: calc(2 * 8px)"));
    assert_eq!(s.margin.left, Some(Length::Px(16.0)));
}

// ---- min()/max()/clamp() ----

#[test]
fn math_calls_pick_and_clamp_one_unit_family() {
    let f = FontCtx::default();
    // min/max fold to the extreme operand.
    assert_eq!(eval_math_call("min(100px, 50px)", &f), Some(Length::Px(50.0)));
    // Arithmetic inside an argument folds before the comparison.
    assert_eq!(eval_math_call("max(100px, 50px + 60px)", &f), Some(Length::Px(110.0)));
    // clamp = max(MIN, min(VAL, MAX)); pinned on each edge and in the middle.
    assert_eq!(eval_math_call("clamp(10px, 5px, 100px)", &f), Some(Length::Px(10.0)));
    assert_eq!(eval_math_call("clamp(10px, 50px, 100px)", &f), Some(Length::Px(50.0)));
    assert_eq!(eval_math_call("clamp(10px, 500px, 100px)", &f), Some(Length::Px(100.0)));
    // Percent-only family folds to Percent.
    assert_eq!(eval_math_call("min(50%, 80%)", &f), Some(Length::Percent(50.0)));
    // Mixed px with percent is unorderable at parse time — drop whole call.
    assert_eq!(eval_math_call("min(50%, 50px)", &f), None);
    assert_eq!(eval_math_call("clamp(10px, 50%, 100px)", &f), None);
    // Nesting goes through the same factor path as calc().
    assert_eq!(
        eval_math_call("min(100px, max(20px, 3 * 10px))", &f),
        Some(Length::Px(30.0))
    );
    // Function names are ASCII-case-insensitive; whitespace is optional.
    assert_eq!(eval_math_call("MIN(100px, 50px)", &f), Some(Length::Px(50.0)));
    assert_eq!(eval_math_call("min( 100px ,50px )", &f), Some(Length::Px(50.0)));
    // clamp arity is exactly three.
    assert_eq!(eval_math_call("clamp(10px, 50px)", &f), None);
    assert_eq!(eval_math_call("clamp(10px, 50px, 100px, 200px)", &f), None);
    // Not a math call at all.
    assert_eq!(eval_math_call("auto", &f), None);
    assert_eq!(eval_math_call("min-content", &f), None);
}

#[test]
fn math_calls_flow_through_declarations_and_sides() {
    let f = FontCtx::default();
    // calc() prefix keeps working through the shared entry.
    assert_eq!(eval_math_call("calc(100px + 50px)", &f), Some(Length::Px(150.0)));
    // Property-level bare call, same posture as calc().
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "width: min(400px, 500px)"));
    assert_eq!(s.width, Some(Length::Px(400.0)));
    // Percent-only call keeps the percent.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "width: min(50%, 80%)"));
    assert_eq!(s.width, Some(Length::Percent(50.0)));
    // Mixed px with percent drops the whole declaration.
    let mut s = ComputedStyle::default();
    assert!(!apply_declarations(&mut s, "width: min(400px, 90%)"));
    assert_eq!(s.width, None);
    // expand_sides keeps call tokens whole across the whitespace split.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "padding: max(4px, 2px) min(8px, 6px)"));
    assert_eq!(s.padding.top, Some(Length::Px(4.0)));
    assert_eq!(s.padding.right, Some(Length::Px(6.0)));
    assert_eq!(s.padding.bottom, Some(Length::Px(4.0)));
    assert_eq!(s.padding.left, Some(Length::Px(6.0)));
    // Shadow lengths go through the px-only view.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "box-shadow: 2px 2px max(4px, 1px) #000"));
    assert_eq!(s.box_shadow.as_ref().map(|sh| sh[0].blur), Some(4.0));
}

#[test]
fn flex_basis_and_gap_length_percentage() {
    // Resolved-value slots (flex-basis, gap) carry lengths and percents
    // (taffy resolves them at layout time); `auto` is flex-basis's
    // initial and stays None, gap has no auto and drops the declaration.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "flex-basis: calc(60px + 40px)"));
    assert_eq!(s.flex_basis, Some(Length::Px(100.0)));
    let mut s = ComputedStyle::default();
    assert_eq!(apply_declarations(&mut s, "flex-basis: auto"), false);
    assert_eq!(s.flex_basis, None);
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "flex-basis: 50%"));
    assert_eq!(s.flex_basis, Some(Length::Percent(50.0)));
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "flex-basis: calc(50% + 10px)"));
    assert_eq!(s.flex_basis, Some(Length::Calc { percent: 50.0, px: 10.0 }));

    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "gap: calc(8px + 4px)"));
    assert_eq!(s.column_gap, Some(Length::Px(12.0)));
    assert_eq!(s.row_gap, Some(Length::Px(12.0)));
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "column-gap: calc(24px - 4px)"));
    assert_eq!(s.column_gap, Some(Length::Px(20.0)));
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "gap: 50% 8px"));
    assert_eq!(s.column_gap, Some(Length::Percent(50.0)));
    assert_eq!(s.row_gap, Some(Length::Px(8.0)));
    let mut s = ComputedStyle::default();
    // Gap grammar has no auto: the whole declaration drops, no partial
    // application.
    assert!(!apply_declarations(&mut s, "gap: auto 8px"));
    assert_eq!(s.column_gap, None);
    assert_eq!(s.row_gap, None);
}

#[test]
fn flex_shorthand_expands_with_shorthand_initials() {
    // css-flexbox-1 §7.1.1: omitted components take the SHORTHAND's
    // initials (1 / 1 / 0%), not the longhands' (0 / 1 / auto) — the
    // deck idiom `flex: 0 0 100vw` needs the third slot (#61).
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "flex: 0 0 400px"));
    assert_eq!(s.flex_grow, Some(0.0));
    assert_eq!(s.flex_shrink, Some(0.0));
    assert_eq!(s.flex_basis, Some(Length::Px(400.0)));

    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "flex: 1"));
    assert_eq!(s.flex_grow, Some(1.0));
    assert_eq!(s.flex_shrink, Some(1.0));
    // 0% — NOT auto: equal-grow cards align regardless of content.
    assert_eq!(s.flex_basis, Some(Length::Percent(0.0)));

    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "flex: 2 3"));
    assert_eq!(s.flex_grow, Some(2.0));
    assert_eq!(s.flex_shrink, Some(3.0));
    assert_eq!(s.flex_basis, Some(Length::Percent(0.0)));

    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "flex: 0 0 100vw"));
    // vw folds against the default 1280px ICB at parse time (batch 162).
    assert_eq!(s.flex_grow, Some(0.0));
    assert_eq!(s.flex_shrink, Some(0.0));
    assert_eq!(s.flex_basis, Some(Length::Px(1280.0)));

    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "flex: 1 1 auto"));
    assert_eq!(s.flex_grow, Some(1.0));
    assert_eq!(s.flex_shrink, Some(1.0));
    // Explicit auto survives — never swapped for the 0% initial.
    assert_eq!(s.flex_basis, None);

    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "flex: 0 auto"));
    assert_eq!(s.flex_grow, Some(0.0));
    assert_eq!(s.flex_shrink, Some(1.0));
    assert_eq!(s.flex_basis, None);

    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "flex: none"));
    assert_eq!(s.flex_grow, Some(0.0));
    assert_eq!(s.flex_shrink, Some(0.0));
    assert_eq!(s.flex_basis, None);

    // `||` order freedom: basis before the numbers is valid CSS.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "flex: 100px 2"));
    assert_eq!(s.flex_grow, Some(2.0));
    assert_eq!(s.flex_basis, Some(Length::Px(100.0)));

    // The shorthand RESETS earlier longhand writes (all three slots).
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "flex-basis: 200px"));
    assert!(apply_declarations(&mut s, "flex-grow: 5"));
    assert!(apply_declarations(&mut s, "flex: 1"));
    assert_eq!(s.flex_grow, Some(1.0));
    assert_eq!(s.flex_basis, Some(Length::Percent(0.0)));

    // Junk drops the whole declaration: no partial application.
    let mut s = ComputedStyle::default();
    assert!(!apply_declarations(&mut s, "flex: 1 2 3"));
    assert_eq!(s.flex_grow, None);
    assert!(!apply_declarations(&mut s, "flex: 10px"));
}

#[test]
fn box_sizing_parses_both_keywords() {
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "box-sizing: border-box"));
    assert_eq!(s.box_sizing, Some(BoxSizing::BorderBox));
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "box-sizing: content-box"));
    assert_eq!(s.box_sizing, Some(BoxSizing::ContentBox));
    // Undeclared stays None (the CSS initial content-box applies downstream).
    assert_eq!(ComputedStyle::default().box_sizing, None);
    // Unknown keywords drop the declaration like any invalid value.
    let mut s = ComputedStyle::default();
    assert!(!apply_declarations(&mut s, "box-sizing: padding-box"));
    assert_eq!(s.box_sizing, None);
}

#[test]
fn background_clip_parses_text_and_box_keywords() {
    // The -webkit- alias is what real pages ship; it must land in the
    // same field.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "-webkit-background-clip: text"));
    assert!(s.background_clip_text);
    // Box keywords are accepted and modeled as the initial box fill.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "background-clip: border-box"));
    assert!(!s.background_clip_text);
    assert_eq!(ComputedStyle::default().background_clip_text, false);
    let mut s = ComputedStyle::default();
    assert!(!apply_declarations(&mut s, "background-clip: no-box"));
    assert!(!s.background_clip_text);
}

// ---- CSS animation: shorthand, keyframes, sampler ----

#[test]
fn animation_shorthand_time_order_keywords_and_bezier() {
    let a = parse_animation_shorthand("agxIn 1.2s cubic-bezier(.2,.7,.2,1) .35s forwards").unwrap();
    assert_eq!(a.name, "agxIn");
    assert_eq!(a.duration, 1.2);
    assert_eq!(a.delay, 0.35);
    assert!(a.fill_forwards);
    assert_eq!(a.easing, Easing::CubicBezier(0.2, 0.7, 0.2, 1.0));

    // First time token is duration, second is delay (CSS order).
    let a = parse_animation_shorthand(".5s 2s agxDraw linear").unwrap();
    assert_eq!((a.duration, a.delay), (0.5, 2.0));
    assert_eq!(a.easing, Easing::Linear);

    // Keywords map to canonical beziers; `both` counts as forwards.
    let a = parse_animation_shorthand("ease-in-out .8s both x").unwrap();
    assert_eq!(a.easing, Easing::CubicBezier(0.42, 0.0, 0.58, 1.0));
    assert!(a.fill_forwards);

    // ms suffix, defaults (duration 0 = snap, no fill), `none` kills it.
    let a = parse_animation_shorthand("400ms agxFade").unwrap();
    assert_eq!(a.duration, 0.4);
    assert_eq!(a.delay, 0.0);
    assert!(!a.fill_forwards);
    assert_eq!(parse_animation_shorthand("none"), None);
    assert_eq!(parse_animation_shorthand("1s steps(3) x"), None);
}

#[test]
fn keyframes_table_parses_stops_offsets_and_percent_selectors() {
    let css = "@keyframes agxIn { from { opacity: 0 } 40% { opacity: .4 } to { opacity: 1 } } \
               .a { animation: agxIn 1s }";
    let (rules, kf) = parse_stylesheet_timed(css, (1280.0, 720.0), CssMediaType::Screen);
    assert_eq!(rules.len(), 1, "{rules:?}");
    let kf = kf.get("agxIn").expect("keyframes captured");
    let offsets: Vec<f32> = kf.stops.iter().map(|s| s.offset).collect();
    assert_eq!(offsets, vec![0.0, 0.4, 1.0]);
    assert!(kf.stops[2].decls.iter().any(|(k, v)| k == "opacity" && v.trim() == "1"));
    // Unsorted source still lands sorted.
    let (_, kf) = parse_stylesheet_timed(
        "@keyframes r { to { opacity: 1 } from { opacity: 0 } }",
        (1280.0, 720.0),
        CssMediaType::Screen,
    );
    let kf = kf.get("r").unwrap();
    assert!(kf.stops[0].offset < kf.stops[1].offset);
    // Names are case-sensitive and separate entries.
    let (_, kf) = parse_stylesheet_timed(
        "@keyframes A { from { opacity: 0 } } @keyframes a { from { opacity: 1 } }",
        (1280.0, 720.0),
        CssMediaType::Screen,
    );
    assert_eq!(kf.len(), 2);
}

/// Style with `animation: name dur [delay] [fill]` on it, ready for
/// sampling against `keyframes`.
fn animated_style(decls: &str) -> ComputedStyle {
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, decls), "declaration parse");
    s
}

fn one_keyframes(body: &str) -> KeyframesMap {
    let (_, kf) = parse_stylesheet_timed(
        &format!("@keyframes k {{ {body} }}"),
        (1280.0, 720.0),
        CssMediaType::Screen,
    );
    assert_eq!(kf.len(), 1, "keyframes body must parse: {body}");
    kf
}

#[test]
fn sampler_before_delay_holds_underlying_value() {
    let kf = one_keyframes("from { opacity: 0 } to { opacity: 1 }");
    // Cascade 1 vs from-stop 0 — the boundary only shows when the two
    // differ (an underlying 0 would pass vacuously either way).
    let mut s = animated_style("opacity: 1; animation: k 2s 1s");
    sample_css_animation(&mut s, &kf, Some(0.5));
    assert_eq!(s.opacity, Some(1.0), "delay window shows cascade value");
    // Exactly at delay start = first frame of the active phase: the
    // from-stop applies, not the cascade value.
    sample_css_animation(&mut s, &kf, Some(1.0));
    assert_eq!(s.opacity, Some(0.0));
}

#[test]
fn sampler_forwards_holds_to_stop_and_none_reverts() {
    let kf = one_keyframes("from { opacity: 0 } to { opacity: 1 }");
    let mut fwd = animated_style("opacity: 0; animation: k 1s forwards");
    sample_css_animation(&mut fwd, &kf, Some(5.0));
    assert_eq!(fwd.opacity, Some(1.0), "fill: forwards pins the to stop");
    // t=None (static render) samples the same end state.
    sample_css_animation(&mut fwd, &kf, None);
    assert_eq!(fwd.opacity, Some(1.0));

    let mut nofill = animated_style("opacity: 0; animation: k 1s");
    sample_css_animation(&mut nofill, &kf, Some(5.0));
    assert_eq!(nofill.opacity, Some(0.0), "fill: none falls back to cascade");
}

#[test]
fn sampler_interpolates_mid_flight_with_linear_and_bezier() {
    let kf = one_keyframes("from { opacity: 0 } to { opacity: 1 }");
    let mut s = animated_style("opacity: 0; animation: k 2s linear");
    sample_css_animation(&mut s, &kf, Some(1.0));
    assert!((s.opacity.unwrap() - 0.5).abs() < 1e-4);

    // ease-in-out at p=.5 is symmetric around the diagonal: y(.5)=.5.
    let mut s = animated_style("opacity: 0; animation: k 2s ease-in-out");
    sample_css_animation(&mut s, &kf, Some(1.0));
    assert!((s.opacity.unwrap() - 0.5).abs() < 1e-3);
}

#[test]
fn transition_shorthand_and_longhands_parse() {
    let s = animated_style("transition: opacity 0.3s ease 0.2s");
    let t = s.transition.as_ref().expect("shorthand parses");
    assert_eq!(t.property.as_deref(), Some("opacity"));
    assert!((t.duration - 0.3).abs() < 1e-5);
    assert!((t.delay - 0.2).abs() < 1e-5, "second time token is the delay");
    assert_eq!(t.easing, Easing::CubicBezier(0.25, 0.1, 0.25, 1.0));

    // ms folds to seconds; `all` is a property token like any other.
    let t = animated_style("transition: color 200ms linear")
        .transition
        .expect("ms shorthand parses");
    assert_eq!(t.property.as_deref(), Some("color"));
    assert!((t.duration - 0.2).abs() < 1e-5);
    assert_eq!(t.delay, 0.0);
    assert_eq!(t.easing, Easing::Linear);

    // `none` disables outright; a curve we don't evaluate kills the
    // whole declaration (the shorthand arm reports the decl rejected).
    let mut s = ComputedStyle::default();
    assert!(!apply_declarations(&mut s, "transition: none"));
    assert!(s.transition.is_none());
    let mut s = ComputedStyle::default();
    assert!(!apply_declarations(&mut s, "transition: opacity 1s steps(4)"));
    assert!(s.transition.is_none());

    // Longhands re-create the spec with the initial values for the
    // parts they don't carry (property "all", 0s delay, `ease`).
    let s = animated_style(
        "transition-property: background-color; transition-duration: 1.5s",
    );
    let t = s.transition.as_ref().expect("longhands build the spec");
    assert_eq!(t.property.as_deref(), Some("background-color"));
    assert!((t.duration - 1.5).abs() < 1e-5);
    assert_eq!(t.delay, 0.0);
    // `none` longhand drops the spec entirely.
    let mut s2 = animated_style("transition: opacity 1s");
    assert!(apply_declarations(&mut s2, "transition-property: none"));
    assert!(s2.transition.is_none());
}

#[test]
fn transition_sampler_overrides_values_along_the_clock() {
    let mut list = vec![CssTransition {
        nid: 7,
        property: "opacity".into(),
        from: TransitionValue::Opacity(0.0),
        to: TransitionValue::Opacity(1.0),
        start: 0.0,
        duration: 1.0,
        delay: 0.0,
        easing: Easing::Linear,
    }];
    // A registration pointing at a detached node (99) is skipped, not
    // a panic — the page can drop the element between the register op
    // and the next style pass.
    let stale = list[0].clone();
    list.push(CssTransition { nid: 99, ..stale });

    let mut styles = std::collections::HashMap::new();
    let mut cs = ComputedStyle::default();
    cs.opacity = Some(1.0);
    styles.insert(crate::diting_dom::NodeId(7), cs);

    sample_css_transitions(&list, None, &mut styles);
    assert_eq!(
        styles[&crate::diting_dom::NodeId(7)].opacity,
        Some(1.0),
        "t=None is the live path: the style write already carries the end state"
    );

    sample_css_transitions(&list, Some(0.5), &mut styles);
    assert!((styles[&crate::diting_dom::NodeId(7)].opacity.unwrap() - 0.5).abs() < 1e-4);
    sample_css_transitions(&list, Some(9.0), &mut styles);
    assert_eq!(
        styles[&crate::diting_dom::NodeId(7)].opacity,
        Some(1.0),
        "past the active window clamps at `to`"
    );

    // The delay window holds the from value; a nonzero start shifts it.
    let mut delayed = list[0].clone();
    delayed.delay = 0.5;
    let mut styles = std::collections::HashMap::new();
    styles.insert(crate::diting_dom::NodeId(7), ComputedStyle::default());
    sample_css_transitions(&[delayed], Some(0.25), &mut styles);
    assert_eq!(styles[&crate::diting_dom::NodeId(7)].opacity, Some(0.0));

    let mut shifted = list[0].clone();
    shifted.start = 2.0;
    sample_css_transitions(&[shifted], Some(2.5), &mut styles);
    assert!((styles[&crate::diting_dom::NodeId(7)].opacity.unwrap() - 0.5).abs() < 1e-4);
}

#[test]
fn transition_sampler_lerps_colors_and_kind_mismatch_keeps_cascade() {
    let base = CssTransition {
        nid: 3,
        property: "color".into(),
        from: TransitionValue::Color([255.0, 0.0, 0.0, 255.0]),
        to: TransitionValue::Color([0.0, 0.0, 255.0, 255.0]),
        start: 0.0,
        duration: 1.0,
        delay: 0.0,
        easing: Easing::Linear,
    };
    let mut styles = std::collections::HashMap::new();
    styles.insert(crate::diting_dom::NodeId(3), ComputedStyle::default());
    sample_css_transitions(&[base.clone()], Some(0.5), &mut styles);
    let c = styles[&crate::diting_dom::NodeId(3)]
        .color
        .expect("color overridden");
    assert_eq!(
        (c.0, c.1, c.2, c.3),
        (128, 0, 128, 255),
        "red→blue midpoint is purple (127.5 rounds to 128)"
    );

    // A from/to whose kind doesn't match the property name is never
    // produced by the register op; if it ever shows up the sampler
    // falls through and the cascade value stands.
    let bad = CssTransition {
        from: TransitionValue::Opacity(1.0),
        to: TransitionValue::Opacity(0.0),
        ..base
    };
    let mut cs = ComputedStyle::default();
    cs.color = Some(Color(9, 9, 9, 255));
    let mut styles = std::collections::HashMap::new();
    styles.insert(crate::diting_dom::NodeId(3), cs);
    sample_css_transitions(&[bad], Some(0.5), &mut styles);
    assert_eq!(
        styles[&crate::diting_dom::NodeId(3)].color,
        Some(Color(9, 9, 9, 255))
    );
}

#[test]
fn transition_sampler_lerps_transform_components() {
    let mk = |from: Option<Transform2D>, to: Option<Transform2D>| CssTransition {
        nid: 5,
        property: "transform".into(),
        from: TransitionValue::Transform(from),
        to: TransitionValue::Transform(to),
        start: 0.0,
        duration: 1.0,
        delay: 0.0,
        easing: Easing::Linear,
    };
    let mut styles = std::collections::HashMap::new();
    styles.insert(crate::diting_dom::NodeId(5), ComputedStyle::default());

    // Two full affines lerp componentwise, translate included.
    let a = parse_transform("translate(200px, 40px) scale(2)").unwrap();
    let b = parse_transform("translate(0px, 0px)").unwrap();
    sample_css_transitions(&[mk(Some(a), Some(b))], Some(0.5), &mut styles);
    let t = styles[&crate::diting_dom::NodeId(5)]
        .transform
        .expect("transform overridden");
    match (t.tx, t.ty, t.a) {
        (Length::Px(tx), Length::Px(ty), a) => {
            assert!((tx - 100.0).abs() < 1e-3 && (ty - 20.0).abs() < 1e-3);
            assert!((a - 1.5).abs() < 1e-4, "scale lerps 2→1 midpoint 1.5");
        }
        _ => panic!("expected px translate and scale"),
    }

    // `none` on a side is identity for the mix (computed snapshot of an
    // untransformed element), so none→matrix still interpolates.
    let to = parse_transform("translate(100px, 0px)").unwrap();
    let mut styles = std::collections::HashMap::new();
    styles.insert(crate::diting_dom::NodeId(5), ComputedStyle::default());
    sample_css_transitions(&[mk(None, Some(to))], Some(0.25), &mut styles);
    match styles[&crate::diting_dom::NodeId(5)].transform.expect("none→T mix").tx {
        Length::Px(tx) => assert!((tx - 25.0).abs() < 1e-3),
        _ => panic!("expected px"),
    }

    // A property/value kind mismatch (never produced by the register
    // op) falls through and the cascade transform stands.
    let bad = CssTransition {
        property: "transform".into(),
        from: TransitionValue::Opacity(1.0),
        to: TransitionValue::Opacity(0.0),
        ..mk(None, None)
    };
    let mut styles = std::collections::HashMap::new();
    styles.insert(crate::diting_dom::NodeId(5), ComputedStyle::default());
    sample_css_transitions(&[bad], Some(0.5), &mut styles);
    let cs = &styles[&crate::diting_dom::NodeId(5)];
    assert!(cs.transform.is_none(), "mismatch keeps cascade");
}

#[test]
fn sampler_multi_iteration_cycles_and_fills() {
    let kf = one_keyframes("from { opacity: 0 } to { opacity: 1 }");
    // Mid-second-cycle of a 3-count run: progress is cycle-local, so
    // 25% into cycle 2 samples like 25% into cycle 1.
    let mut s = animated_style("animation: k 2s 3 linear");
    sample_css_animation(&mut s, &kf, Some(2.5));
    assert!((s.opacity.unwrap() - 0.25).abs() < 1e-4);

    // Easing restarts each cycle: `ease` at cycle-local 0 maps to 0.
    let mut s = animated_style("animation: k 2s 2 ease");
    sample_css_animation(&mut s, &kf, Some(2.0));
    assert!(s.opacity.unwrap() < 1e-3, "cycle 2 opens at the from stop");

    // Exactly at the end of the active duration without fill reverts.
    let mut s = animated_style("opacity: 0.5; animation: k 2s 2 linear");
    sample_css_animation(&mut s, &kf, Some(4.0));
    assert_eq!(s.opacity, Some(0.5), "active duration over, no fill: cascade");

    // Whole-count forwards fill holds the to stop.
    let mut s = animated_style("animation: k 2s 3 linear forwards");
    sample_css_animation(&mut s, &kf, Some(100.0));
    assert_eq!(s.opacity, Some(1.0));

    // Fractional count (2.5) freezes mid-flight at the fractional part.
    let mut s = animated_style("animation: k 2s 2.5 linear forwards");
    sample_css_animation(&mut s, &kf, Some(100.0));
    assert!((s.opacity.unwrap() - 0.5).abs() < 1e-4);

    // Zero count never enters the active phase: fill holds the from stop.
    let mut s = animated_style("opacity: 1; animation: k 2s 0 forwards");
    sample_css_animation(&mut s, &kf, Some(100.0));
    assert_eq!(s.opacity, Some(0.0));

    // Infinite keeps cycling no matter how far out t goes.
    let mut s = animated_style("animation: k 2s infinite linear");
    sample_css_animation(&mut s, &kf, Some(2.0 * 7.0 + 1.5));
    assert!((s.opacity.unwrap() - 0.75).abs() < 1e-4);
}

#[test]
fn sampler_substitutes_custom_properties_in_stops() {
    // The generated declarative SVGs write `to { opacity: var(--o) }`.
    let kf = one_keyframes("from { opacity: 0 } to { opacity: var(--o) }");
    // No easing token = CSS default `ease`, so pin linear for exact math.
    let mut s = animated_style("--o: 0.8; opacity: 0; animation: k 1s linear forwards");
    sample_css_animation(&mut s, &kf, Some(0.5));
    assert!((s.opacity.unwrap() - 0.4).abs() < 1e-4, "var(--o) resolved mid-lerp");
    sample_css_animation(&mut s, &kf, None);
    assert!((s.opacity.unwrap() - 0.8).abs() < 1e-4);
}

#[test]
fn sampler_lerps_transform_and_treats_none_as_identity() {
    let kf = one_keyframes("from { transform: translateY(40px) } to { transform: none }");
    let mut s = animated_style("animation: k 1s linear");
    sample_css_animation(&mut s, &kf, Some(0.5));
    let t = s.transform.as_ref().expect("transform applied");
    let m = t.to_matrix_with(0.0, 0.0);
    assert!((m[5] - 20.0).abs() < 1e-3, "translateY 40→0 at midpoint: {m:?}");
    assert_eq!(m[0], 1.0);

    // Missing transform in a stop falls back to the underlying cascade
    // transform (missing-keyframe semantics), not to identity.
    let kf = one_keyframes("50% { opacity: 1 } to { transform: translateX(10px) }");
    let mut s = animated_style("transform: translateX(30px); animation: k 2s linear");
    sample_css_animation(&mut s, &kf, Some(0.25));
    // 0→50% span: both stops lack transform → underlying stays.
    let m = s.transform.as_ref().unwrap().to_matrix_with(0.0, 0.0);
    assert!((m[4] - 30.0).abs() < 1e-3);
}

#[test]
fn sampler_stroke_dashoffset_self_draw_grammar() {
    // Self-draw: dasharray 1 + pathLength 1, offset 1→0.
    let kf = one_keyframes("from { stroke-dashoffset: 1 } to { stroke-dashoffset: 0 }");
    let mut s = animated_style("stroke-dasharray: 1; stroke-dashoffset: 1; animation: k 1s linear forwards");
    sample_css_animation(&mut s, &kf, Some(0.25));
    assert!((s.svg_dashoffset.unwrap() - 0.75).abs() < 1e-4);
    sample_css_animation(&mut s, &kf, None);
    assert_eq!(s.svg_dashoffset, Some(0.0), "static render shows the drawn state");
}

#[test]
fn bezier_easing_endpoints_and_monotone_interior() {
    let e = Easing::CubicBezier(0.25, 0.1, 0.25, 1.0);
    assert!((e.map(0.0) - 0.0).abs() < 1e-5);
    assert!((e.map(1.0) - 1.0).abs() < 1e-5);
    // Monotone curve: 100 interior samples never decrease.
    let mut prev = 0.0f32;
    for i in 1..100 {
        let y = e.map(i as f32 / 100.0);
        assert!(y >= prev - 1e-5, "non-monotone at {i}: {prev} → {y}");
        prev = y;
    }
    // Degenerate control points (x flat in a region) must still solve via
    // the bisection fallback rather than NaN out.
    let flat = Easing::CubicBezier(0.0, 0.0, 1.0, 1.0);
    let y = flat.map(0.37);
    assert!(y.is_finite() && (0.0..=1.0).contains(&y));
}

#[test]
fn to_matrix_with_resolves_percent_translation_against_reference() {
    let t = parse_transform("translate(10%, 25%)").unwrap();
    let m = t.to_matrix_with(200.0, 100.0);
    assert!((m[4] - 20.0).abs() < 1e-4, "10% of 200");
    assert!((m[5] - 25.0).abs() < 1e-4, "25% of 100");
}

/// white-space parses the full normal/nowrap/pre family, text-overflow
/// parses clip/ellipsis; white-space inherits, text-overflow does not
/// (CSS UI §5.2).
#[test]
fn white_space_and_text_overflow_parse_and_inherit() {
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "white-space: nowrap; text-overflow: ellipsis");
    assert_eq!(s.white_space, Some(WhiteSpace::Nowrap));
    assert_eq!(s.text_overflow, Some(TextOverflow::Ellipsis));

    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "white-space: normal; text-overflow: clip");
    assert_eq!(s.white_space, Some(WhiteSpace::Normal));
    assert_eq!(s.text_overflow, Some(TextOverflow::Clip));

    // The preserve modes all parse (the pre family — batch 106).
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "white-space: pre");
    assert_eq!(s.white_space, Some(WhiteSpace::Pre));
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "white-space: pre-wrap");
    assert_eq!(s.white_space, Some(WhiteSpace::PreWrap));
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "white-space: pre-line");
    assert_eq!(s.white_space, Some(WhiteSpace::PreLine));
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "white-space: break-spaces");
    assert_eq!(s.white_space, Some(WhiteSpace::BreakSpaces));

    // Unknown values drop the whole declaration.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "white-space: nowrapish; text-overflow: '…'");
    assert_eq!(s.white_space, None);
    assert_eq!(s.text_overflow, None);

    let tree = diting_dom::tree_sink::parse_html(r#"<div><p>x</p></div>"#);
    let p = tree.query_selector("p").unwrap().unwrap();
    let parent = ComputedStyle {
        white_space: Some(WhiteSpace::Nowrap),
        text_overflow: Some(TextOverflow::Ellipsis),
        ..Default::default()
    };
    let child = cascade_element("p", &tree, p, &[], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(child.white_space, Some(WhiteSpace::Nowrap), "white-space inherits");
    assert_eq!(child.text_overflow, None, "text-overflow does not inherit");
}

/// The UA stylesheet gives pre/xmp/listing/plaintext `white-space: pre`,
/// filled only when neither inheritance nor an author declaration
/// provides one.
#[cfg(feature = "screenshot")]
#[test]
fn ua_pre_default_and_author_override() {
    let tree = diting_dom::tree_sink::parse_html("<pre>x</pre>");
    let pre = tree.query_selector("pre").unwrap().unwrap();
    let ua = crate::diting_layout::compute_styles(&tree, &[], (1280.0, 720.0));
    assert_eq!(ua[&pre].white_space, Some(WhiteSpace::Pre), "UA default for <pre>");

    let rules = parse_stylesheet_for("pre { white-space: normal }", (800.0, 600.0), CssMediaType::Screen);
    let authored = crate::diting_layout::compute_styles(&tree, &rules, (1280.0, 720.0));
    assert_eq!(authored[&pre].white_space, Some(WhiteSpace::Normal), "author declaration beats the UA default");
}

// ---- box-shadow (blitz#349 family, v1) ----

fn shadow(v: &str) -> Option<Vec<BoxShadow>> {
    let mut s = ComputedStyle::default();
    s.color = Some(Color(10, 20, 30, 255));
    apply_declarations(&mut s, &format!("box-shadow: {v}"));
    s.box_shadow
}

#[test]
fn box_shadow_two_lengths_fold_current_color() {
    let layers = shadow("2px 3px").unwrap();
    assert_eq!(
        layers,
        vec![BoxShadow { dx: 2.0, dy: 3.0, blur: 0.0, spread: 0.0, color: Color(10, 20, 30, 255), inset: false }],
    );
}

#[test]
fn box_shadow_four_lengths_and_color_on_either_side() {
    for v in ["red 4px 5px 6px 7px", "4px 5px 6px 7px red"] {
        assert_eq!(
            shadow(v).unwrap(),
            vec![BoxShadow { dx: 4.0, dy: 5.0, blur: 6.0, spread: 7.0, color: Color(255, 0, 0, 255), inset: false }],
            "{v}"
        );
    }
}

#[test]
fn box_shadow_layers_and_inset_parse() {
    let layers = shadow("2px 2px rgba(0, 0, 0, 0.5), inset 0 1px red").unwrap();
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0].inset, false);
    assert_eq!(layers[0].color, Color(0, 0, 0, 128));
    assert_eq!(layers[1], BoxShadow { dx: 0.0, dy: 1.0, blur: 0.0, spread: 0.0, color: Color(255, 0, 0, 255), inset: true });
}

#[test]
fn box_shadow_rejections_and_reset() {
    assert!(shadow("50% 50%").is_none(), "% needs the receiver box");
    assert!(shadow("2px 2px -1px").is_none(), "negative blur invalid");
    assert!(shadow("red").is_none(), "one length is not a shadow");
    assert!(shadow("1px 1px 2px 3px 4px").is_none(), "five lengths");
    assert!(shadow("1px 1px red red").is_none(), "two colors");
    assert_eq!(shadow("none"), None);
    // `none` clears a prior value; an invalid re-declaration must not.
    let mut s = ComputedStyle::default();
    s.color = Some(Color(0, 0, 0, 255));
    apply_declarations(&mut s, "box-shadow: 1px 1px red; box-shadow: none");
    assert_eq!(s.box_shadow, None, "none clears");
    apply_declarations(&mut s, "box-shadow: 1px 1px red; box-shadow: blue blue");
    assert!(s.box_shadow.is_some(), "invalid re-declaration keeps the prior value");
}

// ---- backdrop-filter (blitz#901 family) ----

fn backdrop(v: &str) -> Option<f32> {
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, &format!("backdrop-filter: {v}"));
    s.backdrop_blur
}

#[test]
fn backdrop_filter_blur_only_v1() {
    assert_eq!(backdrop("blur(12px)"), Some(12.0));
    assert_eq!(backdrop("BLUR(2.5px)"), Some(2.5), "function name case-insensitive");
    assert!(backdrop("blur(0.5em)").is_some(), "em resolves via fonts");
    assert_eq!(backdrop("none"), None);
}

#[test]
fn backdrop_filter_rejections_and_reset() {
    assert!(backdrop("brightness(0.5)").is_none(), "v1 is blur-only");
    assert!(backdrop("blur(4px) blur(4px)").is_none(), "no filter lists in v1");
    assert!(backdrop("blur(-3px)").is_none(), "negative blur invalid");
    assert!(backdrop("blur(50%)").is_none(), "% needs the receiver box");
    assert!(backdrop("blur").is_none(), "missing arguments");
    // `none` clears a prior value; an invalid re-declaration must not.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "backdrop-filter: blur(8px); backdrop-filter: none");
    assert_eq!(s.backdrop_blur, None, "none clears");
    apply_declarations(&mut s, "backdrop-filter: blur(8px); backdrop-filter: invert(1)");
    assert_eq!(s.backdrop_blur, Some(8.0), "invalid re-declaration keeps the prior value");
}

#[test]
fn backdrop_filter_does_not_inherit() {
    let tree = diting_dom::tree_sink::parse_html(r#"<div><p>x</p></div>"#);
    let p = tree.query_selector("p").unwrap().unwrap();
    let parent = ComputedStyle {
        backdrop_blur: Some(6.0),
        ..Default::default()
    };
    let child = cascade_element("p", &tree, p, &[], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(child.backdrop_blur, None, "backdrop-filter is non-inherited");
}

// ---- text-shadow (blitz#271 family) ----

fn tshadow(v: &str) -> Option<Vec<TextShadow>> {
    let mut s = ComputedStyle::default();
    s.color = Some(Color(10, 20, 30, 255));
    apply_declarations(&mut s, &format!("text-shadow: {v}"));
    s.text_shadow
}

#[test]
fn text_shadow_two_lengths_fold_current_color() {
    assert_eq!(
        tshadow("2px 3px").unwrap(),
        vec![TextShadow { dx: 2.0, dy: 3.0, blur: 0.0, color: Color(10, 20, 30, 255) }],
    );
}

#[test]
fn text_shadow_color_on_either_side_and_blur() {
    for v in ["red 4px 5px 6px", "4px 5px 6px red"] {
        assert_eq!(
            tshadow(v).unwrap(),
            vec![TextShadow { dx: 4.0, dy: 5.0, blur: 6.0, color: Color(255, 0, 0, 255) }],
            "{v}"
        );
    }
}

#[test]
fn text_shadow_layers_parse_in_order() {
    let layers = tshadow("1px 1px 2px red, 0 0 4px rgba(0, 0, 0, 0.5)").unwrap();
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0], TextShadow { dx: 1.0, dy: 1.0, blur: 2.0, color: Color(255, 0, 0, 255) });
    assert_eq!(layers[1], TextShadow { dx: 0.0, dy: 0.0, blur: 4.0, color: Color(0, 0, 0, 128) });
}

#[test]
fn text_shadow_rejections_and_reset() {
    assert!(tshadow("inset 1px 1px").is_none(), "inset is not a text-shadow keyword");
    assert!(tshadow("red").is_none(), "one length is not a shadow");
    assert!(tshadow("2px 2px -1px").is_none(), "negative blur invalid");
    assert!(tshadow("1px 1px 2px 3px").is_none(), "no spread length");
    assert!(tshadow("red 1px 1px red").is_none(), "two colors");
    assert_eq!(tshadow("none"), None);
    // `none` clears a prior value; an invalid re-declaration must not.
    let mut s = ComputedStyle::default();
    s.color = Some(Color(0, 0, 0, 255));
    apply_declarations(&mut s, "text-shadow: 1px 1px red; text-shadow: none");
    assert_eq!(s.text_shadow, None, "none clears");
    apply_declarations(&mut s, "text-shadow: 1px 1px red; text-shadow: blue blue");
    assert!(s.text_shadow.is_some(), "invalid re-declaration keeps the prior value");
}

#[test]
fn text_shadow_inherits_like_color() {
    let tree = diting_dom::tree_sink::parse_html(r#"<div><p>x</p></div>"#);
    let p = tree.query_selector("p").unwrap().unwrap();
    let parent = ComputedStyle {
        text_shadow: Some(vec![TextShadow { dx: 1.0, dy: 1.0, blur: 2.0, color: Color(9, 9, 9, 255) }]),
        ..Default::default()
    };
    let child = cascade_element("p", &tree, p, &[], Some(&parent), None, DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(child.text_shadow, parent.text_shadow, "text-shadow inherits");
    // An explicit `none` on the child stops the inheritance.
    let tree2 = diting_dom::tree_sink::parse_html(r#"<p>x</p>"#);
    let p2 = tree2.query_selector("p").unwrap().unwrap();
    let child2 = cascade_element("p", &tree2, p2, &[], Some(&parent), Some("text-shadow: none"), DEFAULT_ROOT_FONT_SIZE, (1280.0, 720.0));
    assert_eq!(child2.text_shadow, None, "author `none` beats inheritance");
}

#[test]
fn overflow_shorthand_and_longhands() {
    // Single-keyword shorthand writes both axes.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "overflow: hidden"));
    assert_eq!(s.overflow_x, Some(Overflow::Hidden));
    assert_eq!(s.overflow_y, Some(Overflow::Hidden));

    // Two-value form: first token = x, second = y (css-overflow-3 §3.1).
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "overflow: hidden auto"));
    assert_eq!(s.overflow_x, Some(Overflow::Hidden));
    assert_eq!(s.overflow_y, Some(Overflow::Auto));

    // Longhands survive shorthand clobbering and partial declarations.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "overflow-x: clip"));
    assert_eq!(s.overflow_x, Some(Overflow::Clip));
    assert_eq!(s.overflow_y, None, "the other axis stays undeclared");
    assert!(apply_declarations(&mut s, "overflow: scroll"));
    assert_eq!(
        (s.overflow_x, s.overflow_y),
        (Some(Overflow::Scroll), Some(Overflow::Scroll)),
        "shorthand clobbers prior longhands like a real shorthand"
    );

    // Bogus forms drop the whole declaration.
    let mut s = ComputedStyle::default();
    assert!(!apply_declarations(&mut s, "overflow: hidden auto scroll"));
    assert!(!apply_declarations(&mut s, "overflow: elbow"));
    assert!(!apply_declarations(&mut s, "overflow-x: elbow"));
    assert_eq!(s, ComputedStyle::default());
}

#[test]
fn overflow_pair_coercion_and_merge() {
    // §3.1: visible + non-visible coerces the visible side to auto,
    // and the undeclared axis reads as visible before coercion.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "overflow-x: hidden");
    assert_eq!(s.resolved_overflow(), (Overflow::Hidden, Overflow::Auto));
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "overflow-y: clip");
    assert_eq!(s.resolved_overflow(), (Overflow::Auto, Overflow::Clip));
    // Both visible (or undeclared) stays visible; both non-visible passes through.
    assert_eq!(ComputedStyle::default().resolved_overflow(), (Overflow::Visible, Overflow::Visible));
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "overflow: hidden scroll");
    assert_eq!(s.resolved_overflow(), (Overflow::Hidden, Overflow::Scroll));

    // The coarse merge picks the strongest axis; no declaration = visible.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "overflow: hidden auto");
    assert_eq!(s.effective_overflow(), Overflow::Auto, "the scrollable axis is the strongest");
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "overflow-x: scroll");
    assert_eq!(s.effective_overflow(), Overflow::Scroll);
    assert_eq!(ComputedStyle::default().effective_overflow(), Overflow::Visible);
    // clips_descendants gates the paint clip: clip/hidden count, plain visible doesn't.
    assert!(s.clips_descendants());
    assert!(!ComputedStyle::default().clips_descendants());
}

#[test]
fn overflow_supports_probe_grammar() {
    assert!(supports_declaration("overflow", "hidden"));
    assert!(supports_declaration("overflow", "hidden auto"));
    assert!(supports_declaration("overflow-x", "clip"));
    assert!(supports_declaration("overflow-y", "scroll"));
    assert!(!supports_declaration("overflow", "hidden auto scroll"), "three keywords are invalid");
    assert!(!supports_declaration("overflow-x", "hidden auto"), "longhand takes one keyword");
    assert!(!supports_declaration("overflow", "elbow"));
}

// ---- generated content (::before/::after v1) ----

#[test]
fn parse_content_value_forms() {
    // Plain string, either quote style.
    assert_eq!(
        parse_content_value(r#""• ""#),
        Some(ContentValue::Str("• ".into())),
    );
    assert_eq!(
        parse_content_value("'X'"),
        Some(ContentValue::Str("X".into())),
    );
    // Quoted-empty must produce a box: the clearfix idiom is
    // `content: ""` — only the UNQUOTED empty token is invalid.
    assert_eq!(parse_content_value(r#""""#), Some(ContentValue::Str("".into())));
    // CSS escapes: hex codepoints (\e116 → U+E116, Bootstrap
    // glyphicons) and the one-whitespace terminator (\41 bc → "Abc").
    assert_eq!(
        parse_content_value(r#""\e116""#),
        Some(ContentValue::Str("\u{e116}".into())),
    );
    assert_eq!(
        parse_content_value(r#""\41 bc""#),
        Some(ContentValue::Str("Abc".into())),
    );
    assert_eq!(
        parse_content_value("attr(data-tip)"),
        Some(ContentValue::Attr("data-tip".into())),
    );
    // none/normal and the unquoted empty token produce no box.
    assert_eq!(parse_content_value("none"), None);
    assert_eq!(parse_content_value("normal"), None);
    assert_eq!(parse_content_value(""), None);
    assert_eq!(parse_content_value("unquoted"), None);
    // Counter forms.
    assert_eq!(
        parse_content_value("counter(x)"),
        Some(ContentValue::Counter {
            name: "x".into(),
            style: CounterStyle::Decimal,
        }),
    );
    assert_eq!(
        parse_content_value("counter(item, lower-roman)"),
        Some(ContentValue::Counter {
            name: "item".into(),
            style: CounterStyle::LowerRoman,
        }),
    );
    assert_eq!(
        parse_content_value(r#"counters(sec, ".")"#),
        Some(ContentValue::Counters {
            name: "sec".into(),
            sep: ".".into(),
            style: CounterStyle::Decimal,
        }),
    );
    assert_eq!(
        parse_content_value(r#"counters(x, "-", upper-alpha)"#),
        Some(ContentValue::Counters {
            name: "x".into(),
            sep: "-".into(),
            style: CounterStyle::UpperAlpha,
        }),
    );
    // Quote keywords.
    assert_eq!(parse_content_value("open-quote"), Some(ContentValue::OpenQuote));
    assert_eq!(parse_content_value("Close-Quote"), Some(ContentValue::CloseQuote));
    assert_eq!(
        parse_content_value("no-open-quote"),
        Some(ContentValue::NoQuote { close: false }),
    );
    // A real-world list: mixed strings and functions.
    assert_eq!(
        parse_content_value(r#""§ " counter(sec) ": ""#),
        Some(ContentValue::List(vec![
            ContentValue::Str("§ ".into()),
            ContentValue::Counter { name: "sec".into(), style: CounterStyle::Decimal },
            ContentValue::Str(": ".into()),
        ])),
    );
    // Malformed forms fail closed.
    assert_eq!(parse_content_value("counter()"), None);
    assert_eq!(parse_content_value("attr()"), None);
    assert_eq!(parse_content_value(r#""unterminated"#), None);
    assert_eq!(parse_content_value("url(x)"), None);
}

#[test]
fn counter_modifier_parsing() {
    assert_eq!(parse_counter_modifiers("none", false), Some(vec![]));
    assert_eq!(parse_counter_modifiers("item", true), Some(vec![("item".into(), 1)]));
    assert_eq!(
        parse_counter_modifiers("x 5", false),
        Some(vec![("x".into(), 5)]),
    );
    assert_eq!(
        parse_counter_modifiers("a b 2", true),
        Some(vec![("a".into(), 1), ("b".into(), 2)]),
    );
    assert_eq!(parse_counter_modifiers("a -1", true), Some(vec![("a".into(), -1)]));
    // Malformed: bare integer, trailing junk.
    assert_eq!(parse_counter_modifiers("5", false), None);
    // Bare names at reset are spec-valid (`counter-reset: item` = 0);
    // only increments get the implicit +1.
    assert_eq!(
        parse_counter_modifiers("a b", false),
        Some(vec![("a".into(), 0), ("b".into(), 0)])
    );
    assert_eq!(parse_counter_modifiers("", false), None);
}

#[test]
fn quotes_parsing_and_counter_formatting() {
    assert_eq!(parse_quotes("none"), Some(vec![]));
    assert_eq!(
        parse_quotes(r#""«" "»""#),
        Some(vec!["«".into(), "»".into()]),
    );
    assert_eq!(parse_quotes(r#""«""#), None, "unpaired value drops");
    assert_eq!(
        format_counter_value(4, CounterStyle::LowerRoman),
        "iv",
    );
    assert_eq!(format_counter_value(1994, CounterStyle::UpperRoman), "MCMXCIV");
    assert_eq!(format_counter_value(0, CounterStyle::LowerRoman), "0", "outside range falls back to decimal");
    assert_eq!(format_counter_value(28, CounterStyle::LowerAlpha), "ab");
    assert_eq!(format_counter_value(28, CounterStyle::UpperAlpha), "AB");
    assert_eq!(format_counter_value(7, CounterStyle::DecimalLeadingZero), "07");
    assert_eq!(format_counter_value(-3, CounterStyle::LowerAlpha), "-3");
}

#[test]
fn content_declaration_lands_in_computed_style() {
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, r#"content: "X""#);
    assert_eq!(s.content, Some(ContentValue::Str("X".into())));
    // Default stays unset — the getComputedStyle face maps it to "normal".
    assert_eq!(ComputedStyle::default().content, None);
}

#[test]
fn nonfinite_numbers_drop_declarations() {
    // #129: page JS can compute a NaN and write `style.left = 'NaNpx'`.
    // Rust's float grammar accepts `NaN`/`inf`/`Infinity`; CSS's does not —
    // Chrome drops the whole declaration, and a stored NaN poisons the
    // layout solve (tmall's 选择视频 dialog collapsed to 100x20).
    let mut s = ComputedStyle::default();
    assert!(
        !apply_declarations(&mut s, "left: NaNpx"),
        "NaNpx must be an invalid <length>"
    );
    assert!(!apply_declarations(&mut s, "top: Infinitypx"));
    assert!(!apply_declarations(&mut s, "width: -infpx"));
    // The margin family's arm reports success unconditionally (its None
    // side resolves to 0, same computed value as Chrome's dropped
    // declaration) — so the contract to pin here is that the NaN lands
    // nowhere, not the arm's return value.
    apply_declarations(&mut s, "margin-left: nan px".replace(' ', "").as_str());
    assert_eq!(s, ComputedStyle::default(), "no non-finite value may land in a slot");

    // A finite sibling declaration in the same block still applies.
    let mut s = ComputedStyle::default();
    apply_declarations(&mut s, "left: 40px; top: NaNpx; right: 10px");
    assert_eq!(s.left, Some(Length::Px(40.0)));
    assert_eq!(s.top, None, "the NaN declaration drops, the rest survive");
    assert_eq!(s.right, Some(Length::Px(10.0)));

    // Numbers without units follow the same grammar (z-index: NaN).
    let mut s = ComputedStyle::default();
    assert!(!apply_declarations(&mut s, "z-index: NaN"));
    assert_eq!(s, ComputedStyle::default());

    // `auto` is the one non-length that IS legal on a box offset: it must
    // apply (resetting the slot), unlike a dropped NaN declaration.
    let mut s = ComputedStyle::default();
    assert!(apply_declarations(&mut s, "left: 40px"));
    assert!(apply_declarations(&mut s, "left: auto"));
    assert_eq!(s.left, None, "left: auto resets the slot");
    assert!(!apply_declarations(&mut s, "left: NaNpx"));
    assert_eq!(s.left, None);
}
