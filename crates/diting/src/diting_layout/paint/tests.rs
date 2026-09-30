use super::*;


fn px(c: &Canvas, x: usize, y: usize) -> [u8; 4] {
    let i = (y * c.width + x) * 4;
    c.data[i..i + 4].try_into().unwrap()
}

/// Fill clips at the canvas edge and an opaque fill overwrites.
#[test]
fn fill_rect_clips_and_overwrites() {
    let mut c = Canvas::new_filled(10, 10, [255, 255, 255, 255]);
    c.fill_rect(8, -2, 4, 4, [200, 40, 40, 255]);
    assert_eq!(px(&c, 9, 0), [200, 40, 40, 255], "clipped fill still paints");
    assert_eq!(px(&c, 7, 0), [255, 255, 255, 255], "outside the rect untouched");
    assert_eq!(px(&c, 0, 9), [255, 255, 255, 255], "below the rect untouched");
}

/// blitz#841 transform half: a Text item inside a non-diagonal SetXf
/// bracket carries LOCAL coordinates, so the ink extent fold must map
/// its ink box through the active map — a rotate(90deg) long line
/// contributes its length to the VERTICAL extent, not the horizontal
/// one. `字`×50 at 10px = 500px single-line ink; under [0,1,-1,0,200,0]
/// (p → (200−y, x)) the (10,20,500,12) box maps to x'∈[168,180],
/// y'∈[10,510].
#[test]
fn ink_extent_maps_bracketed_text_through_the_transform() {
    let items = vec![
        PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 200.0, 0.0] },
        PaintItem::Text {
            text: "字".repeat(50),
            font_size: 10.0,
            bold: false,
            color: [0, 0, 0, 255],
            line_height: 12.0,
            x: 10.0,
            y: 20.0,
            wrap_at: 500.0,
            gradient: None,
            decorations: TextDecorations::default(),
            mono: false,
            word_spacing: 0.0,
            truncate_at: None,
            tokens: None,
            ws: WhiteSpace::Normal,
            text_shadow: None,
small_caps: false, han: None },
        PaintItem::ClearXf,
    ];
    let (w, h) = text_ink_extent(&items);
    assert!((w - 180.0).abs() < 0.01, "rotated line must not report its unrotated 510px width, got {w}");
    assert!((h - 510.0).abs() < 0.01, "rotated line's length lands vertically, got {h}");
}

/// ClearXf pops the bracket: a text after the close folds through the
/// identity again (raw x + ink width, the historical behavior).
#[test]
fn ink_extent_clearxf_restores_identity() {
    let plain = |x: f32, y: f32| PaintItem::Text {
        text: "字".repeat(50),
        font_size: 10.0,
        bold: false,
        color: [0, 0, 0, 255],
        line_height: 12.0,
        x,
        y,
        wrap_at: 500.0,
        gradient: None,
        decorations: TextDecorations::default(),
        mono: false,
        word_spacing: 0.0,
        truncate_at: None,
        tokens: None,
        ws: WhiteSpace::Normal,
        text_shadow: None,
small_caps: false, han: None };
    let items = vec![
        PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 200.0, 0.0] },
        plain(10.0, 20.0),
        PaintItem::ClearXf,
        plain(10.0, 20.0),
    ];
    let (w, h) = text_ink_extent(&items);
    assert!((w - 510.0).abs() < 0.01, "post-bracket text is unrotated, got {w}");
    assert!((h - 510.0).abs() < 0.01, "the rotated text still owns the vertical max, got {h}");
}

/// SetXfCanvas cancels the open map (inline-band splice): text inside it
/// folds through the identity, like the paint pass treats it.
#[test]
fn ink_extent_canvas_bracket_resets_the_map() {
    let items = vec![
        PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 200.0, 0.0] },
        PaintItem::SetXfCanvas,
        PaintItem::Text {
            text: "字".repeat(50),
            font_size: 10.0,
            bold: false,
            color: [0, 0, 0, 255],
            line_height: 12.0,
            x: 10.0,
            y: 20.0,
            wrap_at: 500.0,
            gradient: None,
            decorations: TextDecorations::default(),
            mono: false,
            word_spacing: 0.0,
            truncate_at: None,
            tokens: None,
            ws: WhiteSpace::Normal,
            text_shadow: None,
small_caps: false, han: None },
        PaintItem::ClearXf,
        PaintItem::ClearXf,
    ];
    let (w, _h) = text_ink_extent(&items);
    assert!((w - 510.0).abs() < 0.01, "canvas-spliced text ignores the open rotation, got {w}");
}

/// Nested brackets compose outer∘inner (the walk's relative-own maps):
/// translate(100,0) outside rotate(90deg) maps p → (100−y, x).
#[test]
fn ink_extent_nested_brackets_compose() {
    let items = vec![
        PaintItem::SetXf { xf: [1.0, 0.0, 0.0, 1.0, 100.0, 0.0] },
        PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 0.0, 0.0] },
        PaintItem::Text {
            text: "字".repeat(50),
            font_size: 10.0,
            bold: false,
            color: [0, 0, 0, 255],
            line_height: 12.0,
            x: 10.0,
            y: 20.0,
            wrap_at: 500.0,
            gradient: None,
            decorations: TextDecorations::default(),
            mono: false,
            word_spacing: 0.0,
            truncate_at: None,
            tokens: None,
            ws: WhiteSpace::Normal,
            text_shadow: None,
small_caps: false, han: None },
        PaintItem::ClearXf,
        PaintItem::ClearXf,
    ];
    let (w, h) = text_ink_extent(&items);
    assert!((w - 80.0).abs() < 0.01, "T∘R maps the box to x'∈[68,80], got {w}");
    assert!((h - 510.0).abs() < 0.01, "T∘R keeps the length vertical, got {h}");
}

/// background-clip: text (gradient-text batch): a Text item carrying a
/// TextGradient samples the gradient at each glyph pixel's own position —
/// a to-right gradient over the glyph band runs red on the left half of
/// the INK and blue on the right — and ignores its fill color entirely.
#[test]
fn gradient_text_samples_gradient_at_glyph_positions() {
    let items = vec![PaintItem::Text {
        text: "mmmmm".into(),
        font_size: 20.0,
        bold: true,
        // Would be black ink without the gradient: proves the fill color
        // steps aside when the gradient rides the item.
        color: [0, 0, 0, 255],
        line_height: 24.0,
        x: 10.0,
        y: 4.0,
        wrap_at: 400.0,
        gradient: Some(TextGradient {
            // 90deg = to right across [10, 110): t < 0.5 red, t > 0.5
            // blue, hard switchover at the box center x = 60.
            area: crate::diting_layout::Rect { x: 10.0, y: 0.0, width: 100.0, height: 32.0 },
            stops: vec![(0.0, [255, 0, 0, 255]), (1.0, [0, 0, 255, 255])],
            css_deg: 90.0,
        }),
        decorations: TextDecorations::default(),
        mono: false,
        word_spacing: 0.0,
        truncate_at: None,
        tokens: None,
        ws: WhiteSpace::Normal,
        text_shadow: None,
small_caps: false, han: None }];
    let fonts = crate::diting_fonts::font_book();
    let mut c = Canvas::new_filled(120, 40, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut c);

    let mut reds: Vec<usize> = Vec::new();
    let mut blues: Vec<usize> = Vec::new();
    for y in 0..c.height {
        for x in 0..c.width {
            let [r, g, b, a] = px(&c, x, y);
            if a < 64 {
                continue;
            }
            if r > 120 && b < 120 && g < 120 {
                reds.push(x);
            } else if b > 120 && r < 120 && g < 120 {
                blues.push(x);
            }
        }
    }
    assert!(!reds.is_empty() && !blues.is_empty(), "both gradient halves must paint through the glyphs");
    assert!(
        *reds.iter().max().unwrap() < 60,
        "strong red must stay left of the gradient's midpoint; max red x = {}",
        reds.iter().max().unwrap()
    );
    assert!(
        *blues.iter().min().unwrap() >= 60,
        "strong blue must start right of the midpoint; min blue x = {}",
        blues.iter().min().unwrap()
    );
}

/// Axis-aligned gradients land on the right walls: 180° (the CSS
/// default, to bottom) paints red top / blue bottom, 0° flips, 90°
/// (to right) runs left→right. Plateau stops keep the sampled pixels
/// on the exact end colors.
#[test]
fn gradient_axis_directions() {
    let stops = vec![
        (0.0, [255, 0, 0, 255]),
        (0.2, [255, 0, 0, 255]),
        (0.8, [0, 0, 255, 255]),
        (1.0, [0, 0, 255, 255]),
    ];
    let mut c = Canvas::new_filled(10, 10, [255, 255, 255, 255]);
    c.fill_gradient(0, 0, 10, 10, &stops, 180.0, [(0.0, 0.0); 4]);
    assert_eq!(px(&c, 5, 0), [255, 0, 0, 255], "180°: top row at t=0.05 is red");
    assert_eq!(px(&c, 5, 9), [0, 0, 255, 255], "180°: bottom row at t=0.95 is blue");
    assert_eq!(px(&c, 0, 0), px(&c, 9, 0), "180°: rows are column-invariant");

    let mut c = Canvas::new_filled(10, 10, [255, 255, 255, 255]);
    c.fill_gradient(0, 0, 10, 10, &stops, 0.0, [(0.0, 0.0); 4]);
    assert_eq!(px(&c, 5, 0), [0, 0, 255, 255], "0° (to top): top is blue");
    assert_eq!(px(&c, 5, 9), [255, 0, 0, 255], "0°: bottom is red");

    let mut c = Canvas::new_filled(10, 10, [255, 255, 255, 255]);
    c.fill_gradient(0, 0, 10, 10, &stops, 90.0, [(0.0, 0.0); 4]);
    assert_eq!(px(&c, 0, 5), [255, 0, 0, 255], "90° (to right): left is red");
    assert_eq!(px(&c, 9, 5), [0, 0, 255, 255], "90°: right is blue");
}

/// 135° runs corner to corner: TL at t≈0, BR at t≈1, and the two
/// off-diagonal corners project to the gradient-line center (t=0.5,
/// the mid lerp of the plateau ramp).
#[test]
fn gradient_135deg_diagonal() {
    let stops = vec![
        (0.0, [255, 0, 0, 255]),
        (0.2, [255, 0, 0, 255]),
        (0.8, [0, 0, 255, 255]),
        (1.0, [0, 0, 255, 255]),
    ];
    let mut c = Canvas::new_filled(10, 10, [255, 255, 255, 255]);
    c.fill_gradient(0, 0, 10, 10, &stops, 135.0, [(0.0, 0.0); 4]);
    assert_eq!(px(&c, 0, 0), [255, 0, 0, 255], "TL projects to t=0.05");
    assert_eq!(px(&c, 9, 9), [0, 0, 255, 255], "BR projects to t=0.95");
    assert_eq!(px(&c, 9, 0), [128, 0, 128, 255], "TR projects to t=0.5");
    assert_eq!(px(&c, 0, 9), [128, 0, 128, 255], "BL projects to t=0.5");
}

/// The gradient fill follows the rounded box: a full-circle radius
/// leaves the corner pixels untouched while the middle paints.
#[test]
fn gradient_rounded_clip_follows_box() {
    let stops = vec![(0.0, [255, 0, 0, 255]), (1.0, [0, 0, 255, 255])];
    let mut c = Canvas::new_filled(12, 12, [255, 255, 255, 255]);
    c.fill_gradient(1, 1, 10, 10, &stops, 180.0, [(5.0, 5.0); 4]);
    assert_eq!(px(&c, 1, 1), [255, 255, 255, 255], "corner pixel stays canvas bg");
    assert_eq!(px(&c, 10, 1), [255, 255, 255, 255], "opposite corner too");
    assert_ne!(px(&c, 6, 6), [255, 255, 255, 255], "middle paints");
}

/// Stop interpolation runs premultiplied: a transparent-red → opaque-blue
/// midpoint keeps blue at full saturation instead of the purple a
/// straight rgba lerp produces; positions clamp outside the stop list
/// and equal positions keep the later stop past the hard line.
#[test]
fn gradient_stop_interpolation_is_premultiplied() {
    assert_eq!(lerp_premultiplied([255, 0, 0, 0], [0, 0, 255, 255], 0.5), [0, 0, 255, 128]);
    assert_eq!(lerp_premultiplied([10, 20, 30, 255], [10, 20, 30, 255], 0.5), [10, 20, 30, 255]);
    let stops = vec![(0.25, [255, 0, 0, 0]), (0.75, [0, 0, 255, 255])];
    assert_eq!(gradient_stop_color(&stops, 0.0), [255, 0, 0, 0], "below clamps to first");
    assert_eq!(gradient_stop_color(&stops, 1.0), [0, 0, 255, 255], "above clamps to last");
    // t=0.5 sits mid-ramp: k=0.5, the premultiplied midpoint again.
    assert_eq!(gradient_stop_color(&stops, 0.5), [0, 0, 255, 128]);
    let hard = vec![(0.0, [1, 2, 3, 255]), (0.5, [4, 5, 6, 255]), (0.5, [7, 8, 9, 255]), (1.0, [1, 1, 1, 255])];
    assert_eq!(gradient_stop_color(&hard, 0.5), [7, 8, 9, 255], "equal positions: later stop wins past the hard line");
    assert_eq!(gradient_stop_color(&hard, 0.49), [4, 5, 6, 255], "just before the hard line is the earlier stop");
}

/// Text blits source-over: 50% black over white is mid-gray, and the
/// alpha ramp composites linearly in premultiplied space.
#[test]
fn text_blit_blends_source_over() {
    let mut c = Canvas::new_filled(4, 4, [255, 255, 255, 255]);
    let raster = TextRaster {
        width: 2,
        height: 1,
        baseline: 0.0,
        top: 0.0,
        data: vec![0, 0, 0, 128, 0, 0, 0, 255],
    };
    c.blit_text(&raster, 1, 1);
    assert_eq!(px(&c, 1, 1), [127, 127, 127, 255], "half-alpha black over white");
    assert_eq!(px(&c, 2, 1), [0, 0, 0, 255], "opaque black covers");
    assert_eq!(px(&c, 0, 0), [255, 255, 255, 255], "outside untouched");
}

/// The clip stack constrains fills, nesting intersects, and popping
/// restores — a degenerate intersection clips everything.
#[test]
fn clip_stack_constrains_and_pops() {
    let mut c = Canvas::new_filled(10, 10, [255, 255, 255, 255]);
    c.push_clip(2, 2, 6, 6);
    c.fill_rect(0, 0, 10, 10, [200, 40, 40, 255]);
    assert_eq!(px(&c, 0, 0), [255, 255, 255, 255], "outside clip untouched");
    assert_eq!(px(&c, 5, 5), [200, 40, 40, 255], "inside clip painted");
    assert_eq!(px(&c, 6, 5), [255, 255, 255, 255], "x1 exclusive");

    // Nested clip intersects.
    c.push_clip(4, 4, 8, 8);
    c.fill_rect(0, 0, 10, 10, [0, 0, 0, 255]);
    assert_eq!(px(&c, 3, 5), [200, 40, 40, 255], "inner clip keeps only [4,6)");
    assert_eq!(px(&c, 5, 5), [0, 0, 0, 255]);
    c.pop_clip();
    c.pop_clip();
    c.fill_rect(0, 0, 10, 10, [0, 255, 0, 255]);
    assert_eq!(px(&c, 0, 0), [0, 255, 0, 255], "popped clips restore");

    // Degenerate intersection clips everything beneath.
    c.push_clip(8, 8, 2, 2);
    c.fill_rect(0, 0, 10, 10, [255, 0, 0, 255]);
    assert_eq!(px(&c, 9, 9), [0, 255, 0, 255], "degenerate clip paints nothing");
}

/// Band painting with no shift is pixel-identical to plain execute —
/// the viewport path's dy=0 degenerate case must not perturb the
/// established renderer.
#[test]
fn band_dy_zero_matches_execute() {
    use super::super::image::DecodedImage;
    let items = vec![
        PaintItem::Bg { rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 60.0 }, color: [30, 60, 90, 255], radius: 0.0 },
        PaintItem::Border { rect: super::super::Rect { x: 5.0, y: 5.0, width: 30.0, height: 20.0 }, widths: [2.0, 3.0, 4.0, 1.0], color: [200, 40, 40, 255], radii: [(0.0, 0.0); 4] },
        PaintItem::Clip { rect: super::super::Rect { x: 8.0, y: 8.0, width: 24.0, height: 14.0 } },
        PaintItem::Bg { rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 60.0 }, color: [240, 240, 0, 255], radius: 0.0 },
        PaintItem::PopClip,
        PaintItem::Image {
            rect: super::super::Rect { x: 10.0, y: 30.0, width: 8.0, height: 8.0 },
            paint_rect: super::super::Rect { x: 10.0, y: 30.0, width: 8.0, height: 8.0 },
            image: DecodedImage::new(2, 2, vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]),
            alpha: 1.0,
        },
        PaintItem::Replaced {
            rect: super::super::Rect { x: 25.0, y: 40.0, width: 10.0, height: 12.0 },
            alt: Some(("alt".into(), 16.0, false, 20.0, [0, 0, 0, 255])),
            fill_placeholder: true,
            alpha: 1.0,
            widget: None,
            form: None,
            caret: None,
        },
        PaintItem::Text { text: "hello".into(), font_size: 16.0, bold: false, color: [0, 0, 0, 255], line_height: 20.0, x: 2.0, y: 4.0, wrap_at: 36.0, gradient: None, decorations: TextDecorations::default(), mono: false, word_spacing: 0.0, truncate_at: None, tokens: None, ws: WhiteSpace::Normal, text_shadow: None, small_caps: false, han: None },
    ];
    let fonts = crate::diting_fonts::font_book();
    let mut full = Canvas::new_filled(40, 60, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut full);
    let mut band = Canvas::new_filled(40, 60, [255, 255, 255, 255]);
    execute_band(&items, &fonts, &mut band, 0.0, 0.0);
    assert_eq!(full.data, band.data, "dy=0 band paint equals execute");
}

/// text-decoration paint (#419): an underlined run adds a stroke strictly
/// below the glyph ink, line-through/overline add their own bands, and an
/// empty decoration set paints nothing extra.
#[test]
fn text_decorations_paint_line_bands() {
    let fonts = crate::diting_fonts::font_book();
    let paint = |decorations| {
        let items = vec![PaintItem::Text {
            text: "mmmm".into(),
            font_size: 16.0,
            bold: false,
            color: [0, 0, 0, 255],
            line_height: 20.0,
            x: 2.0,
            y: 4.0,
            wrap_at: 400.0,
            gradient: None,
            decorations,
            mono: false,
            word_spacing: 0.0,
            truncate_at: None,
            tokens: None,
            ws: WhiteSpace::Normal,
            text_shadow: None,
small_caps: false, han: None }];
        let mut c = Canvas::new_filled(80, 32, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        c
    };
    let ink_rows = |c: &Canvas| -> Vec<usize> {
        (0..c.height)
            .map(|y| (0..c.width).filter(|&x| px(c, x, y)[3] > 0 && px(c, x, y)[0] < 128).count())
            .collect()
    };
    let base = ink_rows(&paint(TextDecorations::default()));
    let base_total: usize = base.iter().sum();
    let base_bottom = (0..32).rev().find(|&y| base[y] > 0).unwrap();
    let under = ink_rows(&paint(TextDecorations { underline: true, ..Default::default() }));
    assert!(under.iter().sum::<usize>() > base_total, "underline adds ink");
    // "mmmm" has no descenders: the underline sits strictly below the
    // glyph ink's bottom row.
    let under_bottom = (0..32).rev().find(|&y| under[y] > base[y]).unwrap();
    assert!(under_bottom > base_bottom, "underline ink below glyph bottom {base_bottom}, got {under_bottom}");
    for d in [
        TextDecorations { line_through: true, ..Default::default() },
        TextDecorations { overline: true, ..Default::default() },
        TextDecorations { underline: true, line_through: true, ..Default::default() },
    ] {
        let rows = ink_rows(&paint(d));
        assert!(rows.iter().sum::<usize>() > base_total, "decoration {d:?} adds ink");
    }
    // The xf-bracket path paints the same strokes through the affine.
    let items = vec![
        PaintItem::SetXf { xf: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0] },
        PaintItem::Text {
            text: "mmmm".into(),
            font_size: 16.0,
            bold: false,
            color: [0, 0, 0, 255],
            line_height: 20.0,
            x: 2.0,
            y: 4.0,
            wrap_at: 400.0,
            gradient: None,
            decorations: TextDecorations { underline: true, ..Default::default() },
            mono: false,
            word_spacing: 0.0,
            truncate_at: None,
            tokens: None,
            ws: WhiteSpace::Normal,
            text_shadow: None,
small_caps: false, han: None },
        PaintItem::ClearXf,
    ];
    let mut c = Canvas::new_filled(80, 32, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut c);
    let rows = ink_rows(&c);
    assert!(rows.iter().sum::<usize>() > base_total, "underline paints under identity xf");
}

/// Native form widgets (form paint batch): a checked checkbox draws the
/// ✓ ink inside a white field ringed by the gray border; unchecked
/// leaves the interior empty; a checked radio carries the center dot.
/// The direct band path and the transform-bracket scratch path paint
/// the same widget (the bracket just maps the same local tile).
#[test]
fn form_widgets_paint_checked_state() {
    let fonts = crate::diting_fonts::font_book();
    let box_at = |widget| PaintItem::Replaced {
        rect: super::super::Rect { x: 4.0, y: 4.0, width: 16.0, height: 16.0 },
        alt: None,
        fill_placeholder: false,
        widget,
        alpha: 1.0,
        form: None,
        caret: None,
    };
    // Interior pixel classes: field is white, border gray, ink near-black.
    let field = [255, 255, 255, 255];
    let border = [118, 118, 118, 255];

    // Checked checkbox: ink in the middle, white field around it.
    let mut c = Canvas::new_filled(24, 24, [0, 255, 0, 255]);
    execute(&[box_at(Some(super::super::FormWidget::Checkbox { checked: true }))], &fonts, &mut c);
    assert_eq!(px(&c, 12, 4), border, "top border band");
    assert_eq!(px(&c, 6, 6), field, "field interior inside the border ring");
    // Ink = all channels dark (the green canvas bg would sneak past a
    // red-channel-only probe; the gray border sits above 80).
    let ink = c.data.chunks_exact(4).filter(|p| p[0] < 80 && p[1] < 80 && p[2] < 80 && p[3] > 200).count();
    assert!(ink > 4, "the ✓ must leave dark ink; got {ink} px");

    // Unchecked: same box, no ink anywhere.
    let mut c = Canvas::new_filled(24, 24, [0, 255, 0, 255]);
    execute(&[box_at(Some(super::super::FormWidget::Checkbox { checked: false }))], &fonts, &mut c);
    assert_eq!(px(&c, 6, 6), field, "field still paints");
    let ink = c.data.chunks_exact(4).filter(|p| p[0] < 80 && p[1] < 80 && p[2] < 80 && p[3] > 200).count();
    assert_eq!(ink, 0, "unchecked must carry no ink");

    // Checked radio: center dot, white ring field between dot and border.
    let mut c = Canvas::new_filled(24, 24, [0, 255, 0, 255]);
    execute(&[box_at(Some(super::super::FormWidget::Radio { checked: true }))], &fonts, &mut c);
    assert_eq!(px(&c, 12, 4), border, "circle's top border pixel");
    assert_eq!(px(&c, 12, 12), [26, 26, 26, 255], "center dot");
    // Between the dot and the ring: the white field (dot r=5, white
    // circle r=7, border r=8 — all centered (12,12) — so (6,12), at
    // distance 5.5, is the 2px field band).
    assert_eq!(px(&c, 6, 12), field, "field band between dot and ring");

    // Through a transform bracket: the widget rasterizes into the local
    // scratch and blits through the map — a 180° flip keeps every pixel
    // class, just mirrored, so the same probes hold after flipping x/y.
    let items = vec![
        PaintItem::SetXf { xf: [-1.0, 0.0, 0.0, -1.0, 24.0, 24.0] },
        box_at(Some(super::super::FormWidget::Radio { checked: true })),
        PaintItem::ClearXf,
    ];
    let mut c = Canvas::new_filled(24, 24, [0, 255, 0, 255]);
    execute(&items, &fonts, &mut c);
    assert_eq!(px(&c, 12, 12), [26, 26, 26, 255], "center dot survives the bracket (invariant point)");
    assert_eq!(px(&c, 6, 12), field, "field band rides the bracket");
}

/// A range slider (blitz#456) paints a 4px track spanning the
/// thumb-inset box, the leading segment in the fill gray, and a round
/// thumb parked at the value's fraction — no outer shell, no text.
#[test]
fn form_widget_paint_range_slider() {
    let fonts = crate::diting_fonts::font_book();
    let track = [203, 203, 203, 255];
    let fill = [118, 118, 118, 255];
    let ink = [26, 26, 26, 255];
    let range = |fraction| PaintItem::Replaced {
        rect: super::super::Rect { x: 4.0, y: 4.0, width: 120.0, height: 16.0 },
        alt: None,
        fill_placeholder: false,
        widget: Some(super::super::FormWidget::Range { fraction }),
        alpha: 1.0,
        form: None,
        caret: None,
    };
    // Track x ∈ [11, 117), cy = 12; fraction 0.25 parks the thumb
    // center at 11 + round(106 × 0.25) = 38.
    let mut c = Canvas::new_filled(128, 24, [0, 255, 0, 255]);
    execute(&[range(0.25)], &fonts, &mut c);
    assert_eq!(px(&c, 20, 12), fill, "leading segment behind the thumb");
    assert_eq!(px(&c, 38, 12), ink, "thumb center");
    assert_eq!(px(&c, 100, 12), track, "trailing track");

    // Fraction 0: thumb parked at the start, nothing filled.
    let mut c = Canvas::new_filled(128, 24, [0, 255, 0, 255]);
    execute(&[range(0.0)], &fonts, &mut c);
    assert_eq!(px(&c, 11, 12), ink, "thumb at the track start");
    assert_eq!(px(&c, 38, 12), track, "no fill segment ahead");
    assert_eq!(px(&c, 100, 12), track, "trailing track");
}

/// Text-run layout for the text-carrying form controls (form paint
/// polish batch): the default shell is a 1px gray ring with a white
/// field (the light button face on buttons), the run insets 2px and
/// centers vertically for single-line controls, textarea stays
/// top-anchored, and a select reserves+pains its dropdown arrow at the
/// right. An authored background (fill=false) drops the shell but keeps
/// the run layout, and an empty control paints the bare shell.
#[test]
fn form_controls_pad_center_and_arrow() {
    let fonts = crate::diting_fonts::font_book();
    let run = |text: &str| Some((text.to_string(), 16.0, false, 19.0, [0u8, 0, 0, 255]));
    let ctrl = |form, alt, fill| PaintItem::Replaced {
        rect: super::super::Rect { x: 4.0, y: 4.0, width: 120.0, height: 24.0 },
        alt,
        fill_placeholder: fill,
        widget: None,
        alpha: 1.0,
        form,
        caret: None,
    };
    // Ink bbox over the whole canvas, three channels dark (the green bg
    // and the gray ring/arrow both sit at or above 80).
    let ink_bbox = |c: &Canvas| {
        let mut b: Option<(usize, usize, usize, usize)> = None;
        for y in 0..c.height {
            for x in 0..c.width {
                let [r, g, bl, _] = px(c, x, y);
                if r < 80 && g < 80 && bl < 80 {
                    b = Some(match b {
                        None => (x, y, x, y),
                        Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                    });
                }
            }
        }
        b
    };

    // Text input: ring + white field, run padded in and centered.
    let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
    execute(&[ctrl(Some(super::super::FormRun::Input), run("abcd"), true)], &fonts, &mut c);
    assert_eq!(px(&c, 4, 15), [118, 118, 118, 255], "left ring band");
    assert_eq!(px(&c, 10, 10), [255, 255, 255, 255], "white field inside the ring");
    let (x0, y0, _x1, y1) = ink_bbox(&c).expect("input run ink");
    assert!(x0 >= 6, "run starts at least 2px inside the box (x0={x0})");
    assert!(y0 > 5 && y1 < 27, "run clear of the ring bands (y={y0}..{y1})");
    let cy = (y0 + y1) as f32 / 2.0;
    assert!((13.0..=19.0).contains(&cy), "run vertically centered on 16 (cy={cy})");

    // Authored background: no shell (the canvas shows through), run keeps
    // its layout.
    let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
    execute(&[ctrl(Some(super::super::FormRun::Input), run("abcd"), false)], &fonts, &mut c);
    assert_eq!(px(&c, 10, 10), [0, 255, 0, 255], "no shell without the default look");
    assert!(ink_bbox(&c).is_some(), "the run still paints");

    // Empty control: bare shell, no ink.
    let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
    execute(&[ctrl(Some(super::super::FormRun::Input), None, true)], &fonts, &mut c);
    assert_eq!(px(&c, 10, 10), [255, 255, 255, 255], "empty input keeps its field");
    assert!(ink_bbox(&c).is_none(), "no run, no ink");

    // Button: light button-face field, label centered horizontally.
    let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
    execute(&[ctrl(Some(super::super::FormRun::Button), run("Go"), true)], &fonts, &mut c);
    assert_eq!(px(&c, 10, 10), [239, 239, 239, 255], "button face");
    let (x0, _y0, x1, _y1) = ink_bbox(&c).expect("button label ink");
    let cx = (x0 + x1) as f32 / 2.0;
    assert!((60.0..=68.0).contains(&cx), "label centered on the box center 64 (cx={cx})");

    // Textarea: top-anchored — the whole run sits in the top half of a
    // 40px box (a taller box than the others for the claim to bite).
    let mut c = Canvas::new_filled(132, 50, [0, 255, 0, 255]);
    let tall = PaintItem::Replaced {
        rect: super::super::Rect { x: 4.0, y: 4.0, width: 120.0, height: 40.0 },
        alt: run("line"),
        fill_placeholder: true,
        widget: None,
        alpha: 1.0,
        form: Some(super::super::FormRun::Textarea),
        caret: None,
    };
    execute(&[tall], &fonts, &mut c);
    let (_x0, y0, _x1, y1) = ink_bbox(&c).expect("textarea run ink");
    assert!(y1 < 24, "top-anchored run stays in the top half (y={y0}..{y1})");

    // Select: the dropdown arrow in the right zone, the label clear of it.
    let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
    execute(&[ctrl(Some(super::super::FormRun::Select), run("Alpha"), true)], &fonts, &mut c);
    // cx = 4+120−9 = 115, rows 14..17 (widths 7/5/3/1) — (115,15) is the
    // second row's center pixel.
    assert_eq!(px(&c, 115, 15), [118, 118, 118, 255], "dropdown arrow ink");
    assert_eq!(px(&c, 10, 10), [255, 255, 255, 255], "select field");
    let (x0, _y0, x1, _y1) = ink_bbox(&c).expect("select label ink");
    assert!(x0 >= 6, "label padded 2px in (x0={x0})");
    assert!(x1 < 110, "label stays clear of the arrow zone (x1={x1})");

    // Through a transform bracket: the same shell rasterizes into the
    // local scratch and blits through the map — 180° flip about the
    // canvas center mirrors every probe.
    let items = vec![
        PaintItem::SetXf { xf: [-1.0, 0.0, 0.0, -1.0, 132.0, 34.0] },
        ctrl(Some(super::super::FormRun::Select), run("Alpha"), true),
        PaintItem::ClearXf,
    ];
    let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
    execute(&items, &fonts, &mut c);
    assert_eq!(px(&c, 132 - 115, 34 - 15), [118, 118, 118, 255], "arrow rides the bracket");
    assert_eq!(px(&c, 132 - 10, 34 - 10), [255, 255, 255, 255], "field rides the bracket");
}

/// Caret paint (typing-cursor batch): a 1px bar the line height tall at
/// the offset's token-walk position — offset 0 stands before the first
/// glyph, an offset past the text clears the glyphs, an empty field with
/// a caret still paints the bar (collect synthesizes the run), no caret
/// means no bar, and a textarea offset that wrapped lands on the wrapped
/// line. The run is painted white so its glyphs vanish into the white
/// field — every dark pixel below is the bar itself.
#[test]
fn caret_paints_bar_at_offset() {
    let fonts = crate::diting_fonts::font_book();
    let white_run =
        |text: &str| Some((text.to_string(), 16.0, false, 19.0, [255u8, 255, 255, 255]));
    let input = |alt, caret| PaintItem::Replaced {
        rect: super::super::Rect { x: 4.0, y: 4.0, width: 120.0, height: 24.0 },
        alt,
        fill_placeholder: true,
        widget: None,
        alpha: 1.0,
        form: Some(super::super::FormRun::Input),
        caret,
    };
    let ink = [0u8, 0, 0, 255];
    // The single dark column and its y extent: the white run keeps the
    // glyphs invisible, the gray ring sits at 118, and the green canvas
    // bg fails the all-channels probe — so only the bar qualifies.
    let bar = |c: &Canvas| -> Option<(usize, usize, usize)> {
        let mut found = None;
        for x in 0..c.width {
            let ys: Vec<usize> = (0..c.height)
                .filter(|&y| {
                    let [r, g, b, _] = px(c, x, y);
                    r < 80 && g < 80 && b < 80
                })
                .collect();
            if !ys.is_empty() {
                assert!(found.is_none(), "caret bar must be one 1px column (second at x={x})");
                found = Some((x, *ys.first().unwrap(), *ys.last().unwrap()));
            }
        }
        found
    };

    // Offset 0: the bar stands at the run start, one line tall and
    // vertically centered with the box.
    let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
    execute(&[input(white_run("abcd"), Some((0, ink)))], &fonts, &mut c);
    let (bx, y0, y1) = bar(&c).expect("caret bar at offset 0");
    assert_eq!(bx, 6, "offset 0 stands at the 2px-padded run start");
    assert_eq!(y1 - y0 + 1, 19, "bar is the line height tall");
    let cy = (y0 + y1) as f32 / 2.0;
    assert!((13.0..=19.0).contains(&cy), "bar centered on the box center 16 (cy={cy})");

    // Offset 4 (end of "abcd"): single column well past the glyphs.
    let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
    execute(&[input(white_run("abcd"), Some((4, ink)))], &fonts, &mut c);
    let (bx, _y0, _y1) = bar(&c).expect("caret bar at offset 4");
    assert!(bx > 6 + 16, "offset 4 clears the glyphs (bx={bx})");

    // Empty value with a caret: the bar still paints (collect handed the
    // synthesized run) at the field start.
    let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
    execute(&[input(None, Some((0, ink)))], &fonts, &mut c);
    let (bx, y0, y1) = bar(&c).expect("caret bar on the empty field");
    assert_eq!(bx, 6, "empty field caret at the run start");
    assert_eq!(y1 - y0 + 1, 19, "empty field bar keeps the line height");

    // No caret, white run: nothing dark anywhere.
    let mut c = Canvas::new_filled(132, 34, [0, 255, 0, 255]);
    execute(&[input(white_run("abcd"), None)], &fonts, &mut c);
    assert!(bar(&c).is_none(), "no caret, no bar");

    // Textarea whose value wraps: offset 5 (end of "bb") lands on the
    // second line — the bar's top sits a full line height below the
    // first line's.
    let tall = PaintItem::Replaced {
        rect: super::super::Rect { x: 4.0, y: 4.0, width: 40.0, height: 52.0 },
        alt: white_run("aa bb"),
        fill_placeholder: true,
        widget: None,
        alpha: 1.0,
        form: Some(super::super::FormRun::Textarea),
        caret: Some((5, ink)),
    };
    let mut c = Canvas::new_filled(60, 62, [0, 255, 0, 255]);
    execute(&[tall], &fonts, &mut c);
    let (bx, y0, y1) = bar(&c).expect("wrapped caret bar");
    assert_eq!(y1 - y0 + 1, 19, "wrapped bar keeps the line height");
    assert!(y0 >= 24, "offset 5 lands on the wrapped second line (y0={y0})");
    assert!(bx > 6, "the bar sits after the wrapped token (bx={bx})");
}

/// A band at dy=100 reproduces exactly rows [100, 180) of the full
/// render — the viewport-frame contract AginxOS's screencast builds on.
#[test]
fn band_capture_equals_window_of_full_render() {
    let items = vec![
        PaintItem::Bg { rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 300.0 }, color: [30, 60, 90, 255], radius: 0.0 },
        PaintItem::Bg { rect: super::super::Rect { x: 4.0, y: 120.0, width: 32.0, height: 40.0 }, color: [200, 40, 40, 255], radius: 0.0 },
        PaintItem::Border { rect: super::super::Rect { x: 6.0, y: 240.0, width: 28.0, height: 30.0 }, widths: [3.0, 3.0, 3.0, 3.0], color: [0, 200, 0, 255], radii: [(0.0, 0.0); 4] },
    ];
    let fonts = crate::diting_fonts::font_book();
    let mut full = Canvas::new_filled(40, 300, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut full);
    let mut band = Canvas::new_filled(40, 80, [255, 255, 255, 255]);
    execute_band(&items, &fonts, &mut band, 0.0, 100.0);
    for y in 0..80 {
        for x in 0..40 {
            let f = &full.data[((y + 100) * 40 + x) * 4..((y + 100) * 40 + x) * 4 + 4];
            let b = &band.data[(y * 40 + x) * 4..(y * 40 + x) * 4 + 4];
            assert_eq!(f, b, "band row {y} must equal full row {}", y + 100);
        }
    }
}

/// A clip opened above the band and closed inside it stays paired and
/// still cuts: the clip rect translates with the band, so content
/// beyond the clip's page-space edge stays out even though the clip's
/// own bounds are far above the canvas.
#[test]
fn clip_spanning_band_stays_paired_and_cuts() {
    let items = vec![
        PaintItem::Clip { rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 130.0 } },
        PaintItem::Bg { rect: super::super::Rect { x: 0.0, y: 50.0, width: 40.0, height: 100.0 }, color: [0, 200, 0, 255], radius: 0.0 },
        PaintItem::PopClip,
    ];
    let fonts = crate::diting_fonts::font_book();
    let mut band = Canvas::new_filled(40, 80, [255, 255, 255, 255]);
    execute_band(&items, &fonts, &mut band, 0.0, 100.0);
    // Clip page [0,130) → band [-100,30): green bg page [50,150) → band
    // [-50,50), clipped to rows [0,30).
    assert_eq!(px(&band, 20, 29), [0, 200, 0, 255], "clipped green inside the band");
    assert_eq!(px(&band, 20, 30), [255, 255, 255, 255], "clip's page-space edge still cuts");
    // The stack must have drained: a later fill fills the whole canvas.
    band.fill_rect(0, 0, 40, 80, [0, 0, 255, 255]);
    assert_eq!(px(&band, 0, 0), [0, 0, 255, 255], "clip popped, stack drained");
}

/// Text whose line box straddles the band's top edge keeps its ink in
/// the band (the estimate's one-line top slack), and text far below is
/// skipped without polluting the canvas.
#[test]
fn text_band_edges() {
    let fonts = crate::diting_fonts::font_book();
    // A tall low-content page: only two text leaves, one near the band.
    let items = vec![
        PaintItem::Text { text: "edge".into(), font_size: 16.0, bold: false, color: [0, 0, 0, 255], line_height: 20.0, x: 2.0, y: 96.0, wrap_at: 36.0, gradient: None, decorations: TextDecorations::default(), mono: false, word_spacing: 0.0, truncate_at: None, tokens: None, ws: WhiteSpace::Normal, text_shadow: None, small_caps: false, han: None },
        PaintItem::Text { text: "far".into(), font_size: 16.0, bold: false, color: [0, 0, 0, 255], line_height: 20.0, x: 2.0, y: 500.0, wrap_at: 36.0, gradient: None, decorations: TextDecorations::default(), mono: false, word_spacing: 0.0, truncate_at: None, tokens: None, ws: WhiteSpace::Normal, text_shadow: None, small_caps: false, han: None },
    ];
    let mut band = Canvas::new_filled(40, 80, [255, 255, 255, 255]);
    execute_band(&items, &fonts, &mut band, 0.0, 100.0);
    let ink = band.data.chunks_exact(4).any(|p| p[0] < 128);
    assert!(ink, "straddling text must paint into the band");
    // The far text must not have painted anything (only "edge" ink).
    let dark_rows: Vec<usize> = (0..80)
        .filter(|&y| (0..40).any(|x| band.data[(y * 40 + x) * 4] < 128))
        .collect();
    assert!(dark_rows.iter().all(|&y| y < 25), "no ink from the far-below text: {dark_rows:?}");
}

// ---- affine brackets (rotate/skew/matrix paint) ----

/// rotate(90°) about the 120×60 box's center paints pixel-exactly: the
/// map x' = 90 − y, y' = x − 30 (pivot (60, 30)) turns the box into a
/// 60×120 canvas region — integer multiples of 90° must have zero
/// rasterization slop, exactly like the axis-aligned path.
#[test]
fn rotate_90_bracket_pixel_exact() {
    let items = vec![
        PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 90.0, -30.0] },
        PaintItem::Bg {
            rect: super::super::Rect { x: 0.0, y: 0.0, width: 120.0, height: 60.0 },
            color: [200, 40, 40, 255],
            radius: 0.0,
        },
        PaintItem::ClearXf,
    ];
    let fonts = crate::diting_fonts::font_book();
    let mut c = Canvas::new_filled(120, 120, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut c);
    // Mapped box: x' ∈ [30, 90), y' ∈ [−30, 90) → on-canvas [30,90)×[0,90).
    assert_eq!(px(&c, 30, 0), [200, 40, 40, 255], "mapped TL corner");
    assert_eq!(px(&c, 89, 89), [200, 40, 40, 255], "mapped BR corner");
    assert_eq!(px(&c, 90, 10), [255, 255, 255, 255], "right of the mapped box");
    assert_eq!(px(&c, 29, 10), [255, 255, 255, 255], "left of the mapped box");
    assert_eq!(px(&c, 60, 90), [255, 255, 255, 255], "below the mapped box");
    // The stack drained: a post-ClearXf fill paints unrotated.
    c.fill_rect(0, 0, 5, 5, [0, 0, 255, 255]);
    assert_eq!(px(&c, 0, 0), [0, 0, 255, 255]);
}

/// Nested brackets compose multiplicatively: an inner translate rides
/// the outer rotation (the canvas total is outer∘inner), and both
/// clears return to the identity.
#[test]
fn nested_brackets_compose() {
    // Outer: rotate(90°) about (60, 30) — x' = 90 − y, y' = x − 30.
    // Inner: translate(10, 0). Total: x' = 90 − y, y' = x − 20.
    let items = vec![
        PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 90.0, -30.0] },
        PaintItem::SetXf { xf: [1.0, 0.0, 0.0, 1.0, 10.0, 0.0] },
        PaintItem::Bg {
            rect: super::super::Rect { x: 30.0, y: 60.0, width: 10.0, height: 10.0 },
            color: [0, 200, 0, 255],
            radius: 0.0,
        },
        PaintItem::ClearXf,
        PaintItem::ClearXf,
    ];
    let fonts = crate::diting_fonts::font_book();
    let mut c = Canvas::new_filled(120, 120, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut c);
    // Total maps (x, y) → (90 − y, x − 20): the local (30,60,10,10) box
    // lands at x' ∈ [20,30), y' ∈ [10,20).
    assert_eq!(px(&c, 20, 10), [0, 200, 0, 255], "nested compose TL");
    assert_eq!(px(&c, 29, 19), [0, 200, 0, 255], "nested compose BR");
    assert_eq!(px(&c, 30, 10), [255, 255, 255, 255], "right edge exclusive");
    assert_eq!(px(&c, 20, 20), [255, 255, 255, 255], "bottom edge exclusive");
}

/// An affine clip cuts child ink along the rotation: a clip at local
/// y < 30 under the 90° bracket trims the child background to its
/// inverse image — and the clip pops cleanly afterwards.
#[test]
fn affine_clip_cuts_child_ink() {
    let items = vec![
        PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 90.0, -30.0] },
        PaintItem::Clip {
            rect: super::super::Rect { x: 0.0, y: 0.0, width: 120.0, height: 30.0 },
        },
        PaintItem::Bg {
            rect: super::super::Rect { x: 0.0, y: 0.0, width: 120.0, height: 60.0 },
            color: [200, 40, 40, 255],
            radius: 0.0,
        },
        PaintItem::PopClip,
        PaintItem::ClearXf,
    ];
    let fonts = crate::diting_fonts::font_book();
    let mut c = Canvas::new_filled(120, 120, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut c);
    // Clip local y ∈ [0,30) ⇔ canvas x' = 90 − y ∈ (60, 90]: pixel
    // centers pass at columns 60..=89; the bg box caps at x' < 90.
    assert_eq!(px(&c, 60, 40), [200, 40, 40, 255], "inside the rotated window");
    assert_eq!(px(&c, 59, 40), [255, 255, 255, 255], "outside the rotated window");
    assert_eq!(px(&c, 89, 40), [200, 40, 40, 255], "window's far edge");
    assert_eq!(px(&c, 90, 40), [255, 255, 255, 255], "past the bg box");
    // The affine clip drained: a later fill covers the whole canvas.
    c.fill_rect(0, 0, 120, 120, [0, 0, 255, 255]);
    assert_eq!(px(&c, 0, 0), [0, 0, 255, 255]);
}

/// Text under a bracket rasterizes at raw local metrics and blits
/// through the map: a 180° flip moves the ink to the mirror half of
/// the canvas and none survives at the un-mapped position.
#[test]
fn affine_text_paints_through_bracket() {
    // rotate(180°) about (60, 25): x' = 120 − x, y' = 50 − y.
    let items = vec![
        PaintItem::SetXf { xf: [-1.0, 0.0, 0.0, -1.0, 120.0, 50.0] },
        PaintItem::Text {
            text: "flip".into(),
            font_size: 16.0,
            bold: false,
            color: [0, 0, 0, 255],
            line_height: 20.0,
            x: 10.0,
            y: 10.0,
            wrap_at: 200.0,
            gradient: None,
            decorations: TextDecorations::default(),
            mono: false,
            word_spacing: 0.0,
            truncate_at: None,
            tokens: None,
            ws: WhiteSpace::Normal,
            text_shadow: None,
small_caps: false, han: None },
        PaintItem::ClearXf,
    ];
    let fonts = crate::diting_fonts::font_book();
    let mut c = Canvas::new_filled(120, 50, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut c);
    // Un-mapped ink would sit at x < 30 (the leaf is at x=10, ~30px wide);
    // the 180° flip about x=60 mirrors it to the right half (x > 60).
    let ink_x: Vec<usize> = (0..c.width)
        .filter(|&x| (0..c.height).any(|y| c.data[(y * c.width + x) * 4] < 128))
        .collect();
    assert!(!ink_x.is_empty(), "text must paint through the bracket");
    assert!(
        ink_x.iter().all(|&x| x > 60),
        "ink must land in the mirrored half: {ink_x:?}"
    );
}

/// skewX(45°) — x' = x + y, y' = y — paints pixel-exactly: the sheared
/// parallelogram puts ink at columns the unskewed box can't reach and
/// cuts the ones only it occupied (affine residuals batch).
#[test]
fn skew_x_bracket_pixel_exact() {
    let items = vec![
        PaintItem::SetXf { xf: [1.0, 0.0, 1.0, 1.0, 0.0, 0.0] },
        PaintItem::Bg {
            rect: super::super::Rect { x: 10.0, y: 10.0, width: 40.0, height: 20.0 },
            color: [200, 40, 40, 255],
            radius: 0.0,
        },
        PaintItem::ClearXf,
    ];
    let fonts = crate::diting_fonts::font_book();
    let mut c = Canvas::new_filled(120, 60, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut c);
    // Inverse: lx = cx − cy, ly = cy. Local box [10,50)×[10,30).
    assert_eq!(px(&c, 20, 10), [200, 40, 40, 255], "sheared TL edge");
    assert_eq!(px(&c, 55, 20), [200, 40, 40, 255], "right of the unskewed box — only skew reaches");
    assert_eq!(px(&c, 69, 29), [200, 40, 40, 255], "far bottom-right of the parallelogram");
    assert_eq!(px(&c, 30, 29), [255, 255, 255, 255], "lower-left cut away by the shear");
    assert_eq!(px(&c, 60, 10), [255, 255, 255, 255], "top right edge exclusive (lx = 50)");
    assert_eq!(px(&c, 79, 29), [255, 255, 255, 255], "bottom right edge exclusive");
}

/// matrix(1, 0.5, 0, 1, 5, 0) — x' = x + 5, y' = 0.5x + y — the general
/// affine form, pixel-exact on integer-friendly entries.
#[test]
fn matrix_bracket_pixel_exact() {
    let items = vec![
        PaintItem::SetXf { xf: [1.0, 0.5, 0.0, 1.0, 5.0, 0.0] },
        PaintItem::Bg {
            rect: super::super::Rect { x: 10.0, y: 10.0, width: 40.0, height: 20.0 },
            color: [200, 40, 40, 255],
            radius: 0.0,
        },
        PaintItem::ClearXf,
    ];
    let fonts = crate::diting_fonts::font_book();
    let mut c = Canvas::new_filled(60, 50, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut c);
    // Inverse: lx = cx − 5, ly = cy − 0.5·lx. Local box [10,50)×[10,30).
    assert_eq!(px(&c, 16, 16), [200, 40, 40, 255], "mapped TL region");
    assert_eq!(px(&c, 30, 26), [200, 40, 40, 255], "mid box");
    assert_eq!(px(&c, 30, 33), [200, 40, 40, 255], "sheared down — only the matrix reaches");
    assert_eq!(px(&c, 30, 8), [255, 255, 255, 255], "above the sheared top");
    assert_eq!(px(&c, 55, 40), [255, 255, 255, 255], "past the sheared right");
}

/// A rounded border through a bracket is the ROUNDED ring rotated, not
/// the square-cornered one: corners stay cut along the curve, the
/// widths-inset hole stays open (affine residuals batch ①).
#[test]
fn rotated_rounded_border_pixel_exact() {
    // rotate(90°) about (20, 20): (x, y) → (40 − y, x).
    let items = vec![
        PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 40.0, 0.0] },
        PaintItem::Border {
            rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 40.0 },
            widths: [6.0, 6.0, 6.0, 6.0],
            color: [200, 40, 40, 255],
            radii: [(12.0, 12.0); 4],
        },
        PaintItem::ClearXf,
    ];
    let fonts = crate::diting_fonts::font_book();
    let mut c = Canvas::new_filled(40, 40, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut c);
    // Inverse: lx = 40 − cy, ly = cx.
    assert_eq!(px(&c, 20, 20), [255, 255, 255, 255], "the inner hole stays open");
    assert_eq!(px(&c, 20, 5), [200, 40, 40, 255], "straight edge band");
    assert_eq!(px(&c, 33, 6), [200, 40, 40, 255], "corner arc band (outer in, inner out)");
    assert_eq!(px(&c, 37, 2), [255, 255, 255, 255], "outside the rounded corner — square paint would hit");
}

/// The axis-aligned twin: nonzero radii switch the border to the rounded
/// ring — corners cut along the arc, hole open, edges solid. Zero radii
/// keep the historical four-band paint (covered by the older tests).
#[test]
fn rounded_border_ring_axis_aligned() {
    let items = vec![PaintItem::Border {
        rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 40.0 },
        widths: [4.0, 4.0, 4.0, 4.0],
        color: [200, 40, 40, 255],
        radii: [(10.0, 10.0); 4],
    }];
    let fonts = crate::diting_fonts::font_book();
    let mut c = Canvas::new_filled(40, 40, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut c);
    assert_eq!(px(&c, 0, 0), [255, 255, 255, 255], "corner cut by the 10px arc");
    assert_eq!(px(&c, 5, 5), [200, 40, 40, 255], "arc band mid-corner");
    assert_eq!(px(&c, 20, 0), [200, 40, 40, 255], "top band");
    assert_eq!(px(&c, 0, 20), [200, 40, 40, 255], "left band");
    assert_eq!(px(&c, 20, 20), [255, 255, 255, 255], "hole open");
}

/// #38: fractional border widths on the square four-band path used to
/// `as i64`-truncate to zero (border: 0.8px painted NOTHING). Now each
/// side snaps to the nearest device pixel with alpha scaled by the
/// coverage, a zero-width side stays unpainted, and integer widths are
/// unchanged.
#[test]
fn fractional_border_widths_paint_with_coverage() {
    let fonts = crate::diting_fonts::font_book();
    let paint = |widths: [f32; 4]| {
        let items = vec![PaintItem::Border {
            rect: super::super::Rect { x: 0.0, y: 0.0, width: 40.0, height: 30.0 },
            widths,
            color: [200, 40, 40, 255],
            radii: [(0.0, 0.0); 4],
        }];
        let mut c = Canvas::new_filled(40, 30, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        c
    };
    // 0.8px: every side 1px at coverage 0.8 -> alpha 204 over white.
    let c = paint([0.8, 0.8, 0.8, 0.8]);
    assert_eq!(px(&c, 20, 0), [211, 83, 83, 255], "0.8px top paints 1px at 0.8 coverage");
    assert_eq!(px(&c, 20, 29), [211, 83, 83, 255], "0.8px bottom");
    assert_eq!(px(&c, 0, 15), [211, 83, 83, 255], "0.8px left");
    assert_eq!(px(&c, 39, 15), [211, 83, 83, 255], "0.8px right");
    assert_eq!(px(&c, 20, 15), [255, 255, 255, 255], "interior untouched");
    // 1.5px: snaps to 2px at coverage 0.75 -> alpha 191 on both rows.
    let c = paint([1.5, 1.5, 1.5, 1.5]);
    assert_eq!(px(&c, 20, 0), [213, 93, 93, 255], "1.5px row 0");
    assert_eq!(px(&c, 20, 1), [213, 93, 93, 255], "1.5px row 1");
    assert_eq!(px(&c, 20, 2), [255, 255, 255, 255], "1.5px stops at 2 rows");
    // A zero side still paints nothing (blitz#837's zero-side repro shape).
    let c = paint([0.0, 1.0, 1.0, 1.0]);
    assert_eq!(px(&c, 20, 0), [255, 255, 255, 255], "zero top stays unpainted");
    assert_eq!(px(&c, 20, 29), [200, 40, 40, 255], "1px bottom full alpha");
}

/// SetXfCanvas cancels the enclosing bracket: the canvas-space Bg paints
/// UNROTATED (its rotated image would be off-canvas entirely), and the
/// matching ClearXf restores the bracket for subsequent local items
/// (affine residuals batch ②, the inline-band splice's engine).
#[test]
fn canvas_bracket_cancels_enclosing_map() {
    // rotate(90°) about (60, 30): x' = 90 − y, y' = x − 30.
    let items = vec![
        PaintItem::SetXf { xf: [0.0, 1.0, -1.0, 0.0, 90.0, -30.0] },
        PaintItem::SetXfCanvas,
        PaintItem::Bg {
            rect: super::super::Rect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 },
            color: [200, 40, 40, 255],
            radius: 0.0,
        },
        PaintItem::ClearXf,
        PaintItem::Bg {
            rect: super::super::Rect { x: 40.0, y: 40.0, width: 10.0, height: 10.0 },
            color: [0, 0, 200, 255],
            radius: 0.0,
        },
        PaintItem::ClearXf,
    ];
    let fonts = crate::diting_fonts::font_book();
    let mut c = Canvas::new_filled(120, 120, [255, 255, 255, 255]);
    execute(&items, &fonts, &mut c);
    // Rotated, the red box maps to y' ∈ [−30,−20) — off-canvas; painting
    // at (0,0) proves the bracket was cancelled for it.
    assert_eq!(px(&c, 0, 0), [200, 40, 40, 255], "canvas-space bg paints unrotated");
    assert_eq!(px(&c, 9, 9), [200, 40, 40, 255], "canvas-space bg far corner");
    // After the ClearXf the rotation is back: local (40,40,10,10) maps
    // to canvas x' = 90 − y ∈ (40,50], y' = x − 30 ∈ [10,20).
    assert_eq!(px(&c, 41, 11), [0, 0, 200, 255], "bracket restored after ClearXf");
    assert_eq!(px(&c, 45, 15), [0, 0, 200, 255], "restored map far corner");
    assert_eq!(px(&c, 50, 15), [255, 255, 255, 255], "restored map edge exclusive");
}

/// text-overflow: ellipsis (blitz#888): the marker is a raster-time
/// rendering effect. Ink stops at the truncate limit (the overflow is
/// never painted) while the untruncated run inks well past it, and the
/// truncated run still carries marker ink near the limit.
#[test]
fn ellipsis_truncates_paint_ink_at_the_limit() {
    let fonts = crate::diting_fonts::font_book();
    let paint = |truncate_at: Option<f32>| {
        let items = vec![PaintItem::Text {
            text: "mmmmmmmmmmmmmmmmmmmm".into(),
            font_size: 16.0,
            bold: false,
            color: [0, 0, 0, 255],
            line_height: 20.0,
            x: 2.0,
            y: 4.0,
            wrap_at: 400.0,
            gradient: None,
            decorations: TextDecorations::default(),
            mono: false,
            word_spacing: 0.0,
            truncate_at,
            tokens: None,
            ws: WhiteSpace::Normal,
            text_shadow: None,
small_caps: false, han: None }];
        let mut c = Canvas::new_filled(400, 32, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        c
    };
    let right_edge = |c: &Canvas| -> usize {
        (0..c.width)
            .rev()
            .find(|&x| (0..c.height).any(|y| px(c, x, y)[3] > 0 && px(c, x, y)[0] < 128))
            .unwrap_or(0)
    };
    let clipped = right_edge(&paint(Some(60.0)));
    let full = right_edge(&paint(None));
    assert!(
        clipped <= 2 + 62,
        "ink stops at the limit (x=2 + 60), got {clipped}"
    );
    assert!(full > 80, "untruncated run inks past the limit, got {full}");
    assert!(clipped > 30, "marker ink near the limit, got {clipped}");
}

/// Paint half of obscura#983: handing the decorations painter the item's
/// pre-shaped wrap tokens must stroke byte-identically to re-shaping —
/// the multi-line wrap and the ellipsis truncation cut included.
#[test]
fn decorations_pre_shaped_matches_reshaped() {
    let fonts = crate::diting_fonts::font_book();
    let text = "淘宝商品列表页的一段中文文本需要折行处理".repeat(3);
    let deco = TextDecorations { underline: true, line_through: true, ..Default::default() };
    let tokens = tokens_of(&text, 16.0, false, &fonts, false, 0.0, WhiteSpace::Normal, false, None);
    let stroke = |pre: Option<&[Token]>, truncate_at: Option<f32>| {
        let mut c = Canvas::new_filled(320, 200, [255, 255, 255, 255]);
        paint_text_decorations(&mut c, &fonts, &text, 16.0, false, [0, 0, 0, 255], 24.0, 2.0, 4.0, 300.0, deco, false, 0.0, truncate_at, WhiteSpace::Normal, false, None, pre, 0.0, 0.0);
        c.data
    };
    assert_eq!(stroke(None, None), stroke(Some(&tokens), None), "wrapped: pre-shaped == re-shaped");
    assert_eq!(stroke(None, Some(200.0)), stroke(Some(&tokens), Some(200.0)), "truncated: pre-shaped == re-shaped");
}

// ---- box-shadow (blitz#349 family, v1) ----

/// A zero-blur offset shadow paints hard: visible in the offset band
/// outside the element, knocked out inside the element's own box even
/// though the element paints no background of its own.
#[test]
fn box_shadow_offset_band_knocked_out_inside() {
    let mut c = Canvas::new_filled(40, 40, [255, 255, 255, 255]);
    let rect = Rect { x: 10.0, y: 10.0, width: 10.0, height: 10.0 };
    c.fill_box_shadow(&rect, [255, 0, 0, 255], [(0.0, 0.0); 4], 5.0, 5.0, 0.0, 0.0, false);
    assert_eq!(px(&c, 20, 20), [255, 0, 0, 255], "offset band right/below");
    assert_eq!(px(&c, 19, 19), [255, 255, 255, 255], "inside the element: knocked out");
    assert_eq!(px(&c, 5, 5), [255, 255, 255, 255], "opposite corner: no shadow");
    assert_eq!(px(&c, 30, 30), [255, 255, 255, 255], "past the offset box: none");
}

/// The feather falls off monotonically with distance from the shadow
/// box edge; under the feather the element interior stays knocked out,
/// and past blur px nothing paints at all.
#[test]
fn box_shadow_blur_falloff_monotonic() {
    let mut c = Canvas::new_filled(60, 30, [255, 255, 255, 255]);
    let rect = Rect { x: 10.0, y: 10.0, width: 10.0, height: 10.0 };
    c.fill_box_shadow(&rect, [0, 0, 0, 255], [(0.0, 0.0); 4], 0.0, 0.0, 8.0, 0.0, false);
    let ink = |x: usize, y: usize| 255 - px(&c, x, y)[0];
    assert_eq!(ink(19, 14), 0, "knocked out even under the feather");
    let near = ink(21, 14); // 1.5px past the right edge (edge at 20)
    let far = ink(26, 14); // 6.5px past
    assert!(near > far, "monotonic: {near} > {far}");
    assert!(near < 255 && near > 0, "feather band is partial: {near}");
    assert_eq!(ink(0, 14), 0, "past the feather extent: none");
}

/// Inset v2: ink fills between the shadow box edge and the element
/// edge, hard-clipped to the element box. A 10px element with spread 2
/// gets a full-ink 2px ring around the shadow box (12..18) and a
/// hollow center; nothing lands outside the element.
#[test]
fn box_shadow_inset_spread_ring_clipped_to_element() {
    let mut c = Canvas::new_filled(40, 40, [255, 255, 255, 255]);
    let rect = Rect { x: 10.0, y: 10.0, width: 10.0, height: 10.0 };
    c.fill_box_shadow(&rect, [255, 0, 0, 255], [(0.0, 0.0); 4], 0.0, 0.0, 0.0, 2.0, true);
    assert_eq!(px(&c, 10, 14), [255, 0, 0, 255], "element edge: full ink");
    assert_eq!(px(&c, 11, 14), [255, 0, 0, 255], "ring band (0.5 outside the shadow box)");
    assert_eq!(px(&c, 13, 14), [255, 255, 255, 255], "inside the shadow box: hollow");
    assert_eq!(px(&c, 9, 14), [255, 255, 255, 255], "outside the element: hard clip");
    assert_eq!(px(&c, 30, 14), [255, 255, 255, 255], "nowhere past the element");
}

/// The inset feather falls inward from the shadow box edge: partial at
/// the element edge, monotonic down to nothing `blur` px inside, and
/// the element interior deep in the hollow stays clean.
#[test]
fn box_shadow_inset_blur_falloff_monotonic() {
    let mut c = Canvas::new_filled(60, 60, [255, 255, 255, 255]);
    let rect = Rect { x: 10.0, y: 10.0, width: 40.0, height: 40.0 };
    c.fill_box_shadow(&rect, [0, 0, 0, 255], [(0.0, 0.0); 4], 0.0, 0.0, 8.0, 0.0, true);
    let ink = |x: usize, y: usize| 255 - px(&c, x, y)[0];
    let near = ink(10, 30); // 0.5 inside the element edge (edge at 10)
    let far = ink(16, 30); // 6.5 inside
    assert!(near > far, "monotonic inward: {near} > {far}");
    assert!(near < 255 && near > 0, "feather band is partial: {near}");
    assert_eq!(ink(19, 30), 0, "past the feather extent: none");
}

/// The SDF is exact on the straight edges: -5 at the center of a
/// 10x10 box (r=2), 0 on the right edge, +3 three px out.
#[test]
fn sd_rounded_box_exact_on_edges() {
    assert_eq!(sd_rounded_box(0.0, 0.0, 3.0, 3.0, 2.0), -5.0);
    assert!(sd_rounded_box(5.0, 0.0, 3.0, 3.0, 2.0).abs() < 1e-9, "edge distance 0");
    assert!((sd_rounded_box(8.0, 0.0, 3.0, 3.0, 2.0) - 3.0).abs() < 1e-9, "+3 outside");
}

// ---- backdrop-filter: blur() (blitz#901 family) ----

/// The blitz#901 criterion end to end on the canvas: a hard red/blue
/// seam under a rounded glass box — the seam smears INSIDE the rounded
/// shape, the corner-cut zone keeps the SHARP backdrop.
#[test]
fn backdrop_blur_smears_inside_rounded_shape_only() {
    let mut out = Canvas::new_filled(120, 60, [255, 0, 0, 255]);
    out.fill_rect(60, 0, 60, 60, [0, 0, 255, 255]);
    // Glass box x [20,100) y [10,50), uniform 20px corners.
    out.blur_backdrop(20.0, 10.0, 80.0, 40.0, [(20.0, 20.0); 4], 6.0);
    let px = |x: usize, y: usize| {
        let i = (y * 120 + x) * 4;
        (out.data[i], out.data[i + 1], out.data[i + 2], out.data[i + 3])
    };
    // Seam center inside the shape: red and blue both bled in.
    let (r, g, b, a) = px(60, 30);
    assert!(r > 40 && r < 215, "red bled right: {r}");
    assert!(b > 40 && b < 215, "blue bled left: {b}");
    assert_eq!(g, 0);
    assert_eq!(a, 255);
    // 1px left of the seam, still inside the shape: the blur kernel
    // (radius 3) definitely reaches it with blue.
    assert!(px(59, 30).2 > 0, "blur reaches the shape interior");
    // Corner-cut zone: (22,12) is inside the bounding box but 25.5px
    // from the TL corner circle center (40,30) — outside the shape,
    // the backdrop must stay SHARP pure red (blitz#901's exact bug).
    assert_eq!(px(22, 12), (255, 0, 0, 255), "corner-cut zone keeps the sharp backdrop");
    // Fully outside the box: untouched.
    assert_eq!(px(5, 5), (255, 0, 0, 255));
}

// ---- text-shadow (blitz#271 family) ----

fn shadow_item(layers: Vec<TextShadow>) -> Vec<PaintItem> {
    vec![PaintItem::Text {
        text: "mmmmm".into(),
        font_size: 16.0,
        bold: false,
        color: [0, 0, 0, 255],
        line_height: 20.0,
        x: 10.0,
        y: 20.0,
        wrap_at: 400.0,
        gradient: None,
        decorations: TextDecorations::default(),
        mono: false,
        word_spacing: 0.0,
        truncate_at: None,
        tokens: None,
        ws: WhiteSpace::Normal,
        text_shadow: if layers.is_empty() { None } else { Some(layers) },
small_caps: false, han: None }]
}

fn reds(c: &Canvas) -> Vec<(usize, usize)> {
    // Red-over-white composites keep r > g exactly when a red layer
    // contributed — pure white and pure black both have r == g, so any
    // nonzero red coverage (a 1/255 feather sliver included) matches.
    let mut out = Vec::new();
    for y in 0..c.height {
        for x in 0..c.width {
            if px(c, x, y)[0] > px(c, x, y)[1] {
                out.push((x, y));
            }
        }
    }
    out
}

fn darks(c: &Canvas) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for y in 0..c.height {
        for x in 0..c.width {
            let p = px(c, x, y);
            if p[0] < 80 && p[1] < 80 && p[2] < 80 {
                out.push((x, y));
            }
        }
    }
    out
}

/// A hard (blur 0) layer is an exact recolored copy of the glyph raster
/// at the offset: every red pixel has glyph ink `(-dx, -dy)` from it in
/// a shadow-less render, glyphs stay black, and the offset makes the
/// shadow stick out where the glyphs aren't.
#[test]
fn text_shadow_hard_offset_recolor() {
    let fonts = crate::diting_fonts::font_book();
    let plain = {
        let mut c = Canvas::new_filled(140, 40, [255, 255, 255, 255]);
        execute(&shadow_item(vec![]), &fonts, &mut c);
        c
    };
    let mut c = Canvas::new_filled(140, 40, [255, 255, 255, 255]);
    execute(&shadow_item(vec![TextShadow { dx: 8.0, dy: 0.0, blur: 0.0, color: crate::diting_css::Color(255, 0, 0, 255) }]), &fonts, &mut c);
    let red = reds(&c);
    assert!(!red.is_empty(), "shadow ink exists");
    // The shadow is a byte-identical recolored copy: every red pixel
    // (any nonzero coverage) mirrors glyph coverage at (-8,0).
    let plain_ink: std::collections::HashSet<(usize, usize)> = (0..plain.height)
        .flat_map(|y| (0..plain.width).map(move |x| (x, y)))
        .filter(|(x, y)| px(&plain, *x, *y)[0] < 255)
        .collect();
    for (x, y) in &red {
        assert!(x >= &8 && plain_ink.contains(&(x - 8, *y)), "red pixel ({x},{y}) mirrors glyph ink at (-8,0)");
    }
    // Glyphs paint OVER the shadow: solid glyph pixels stay pure black.
    for (x, y) in darks(&plain) {
        assert!(px(&c, x, y)[0] < 80, "solid glyph ({x},{y}) wins over the shadow");
    }
}

/// A co-located layer (dx=dy=0) sits entirely UNDER the glyphs: solid
/// glyph pixels stay pure glyph ink — none of the shadow color leaks
/// through full coverage.
#[test]
fn text_shadow_under_glyphs() {
    let fonts = crate::diting_fonts::font_book();
    let mut c = Canvas::new_filled(140, 40, [255, 255, 255, 255]);
    execute(&shadow_item(vec![TextShadow { dx: 0.0, dy: 0.0, blur: 0.0, color: crate::diting_css::Color(255, 0, 0, 255) }]), &fonts, &mut c);
    let plain = {
        let mut c2 = Canvas::new_filled(140, 40, [255, 255, 255, 255]);
        execute(&shadow_item(vec![]), &fonts, &mut c2);
        c2
    };
    for (x, y) in darks(&plain) {
        assert!(px(&c, x, y)[0] < 80, "solid glyph ({x},{y}) covers the shadow");
    }
}

/// A blurred layer spreads past the hard extent (feather reaches
/// `pad` px beyond the raster on both sides) and falls off toward the
/// edge — center-row alpha above the feather-tip alpha.
#[test]
fn text_shadow_blur_spreads_and_falls_off() {
    let fonts = crate::diting_fonts::font_book();
    let span = |blur: f32| {
        let mut c = Canvas::new_filled(200, 80, [255, 255, 255, 255]);
        execute(
            &shadow_item(vec![TextShadow { dx: 0.0, dy: 14.0, blur, color: crate::diting_css::Color(255, 0, 0, 255) }]),
            &fonts,
            &mut c,
        );
        let rows: Vec<usize> = reds(&c).iter().map(|(_, y)| *y).collect();
        (*rows.iter().min().unwrap(), *rows.iter().max().unwrap())
    };
    let (hard_top, hard_bot) = span(0.0);
    let (soft_top, soft_bot) = span(6.0);
    assert!(soft_top < hard_top, "feather reaches above the hard extent: {} < {hard_top}", soft_top);
    assert!(soft_bot > hard_bot, "feather reaches below the hard extent: {} > {hard_bot}", soft_bot);
    // Falloff: at the hard span's center row, alpha (redness) exceeds
    // the feather tip rows of the soft render.
    let mid_alpha = |c_row: usize, blur: f32| {
        let mut c = Canvas::new_filled(200, 80, [255, 255, 255, 255]);
        execute(
            &shadow_item(vec![TextShadow { dx: 0.0, dy: 14.0, blur, color: crate::diting_css::Color(255, 0, 0, 255) }]),
            &fonts,
            &mut c,
        );
        255 - px(&c, 30, c_row)[1]
    };
    let center = (hard_top + hard_bot) / 2;
    assert!(mid_alpha(center, 6.0) > mid_alpha(soft_top, 6.0), "center ink above feather tip");
    assert!(mid_alpha(center, 6.0) > mid_alpha(soft_bot, 6.0), "center ink above feather tip (bottom)");
}

/// Layer order: first-declared paints ON TOP. Two layers at the SAME
/// offset — the later (second-declared, painted first) blue must be
/// fully covered by the first-declared red.
#[test]
fn text_shadow_first_layer_on_top() {
    let fonts = crate::diting_fonts::font_book();
    let mut c = Canvas::new_filled(140, 40, [255, 255, 255, 255]);
    execute(
        &shadow_item(vec![
            TextShadow { dx: 8.0, dy: 0.0, blur: 0.0, color: crate::diting_css::Color(255, 0, 0, 255) },
            TextShadow { dx: 8.0, dy: 0.0, blur: 0.0, color: crate::diting_css::Color(0, 0, 255, 255) },
        ]),
        &fonts,
        &mut c,
    );
    assert!(!reds(&c).is_empty(), "first-declared red shows");
    let mut blue = 0;
    for y in 0..c.height {
        for x in 0..c.width {
            let p = px(&c, x, y);
            if p[2] > 150 && p[0] < 80 {
                blue += 1;
            }
        }
    }
    assert_eq!(blue, 0, "second-declared blue never surfaces under an identical red");
}

/// #76: the CSS blur radius sizes the padded scratch tile directly, so
/// an absurd radius used to mean an absurd allocation
/// (`text-shadow: 0 0 100000px` ≈ tens of GB). Two guards, pinned
/// behaviorally: the effective radius is clamped (100000 px renders
/// pixel-identically to 256 px — same clamped value, same code path),
/// and a padded tile past the scratch budget paints the unfeathered
/// copy — byte-identical to what blur 0 paints.
#[test]
fn text_shadow_blur_clamped_and_budgeted() {
    let fonts = crate::diting_fonts::font_book();
    let render = |blur: f32| {
        let mut c = Canvas::new_filled(200, 80, [255, 255, 255, 255]);
        execute(
            &shadow_item(vec![TextShadow { dx: 0.0, dy: 14.0, blur, color: crate::diting_css::Color(255, 0, 0, 255) }]),
            &fonts,
            &mut c,
        );
        reds(&c)
    };
    assert_eq!(render(100_000.0), render(MAX_SHADOW_BLUR), "absurd blur clamps to the same effective radius");

    // Budget: a 3000-glyph single-line run (wrap_at beyond the line) at
    // the clamped radius overshoots the scratch tile budget — the
    // fallback paints the unfeathered copy, exactly what blur 0 paints.
    let wide = |blur: f32| {
        let items = vec![PaintItem::Text {
            text: "m".repeat(3000),
            font_size: 16.0,
            bold: false,
            color: [0, 0, 0, 255],
            line_height: 20.0,
            x: 10.0,
            y: 20.0,
            wrap_at: 100_000.0,
            gradient: None,
            decorations: TextDecorations::default(),
            mono: false,
            word_spacing: 0.0,
            truncate_at: None,
            tokens: None,
            ws: WhiteSpace::Normal,
            text_shadow: Some(vec![TextShadow { dx: 0.0, dy: 0.0, blur, color: crate::diting_css::Color(255, 0, 0, 255) }]),
            small_caps: false,
            han: None,
        }];
        let mut c = Canvas::new_filled(256, 64, [255, 255, 255, 255]);
        execute(&items, &fonts, &mut c);
        reds(&c)
    };
    let budgeted = wide(MAX_SHADOW_BLUR);
    assert_eq!(budgeted, wide(0.0), "budget-overshooting tile paints the unfeathered copy");
    assert!(!budgeted.is_empty(), "fallback still draws shadow ink");
}

/// Chrome shadows the decorations with the glyphs (blitz#984): a hard
/// layer restamps underline/overline/line-through strokes in the shadow
/// color at the same offset. Underline-only ink (present in a decorated
/// render, absent undecorated) must appear red-shifted — every stroke
/// pixel carries a red twin at (+8, 0).
#[test]
fn text_shadow_underlines_are_shadowed() {
    let fonts = crate::diting_fonts::font_book();
    let item = |decorated: bool, shadow: Option<TextShadow>| {
        vec![PaintItem::Text {
            text: "mmmmm".into(),
            font_size: 16.0,
            bold: false,
            color: [0, 0, 0, 255],
            line_height: 20.0,
            x: 10.0,
            y: 20.0,
            wrap_at: 400.0,
            gradient: None,
            decorations: TextDecorations { underline: decorated, ..TextDecorations::default() },
            mono: false,
            word_spacing: 0.0,
            truncate_at: None,
            tokens: None,
            ws: WhiteSpace::Normal,
            text_shadow: shadow.map(|sh| vec![sh]),
            small_caps: false,
            han: None,
        }]
    };
    let render = |items: &[PaintItem]| {
        let mut c = Canvas::new_filled(140, 40, [255, 255, 255, 255]);
        execute(items, &fonts, &mut c);
        c
    };
    let ink = |c: &Canvas| {
        (0..c.height)
            .flat_map(|y| (0..c.width).map(move |x| (x, y)))
            .filter(|(x, y)| px(c, *x, *y)[0] < 255)
            .collect::<std::collections::HashSet<(usize, usize)>>()
    };
    let decorated_ink = ink(&render(&item(true, None)));
    let plain_ink = ink(&render(&item(false, None)));
    let stroke_only: Vec<(usize, usize)> = decorated_ink.difference(&plain_ink).copied().collect();
    assert!(!stroke_only.is_empty(), "the underline contributes stroke-only ink");

    let shadowed = render(&item(true, Some(TextShadow { dx: 8.0, dy: 0.0, blur: 0.0, color: crate::diting_css::Color(255, 0, 0, 255) })));
    // Twins landing under real ink (glyphs or the real stroke itself) are
    // covered — the shadow rides UNDER the content, like Chrome. The ones
    // in letter gaps must be red, and at least one must be visible or the
    // stroke restamp isn't happening at all.
    let mut visible = 0;
    for (x, y) in &stroke_only {
        let twin = (x + 8, *y);
        if twin.0 >= shadowed.width || decorated_ink.contains(&twin) {
            continue;
        }
        assert!(
            px(&shadowed, twin.0, twin.1)[0] > px(&shadowed, twin.0, twin.1)[1],
            "underline pixel ({x},{y}) has its red shadow twin at (+8,0)"
        );
        visible += 1;
    }
    assert!(visible > 0, "some stroke twin is visible in letter gaps");
}

/// The separable box blur preserves the plane's total mass up to edge
/// clamping and is idempotent-flat on a constant plane.
#[test]
fn box_blur_alpha_preserves_mass_and_flat() {
    let src = vec![0u8; 100];
    let flat = vec![200u8; 100];
    assert_eq!(box_blur_alpha(&flat, 10, 10, 2, true), flat, "constant plane stays constant");
    // Non-square plane: the vertical pass smears down the lit column
    // only. Guards the transposed-stride bug (a square buffer hides it
    // because w == h makes row and column strides coincide).
    let mut col = vec![0u8; 21]; // 7 wide, 3 tall
    col[7 + 2] = 255; // row 1, col 2
    let out = box_blur_alpha(&col, 7, 3, 1, false);
    for x in 0..7 {
        for y in 0..3 {
            let expect = if x == 2 { [127u8, 85, 127][y] } else { 0 };
            assert_eq!(out[y * 7 + x], expect, "vertical blur at ({x},{y})");
        }
    }
    let mut one = vec![0u8; 100];
    one[45] = 255;
    let out = box_blur_alpha(&one, 10, 10, 2, true);
    let total: u32 = out.iter().map(|&v| v as u32).sum();
    assert!(total > 200 && total <= 255 * 5, "mass spreads into the window, got {total}");
    assert!(out[45] < 255, "peak diluted");
    let empty: Vec<u8> = box_blur_alpha(&src, 10, 10, 1, false).to_vec();
    assert!(empty.iter().all(|&v| v == 0), "zero plane stays zero");
}

