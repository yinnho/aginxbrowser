use std::sync::Arc;

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    tool, tool_handler, tool_router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpService, StreamableHttpServerConfig,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::render::smart_fetch;
use crate::server::{do_click, do_eval, do_search};
use crate::session::{self, SessionCommand};
use crate::{ClickRequest, EvalRequest, FetchRequest, OutputFormat, SearchRequest};

// ============================================================================
// Tool parameter structs (JsonSchema → auto-generated MCP input schemas)
// ============================================================================

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FetchParams {
    /// The URL to fetch
    pub url: String,
    /// Output format: "markdown", "html", or "text" (default: markdown)
    #[serde(default = "default_format")]
    pub format: String,
    /// CSS selector to extract specific content
    #[serde(default)]
    pub selector: Option<String>,
    /// Seconds to wait for JS rendering
    #[serde(default)]
    pub wait_secs: Option<u64>,
    /// Route through proxy (for blocked foreign sites)
    #[serde(default)]
    pub use_proxy: bool,
    /// Maximum characters to return (default: 50000)
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
    /// Auto-detect and bypass Cloudflare Turnstile challenges (default: true)
    #[serde(default = "default_true")]
    pub auto_bypass_challenge: bool,
    /// Rendering strategy: "auto" (default), "http", or "obscura"
    #[serde(default)]
    pub render_tier: crate::RenderTier,
    /// TLS fingerprint override (stealth mode only): "chrome145", "firefox133", etc.
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
    /// JS expression to extract from the page after rendering
    #[serde(default)]
    pub js_extract: Option<JsExtractParams>,
    /// Strip prompt-injection payloads from the text output (default true):
    /// zero-width/steganographic characters, instruction-shaped lines
    /// ("ignore previous instructions", chat markup tokens, CJK variants),
    /// and text hidden via opacity:0 / tiny fonts. A `sanitize_report`
    /// field counts what was removed — stripping is observable, never
    /// silent. Set false for raw output.
    #[serde(default = "default_true")]
    pub sanitize: bool,
    /// Capture script-initiated API responses: a list of URL substrings
    /// (e.g. ["/api/"]) whose matching fetch/XHR bodies come back in an
    /// `xhr` array; an empty list captures every XHR/Fetch. Forces browser
    /// rendering (script-initiated requests only exist after JS runs).
    #[serde(default)]
    pub capture_xhr: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct JsExtractParams {
    /// JavaScript expression to evaluate (e.g. "window.__INITIAL_STATE__")
    pub expression: String,
    /// Timeout in milliseconds (default: 5000)
    #[serde(default = "default_js_timeout")]
    pub timeout_ms: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EvalParams {
    /// The URL to load
    pub url: String,
    /// JavaScript code to execute (supports async/Promise)
    pub script: String,
    /// Seconds to wait before executing
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ClickParams {
    /// The URL to load
    pub url: String,
    /// CSS selector of element to click
    pub selector: String,
    /// Seconds to wait for the page to settle after load, before clicking
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Search query
    pub q: String,
    /// Fetch content for top N results
    #[serde(default)]
    pub fetch_top: usize,
    /// Search categories (default: general)
    #[serde(default = "default_categories")]
    pub categories: String,
    /// Maximum number of results (default: 10)
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    /// Max characters per result content
    #[serde(default = "default_max_chars_per")]
    pub max_chars_per: usize,
    /// Restrict to these engine names (e.g. ["baidu"], ["sogou_wechat"]).
    /// Empty = all engines serving `categories`. Invalid names return an
    /// error listing the valid ones.
    #[serde(default)]
    pub engines: Vec<String>,
    /// Freshness window: "day" | "week" | "month" | "year". Honored by
    /// engines with dated results (e.g. bing_news filters by pubDate);
    /// others ignore it.
    #[serde(default)]
    pub time_range: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DownloadParams {
    /// URL of the file to download (http/https)
    pub url: String,
    /// Explicit output filename. When omitted: Content-Disposition → URL tail → "download"
    #[serde(default)]
    pub filename: Option<String>,
    /// Resume an interrupted download when a local partial file exists
    #[serde(default)]
    pub resume: bool,
    /// Route through proxy (default: false; auto-enabled for known blocked domains)
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to send with the request: `"name=value"` strings or
    /// CDP-style objects `{"name","value","domain",...}` for gated downloads
    #[serde(default, deserialize_with = "crate::server::cookie_list_from_json")]
    pub cookies: Vec<String>,
}

// ---------------------------------------------------------------------------
// Session tool parameter structs
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionCreateParams {
    /// Initial URL to navigate to (optional)
    #[serde(default)]
    pub url: Option<String>,
    /// Route through proxy (default: false)
    #[serde(default)]
    pub use_proxy: bool,
    /// Cookies to inject before navigation: `"name=value"` strings or
    /// CDP-style objects `{"name","value","domain",...}`. Lets the session
    /// start already logged-in. Round-trips with session_cookies.
    #[serde(default, deserialize_with = "crate::server::cookie_list_from_json")]
    pub cookies: Vec<String>,
    /// Web Storage to inject after the initial navigation lands:
    /// {"local_storage": {"k":"v"}, "session_storage": {"k":"v"}}. For login
    /// states that live in localStorage rather than the cookie jar.
    /// Round-trips with session_storage.
    pub storage: Option<Value>,
    /// Idle time-to-live in seconds before the session is evicted
    /// (default: 480, clamped 60..3600). Raise it for long workflows.
    pub ttl_secs: Option<u64>,
    /// Initial viewport width in CSS pixels. Pinned for the session's life
    /// (survives navigation) so element rects and media queries anchor to
    /// the same layout across every page of the visit.
    pub width: Option<u32>,
    /// Initial viewport height in CSS pixels.
    pub height: Option<u32>,
    /// Mobile device emulation (coarse pointer, no hover) for the initial
    /// viewport.
    #[serde(default)]
    pub mobile: bool,
    /// Exempt the session from the idle reaper: it lives until
    /// session_close or server exit, so a workflow interrupted by long
    /// non-browser steps keeps its login state.
    #[serde(default)]
    pub keepalive: bool,
    /// Persist the login state (cookies + localStorage/sessionStorage +
    /// viewport + dialog policy) to the server's local store after every
    /// action. If the session idles out — or the whole server restarts —
    /// the next call with the same session_id revives it logged-in
    /// (storageState-style recovery, no re-login). Explicit session_close
    /// drops the snapshot.
    #[serde(default)]
    pub persistent: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionCookiesParams {
    /// Session ID
    pub session_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionCloneParams {
    /// Session ID to derive from (stays alive and untouched)
    pub session_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionConsoleParams {
    /// Session ID
    pub session_id: String,
    /// Only entries at this level: "log" | "info" | "warn" | "error"
    #[serde(default)]
    pub level: Option<String>,
    /// Only entries logged at or after this Unix epoch millisecond timestamp
    #[serde(default)]
    pub since_ts: Option<u64>,
    /// Only entries whose page URL contains this substring
    #[serde(default)]
    pub url_contains: Option<String>,
    /// Keep only the most recent N matching entries
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionDialogParams {
    /// Session ID
    pub session_id: String,
    /// "list" reports the policy and dialog history; "accept"/"dismiss" set
    /// the answer applied to subsequent window.confirm/prompt calls (alert
    /// is always logged, never blocking).
    pub action: String,
    /// With action "accept": text window.prompt returns once accepted
    /// (omitted keeps the current text).
    #[serde(default)]
    pub prompt_text: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionNavigateParams {
    /// Session ID
    pub session_id: String,
    /// URL to navigate to
    pub url: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionStateParams {
    /// Session ID
    pub session_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionClickParams {
    /// Session ID
    pub session_id: String,
    /// Element index (from /state output)
    pub index: usize,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionClickXyParams {
    /// Session ID
    pub session_id: String,
    /// Viewport X coordinate in CSS pixels
    pub x: f64,
    /// Viewport Y coordinate in CSS pixels
    pub y: f64,
    /// Mouse button: "left" (default), "right", "middle"
    #[serde(default)]
    pub button: Option<String>,
    /// Click count: 1 single (default), 2 adds dblclick, 3+ sets detail
    #[serde(default)]
    pub click_count: Option<u32>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionXy {
    /// X coordinate in viewport CSS pixels
    pub x: f64,
    /// Y coordinate in viewport CSS pixels
    pub y: f64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionDragParams {
    /// Session ID
    pub session_id: String,
    /// Where to press the mouse button down
    pub from: SessionXy,
    /// Where to release it
    pub to: SessionXy,
    /// Interpolated mousemove events between from and to (default 10)
    #[serde(default)]
    pub steps: Option<u32>,
    /// Delay between moves in ms (default 30) — gives mousemove-driven
    /// widgets time to react per step
    #[serde(default)]
    pub delay_ms: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionInputParams {
    /// Session ID
    pub session_id: String,
    /// Element index (from /state output)
    pub index: usize,
    /// Text to type into the input field
    pub text: String,
    /// Event fidelity: "full" types one character at a time with a
    /// keydown/keypress/input/keyup cycle per character, for pages whose
    /// listeners key on keyboard events (e.g. keypress-Enter login forms).
    /// Default fires a single input+change pair after the value is set.
    #[serde(default)]
    pub events: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionScrollParams {
    /// Session ID
    pub session_id: String,
    /// Scroll direction: "up" or "down" (default: down)
    #[serde(default = "default_scroll_dir")]
    pub direction: String,
    /// Scroll amount in viewport-heights (default: 3)
    #[serde(default = "default_scroll_amount")]
    pub amount: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionEvalParams {
    /// Session ID
    pub session_id: String,
    /// JavaScript code to execute
    pub script: String,
    /// Await budget for the script's promise in ms (default 5000, clamped
    /// 100..120000). Pass a larger budget for slow page-side work such as
    /// uploads through the page's own fetch; on expiry the tool errors with
    /// EVAL_TIMEOUT (the script may still be running) instead of returning
    /// a null result.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RenderMarkdownParams {
    /// Full markdown document. Prose rides a plain offline shell; archify
    /// fenced code blocks carry typed zero-coordinate diagram JSON and
    /// render to inline SVG.
    pub markdown: String,
    /// Color theme: "light" (default) or "dark" — the shell background/
    /// foreground and every SVG palette slot swap together; the receipt
    /// records which theme produced the bytes
    pub theme: Option<String>,
    /// Visual preset: "classic" (default), "signal-flow", "blueprint", or
    /// "editorial" — a palette family orthogonal to theme (each preset
    /// exists in both light and dark). The receipt records preset and
    /// theme separately
    pub preset: Option<String>,
    /// Optional session ID: also load the rendered HTML into that live
    /// session (local and free) so session_screenshot / session_state can
    /// verify the artifact
    pub session_id: Option<String>,
    /// Quality profile for the composition audit: "standard" (default) or
    /// "showcase" — the delivery gate. The audit grades route crossings,
    /// corridors, label clearance, rhythm, and projected text size in the
    /// receipt (diagrams[].composition); it never changes the artifact
    /// bytes, only how findings are severity-rated
    pub quality: Option<String>,
    /// Bake the entrance choreography into the artifact (default false):
    /// pure-declarative CSS animation — headings split into per-glyph/per-
    /// word spans that rise in with expo easing, prose blocks stagger up a
    /// nth-child delay ladder, and diagram figures grow in with a back
    /// ease (GSAP's easing math as public cubic-bezier equivalents). The
    /// diagrams animate too, on one story clock: nodes pop in one beat at
    /// a time, solid edges draw themselves (dash-offset drain), dashed
    /// returns fade, sequence messages land as they are "sent", and a
    /// timed caption strip under each figure subtitles the beats — under
    /// prefers-reduced-motion the strip becomes a static transcript.
    /// Zero scripts: the file itself animates in any browser, subtitles
    /// and all; the receipt records motion (plus diagrams[].story with
    /// the beat times, the hook for muxing voice later) so a cached
    /// artifact is never mistaken for the static one
    pub motion: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionViewportParams {
    /// Session ID
    pub session_id: String,
    /// Viewport width in CSS pixels; omit to keep the current width
    pub width: Option<u32>,
    /// Viewport height in CSS pixels; omit to keep the current height
    pub height: Option<u32>,
    /// Mobile emulation: matchMedia answers pointer:coarse / hover:none and
    /// navigator.maxTouchPoints reports 5 (default: false)
    #[serde(default)]
    pub mobile: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionScreenshotParams {
    /// Session ID
    pub session_id: String,
    /// Render width in CSS pixels; defaults to the session's current viewport
    pub width: Option<u32>,
    /// Render height in CSS pixels; defaults to the session's current viewport
    pub height: Option<u32>,
    /// Capture the full scrollable page instead of the viewport (default: false)
    #[serde(default)]
    pub full_page: bool,
    /// CSS selector: capture only that element's box
    pub selector: Option<String>,
    /// With selector, capture every match (default: first match only)
    #[serde(default)]
    pub selector_all: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionWaitParams {
    /// Session ID
    pub session_id: String,
    /// CSS selector to wait for (e.g. ".price-card")
    pub selector: Option<String>,
    /// JS expression polled until truthy (e.g. "document.querySelectorAll('.card').length >= 3")
    pub predicate: Option<String>,
    /// Give up after this many milliseconds (default: 10000, max: 120000)
    #[serde(default = "default_wait_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_wait_timeout_ms() -> u64 {
    10_000
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionExportParams {
    /// Session ID
    pub session_id: String,
    /// Output format: "bash" (default) renders a runnable curl script that
    /// replays every recorded action against a fresh session; "jsonl" returns
    /// the raw action log, one JSON object per line; "json" returns a
    /// flow.json document (editable ops, cookies/storage stripped) for
    /// replay via flow_run
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FlowRunParams {
    /// Inline flow document: {create?, vars?, steps:[{op, args, expect?, save?}]}
    #[serde(default)]
    pub flow: Option<serde_json::Value>,
    /// Or run a server-side workflow/<name>/flow.json asset. An unknown name
    /// errors back with the list of installed workflows — that error is the
    /// discovery call.
    #[serde(default)]
    pub name: Option<String>,
    /// Values for {{placeholders}} in step args; wins over the flow's own
    /// vars defaults.
    #[serde(default)]
    pub vars: Option<serde_json::Value>,
    /// Reuse a live session (e.g. from import_curl) instead of creating a
    /// fresh one — that's how login state and flows compose.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionNetworkParams {
    /// Session ID
    pub session_id: String,
    /// "media" extracts playback/stream links (m3u8/HLS, mp4, dash, ...) from the requests the page actually issued - the reliable way to get a real video link, since URLs embedded in page HTML are often decoys. Media elements and player iframes the engine never fetches (video/audio/source src, iframe src) are merged in as candidates: entries carry via="network" (confirmed requests) or via="dom" (candidates, with their tag; iframes surface as kind "iframe" - player pages to navigate or sniff inside, not playable URLs). Omit to list every request as compact rows.
    #[serde(default)]
    pub filter: Option<String>,
    /// Add an `xhr` array of background API responses (the page's own fetch/XHR
    /// traffic with retained bodies) alongside the request rows — the page's
    /// API face is often the cleanest structured read of its data.
    #[serde(default)]
    pub include_bodies: Option<bool>,
    /// Narrow the `xhr` array to URLs containing this substring.
    #[serde(default)]
    pub url_contains: Option<String>,
    /// Per-body character cap for the `xhr` array (default 4000).
    #[serde(default)]
    pub body_max_chars: Option<usize>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionCloseParams {
    /// Session ID
    pub session_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ImportCurlParams {
    /// A "Copy as cURL" command pasted from Chrome DevTools (Network panel →
    /// right-click any authenticated request). bash, PowerShell and cmd
    /// flavors all parse; the cookie set is injected and the session
    /// navigates to the copied request's URL.
    pub curl: String,
    /// Route the session's traffic through the engine proxy.
    #[serde(default)]
    pub use_proxy: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CacheParams {
    /// Full-text search over cached page contents, titles, URLs and past search queries. Omit to list the latest rows.
    #[serde(default)]
    pub query: Option<String>,
    /// Only rows whose URL contains this substring
    #[serde(default)]
    pub url: Option<String>,
    /// Return the FULL cached content of this exact URL instead of listing hits
    #[serde(default)]
    pub get: Option<String>,
    /// Which rows to search: "auto" (default, pages + searches), "pages", or "searches"
    #[serde(default)]
    pub kind: Option<String>,
    /// Only rows stored within the last N hours
    #[serde(default)]
    pub since_hours: Option<u64>,
    /// Maximum rows returned (default: 10, max 100)
    #[serde(default = "default_max_results")]
    pub limit: usize,
    /// Return row counts and database size instead of rows
    #[serde(default)]
    pub stats: bool,
    /// Delete matching rows instead of returning them (requires url, since_hours, or all)
    #[serde(default)]
    pub clear: bool,
    /// With clear: delete everything cached for this caller
    #[serde(default)]
    pub all: bool,
}

fn default_scroll_dir() -> String {
    "down".to_string()
}
fn default_scroll_amount() -> u32 {
    3
}

fn default_format() -> String {
    "markdown".to_string()
}
fn default_video_fps() -> f64 {
    24.0
}
fn default_video_width() -> u32 {
    1280
}
fn default_video_height() -> u32 {
    720
}
fn default_video_hold_tail_secs() -> f64 {
    0.5
}
fn default_video_max_duration_secs() -> f64 {
    120.0
}
fn default_video_wait_timelines_ms() -> u64 {
    10_000
}
fn default_audio_volume() -> f32 {
    1.0
}
fn default_audio_loop() -> bool {
    true
}

/// render_video `audio`: background music track — fetched, looped to cover
/// the video, volume-scaled, optionally faded out at the end.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RenderVideoAudio {
    /// URL of the music file. mp3/wav/ogg/m4a — probed by content.
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

/// render_video `narration[]`: one voiceover clip placed at a start time —
/// generate with any TTS, hand us the URL; all clips mix into one AAC track.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RenderNarrationClip {
    /// URL of the voice clip (any TTS output; mp3/wav/ogg/m4a).
    pub url: String,
    /// Seconds from video t=0 where this line starts. Default 0.
    #[serde(default)]
    pub start_secs: f64,
    /// Linear multiplier 0..=2. Default 1.
    #[serde(default = "default_audio_volume")]
    pub volume: f32,
}

/// render_video parameters — mirrors POST /video's request shape.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RenderVideoParams {
    /// Page URL whose scripts register timelines in `window.__timelines`
    /// (GSAP-style objects with `duration()` + `pause(t)`).
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
    /// Route through proxy (for blocked foreign sites)
    #[serde(default)]
    pub use_proxy: bool,
    /// TLS fingerprint override (stealth mode only)
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
    /// Background music: looped to cover the video, volume-scaled, faded
    /// out at the tail.
    #[serde(default)]
    pub audio: Option<RenderVideoAudio>,
    /// Voiceover clips, each starting at its own time (any TTS output;
    /// mixed into one AAC track).
    #[serde(default)]
    pub narration: Vec<RenderNarrationClip>,
    /// Inline SRT subtitles muxed as a soft (toggleable) mov_text track.
    #[serde(default)]
    pub subtitles_srt: Option<String>,
    /// ISO language tag for the subtitle track, e.g. "eng" / "zh".
    #[serde(default)]
    pub subtitles_language: Option<String>,
    /// Burn the cues into the frame pixels too (hardsub) — on by default
    /// when `subtitles_srt` is present; QuickTime, WeChat and most social
    /// embeds ignore the soft mov_text track. `false` keeps the soft track
    /// only.
    #[serde(default)]
    pub burn_subtitles: Option<bool>,
}

fn default_pdf_width() -> u32 {
    794
}
fn default_pdf_height() -> u32 {
    1123
}
fn default_pdf_max_pages() -> usize {
    50
}
fn default_pdf_jpeg_quality() -> u8 {
    90
}
fn default_pdf_format() -> String {
    "pdf".to_string()
}

/// render_pdf parameters — mirrors POST /pdf's request shape.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RenderPdfParams {
    /// Page URL to cut into pages.
    pub url: String,
    /// Output format: "pdf" (default), "png" (one base64 PNG per page),
    /// "pptx" (one slide per page, image-based), "pptx-native" (editable:
    /// element-level DrawingML — real text runs, gradient shapes, image
    /// parts; requires `selector`), or "docx" (one page-sized section per
    /// page).
    #[serde(default = "default_pdf_format")]
    pub format: String,
    /// Page width in CSS pixels. Default 794 (A4 @96dpi).
    #[serde(default = "default_pdf_width")]
    pub width: u32,
    /// Page height in CSS pixels — print pagination only. Default 1123.
    #[serde(default = "default_pdf_height")]
    pub height: u32,
    /// CSS selector; present → slides mode (one page per match, sized to the
    /// element). Absent → print mode (fixed-height pages at block boundaries).
    #[serde(default)]
    pub selector: Option<String>,
    /// Safety cap on emitted pages. Default 50.
    #[serde(default = "default_pdf_max_pages")]
    pub max_pages: usize,
    /// JPEG quality for PDF page embedding (1-100). Default 90.
    #[serde(default = "default_pdf_jpeg_quality")]
    pub jpeg_quality: u8,
    /// Route through proxy (for blocked foreign sites)
    #[serde(default)]
    pub use_proxy: bool,
    /// TLS fingerprint override (stealth mode only)
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
}
fn default_max_chars() -> usize {
    50000
}
fn default_categories() -> String {
    "general".to_string()
}
fn default_max_results() -> usize {
    10
}
fn default_max_chars_per() -> usize {
    4000
}
fn default_true() -> bool {
    true
}
fn default_js_timeout() -> u64 {
    5000
}

// ============================================================================
// MCP Server — wraps aginxbrowser HTTP API as MCP tools
// ============================================================================

/// MCP server wrapping aginxbrowser's core operations as MCP tools.
///
/// The sync operations (fetch/eval/click) each call `run_on_local_runtime`
/// internally, which creates its own current-thread Tokio runtime for V8.
/// Since MCP tool handlers run on Tokio's multi-threaded runtime, we must
/// dispatch these calls via `spawn_blocking` to avoid the "cannot start a
/// runtime from within a runtime" panic.
#[derive(Debug, Clone)]
pub struct AginxBrowserMcp {
    /// Local-store owner for this MCP instance: "global" under the default
    /// scope, or a unique id per client session when
    /// AGINXBROWSER_STORE_SCOPE=session (shared deployments).
    pub owner: String,
}

#[tool_router]
impl AginxBrowserMcp {
    #[tool(
        description = "Fetch a webpage and return clean markdown/html/text. Use whenever the agent needs to READ any web page - blogs, docs, articles, JS-rendered SPAs, Cloudflare-protected sites. Static pages are served over plain HTTP (~100ms tier:\"http\"); pages that need JS get the full browser (tier:\"browser\"). render_tier selects auto (default) / http (pure HTTP, refuses the upgrade) / obscura (always browser).",
        annotations(title = "Fetch Webpage", read_only_hint = true)
    )]
    async fn fetch(&self, Parameters(params): Parameters<FetchParams>) -> String {
        if let Err(e) = crate::robots::assert_allowed(&params.url).await {
            return json!({ "error": e }).to_string();
        }
        let req = FetchRequest {
            url: params.url,
            format: match params.format.as_str() {
                "html" => OutputFormat::Html,
                "text" => OutputFormat::Text,
                _ => OutputFormat::Markdown,
            },
            selector: params.selector,
            wait_secs: params.wait_secs,
            use_proxy: params.use_proxy,
            cookies: vec![],
            max_chars: params.max_chars,
            auto_bypass_challenge: params.auto_bypass_challenge,
            render_tier: params.render_tier,
            tls_fingerprint: params.tls_fingerprint,
            js_extract: params.js_extract.map(|j| crate::JsExtractConfig {
                expression: j.expression,
                timeout_ms: j.timeout_ms,
            }),
            sanitize: params.sanitize,
            capture_xhr: params.capture_xhr,
        };

        match smart_fetch(req).await {
            Ok(resp) => {
                crate::store::record_fetch(&self.owner, &resp);
                let mut out = json!({
                    "url": resp.url,
                    "title": resp.title,
                    "content": resp.content,
                    "truncated": resp.truncated,
                    "tier": resp.tier,
                });
                if !resp.redirected_from.is_empty() {
                    out["redirected_from"] = json!(resp.redirected_from);
                }
                if let Some(report) = &resp.sanitize_report {
                    out["sanitize_report"] = json!(report);
                }
                if !resp.xhr.is_empty() {
                    out["xhr"] = json!(resp.xhr);
                }
                out.to_string()
            }
            Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
        }
    }

    #[tool(
        description = "Execute JavaScript on a one-off page: loads `url` in a fresh browser context, \
optionally waits `wait_secs` for the page to settle, evaluates `script` (async/Promise supported) \
and returns `{url, result}`. Script-driven navigation (location.href, form submit) is drained and \
reflected in the returned `url`. Stateless — no cookies or page state shared with other calls; \
when the script needs prior page state or a login, use session_eval.",
        annotations(title = "Evaluate JavaScript")
    )]
    async fn eval(&self, Parameters(params): Parameters<EvalParams>) -> String {
        if let Err(e) = crate::robots::assert_allowed(&params.url).await {
            return json!({ "error": e }).to_string();
        }
        let req = EvalRequest {
            url: params.url,
            script: params.script,
            wait_secs: params.wait_secs,
            use_proxy: false,
            cookies: vec![],
            tls_fingerprint: None,
        };

        match tokio::task::spawn_blocking(move || do_eval(req)).await {
            Ok(Ok(resp)) => json!({
                "url": resp.url,
                "result": resp.result
            })
            .to_string(),
            Ok(Err(e)) => json!({ "error": format!("{}", e) }).to_string(),
            Err(e) => json!({ "error": format!("task panicked: {}", e) }).to_string(),
        }
    }

    #[tool(
        description = "Render a page's animation timelines to an MP4 video. The page's scripts must \
expose `window.__timelines` — objects with `duration()` and `pause(t)` (a paused \
gsap.timeline registered there works as-is). Each frame seeks every timeline to \
t=i/fps and paints the viewport, so the output is deterministic — no wall clock \
in the pixel values. Audio: `narration[]` places TTS/voice clips at start times \
(mixed into one AAC track), `audio` adds looped background music, and \
`subtitles_srt` muxes an SRT as a soft mov_text track and (by default, \
`burn_subtitles: false` to opt out) burns the same cues into the frame pixels — \
QuickTime, WeChat and most social embeds ignore the soft track. Requires ffmpeg on the \
server. Returns base64 MP4 \
(H.264, yuv420p) plus frame count and durations.",
        annotations(title = "Render Timeline Video")
    )]
    async fn render_video(&self, Parameters(params): Parameters<RenderVideoParams>) -> String {
        if let Err(e) = crate::robots::assert_allowed(&params.url).await {
            return json!({ "error": e }).to_string();
        }
        #[cfg(feature = "screenshot")]
        {
            let req = crate::VideoRequest {
                url: params.url,
                fps: params.fps,
                width: params.width,
                height: params.height,
                hold_tail_secs: params.hold_tail_secs,
                max_duration_secs: params.max_duration_secs,
                wait_timelines_ms: params.wait_timelines_ms,
                use_proxy: params.use_proxy,
                cookies: vec![],
                tls_fingerprint: params.tls_fingerprint,
                audio: params.audio.map(|a| crate::VideoAudioRequest {
                    url: a.url,
                    volume: a.volume,
                    fade_out_secs: a.fade_out_secs,
                    loop_audio: a.loop_audio,
                }),
                narration: params
                    .narration
                    .into_iter()
                    .map(|c| crate::VideoNarrationClip {
                        url: c.url,
                        start_secs: c.start_secs,
                        volume: c.volume,
                    })
                    .collect(),
                subtitles_srt: params.subtitles_srt,
                subtitles_language: params.subtitles_language,
                burn_subtitles: params.burn_subtitles,
            };
            return match tokio::task::spawn_blocking(move || crate::server::do_video(req)).await {
                Ok(Ok(resp)) => json!({
                    "url": resp.url,
                    "title": resp.title,
                    "frames": resp.frames,
                    "timeline_secs": resp.timeline_secs,
                    "duration_secs": resp.duration_secs,
                    "width": resp.width,
                    "height": resp.height,
                    "video_base64": resp.video_base64,
                    "has_audio": resp.has_audio,
                    "has_subtitles": resp.has_subtitles,
                    "burned_subtitles": resp.burned_subtitles,
                    "format": resp.format,
                })
                .to_string(),
                Ok(Err(e)) => json!({ "error": format!("{e:#}") }).to_string(),
                Err(e) => json!({ "error": format!("task panicked: {e}") }).to_string(),
            };
        }
        #[cfg(not(feature = "screenshot"))]
        {
            let _ = (params.url, params.use_proxy, params.tls_fingerprint);
            json!({ "error": "render_video requires the `screenshot` feature" }).to_string()
        }
    }

    #[tool(
        description = "Cut a rendered page into pages and package as PDF, PNGs, PPTX or DOCX. Print mode \
(no selector) paginates the document into fixed-height pages (default 794x1123, A4 @96dpi), breaking \
at top-level block boundaries — no half-cut text where a break can land on a block edge. Slides mode \
(selector set) makes one page per match, sized to that element — generate an HTML deck with one .slide \
per page and each becomes a deck page. format \"pdf\" (default) returns base64 image-based PDF; \"png\" \
returns one base64 PNG per page in pages_base64; \"pptx\" returns a base64 PPTX (one slide per page, \
deck-sized to the largest page); \"docx\" returns a base64 DOCX (one page-sized section per page, each \
section keeps its own height). Returns page count and packaging.",
        annotations(title = "Render Page Set (PDF/PPTX/DOCX)")
    )]
    async fn render_pdf(&self, Parameters(params): Parameters<RenderPdfParams>) -> String {
        if let Err(e) = crate::robots::assert_allowed(&params.url).await {
            return json!({ "error": e }).to_string();
        }
        #[cfg(feature = "screenshot")]
        {
            let req = crate::PdfRequest {
                url: params.url,
                format: params.format,
                width: params.width,
                height: params.height,
                selector: params.selector,
                max_pages: params.max_pages,
                jpeg_quality: params.jpeg_quality,
                use_proxy: params.use_proxy,
                cookies: vec![],
                tls_fingerprint: params.tls_fingerprint,
            };
            return match tokio::task::spawn_blocking(move || crate::server::do_pdf(req)).await {
                Ok(Ok(resp)) => json!({
                    "url": resp.url,
                    "title": resp.title,
                    "pages": resp.pages,
                    "width": resp.width,
                    "height": resp.height,
                    "pdf_base64": resp.pdf_base64,
                    "pages_base64": resp.pages_base64,
                    "pptx_base64": resp.pptx_base64,
                    "docx_base64": resp.docx_base64,
                    "format": resp.format,
                })
                .to_string(),
                Ok(Err(e)) => json!({ "error": format!("{e:#}") }).to_string(),
                Err(e) => json!({ "error": format!("task panicked: {e}") }).to_string(),
            };
        }
        #[cfg(not(feature = "screenshot"))]
        {
            let _ = (params.url, params.use_proxy, params.tls_fingerprint);
            json!({ "error": "render_pdf requires the `screenshot` feature" }).to_string()
        }
    }

    #[tool(
        description = "Click an element on a one-off page: loads `url` in a fresh browser context \
(stateless — no cookies unless passed, no shared state with other calls), waits `wait_secs` after \
load before clicking, then fires a DOM click on the first CSS-selector match. The click may \
trigger navigation (link, form submit) — the response `url` and `text_after` are read after that \
navigation lands. Returns `clicked:false` when the selector matches nothing. For multi-step \
interaction on a shared page use session_click instead.",
        annotations(title = "Click Element")
    )]
    async fn click(&self, Parameters(params): Parameters<ClickParams>) -> String {
        if let Err(e) = crate::robots::assert_allowed(&params.url).await {
            return json!({ "error": e }).to_string();
        }
        let req = ClickRequest {
            url: params.url,
            selector: params.selector,
            wait_secs: params.wait_secs,
            use_proxy: false,
            cookies: vec![],
            tls_fingerprint: None,
        };

        match tokio::task::spawn_blocking(move || do_click(req)).await {
            Ok(Ok(resp)) => json!({
                "url": resp.url,
                "clicked": resp.clicked,
                "text_after": resp.text_after
            })
            .to_string(),
            Ok(Err(e)) => json!({ "error": format!("{}", e) }).to_string(),
            Err(e) => json!({ "error": format!("task panicked: {}", e) }).to_string(),
        }
    }

    #[tool(
        description = "Search the web across Baidu/Bing/Sogou/WeChat/Google (aggregated + deduped) and optionally fetch the top results' full content. Use when the agent needs to FIND information online - replaces a search API. Supports image search returning direct image URLs. Optional engines: [\"baidu\"]-style filter by engine name (invalid names error with the valid list; /doctor lists them with live health). Optional time_range day/week/month/year for news freshness (engines without dated results ignore it). Response carries engine_errors explaining any engine that contributed nothing (CAPTCHA suspension, transient failure).",
        annotations(title = "Web Search", read_only_hint = true)
    )]
    async fn search(&self, Parameters(params): Parameters<SearchParams>) -> String {
        let categories = params.categories.clone();
        let req = SearchRequest {
            q: params.q,
            fetch_top: params.fetch_top,
            categories: params.categories,
            language: "zh-CN".to_string(),
            max_results: params.max_results,
            max_chars_per: params.max_chars_per,
            wait_secs: 3,
            use_proxy: false,
            engines: params.engines,
            time_range: params.time_range,
        };

        // do_search is already async and uses spawn_blocking internally for
        // the fetch_top body-grabbing, so it's safe to call directly.
        match do_search(req).await {
            Ok(resp) => {
                crate::store::record_search(&self.owner, &resp.query, &categories, &resp);
                json!({
                    "query": resp.query,
                    "number_of_results": resp.number_of_results,
                    "results": resp.results,
                    "captcha_events": resp.captcha_events,
                    "engine_errors": resp.engine_errors
                })
                .to_string()
            }
            Err(e) => json!({ "error": format!("{:?}", e) }).to_string(),
        }
    }

    #[tool(
        description = "Download a file over HTTP(S) with streaming to disk (no memory buffering), SHA-256 integrity hash, and optional resume of interrupted transfers. Filename resolution: explicit param → Content-Disposition → URL tail. Use for binaries, archives, datasets, documents - anything where the agent wants the FILE saved, not its text content read.",
        annotations(title = "Download File")
    )]
    async fn download(&self, Parameters(params): Parameters<DownloadParams>) -> String {
        if let Err(e) = crate::robots::assert_allowed(&params.url).await {
            return json!({ "error": e }).to_string();
        }
        match crate::download::do_download(crate::download::DownloadRequest {
            url: params.url,
            filename: params.filename,
            resume: params.resume,
            use_proxy: params.use_proxy,
            cookies: params.cookies,
        })
        .await
        {
            Ok(resp) => json!({
                "url": resp.url,
                "path": resp.path,
                "filename": resp.filename,
                "size_bytes": resp.size_bytes,
                "content_type": resp.content_type,
                "sha256": resp.sha256,
                "resumed": resp.resumed,
            })
            .to_string(),
            Err(e) => json!({ "error": format!("{e:#}") }).to_string(),
        }
    }

    #[tool(
        description = "Query the LOCAL CACHE of every page this server has fetched and every search it has run. Check here BEFORE re-fetching or re-searching — a hit is instant and free while a fresh fetch costs 5-60s. Use query for full-text search (works for Chinese substrings and English words), get to pull a page's full cached content, stats for counts, clear to delete rows.",
        annotations(title = "Local Cache", read_only_hint = false)
    )]
    async fn cache(&self, Parameters(params): Parameters<CacheParams>) -> String {
        if params.clear {
            return match crate::store::clear(&self.owner, params.url.as_deref(), params.since_hours, params.all)
            {
                Ok((pages, searches)) => json!({
                    "cleared_pages": pages,
                    "cleared_searches": searches
                })
                .to_string(),
                Err(e) => json!({ "error": e }).to_string(),
            };
        }
        if let Some(url) = &params.get {
            return match crate::store::get_page(&self.owner, url) {
                Ok(Some(p)) => json!(p).to_string(),
                Ok(None) => json!({ "error": "not in cache", "url": url }).to_string(),
                Err(e) => json!({ "error": e }).to_string(),
            };
        }
        if params.stats {
            return match crate::store::stats(&self.owner) {
                Ok(s) => json!(s).to_string(),
                Err(e) => json!({ "error": e }).to_string(),
            };
        }
        let q = crate::store::CacheQuery {
            query: params.query,
            url: params.url,
            kind: params.kind.unwrap_or_else(|| "auto".into()),
            since_hours: params.since_hours,
            limit: params.limit,
        };
        match crate::store::query(&self.owner, &q) {
            Ok(r) => json!(r).to_string(),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    // ------------------------------------------------------------------
    // Session tools
    // ------------------------------------------------------------------

    #[tool(
        description = "Create a persistent interactive browser session for multi-step interaction - clicking, typing, scrolling, reading state across page transitions. Use when the agent must INTERACT with a page (login flows, forms, pagination, click-through) rather than read it once. Returns session_id; persists 8 min idle. With persistent:true the login state survives idle eviction and server restarts - the same session_id revives logged-in.",
        annotations(title = "Create Browser Session")
    )]
    async fn session_create(&self, Parameters(params): Parameters<SessionCreateParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        mgr.evict_expired();
        let url = params.url.clone();
        let storage = params.storage.clone();
        let pin = match (params.width, params.height) {
            (None, None) => None,
            (w, h) => Some((w, h, params.mobile)),
        };
        let id = mgr.create(
            params.url.as_deref(),
            params.use_proxy,
            params.cookies,
            storage,
            params.ttl_secs,
            pin,
            params.keepalive,
            params.persistent,
        );
        let mut resp = json!({ "session_id": id, "url": url });
        if let Some(s) = mgr.expires_in_secs(&id) {
            resp["expires_in_secs"] = json!(s);
        }
        if params.persistent {
            resp["persistent"] = json!(true);
        }
        resp.to_string()
    }

    #[tool(
        description = "Derive a new browser session from a live one, carrying the full login state: \
cookies, localStorage/sessionStorage, viewport pin, dialog policy, proxy and keepalive flags. The \
source session stays untouched. Use to snapshot a logged-in state before risky actions, or to run \
the same login in parallel tabs. Returns {session_id (new), cloned_from, url, viewport}.",
        annotations(title = "Clone Session")
    )]
    async fn session_clone(&self, Parameters(params): Parameters<SessionCloneParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.clone_session(&params.session_id).await {
            Ok(resp) => resp.to_string(),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Navigate a browser session to a new URL.",
        annotations(title = "Session Navigate")
    )]
    async fn session_navigate(&self, Parameters(params): Parameters<SessionNavigateParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Navigate {
            url: params.url.clone(),
            reply,
        }).await {
            Ok(resp) => stamped_json(json!({ "url": resp.url, "title": resp.title }), &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Get the current page state as an indexed list of interactive elements. Returns compact text with [N] indexes for use with click/input tools.",
        annotations(title = "Session State", read_only_hint = true)
    )]
    async fn session_state(&self, Parameters(params): Parameters<SessionStateParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::State { reply }).await {
            Ok(text) => text,
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Export the session's current cookies as [\"name=value\", ...] for the page's URL. Use to persist a logged-in session and replay it later via session_create with cookies. Round-trips with session_create's cookies field.",
        annotations(title = "Session Cookies", read_only_hint = true)
    )]
    async fn session_cookies(&self, Parameters(params): Parameters<SessionCookiesParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Cookies { reply }).await {
            Ok(text) => stamped(text, &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Snapshot the session's localStorage/sessionStorage for the current origin: \
{url, local_storage, session_storage}. Feed it back via session_create's `storage` field to restore \
a logged-in state in a new session — the half of login state that cookies can't carry (many sites \
keep the session token in localStorage). Call before the session idles out.",
        annotations(title = "Session Storage", read_only_hint = true)
    )]
    async fn session_storage(&self, Parameters(params): Parameters<SessionCookiesParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Storage { reply }).await {
            Ok(text) => stamped(text, &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Read the session's recent page console output (log/info/warn/error) as \
{url, total, matched, messages:[{ts_ms, level, text, url}]}, newest last. Ring buffer of 500 \
entries; captures output from page scripts, clicks, evals and navigation alike. Optional filters: \
level (exact, e.g. \"error\"), since_ts (epoch ms), url_contains (page URL substring), limit \
(most recent N matches). The fastest way to see WHY a page misbehaves: click the button, call \
this, read the error.",
        annotations(title = "Session Console", read_only_hint = true)
    )]
    async fn session_console(&self, Parameters(params): Parameters<SessionConsoleParams>) -> String {
        let filter = session::ConsoleFilter {
            level: params.level,
            since_ts: params.since_ts,
            url_contains: params.url_contains,
            limit: params.limit,
        };
        let mut mgr = session::SESSIONS.lock().await;
        match mgr
            .send(&params.session_id, |reply| SessionCommand::Console { filter, reply })
            .await
        {
            Ok(text) => stamped(text, &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Inspect or flip the session's dialog policy for window.alert/confirm/prompt. \
Dialogs never block the page: each is auto-answered (default dismiss) and logged into \
session_console at level \"dialog\". action \"list\" reports {policy, prompt_text, dialogs}; \
\"accept\" makes subsequent confirm() true and prompt() return prompt_text (or the call's \
default argument); \"dismiss\" restores the default.",
        annotations(title = "Session Dialog")
    )]
    async fn session_dialog(&self, Parameters(params): Parameters<SessionDialogParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr
            .send(&params.session_id, |reply| SessionCommand::Dialog {
                action: params.action,
                prompt_text: params.prompt_text,
                reply,
            })
            .await
        {
            Ok(text) => stamped(text, &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Click an interactive element by its index (from session_state output) inside \
a live browser session: scrolls it into view and fires a DOM click on the session's current page. \
A submit click may navigate the session — the returned `url`/`text_after` reflect the page after \
the action, and session state (cookies, localStorage, globals) persists for follow-up calls. \
Indexes come from the most recent session_state; re-list after navigation.",
        annotations(title = "Session Click")
    )]
    async fn session_click(&self, Parameters(params): Parameters<SessionClickParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Click {
            index: params.index,
            reply,
        }).await {
            Ok(resp) => stamped_json(json!({ "url": resp.url, "clicked": resp.clicked, "text_after": resp.text_after }), &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Click at viewport coordinates (CSS pixels) via real mouse events — \
pointerdown/mousedown, pointerup/mouseup, then click on whatever element is hit there. \
For canvas/map surfaces with no DOM element to index. click_count 2 adds dblclick.",
        annotations(title = "Session Click XY")
    )]
    async fn session_click_xy(
        &self,
        Parameters(params): Parameters<SessionClickXyParams>,
    ) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::ClickXY {
            x: params.x,
            y: params.y,
            button: params.button.unwrap_or_else(|| "left".to_string()),
            click_count: params.click_count.unwrap_or(1),
            reply,
        })
        .await
        {
            Ok(text) => stamped(text, &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Drag the mouse from one viewport position to another: press at `from`, \
`steps` interpolated mousemove events (delay_ms apart), release at `to`. Moves AMarker-style \
drag targets and canvas selections that only track while the pointer travels.",
        annotations(title = "Session Drag")
    )]
    async fn session_drag(&self, Parameters(params): Parameters<SessionDragParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Drag {
            from_x: params.from.x,
            from_y: params.from.y,
            to_x: params.to.x,
            to_y: params.to.y,
            steps: params.steps.unwrap_or(10),
            delay_ms: params.delay_ms.unwrap_or(30),
            reply,
        })
        .await
        {
            Ok(text) => stamped(text, &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Type text into an input/textarea element by its index (from session_state output).",
        annotations(title = "Session Input")
    )]
    async fn session_input(&self, Parameters(params): Parameters<SessionInputParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        let full = params.events.as_deref() == Some("full");
        match mgr.send(&params.session_id, |reply| SessionCommand::Input {
            index: params.index,
            text: params.text.clone(),
            full_events: full,
            reply,
        }).await {
            Ok(filled) => stamped(filled.to_string(), &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Scroll the page up or down by a number of viewport-heights.",
        annotations(title = "Session Scroll")
    )]
    async fn session_scroll(&self, Parameters(params): Parameters<SessionScrollParams>) -> String {
        let direction = match params.direction.as_str() {
            "up" => session::ScrollDirection::Up,
            _ => session::ScrollDirection::Down,
        };
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Scroll {
            direction,
            amount: params.amount,
            reply,
        }).await {
            Ok(scrolled) => stamped_json(json!({ "scrolled": scrolled }), &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Execute arbitrary JavaScript in a live browser session and return the result. \
Runs in the session's current page, so DOM mutations, globals and storage persist across calls — \
unlike the stateless eval tool, which loads its own throwaway page each call. Script-driven \
navigation moves the session's URL. JS exceptions are reported with name, line/column and stack.",
        annotations(title = "Session Eval")
    )]
    async fn session_eval(&self, Parameters(params): Parameters<SessionEvalParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Eval {
            script: params.script.clone(),
            timeout_ms: params.timeout_ms,
            reply,
        }).await {
            Ok(result) => stamped_json(json!({ "result": result }), &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Set the session's viewport (device emulation): scripts see innerWidth/innerHeight \
move, media queries like (max-width: 600px) re-evaluate, element rects re-anchor, and mobile=true \
flips pointer/hover matchMedia answers to coarse/none. Omitted width/height keeps the current value.",
        annotations(title = "Session Viewport")
    )]
    async fn session_viewport(&self, Parameters(params): Parameters<SessionViewportParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Viewport {
            width: params.width,
            height: params.height,
            mobile: params.mobile,
            reply,
        }).await {
            Ok(viewport) => stamped_json(json!({ "viewport": viewport }), &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Screenshot the session's CURRENT DOM state (mutations from clicks/evals included) \
as a base64 PNG via the built-in renderer. Width/height default to the session's viewport, so \
session_viewport + session_screenshot shows the responsive layout. Returns \
{url, width, height, image_base64, format}.",
        annotations(title = "Session Screenshot")
    )]
    async fn session_screenshot(&self, Parameters(params): Parameters<SessionScreenshotParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Screenshot {
            width: params.width,
            height: params.height,
            full_page: params.full_page,
            selector: params.selector.clone(),
            selector_all: params.selector_all,
            reply,
        }).await {
            Ok(s) => stamped(s, &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Wait until a CSS selector matches or a JS predicate turns truthy, with a timeout. \
The page's event loop keeps running while waiting (fetches, timers, promise chains progress), so this \
replaces blind sleeps for async content: navigate, session_wait for '.price-card', then click/read. \
Returns {matched, elapsed_ms, detail:{tag,text} or the predicate value}; errors with `timeout ...` \
naming the selector/predicate on expiry. Exactly one of selector/predicate.",
        annotations(title = "Session Wait", read_only_hint = true)
    )]
    async fn session_wait(&self, Parameters(params): Parameters<SessionWaitParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Wait {
            selector: params.selector.clone(),
            predicate: params.predicate.clone(),
            timeout_ms: params.timeout_ms,
            reply,
        }).await {
            Ok(s) => stamped(s, &mgr, &params.session_id),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Read the session's network request log. filter=\"media\" extracts playback/stream URLs (m3u8/HLS, mp4, dash, flv...) actually requested by the page's player at runtime - the reliable way to get a real video link, since links embedded in page HTML are often decoys. Media elements and player iframes the engine never fetches (video/audio/source/iframe src) are merged in as candidates: via=\"network\" entries are confirmed requests, via=\"dom\" entries are candidates carrying their tag (iframes = kind \"iframe\", navigate into them to sniff). Default returns every request as compact rows (method/url/status/type/size). Navigate to the video page first, let it load, then call this.",
        annotations(title = "Session Network Sniffer", read_only_hint = true)
    )]
    async fn session_network(&self, Parameters(params): Parameters<SessionNetworkParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        match mgr.send(&params.session_id, |reply| SessionCommand::Network {
            media_only: params.filter.as_deref() == Some("media"),
            include_bodies: params.include_bodies.unwrap_or(false),
            url_contains: params.url_contains,
            body_max_chars: params.body_max_chars.unwrap_or(4000),
            reply,
        }).await {
            Ok(text) => text,
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "Import login state from a real browser in one paste. The human logs into a \
site in their own Chrome (solving the CAPTCHA/SMS once), opens DevTools → Network, right-clicks \
any authenticated request → \"Copy as cURL\", and passes the command here. Returns a live \
session_id already carrying that site's cookies and sitting on the copied request's URL — the \
agent continues from where the human left off, no password or second login needed. Works with \
bash, PowerShell and cmd copy flavors.",
        annotations(title = "Import Login From cURL")
    )]
    async fn import_curl(&self, Parameters(params): Parameters<ImportCurlParams>) -> String {
        match crate::curl_import::create_session_from_curl(&params.curl, params.use_proxy).await {
            Ok(v) => v.to_string(),
            Err(e) => json!({ "error": e }).to_string(),
        }
    }

    #[tool(
        description = "List live browser sessions with idle age and the time left before auto-eviction. Use to discover a session to reuse instead of creating a new one; sessions expire after 8 min idle.",
        annotations(title = "List Browser Sessions", read_only_hint = true)
    )]
    async fn session_list(&self) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        mgr.evict_expired();
        let sessions = mgr.list();
        json!({ "count": sessions.len(), "sessions": sessions }).to_string()
    }

    #[tool(
        description = "Export a browser session's recorded action log. Format \"bash\" (default) returns a runnable curl script that replays every recorded action (navigate/click/input/scroll/eval) against a fresh session on this server — hand it to a shell or cron, zero model tokens. Format \"jsonl\" returns the raw action log, one JSON object per line. Format \"json\" returns a flow.json document — the same recording as editable ops ({op, args}) with cookies/storage stripped — that flow_run replays server-side.",
        annotations(title = "Export Session Replay Script", read_only_hint = true)
    )]
    async fn session_export(&self, Parameters(params): Parameters<SessionExportParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        let jsonl = match mgr.send(&params.session_id, |reply| SessionCommand::Export { reply }).await {
            Ok(j) => j,
            Err(e) => return json!({ "error": e }).to_string(),
        };
        match params.format.as_deref() {
            Some("jsonl") => stamped_json(json!({ "format": "jsonl", "actions": jsonl }), &mgr, &params.session_id),
            Some("json") => {
                let doc = crate::flow::recorded_to_flow(&jsonl);
                stamped_json(json!({ "format": "json", "flow": doc }), &mgr, &params.session_id)
            }
            _ => {
                let script = session::replay_bash(&jsonl, "http://127.0.0.1:8089");
                stamped_json(json!({ "format": "bash", "script": script }), &mgr, &params.session_id)
            }
        }
    }

    #[tool(
        description = "Run a flow — a recorded, editable JSON browser-session script — deterministically, with zero model tokens. Steps are {op, args, expect?, save?}: ops cover navigate/click/click_xy/input/scroll/eval/wait/screenshot/state/cookies; {{var}} placeholders in args are filled from vars; expect asserts (url_contains | selector | text_contains | eval_truthy) abort with evidence on failure; save collects a step's output into the receipt. Source the flow inline via \"flow\", or by \"name\" from the server's workflow/<name>/flow.json (unknown name → error lists installed workflows). Pass session_id to reuse a live session (e.g. from import_curl) so login state and flows compose. The receipt carries status ok/failed, saved outputs, the session_id (kept alive), and on failure the failing step, reason and a diagnostic screenshot — fix the flow or take the session over from there.",
        annotations(title = "Run Flow")
    )]
    async fn flow_run(&self, Parameters(params): Parameters<FlowRunParams>) -> String {
        let doc = match crate::flow::resolve_flow_doc(params.flow, params.name.as_deref()) {
            Ok(d) => d,
            Err(e) => return json!({ "error": e }).to_string(),
        };
        let vars = params
            .vars
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let mut mgr = session::SESSIONS.lock().await;
        crate::flow::run_flow(&mut mgr, &doc, &vars, params.session_id).await.to_string()
    }

    #[tool(
        description = "Close a browser session and free its resources. For a persistent session this also drops the on-disk login snapshot - idle expiry keeps it, an explicit close does not.",
        annotations(title = "Session Close")
    )]
    async fn session_close(&self, Parameters(params): Parameters<SessionCloseParams>) -> String {
        let mut mgr = session::SESSIONS.lock().await;
        mgr.close(&params.session_id);
        json!({ "ok": true }).to_string()
    }

    #[tool(
        description = "Render a markdown document into a deterministic, self-contained HTML artifact - the document layer, so the agent never writes HTML by hand. Prose rides a plain offline shell (no fonts, no scripts); archify fenced code blocks carry typed zero-coordinate diagram JSON (sequence, workflow, architecture, dataflow, lifecycle families) and render to inline SVG via the layout engine. Same input, same bytes: the receipt carries the sha256 so determinism is verifiable. theme picks light (default) or dark; preset picks the palette family — classic (default), signal-flow, blueprint, editorial — orthogonal to theme; colors bake at generation time (presentation attributes, not CSS variables), and the receipt records both preset and theme. quality picks the composition audit profile — standard (default) or showcase, the delivery gate: the receipt's diagrams[].composition grades route crossings, ambiguous corridors, label clearance (2px standard / 4px showcase), route rhythm, and node text projected to the 930px reader width; the audit never changes the artifact bytes. Mermaid sources are the agent's job to translate, not the engine's: flowchart/graph → workflow (lanes + columns), sequenceDiagram → sequence, stateDiagram-v2 → lifecycle (bands), erDiagram/class → architecture (grid + boundaries) — read the topology and emit the matching zero-coordinate archify JSON; the engine accepts only archify JSON. A broken diagram degrades to a visible code block and lands in receipt.diagnostics; an authored route preset that cannot be honored is self-repaired to a verified semantic substitute and disclosed in receipt diagrams[].repairs - the document still renders. A fence may also carry views: [{id,label,nodes,note?}] (node ids of the active family), emitted as guided-view tabs above the diagram plus an inlined viewer script - clicking a tab lights the member nodes and the routes between them (subgraph), clicking a node lights it with its direct neighbors (ego graph), everything else dims; a view's optional note shows as a caption while it is active (the story layer). window.agxViewer in a session drives and reads the same state programmatically: {focus,view,state} as before, plus route(i,from,to) which returns and lights the shortest authored directed path between two nodes (null when unreachable, state untouched), and reach(i,id,down|up) which returns and lights the authored downstream/upstream closure ({nodes,links}); both dim the rest of the diagram. diagrams[].views in the receipt lists the tabs. motion: true bakes an entrance choreography into the artifact: pure-declarative CSS animation with zero scripts - headings split into per-glyph (CJK) / per-word (latin) spans that rise in with expo easing, prose blocks stagger up an nth-child delay ladder, diagram figures grow in with a back ease (GSAP's easing math as public cubic-bezier equivalents, nothing embedded); the diagrams themselves play a flow story on the same clock - nodes land beat by beat, solid edges draw in (dash-offset), dashed returns fade, sequence messages arrive as sent - with a timed caption strip under each figure as the subtitles, which becomes a static transcript under prefers-reduced-motion; the file itself animates in any browser and the receipt records motion plus diagrams[].story (beat times and captions - the hook for muxing voice later). With session_id the artifact is also loaded into that session (local, free) and the reply carries viewport acceptance: scroll extents measured in the live session and graded fits/tall/wide/oversized, telling the agent how to read the page back. Diagram vocabulary adapted from archify (MIT).",
        annotations(title = "Render Markdown")
    )]
    async fn render_markdown(&self, Parameters(params): Parameters<RenderMarkdownParams>) -> String {
        use crate::docgen::theme::Theme;
        // (preset, mode) → theme. One-sided requests fill in the classic/
        // light defaults; the all-default request keeps riding render(),
        // the historical entry point.
        let resolved: Result<Option<&'static Theme>, String> = match (&params.preset, &params.theme)
        {
            (None, None) => Ok(None),
            (None, Some(mode)) => Theme::by_name(mode).map(Some).ok_or_else(|| {
                format!(
                    "unknown theme \"{mode}\" — expected one of: {}",
                    Theme::names().join(", ")
                )
            }),
            (Some(preset), mode) => {
                Theme::resolve(preset, mode.as_deref().unwrap_or("light"))
                    .map(Some)
                    .ok_or_else(|| {
                        format!(
                            "unknown preset \"{preset}\" or theme \"{}\" — presets: {}; themes: {}",
                            mode.as_deref().unwrap_or("light"),
                            Theme::presets().join(", "),
                            Theme::names().join(", ")
                        )
                    })
            }
        };
        let quality = params.quality.as_deref().map_or(
            Ok(crate::docgen::checks::Quality::Standard),
            |name| {
                crate::docgen::checks::Quality::by_name(name).ok_or_else(|| {
                    format!("unknown quality \"{name}\" — expected one of: standard, showcase")
                })
            },
        );
        let rendered = match (resolved, quality) {
            (Err(e), _) | (_, Err(e)) => return json!({ "error": e }).to_string(),
            (Ok(None), Ok(quality)) => crate::docgen::render_with_quality(
                &params.markdown,
                &crate::docgen::theme::LIGHT,
                quality,
                params.motion.unwrap_or(false),
            ),
            (Ok(Some(theme)), Ok(quality)) => crate::docgen::render_with_quality(
                &params.markdown,
                theme,
                quality,
                params.motion.unwrap_or(false),
            ),
        };
        let crate::docgen::RenderOutcome { html, receipt } = rendered;
        match params.session_id {
            None => json!({ "receipt": receipt, "html": html }).to_string(),
            Some(sid) => {
                let mut mgr = session::SESSIONS.lock().await;
                match mgr
                    .send(&sid, |reply| SessionCommand::SetContent {
                        html: html.clone(),
                        reply,
                    })
                    .await
                {
                    Ok(loaded) => {
                        // Viewport acceptance: measure the loaded artifact in
                        // the live session (engine truth, not a projection)
                        // so the agent knows how to read it back — done,
                        // full-page screenshot, or widen first.
                        let measured = mgr
                            .send(&sid, |reply| SessionCommand::Eval {
                                script: VIEWPORT_PROBE.to_string(),
                                timeout_ms: None,
                                reply,
                            })
                            .await
                            .ok();
                        let mut receipt = receipt;
                        let viewport = measured.map(|v| {
                            let graded = grade_viewport(&v);
                            if let Some(checks) = receipt
                                .get_mut("checks")
                                .and_then(Value::as_array_mut)
                            {
                                checks.push(json!(format!(
                                    "viewport acceptance: {} (scroll {}x{} vs viewport {}x{})",
                                    graded["tier"].as_str().unwrap_or("?"),
                                    graded["scrollWidth"], graded["scrollHeight"],
                                    graded["innerWidth"], graded["innerHeight"],
                                )));
                            }
                            graded
                        });
                        let mut reply = json!({ "receipt": receipt, "loaded": loaded });
                        if let Some(v) = viewport {
                            reply["viewport"] = v;
                        }
                        stamped_json(reply, &mgr, &sid)
                    }
                    Err(e) => json!({ "receipt": receipt, "error": e }).to_string(),
                }
            }
        }
    }
}

/// Live-session measurement behind render_markdown's viewport acceptance:
/// document scroll extents, diagram count, and how far the widest diagram
/// was scaled down (drawn width vs viewBox width — the legibility signal).
const VIEWPORT_PROBE: &str = r#"(function(){
    var d = document.documentElement;
    var figs = document.querySelectorAll('figure.agx-diagram svg');
    var scales = [];
    for (var i = 0; i < figs.length; i++) {
        var vb = (figs[i].getAttribute('viewBox') || '').trim().split(/\s+/);
        var r = figs[i].getBoundingClientRect();
        if (vb.length === 4 && Number(vb[2]) > 0 && r.width > 0) {
            scales.push(r.width / Number(vb[2]));
        }
    }
    var minScale = null;
    for (var j = 0; j < scales.length; j++) {
        if (minScale === null || scales[j] < minScale) minScale = scales[j];
    }
    return {
        innerWidth: innerWidth, innerHeight: innerHeight,
        scrollWidth: d.scrollWidth, scrollHeight: d.scrollHeight,
        diagrams: figs.length, minScale: minScale
    };
})()"#;

/// Grade measured extents into the agent-facing tier: how to read the page
/// back, not whether it is "good" — tall means full-page screenshot, wide
/// means widen the viewport or accept horizontal scroll.
fn grade_viewport(v: &Value) -> Value {
    let get = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
    let (iw, ih, sw, sh) = (
        get("innerWidth"),
        get("innerHeight"),
        get("scrollWidth"),
        get("scrollHeight"),
    );
    let tier = match (sw > iw, sh > ih) {
        (false, false) => "fits",
        (false, true) => "tall",
        (true, false) => "wide",
        (true, true) => "oversized",
    };
    json!({
        "innerWidth": iw,
        "innerHeight": ih,
        "scrollWidth": sw,
        "scrollHeight": sh,
        "diagrams": get("diagrams"),
        "minScale": v.get("minScale").cloned().unwrap_or(Value::Null),
        "tier": tier,
    })
}

/// Feedback ③: session_* responses carry the session's remaining idle
/// lifetime so the agent can re-arm cookies/storage export before the reaper
/// fires instead of discovering an expired session mid-workflow. keepalive
/// sessions have no expiry and stay unstamped. Free-form text (session_state's
/// compact listing, the network rows) passes through untouched.
fn stamped_json(
    mut v: serde_json::Value,
    mgr: &session::SessionManager,
    sid: &str,
) -> String {
    if let Some(s) = mgr.expires_in_secs(sid) {
        v["expires_in_secs"] = json!(s);
    }
    v.to_string()
}

fn stamped(text: String, mgr: &session::SessionManager, sid: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) if v.is_object() => stamped_json(v, mgr, sid),
        _ => text,
    }
}

// Identify as the product, not the framework: the macro's default
// Implementation::from_build_env() reads rmcp's own crate name, so every
// handshake said "rmcp/1.7.0". Version comes from our CARGO_PKG_VERSION.
#[tool_handler(name = "aginxbrowser")]
impl ServerHandler for AginxBrowserMcp {}

// ============================================================================
// Server startup
// ============================================================================

/// Start MCP server on stdio transport.
pub async fn run_mcp_stdio() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::info!("Starting aginxbrowser MCP server on stdio");
    AginxBrowserMcp {
        owner: crate::store::session_owner(),
    }
    .serve(rmcp::transport::io::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

/// Build an MCP server for the streamable HTTP transport, mounted at `/mcp`.
///
/// rmcp's streamable HTTP server validates the inbound `Host` header against
/// `allowed_hosts` (defaults to loopback only) to prevent DNS rebinding, so a
/// public deployment must list its own hostname.
pub fn mcp_http_service() -> StreamableHttpService<AginxBrowserMcp, LocalSessionManager> {
    let mut hosts = vec![
        "browser.aginx.net".to_string(),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ];
    // rmcp's Host-header guard (DNS-rebinding protection) defaults to
    // loopback only, so an instance reached over a LAN IP or a docker
    // hostname would have /mcp rejected. Operators extend the allowlist
    // with a comma-separated list instead of rebuilding.
    if let Ok(extra) = std::env::var("AGINXBROWSER_MCP_ALLOWED_HOSTS") {
        hosts.extend(
            extra
                .split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(String::from),
        );
    }
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(hosts);
    StreamableHttpService::new(
        || Ok(AginxBrowserMcp {
            owner: crate::store::session_owner(),
        }),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_tiers_classify_the_readback_strategy() {
        let probe = |iw: i64, ih: i64, sw: i64, sh: i64| {
            grade_viewport(&json!({
                "innerWidth": iw, "innerHeight": ih,
                "scrollWidth": sw, "scrollHeight": sh,
                "diagrams": 1, "minScale": 0.25,
            }))
        };
        assert_eq!(probe(1280, 1000, 1280, 900)["tier"], "fits");
        assert_eq!(probe(1280, 1000, 1280, 2452)["tier"], "tall");
        assert_eq!(probe(1280, 1000, 2200, 900)["tier"], "wide");
        assert_eq!(probe(1280, 1000, 2200, 2452)["tier"], "oversized");
        // Facts pass through for the agent to reason with.
        let graded = probe(1280, 1000, 1280, 2452);
        assert_eq!(graded["minScale"], json!(0.25));
        assert_eq!(graded["diagrams"], json!(1));
        // Missing numbers degrade to zeros, not a panic.
        assert_eq!(grade_viewport(&json!({}))["tier"], "fits");
    }
}
