//! Native form-control painting (checkables, selects, text fields with
//! their caret). Split from paint.rs — ARCHITECTURE.md §6 P2 god-file
//! ratchet (#86).
use super::paint::{alpha_color, est_width, Canvas};
use super::text::{greedy_wrap, tokens_of, FontBook};
use super::{FormRun, FormWidget};
use crate::diting_css::WhiteSpace;

/// Draw a checkable input's native widget (form paint batch): a bordered
/// box — square with a ✓ for checkboxes, circular ring for radios — with
/// the checked state carried in ink. The look is Chrome's neutral light
/// default (white field, gray border, dark mark) rather than any platform
/// accent, so it reads on both light and dark pages. Works in whatever
/// coordinate space `out` is in: the direct path passes page-band coords,
/// the affine path a local scratch at (0, 0).
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_form_widget(
    out: &mut Canvas,
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    widget: FormWidget,
    fonts: &FontBook,
    alpha: f32,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let (radio, checked, range) = match widget {
        FormWidget::Checkbox { checked } => (false, checked, None),
        FormWidget::Radio { checked } => (true, checked, None),
        FormWidget::Range { fraction } => (false, false, Some(fraction)),
    };
    // A slider (blitz#456) has no outer shell: a 4px track spanning the box
    // inset by the thumb radius, a filled leading segment and a 14px thumb
    // — the same neutral gray ramp as the checkables (light track, gray
    // fill, ink thumb) reads on both light and dark pages.
    if let Some(fraction) = range {
        let track = alpha_color([203, 203, 203, 255], alpha);
        let border = alpha_color([118, 118, 118, 255], alpha);
        let ink = alpha_color([26, 26, 26, 255], alpha);
        let thumb_r = 7i64;
        let cy = y + h / 2;
        let tx = x + thumb_r;
        let tw = (w - 2 * thumb_r).max(1);
        let thumb_cx = tx + ((tw as f32 * fraction.clamp(0.0, 1.0)).round() as i64);
        out.fill_rounded_rect(tx, cy - 2, tw, 4, 2.0, track);
        out.fill_rounded_rect(tx, cy - 2, (thumb_cx - tx).max(0), 4, 2.0, border);
        out.fill_rounded_rect(
            thumb_cx - thumb_r,
            cy - thumb_r,
            thumb_r * 2,
            thumb_r * 2,
            thumb_r as f32,
            ink,
        );
        return;
    }
    let border = alpha_color([118, 118, 118, 255], alpha);
    let fill = alpha_color([255, 255, 255, 255], alpha);
    let ink = alpha_color([26, 26, 26, 255], alpha);
    // fill_rounded_rect clamps the radius to half the shorter side, so a
    // radio's w/2 is the full circle; the inset re-fill keeps a 1px ring.
    let radius = if radio { w.min(h) as f32 / 2.0 } else { 2.0 };
    out.fill_rounded_rect(x, y, w, h, radius, border);
    out.fill_rounded_rect(x + 1, y + 1, w - 2, h - 2, (radius - 1.0).max(0.0), fill);
    if !checked {
        return;
    }
    if radio {
        let d = (w.min(h) - 6).max(2);
        out.fill_rounded_rect(
            x + (w - d) / 2,
            y + (h - d) / 2,
            d,
            d,
            d as f32 / 2.0,
            ink,
        );
    } else {
        // The bundled symbol set carries ✓ (the font-fallback batch's
        // coverage tail), and the raster cache (#399) makes the repeat free.
        // Centering goes by the tile's ink bbox, not its full line box —
        // the cramped CJK metrics leave the glyph off-center inside the box.
        let fs = (h as f32 * 0.82).max(6.0);
        let r = fonts.rasterize("✓", fs, false, ink, fs * 1.45, false);
        if let Some((bx0, by0, bx1, by1)) = r.ink_bbox() {
            let iw = (bx1 - bx0 + 1) as i64;
            let ih = (by1 - by0 + 1) as i64;
            let tx = x + (w - iw) / 2 - bx0 as i64;
            let ty = y + (h - ih) / 2 - by0 as i64;
            out.blit_text(&r, tx, ty);
        }
    }
}

/// The closed select's dropdown mark (form paint polish batch): a 7×4
/// solid ▼ centered in the 16px zone at the box's right, in the border
/// gray — hard-edged rows like every other primitive here.
fn paint_select_arrow(out: &mut Canvas, x: i64, y: i64, w: i64, h: i64, alpha: f32) {
    let cx = x + w - 9;
    let cy = y + h / 2;
    let color = alpha_color([118, 118, 118, 255], alpha);
    for (row, width) in [7i64, 5, 3, 1].into_iter().enumerate() {
        out.fill_rect(cx - width / 2, cy - 2 + row as i64, width, 1, color);
    }
}

/// A text-carrying form control's default shell + run layout (form paint
/// polish batch): Chrome's 1px gray ring with a white field (the light
/// button face for buttons), the run inset by the control's 2px padding —
/// vertically centered for single-line controls, top-anchored for textarea
/// — and a select's arrow in its reserved right zone. The shell only
/// paints while `fill` is set (the author styled no background of their
/// own — their bg/border already read as the box). Works in whatever
/// coordinate space `out` is in, like [`paint_form_widget`].
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_form_control(
    out: &mut Canvas,
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    run: Option<&(String, f32, bool, f32, [u8; 4])>,
    form: FormRun,
    fill: bool,
    fonts: &FontBook,
    alpha: f32,
    caret: Option<(usize, [u8; 4])>,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    if fill {
        let border = alpha_color([118, 118, 118, 255], alpha);
        let field = alpha_color(
            match form {
                FormRun::Button => [239, 239, 239, 255],
                _ => [255, 255, 255, 255],
            },
            alpha,
        );
        out.fill_rounded_rect(x, y, w, h, 2.0, border);
        out.fill_rounded_rect(x + 1, y + 1, w - 2, h - 2, 1.0, field);
    }
    if form == FormRun::Select {
        paint_select_arrow(out, x, y, w, h, alpha);
    }
    // Metrics come from the run; the fallback pair only carries a caret
    // whose collect site failed to synthesize a run (the collect path
    // guarantees one whenever a caret is present, so the empty text below
    // keeps the fallback from ever painting).
    let (text, font_size, bold, line_height, color) = match run {
        Some((t, fs, b, lh, c)) => (t.as_str(), *fs, *b, *lh, *c),
        None => ("", 16.0, false, 19.0, [0, 0, 0, 255]),
    };
    // Ink stays inside the field: wrap at the box minus the padding, and
    // the select additionally reserves its arrow zone.
    let wrap_at = match form {
        FormRun::Select => (w - 20).max(0),
        FormRun::Button => w,
        _ => (w - 4).max(0),
    };
    // The line-box top the tile hangs from: centered for the single-line
    // controls ((h − lh)/2, symmetric overflow when the box runs shorter
    // than the line), 2px below the top edge for textarea. Button labels
    // also center horizontally, at the estimator width the band prefilter
    // uses — close enough to the ink the rasterizer will lay down.
    let (tx, ty) = match form {
        FormRun::Button => {
            let est = est_width(text, font_size).min(w as f32);
            (x as f32 + ((w as f32 - est) / 2.0).max(2.0), y as f32 + (h as f32 - line_height) / 2.0)
        }
        FormRun::Textarea => (x as f32 + 2.0, y as f32 + 2.0),
        _ => (x as f32 + 2.0, y as f32 + (h as f32 - line_height) / 2.0),
    };
    if !text.trim().is_empty() {
        let r = fonts.rasterize_wrapped(
            text,
            font_size,
            bold,
            alpha_color(color, alpha),
            wrap_at.max(1) as f32,
            line_height,
            false,
            0.0, // control labels carry no inherited word-spacing (v1 boundary)
            None, // control labels never truncate (input text-overflow is a v2 face)
            WhiteSpace::Normal, // control labels always collapse (textarea value editing is a v2 face)
            false, // control labels never synthesize small-caps
        );
        out.push_clip(x + 1, y + 1, x + w - 1, y + h - 1);
        out.blit_text(&r, tx.round() as i64, (ty + r.top).round() as i64);
        out.pop_clip();
    }
    // Caret (typing-cursor batch): a 1px bar the line height tall, riding
    // the same token/wrap walk the rasterizer lays the run down with, so
    // it stands exactly between the glyphs the offset names — including
    // on an empty value, where the run above paints nothing. Always on
    // (no blink): a screenshot is one instant, and Chrome's 50%-duty
    // blink would make the caret randomly vanish from captures. Chrome
    // colors it caret-color (default: the used color); collect passes
    // the ink so the placeholder gray can never leak into the bar.
    if let Some((off, ink)) = caret {
        if matches!(form, FormRun::Input | FormRun::Textarea) {
            let tokens = tokens_of(text, font_size, bold, fonts, false, 0.0, WhiteSpace::Normal, false);
            let lines = greedy_wrap(&tokens, Some(wrap_at.max(1) as f32), WhiteSpace::Normal);
            // tokens_of tokenizes the TRIMMED text: map the offset into
            // trimmed coordinates. A caret inside the leading whitespace
            // pins to the line start — accepted v1 imprecision.
            let leading = text.chars().count() - text.trim_start().chars().count();
            let off = off.saturating_sub(leading);
            // Which wrapped line holds the offset, and the ink width
            // before it on that line: consume whole tokens while their
            // cumulative chars stay at/below the offset; a mid-token
            // landing takes the proportional slice of that token's
            // advance. Walking painted tokens only (greedy_wrap drops
            // whitespace before breaks) keeps the caret glued to the
            // glyphs actually on screen.
            let mut line = 0usize;
            let mut x_before = 0.0f32;
            let mut cum = 0usize;
            'lines: for (li, l) in lines.iter().enumerate() {
                line = li;
                x_before = 0.0;
                for &ti in &l.token_idx {
                    let tok = &tokens[ti];
                    let n = tok.text.chars().count();
                    if cum + n <= off {
                        x_before += tok.width;
                        cum += n;
                    } else {
                        if off > cum {
                            x_before += tok.width * (off - cum) as f32 / n as f32;
                        }
                        break 'lines;
                    }
                }
            }
            // Input never wraps in Chrome (the text scrolls); the engine
            // has no text scroll, so a caret that wrapped past the first
            // line clamps to the right padding — the deterministic
            // stand-in. Every bar clamps into the field regardless.
            let bar_x = if form == FormRun::Input && line > 0 {
                x as f32 + w as f32 - 3.0
            } else {
                tx + x_before
            }
            .round();
            let lo = x as f32 + 2.0;
            let hi = (x as f32 + w as f32 - 3.0).max(lo);
            let bar_x = bar_x.max(lo).min(hi) as i64;
            let bar_y = (ty + line as f32 * line_height).round() as i64;
            let bar_h = line_height.ceil().max(1.0) as i64;
            out.push_clip(x + 1, y + 1, x + w - 1, y + h - 1);
            out.fill_rect(bar_x, bar_y, 1, bar_h, alpha_color(ink, alpha));
            out.pop_clip();
        }
    }
}
