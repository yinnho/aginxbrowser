use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use crate::diting_dom::{parse_html, DomTree};
use crate::diting_js::runtime::JsRuntime;
use crate::diting_net::{HttpClient, NetError, Response};
use url::Url;

use crate::diting_browser::context::BrowserContext;
use crate::diting_browser::lifecycle::LifecycleState;
// Page decomposition (ARCHITECTURE.md P2 batch 3): navigation & settle,
// script execution, viewport/band capture, JS evaluation and network
// bookkeeping live in sibling files under this directory. The `page::`
// paths consumers use are unchanged — everything below still hangs off
// this module.
mod eval;
mod navigate;
mod net;
mod scripts;
mod viewport;

#[cfg(test)]
mod tests;

fn decode_data_uri(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let meta = &rest[..comma];
    let payload = &rest[comma + 1..];
    if meta.split(';').any(|t| t.eq_ignore_ascii_case("base64")) {
        let cleaned: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
        BASE64.decode(cleaned).ok()
    } else {
        Some(percent_decode(payload))
    }
}

fn percent_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = hex_val(b[i + 1]);
            let lo = hex_val(b[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(feature = "stealth")]
use crate::diting_net::StealthHttpClient;

/// Returns true when a JS-initiated navigation would step from a
/// non-file scheme into a file: URL. We treat that move as an SOP
/// violation because the existing realm survives the navigation and
/// can read the new document's body.
fn cross_scheme_to_file(from: &str, to: &str) -> bool {
    let to_is_file = Url::parse(to)
        .map(|u| u.scheme().eq_ignore_ascii_case("file"))
        .unwrap_or(false);
    if !to_is_file {
        return false;
    }
    Url::parse(from)
        .map(|u| !u.scheme().eq_ignore_ascii_case("file"))
        .unwrap_or(true)
}

/// Sub-resource fetch policy. http(s) is always fine; data: is allowed
/// because the bytes are inline in the URI (no network fetch, no SSRF);
/// file: is only allowed when the page itself was loaded from file:;
/// everything else (javascript:, chrome:, etc) is blocked.
/// Real Chrome allows data: subresources by default; Instagram and most
/// Meta properties depend on this for their inline bootstrap scripts.
fn subresource_allowed(page_url: Option<&Url>, resource: &str) -> bool {
    let Ok(target) = Url::parse(resource) else { return false };
    let scheme = target.scheme().to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" | "data" => true,
        "file" => page_url.map(|u| u.scheme().eq_ignore_ascii_case("file")).unwrap_or(false),
        _ => false,
    }
}

/// Chrome renders a response whose MIME says "this is not HTML" as a plain
/// text document: body is a single `<pre>` holding the source verbatim, so
/// newlines survive and markup characters stay inert. Feeding the bytes to
/// the HTML parser instead collapses the formatting and interprets any
/// `<tag>`-looking text as real elements.
fn renders_as_text_document(mime: &str) -> bool {
    if mime == "text/html" || mime == "application/xhtml+xml" || mime.starts_with("image/") {
        return false;
    }
    mime.starts_with("text/")
        || mime == "application/json"
        || mime == "application/javascript"
        || mime.ends_with("+json")
        || mime == "application/xml"
        || mime.ends_with("+xml")
}

fn plain_text_document(body: &str) -> DomTree {
    let mut escaped = String::with_capacity(body.len());
    for ch in body.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(ch),
        }
    }
    parse_html(&format!("<pre>{escaped}</pre>"))
}

/// Escape a value for safe inclusion inside a JavaScript template
/// literal. The previous implementation only escaped `\`, `` ` `` and
/// `${`; that left U+2028 / U+2029 (the JS-specific line terminators)
/// and other control characters as breakout vectors. Done at the
/// callsite means future tweaks come back to one function.
fn escape_for_js_template_literal(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '`' => out.push_str("\\`"),
            '$' => out.push_str("\\$"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            '\u{0000}' => out.push_str("\\0"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// One recorded network exchange (CDP `Network.requestWillBeSent` /
/// `responseReceived` shape). Recorded for every document + subresource
/// fetch and drained from the JS runtime for script-initiated requests;
/// read by the session /network and /har surfaces.
#[derive(Debug, Clone)]
pub struct NetworkEvent {
    pub request_id: String,
    pub url: String,
    pub method: String,
    pub resource_type: String,
    pub status: u16,
    pub headers: std::collections::HashMap<String, String>,
    pub response_headers: Arc<std::collections::HashMap<String, String>>,
    pub body_size: usize,
    pub timestamp: f64,
    /// Set on `status: 0` script-initiated rows that never produced a
    /// servable response (SSRF block, CORS refusal, transport failure).
    /// Navigation/subresource rows leave it `None`.
    pub error: Option<String>,
}

/// A response body retained for `get_response_body` (upstream #360). Bodies
/// are classified with the Chromium DevTools policy (see
/// `diting_net::decode_devtools_body`): replacement-free text is stored as a
/// string (`base64_encoded = false`, declared-GBK pages included, matching
/// Chrome); opaque or undecodable bodies are stored base64 so
/// `take_response_body_raw` is byte-exact.
#[derive(Debug, Clone)]
pub struct StoredResponseBody {
    pub body: String,
    pub base64_encoded: bool,
}

fn response_body_entry_limit() -> usize {
    std::env::var("AGINXBROWSER_NETWORK_BODY_BUFFER_ENTRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128)
}

fn response_body_byte_limit() -> usize {
    std::env::var("AGINXBROWSER_NETWORK_BODY_BUFFER_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024)
}

/// Module evaluation budget (ms): the timeout driving a module's load + eval
/// to completion during the script phase. Default 10s;
/// `AGINXBROWSER_MODULE_EVAL_TIMEOUT_MS` raises it for slow module graphs
/// (dev-server HMR clients — obscura#531).
fn module_eval_budget_ms() -> u64 {
    module_eval_budget_from(
        std::env::var("AGINXBROWSER_MODULE_EVAL_TIMEOUT_MS")
            .ok()
            .as_deref(),
    )
}

fn module_eval_budget_from(raw: Option<&str>) -> u64 {
    raw.and_then(|s| s.parse().ok()).unwrap_or(10_000)
}

/// Engine-side init hook: `AGINXBROWSER_INIT_SCRIPT=<path>` runs its file
/// contents before any page script on every navigation (same slot as CDP
/// preloads) — the pre-hydration instrumentation point for debugging apps
/// that bind during module eval.
fn env_init_script_source() -> Option<String> {
    env_init_script_from(std::env::var("AGINXBROWSER_INIT_SCRIPT").ok().as_deref())
}

fn env_init_script_from(path: Option<&str>) -> Option<String> {
    let source = path.and_then(|p| std::fs::read_to_string(p).ok())?;
    if source.trim().is_empty() {
        None
    } else {
        Some(source)
    }
}

/// Emulated media environment from CDP `Emulation.setEmulatedMedia`
/// (Playwright's `page.emulateMedia`). `features` holds `prefers-*`
/// (name, value) pairs; `media` is the emulated media type, `Some(None)`
/// meaning an explicit clear back to `screen` (Chrome's `""`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmulatedMedia {
    pub features: Vec<(String, String)>,
    pub media: Option<Option<String>>,
}

pub struct Page {
    pub id: String,
    /// Upstream frame-realm identifier: one Page can host sub-frame realms
    /// keyed by frame id. We run a single realm per page, so nothing reads
    /// it yet (frame-realm absorption is parked — see docs/engine/browser.md).
    #[allow(dead_code)]
    pub frame_id: String,
    pub url: Option<Url>,
    pub dom: Option<DomTree>,
    pub js: Option<JsRuntime>,
    /// sessionStorage snapshot for the CURRENT document, as `(origin, entries)`.
    /// sessionStorage is per-tab-per-origin: it survives a same-origin
    /// navigation (like a reload) but a cross-origin one discards it. The realm
    /// is rebuilt on every navigation (`init_js`) and parked on target switch
    /// (`suspend_js`/`resume_js`), so we snapshot before teardown and re-seed
    /// after rebuild — otherwise a same-origin navigation or a second target's
    /// evaluate wipes the store (upstream #678).
    session_storage: Option<(String, std::collections::HashMap<String, String>)>,
    /// Console calls drained out of the realm when it was suspended. Without
    /// this buffer, anything logged since the last drain died with the realm
    /// on a target switch or storm-control park, and the client saw an empty
    /// console after resume (obscura#971 same hole).
    suspended_console: Vec<(String, String, String)>,
    /// Viewport override for session_viewport / CDP
    /// setDeviceMetricsOverride: (width, height, mobile). Lives on the
    /// Page, not the realm, because every navigation rebuilds the realm and
    /// republishes the desktop persona viewport (`set_user_agent` →
    /// `__diting_setPersona`) — the override has to be replayed after that
    /// or the page silently flips back mid-session.
    viewport_override: Option<(f32, f32, bool)>,
    /// devicePixelRatio pin from the same override (CDP deviceScaleFactor
    /// above zero); None keeps the persona's dpr reporting through. Rides
    /// the same replay as `viewport_override`.
    dpr_override: Option<f64>,
    /// Emulated media environment from CDP `Emulation.setEmulatedMedia`
    /// (Playwright's `page.emulateMedia`). Lives on the Page for the same
    /// reason as `viewport_override` — every navigation rebuilds the realm,
    /// so the emulation has to be replayed or the page silently flips back
    /// to the persona defaults mid-session (#29).
    emulated_media: Option<EmulatedMedia>,
    /// 32-bit seed the JS persona draws its hardware identity from
    /// (screen/dpr/GPU/canvas). Lives on the Page because every navigation
    /// rebuilds the realm and `__diting_init` self-deletes after drawing a
    /// fresh identity — without a re-pin, one visit would flip its screen
    /// and GPU page to page, its own automation tell. Same pattern as the
    /// viewport override above.
    fp_seed: u64,
    pub lifecycle: LifecycleState,
    pub http_client: Arc<HttpClient>,
    pub context: Arc<BrowserContext>,
    pub title: String,
    /// URL of the document that initiated the current navigation, exposed to
    /// JS as `document.referrer`. Direct automation navigations leave this
    /// empty; document-initiated navigations (location.href, form submit)
    /// set it per strict-origin-when-cross-origin (upstream edb1785).
    pub referrer: String,
    /// Referrer Policy the main response delivered via its `Referrer-Policy`
    /// header (last valid comma token). Establishes the document's policy
    /// outright — a <meta name=referrer> cannot override it. Empty = none
    /// delivered; the meta / spec default take over.
    pub referrer_policy_header: String,
    /// WHATWG canonical name of the current document's character encoding
    /// (e.g. "UTF-8", "EUC-JP"), detected when the response body is decoded.
    /// Exposed to JS as `document.characterSet` and used for the URL query
    /// encoding override on `<a>`/`<area>` hrefs in legacy-charset documents.
    pub encoding: String,
    /// MIME type of the main response (lowercased, parameters stripped).
    /// Backs `document.contentType`. None = the response carried no
    /// Content-Type — the JS layer then falls back to URL sniffing.
    pub content_type: Option<String>,
    /// Navigation history for Page.getNavigationHistory / navigateToHistoryEntry.
    /// Entries are URLs in visit order; `history_index` is the current position.
    /// Pushed on every successful navigation; truncated on goBack -> new nav.
    pub history: Vec<String>,
    pub history_index: usize,
    pub network_events: Vec<NetworkEvent>,
    /// Events of the outgoing document, carried across the per-navigation
    /// reset of `network_events` (and the JS runtime swap) so the CDP drain
    /// — which only runs after the new document settles — can still emit
    /// them. Filled at the top of `navigate_single`; consumed by the CDP
    /// navigation emitter.
    ///
    /// `pub`: consumed by the product crate's CDP face since the workspace
    /// split (engine data, product emission order).
    pub carried_network_events: Vec<NetworkEvent>,
    /// The outgoing document's URL, kept beside the carried events so they
    /// can be emitted attributed to the document they belonged to.
    pub carried_network_url: String,
    network_event_counter: u32,
    /// Passive on_request/on_response callbacks, scoped to this page (upstream
    /// issue #408): they fire for document/subresource fetches this Page makes
    /// and for script-initiated fetch()/XHR in its realm, never for a sibling
    /// page's traffic, and die with the page.
    callbacks: Arc<crate::diting_net::CallbackRegistry>,
    /// Response bodies retained for `get_response_body`, keyed by the
    /// NetworkEvent request id (`{page}.{N}` for page-side fetches,
    /// `fetch-{N}` for script-initiated ones). LRU-bounded by
    /// `response_body_entry_limit` / `response_body_byte_limit`.
    response_bodies: std::collections::HashMap<String, StoredResponseBody>,
    response_body_order: std::collections::VecDeque<String>,
    /// `Network.setBlockedURLs` patterns: a hard block — matched static
    /// subresources (parser-time `<script src>`, `<link rel=stylesheet>`)
    /// fail to load without any client interaction. Deliberately separate
    /// from `Fetch.enable` interception, whose pause flow cannot cover
    /// navigation-time loads on this bridge (see `domains::fetch` docs).
    pub blocked_urls: Vec<String>,
    /// Fetch-domain interception kernel channel. `Some` means armed — the
    /// CDP bridge receives every script-initiated fetch()/XHR as an
    /// `InterceptedRequest` and answers with a resolution. Cleared by
    /// `set_fetch_intercept(None)`.
    intercept_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::diting_js::ops::InterceptedRequest>>,
    // Scripts to execute in the page's JS context BEFORE any of the page's
    // own scripts run — the CDP `Page.addScriptToEvaluateOnNewDocument`
    // contract. Includes `Runtime.addBinding` shims so puppeteer's
    // `exposeFunction` bindings exist before inline `<script>` tags execute.
    preload_scripts: Vec<String>,
    /// Page-scoped navigation deadline override. `None` falls back to
    /// `AGINXBROWSER_NAV_TIMEOUT_MS` (default 30s); set via
    /// `set_navigation_timeout` when an automation request carries an
    /// explicit per-call timeout (upstream parity).
    navigation_timeout_ms: Option<u64>,
    /// Storm backoff for the idle pump: when a pump slice ends in a watchdog
    /// termination, the page's runaway loop (e.g. a MutationObserver that
    /// mutates in its own callback) re-queues itself on every subsequent
    /// pump, so re-entering immediately just re-feeds it at 100% CPU for the
    /// session's life. Park until `storm_hot_until` instead; the backoff
    /// doubles per termination (200ms floor, 5s ceiling) and resets on the
    /// first clean idle settle.
    storm_backoff_ms: u64,
    storm_hot_until: Option<tokio::time::Instant>,
    /// #66 busy-storm accounting: (window start, `v8_active_ns` at window
    /// start). Every idle-pump iteration diffs the runtime's cumulative
    /// poll-execution time against this mark; when a trailing window of
    /// `js_busy_limit_secs()` (default 30s) shows ≥40% of wall time spent
    /// inside `poll_event_loop`, the realm froze. Such a burner never fires
    /// a watchdog (every callback stays under budget) and never reports
    /// idle (self-rescheduling driver), so the storm backoff above can't
    /// see it — measured 45-48% of a core for as long as the session lives.
    busy_mark: Option<(tokio::time::Instant, u64)>,
    /// Set when the busy window tripped: the idle pump parks instead of
    /// re-entering V8. Commands still run (eval stays useful for
    /// inspection); the next document swap (Navigate/SetContent) rebuilds
    /// the realm and clears this.
    busy_frozen: bool,
    #[cfg(feature = "stealth")]
    pub stealth_client: Option<Arc<StealthHttpClient>>,
}

impl Page {
    pub fn new(id: String, context: Arc<BrowserContext>) -> Self {
        let http_client = context.http_client.clone();
        // Chromium convention: the main frame's frameId == the targetId.
        // Playwright's frame manager looks up the main frame by targetId
        // (via target._targetInfo.targetId), so any divergence here makes
        // Page.getFrameTree return a frame the client cannot match,
        // triggering a Target.closeTarget and "Frame has been detached".
        let frame_id = id.clone();
        #[cfg(feature = "stealth")]
        let stealth_client = if context.stealth {
            // The wreq client backing StealthHttpClient does not speak SOCKS5.
            // Callers must validate the proxy scheme up front and fail loudly
            // (see obscura-cli) rather than silently rewriting socks5:// to
            // http://, which only works when the upstream happens to be a
            // Clash-style mixed-mode proxy and breaks plain SOCKS5 servers
            // like `ssh -ND` (#160).
            let emulation = context
                .tls_fingerprint
                .as_deref()
                .and_then(crate::diting_net::parse_tls_fingerprint)
                .unwrap_or(wreq_util::Profile::Chrome145);
            // Single source of truth for the page's identity: the context's
            // resolved UA drives every surface. Left to itself the stealth
            // client falls back to AGINXBROWSER_UA / a Linux default, which
            // put a Linux User-Agent and a Linux TLS fingerprint on stealth
            // document requests behind a macOS navigator/platform persona —
            // a one-glance bot tell (obscura #481 class).
            let os = crate::diting_net::emulation_os_for_ua(&context.user_agent);
            let mut client = StealthHttpClient::with_proxy_and_emulation(
                context.cookie_jar.clone(),
                context.proxy_url.as_deref(),
                Some(os),
                emulation,
            );
            // The context's private-network opt-in must reach the stealth
            // transport too, or a context that allows RFC1918 targets opens
            // its document requests on a client that rejects them (the
            // half-threaded-flag shape of obscura#793).
            client.allow_private_network = context.allow_private_network;
            if let Ok(mut guard) = client.user_agent.try_write() {
                *guard = context.user_agent.clone();
            }
            Some(Arc::new(client))
        } else {
            None
        };

        Page {
            id,
            frame_id,
            url: None,
            dom: None,
            js: None,
            lifecycle: LifecycleState::Idle,
            http_client,
            context,
            title: String::new(),
            referrer: String::new(),
            referrer_policy_header: String::new(),
            encoding: "UTF-8".to_string(),
            content_type: None,
            history: Vec::new(),
            history_index: 0,
            network_events: Vec::new(),
            carried_network_events: Vec::new(),
            carried_network_url: String::new(),
            network_event_counter: 0,
            session_storage: None,
            suspended_console: Vec::new(),
            viewport_override: None,
            dpr_override: None,
            emulated_media: None,
            fp_seed: u64::from_be_bytes(
                uuid::Uuid::new_v4().into_bytes()[..8].try_into().unwrap(),
            ),
            callbacks: Arc::new(crate::diting_net::CallbackRegistry::new()),
            response_bodies: std::collections::HashMap::new(),
            response_body_order: std::collections::VecDeque::new(),
            blocked_urls: Vec::new(),
            intercept_tx: None,
            preload_scripts: Vec::new(),
            navigation_timeout_ms: None,
            storm_backoff_ms: 0,
            storm_hot_until: None,
            busy_mark: None,
            busy_frozen: false,
            #[cfg(feature = "stealth")]
            stealth_client,
        }
    }

    /// Pin the hardware persona seed (screen/dpr/GPU/canvas all draw from
    /// it). Must run before the first navigation — init_js bakes the seed
    /// into the JS runtime on every navigation, so a pre-goto pin holds for
    /// the page's whole life. The account layer uses this to give each
    /// named identity one stable device.
    pub fn set_fingerprint_seed(&mut self, seed: u64) {
        self.fp_seed = seed;
    }

    /// Hard block from `Network.setBlockedURLs`: matched resources fail
    /// outright (Chrome semantics — no pause, no client round trip).
    /// `pub(crate)`: render-path fetchers outside this module (screenshot
    /// prefetch, pptx_native image export) share the same hard block.
    pub fn url_blocked(&self, url: &str) -> bool {
        if self.blocked_urls.is_empty() {
            return false;
        }
        self.blocked_urls
            .iter()
            .any(|p| crate::diting_net::url_pattern_matches(p, url))
    }

    /// Fetch the main document. Stealth mode bypasses the tracing client —
    /// the wreq-backed stealth transport has no callback hook (and stealth
    /// pages are exactly the ones whose observers should not double-fire on
    /// a side channel); observers still see scripts/stylesheets/fetches.
    async fn fetch_document(&self, url: &Url) -> Result<Response, NetError> {
        // CDP Fetch interception covers navigation documents too (Playwright
        // `page.route` / Puppeteer `setRequestInterception`). The document
        // request parks on the same resolution channel as a script-initiated
        // fetch; the load task runs spawned off the CDP loop, so the bridge
        // can answer it from a later command or the idle pump. No answer
        // inside the shared resolution timeout → fall through to the real
        // request (Continue semantics). Intercept resolution `Continue`
        // overrides (url/method/headers rewrites) are not applied to
        // documents — clients rewrite the bare `route.continue_()` case in
        // practice, and honored rewrites would need re-running the SSRF gate
        // on the substituted URL before commit.
        if let Some(tx) = &self.intercept_tx {
            let (resolver, rx) = tokio::sync::oneshot::channel();
            let mut headers = std::collections::HashMap::new();
            if let Ok(ua) = self.http_client.user_agent.try_read() {
                headers.insert("User-Agent".to_string(), ua.clone());
            }
            let request = crate::diting_js::ops::InterceptedRequest {
                url: url.to_string(),
                method: "GET".to_string(),
                headers,
                resource_type: "Document".to_string(),
                resolver,
            };
            if tx.send(request).is_ok() {
                let timeout_ms = crate::diting_js::ops::INTERCEPT_RESOLUTION_TIMEOUT_MS
                    .load(std::sync::atomic::Ordering::Relaxed);
                match tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    rx,
                )
                .await
                {
                    Ok(Ok(crate::diting_js::ops::InterceptResolution::Fulfill {
                        status,
                        headers,
                        body,
                    })) => {
                        return Ok(Response {
                            url: url.clone(),
                            status,
                            headers,
                            body,
                            redirected_from: Vec::new(),
                        });
                    }
                    Ok(Ok(crate::diting_js::ops::InterceptResolution::Fail { reason })) => {
                        return Err(NetError::Network(format!(
                            "request blocked by client, reason: {reason}"
                        )));
                    }
                    // Continue / dropped resolver / timeout: real request below.
                    Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {}
                }
            }
        }
        #[cfg(feature = "stealth")]
        if let Some(ref stealth) = self.stealth_client {
            return stealth.fetch(url).await;
        }
        // A JS/link-initiated navigation carries the referring document's
        // URL per browser semantics; direct automation navigations leave
        // self.referrer empty (edb1785) and go out bare.
        let referrer = if self.referrer.is_empty() { None } else { Some(self.referrer.as_str()) };
        self.http_client
            .fetch_with_callbacks(url, Some(&self.callbacks), crate::diting_net::ResourceType::Document, referrer)
            .await
    }
    fn init_js(&mut self) {
        // Drop any existing runtime so the JS realm starts clean on
        // every navigation. The old code reused the V8 isolate and
        // only re-bound `globalThis.document`, leaving window.onload,
        // custom window properties and event handlers from the prior
        // page in place. That made it possible for a page to set
        // attacker-controlled state, trigger a navigation, and then
        // run code in the next document's context.
        self.snapshot_session_storage();
        // Same drain as suspend_js: console calls the outgoing document
        // logged since the last pump must survive the realm swap (Chrome
        // preserves per-tab console history across navigations).
        if let Some(js) = self.js.as_mut() {
            let calls = js.take_pending_console_calls();
            self.suspended_console.extend(calls);
        }
        if self.js.is_some() {
            let _ = self.js.take();
        }

        // Thread the BrowserContext's proxy through to the ES-module loader
        // and op_fetch_url so dynamic imports and JS fetch() honour the
        // configured upstream proxy (#139). When proxy_url is None this is
        // equivalent to with_base_url() (direct connection).
        let mut rt = JsRuntime::with_base_url_and_proxy(
            &self.url_string(),
            self.context.proxy_url.clone(),
        );
        rt.set_url(&self.url_string());
        rt.set_encoding(&self.encoding);
        rt.set_content_type(self.content_type.as_deref().unwrap_or(""));
        rt.set_title(&self.title);
        rt.set_referrer(&self.referrer);
        rt.set_referrer_policy(&self.referrer_policy_header);
        // Re-pin this page's hardware persona before anything else reads it
        // (the fresh realm just drew a throwaway identity at construction).
        rt.set_fingerprint_seed(self.fp_seed);

        // JS-layer UA must match the HTTP-layer UA we advertise (set via
        // AGINXBROWSER_UA / context.user_agent). Hardcoding the stealth
        // client's Linux UA here left navigator.userAgent as Linux while HTTP
        // headers said macOS — anti-bot checks that read navigator (Baidu
        // Wenku's 安全验证) caught the mismatch. Prefer the context UA; fall
        // back to the stealth client's UA only if none is set.
        let ua_to_set = if let Ok(ua) = self.http_client.user_agent.try_read() {
            ua.clone()
        } else {
            #[cfg(feature = "stealth")]
            { if self.stealth_client.is_some() { crate::diting_net::STEALTH_USER_AGENT.to_string() } else { String::new() } }
            #[cfg(not(feature = "stealth"))]
            { String::new() }
        };
        if !ua_to_set.is_empty() {
            rt.set_user_agent(&ua_to_set);
        }
        let lang = std::env::var("AGINXBROWSER_ACCEPT_LANGUAGE")
            .unwrap_or_else(|_| "zh-CN,zh;q=0.9,en;q=0.8".to_string());
        rt.set_language(&lang);

        rt.set_cookie_jar(self.context.cookie_jar.clone());
        rt.set_http_client(self.http_client.clone());

        if let Some(tx) = &self.intercept_tx {
            rt.set_intercept_tx(tx.clone());
            // tx presence == armed: `set_fetch_intercept(None)` clears both,
            // so a realm rebuilt after navigation (init_js runs on every
            // document) resumes intercepting instead of silently passing
            // fetches straight through while the bridge still waits.
            rt.set_intercept_enabled(true);
        }
        if !self.blocked_urls.is_empty() {
            // setBlockedURLs must survive navigation like the intercept arm
            // does — the static loaders read the Page field directly, the
            // JS path (fetch()/XHR) needs it replayed into the fresh realm.
            rt.set_blocked_urls(self.blocked_urls.clone());
        }

        // Script-initiated fetch()/XHR fire the page's passive observers too
        // (upstream #408).
        rt.set_callbacks(self.callbacks.clone());

        if let Some(dom) = self.dom.take() {
            rt.set_dom(dom);
        }

        self.js = Some(rt);
        // Fresh document ⇒ fresh JS accounting: a busy-freeze from the
        // previous realm must not follow the operator into the new page
        // (#66 — Navigate/SetContent is the documented unfreeze path).
        self.busy_frozen = false;
        self.busy_mark = None;
        self.restore_session_storage();
        self.apply_viewport_override();
        self.apply_emulated_media();
    }

    /// Capture the live realm's `sessionStorage` into `self.session_storage`
    /// before the realm is dropped (navigation teardown or target switch).
    /// Runs one synchronous round-trip reading `location.origin` + every entry.
    fn snapshot_session_storage(&mut self) {
        let js = match self.js.as_mut() {
            Some(js) => js,
            None => return,
        };
        let expr = "(function(){ var o={}; var ks=Object.keys(sessionStorage); for (var i=0;i<ks.length;i++){ o[ks[i]]=sessionStorage.getItem(ks[i]); } return { origin: location.origin, entries: o }; })()";
        let val = match js.evaluate(expr) {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut map = match val {
            serde_json::Value::Object(m) => m,
            _ => return,
        };
        let entries = map.remove("entries");
        let origin = match map.get("origin").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return,
        };
        let entries = match entries {
            Some(serde_json::Value::Object(e)) => e,
            _ => return,
        };
        let mut store = std::collections::HashMap::new();
        for (k, v) in entries {
            if let Some(s) = v.as_str() {
                store.insert(k, s.to_string());
            }
        }
        self.session_storage = Some((origin, store));
    }

    /// Re-seed `sessionStorage` into the freshly rebuilt realm if its origin
    /// matches the snapshot's origin (same-origin navigation, or resume after
    /// a target switch). A cross-origin navigation leaves the snapshot behind
    /// and gets a fresh empty store, matching the per-tab-per-origin spec.
    fn restore_session_storage(&mut self) {
        let Some((origin, entries)) = self.session_storage.take() else {
            return;
        };
        let js = match self.js.as_mut() {
            Some(js) => js,
            None => return,
        };
        let new_origin = match js.evaluate("location.origin") {
            Ok(serde_json::Value::String(s)) => s,
            _ => return,
        };
        if new_origin != origin {
            return;
        }
        let mut seed = String::from("(function(){");
        for (k, v) in &entries {
            seed.push_str("sessionStorage.setItem(");
            seed.push_str(&serde_json::to_string(k).unwrap_or_else(|_| "null".to_string()));
            seed.push(',');
            seed.push_str(&serde_json::to_string(v).unwrap_or_else(|_| "null".to_string()));
            seed.push_str(");");
        }
        seed.push_str("})()");
        let _ = js.evaluate(&seed);
    }

    /// Resolve the document base URL per HTML spec:
    /// https://html.spec.whatwg.org/multipage/urls-and-fetching.html#document-base-url
    /// Falls back to self.url when no <base href> exists.
    fn resolve_base_url(&self) -> Option<url::Url> {
        let doc_url = self.url.as_ref()?;
        let base_href: Option<String> = self.js.as_ref().and_then(|js| {
            js.with_dom(|dom| {
                match dom.query_selector("base[href]") {
                    Ok(Some(nid)) => {
                        dom.get_node(nid).and_then(|n| n.get_attribute("href").map(|s| s.to_string()))
                    }
                    _ => None,
                }
            }).flatten()
        });
        match base_href {
            Some(href) => doc_url.join(&href).ok(),
            None => Some(doc_url.clone()),
        }
    }

    #[allow(dead_code)] // manual hook; navigations run preloads themselves (init_js)
    pub fn execute_preload_script(&mut self, source: &str) -> Result<(), String> {
        if let Some(js) = &mut self.js {
            js.execute_script("<preload>", source)
        } else {
            Err("No JS runtime".to_string())
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // storm control: park the realm between microtask bursts
    pub fn suspend_js(&mut self) {
        self.snapshot_session_storage();
        if let Some(js) = &self.js {
            if let Some(dom) = js.take_dom() {
                self.dom = Some(dom);
            }
            // Drain before the realm drops: console calls logged since the
            // last pump must survive suspension like the DOM does.
            self.suspended_console.extend(js.take_pending_console_calls());
        }
        self.js = None;
    }

    #[cfg_attr(not(test), allow(dead_code))] // storm control: re-arm after suspend_js
    pub fn resume_js(&mut self) {
        if self.js.is_some() {
            return;
        }
        self.init_js();
    }

    #[cfg_attr(not(test), allow(dead_code))] // tests assert on the suspend/resume lifecycle
    pub fn has_js(&self) -> bool {
        self.js.is_some()
    }

    pub fn take_pending_navigation(&self) -> Option<(String, String, String)> {
        if let Some(js) = &self.js {
            js.take_pending_navigation()
        } else {
            None
        }
    }

    pub fn take_pending_write_nav(&self) -> bool {
        match &self.js {
            Some(js) => js.take_pending_write_nav(),
            None => false,
        }
    }

    #[allow(dead_code)] // CDP Runtime.addBinding parity — drained as bindingCalled events
    pub fn take_pending_binding_calls(&self) -> Vec<(String, String)> {
        if let Some(js) = &self.js {
            js.take_pending_binding_calls()
        } else {
            Vec::new()
        }
    }

    /// Drain queued console calls (level, message, page URL at log time)
    /// for CDP `Runtime.consoleAPICalled` and the session console ring.
    /// Returns the suspension buffer first, then the live realm's pending
    /// calls, so ordering across a suspend/resume round-trip is preserved.
    pub fn take_pending_console_calls(&mut self) -> Vec<(String, String, String)> {
        let mut calls = std::mem::take(&mut self.suspended_console);
        if let Some(js) = &self.js {
            calls.extend(js.take_pending_console_calls());
        }
        calls
    }

    /// Set the session-side dialog policy for window.confirm/prompt (see
    /// `JsState::dialog_accept`); None keeps the stored prompt text.
    pub fn set_dialog_policy(&self, accept: bool, prompt_text: Option<String>) {
        if let Some(js) = &self.js {
            js.set_dialog_policy(accept, prompt_text);
        }
    }

    /// Current dialog policy: (accept, prompt_text).
    pub fn dialog_policy(&self) -> (bool, Option<String>) {
        match &self.js {
            Some(js) => js.dialog_policy(),
            None => (false, None),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // engine path is live (init_js runs these); no bin caller registers one yet
    pub fn set_preload_scripts(&mut self, scripts: Vec<String>) {
        self.preload_scripts = scripts;
    }

    /// Append one preload script, keeping any already registered (CDP
    /// `Page.addScriptToEvaluateOnNewDocument` is additive; our
    /// `set_preload_scripts` replaces the whole group).
    #[cfg_attr(not(test), allow(dead_code))] // batch-1 absorption; wire at session init when a preload need lands
    pub fn add_preload_script(&mut self, script: String) {
        self.preload_scripts.push(script);
    }

    pub async fn process_pending_navigation(&mut self) -> Result<bool, PageError> {
        if let Some((url, method, body)) = self.take_pending_navigation() {
            // A navigation the page asked for itself is document-initiated:
            // the first hop carries a referrer per
            // strict-origin-when-cross-origin, unlike direct automation
            // navigations which send none (upstream parity).
            let source_url = self
                .url
                .as_ref()
                .and_then(|source| {
                    Url::parse(&url)
                        .ok()
                        .map(|target| crate::diting_net::client::HttpClient::navigation_referrer(source, &target))
                })
                .unwrap_or_default();
            self.navigate_with_wait_post_ref(
                &url,
                crate::diting_browser::lifecycle::WaitUntil::Load,
                &method,
                &body,
                &source_url,
            )
            .await?;
            Ok(true)
        } else {
            // A page that routed itself through history has still navigated
            // (SPA pushState adoption — see fork_virtual_url.rs).
            Ok(self.sync_virtual_url())
        }
    }

    /// Arm / disarm the Fetch-domain interception kernel. `Some(tx)` routes
    /// every script-initiated fetch()/XHR through the CDP bridge as an
    /// `InterceptedRequest` (the bridge answers via the per-request oneshot);
    /// `None` disarms and lets in-flight resolvers fall through to the real
    /// request. Arming survives navigation — `init_js` carries the channel
    /// into each rebuilt realm.
    pub fn set_fetch_intercept(&mut self, tx: Option<tokio::sync::mpsc::UnboundedSender<crate::diting_js::ops::InterceptedRequest>>) {
        self.intercept_tx = tx.clone();
        if let Some(js) = &self.js {
            match tx {
                Some(tx) => {
                    js.set_intercept_tx(tx);
                    js.set_intercept_enabled(true);
                }
                None => js.set_intercept_enabled(false),
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PageError {
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Network error: {0}")]
    NetworkError(String),

    /// Not HTTP 3xx redirects (those are `NetError::TooManyRedirects`) —
    /// this counts documents in a JS-initiated navigation chain. The count
    /// includes the requested document, so a limit of N buys N-1
    /// navigations on top. The message must name the layer: operators
    /// debugging "too many redirects" against a server that never 3xx'd
    /// lose hours one layer down (obscura#664).
    #[error("client navigation chain exceeded {0} documents (JS location/form hops, not HTTP redirects) — raise AGINXBROWSER_NAV_CHAIN_LIMIT if this flow is legitimate")]
    TooManyClientNavigations(usize),
}

impl From<NetError> for PageError {
    fn from(e: NetError) -> Self {
        PageError::NetworkError(e.to_string())
    }
}
