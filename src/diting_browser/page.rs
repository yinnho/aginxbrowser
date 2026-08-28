use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use crate::diting_dom::{parse_html, DomTree};
use crate::diting_js::runtime::JsRuntime;
use crate::diting_net::{HttpClient, NetError, Response};
use url::Url;

use crate::diting_browser::context::BrowserContext;
use crate::diting_browser::lifecycle::LifecycleState;

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
/// fetch; no reader beyond tests exists yet — the /network service endpoint
/// is the intended consumer.
#[allow(dead_code)]
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
}

/// A response body retained for `get_response_body` (upstream #360). Text
/// bodies are stored lossy-UTF-8 (`base64_encoded = false`); binary bodies
/// base64 so `take_response_body_raw` is byte-exact.
#[cfg_attr(not(test), allow(dead_code))] // read by tests; CDP getResponseBody consumer pending
#[derive(Debug, Clone)]
pub struct StoredResponseBody {
    pub body: String,
    pub base64_encoded: bool,
}

/// True when a Content-Type deserves text storage rather than base64. No
/// Content-Type at all counts as text, matching the HTML-parse default.
fn is_text_like_content_type(content_type: Option<&str>) -> bool {
    let ct = match content_type {
        Some(c) => c.split(';').next().unwrap_or(c).trim().to_ascii_lowercase(),
        None => return true,
    };
    if ct.is_empty() {
        return true;
    }
    ct.starts_with("text/")
        || ct == "application/json"
        || ct == "application/xml"
        || ct == "application/xhtml+xml"
        || ct == "application/javascript"
        || ct == "application/ecmascript"
        || ct == "image/svg+xml"
        || ct.ends_with("+json")
        || ct.ends_with("+xml")
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

/// Compute the default `strict-origin-when-cross-origin` referrer for a
/// document-initiated navigation (upstream edb1785). Same-origin sends the
/// full source URL minus fragment/credentials; cross-origin sends only the
/// origin; downgrades (https -> http) and non-HTTP(S) schemes send nothing.
/// Referrer-Policy overrides are not yet plumbed through.
fn navigation_referrer(source: &Url, target: &Url) -> String {
    if !matches!(source.scheme(), "http" | "https")
        || !matches!(target.scheme(), "http" | "https")
        || (source.scheme() == "https" && target.scheme() == "http")
    {
        return String::new();
    }

    if source.origin() == target.origin() {
        let mut sanitized = source.clone();
        sanitized.set_fragment(None);
        let _ = sanitized.set_username("");
        let _ = sanitized.set_password(None);
        return sanitized.to_string();
    }

    let mut origin = source.origin().ascii_serialization();
    origin.push('/');
    origin
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
    pub lifecycle: LifecycleState,
    pub http_client: Arc<HttpClient>,
    pub context: Arc<BrowserContext>,
    pub title: String,
    /// URL of the document that initiated the current navigation, exposed to
    /// JS as `document.referrer`. Direct automation navigations leave this
    /// empty; document-initiated navigations (location.href, form submit)
    /// set it per strict-origin-when-cross-origin (upstream edb1785).
    pub referrer: String,
    /// WHATWG canonical name of the current document's character encoding
    /// (e.g. "UTF-8", "EUC-JP"), detected when the response body is decoded.
    /// Exposed to JS as `document.characterSet` and used for the URL query
    /// encoding override on `<a>`/`<area>` hrefs in legacy-charset documents.
    pub encoding: String,
    /// Navigation history for Page.getNavigationHistory / navigateToHistoryEntry.
    /// Entries are URLs in visit order; `history_index` is the current position.
    /// Pushed on every successful navigation; truncated on goBack -> new nav.
    pub history: Vec<String>,
    pub history_index: usize,
    pub network_events: Vec<NetworkEvent>,
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
    pub intercept_enabled: bool,
    pub intercept_block_patterns: Vec<String>,
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
                .unwrap_or(wreq_util::Emulation::Chrome145);
            Some(Arc::new(StealthHttpClient::with_proxy_and_emulation(
                context.cookie_jar.clone(),
                context.proxy_url.as_deref(),
                None,
                emulation,
            )))
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
            encoding: "UTF-8".to_string(),
            history: Vec::new(),
            history_index: 0,
            network_events: Vec::new(),
            network_event_counter: 0,
            session_storage: None,
            callbacks: Arc::new(crate::diting_net::CallbackRegistry::new()),
            response_bodies: std::collections::HashMap::new(),
            response_body_order: std::collections::VecDeque::new(),
            intercept_enabled: false,
            intercept_block_patterns: Vec::new(),
            intercept_tx: None,
            preload_scripts: Vec::new(),
            navigation_timeout_ms: None,
            storm_backoff_ms: 0,
            storm_hot_until: None,
            #[cfg(feature = "stealth")]
            stealth_client,
        }
    }

    fn should_block_url(&self, url: &str) -> bool {
        if !self.intercept_enabled || self.intercept_block_patterns.is_empty() {
            return false;
        }
        for pattern in &self.intercept_block_patterns {
            if pattern == "*" { return true; }
            if pattern.starts_with('*') && pattern.ends_with('*') {
                if url.contains(&pattern[1..pattern.len()-1]) { return true; }
            } else if pattern.starts_with('*') {
                if url.ends_with(&pattern[1..]) { return true; }
            } else if pattern.ends_with('*') {
                if url.starts_with(&pattern[..pattern.len()-1]) { return true; }
            } else if url.contains(pattern) {
                return true;
            }
        }
        false
    }

    /// Fetch the main document. Stealth mode bypasses the tracing client —
    /// the wreq-backed stealth transport has no callback hook (and stealth
    /// pages are exactly the ones whose observers should not double-fire on
    /// a side channel); observers still see scripts/stylesheets/fetches.
    async fn fetch_document(&self, url: &Url) -> Result<Response, NetError> {
        #[cfg(feature = "stealth")]
        if let Some(ref stealth) = self.stealth_client {
            return stealth.fetch(url).await;
        }
        self.http_client
            .fetch_with_callbacks(url, Some(&self.callbacks), crate::diting_net::ResourceType::Document)
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
        rt.set_title(&self.title);
        rt.set_referrer(&self.referrer);

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
        }

        // Script-initiated fetch()/XHR fire the page's passive observers too
        // (upstream #408).
        rt.set_callbacks(self.callbacks.clone());

        if let Some(dom) = self.dom.take() {
            rt.set_dom(dom);
        }

        self.js = Some(rt);
        self.restore_session_storage();
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

    async fn execute_scripts(&mut self) {
        tracing::info!("execute_scripts called, js runtime exists: {}", self.js.is_some());
        // Compute document base URL, respecting <base href>.
        let document_base = self.resolve_base_url();
        // Soft deadline on the entire script-execution phase. Heavy SPAs
        // (GitHub, Linear, CodeSandbox) ship 50+ scripts and our serial
        // fetch + execute loop can blow past a 25s Puppeteer goto timeout.
        // Override via AGINXBROWSER_SCRIPT_DEADLINE_MS for slow networks.
        let script_deadline_ms: u64 = std::env::var("AGINXBROWSER_SCRIPT_DEADLINE_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10_000);
        let script_deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(script_deadline_ms);

        // Hard backstop over the WHOLE script-execution phase. Inline scripts
        // run back-to-back with no await between them, so neither the soft
        // deadline above (only checked between scripts) nor the per-script guard
        // can interrupt a page that burns the budget across many synchronous
        // scripts (the real-world SPA / anti-bot busy-loop hang). This watchdog
        // terminates the isolate if cumulative synchronous script work overruns.
        let exec_wd = self
            .js
            .as_mut()
            .map(|js| js.arm_watchdog(std::time::Duration::from_millis(script_deadline_ms + 1000)));

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum ScriptKind {
            Classic,
            Module,
            ImportMap,
        }

        #[derive(Debug)]
        struct ScriptInfo {
            src: Option<String>,
            inline: String,
            is_defer: bool,
            is_async: bool,
            kind: ScriptKind,
            nid: u32,
            /// Document base URL at this element's parser encounter point.
            /// A later <base href> must not rebase an earlier import map
            /// (upstream 34373c3 temporal-base semantics).
            base_url: String,
        }

        let all_scripts = match &self.js {
            Some(js) => {
                let document_url = self.url_string();
                js.with_dom(|dom| {
                    let script_ids = dom.query_selector_all("script").unwrap_or_default();
                    // Walk the tree once tracking the <base href> in effect at
                    // each script's encounter position.
                    let mut bases_at_script = std::collections::HashMap::new();
                    let mut active_base = Url::parse(&document_url).ok();
                    let mut found_base = false;
                    if let Some(root) = Some(dom.document()) {
                        for nid in dom.descendants(root) {
                            let Some(node) = dom.get_node(nid) else { continue };
                            let Some(name) = node.as_element() else { continue };
                            if name.local.as_ref() == "base" && !found_base {
                                if let Some(href) = node.get_attribute("href") {
                                    found_base = true;
                                    if let Some(resolved) = active_base
                                        .as_ref()
                                        .and_then(|base| base.join(&href).ok())
                                    {
                                        active_base = Some(resolved);
                                    }
                                }
                            } else if name.local.as_ref() == "script" {
                                bases_at_script.insert(
                                    nid.raw(),
                                    active_base
                                        .as_ref()
                                        .map(ToString::to_string)
                                        .unwrap_or_else(|| document_url.clone()),
                                );
                            }
                        }
                    }
                    let mut scripts = Vec::new();

                    for sid in script_ids {
                        if let Some(node) = dom.get_node(sid) {
                            let src = node.get_attribute("src").map(|s| s.to_string());
                            let script_type = node
                                .get_attribute("type")
                                .unwrap_or("")
                                .trim()
                                .to_ascii_lowercase();
                            let is_defer = node.get_attribute("defer").is_some();
                            let is_async = node.get_attribute("async").is_some();
                            let kind = match script_type.as_str() {
                                "module" => ScriptKind::Module,
                                "importmap" => ScriptKind::ImportMap,
                                "" | "text/javascript" | "application/javascript" => {
                                    ScriptKind::Classic
                                }
                                _ => continue,
                            };

                            let inline_code = if src.is_none() {
                                dom.text_content(sid)
                            } else {
                                String::new()
                            };

                            if matches!(kind, ScriptKind::ImportMap)
                                || src.is_some()
                                || !inline_code.trim().is_empty()
                            {
                                scripts.push(ScriptInfo {
                                    src,
                                    inline: inline_code,
                                    is_defer,
                                    is_async,
                                    kind,
                                    nid: sid.raw(),
                                    base_url: bases_at_script
                                        .get(&sid.raw())
                                        .cloned()
                                        .unwrap_or_else(|| document_url.clone()),
                                });
                            }
                        }
                    }
                    scripts
                }).unwrap_or_default()
            }
            None => return,
        };

        // Import maps register before any module graph starts (upstream
        // 34373c3). Parser-discovered maps merge in encounter order using the
        // base URL in effect at each element; a later map cannot rebind a
        // specifier an earlier resolution already observed.
        for script in &all_scripts {
            if script.kind == ScriptKind::ImportMap {
                if script.src.is_some() {
                    tracing::warn!("External import maps are not supported");
                    continue;
                }
                if let Some(js) = &self.js {
                    if let Err(error) = js.add_import_map(&script.inline, &script.base_url) {
                        tracing::warn!("Ignoring invalid import map: {}", error);
                    }
                }
            }
        }

        let mut regular = Vec::new();
        let mut deferred = Vec::new();
        let mut async_scripts = Vec::new();

        let mut module_scripts: Vec<ScriptInfo> = Vec::new();

        for script in all_scripts {
            match script.kind {
                ScriptKind::Module => module_scripts.push(script),
                ScriptKind::ImportMap => continue,
                ScriptKind::Classic => {
                    if script.is_defer {
                        deferred.push(script);
                    } else if script.is_async {
                        async_scripts.push(script);
                    } else {
                        regular.push(script);
                    }
                }
            }
        }

        let scripts = regular;

        tracing::info!("Found {} regular + {} deferred + {} async scripts", scripts.len(), deferred.len(), async_scripts.len());
        let all_to_execute: Vec<ScriptInfo> = scripts.into_iter()
            .chain(deferred.into_iter())
            .chain(async_scripts.into_iter())
            .collect();

        let mut resolved: Vec<(usize, String)> = Vec::new();
        let mut fetch_tasks: Vec<(usize, String)> = Vec::new();

        for (i, script) in all_to_execute.iter().enumerate() {
            if let Some(src_url) = &script.src {
                let full_url = if src_url.starts_with("http://") || src_url.starts_with("https://") {
                    src_url.clone()
                } else if let Some(base) = &document_base {
                    base.join(src_url).map(|u| u.to_string()).unwrap_or_else(|_| src_url.clone())
                } else {
                    src_url.clone()
                };

                if !subresource_allowed(self.url.as_ref(), &full_url) {
                    // Block file://, data:, javascript:, and other
                    // off-origin schemes from being injected as a
                    // <script src>. Without this an http page can
                    // include <script src="file:///etc/passwd"> and
                    // see the body parsed as JS source.
                    tracing::warn!(
                        "blocking cross-scheme <script src>: page={} src={}",
                        self.url_string(),
                        full_url,
                    );
                    continue;
                }
                if self.should_block_url(&full_url) {
                    tracing::info!("Blocked script by interception: {}", full_url);
                    continue;
                }
                resolved.push((i, full_url.clone()));
                fetch_tasks.push((i, full_url));
            }
        }

        let client = self.http_client.clone();
        let script_callbacks = self.callbacks.clone();
        let fetch_futures: Vec<_> = fetch_tasks.iter().map(|(idx, url)| {
            let client = client.clone();
            let script_callbacks = script_callbacks.clone();
            let url = url.clone();
            let idx = *idx;
            async move {
                let parsed = Url::parse(&url).unwrap_or_else(|_| Url::parse("about:blank").unwrap());
                if parsed.scheme() == "data" {
                    // data: URIs are inline; decode locally, no network fetch.
                    // Instagram and other Meta properties serve their bootstrap
                    // as <script src="data:application/x-javascript;base64,...">.
                    let body = decode_data_uri(&url).unwrap_or_default();
                    let content_type = url
                        .strip_prefix("data:")
                        .and_then(|s| s.split(',').next())
                        .unwrap_or("application/javascript")
                        .split(';')
                        .next()
                        .unwrap_or("application/javascript")
                        .to_string();
                    let mut headers = std::collections::HashMap::new();
                    headers.insert("content-type".to_string(), content_type);
                    let resp = crate::diting_net::Response {
                        url: parsed,
                        status: 200,
                        headers,
                        body,
                        redirected_from: Vec::new(),
                    };
                    return Some((idx, url, resp));
                }
                match client
                    .fetch_with_callbacks(&parsed, Some(script_callbacks.as_ref()), crate::diting_net::ResourceType::Script)
                    .await
                {
                    Ok(resp) => Some((idx, url, resp)),
                    Err(e) => {
                        tracing::warn!("Failed to fetch script {}: {}", url, e);
                        None
                    }
                }
            }
        }).collect();

        // Bound concurrency: a page with 100 external scripts would
        // otherwise open 100 sockets at once, exhausting the connection
        // pool / ephemeral ports and triggering OS-level backpressure.
        // 16 is well above the per-host pool ceiling most browsers use
        // and matches what real Chrome does for a given origin.
        use futures::StreamExt as _;
        let fetch_stream = futures::stream::iter(fetch_futures)
            .buffer_unordered(16);
        let fetch_results = match tokio::time::timeout_at(
            script_deadline,
            fetch_stream.collect::<Vec<_>>(),
        ).await {
            Ok(results) => results,
            Err(_) => {
                tracing::warn!(
                    "execute_scripts: fetch deadline reached, some scripts may not have loaded"
                );
                Vec::new()
            }
        };

        let mut fetched: std::collections::HashMap<usize, (String, String, crate::diting_net::Response)> = std::collections::HashMap::new();
        for result in fetch_results {
            if let Some((idx, url, resp)) = result {
                // Script bodies: only the HTTP Content-Type charset matters
                // (no in-band meta-charset for JS).
                let code = crate::diting_net::decode_non_html(&resp.body, resp.content_type());
                fetched.insert(idx, (url, code, resp));
            }
        }

        // Spec: readyState is "loading" while parser-discovered scripts execute.
        // Scripts that check readyState === 'loading' will register DOMContentLoaded
        // listeners instead of calling their callback immediately.
        if let Some(js) = &mut self.js {
            let _ = js.execute_script("<ready-state>", "globalThis.__documentReadyState__ = 'loading';");
        }

        // CDP `Page.addScriptToEvaluateOnNewDocument` contract: preload
        // sources must run BEFORE any of the page's own scripts. This is
        // also where puppeteer's `exposeFunction` wrapper installs itself —
        // if preload runs after page scripts, every early binding call
        // hits an undefined function and silently no-ops.
        let preload_sources = self.preload_scripts.clone();
        if let Some(js) = &mut self.js {
            for source in &preload_sources {
                if let Err(e) = js.execute_script_guarded("<preload>", source.as_str()) {
                    tracing::debug!("Preload script error: {}", e);
                }
            }
        }

        for (i, script) in all_to_execute.iter().enumerate() {
            if tokio::time::Instant::now() >= script_deadline {
                tracing::warn!(
                    "execute_scripts: deadline reached, skipping {} remaining scripts",
                    all_to_execute.len() - i,
                );
                break;
            }
            if script.src.is_some() {
                if let Some((url, code, resp)) = fetched.remove(&i) {
                    tracing::info!("Executing script ({} bytes): {}", code.len(), url);
                    self.record_network_event_with_body(&url, "GET", "Script", resp.status, &resp.headers, &resp.body);
                    if let Some(js) = &mut self.js {
                        let _ = js.execute_script("<current-script>", &format!("globalThis.__currentScriptNid={};", script.nid));
                        if let Err(e) = js.execute_script_guarded(&url, &code) {
                            tracing::warn!("Script error ({}): {}", url, e);
                        }
                        let _ = js.execute_script("<current-script>", "globalThis.__currentScriptNid=0;");
                    }
                }
            } else if !script.inline.is_empty() {
                if let Some(js) = &mut self.js {
                    let _ = js.execute_script("<current-script>", &format!("globalThis.__currentScriptNid={};", script.nid));
                    if let Err(e) = js.execute_script_guarded("<inline>", &script.inline) {
                        tracing::warn!("Inline script error: {}", e);
                    }
                    let _ = js.execute_script("<current-script>", "globalThis.__currentScriptNid=0;");
                }
            }
        }

        for module_script in &module_scripts {
            if tokio::time::Instant::now() >= script_deadline {
                tracing::warn!("execute_scripts: deadline reached, skipping remaining module scripts");
                break;
            }
            if let Some(ref src) = module_script.src {
                let full_url = if src.starts_with("http://") || src.starts_with("https://") || src.starts_with("data:") {
                    src.clone()
                } else {
                    Url::parse(&module_script.base_url)
                        .ok()
                        .and_then(|base| base.join(src).ok())
                        .map(|u| u.to_string())
                        .unwrap_or_else(|| src.clone())
                };

                tracing::info!("Loading ES module: {}", full_url);
                if let Some(js) = &mut self.js {
                    match js.load_module(&full_url, 10_000).await {
                        Ok(()) => {
                            tracing::info!("ES module loaded: {}", full_url);
                            self.record_network_event(&full_url, "GET", "Script", 200, &std::collections::HashMap::new(), 0);
                        }
                        Err(e) => {
                            tracing::warn!("ES module error ({}): {}", full_url, e);
                        }
                    }
                }
            } else if !module_script.inline.is_empty() {
                let base = module_script.base_url.clone();
                if let Some(js) = &mut self.js {
                    if let Err(e) = js.load_inline_module(&module_script.inline, &base, 10_000).await {
                        tracing::warn!("Inline ES module error: {}", e);
                    }
                }
            }
        }

        if let Some(js) = &mut self.js {
            // Spec order: readyState -> interactive, fire DOMContentLoaded on both
            // document and window, then readyState -> complete, fire load.
            let _ = js.execute_script("<load-events>",
                "globalThis.__documentReadyState__ = 'interactive';\n\
                 try { document.dispatchEvent(new Event('DOMContentLoaded', {bubbles:false,cancelable:false})); } catch(e) {}\n\
                 try { window.dispatchEvent(new Event('DOMContentLoaded', {bubbles:false,cancelable:false})); } catch(e) {}\n\
                 if (typeof window.onload === 'function') { try { window.onload(); } catch(e) {} }\n\
                 // HTML spec: event handler content attributes on <body> for
                 // window-evented names (onload & friends) are exposed as the
                 // matching Window handler. `<body onload=\"...\">` therefore
                 // runs when window's load fires — not only when the load
                 // event is dispatched on the body itself. Byte-WAF challenge
                 // pages (juejin.cn class) drive their whole PoW from
                 // `<body onload=\"readygo()\">`, so without this forwarding
                 // the challenge never starts and the page hangs on
                 // \"Please wait...\" forever.
                 try {\n\
                     (function() {\n\
                         var b = document.body;\n\
                         if (!b) return;\n\
                         var h = b.onload;\n\
                         if (typeof h !== 'function' && b._resolveInlineHandler) h = b._resolveInlineHandler('onload');\n\
                         if (typeof h === 'function' && h !== window.onload) h.call(b, new Event('load'));\n\
                     })();\n\
                 } catch(e) {}\n\
                 globalThis.__documentReadyState__ = 'complete';\n\
                 try { window.dispatchEvent(new Event('load', {bubbles:false,cancelable:false})); } catch(e) {}");
        }

        if let Some(js) = &mut self.js {
            // Bound the post-script settle loop by wall clock, not just by the
            // 10ms-tick branch. The old code only consulted `deadline` inside
            // the `Err(_)` arm (when the inner tick timed out), so a steady
            // stream of inflight XHR/fetch (active_requests() > 0) kept the
            // loop running indefinitely because it took the `Ok(Ok(()))` arm
            // and slept 1ms each iteration without ever checking the clock.
            // On busy sites this could keep the V8 lock held for tens of
            // seconds, wedging the entire CDP dispatcher (see triage for
            // issue series around the 40-site compat sweep).
            // A single run_event_loop poll that pins the thread inside V8 makes
            // the per-poll tokio timeouts below useless, so guard the whole loop
            // with a watchdog that fires 250ms past the longest deadline.
            //
            // A dynamic external script may still be in flight at 500ms. Keep
            // pumping only while such a fetch is pending, up to a separate
            // bounded budget, so normal pages and unrelated fetches retain the
            // fast path (upstream a6bb741).
            let dynamic_settle_ms = std::env::var("AGINXBROWSER_DYNAMIC_SCRIPT_SETTLE_MS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(3_000)
                .max(500);
            let settle_wd = js.arm_watchdog(std::time::Duration::from_millis(dynamic_settle_ms + 250));
            let started = tokio::time::Instant::now();
            let deadline = started + tokio::time::Duration::from_millis(500);
            let dynamic_deadline = started + tokio::time::Duration::from_millis(dynamic_settle_ms);
            let mut idle_count = 0u32;
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline
                    && (now >= dynamic_deadline || !js.has_pending_dynamic_scripts())
                {
                    break;
                }
                let result = tokio::time::timeout(
                    tokio::time::Duration::from_millis(10),
                    js.run_event_loop(),
                ).await;

                match result {
                    Ok(Ok(())) => {
                        if self.http_client.active_requests() == 0 {
                            idle_count += 1;
                            if idle_count >= 2 {
                                break;
                            }
                            tokio::task::yield_now().await;
                        } else {
                            idle_count = 0;
                            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                        }
                    }
                    Ok(Err(_)) => break,
                    Err(_) => {
                        idle_count = 0;
                    }
                }
            }
            js.disarm_watchdog(settle_wd);
        }
        if let Some(token) = exec_wd {
            if let Some(js) = self.js.as_mut() {
                js.disarm_watchdog(token);
            }
        }
    }

    pub async fn navigate(&mut self, url_str: &str) -> Result<(), PageError> {
        self.navigate_with_wait(url_str, crate::diting_browser::lifecycle::WaitUntil::Load).await
    }

    pub async fn navigate_with_wait(
        &mut self,
        url_str: &str,
        wait_until: crate::diting_browser::lifecycle::WaitUntil,
    ) -> Result<(), PageError> {
        self.navigate_with_wait_post(url_str, wait_until, "GET", "").await
    }

    pub async fn navigate_with_wait_post(
        &mut self,
        url_str: &str,
        wait_until: crate::diting_browser::lifecycle::WaitUntil,
        method: &str,
        body: &str,
    ) -> Result<(), PageError> {
        // Direct automation navigations carry no referrer (upstream edb1785:
        // only document-initiated navigations set one; the JS-triggered chain
        // inside the inner loop stamps each subsequent hop).
        self.navigate_with_wait_post_ref(url_str, wait_until, method, body, "")
            .await
    }

    /// Page-scoped navigation deadline: `set_navigation_timeout` when the
    /// automation request carries an explicit timeout, else
    /// `AGINXBROWSER_NAV_TIMEOUT_MS` (default 30s). Complements the env var the
    /// way upstream does (structured field over process-wide default).
    fn navigation_timeout(&self) -> tokio::time::Duration {
        let ms = self.navigation_timeout_ms.unwrap_or_else(|| {
            std::env::var("AGINXBROWSER_NAV_TIMEOUT_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30_000)
        });
        tokio::time::Duration::from_millis(ms)
    }

    /// Set a page-scoped navigation deadline in milliseconds; `None` restores
    /// the process-wide default.
    #[cfg_attr(not(test), allow(dead_code))] // batch-1 upstream parity; wire when per-request nav timeout becomes an API input
    pub fn set_navigation_timeout(&mut self, ms: Option<u64>) {
        self.navigation_timeout_ms = ms;
    }

    async fn navigate_with_wait_post_ref(
        &mut self,
        url_str: &str,
        wait_until: crate::diting_browser::lifecycle::WaitUntil,
        method: &str,
        body: &str,
        initial_referrer: &str,
    ) -> Result<(), PageError> {
        // The initiating document's contribution to `document.referrer` on
        // the first hop. Empty for direct automation navigations; the page's
        // own pending navigation (process_pending_navigation) passes a
        // strict-origin-when-cross-origin value here. Subsequent hops of a
        // JS-triggered chain are stamped inside the inner loop.
        self.referrer = initial_referrer.to_string();
        // Hard ceiling on a single end-to-end navigation. Without this a slow
        // primary fetch or a runaway settle loop can hold the V8 lock for
        // arbitrarily long (we've measured 60+ seconds on JS-heavy news
        // sites), wedging every other in-flight CDP request because the
        // dispatcher holds the lock across the entire handler. 30 seconds
        // matches reqwest's default per-request timeout — the worst case is
        // one slow primary GET plus one slow JS-redirect chain step. Override
        // with `AGINXBROWSER_NAV_TIMEOUT_MS=NN`, or set a page-scoped deadline when
        // the automation request already has an explicit timeout.
        let nav_timeout = self.navigation_timeout();
        let nav_timeout_ms = nav_timeout.as_millis() as u64;

        let result = match tokio::time::timeout(
            nav_timeout,
            self.navigate_with_wait_post_inner(url_str, wait_until, method, body),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                self.lifecycle = crate::diting_browser::lifecycle::LifecycleState::Failed;
                Err(PageError::NetworkError(format!(
                    "navigation exceeded {nav_timeout_ms}ms deadline"
                )))
            }
        };
        if result.is_ok() {
            self.push_history(self.url_string());
        }
        result
    }

    /// Drive the JS event loop after navigation so deferred work can run:
    /// pending timers (setTimeout / setInterval), queued microtasks, in-flight
    /// fetches, and completion callbacks such as testharness's
    /// `add_completion_callback`. Returns as soon as the loop goes idle, or
    /// after `max_ms`. Without this the page is observed exactly as it stood at
    /// the load event, before any async work settles, which silently strands
    /// timer-driven tests and dynamic pages.
    pub async fn settle(&mut self, max_ms: u64) {
        if max_ms == 0 {
            return;
        }
        if let Some(js) = &mut self.js {
            // Bounded against both async idle and synchronous microtask storms:
            // a plain tokio timeout cannot preempt a page that pins the thread
            // inside V8 (the real-world SPA hang), so settle drives the loop
            // through the watchdog-guarded path.
            let _ = js.run_event_loop_bounded(max_ms).await;
        }
    }

    /// Drive the JS event loop until it goes idle, capped at `max_ms`. Use
    /// after interactions that kick off async work whose completion matters
    /// (client-side route transitions: the RSC fetch → flight parse → render →
    /// pushState chain). Returns `true` if the page quiesced within the
    /// budget; `false` means still busy (or capped by an interval timer).
    pub async fn settle_until_idle(&mut self, max_ms: u64) -> bool {
        if max_ms == 0 {
            return true;
        }
        if let Some(js) = &mut self.js {
            let fired_before = js.watchdog_fired_total();
            let idle = js.run_event_loop_until_idle(max_ms).await;
            if js.watchdog_fired_total() > fired_before {
                self.storm_backoff_ms = (self.storm_backoff_ms.max(200) * 2).min(5000);
                self.storm_hot_until = Some(
                    tokio::time::Instant::now()
                        + tokio::time::Duration::from_millis(self.storm_backoff_ms),
                );
            } else if idle {
                self.storm_backoff_ms = 0;
                self.storm_hot_until = None;
            }
            return idle;
        }
        true
    }

    /// One background event-loop slice for the idle session loop: pump the
    /// JS event loop for up to `ms`; once the loop goes quiescent, park
    /// until the slice deadline. A real browser's main thread never stops
    /// between user actions - timers, fetch callbacks and promise chains
    /// keep firing. Our sessions previously froze the loop between commands
    /// (blocking `recv()`), which stalled collectors with their own
    /// deadlines: WorkOS Radar's 5s worker-response window expired while
    /// its 5s timer sat un-pumped (measured 31s frozen). Cancellation-safe
    /// at slice boundaries - the caller drops this future via `select!`
    /// when a command arrives, same as the `settle_until_idle` timeout path.
    pub async fn pump_event_loop_slice(&mut self, ms: u64) {
        if ms == 0 {
            return;
        }
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(ms);
        if let Some(hot_until) = self.storm_hot_until {
            if hot_until > tokio::time::Instant::now() {
                // Storming page (watchdog-terminated earlier): park instead
                // of re-feeding the runaway loop. The session command loop
                // races this park against command arrival via select!.
                tokio::time::sleep_until(std::cmp::min(hot_until, deadline)).await;
                return;
            }
        }
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return;
            }
            let remaining = (deadline - now).as_millis() as u64;
            let idle = self.settle_until_idle(remaining).await;
            if idle {
                // Quiescent: park for the rest of the slice. The session
                // command loop races this against command arrival, so a
                // new command preempts the park immediately.
                tokio::time::sleep_until(deadline).await;
                return;
            }
        }
    }

    /// Append the current URL to the history stack, truncating any forward
    /// entries past the cursor (matches real Chrome: navigating after a
    /// goBack clobbers the forward history).
    pub fn push_history(&mut self, url: String) {
        if url.is_empty() { return; }
        // Don't dupe consecutive entries (Page.reload would otherwise pile up).
        if self.history.get(self.history_index) == Some(&url) {
            return;
        }
        if !self.history.is_empty() && self.history_index < self.history.len() - 1 {
            self.history.truncate(self.history_index + 1);
        }
        self.history.push(url);
        self.history_index = self.history.len() - 1;
    }

    /// Move the history cursor without re-navigating; used by
    /// Page.navigateToHistoryEntry which then drives the actual fetch.
    #[cfg_attr(not(test), allow(dead_code))] // exercised by the history tests below
    pub fn set_history_index(&mut self, idx: usize) {
        if idx < self.history.len() {
            self.history_index = idx;
        }
    }

    async fn navigate_with_wait_post_inner(
        &mut self,
        url_str: &str,
        wait_until: crate::diting_browser::lifecycle::WaitUntil,
        method: &str,
        body: &str,
    ) -> Result<(), PageError> {
        let mut current_url = url_str.to_string();
        let mut current_method = method.to_string();
        let mut current_body = body.to_string();
        // This cap counts documents in a JS-initiated navigation chain
        // (location/form hops), not HTTP 3xx redirects — those are budgeted
        // separately (20) by the net client. The low default is right: it is
        // what stops a page that resets `location` on every load. But a
        // legitimate long chain (SSO handover across providers) must be
        // raisable by the operator — env knob in the shape of
        // AGINXBROWSER_NAV_TIMEOUT_MS (obscura#664).
        let chain_limit = std::env::var("AGINXBROWSER_NAV_CHAIN_LIMIT")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(10);
        for chain in 0..chain_limit {
            self.navigate_single(&current_url, wait_until, &current_method, &current_body).await?;
            if let Some((next_url, next_method, next_body)) = self.take_pending_navigation() {
                if cross_scheme_to_file(&current_url, &next_url) {
                    // SOP gate. A web page must not be able to drive
                    // a navigation to file:// and then read the loaded
                    // document. Without this an http(s) page sets
                    // window.onload, calls location.href = "file:..."
                    // and harvests document.body from a local file
                    // once the new document loads.
                    tracing::warn!(
                        "blocking JS-initiated cross-scheme navigation to file: {} -> {}",
                        current_url,
                        next_url,
                    );
                    break;
                }
                tracing::info!("JS-triggered navigation chain: {} {} -> {}", current_method, current_url, next_url);
                // The chain step is document-initiated: the new document sees
                // the strict-origin-when-cross-origin referrer of this one.
                self.referrer = Url::parse(&current_url)
                    .ok()
                    .and_then(|src| Url::parse(&next_url).ok().map(|dst| navigation_referrer(&src, &dst)))
                    .unwrap_or_default();
                current_url = next_url;
                current_method = next_method;
                current_body = next_body;
                if chain + 1 == chain_limit {
                    // Hit the cap and the page still wants to keep
                    // chaining. Surface that as an error instead of
                    // returning Ok(()) so callers can distinguish a
                    // successful load from a redirect storm.
                    return Err(PageError::TooManyClientNavigations(chain_limit));
                }
                continue;
            }
            break;
        }
        Ok(())
    }

    async fn navigate_single(
        &mut self,
        url_str: &str,
        wait_until: crate::diting_browser::lifecycle::WaitUntil,
        method: &str,
        body: &str,
    ) -> Result<(), PageError> {
        let url = Url::parse(url_str).map_err(|e| PageError::InvalidUrl(e.to_string()))?;

        self.lifecycle = LifecycleState::Loading;
        self.url = Some(url.clone());
        self.network_events.clear();

        if url.scheme() == "about" {
            self.navigate_blank();
            self.init_js();
            // Preloads (Page.addScriptToEvaluateOnNewDocument, the
            // Runtime.addBinding shim) must run on about:blank too —
            // puppeteer's `browser.newPage()` lands on about:blank and
            // a follow-up `exposeFunction` is unusable otherwise.
            let preload_sources = self.preload_scripts.clone();
            if let Some(js) = &mut self.js {
                for source in &preload_sources {
                    if let Err(e) = js.execute_script_guarded("<preload>", source.as_str()) {
                        tracing::debug!("Preload script error on about:blank: {}", e);
                    }
                }
            }
            return Ok(());
        }

        let response = if url.scheme() == "data" {
            let content_type = url_str.strip_prefix("data:")
                .and_then(|s| s.split(',').next())
                .unwrap_or("text/html")
                .split(';').next()
                .unwrap_or("text/html")
                .to_string();
            let body_bytes = decode_data_uri(url_str).unwrap_or_default();
            let mut headers = std::collections::HashMap::new();
            headers.insert("content-type".to_string(), content_type);
            Ok(crate::diting_net::Response { url: url.clone(), status: 200, headers, body: body_bytes, redirected_from: Vec::new() })
        } else if method == "POST" {
            self.http_client
                .post_form_with_callbacks(&url, body, Some(&self.callbacks), crate::diting_net::ResourceType::Document)
                .await
        } else {
            self.fetch_document(&url).await
        }.map_err(|e| {
            self.lifecycle = LifecycleState::Failed;
            PageError::NetworkError(e.to_string())
        })?;

        self.record_network_event_with_body(
            url.as_str(),
            method,
            "Document",
            response.status,
            &response.headers,
            &response.body,
        );

        if !response.redirected_from.is_empty() {
            self.url = Some(response.url.clone());
        }

        // Honor the response charset: HTTP Content-Type → <meta charset> sniff
        // in the first 1KB → UTF-8 fallback. Without this, every non-UTF-8
        // page (GBK, Big5, Shift-JIS, Windows-125x, EUC-KR, ISO-8859-x)
        // came through as replacement characters.
        let (body_text, encoding_name) =
            crate::diting_net::decode_response_with_name(&response.body, response.content_type());
        self.encoding = encoding_name.to_string();
        let dom = parse_html(&body_text);

        self.title = dom
            .query_selector("title")
            .ok()
            .flatten()
            .map(|title_id| dom.text_content(title_id))
            .unwrap_or_default();

        let stylesheet_urls: Vec<String> = dom
            .query_selector_all("link")
            .unwrap_or_default()
            .iter()
            .filter_map(|&nid| {
                let node = dom.get_node(nid)?;
                let rel = node.get_attribute("rel")?;
                if rel.to_lowercase() != "stylesheet" {
                    return None;
                }
                node.get_attribute("href").map(|s| s.to_string())
            })
            .collect();

        let mut css_fetch_urls: Vec<String> = Vec::new();
        for href in &stylesheet_urls {
            let full_url = if href.starts_with("http://") || href.starts_with("https://") {
                href.clone()
            } else if let Some(base) = &self.url {
                base.join(href).map(|u| u.to_string()).unwrap_or_else(|_| href.clone())
            } else {
                href.clone()
            };
            if !subresource_allowed(self.url.as_ref(), &full_url) {
                tracing::warn!(
                    "blocking cross-scheme <link rel=stylesheet href>: page={} href={}",
                    self.url_string(),
                    full_url,
                );
                continue;
            }
            if self.should_block_url(&full_url) {
                tracing::info!("Blocked stylesheet by interception: {}", full_url);
                continue;
            }
            css_fetch_urls.push(full_url);
        }

        let client = self.http_client.clone();
        let css_callbacks = self.callbacks.clone();
        let css_futures: Vec<_> = css_fetch_urls.iter().map(|full_url| {
            let client = client.clone();
            let css_callbacks = css_callbacks.clone();
            let url_str = full_url.clone();
            async move {
                let parsed = Url::parse(&url_str).unwrap_or_else(|_| Url::parse("about:blank").unwrap());
                match client
                    .fetch_with_callbacks(&parsed, Some(css_callbacks.as_ref()), crate::diting_net::ResourceType::Stylesheet)
                    .await
                {
                    Ok(resp) => Some((url_str, resp)),
                    Err(e) => {
                        tracing::debug!("Failed to fetch stylesheet {}: {}", url_str, e);
                        None
                    }
                }
            }
        }).collect();

        // Same concurrency cap as script fetches.
        use futures::StreamExt as _;
        let css_results: Vec<_> = futures::stream::iter(css_futures)
            .buffer_unordered(16)
            .collect()
            .await;
        let mut css_sources = Vec::new();
        for result in css_results {
            if let Some((url_str, resp)) = result {
                // CSS bodies: honor the Content-Type charset; CSS @charset is
                // out of scope for the current scrape-focused pipeline.
                let css = crate::diting_net::decode_non_html(&resp.body, resp.content_type());
                self.record_network_event_with_body(&url_str, "GET", "Stylesheet", resp.status, &resp.headers, &resp.body);
                css_sources.push(css);
            }
        }

        self.dom = Some(dom);
        self.init_js();

        // Inject CSS as a global so getComputedStyle and any CSS-aware shim
        // can read it. Has to happen before scripts run, regardless of
        // waitUntil, so handlers that read window.__diting_css see it.
        if !css_sources.is_empty() {
            if let Some(js) = &mut self.js {
                let combined_css = css_sources.join("\n");
                // Use the thorough template-literal escape that
                // covers U+2028 / U+2029 and other control chars.
                // The previous escaper only handled `, \, and ${,
                // letting attacker-controlled CSS containing a raw
                // U+2028 break out of the template literal and run
                // arbitrary JS in the page's V8 realm.
                let escaped = escape_for_js_template_literal(&combined_css);
                let code = format!("globalThis.__diting_css = `{}`;", escaped);
                let _ = js.execute_script("<css>", &code);
            }
        }
        if let Some(js) = &mut self.js {
            let _ = js.execute_script("<iframe-load>",
                "(function() { var iframes = document.querySelectorAll('iframe[src]'); for (var i = 0; i < iframes.length; i++) { var src = iframes[i].getAttribute('src'); if (src && src !== 'about:blank') iframes[i]._loadIframeSrc(src); } })()");
        }

        // Spec: DOMContentLoaded fires AFTER parser-blocking scripts run,
        // not before. Skipping execute_scripts() on the DCL path meant
        // every inline <script> in the page was silently dropped: form
        // listeners never registered, frameworks never bootstrapped,
        // page.click() handlers were no-ops. Now scripts run regardless
        // of waitUntil and DCL means "DOM parsed AND scripts executed".
        self.execute_scripts().await;

        self.lifecycle = LifecycleState::DomContentLoaded;

        if wait_until == crate::diting_browser::lifecycle::WaitUntil::DomContentLoaded {
            return Ok(());
        }

        if let Some(js) = &mut self.js {
            if let Ok(new_title) = js.evaluate("document.title") {
                if let Some(t) = new_title.as_str() {
                    self.title = t.to_string();
                }
            }
        }

        self.lifecycle = LifecycleState::Loaded;

        if matches!(
            wait_until,
            crate::diting_browser::lifecycle::WaitUntil::NetworkIdle0 | crate::diting_browser::lifecycle::WaitUntil::NetworkIdle2
        ) {
            let threshold = match wait_until {
                crate::diting_browser::lifecycle::WaitUntil::NetworkIdle0 => 0,
                crate::diting_browser::lifecycle::WaitUntil::NetworkIdle2 => 2,
                _ => 0,
            };

            // Same hazard as the post-script settle: a synchronous poll can pin
            // the thread past the 5s network-idle deadline, so arm a watchdog
            // that terminates the isolate ~500ms past it.
            let netidle_wd = self
                .js
                .as_mut()
                .map(|js| js.arm_watchdog(std::time::Duration::from_millis(5500)));
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
            let mut idle_since: Option<tokio::time::Instant> = None;

            loop {
                let active = self.http_client.active_requests();
                let now = tokio::time::Instant::now();

                if active <= threshold {
                    if idle_since.is_none() {
                        idle_since = Some(now);
                    }
                    if now.duration_since(idle_since.unwrap()) >= tokio::time::Duration::from_millis(500) {
                        break;
                    }
                } else {
                    idle_since = None;
                }

                if now >= deadline {
                    tracing::debug!("Network idle timeout reached with {} active requests", active);
                    break;
                }

                if let Some(js) = &mut self.js {
                    let _ = tokio::time::timeout(
                        tokio::time::Duration::from_millis(50),
                        js.run_event_loop(),
                    ).await;
                } else {
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }

            if let Some(token) = netidle_wd {
                if let Some(js) = self.js.as_mut() {
                    js.disarm_watchdog(token);
                }
            }
            self.lifecycle = LifecycleState::NetworkIdle;
        }

        Ok(())
    }

    pub fn navigate_blank(&mut self) {
        self.snapshot_session_storage();
        self.js = None;
        self.url = Some(Url::parse("about:blank").unwrap());
        self.dom = Some(parse_html("<!DOCTYPE html><html><head></head><body></body></html>"));
        self.title = String::new();
        self.lifecycle = LifecycleState::Loaded;
    }

    pub fn url_string(&self) -> String {
        self.url
            .as_ref()
            .map(|u| u.to_string())
            .unwrap_or_else(|| "about:blank".to_string())
    }

    #[cfg_attr(not(test), allow(dead_code))] // snapshot helper exercised by tests; DOM consumers read via evaluate
    pub fn with_dom<R>(&self, f: impl FnOnce(&DomTree) -> R) -> Option<R> {
        if let Some(js) = &self.js {
            return js.with_dom(f);
        }
        self.dom.as_ref().map(f)
    }

    #[allow(dead_code)] // CDP DOM.getFlattenedDocument parity — no CDP client to serve it yet
    pub fn dom(&self) -> Option<&DomTree> {
        self.dom.as_ref()
    }

    /// V8 isolate handle for this page's runtime, if it has been initialized.
    /// Lets the CDP dispatcher arm a per-command watchdog (which bounds any one
    /// command so a hung page cannot hold the process-wide V8 lock forever)
    /// without taking `&mut self`.
    #[allow(dead_code)] // CDP per-command-watchdog plumbing — the CDP server itself is not absorbed
    pub fn isolate_handle(&self) -> Option<crate::diting_js::runtime::IsolateHandle> {
        self.js.as_ref().map(|js| js.isolate_handle())
    }

    /// Clear a V8 termination left by a per-command watchdog so the next command
    /// on this page can run. No-op if the runtime is absent or not terminating.
    #[allow(dead_code)] // ditto — watchdog-clearing half of the CDP plumbing above
    pub fn cancel_v8_termination(&mut self) {
        if let Some(js) = self.js.as_mut() {
            js.cancel_termination();
        }
    }

    /// Like [`Self::evaluate`] but bounded by a V8 watchdog so a runaway
    /// expression cannot hang the process. A non-zero `timeout` of zero falls
    /// back to the unbounded path.
    pub fn evaluate_with_timeout(
        &mut self,
        expression: &str,
        timeout: std::time::Duration,
    ) -> serde_json::Value {
        if let Some(js) = &mut self.js {
            match js.evaluate_with_timeout(expression, timeout) {
                Ok(val) => val,
                Err(e) => {
                    let preview: String = expression.chars().take(80).collect();
                    tracing::debug!("JS eval error/timeout for '{}': {}", preview, e);
                    serde_json::Value::Null
                }
            }
        } else {
            self.evaluate(expression)
        }
    }

    pub fn evaluate(&mut self, expression: &str) -> serde_json::Value {
        if let Some(js) = &mut self.js {
            match js.evaluate(expression) {
                Ok(val) => val,
                Err(e) => {
                    let preview: String = expression.chars().take(80).collect();
                    tracing::debug!("JS eval error for '{}': {}", preview, e);
                    serde_json::Value::Null
                }
            }
        } else {
            match expression.trim() {
                "document.title" => serde_json::Value::String(self.title.clone()),
                "document.URL" | "document.location.href" | "window.location.href" => {
                    serde_json::Value::String(self.url_string())
                }
                _ => serde_json::Value::Null,
            }
        }
    }

    pub async fn evaluate_for_cdp(
        &mut self,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
    ) -> crate::diting_js::runtime::RemoteObjectInfo {
        if let Some(js) = &mut self.js {
            match js.evaluate_for_cdp(expression, return_by_value, await_promise).await {
                Ok(info) => info,
                Err(e) => {
                    // Bug #24 diagnosis aid: an erroring eval previously surfaced
                    // as a silent `null` at the HTTP layer (and only a debug log
                    // here), which is indistinguishable from a JS null return.
                    // warn! so a wedged/degraded runtime is visible in logs.
                    let preview: String = expression.chars().take(120).collect();
                    tracing::warn!("evaluate_for_cdp error for '{}': {}", preview, e);
                    crate::diting_js::runtime::RemoteObjectInfo {
                        js_type: "undefined".into(),
                        subtype: None,
                        class_name: String::new(),
                        description: String::new(),
                        object_id: None,
                        value: None,
                    }
                }
            }
        } else {
            let val = self.evaluate(expression);
            crate::diting_js::runtime::RemoteObjectInfo {
                js_type: match &val {
                    serde_json::Value::String(_) => "string".into(),
                    serde_json::Value::Number(_) => "number".into(),
                    serde_json::Value::Bool(_) => "boolean".into(),
                    _ => "undefined".into(),
                },
                subtype: None,
                class_name: String::new(),
                description: String::new(),
                object_id: None,
                value: Some(val),
            }
        }
    }

    #[allow(dead_code)] // CDP Runtime.callFunctionOn parity; our eval path goes through evaluate_for_cdp
    pub async fn call_function_on_for_cdp(
        &mut self,
        function_declaration: &str,
        object_id: Option<&str>,
        args: &[serde_json::Value],
        return_by_value: bool,
        await_promise: bool,
    ) -> crate::diting_js::runtime::RemoteObjectInfo {
        if let Some(js) = &mut self.js {
            match js.call_function_on_for_cdp(function_declaration, object_id, args, return_by_value, await_promise).await {
                Ok(info) => info,
                Err(e) => {
                    tracing::debug!("callFunctionOn error: {}", e);
                    crate::diting_js::runtime::RemoteObjectInfo {
                        js_type: "undefined".into(),
                        subtype: None,
                        class_name: String::new(),
                        description: String::new(),
                        object_id: None,
                        value: None,
                    }
                }
            }
        } else {
            crate::diting_js::runtime::RemoteObjectInfo {
                js_type: "undefined".into(),
                subtype: None,
                class_name: String::new(),
                description: String::new(),
                object_id: None,
                value: None,
            }
        }
    }

    /// Exception-preserving variant of [`evaluate_for_cdp`]: a thrown/rejected
    /// expression comes back as an `EvalOutcome` exception so the CDP layer can
    /// emit `Runtime.exceptionThrown` + `exceptionDetails` instead of collapsing
    /// the throw to `undefined`.
    pub async fn evaluate_for_cdp_outcome(
        &mut self,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
    ) -> crate::diting_js::runtime::EvalOutcome {
        if let Some(js) = &mut self.js {
            match js
                .evaluate_for_cdp_outcome(expression, return_by_value, await_promise)
                .await
            {
                Ok(outcome) => outcome,
                Err(e) => {
                    let preview: String = expression.chars().take(120).collect();
                    tracing::warn!("evaluate_for_cdp error for '{}': {}", preview, e);
                    crate::diting_js::runtime::EvalOutcome {
                        info: crate::diting_js::runtime::RemoteObjectInfo {
                            js_type: "undefined".into(),
                            subtype: None,
                            class_name: String::new(),
                            description: String::new(),
                            object_id: None,
                            value: None,
                        },
                        exception: None,
                    }
                }
            }
        } else {
            let val = self.evaluate(expression);
            crate::diting_js::runtime::EvalOutcome {
                info: crate::diting_js::runtime::RemoteObjectInfo {
                    js_type: match &val {
                        serde_json::Value::String(_) => "string".into(),
                        serde_json::Value::Number(_) => "number".into(),
                        serde_json::Value::Bool(_) => "boolean".into(),
                        _ => "undefined".into(),
                    },
                    subtype: None,
                    class_name: String::new(),
                    description: String::new(),
                    object_id: None,
                    value: Some(val),
                },
                exception: None,
            }
        }
    }

    /// Exception-preserving variant of [`call_function_on_for_cdp`].
    pub async fn call_function_on_for_cdp_outcome(
        &mut self,
        function_declaration: &str,
        object_id: Option<&str>,
        args: &[serde_json::Value],
        return_by_value: bool,
        await_promise: bool,
    ) -> crate::diting_js::runtime::EvalOutcome {
        if let Some(js) = &mut self.js {
            match js
                .call_function_on_for_cdp_outcome(
                    function_declaration,
                    object_id,
                    args,
                    return_by_value,
                    await_promise,
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(e) => {
                    tracing::debug!("callFunctionOn error: {}", e);
                    crate::diting_js::runtime::EvalOutcome {
                        info: crate::diting_js::runtime::RemoteObjectInfo {
                            js_type: "undefined".into(),
                            subtype: None,
                            class_name: String::new(),
                            description: String::new(),
                            object_id: None,
                            value: None,
                        },
                        exception: None,
                    }
                }
            }
        } else {
            crate::diting_js::runtime::EvalOutcome {
                info: crate::diting_js::runtime::RemoteObjectInfo {
                    js_type: "undefined".into(),
                    subtype: None,
                    class_name: String::new(),
                    description: String::new(),
                    object_id: None,
                    value: None,
                },
                exception: None,
            }
        }
    }

    #[allow(dead_code)] // CDP Network.setBlockedURLs parity — no CDP client to call it yet
    pub fn set_blocked_urls(&mut self, patterns: Vec<String>) {
        if let Some(js) = &self.js {
            js.set_blocked_urls(patterns);
        }
    }

    #[allow(dead_code)] // CDP Runtime.releaseObject parity — object-store ids are never handed out
    pub fn release_object(&mut self, object_id: &str) {
        if let Some(js) = &mut self.js {
            js.release_object(object_id);
        }
    }

    fn record_network_event(
        &mut self,
        url: &str,
        method: &str,
        resource_type: &str,
        status: u16,
        response_headers: &std::collections::HashMap<String, String>,
        body_size: usize,
    ) -> String {
        self.network_event_counter += 1;
        let request_id = format!("{}.{}", self.id, self.network_event_counter);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        self.network_events.push(NetworkEvent {
            request_id: request_id.clone(),
            url: url.to_string(),
            method: method.to_string(),
            resource_type: resource_type.to_string(),
            status,
            headers: std::collections::HashMap::new(),
            response_headers: Arc::new(response_headers.clone()),
            body_size,
            timestamp,
        });
        request_id
    }

    /// Record the event and retain the body for `get_response_body`. Text
    /// Content-Types store lossy-UTF-8; anything else stores base64 so
    /// `take_response_body_raw` is byte-exact.
    fn record_network_event_with_body(
        &mut self,
        url: &str,
        method: &str,
        resource_type: &str,
        status: u16,
        response_headers: &std::collections::HashMap<String, String>,
        body: &[u8],
    ) -> String {
        let base64_encoded =
            !is_text_like_content_type(response_headers.get("content-type").map(|s| s.as_str()));
        let request_id = self.record_network_event(
            url,
            method,
            resource_type,
            status,
            response_headers,
            body.len(),
        );
        self.store_response_body(request_id.clone(), body, base64_encoded);
        request_id
    }

    fn store_response_body(&mut self, request_id: String, body: &[u8], base64_encoded: bool) {
        let max_entries = response_body_entry_limit();
        let max_bytes = response_body_byte_limit();
        if max_entries == 0 || max_bytes == 0 || body.len() > max_bytes {
            return;
        }
        let body = if base64_encoded {
            BASE64.encode(body)
        } else {
            String::from_utf8_lossy(body).to_string()
        };
        self.response_bodies.insert(
            request_id.clone(),
            StoredResponseBody {
                body,
                base64_encoded,
            },
        );
        self.response_body_order.push_back(request_id);
        while self.response_body_order.len() > max_entries {
            if let Some(oldest) = self.response_body_order.pop_front() {
                self.response_bodies.remove(&oldest);
            }
        }
    }

    /// Body stored for a request id: page-side (`{page}.{N}`) or
    /// script-initiated (`fetch-{N}`, retained in the JS runtime).
    #[cfg_attr(not(test), allow(dead_code))] // batch-2 kernel; /network endpoint is the pending consumer
    pub fn get_response_body(&self, request_id: &str) -> Option<StoredResponseBody> {
        self.response_bodies.get(request_id).cloned().or_else(|| {
            self.js
                .as_ref()?
                .get_network_response_body(request_id)
                .map(|body| StoredResponseBody {
                    body: body.body,
                    base64_encoded: body.base64_encoded,
                })
        })
    }

    /// Take a stored response body as raw bytes for CDP streaming
    /// (Fetch.takeResponseBodyAsStream). Removes it from the page-side cache
    /// and transfers ownership to the caller, so a large body is held once
    /// and freed when the stream is closed rather than lingering in this
    /// long-running process (upstream #360). Binary bodies are stored base64
    /// (byte-exact); text bodies return their UTF-8 bytes. Returns None if
    /// the body was never cached (e.g. it exceeded
    /// AGINXBROWSER_NETWORK_BODY_BUFFER_BYTES) or the id is unknown.
    #[cfg_attr(not(test), allow(dead_code))] // batch-2 kernel; CDP stream-take consumer pending
    pub fn take_response_body_raw(&mut self, request_id: &str) -> Option<Vec<u8>> {
        let stored = if let Some(body) = self.response_bodies.remove(request_id) {
            self.response_body_order.retain(|id| id != request_id);
            body
        } else {
            self.js
                .as_ref()?
                .get_network_response_body(request_id)
                .map(|b| StoredResponseBody {
                    body: b.body,
                    base64_encoded: b.base64_encoded,
                })?
        };
        if stored.base64_encoded {
            BASE64.decode(stored.body.as_bytes()).ok()
        } else {
            Some(stored.body.into_bytes())
        }
    }

    /// Make the body stored under `from_id` also retrievable under `to_id`.
    /// The main navigation resource is stored under its internal request id,
    /// but the CDP layer reports it with the navigation's loaderId as the
    /// requestId (Chrome's `requestId === loaderId` convention). Without this
    /// alias, `Network.getResponseBody(loaderId)` misses (upstream #340).
    #[cfg_attr(not(test), allow(dead_code))] // batch-2 kernel; pairs with get_response_body
    pub fn alias_response_body(&mut self, from_id: &str, to_id: &str) {
        if from_id == to_id || self.response_bodies.contains_key(to_id) {
            return;
        }
        if let Some(body) = self.response_bodies.get(from_id).cloned() {
            self.response_bodies.insert(to_id.to_string(), body);
            self.response_body_order.push_back(to_id.to_string());
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // batch-2 kernel; frees the LRU caches between sessions
    pub fn clear_response_bodies(&mut self) {
        self.response_bodies.clear();
        self.response_body_order.clear();
        if let Some(js) = &self.js {
            js.clear_network_response_bodies();
        }
    }

    /// Move network events recorded for script-initiated requests
    /// (fetch/XHR) from the JS runtime into this page's `network_events`, so
    /// the CDP layer emits Network.requestWillBeSent / responseReceived for
    /// them (upstream #406). Idempotent: the runtime's queue is drained. The
    /// `fetch-{N}` request id is preserved so get_response_body resolves.
    #[cfg_attr(not(test), allow(dead_code))] // batch-2 kernel; the /network consumer will call this itself
    pub fn sync_js_network_events(&mut self) {
        let events = match self.js.as_ref() {
            Some(js) => js.take_js_network_events(),
            None => return,
        };
        for ev in events {
            self.network_events.push(NetworkEvent {
                request_id: ev.request_id,
                url: ev.url,
                method: ev.method,
                resource_type: "Fetch".to_string(),
                status: ev.status,
                headers: std::collections::HashMap::new(),
                response_headers: Arc::new(ev.response_headers),
                body_size: ev.body_size,
                timestamp: ev.timestamp,
            });
        }
    }

    /// Register a passive callback fired for every request this page's
    /// fetches (document, subresources) and its JS fetch()/XHR make, once the
    /// method/headers are known and before it is sent. Non-blocking; use
    /// `enable_interception` to mutate or block. Returns a stable id; pass it
    /// to `off_request` to detach (upstream #408). Scoped to this page: it
    /// never sees sibling pages' requests and dies with the page.
    #[cfg_attr(not(test), allow(dead_code))] // batch-2 kernel; wire at session init when /network lands
    pub fn on_request(&mut self, cb: crate::diting_net::RequestCallback) -> u64 {
        self.callbacks.add_request(cb)
    }

    /// Register a passive callback fired with every response this page
    /// receives, including its body. Non-blocking. The main path for crawlers
    /// that need to capture API response payloads. Returns a stable id for
    /// `off_response`. Page-scoped like `on_request`.
    #[cfg_attr(not(test), allow(dead_code))] // batch-2 kernel; wire at session init when /network lands
    pub fn on_response(&mut self, cb: crate::diting_net::ResponseCallback) -> u64 {
        self.callbacks.add_response(cb)
    }

    /// Detach a request observer registered with `on_request`. Returns true
    /// if one was removed.
    #[cfg_attr(not(test), allow(dead_code))] // pair-unregister for on_request
    pub fn off_request(&mut self, id: u64) -> bool {
        self.callbacks.remove_request(id)
    }

    /// Detach a response observer registered with `on_response`.
    #[allow(dead_code)] // pair-unregister for on_response (tests unregister requests, not responses)
    pub fn off_response(&mut self, id: u64) -> bool {
        self.callbacks.remove_response(id)
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

    #[allow(dead_code)] // CDP Runtime.releaseObjectGroup parity
    pub fn release_object_group(&mut self) {
        if let Some(js) = &mut self.js {
            js.release_object_group();
        }
    }

    pub fn take_pending_navigation(&self) -> Option<(String, String, String)> {
        if let Some(js) = &self.js {
            js.take_pending_navigation()
        } else {
            None
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

    /// Drain queued console calls (level, message) for CDP `Runtime.consoleAPICalled`.
    pub fn take_pending_console_calls(&self) -> Vec<(String, String)> {
        if let Some(js) = &self.js {
            js.take_pending_console_calls()
        } else {
            Vec::new()
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
                        .map(|target| navigation_referrer(source, &target))
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

    #[allow(dead_code)] // Fetch-domain intercept kernel (requestPaused channel); no CDP client to answer pauses yet
    pub fn set_intercept_tx(&mut self, tx: tokio::sync::mpsc::UnboundedSender<crate::diting_js::ops::InterceptedRequest>) {
        self.intercept_tx = Some(tx.clone());
        if let Some(js) = &self.js {
            js.set_intercept_tx(tx);
        }
    }

    #[allow(dead_code)] // toggles the kernel above; kept separate so fetch() never auto-pauses
    pub fn enable_intercept(&mut self, enabled: bool) {
        self.intercept_enabled = enabled;
        if let Some(js) = &self.js {
            js.set_intercept_enabled(enabled);
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


#[cfg(test)]
mod tests {
    use super::*;

    /// Holds PRIVATE_NET_ENV_LOCK for the test's duration and clears
    /// AGINXBROWSER_ALLOW_PRIVATE_NETWORK on exit so the ambient env never leaks
    /// into the next test (several diting_net tests assert on the unset
    /// state). The field is the point: holding the guard is what locks.
    #[allow(dead_code)]
    struct NetGuard(std::sync::MutexGuard<'static, ()>);
    impl Drop for NetGuard {
        fn drop(&mut self) {
            std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        }
    }
    fn net_test_guard() -> NetGuard {
        let guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        NetGuard(guard)
    }

    /// Multi-path local HTTP server on 127.0.0.1. Bodies are owned Strings so
    /// a route can embed the port of another server (cross-origin tests).
    /// Answers up to 64 requests: one navigation may pull the document plus
    /// stylesheets and scripts. Unmatched paths get a 404.
    fn local_http_server(routes: Vec<(&'static str, u16, String)>) -> u16 {
        local_http_server_typed(
            routes
                .into_iter()
                .map(|(p, s, b)| (p, s, "text/html", b))
                .collect(),
        )
    }

    /// `local_http_server` with a per-route Content-Type (batch 2: response
    /// bodies store text lossy-UTF-8 vs binary base64, so tests need to
    /// control it).
    fn local_http_server_typed(routes: Vec<(&'static str, u16, &'static str, String)>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..64 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let path = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let (status, ctype, body) = routes
                    .iter()
                    .find(|(p, _, _, _)| *p == path)
                    .map(|(_, s, c, b)| (*s, *c, b.clone()))
                    .unwrap_or((404, "text/html", String::new()));
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    fn test_page() -> Page {
        let context = Arc::new(BrowserContext::with_storage_and_network(
            "test".into(),
            None,
            false,
            None,
            None,
            true, // allow_private_network: tests talk to 127.0.0.1
            None,
        ));
        Page::new("page-test".into(), context)
    }

    // ---- pure functions -------------------------------------------------

    #[test]
    fn subresource_allowed_policy_matrix() {
        let http_page = Url::parse("http://example.com/page").ok();
        let file_page = Url::parse("file:///tmp/x.html").ok();
        assert!(subresource_allowed(http_page.as_ref(), "https://cdn.example.com/a.js"));
        assert!(subresource_allowed(http_page.as_ref(), "data:text/javascript,1"));
        assert!(!subresource_allowed(http_page.as_ref(), "file:///etc/passwd"));
        assert!(subresource_allowed(file_page.as_ref(), "file:///tmp/sibling.js"));
        assert!(!subresource_allowed(http_page.as_ref(), "javascript:alert(1)"));
        assert!(!subresource_allowed(http_page.as_ref(), "not a url"));
        // No page URL yet (pre-navigation): http(s) is still fine — there is
        // nothing origin-sensitive to protect yet — but file: stays blocked.
        assert!(subresource_allowed(None, "https://example.com/a.js"));
        assert!(!subresource_allowed(None, "file:///etc/passwd"));
    }

    #[test]
    fn cross_scheme_to_file_matrix() {
        assert!(cross_scheme_to_file("http://a.com/", "file:///etc/passwd"));
        assert!(cross_scheme_to_file("https://a.com/", "FILE:///etc/passwd"));
        assert!(!cross_scheme_to_file("file:///tmp/a", "file:///tmp/b"));
        assert!(!cross_scheme_to_file("http://a.com/", "https://b.com/"));
        // Unparseable source is treated as non-file: block.
        assert!(cross_scheme_to_file("::not-a-url::", "file:///etc/passwd"));
    }

    #[test]
    fn navigation_referrer_matrix() {
        let same = |a: &str, b: &str| {
            navigation_referrer(&Url::parse(a).unwrap(), &Url::parse(b).unwrap())
        };
        // Same-origin: full URL, fragment and credentials stripped.
        assert_eq!(
            same("http://example.com/a#frag", "http://example.com/b"),
            "http://example.com/a"
        );
        assert_eq!(
            same("http://user:pw@example.com/a", "http://example.com/b"),
            "http://example.com/a"
        );
        // Cross-origin: origin + '/' only.
        assert_eq!(
            same("http://example.com/a/b?c=1", "http://other.com/d"),
            "http://example.com/"
        );
        // Downgrade and non-HTTP schemes: nothing.
        assert_eq!(same("https://example.com/a", "http://example.com/b"), "");
        assert_eq!(same("file:///tmp/a", "http://example.com/b"), "");
    }

    #[test]
    fn decode_data_uri_variants() {
        assert_eq!(
            decode_data_uri("data:text/html,%3Cp%3Ehi%3C/p%3E"),
            Some(b"<p>hi</p>".to_vec())
        );
        assert_eq!(
            decode_data_uri("data:application/js;base64,d2luZG93LmE9MQ=="),
            Some(b"window.a=1".to_vec())
        );
        // Base64 with embedded whitespace is tolerated.
        assert_eq!(decode_data_uri("data:;base64,\n aGk="), Some(b"hi".to_vec()));
        assert_eq!(decode_data_uri("data:no-comma"), None);
        assert_eq!(decode_data_uri("http://example.com/"), None);
    }

    #[test]
    fn escape_for_js_template_literal_blocks_breakouts() {
        // Exact escaped forms: every breakout character becomes a backslash
        // escape, so no unescaped ` or ${ can terminate the literal early.
        assert_eq!(escape_for_js_template_literal("a`b${c}"), "a\\`b\\${c}");
        assert_eq!(escape_for_js_template_literal("x\u{2028}y\u{2029}"), "x\\u2028y\\u2029");
        // \n has no dedicated arm; it falls through the generic <0x20 branch.
        assert_eq!(escape_for_js_template_literal("\0\r\n"), "\\0\\r\\u000a");
        assert_eq!(escape_for_js_template_literal("plain"), "plain");
    }

    // ---- history --------------------------------------------------------

    #[test]
    fn push_history_dedupes_consecutive_and_truncates_forward() {
        let mut p = test_page();
        p.push_history("http://a/1".into());
        p.push_history("http://a/1".into()); // duplicate: ignored
        assert_eq!(p.history, vec!["http://a/1"]);
        p.push_history("http://a/2".into());
        assert_eq!(p.history_index, 1);
        p.set_history_index(0); // go back
        p.push_history("http://a/3".into()); // clobbers forward entry
        assert_eq!(p.history, vec!["http://a/1", "http://a/3"]);
        assert_eq!(p.history_index, 1);
        p.set_history_index(99); // out of bounds: no-op
        assert_eq!(p.history_index, 1);
    }

    // ---- no-network navigations ------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn about_blank_initializes_realm_and_runs_preload_scripts() {
        let mut p = test_page();
        p.set_preload_scripts(vec!["window.__pre = 'yes';".into()]);
        p.navigate("about:blank").await.unwrap();
        assert_eq!(p.lifecycle, LifecycleState::Loaded);
        assert_eq!(p.evaluate("window.__pre"), serde_json::json!("yes"));
        assert_eq!(p.url_string(), "about:blank");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn data_uri_document_executes_script_and_sets_title() {
        let mut p = test_page();
        p.navigate("data:text/html,%3Cscript%3Ewindow.__x%3D'ran'%3C/script%3E%3Ctitle%3ET%3C/title%3E")
            .await
            .unwrap();
        assert_eq!(p.evaluate("window.__x"), serde_json::json!("ran"));
        assert_eq!(p.title, "T");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn navigate_blank_resets_state() {
        let mut p = test_page();
        p.title = "stale".into();
        p.navigate_blank();
        assert_eq!(p.url_string(), "about:blank");
        assert_eq!(p.title, "");
        assert_eq!(p.lifecycle, LifecycleState::Loaded);
        assert!(p.js.is_none());
    }

    // ---- navigation chains over a local server ---------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn js_triggered_navigation_chain_lands_on_final_page() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            ("/a", 200, "<html><script>location.href = '/b';</script></html>".into()),
            ("/b", 200, "<html><head><title>B-Landed</title></head><body><script>window.__here = document.URL;</script></body></html>".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.url_string(), format!("http://127.0.0.1:{port}/b"));
        assert_eq!(p.title, "B-Landed");
        assert!(p.evaluate("window.__here").as_str().unwrap().ends_with("/b"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chain_hop_sets_same_origin_referrer() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            ("/a", 200, "<html><script>location.href = '/b';</script></html>".into()),
            ("/b", 200, "<html><script>window.__ref = document.referrer;</script></html>".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(
            p.evaluate("window.__ref"),
            serde_json::json!(format!("http://127.0.0.1:{port}/a"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cross_origin_chain_referrer_is_origin_only() {
        let _g = net_test_guard();
        // Different ports on 127.0.0.1 are different origins.
        let port_b = local_http_server(vec![(
            "/b",
            200,
            "<html><script>window.__ref = document.referrer;</script></html>".into(),
        )]);
        let port_a = local_http_server(vec![(
            "/a",
            200,
            format!(
                "<html><script>location.href = 'http://127.0.0.1:{port_b}/b';</script></html>"
            ),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port_a}/a")).await.unwrap();
        assert_eq!(
            p.evaluate("window.__ref"),
            serde_json::json!(format!("http://127.0.0.1:{port_a}/"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn direct_navigation_leaves_referrer_empty() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><script>window.__ref = document.referrer;</script></html>".into(),
        )]);
        let mut p = test_page();
        // An earlier navigation must not leak into the next one's referrer.
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.evaluate("window.__ref"), serde_json::json!(""));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sop_blocks_js_navigation_into_file_scheme() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><script>location.href = 'file:///etc/passwd';</script></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        // The chain loop breaks instead of navigating; we stay on /a.
        assert_eq!(p.url_string(), format!("http://127.0.0.1:{port}/a"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subresource_gate_blocks_file_script_src() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><body><script src=\"file:///tmp/evil.js\"></script><script>window.__pwned = 'inline-ran';</script></body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        // The inline script after the file: src still ran, but nothing could
        // have been loaded from file: — assert the page survived and no file
        // fetch shows up in the network log.
        assert_eq!(p.evaluate("window.__pwned"), serde_json::json!("inline-ran"));
        assert!(p.network_events.iter().all(|e| !e.url.starts_with("file:")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn js_chain_over_limit_reports_too_many_redirects() {
        let _g = net_test_guard();
        // Every /hopN page redirects to /hop{N+1}: an infinite JS chain that
        // must stop at the default chain limit (10 documents) with
        // TooManyClientNavigations — and the message must not blame HTTP
        // redirects (obscura#664: a message pointing at the wrong layer
        // costs more than none).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..64 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let path = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/hop0")
                    .to_string();
                let next = match path.strip_prefix("/hop").and_then(|n| n.parse::<usize>().ok()) {
                    Some(n) => format!("/hop{}", n + 1),
                    None => "/hop0".to_string(),
                };
                let body = format!("<html><script>location.href = '{next}';</script></html>");
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        let mut p = test_page();
        let err = p
            .navigate(&format!("http://127.0.0.1:{port}/hop0"))
            .await
            .unwrap_err();
        assert!(matches!(err, PageError::TooManyClientNavigations(10)), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("client navigation chain"), "message must name the layer: {msg}");
        assert!(
            !msg.starts_with("Too many redirects"),
            "message must not blame HTTP redirects — the server never 3xx'd: {msg}"
        );
        assert!(
            msg.contains("AGINXBROWSER_NAV_CHAIN_LIMIT"),
            "message must name the operator remedy: {msg}"
        );
    }

    /// obscura#664 class: the navigation-chain cap counts documents, not
    /// HTTP redirects, and an operator with a legitimate long chain (SSO
    /// handover across several providers) must be able to raise it. Env
    /// knob, in the shape of `AGINXBROWSER_NAV_TIMEOUT_MS`.
    #[tokio::test(flavor = "current_thread")]
    async fn nav_chain_limit_env_unblocks_long_chains() {
        let _g = net_test_guard();
        // Finite chain /hop0 → /hop1 → … → /hop10 (terminal document):
        // 11 documents total, one past the default cap of 10.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..16 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let path = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/hop0")
                    .to_string();
                let body = match path.strip_prefix("/hop").and_then(|n| n.parse::<usize>().ok()) {
                    Some(10) => "<html><body>done</body></html>".to_string(),
                    Some(n) => {
                        format!("<html><script>location.href = '/hop{}';</script></html>", n + 1)
                    }
                    None => "/hop0".to_string(),
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        std::env::set_var("AGINXBROWSER_NAV_CHAIN_LIMIT", "12");
        let mut p = test_page();
        let res = p.navigate(&format!("http://127.0.0.1:{port}/hop0")).await;
        std::env::remove_var("AGINXBROWSER_NAV_CHAIN_LIMIT");
        res.unwrap();
        assert_eq!(p.url_string(), format!("http://127.0.0.1:{port}/hop10"));
    }

    // ---- wait semantics & network events ---------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn subresources_recorded_as_network_events() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            ("/a", 200, "<html><head><link rel=\"stylesheet\" href=\"/s.css\"></head><body><script src=\"/x.js\"></script></body></html>".into()),
            ("/s.css", 200, "body{color:red}".into()),
            ("/x.js", 200, "window.__js = 'loaded';".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.evaluate("window.__js"), serde_json::json!("loaded"));
        let kinds: Vec<&str> = p.network_events.iter().map(|e| e.resource_type.as_str()).collect();
        assert!(kinds.contains(&"Document"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"Stylesheet"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"Script"), "kinds: {kinds:?}");
        // request_id is page-id scoped.
        assert!(p
            .network_events
            .iter()
            .all(|e| e.request_id.starts_with("page-test.")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn domcontentloaded_wait_returns_after_scripts_execute() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><head><title>DCL</title></head><body><script>window.__ran = 'yes';</script></body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate_with_wait(
            &format!("http://127.0.0.1:{port}/a"),
            crate::diting_browser::lifecycle::WaitUntil::DomContentLoaded,
        )
        .await
        .unwrap();
        // DCL means DOM parsed AND scripts executed.
        assert_eq!(p.evaluate("window.__ran"), serde_json::json!("yes"));
        assert_eq!(p.lifecycle, LifecycleState::DomContentLoaded);
    }

    // ---- body onload forwarding (byte-WAF challenge prerequisite) ----------

    #[tokio::test(flavor = "current_thread")]
    async fn body_onload_attribute_handler_runs_on_window_load() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            // `<body onload="...">` is a Window-level handler per HTML spec;
            // the load event fires on window, so the content attribute must
            // be forwarded there or the handler never runs.
            "<html><head></head><body onload=\"window.__bodyOnloadRan = 'yes'\">x</body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.evaluate("window.__bodyOnloadRan"), serde_json::json!("yes"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn window_load_paths_all_fire() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><head></head><body><script>\
             window.__viaListener = 'no';\
             window.addEventListener('load', function() { window.__viaListener = 'yes'; });\
             window.onload = function() { window.__viaProperty = 'yes'; };\
             </script></body></html>"
                .into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.evaluate("window.__viaListener"), serde_json::json!("yes"));
        assert_eq!(p.evaluate("window.__viaProperty"), serde_json::json!("yes"));
    }

    /// Byte-WAF JS challenge (juejin.cn class) auto-solves end to end:
    /// `<body onload="readygo()">` drives a setInterval PoW over the inline
    /// SHA-256 helpers, sets the `_wafchallengeid` answer cookie, and
    /// `location.reload()` re-requests with it fast enough to beat the
    /// Max-Age=1 window. The local server plays the WAF: no cookie ->
    /// challenge page, valid answer -> real page.
    #[tokio::test(flavor = "current_thread")]
    async fn byte_waf_js_challenge_autosolves() {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let _g = net_test_guard();

        let prefix: Vec<u8> = (0u8..32).collect();
        let secret = 7u64; // solution lives at i=7
        let expect: Vec<u8> = {
            let mut m = prefix.clone();
            m.extend_from_slice(secret.to_string().as_bytes());
            Sha256::digest(&m).to_vec()
        };
        let cs = serde_json::json!({
            "v": {
                "a": base64::engine::general_purpose::STANDARD.encode(&prefix),
                "b": 1787795373i64,
                "c": base64::engine::general_purpose::STANDARD.encode(&expect),
            },
            "s": base64::engine::general_purpose::STANDARD.encode([9u8; 32]),
        })
        .to_string();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut served_real = false;
        let mut challenge_hits = 0u32;
        let log = std::sync::Arc::new(std::sync::Mutex::new((served_real, challenge_hits)));
        let log2 = log.clone();
        let prefix2 = prefix.clone();
        let expect2 = expect.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut served_real, mut challenge_hits) = *log2.lock().unwrap();
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 16384];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let cookie_hdr = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("cookie:"))
                    .map(|l| l.to_string())
                    .unwrap_or_default();
                let passed = cookie_hdr
                    .split("_wafchallengeid=")
                    .nth(1)
                    .and_then(|v| v.split(';').next())
                    .map(str::trim)
                    .and_then(|v| {
                        base64::engine::general_purpose::STANDARD.decode(v).ok()
                    })
                    .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
                    .and_then(|c| {
                        c.get("d")
                            .and_then(|d| d.as_str())
                            .and_then(|d| {
                                base64::engine::general_purpose::STANDARD.decode(d).ok()
                            })
                            .map(|answer| {
                                let mut m = prefix2.clone();
                                m.extend_from_slice(&answer);
                                Sha256::digest(&m).to_vec() == expect2
                            })
                    })
                    .unwrap_or(false);
                let body = if passed {
                    served_real = true;
                    "<html><head><title>Real Page</title></head><body>unlocked</body></html>".to_string()
                } else {
                    challenge_hits += 1;
                    format!(
                        "<html><head><title>challenge</title></head><body onload=\"readygo()\">\
                         <script>window.WAFJS = function(){{}};</script>\
                         <script>{helpers}</script>\
                         <script>function readygo(){{var wci=\"_wafchallengeid\",cs=\"{cs}\",c=JSON.parse(atob(cs)),\
                         prefix=b64tou8a(c.v.a),expect=b64tohex(c.v.c),i=0,\
                         iid=setInterval(function(){{expect===s256(prefix,\"\"+i)&&\
                         (c.d=btoa(\"\"+i),clearInterval(iid),\
                         document.cookie=wci+\"=\"+btoa(JSON.stringify(c))+\"; Max-Age=1\",\
                         window.location.reload()),i++,i>1e6&&clearInterval(iid)}},1)}}</script>\
                         Please wait...</body></html>",
                        helpers = include_str!("../../tests/waf_sha256_helpers.js"),
                        cs = base64::engine::general_purpose::STANDARD.encode(cs.as_bytes()),
                    )
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
                *log2.lock().unwrap() = (served_real, challenge_hits);
            }
        });

        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/")).await.unwrap();
        // The challenge page must have auto-passed: readygo ran from the body
        // onload attribute, solved the PoW, and reloaded with the answer.
        assert_eq!(p.evaluate("document.title"), serde_json::json!("Real Page"));
        let (served_real, challenge_hits) = *log.lock().unwrap();
        assert!(served_real, "server never served the real page");
        assert!(challenge_hits >= 1, "challenge was never issued");
    }

    
    // ---- suspend / resume ---------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn suspend_resume_preserves_dom_and_rebuilds_realm() {
        let mut p = test_page();
        p.navigate("data:text/html,%3Cscript%3Ewindow.__mark%3D'old'%3C/script%3E%3Ctitle%3ECarry%3C/title%3E")
            .await
            .unwrap();
        assert_eq!(p.evaluate("window.__mark"), serde_json::json!("old"));
        p.suspend_js();
        assert!(!p.has_js());
        // DOM survives suspension and stays queryable.
        let title = p
            .with_dom(|dom| {
                dom.query_selector("title")
                    .ok()
                    .flatten()
                    .map(|nid| dom.text_content(nid))
            })
            .flatten();
        assert_eq!(title.as_deref(), Some("Carry"));
        // Static evaluate fallback with no runtime.
        assert_eq!(p.evaluate("document.title"), serde_json::json!("Carry"));
        assert_eq!(
            p.evaluate("window.location.href"),
            serde_json::json!(p.url_string())
        );
        assert_eq!(p.evaluate("1 + 1"), serde_json::Value::Null);
        // Resume rebuilds the realm: page state (window.__mark) is gone —
        // init_js never carries the old realm across.
        p.resume_js();
        assert!(p.has_js());
        assert_eq!(p.evaluate("window.__mark"), serde_json::Value::Null);
        assert_eq!(p.evaluate("document.title"), serde_json::json!("Carry"));
    }

    // ---- sessionStorage persistence (#678) ---------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn session_storage_survives_same_origin_navigation() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            ("/a", 200, "<html><title>A</title></html>".into()),
            ("/b", 200, "<html><title>B</title></html>".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a"))
            .await
            .unwrap();
        p.evaluate("sessionStorage.setItem('k', 'v1')");
        assert_eq!(
            p.evaluate("sessionStorage.getItem('k')"),
            serde_json::json!("v1")
        );
        // Same-origin navigation must preserve sessionStorage (reload-like).
        p.navigate(&format!("http://127.0.0.1:{port}/b"))
            .await
            .unwrap();
        assert_eq!(
            p.evaluate("sessionStorage.getItem('k')"),
            serde_json::json!("v1")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_storage_survives_suspend_resume() {
        let mut p = test_page();
        p.navigate("data:text/html,<html><title>S</title></html>")
            .await
            .unwrap();
        p.evaluate("sessionStorage.setItem('foo', 'bar')");
        assert_eq!(
            p.evaluate("sessionStorage.getItem('foo')"),
            serde_json::json!("bar")
        );
        // A second target's evaluate parks this realm (suspend_js) and later
        // resumes it via init_js; sessionStorage must survive the round-trip.
        p.suspend_js();
        p.resume_js();
        assert_eq!(
            p.evaluate("sessionStorage.getItem('foo')"),
            serde_json::json!("bar")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_storage_survives_data_url_navigation() {
        let mut p = test_page();
        p.navigate("data:text/html,<html><title>one</title></html>")
            .await
            .unwrap();
        p.evaluate("sessionStorage.setItem('ticket', 'abc-123')");
        assert_eq!(
            p.evaluate("sessionStorage.getItem('ticket')"),
            serde_json::json!("abc-123")
        );
        p.navigate("data:text/html,<html><title>two</title></html>")
            .await
            .unwrap();
        assert_eq!(
            p.evaluate("sessionStorage.getItem('ticket')"),
            serde_json::json!("abc-123"),
            "data: URL same-origin (opaque) navigation must preserve sessionStorage"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_storage_cleared_on_cross_origin_navigation() {
        let _g = net_test_guard();
        let port_a = local_http_server(vec![("/a", 200, "<html><title>A</title></html>".into())]);
        let port_b = local_http_server(vec![("/b", 200, "<html><title>B</title></html>".into())]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port_a}/a"))
            .await
            .unwrap();
        p.evaluate("sessionStorage.setItem('k', 'v1')");
        // Different port = different origin: the old store must be discarded,
        // matching per-tab-per-origin semantics.
        p.navigate(&format!("http://127.0.0.1:{port_b}/b"))
            .await
            .unwrap();
        assert_eq!(
            p.evaluate("sessionStorage.getItem('k')"),
            serde_json::Value::Null
        );
    }

    // ---- batch 1: fork_virtual_url + preload push + nav timeout -----------

    #[tokio::test(flavor = "current_thread")]
    async fn process_pending_navigation_adopts_spa_pushstate_route() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/app",
            200,
            "<html><body><script>window.__booted = 1;</script></body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app")).await.unwrap();
        // SPA click handler: renders in place, moves the URL via pushState.
        // (evaluate wraps single expressions only — `return (a; b)` would be
        // a SyntaxError — so the call and the probe run separately.)
        p.evaluate("history.pushState(null, '', '/app/settings')");
        assert_eq!(
            p.evaluate("globalThis.__virtualUrl"),
            serde_json::json!(format!("http://127.0.0.1:{port}/app/settings"))
        );
        // The session pump has no pending navigation to process, but the page
        // still routed itself — that counts (fork_virtual_url.rs).
        assert!(p.process_pending_navigation().await.unwrap());
        assert!(p.url_string().ends_with("/app/settings"));
        assert!(p.history.last().unwrap().ends_with("/app/settings"));
        // Idempotent: adopting the same virtual URL again changes nothing.
        assert!(!p.process_pending_navigation().await.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn process_pending_navigation_without_route_returns_false() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/plain",
            200,
            "<html><body>no scripts</body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/plain")).await.unwrap();
        assert!(!p.process_pending_navigation().await.unwrap());
        assert!(p.url_string().ends_with("/plain"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn process_pending_navigation_carries_hop1_referrer() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            (
                "/sender",
                200,
                "<html><body>plain document</body></html>".into(),
            ),
            (
                "/receiver",
                200,
                "<html><script>window.__ref = document.referrer;</script></html>".into(),
            ),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/sender")).await.unwrap();
        assert!(p.url_string().ends_with("/sender"));
        // Queue a navigation AFTER navigate() returned, the way a click
        // handler on a live page does — the inner chain is done, so only the
        // session pump's process_pending_navigation can pick it up.
        p.evaluate("setTimeout(() => { location.href = '/receiver'; }, 100)");
        let _ = p.settle_until_idle(3_000).await;
        assert!(p.process_pending_navigation().await.unwrap());
        assert!(p.url_string().ends_with("/receiver"));
        // The page asked for this navigation itself, so unlike direct
        // automation navigations the first hop carries a referrer.
        assert_eq!(
            p.evaluate("window.__ref"),
            serde_json::json!(format!("http://127.0.0.1:{port}/sender"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn add_preload_script_appends_in_order() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/target",
            200,
            "<html><script>window.__pageRan = (window.__order || '') + 'P';</script></html>".into(),
        )]);
        let mut p = test_page();
        p.add_preload_script(
            "window.__order = (window.__order || '') + '1';".into(),
        );
        p.add_preload_script(
            "window.__order = (window.__order || '') + '2';".into(),
        );
        p.navigate(&format!("http://127.0.0.1:{port}/target")).await.unwrap();
        // Push semantics: both scripts ran, in registration order, before the
        // page's own script.
        assert_eq!(p.evaluate("window.__pageRan"), serde_json::json!("12P"));
    }

    #[test]
    fn navigation_timeout_field_overrides_env_default() {
        let mut p = test_page();
        let env_or_default = std::env::var("AGINXBROWSER_NAV_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30_000);
        assert_eq!(p.navigation_timeout().as_millis() as u64, env_or_default);
        p.set_navigation_timeout(Some(1_500));
        assert_eq!(p.navigation_timeout().as_millis() as u64, 1_500);
        p.set_navigation_timeout(None);
        assert_eq!(p.navigation_timeout().as_millis() as u64, env_or_default);
    }

    // ---- batch 2: network callbacks + response bodies ----------------------

    /// Shared recorder for callback tests: on_request/on_response push into
    /// these from inside the registry's fire path.
    #[derive(Default)]
    struct NetLog {
        requests: std::sync::Mutex<Vec<(String, String)>>, // (resource_type, url)
        responses: std::sync::Mutex<Vec<(String, String, usize)>>, // (type, url, body len)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn on_request_and_on_response_fire_for_document_and_subresources() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            (
                "/page",
                200,
                format!(
                    "<html><head><link rel=stylesheet href='/s.css'></head>\
                     <script src='/j.js'></script></html>"
                ),
            ),
            ("/s.css", 200, "body{color:red}".into()),
            ("/j.js", 200, "window.__j = 1;".into()),
        ]);
        let log = Arc::new(NetLog::default());
        let mut p = test_page();
        {
            let log = log.clone();
            p.on_request(Arc::new(move |info| {
                log.requests
                    .lock()
                    .unwrap()
                    .push((info.resource_type.as_str().to_string(), info.url.to_string()));
            }));
        }
        {
            let log = log.clone();
            p.on_response(Arc::new(move |info, resp| {
                log.responses.lock().unwrap().push((
                    info.resource_type.as_str().to_string(),
                    info.url.to_string(),
                    resp.body.len(),
                ));
            }));
        }
        p.navigate(&format!("http://127.0.0.1:{port}/page")).await.unwrap();

        let reqs = log.requests.lock().unwrap();
        let kinds: Vec<&str> = reqs.iter().map(|(k, _)| k.as_str()).collect();
        assert!(kinds.contains(&"Document"), "requests: {reqs:?}");
        assert!(kinds.contains(&"Script"), "requests: {reqs:?}");
        assert!(kinds.contains(&"Stylesheet"), "requests: {reqs:?}");
        // The document observer saw the fully-built header set, not an empty
        // one: our client always sends User-Agent.
        let resps = log.responses.lock().unwrap();
        assert!(
            resps
                .iter()
                .any(|(k, _, len)| k == "Document" && *len > 0),
            "responses: {resps:?}"
        );
        assert!(
            resps.iter().any(|(k, _, _)| k == "Stylesheet"),
            "responses: {resps:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn off_request_detaches_observer() {
        let _g = net_test_guard();
        let port = local_http_server(vec![("/x", 200, "<html></html>".into())]);
        let log = Arc::new(NetLog::default());
        let mut p = test_page();
        let id = {
            let log = log.clone();
            p.on_request(Arc::new(move |info| {
                log.requests
                    .lock()
                    .unwrap()
                    .push((info.resource_type.as_str().to_string(), info.url.to_string()));
            }))
        };
        assert!(p.off_request(id));
        // Double detach is a visible no-op.
        assert!(!p.off_request(id));
        p.navigate(&format!("http://127.0.0.1:{port}/x")).await.unwrap();
        assert!(log.requests.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn document_body_retrievable_and_take_removes() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/doc",
            200,
            "<html><head><title>BodyStore</title></head><body>MARKER-42</body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/doc")).await.unwrap();
        let doc_event = p
            .network_events
            .iter()
            .find(|e| e.resource_type == "Document")
            .expect("document network event");
        let rid = doc_event.request_id.clone();
        let stored = p.get_response_body(&rid).expect("stored document body");
        assert!(!stored.base64_encoded);
        assert!(stored.body.contains("MARKER-42"), "{}", stored.body);
        // take_response_body_raw hands over the bytes and drops the entry.
        let raw = p.take_response_body_raw(&rid).expect("raw bytes");
        assert!(String::from_utf8_lossy(&raw).contains("MARKER-42"));
        assert!(p.get_response_body(&rid).is_none());
        assert!(p.take_response_body_raw(&rid).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn alias_response_body_renames_entry() {
        let _g = net_test_guard();
        let port = local_http_server(vec![("/d", 200, "<html>ALIAS-ME</html>".into())]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/d")).await.unwrap();
        let rid = p
            .network_events
            .iter()
            .find(|e| e.resource_type == "Document")
            .unwrap()
            .request_id
            .clone();
        // Chrome's requestId === loaderId convention (upstream #340).
        p.alias_response_body(&rid, "loader-1");
        assert!(p.get_response_body("loader-1").is_some());
        assert!(p.get_response_body(&rid).is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn binary_body_stored_base64_and_take_is_byte_exact() {
        let _g = net_test_guard();
        // Non-text Content-Type forces base64 storage; the expectation is the
        // exact wire bytes (a Rust String holds UTF-8, so chars >= 0x80 are
        // sent as their multi-byte encodings — take must return those, lossless).
        let bin: String = vec![0u8, 159, 146, 150, 255, 1, 2]
            .into_iter()
            .map(|b| b as char)
            .collect();
        let wire_bytes = bin.as_bytes().to_vec();
        let port = local_http_server_typed(vec![
            ("/bin", 200, "application/octet-stream", bin),
            (
                "/host",
                200,
                "text/html",
                "<html><script src='/bin'></script></html>".into(),
            ),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/host")).await.unwrap();
        let rid = p
            .network_events
            .iter()
            .find(|e| e.url.ends_with("/bin"))
            .expect("script event for binary body")
            .request_id
            .clone();
        let stored = p.get_response_body(&rid).expect("binary body stored");
        assert!(stored.base64_encoded, "octet-stream must store base64");
        let raw = p.take_response_body_raw(&rid).expect("raw bytes");
        assert_eq!(raw, wire_bytes);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn js_fetch_surfaces_as_network_event_with_body() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            (
                "/app",
                200,
                "<html><script>\
                 fetch('/api').then(r => r.text()).then(t => { window.__got = t; });\
                 </script></html>"
                    .into(),
            ),
            ("/api", 200, "{\"v\": 7}".into()),
        ]);
        let log = Arc::new(NetLog::default());
        let mut p = test_page();
        {
            let log = log.clone();
            p.on_response(Arc::new(move |info, resp| {
                log.responses.lock().unwrap().push((
                    info.resource_type.as_str().to_string(),
                    info.url.to_string(),
                    resp.body.len(),
                ));
            }));
        }
        p.navigate(&format!("http://127.0.0.1:{port}/app")).await.unwrap();
        assert_eq!(p.evaluate("window.__got"), serde_json::json!("{\"v\": 7}"));

        // Script-initiated traffic fires the page's observers too…
        let resps = log.responses.lock().unwrap();
        assert!(
            resps.iter().any(|(k, u, _)| k == "Fetch" && u.ends_with("/api")),
            "responses: {resps:?}"
        );
        drop(resps);

        // …and syncs into the page's network events with a fetch-{N} id whose
        // body resolves through the JS-side store.
        p.sync_js_network_events();
        let ev = p
            .network_events
            .iter()
            .find(|e| e.url.ends_with("/api"))
            .expect("fetch network event after sync");
        assert_eq!(ev.resource_type, "Fetch");
        assert!(ev.request_id.starts_with("fetch-"), "{}", ev.request_id);
        let stored = p.get_response_body(&ev.request_id).expect("fetch body");
        assert!(!stored.base64_encoded);
        assert_eq!(stored.body, "{\"v\": 7}");
        // Idempotent drain.
        p.sync_js_network_events();
        assert_eq!(
            p.network_events.iter().filter(|e| e.url.ends_with("/api")).count(),
            1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clear_response_bodies_drops_page_and_js_stores() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            (
                "/app",
                200,
                "<html><script>fetch('/a1').then(r => r.text());</script></html>".into(),
            ),
            ("/a1", 200, "one".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app")).await.unwrap();
        let doc_rid = p
            .network_events
            .iter()
            .find(|e| e.resource_type == "Document")
            .unwrap()
            .request_id
            .clone();
        assert!(p.get_response_body(&doc_rid).is_some());
        p.sync_js_network_events();
        let fetch_rid = p
            .network_events
            .iter()
            .find(|e| e.url.ends_with("/a1"))
            .unwrap()
            .request_id
            .clone();
        assert!(p.get_response_body(&fetch_rid).is_some());

        p.clear_response_bodies();
        assert!(p.get_response_body(&doc_rid).is_none());
        assert!(p.get_response_body(&fetch_rid).is_none());
    }

    // ---- import maps (upstream 34373c3) ----------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn parser_import_map_before_first_module_controls_resolution() {
        let _g = net_test_guard();
        let port = local_http_server_typed(vec![
            ("/app/index.html", 200, "text/html",
             r#"<html><head>
                <script type="importmap">{"imports":{"ordered":"./before.js"}}</script>
                <script type="module">
                    import { value } from "ordered";
                    globalThis.__parser_import_map_value = value;
                </script>
                <script type="importmap">{"imports":{"ordered":"./after.js"}}</script>
            </head><body></body></html>"#.into()),
            ("/app/before.js", 200, "application/javascript", "export const value = 'before-first-module';".into()),
            ("/app/after.js", 200, "application/javascript", "export const value = 'later-map';".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app/index.html")).await.unwrap();
        assert_eq!(
            p.evaluate("globalThis.__parser_import_map_value"),
            serde_json::json!("before-first-module")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn later_import_map_adds_unrelated_rule_without_rebinding_resolved_rule() {
        let _g = net_test_guard();
        let port = local_http_server_typed(vec![
            ("/app/index.html", 200, "text/html",
             r#"<html><head>
                <script type="importmap">{"imports":{"fixed":"./before.js"}}</script>
                <script type="module">
                    import { value } from "fixed";
                    globalThis.__first_map_value = value;
                </script>
                <script type="importmap">{"imports":{"fixed":"./after.js","later":"./later.js"}}</script>
                <script type="module">
                    import { value as fixed } from "fixed";
                    import { value as later } from "later";
                    globalThis.__later_map_values = [fixed, later];
                </script>
            </head><body></body></html>"#.into()),
            ("/app/before.js", 200, "application/javascript", "export const value = 'before-first-module';".into()),
            ("/app/later.js", 200, "application/javascript", "export const value = 'later-map';".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app/index.html")).await.unwrap();
        assert_eq!(
            p.evaluate("globalThis.__first_map_value"),
            serde_json::json!("before-first-module")
        );
        assert_eq!(
            p.evaluate("globalThis.__later_map_values"),
            serde_json::json!(["before-first-module", "later-map"])
        );
        // after.js must never have been fetched: the second map's rebind of
        // "fixed" was discarded because the first module already resolved it.
        let urls: Vec<&str> = p.network_events.iter().map(|e| e.url.as_str()).collect();
        assert!(!urls.iter().any(|u| u.ends_with("/after.js")), "urls: {urls:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dynamically_inserted_import_map_controls_later_dynamic_import() {
        let _g = net_test_guard();
        let port = local_http_server_typed(vec![
            ("/app/index.html", 200, "text/html",
             r#"<html><head></head><body>
                <script>
                    const map = document.createElement("script");
                    map.type = "importmap";
                    map.textContent = JSON.stringify({imports:{dynamicName:"./later.js"}});
                    document.head.appendChild(map);
                    import("dynamicName")
                        .then(module => globalThis.__dynamic_map_value = module.value)
                        .catch(error => globalThis.__dynamic_map_value = error.message);
                </script>
            </body></html>"#.into()),
            ("/app/later.js", 200, "application/javascript", "export const value = 'later-map';".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app/index.html")).await.unwrap();
        assert_eq!(
            p.evaluate("globalThis.__dynamic_map_value"),
            serde_json::json!("later-map")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn later_base_element_does_not_rebase_an_earlier_import_map() {
        let _g = net_test_guard();
        let port = local_http_server_typed(vec![
            ("/app/index.html", 200, "text/html",
             r#"<html><head>
                <script type="importmap">{"imports":{"fixed":"./before.js"}}</script>
                <base href="/assets/">
                <script type="module">
                    import { value } from "fixed";
                    globalThis.__temporal_base_value = value;
                </script>
            </head><body></body></html>"#.into()),
            ("/assets/before.js", 200, "application/javascript", "export const value = 'wrong-base';".into()),
            ("/app/before.js", 200, "application/javascript", "export const value = 'before-first-module';".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app/index.html")).await.unwrap();
        assert_eq!(
            p.evaluate("globalThis.__temporal_base_value"),
            serde_json::json!("before-first-module")
        );
    }
}
