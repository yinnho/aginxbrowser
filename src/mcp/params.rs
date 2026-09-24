//! Tool parameter structs (JsonSchema → auto-generated MCP input
//! schemas). Split from the module root — ARCHITECTURE.md §6 P2 god-file
//! ratchet (#86).
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// Run as a named login identity (the multi-account layer): a private
    /// cookie jar seeded from the account record, write-back to the account
    /// store after every action. Concurrent logins (`taobao-scraper` vs
    /// `taobao-publisher`) never clobber each other. The account record
    /// survives the session — a later create with the same name picks up
    /// the warm jar. 1-64 chars of [a-zA-Z0-9_-].
    #[serde(default)]
    pub account: Option<String>,
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
pub struct SessionPreloadParams {
    /// Session ID
    pub session_id: String,
    /// Full JS sources, in order. Sources run before each new document's own
    /// scripts (including inline ones) — the only hook that beats pages whose
    /// signing layer captures window.fetch/XHR natives at parse time. `[]`
    /// clears the group. Set before the session's first navigate and it
    /// applies to every navigation from then on.
    #[serde(default)]
    pub scripts: Vec<String>,
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
    /// Interpolated mousemove events between from and to. Default: 24 with
    /// humanize on, 10 without
    #[serde(default)]
    pub steps: Option<u32>,
    /// Mean delay between moves in ms (default 18 humanized / 30 linear) —
    /// per-step timing is jittered around this when humanizing
    #[serde(default)]
    pub delay_ms: Option<u64>,
    /// Humanize the trajectory: minimum-jerk easing, perpendicular wobble,
    /// timing jitter, grip/settle pauses, occasional hesitation and
    /// overshoot-and-correct. Set false when a test/tool needs exact linear
    /// interpolation. Default: true
    #[serde(default)]
    pub humanize: Option<bool>,
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
pub struct SessionFileSpecParams {
    /// File name the page sees (and what multipart uploads as filename)
    pub name: String,
    /// File content, standard base64 (padding allowed)
    pub content_base64: String,
    /// MIME type (default "application/octet-stream")
    #[serde(default)]
    pub mime_type: Option<String>,
    /// Last-modified time in ms since epoch (default: now)
    #[serde(default)]
    pub last_modified: Option<f64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionSetFilesParams {
    /// Session ID
    pub session_id: String,
    /// CSS selector for the file input, e.g. "input[type=file]". File inputs
    /// are often hidden, so this is selector-addressed rather than using the
    /// /state index.
    pub selector: String,
    /// Files to select
    pub files: Vec<SessionFileSpecParams>,
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
    /// Override the run's step-execution budget (branch loops re-run steps,
    /// so every revisit counts). Default 1000, clamped 1..=100000. A flow
    /// document may also declare its own max_steps; this wins.
    #[serde(default)]
    pub max_steps: Option<u64>,
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
pub struct SessionChallengesParams {
    /// Session ID
    pub session_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SessionVerdictParams {
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
    /// Attach the session to a named account: the imported login lands in
    /// the account's private jar and is written back under its name after
    /// every action — one import per identity, no clobbering.
    #[serde(default)]
    pub account: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AccountVerifyParams {
    /// The account to check.
    pub name: String,
    /// Teach-once: the page that shows login state (its login wall if the
    /// account is logged out). Remembered after the first call.
    #[serde(default)]
    pub url: Option<String>,
    /// Teach-once: a JS expression that is truthy when logged in, e.g.
    /// `!!document.querySelector('.user-nick')`. Remembered after the first
    /// call — later calls can pass neither and rerun the spec.
    #[serde(default)]
    pub predicate: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AccountDeleteParams {
    /// The account to delete: stored record AND live jar.
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AccountLoginParams {
    /// The account to log in as (created implicitly on first use; 1-64
    /// chars of [a-zA-Z0-9_-]).
    pub name: String,
    /// The login page URL to open as this account.
    pub url: String,
    /// A JS expression truthy on the page the site lands on AFTER login,
    /// e.g. `!!document.querySelector('.user-nick')`. With it and no human
    /// step detected, the call waits for the automatic login bounce and
    /// stamps the account's verify spec on success. Without it, the call
    /// just opens the page and reports what kind of login it sees.
    #[serde(default)]
    pub predicate: Option<String>,
    /// Route through the engine proxy. Seeds a fresh account; an account
    /// with an existing record reuses its recorded egress.
    #[serde(default)]
    pub use_proxy: bool,
    /// Wait budget in ms for the automatic-login bounce (default 60000,
    /// clamped 1000..120000). Never spent while a human step
    /// (QR/SMS/password/slider) is outstanding — those return immediately
    /// with a session handoff.
    #[serde(default = "default_login_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_login_timeout_ms() -> u64 {
    60_000
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
