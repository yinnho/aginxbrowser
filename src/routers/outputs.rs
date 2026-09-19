//! Render-product routes (ARCHITECTURE.md P2): /screenshot, /video, /pdf —
//! the pixel/encoding outputs of the pipeline, behind the `screenshot`
//! feature. Split from the crate root; behavior unchanged.
use axum::extract::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::robots;
use crate::server;
use crate::{spawn_blocking, AppError};

/// /screenshot request: render a page's JS-rendered DOM to a PNG.
#[cfg(feature = "screenshot")]
#[derive(Debug, Deserialize, Clone)]
pub struct ScreenshotRequest {
    pub url: String,
    /// Viewport width in CSS pixels. Default 1280.
    #[serde(default = "default_screenshot_width")]
    pub width: u32,
    /// Viewport height in CSS pixels. Default 800 (ignored when `full_page`).
    #[serde(default = "default_screenshot_height")]
    pub height: u32,
    /// Device pixel ratio. Default 1.0. Higher = sharper but larger PNG.
    #[serde(default = "default_screenshot_scale")]
    pub scale: f32,
    /// Capture the full scrolled page height (tracks computed content height,
    /// capped at 16000px) instead of just the viewport. Default true.
    #[serde(default = "default_screenshot_full_page")]
    pub full_page: bool,
    /// Extra seconds to wait for JS rendering after load before capturing.
    #[serde(default)]
    pub wait_secs: Option<u64>,
    /// CSS selector for element-level capture. Default (None): whole page.
    /// With `selector_all=false` the image is cropped to the first match and
    /// its rect is returned; with `selector_all=true` the image renders
    /// normally and rects for every match are returned.
    #[serde(default)]
    pub selector: Option<String>,
    /// With `selector`: report rects for ALL matches instead of cropping to
    /// the first. Default false.
    #[serde(default)]
    pub selector_all: bool,
    /// With `selector`: also run the diting layout engine over the page HTML
    /// and return its rects in `selector_rects_diting` (an independent
    /// cross-check of the Blitz pipeline). Default false.
    #[serde(default)]
    pub diting_rects: bool,
    /// Render engine: "diting" (default — our own css+layout+paint stack,
    /// no Stylo/vello/parley in the path) or "blitz" (the Blitz reference
    /// pipeline via the pinned rev, for comparison renders; requires the
    /// `blitz-reference` feature at build time — without it the request
    /// errors instead of silently switching engines).
    #[serde(default)]
    pub engine: Option<String>,
    /// Route through AGINXBROWSER_PROXY. Default false (direct).
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to inject before navigation. Entries are `"name=value"`
    /// strings or CDP-style objects `{"name","value","domain",...}`.
    #[serde(default, deserialize_with = "crate::server::cookie_list_from_json")]
    pub cookies: Vec<String>,
    /// TLS fingerprint override (stealth mode only).
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
}

#[cfg(feature = "screenshot")]
fn default_screenshot_width() -> u32 {
    1280
}
#[cfg(feature = "screenshot")]
fn default_screenshot_height() -> u32 {
    800
}
#[cfg(feature = "screenshot")]
fn default_screenshot_scale() -> f32 {
    1.0
}
#[cfg(feature = "screenshot")]
fn default_screenshot_full_page() -> bool {
    true
}

#[cfg(feature = "screenshot")]
fn default_video_fps() -> f64 {
    24.0
}
#[cfg(feature = "screenshot")]
fn default_video_width() -> u32 {
    1280
}
#[cfg(feature = "screenshot")]
fn default_video_height() -> u32 {
    720
}
#[cfg(feature = "screenshot")]
fn default_video_hold_tail_secs() -> f64 {
    0.5
}
#[cfg(feature = "screenshot")]
fn default_video_max_duration_secs() -> f64 {
    120.0
}
#[cfg(feature = "screenshot")]
fn default_video_wait_timelines_ms() -> u64 {
    10_000
}
#[cfg(feature = "screenshot")]
fn default_audio_volume() -> f32 {
    1.0
}
#[cfg(feature = "screenshot")]
fn default_audio_loop() -> bool {
    true
}

/// /video `audio`: background track fetched through the page's HTTP client
/// and muxed in — looped by default, volume-scaled, optionally faded out
/// over the final seconds.
#[cfg(feature = "screenshot")]
#[derive(Debug, Deserialize, Clone)]
pub struct VideoAudioRequest {
    pub url: String,
    /// Linear multiplier 0..=2; 1 = as authored. Default 1.
    #[serde(default = "default_audio_volume")]
    pub volume: f32,
    /// Fade out over the final N seconds. Default 0 (none).
    #[serde(default)]
    pub fade_out_secs: f32,
    /// Loop to cover the whole video. Default true.
    #[serde(default = "default_audio_loop")]
    pub loop_audio: bool,
}

/// /video `narration[]`: one voiceover clip placed at a start time —
/// generate with any TTS, hand us the URL; all clips mix into one AAC track.
#[cfg(feature = "screenshot")]
#[derive(Debug, Deserialize, Clone)]
pub struct VideoNarrationClip {
    pub url: String,
    /// Seconds from video t=0 where this clip starts. Default 0.
    #[serde(default)]
    pub start_secs: f64,
    /// Linear multiplier 0..=2. Default 1.
    #[serde(default = "default_audio_volume")]
    pub volume: f32,
}

#[cfg(feature = "screenshot")]
fn default_pdf_width() -> u32 {
    794
}
#[cfg(feature = "screenshot")]
fn default_pdf_height() -> u32 {
    1123
}
#[cfg(feature = "screenshot")]
fn default_pdf_max_pages() -> usize {
    50
}
#[cfg(feature = "screenshot")]
fn default_pdf_jpeg_quality() -> u8 {
    90
}
#[cfg(feature = "screenshot")]
fn default_pdf_format() -> String {
    "pdf".to_string()
}

/// /pdf request: cut the page into pages and package as PDF (default) or
/// per-page PNGs. No `selector` → print mode (fixed-height pages, breaks at
/// top-level block boundaries); `selector` → slides mode (one page per
/// match, sized to the element). Requires the `screenshot` feature.
#[cfg(feature = "screenshot")]
#[derive(Debug, Deserialize, Clone)]
pub struct PdfRequest {
    pub url: String,
    /// Output format: `"pdf"` (default), `"png"` (one base64 PNG per page),
    /// `"pptx"` (one slide per page, image-based), `"pptx-native"` (editable:
    /// element-level DrawingML, requires `selector`), or `"docx"` (one
    /// page-sized section per page).
    #[serde(default = "default_pdf_format")]
    pub format: String,
    /// Page width in CSS pixels. Default 794 (A4 @96dpi).
    #[serde(default = "default_pdf_width")]
    pub width: u32,
    /// Page height in CSS pixels — print pagination only (slides size each
    /// page to its element). Default 1123 (A4 @96dpi).
    #[serde(default = "default_pdf_height")]
    pub height: u32,
    /// CSS selector; present → slides mode (one page per match).
    #[serde(default)]
    pub selector: Option<String>,
    /// Safety cap on emitted pages. Default 50.
    #[serde(default = "default_pdf_max_pages")]
    pub max_pages: usize,
    /// JPEG quality for PDF page embedding (1-100). Default 90.
    #[serde(default = "default_pdf_jpeg_quality")]
    pub jpeg_quality: u8,
    /// Route through AGINXBROWSER_PROXY. Default false (direct).
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to inject before navigation (`"name=value"` strings or CDP-style
    /// objects) — for pages behind a login.
    #[serde(default, deserialize_with = "crate::server::cookie_list_from_json")]
    pub cookies: Vec<String>,
    /// TLS fingerprint override (stealth mode only).
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
}

/// /pdf response: PDF, per-page PNGs, or PPTX/DOCX container, base64.
/// Exactly one of `pdf_base64` / `pages_base64` / `pptx_base64` /
/// `docx_base64` is present (per the requested format).
#[cfg(feature = "screenshot")]
#[derive(Debug, Serialize)]
pub struct PdfResponse {
    pub url: String,
    pub title: Option<String>,
    /// Number of pages emitted.
    pub pages: usize,
    /// Requested page width in CSS px (print mode; slides pages vary in
    /// height per element).
    pub width: u32,
    pub height: u32,
    /// Base64-encoded PDF bytes (`format:"pdf"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf_base64: Option<String>,
    /// One base64 PNG per page (`format:"png"`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pages_base64: Vec<String>,
    /// Base64-encoded PPTX bytes — `format:"pptx"` is image-based (one
    /// slide per page); `format:"pptx-native"` is editable DrawingML.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pptx_base64: Option<String>,
    /// Base64-encoded DOCX bytes (`format:"docx"`) — one page-sized section
    /// per page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docx_base64: Option<String>,
    /// Echoes the requested output format ("pdf" / "png" / "pptx" /
    /// "pptx-native" / "docx").
    pub format: String,
}

/// /video request: render the page's registered timelines
/// (`window.__timelines` — GSAP-style objects with `duration()` + `pause(t)`)
/// to an MP4. The seek protocol is deterministic — frame t = i/fps, no wall
/// clock in the pixel values. Requires ffmpeg on PATH.
#[cfg(feature = "screenshot")]
#[derive(Debug, Deserialize, Clone)]
pub struct VideoRequest {
    pub url: String,
    /// Frames per second. Default 24.
    #[serde(default = "default_video_fps")]
    pub fps: f64,
    /// Viewport width in CSS pixels (floored to even — yuv420p). Default 1280.
    #[serde(default = "default_video_width")]
    pub width: u32,
    /// Viewport height in CSS pixels. Default 720.
    #[serde(default = "default_video_height")]
    pub height: u32,
    /// Freeze the final timeline state for this many extra seconds. Default 0.5.
    #[serde(default = "default_video_hold_tail_secs")]
    pub hold_tail_secs: f64,
    /// Safety cap on timeline + hold tail, seconds. Default 120.
    #[serde(default = "default_video_max_duration_secs")]
    pub max_duration_secs: f64,
    /// How long to wait for `window.__timelines` to appear, ms. Default 10000.
    #[serde(default = "default_video_wait_timelines_ms")]
    pub wait_timelines_ms: u64,
    /// Route through AGINXBROWSER_PROXY. Default false (direct).
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to inject before navigation (`"name=value"` strings or CDP-style
    /// objects) — for pages whose timelines sit behind a login.
    #[serde(default, deserialize_with = "crate::server::cookie_list_from_json")]
    pub cookies: Vec<String>,
    /// TLS fingerprint override (stealth mode only).
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
    /// Background music track: looped by default, faded at the tail.
    #[serde(default)]
    pub audio: Option<VideoAudioRequest>,
    /// Voiceover clips, each starting at its own time (mixed into one track).
    #[serde(default)]
    pub narration: Vec<VideoNarrationClip>,
    /// Inline SRT subtitles muxed as a soft (toggleable) mov_text track.
    #[serde(default)]
    pub subtitles_srt: Option<String>,
    /// ISO language tag for the subtitle track, e.g. "eng" / "zh".
    #[serde(default)]
    pub subtitles_language: Option<String>,
    /// Burn the cues into the frame pixels too (hardsub) — on by default
    /// when `subtitles_srt` is present; QuickTime, WeChat and most social
    /// embeds ignore the soft track. `false` keeps the soft track only.
    #[serde(default)]
    pub burn_subtitles: Option<bool>,
}

/// /video response: MP4 encoded as base64 (`base64 -d > out.mp4` or
/// `<video src="data:video/mp4;base64,...">`).
#[cfg(feature = "screenshot")]
#[derive(Debug, Serialize)]
pub struct VideoResponse {
    pub url: String,
    pub title: Option<String>,
    /// Frames written to the encoder.
    pub frames: u32,
    /// Longest registered timeline, seconds.
    pub timeline_secs: f64,
    /// Total video length = timeline + hold tail, seconds.
    pub duration_secs: f64,
    /// Encoded pixel size (request viewport floored to even).
    pub width: u32,
    pub height: u32,
    /// Base64-encoded MP4 bytes (H.264, yuv420p).
    pub video_base64: String,
    /// Whether an audio track (BGM and/or narration) was muxed in.
    pub has_audio: bool,
    /// Whether a soft subtitle track was muxed in.
    pub has_subtitles: bool,
    /// Whether the cues were also burned into the frame pixels.
    pub burned_subtitles: bool,
    /// Always "mp4" for now.
    pub format: String,
}

/// /screenshot response: PNG encoded as base64 (so it rides in the existing
/// JSON API; clients `base64 -d` or `<img src="data:image/png;base64,...">`).
#[cfg(feature = "screenshot")]
#[derive(Debug, Serialize)]
pub struct ScreenshotResponse {
    pub url: String,
    pub title: Option<String>,
    /// Actual rendered pixel dimensions of the PNG (differs from the request
    /// when `full_page` tracks content height or a `selector` crop is used).
    pub width: u32,
    pub height: u32,
    /// Base64-encoded PNG bytes.
    pub image_base64: String,
    /// Always "png" for now.
    pub format: String,
    /// CSS-pixel rects (page-relative) for the `selector` match(es). Present
    /// only when a selector was given. Single match = the cropped region.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_rects: Option<Vec<crate::screenshot::ElementRect>>,
    /// The same rects computed by the diting engine (diting_dom/css/layout)
    /// as an independent pass over the page HTML — the Blitz/Stylo pipeline's
    /// cross-check. Present only when a selector was given and the request
    /// opted in with `diting_rects`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_rects_diting: Option<Vec<crate::screenshot::ElementRect>>,
}

#[cfg(feature = "screenshot")]
pub(crate) async fn screenshot_handler(
    Json(req): Json<ScreenshotRequest>,
) -> Result<impl IntoResponse, AppError> {
    robots::assert_allowed(&req.url)
        .await
        .map_err(AppError::Forbidden)?;
    // V8 (deno_core) holds !Send state, so drive the whole capture on a
    // current-thread runtime on a blocking thread — same pattern as do_eval.
    let resp = spawn_blocking(move || server::do_screenshot(req)).await??;
    Ok((StatusCode::OK, Json(resp)))
}

/// Timeline video: seek the page's registered timelines frame by frame and
/// encode the viewport bands to MP4 (ffmpeg pipe). Same threading pattern as
/// /screenshot — V8 is !Send.
#[cfg(feature = "screenshot")]
pub(crate) async fn video_handler(Json(req): Json<VideoRequest>) -> Result<impl IntoResponse, AppError> {
    robots::assert_allowed(&req.url)
        .await
        .map_err(AppError::Forbidden)?;
    let resp = spawn_blocking(move || server::do_video(req)).await??;
    Ok((StatusCode::OK, Json(resp)))
}

/// Page set: cut the page into pages (print pagination or per-selector
/// slides) and package as PDF / PNGs. Same threading pattern as /screenshot.
#[cfg(feature = "screenshot")]
pub(crate) async fn pdf_handler(Json(req): Json<PdfRequest>) -> Result<impl IntoResponse, AppError> {
    robots::assert_allowed(&req.url)
        .await
        .map_err(AppError::Forbidden)?;
    let resp = spawn_blocking(move || server::do_pdf(req)).await??;
    Ok((StatusCode::OK, Json(resp)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // /video defaults must all be materialized by serde (not left zeroed when
    // the caller omits them) — a 0 fps or 0-height request would produce a
    // broken encode, so the defaults ARE the contract.
    #[cfg(feature = "screenshot")]
    #[test]
    fn video_request_defaults_are_materialized() {
        let r: VideoRequest = serde_json::from_str(r#"{"url":"https://e.com/anim.html"}"#).unwrap();
        assert_eq!(r.fps, 24.0);
        assert_eq!((r.width, r.height), (1280, 720));
        assert_eq!(r.hold_tail_secs, 0.5);
        assert_eq!(r.max_duration_secs, 120.0);
        assert_eq!(r.wait_timelines_ms, 10_000);
        assert!(!r.use_proxy);
        assert!(r.cookies.is_empty());
    }

    // /pdf defaults likewise: a 0×0 page size or 0-page cap would break the
    // pagination math, so the serde defaults are the contract.

    #[cfg(feature = "screenshot")]
    #[test]
    fn pdf_request_defaults_are_materialized() {
        let r: PdfRequest = serde_json::from_str(r#"{"url":"https://e.com/doc.html"}"#).unwrap();
        assert_eq!(r.format, "pdf");
        assert_eq!((r.width, r.height), (794, 1123));
        assert!(r.selector.is_none());
        assert_eq!(r.max_pages, 50);
        assert_eq!(r.jpeg_quality, 90);
        assert!(!r.use_proxy);
        assert!(r.cookies.is_empty());
    }
}
