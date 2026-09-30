//! Screencast + frame-encode family, split out of page.rs (god-file ratchet,
//! ARCHITECTURE.md §6 P2): the pure PNG/JPEG encoders `captureScreenshot` and
//! the screencast pump share, plus the 33 ms-tick frame pump itself. Compiles
//! only under the `screenshot` feature — the `mod screencast;` declaration in
//! page.rs is the single gate.
//!
//! Child of the page domain on purpose: the pump emits through the parent's
//! private `emit` helper so browser- vs session-level event stamping stays in
//! one place.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};

use crate::cdp::dispatch::{CdpContext, ScreencastState};

use super::{emit, now_epoch_seconds};

/// `quality` param: JPEG quality 0-100 (Chrome default 100).
pub(super) fn quality_param(params: &Value) -> u8 {
    params
        .get("quality")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(0, 100) as u8
}

/// Re-encode an already-encoded PNG as jpeg for the legacy full-page path,
/// whose renderer hands back `RenderedScreenshot::png` (compressed bytes, not
/// a raw frame buffer — that distinction is the whole point of this helper).
pub(super) fn encode_legacy_jpeg(png: &[u8], quality: u8) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(png).map_err(|e| format!("jpeg decode: {e}"))?;
    let mut out = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    img.to_rgb8()
        .write_with_encoder(encoder)
        .map_err(|e| format!("jpeg encode: {e}"))?;
    Ok(out.into_inner())
}

/// Encode a band frame as png (default) or jpeg (`quality`), mirroring the
/// render path's encoder settings.
pub(super) fn encode_frame(
    format: &str,
    quality: u8,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> Result<Vec<u8>, String> {
    if format == "jpeg" || format == "jpg" {
        let img = image::RgbaImage::from_raw(width, height, rgba)
            .ok_or_else(|| "frame buffer size mismatch".to_string())?;
        let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
        let mut out = std::io::Cursor::new(Vec::new());
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        rgb.write_with_encoder(encoder)
            .map_err(|e| format!("jpeg encode: {e}"))?;
        Ok(out.into_inner())
    } else {
        let mut png_bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(
                std::io::Cursor::new(&mut png_bytes),
                width.max(1),
                height.max(1),
            );
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .map_err(|e| format!("png encode header: {e}"))?;
            writer
                .write_image_data(&rgba)
                .map_err(|e| format!("png encode: {e}"))?;
        }
        Ok(png_bytes)
    }
}

/// Downscale a frame into `max_width`/`max_height` (0 = unconstrained),
/// keeping aspect via `imageops::thumbnail` (area-average, like Chrome's
/// screencast downscale).
pub(super) fn maybe_downscale(
    max_width: u32,
    max_height: u32,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> (u32, u32, Vec<u8>) {
    let mut scale = 1.0f32;
    if max_width > 0 {
        scale = scale.min(max_width as f32 / width.max(1) as f32);
    }
    if max_height > 0 {
        scale = scale.min(max_height as f32 / height.max(1) as f32);
    }
    if scale >= 1.0 {
        return (width, height, rgba);
    }
    let Some(img) = image::RgbaImage::from_raw(width, height, rgba) else {
        return (width, height, Vec::new());
    };
    let nw = ((width as f32 * scale).floor() as u32).max(1);
    let nh = ((height as f32 * scale).floor() as u32).max(1);
    let thumb = image::imageops::thumbnail(&img, nw, nh);
    (nw, nh, thumb.into_raw())
}

/// Produce one screencast frame per armed, unacked, damaged page. Called on
/// the connection loop's 33 ms tick and once immediately from
/// `startScreencast` (Chrome emits the first frame right away). Damage =
/// (dom epoch, scroll, viewport): a static page costs zero frames, a scroll
/// or mutation re-blits — frame cost is independent of page height.
/// Per-tick JS settle budget for armed screencast pages: long enough for a
/// due timer to fire inside the poll, short enough that several armed pages
/// still fit inside the 33 ms pump cadence.
const SCREENCEAST_SETTLE_MS: u64 = 5;

pub(crate) async fn pump_screencast_frames(ctx: &mut CdpContext) {
    if ctx.screencast.is_empty() {
        return;
    }
    let entries: Vec<(String, ScreencastState)> = ctx
        .screencast
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (page_id, mut st) in entries {
        // Drive the page's JS event loop a notch so page-driven damage
        // (timers, animations) progresses without a client message: dispatch
        // is otherwise the loop's only poller, and a silent connection
        // freezes a self-updating page — frames flowed only after a
        // heartbeat evaluate on the AginxOS real-device report. Runs before
        // the ack gate on purpose: a slow-acking client must not freeze the
        // page's own time. Idle pages return from settle immediately.
        if let Some(page) = ctx.get_page_mut(&page_id) {
            page.settle(SCREENCEAST_SETTLE_MS).await;
        }
        if st.outstanding_ack {
            continue;
        }
        // Damage signature first (immutable page access — cheap skip before
        // any layout/paint work). A page id with no page behind it (closed
        // while the cast was armed) drops its state here, or the pump would
        // keep waking every tick for a dead id.
        let (gen, epoch, rev, ox, oy) = {
            let Some(page) = ctx.get_page(&page_id) else {
                ctx.screencast.remove(&page_id);
                continue;
            };
            (
                page.realm_gen(),
                page.dom_epoch(),
                page.layout_rev(),
                page.scroll_offset().0,
                page.scroll_offset().1,
            )
        };
        let (vw, vh) = {
            let Some(page) = ctx.get_page(&page_id) else { continue };
            page.effective_viewport()
        };
        let sig = (gen, epoch, rev, ox, oy, vw, vh);
        if st.last_damage == Some(sig) {
            continue;
        }
        // everyNthFrame: count *changed* frames; the first changed frame
        // always emits, then every Nth after that.
        st.frame_seq += 1;
        let nth_hit = (st.frame_seq - 1) % st.every_nth_frame.max(1) as u64 == 0;
        st.last_damage = Some(sig);
        if !nth_hit {
            ctx.screencast.insert(page_id, st);
            continue;
        }
        let produced = {
            let Some(page) = ctx.get_page(&page_id) else { continue };
            match page.viewport_band_frame(ox, oy, (vw, vh)) {
                None => None,
                Some((frame, missing)) => {
                    if !missing.is_empty() {
                        page.fetch_band_images(missing).await;
                        page.viewport_band_frame(ox, oy, (vw, vh)).map(|(f, _)| f)
                    } else {
                        Some(frame)
                    }
                }
            }
        };
        let Some(frame) = produced else {
            // No live document yet (pre-navigation): persist the signature
            // so the pump doesn't retry a doomed produce every tick — the
            // next epoch/scroll change re-arms it.
            ctx.screencast.insert(page_id, st);
            continue;
        };
        let (w, h, rgba) = maybe_downscale(
            st.max_width,
            st.max_height,
            frame.width,
            frame.height,
            frame.rgba,
        );
        let data = match encode_frame(&st.format, st.quality, w, h, rgba) {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!("screencast frame encode failed: {e}");
                continue;
            }
        };
        st.outstanding_ack = true;
        let params = json!({
            "data": BASE64.encode(&data),
            "metadata": {
                "offsetTop": 0,
                "pageScaleFactor": 1,
                "deviceWidth": vw as i64,
                "deviceHeight": vh as i64,
                "scrollOffsetX": frame.dx as i64,
                "scrollOffsetY": frame.dy as i64,
                "timestamp": now_epoch_seconds(),
            },
            "sessionId": st.frame_seq as i64,
        });
        emit(ctx, "Page.screencastFrame", params, &st.session_id);
        ctx.screencast.insert(page_id, st);
    }
}
