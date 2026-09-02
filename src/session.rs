//! Interactive browser sessions for agent-style browsing.
//!
//! Each session lives on its own OS thread with a persistent Browser + Page.
//! Commands are dispatched via channels, results returned via oneshot.
//! Sessions auto-expire after 8 minutes of inactivity.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::page::Page;

/// Monotonic counter for unique session IDs within a process.
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Maximum idle time before a session is evicted.
const SESSION_TIMEOUT: Duration = Duration::from_secs(480); // 8 minutes

// ---------------------------------------------------------------------------
// Command protocol
// ---------------------------------------------------------------------------

pub enum SessionCommand {
    Navigate {
        url: String,
        reply: oneshot::Sender<Result<SessionNavResponse, String>>,
    },
    State {
        reply: oneshot::Sender<Result<String, String>>,
    },
    Click {
        index: usize,
        reply: oneshot::Sender<Result<SessionClickResponse, String>>,
    },
    Input {
        index: usize,
        text: String,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    Scroll {
        direction: ScrollDirection,
        amount: u32,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    Eval {
        script: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Export the session's current cookies (for the page's URL) as a JSON
    /// string `{"url":...,"cookies":["name=value",...]}`. Round-trips with
    /// `session_create`'s `cookies` field to replay a logged-in session.
    Cookies {
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Acknowledged by the session thread right before it exits. The closer
    /// waits on this to learn the thread actually stopped - without it, close
    /// replies ok while the thread is still pinned inside V8 (a runaway eval)
    /// and keeps burning CPU.
    Close {
        reply: oneshot::Sender<()>,
    },
    /// Export the session's recorded action log as JSONL (one
    /// RecordedAction per line) — the raw material for replay scripts.
    Export {
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Snapshot the page's network request log (the sniffer surface). With
    /// `media_only`, the reply is `{"url","media":[{url,kind,status,mime,via}]}`
    /// — playback links extracted from requests the page actually issued
    /// (via "network"), merged with DOM-observed media-element and player
    /// iframe sources the engine never fetches (via "dom" candidates);
    /// otherwise `{"url","total","requests":[...]}` compact rows.
    Network {
        media_only: bool,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Export the current page's traffic as a HAR 1.2 JSON document
    /// (retained response bodies included).
    Har {
        reply: oneshot::Sender<Result<String, String>>,
    },
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum ScrollDirection {
    Up,
    Down,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SessionNavResponse {
    pub url: String,
    pub title: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionClickResponse {
    pub url: String,
    pub clicked: bool,
    /// Post-click landed page text (body.innerText, capped), so the client
    /// can diff before/after in one response — same evidence contract as the
    /// stateless /click.
    pub text_after: Option<String>,
}

/// One live session, as reported by [`SessionManager::list`].
#[derive(Debug, Serialize)]
pub struct SessionListEntry {
    pub session_id: String,
    /// Seconds since the session last answered a command.
    pub idle_secs: u64,
    /// Idle budget left before auto-eviction.
    pub expires_in_secs: u64,
}

/// One recorded session action — the replay log. Only actions that change
/// the page are recorded; reads (State, Cookies) would just add noise to a
/// replay script.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RecordedAction {
    Create { url: Option<String>, use_proxy: bool, cookies: Vec<String> },
    Navigate { url: String, ok: bool },
    Click { index: usize, ok: bool },
    Input { index: usize, text: String, ok: bool },
    Scroll { direction: String, amount: u32 },
    Eval { script: String },
}

// ---------------------------------------------------------------------------
// Session handle
// ---------------------------------------------------------------------------

struct BrowserSession {
    cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    last_active: Instant,
}

impl BrowserSession {
    fn is_expired(&self) -> bool {
        self.last_active.elapsed() > SESSION_TIMEOUT
    }
}

// ---------------------------------------------------------------------------
// Session manager
// ---------------------------------------------------------------------------

pub struct SessionManager {
    sessions: HashMap<String, BrowserSession>,
}

/// Global session manager, shared between HTTP handlers and MCP tools.
pub static SESSIONS: std::sync::LazyLock<tokio::sync::Mutex<SessionManager>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(SessionManager::new()));

impl SessionManager {
    pub fn new() -> Self {
        SessionManager {
            sessions: HashMap::new(),
        }
    }

    /// Create a new browser session. Returns the session ID.
    pub fn create(&mut self, start_url: Option<&str>, use_proxy: bool, cookies: Vec<String>) -> String {
        let session_id = format!("s_{}", SESSION_COUNTER.fetch_add(1, Ordering::Relaxed));

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let thread_id = session_id.clone();
        let thread_url = start_url.map(|s| s.to_string());
        std::thread::Builder::new()
            .name(format!("session-{}", &thread_id[..8.min(thread_id.len())]))
            // Deep stack for the V8 isolate — see server::v8_stack_size.
            // A default 2 MB thread dies on minified SPA recursion
            // (juejin.cn class) before the page renders.
            .stack_size(crate::server::v8_stack_size())
            .spawn(move || {
                session_thread(thread_id, thread_url, use_proxy, cookies, cmd_rx);
            })
            .expect("failed to spawn session thread");

        self.sessions.insert(
            session_id.clone(),
            BrowserSession {
                cmd_tx,
                last_active: Instant::now(),
            },
        );
        session_id
    }

    /// Send a command to a session and await the result.
    pub async fn send<T: Send + 'static>(
        &mut self,
        session_id: &str,
        make_cmd: impl FnOnce(oneshot::Sender<Result<T, String>>) -> SessionCommand,
    ) -> Result<T, String> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| format!("session not found: {}", session_id))?;
        if session.is_expired() {
            self.close(session_id);
            return Err(format!("session expired: {}", session_id));
        }
        session.last_active = Instant::now();

        let (reply_tx, reply_rx) = oneshot::channel();
        session
            .cmd_tx
            .send(make_cmd(reply_tx))
            .map_err(|_| "session thread died".to_string())?;

        reply_rx.await.map_err(|_| "session thread died".to_string())?
    }

    /// Close and remove a session. Fire-and-forget; use [`Self::close_and_wait`]
    /// when the caller needs to know the session thread actually stopped.
    pub fn close(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.remove(session_id) {
            let (tx, _rx) = oneshot::channel();
            let _ = session.cmd_tx.send(SessionCommand::Close { reply: tx });
        }
    }

    /// Close and wait (bounded) for the session thread to acknowledge. Returns
    /// true if the thread exited (or was already dead); false means it did not
    /// ack within the budget - the command stays queued and the thread will
    /// exit when its current (watchdog-bounded) command finishes.
    pub async fn close_and_wait(&mut self, session_id: &str) -> bool {
        if let Some(session) = self.sessions.remove(session_id) {
            let (tx, rx) = oneshot::channel();
            if session.cmd_tx.send(SessionCommand::Close { reply: tx }).is_err() {
                return true; // thread already gone
            }
            match tokio::time::timeout(std::time::Duration::from_secs(20), rx).await {
                Ok(Ok(())) => true,
                Ok(Err(_)) => true, // dropped sender = thread exiting
                Err(_) => false,
            }
        } else {
            false
        }
    }

    /// Evict expired sessions.
    pub fn evict_expired(&mut self) {
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.is_expired())
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.close(&id);
        }
    }

    /// Snapshot of live sessions — id + idle age only, most recently active
    /// first. The page URL lives inside the session thread; fetching it per
    /// entry would round-trip a State command per session, too heavy for a
    /// listing. Call [`Self::evict_expired`] first if the list must not
    /// include idle-but-not-yet-evicted sessions.
    pub fn list(&self) -> Vec<SessionListEntry> {
        let mut out: Vec<SessionListEntry> = self
            .sessions
            .iter()
            .map(|(id, s)| SessionListEntry {
                session_id: id.clone(),
                idle_secs: s.last_active.elapsed().as_secs(),
                expires_in_secs: SESSION_TIMEOUT.saturating_sub(s.last_active.elapsed()).as_secs(),
            })
            .collect();
        out.sort_by_key(|e| e.idle_secs);
        out
    }

    /// Live session count (call [`Self::evict_expired`] first if the number
    /// must not include idle-but-not-yet-evicted sessions).
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

// ---------------------------------------------------------------------------
// Replay script generation
// ---------------------------------------------------------------------------

/// Embed a string in single quotes for bash: `'` → `'\''` (the standard
/// close-quote-escaped-quote-reopen idiom). Everything else is literal.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Render a recorded action log (JSONL, as returned by the Export command)
/// as a runnable bash script that replays the session against the HTTP API
/// with plain curl — replay with zero model tokens and nothing to install.
///
/// Index-based actions replay against indexes from the ORIGINAL run's
/// `/state` output; if the page's element order changed, indexes may point
/// elsewhere. That's inherent to index-based replay — the script is a
/// starting point, auditable and editable.
pub fn replay_bash(jsonl: &str, default_base: &str) -> String {
    let mut out = String::new();
    out.push_str("#!/usr/bin/env bash\n");
    out.push_str("# aginxbrowser session replay — recorded actions re-run as plain curl.\n");
    out.push_str("# No LLM in the loop: replay costs zero model tokens.\n");
    out.push_str("# Treat this file like credentials — it contains any cookies injected\n");
    out.push_str("# at session create.\n");
    out.push_str("set -eu\n");
    out.push_str(&format!("BASE=\"${{AGINXBROWSER_URL:-{default_base}}}\"\n"));
    out.push_str("POST() { curl -sS -X POST \"$BASE/$1\" -H 'Content-Type: application/json' -d \"$2\"; }\n\n");

    let mut sid_bound = false;
    for line in jsonl.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        // Payload = JSON body, shell-single-quoted for the POST helper's -d "$2".
        let payload = |body: Value| shell_quote(&body.to_string());
        match v["action"].as_str().unwrap_or_default() {
            "create" => {
                let body = payload(serde_json::json!({
                    "url": v["url"].clone(),
                    "cookies": v["cookies"].clone(),
                    "use_proxy": v["use_proxy"].as_bool().unwrap_or(false),
                }));
                out.push_str(&format!(
                    "SID=$(POST session/create {body} | sed -n 's/.*\"session_id\":\"\\([^\"]*\\)\".*/\\1/p')\n"
                ));
                out.push_str("[ -n \"$SID\" ] || { echo \"session create failed\" >&2; exit 1; }\n");
                sid_bound = true;
            }
            "navigate" if sid_bound => {
                let body = payload(serde_json::json!({"url": v["url"].clone()}));
                out.push_str(&format!("POST \"session/$SID/navigate\" {body} > /dev/null\n"));
            }
            "click" if sid_bound => {
                let body = payload(serde_json::json!({"index": v["index"].clone()}));
                out.push_str(&format!("POST \"session/$SID/click\" {body} > /dev/null\n"));
            }
            "input" if sid_bound => {
                let body = payload(serde_json::json!({"index": v["index"].clone(), "text": v["text"].clone()}));
                out.push_str(&format!("POST \"session/$SID/input\" {body} > /dev/null\n"));
            }
            "scroll" if sid_bound => {
                let body = payload(serde_json::json!({
                    "direction": v["direction"].clone(),
                    "amount": v["amount"].clone(),
                }));
                out.push_str(&format!("POST \"session/$SID/scroll\" {body} > /dev/null\n"));
            }
            "eval" if sid_bound => {
                let body = payload(serde_json::json!({"script": v["script"].clone()}));
                out.push_str(&format!("POST \"session/$SID/eval\" {body} > /dev/null\n"));
            }
            _ => {}
        }
    }
    if sid_bound {
        out.push_str("\necho '--- final state ---'\n");
        out.push_str("POST \"session/$SID/state\" '{}'\n");
        out.push_str("echo\n");
    }
    out
}

// ---------------------------------------------------------------------------
// Session thread — owns Browser + Page
// ---------------------------------------------------------------------------

fn session_thread(
    _session_id: String,
    start_url: Option<String>,
    use_proxy: bool,
    cookies: Vec<String>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build session runtime");

    rt.block_on(async {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let browser = crate::server::build_browser(use_proxy, "", None)
                    .expect("failed to build session browser");
                // Inject cookies before navigation so a session can start
                // already logged-in (cookies gathered from a prior session via
                // the Cookies command, or hand-exported). Mirrors /fetch.
                if !cookies.is_empty() {
                    let target = start_url.as_deref().unwrap_or("");
                    crate::server::inject_cookies(&browser, &cookies, target);
                }
                let mut page = browser.new_page().await.expect("failed to create session page");

                // Replay log: every page-changing action this session took,
                // in order. In-memory only, dies with the session; exported
                // explicitly via the Export command. Declared before the
                // initial navigation below (which moves start_url).
                let mut recorder: Vec<RecordedAction> = vec![RecordedAction::Create {
                    url: start_url.clone(),
                    use_proxy,
                    cookies: cookies.clone(),
                }];

                // Navigate to start URL if provided.
                // Page budget: every page this session walks counts (initial
                // navigation included). Bounds bulk walking; working one page
                // (state/scroll/eval/typing) stays free. See rate.rs.
                let mut pages_loaded: u32 = 0;
                if let Some(url) = start_url {
                    match page.goto(&url).await {
                        Ok(()) => pages_loaded += 1,
                        Err(e) => tracing::warn!("session: initial navigation failed: {}", e),
                    }
                }

                // Element index → _nid mapping (rebuilt on each /state call).
                let mut element_map: HashMap<usize, u64> = HashMap::new();

                // Command loop. Between commands the JS event loop keeps
                // running (200ms slices) instead of freezing on a blocking
                // recv(): timers, fetch callbacks and promise chains must
                // progress while the session idles, exactly like a real
                // browser's main thread. Without the pump, async work
                // started by page scripts stalled until the next command -
                // WorkOS Radar's 5s worker-response window expired with its
                // timer un-pumped (measured 31s frozen).
                loop {
                    let cmd = tokio::select! {
                        biased;
                        cmd = cmd_rx.recv() => match cmd {
                            Some(c) => c,
                            None => break,
                        },
                        _ = page.pump_event_loop_slice(200) => continue,
                    };
                    match cmd {
                        SessionCommand::Navigate { url, reply } => {
                            // Budget first (local, free), then the per-domain
                            // rate gate — same stance as the stateless paths.
                            let result = match crate::rate::check_page_budget(pages_loaded)
                                .and_then(|_| crate::rate::check_domain(&url))
                            {
                                Err(reason) => Err(reason),
                                Ok(()) => match page.goto(&url).await {
                                    Ok(()) => {
                                        let final_url = page.url();
                                        let title = page
                                            .evaluate("document.title")
                                            .as_str()
                                            .filter(|s| !s.is_empty())
                                            .map(|s| s.to_string());
                                        element_map.clear();
                                        pages_loaded += 1;
                                        Ok(SessionNavResponse { url: final_url, title })
                                    }
                                    Err(e) => Err(format!("navigation failed: {}", e)),
                                },
                            };
                            recorder.push(RecordedAction::Navigate {
                                ok: result.is_ok(),
                                url,
                            });
                            let _ = reply.send(result);
                        }

                        SessionCommand::State { reply } => {
                            element_map.clear();
                            let result = extract_indexed_state(&mut page, &mut element_map);
                            let _ = reply.send(result);
                        }

                        SessionCommand::Click { index, reply } => {
                            // A click that changes the page is a page walk and
                            // spends the budget; one that only toggles state
                            // (a checkbox, a menu) is free.
                            let result = match crate::rate::check_page_budget(pages_loaded) {
                                Err(reason) => Err(reason),
                                Ok(()) => {
                                    let before = page.url();
                                    click_by_index(&mut page, &element_map, index)
                                        .await
                                        .map(|resp| {
                                            if resp.url != before {
                                                pages_loaded += 1;
                                            }
                                            resp
                                        })
                                }
                            };
                            recorder.push(RecordedAction::Click { index, ok: result.is_ok() });
                            let _ = reply.send(result);
                        }

                        SessionCommand::Input { index, text, reply } => {
                            let result = input_by_index(&mut page, &element_map, index, &text);
                            recorder.push(RecordedAction::Input {
                                index,
                                text,
                                ok: result.is_ok(),
                            });
                            let _ = reply.send(result);
                        }

                        SessionCommand::Scroll { direction, amount, reply } => {
                            let dy = match direction {
                                ScrollDirection::Up => -(amount as i32) * 100,
                                ScrollDirection::Down => (amount as i32) * 100,
                            };
                            let js = format!("window.scrollBy(0, {})", dy);
                            page.evaluate_with_timeout(&js, crate::page::INTERACTION_EVAL_TIMEOUT);
                            recorder.push(RecordedAction::Scroll {
                                direction: match direction {
                                    ScrollDirection::Up => "up".to_string(),
                                    ScrollDirection::Down => "down".to_string(),
                                },
                                amount,
                            });
                            let _ = reply.send(Ok(true));
                        }

                        SessionCommand::Eval { script, reply } => {
                            let val = page.evaluate_async(&script).await;
                            recorder.push(RecordedAction::Eval { script });
                            // Drain any JS-initiated navigation the script
                            // started (location.href / form submit) so the
                            // session's current URL moves with it — same
                            // policy as click_by_index below.
                            let _ = page.process_pending_navigation().await;
                            let _ = reply.send(Ok(val));
                        }

                        SessionCommand::Cookies { reply } => {
                            let url_str = page.url();
                            let cookies: Vec<String> = match url::Url::parse(&url_str) {
                                Ok(u) => page
                                    .context
                                    .cookie_jar
                                    .get_cookie_header(&u)
                                    .split("; ")
                                    .filter(|s| !s.is_empty())
                                    .map(|s| s.to_string())
                                    .collect(),
                                Err(_) => vec![],
                            };
                            let resp = serde_json::json!({ "url": url_str, "cookies": cookies });
                            let _ = reply.send(Ok(resp.to_string()));
                        }

                        SessionCommand::Export { reply } => {
                            let jsonl = recorder
                                .iter()
                                .filter_map(|a| serde_json::to_string(a).ok())
                                .collect::<Vec<_>>()
                                .join("\n");
                            let _ = reply.send(Ok(jsonl));
                        }

                        SessionCommand::Network { media_only, reply } => {
                            page.inner.sync_js_network_events();
                            let payload = if media_only {
                                // Media elements and player iframes are never
                                // fetched by the engine (no media/frame
                                // loading), so they cannot surface in the
                                // network log — collect their DOM sources and
                                // merge them in as candidates.
                                let dom = page
                                    .evaluate(DOM_MEDIA_SCRIPT)
                                    .as_str()
                                    .unwrap_or("[]")
                                    .to_string();
                                let events = &page.inner.network_events;
                                let mut media = crate::har::media_entries(events);
                                merge_dom_candidates(&mut media, &dom);
                                let body_of = |rid: &str| page.inner.get_response_body(rid);
                                media.extend(crate::har::media_from_bodies(events, &body_of));
                                serde_json::json!({
                                    "url": page.url(),
                                    "media": media,
                                })
                            } else {
                                let events = &page.inner.network_events;
                                serde_json::json!({
                                    "url": page.url(),
                                    "total": events.len(),
                                    "requests": crate::har::compact_events(events),
                                })
                            };
                            let _ = reply.send(Ok(payload.to_string()));
                        }

                        SessionCommand::Har { reply } => {
                            page.inner.sync_js_network_events();
                            let title = page
                                .evaluate("document.title")
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            let events = &page.inner.network_events;
                            let body_of = |rid: &str| page.inner.get_response_body(rid);
                            let har = crate::har::har_log(&title, events, &body_of);
                            let _ = reply.send(Ok(har.to_string()));
                        }

                        SessionCommand::Close { reply } => {
                            let _ = reply.send(());
                            break;
                        }
                    }

                    // Pump the JS event loop briefly after every command.
                    // Commands that only evaluate synchronously (Eval
                    // returning a non-Promise, Scroll, State) can still have
                    // started async work — a fetch fired from a submit/click
                    // handler, promise chains, timers — which needs
                    // event-loop turns to progress. Without this the work
                    // stranded until the next navigation (React server-action
                    // fetches never resolved). Returns immediately when the
                    // loop is idle, so quiescent pages pay nothing; busy pages
                    // get up to 1.5s of drain per command.
                    page.settle_until_idle(1500).await;
                }
            })
            .await;
    });
}

// ---------------------------------------------------------------------------
// DOM media candidates
// ---------------------------------------------------------------------------

/// Collect playback-relevant sources the engine never fetches (media
/// elements, player iframes), as a JSON array of `{url, tag}`. Relative URLs
/// resolve against the page location; duplicates collapse.
const DOM_MEDIA_SCRIPT: &str = r#"(function(){
    var out = [];
    var seen = {};
    function add(u, tag) {
        if (!u) return;
        try { u = new URL(String(u), location.href).href; } catch (e) { return; }
        if (!seen[u]) { seen[u] = 1; out.push({ url: u, tag: tag }); }
    }
    var els = document.querySelectorAll('video,audio,source,iframe');
    for (var i = 0; i < els.length; i++) {
        var e = els[i];
        add(e.getAttribute('src'), e.tagName.toLowerCase());
    }
    return JSON.stringify(out);
})()"#;

/// Merge DOM-observed candidates into the network-derived media list. A
/// candidate the network log already confirms (same URL ignoring query —
/// players append auth/expiry tokens the markup never carries) is dropped;
/// iframes surface as kind "iframe" (player pages to navigate or sniff
/// inside, not playable URLs themselves); the rest must classify as media
/// or they are dropped. These are candidates, not confirmations — `via`
/// says which side produced each entry.
fn merge_dom_candidates(media: &mut Vec<Value>, dom_json: &str) {
    let dom: Vec<Value> = match serde_json::from_str(dom_json) {
        Ok(v) => v,
        Err(_) => return,
    };
    let bare = |u: &str| {
        u.split(['?', '#'])
            .next()
            .unwrap_or(u)
            .to_ascii_lowercase()
    };
    let confirmed: std::collections::HashSet<String> = media
        .iter()
        .filter_map(|m| m["url"].as_str())
        .map(bare)
        .collect();
    for cand in dom {
        let Some(url) = cand["url"].as_str().map(str::to_string) else {
            continue;
        };
        let tag = cand["tag"].as_str().unwrap_or("").to_string();
        if confirmed.contains(&bare(&url)) {
            continue;
        }
        let kind = if tag == "iframe" {
            "iframe".to_string()
        } else {
            match crate::har::media_kind(&url, None) {
                Some(k) => k.to_string(),
                None => continue,
            }
        };
        media.push(serde_json::json!({
            "url": url,
            "kind": kind,
            "via": "dom",
            "tag": tag,
        }));
    }
}

// ---------------------------------------------------------------------------
// Indexed state extraction
// ---------------------------------------------------------------------------

/// JS script that queries all interactive elements, assigns sequential indexes,
/// stores the `_nid` mapping in `window.__session_element_map`, and returns a
/// JSON array of element descriptors.
const STATE_SCRIPT: &str = r#"
(function() {
    var interactive = document.querySelectorAll(
        'a, button, input, select, textarea, [role="button"], [role="link"], [onclick], [tabindex]'
    );
    var elements = [];
    var indexMap = {};
    var idx = 0;
    for (var i = 0; i < interactive.length; i++) {
        var el = interactive[i];
        var style = el.offsetWidth === 0 && el.offsetHeight === 0;
        if (style) continue;
        var box = el.getBoundingClientRect();
        var info = {
            index: idx,
            tag: el.tagName.toLowerCase(),
            text: (el.innerText || '').trim().substring(0, 100),
            x: Math.round(box.x), y: Math.round(box.y),
            w: Math.round(box.width), h: Math.round(box.height),
            attrs: {}
        };
        var attrNames = ['id', 'class', 'href', 'type', 'name', 'value',
                         'placeholder', 'aria-label', 'title', 'src', 'alt', 'role'];
        for (var j = 0; j < attrNames.length; j++) {
            var v = el.getAttribute(attrNames[j]);
            if (v !== null) info.attrs[attrNames[j]] = v;
        }
        if (el._nid !== undefined) {
            indexMap[idx] = el._nid;
        }
        elements.push(info);
        idx++;
    }
    window.__session_element_map = indexMap;
    return JSON.stringify({url: location.href, title: document.title,
                           viewport: {w: window.innerWidth, h: window.innerHeight},
                           elements: elements});
})()
"#;

fn extract_indexed_state(
    page: &mut Page,
    element_map: &mut HashMap<usize, u64>,
) -> Result<String, String> {
    let val = page.evaluate(STATE_SCRIPT);
    let json_str = match val.as_str() {
        Some(s) => s.to_string(),
        None => return Err("state extraction returned non-string".into()),
    };

    // Parse the JSON to extract element_map, then format as compact text.
    let parsed: Value = serde_json::from_str(&json_str)
        .map_err(|e| format!("state parse error: {}", e))?;

    // Build element_map from the JS-side indexMap.
    let map_val = page.evaluate("JSON.stringify(window.__session_element_map)");
    if let Some(map_str) = map_val.as_str() {
        if let Ok(map_obj) = serde_json::from_str::<HashMap<String, u64>>(map_str) {
            for (k, v) in map_obj {
                if let Ok(idx) = k.parse::<usize>() {
                    element_map.insert(idx, v);
                }
            }
        }
    }

    // Format compact text output.
    let url = parsed.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let title = parsed.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let mut out = String::new();
    out.push_str(&format!("url={}\n", url));
    out.push_str(&format!("title={}\n", title));
    // Viewport size so the agent can tell which rects are on-screen
    // (scroll down / scroll to element before clicking off-viewport ones).
    if let Some(vp) = parsed.get("viewport") {
        out.push_str(&format!(
            "viewport={}x{}\n\n",
            vp.get("w").and_then(|v| v.as_i64()).unwrap_or(0),
            vp.get("h").and_then(|v| v.as_i64()).unwrap_or(0)
        ));
    } else {
        out.push('\n');
    }

    if let Some(elements) = parsed.get("elements").and_then(|v| v.as_array()) {
        for el in elements {
            let idx = el.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
            let tag = el.get("tag").and_then(|v| v.as_str()).unwrap_or("");
            let text = el.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let attrs = el.get("attrs").and_then(|v| v.as_object());

            let mut attr_parts = Vec::new();
            if let Some(attrs) = attrs {
                for (k, v) in attrs {
                    let vs = v.as_str().unwrap_or("");
                    // Truncate long class values. Slice by chars, not bytes —
                    // multi-byte UTF-8 (—, CJK) panics on byte indexing.
                    if k == "class" && vs.chars().count() > 50 {
                        let t: String = vs.chars().take(50).collect();
                        attr_parts.push(format!("{}=\"{}…\"", k, t));
                    } else {
                        attr_parts.push(format!("{}=\"{}\"", k, vs));
                    }
                }
            }
            let attr_str = if attr_parts.is_empty() {
                String::new()
            } else {
                format!(" {}", attr_parts.join(" "))
            };
            // Page-relative rect (viewport coords: y is relative to the
            // current scroll position) — lets the agent "see where it is"
            // before clicking or scrolling.
            let rect = format!(
                " rect=[{},{},{}x{}]",
                el.get("x").and_then(|v| v.as_i64()).unwrap_or(0),
                el.get("y").and_then(|v| v.as_i64()).unwrap_or(0),
                el.get("w").and_then(|v| v.as_i64()).unwrap_or(0),
                el.get("h").and_then(|v| v.as_i64()).unwrap_or(0)
            );

            if text.is_empty() {
                out.push_str(&format!("[{}] <{}{}{} />\n", idx, tag, attr_str, rect));
            } else {
                let display_text = if text.chars().count() > 80 {
                    let t: String = text.chars().take(80).collect();
                    format!("{}…", t)
                } else {
                    text.to_string()
                };
                out.push_str(&format!("[{}] <{}{}{}>{}</{}>\n", idx, tag, attr_str, rect, display_text, tag));
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Click / Input by index
// ---------------------------------------------------------------------------

async fn click_by_index(
    page: &mut Page,
    element_map: &HashMap<usize, u64>,
    index: usize,
) -> Result<SessionClickResponse, String> {
    let nid = *element_map.get(&index).ok_or_else(|| format!("invalid index: {}", index))?;
    let js = format!(
        "(function() {{ var el = globalThis._wrap && globalThis._wrap({}); if (el) {{ el.scrollIntoView({{block:'center'}}); el.click(); return true; }} return false; }})()",
        nid
    );
    let result = page.evaluate_with_timeout(&js, crate::page::INTERACTION_EVAL_TIMEOUT);
    let clicked = result.as_bool().unwrap_or(false);
    // Drain any JS-initiated navigation the click started (location.href /
    // form.submit) so the returned URL reflects the post-click page — matches
    // the firecrawl /v1/scrape click handling.
    let _ = page.process_pending_navigation().await;
    if clicked {
        // Wait for quiescence, not a fixed slice: a client-side route
        // transition (RSC fetch → flight parse → render → pushState) only
        // counts as done when the loop drains. Capped so interval-heavy
        // pages can't pin the command.
        page.settle_until_idle(5000).await;
    }
    let url = page.url();
    let text_after = page
        .evaluate("document.body.innerText")
        .as_str()
        .map(|s| s.chars().take(2000).collect::<String>());
    Ok(SessionClickResponse { url, clicked, text_after })
}

fn input_by_index(
    page: &mut Page,
    element_map: &HashMap<usize, u64>,
    index: usize,
    text: &str,
) -> Result<bool, String> {
    let nid = *element_map.get(&index).ok_or_else(|| format!("invalid index: {}", index))?;
    // Escape single quotes in text.
    let escaped = text.replace('\\', "\\\\").replace('\'', "\\'");
    // React/Vue controlled inputs: assigning `el.value` directly goes through
    // React's _valueTracker own-property setter, which records the new value -
    // the following `input` event then compares equal and React swallows it
    // (onChange never fires). Reset the tracker and use the prototype setter
    // so the dispatched event registers as a real change.
    let js = format!(
        "(function() {{ var el = globalThis._wrap && globalThis._wrap({}); if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {{ el.focus(); if (el._valueTracker) el._valueTracker.setValue(''); var p = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value') || Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value'); if (p && p.set) p.set.call(el, '{}'); else el.value = '{}'; el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(new Event('change', {{bubbles: true}})); return true; }} return false; }})()",
        nid, escaped, escaped
    );
    let result = page.evaluate_with_timeout(&js, crate::page::INTERACTION_EVAL_TIMEOUT);
    Ok(result.as_bool().unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression (WorkOS Radar collector frozen 31s; the same bug class the
    /// upstream engine fixed as v0.2.1 "MCP pumps the page task queue between
    /// tool calls"): while a session sits idle between commands, its page's
    /// timers, microtasks and interval callbacks must keep firing like a real
    /// browser's main thread. Arms a timer, a promise chain and an interval,
    /// stays idle well past their deadlines with no command in flight, then
    /// reads the markers back. A session that parks on a blocking recv()
    /// instead of pumping 200ms slices leaves all three markers unset.
    #[tokio::test]
    async fn idle_session_keeps_timers_and_microtasks_firing() {
        let mut mgr = SessionManager::new();
        let sid = mgr.create(Some("about:blank"), false, vec![]);

        let armed = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: r#"(function() {
                    window.__marks = {timeout: false, micro: false, ticks: 0};
                    setTimeout(function() { window.__marks.timeout = true; }, 400);
                    Promise.resolve().then(function() { window.__marks.micro = true; });
                    setInterval(function() { window.__marks.ticks++; }, 200);
                    return 'armed';
                })()"#
                    .to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(armed.as_str().unwrap_or(""), "armed");

        // Idle gap: no command sent for well past the 400ms timer deadline.
        tokio::time::sleep(Duration::from_millis(1500)).await;

        let state = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "JSON.stringify(window.__marks)".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert!(mgr.close_and_wait(&sid).await, "session thread must ack close");

        let json: Value = serde_json::from_str(state.as_str().unwrap_or("null")).unwrap_or(Value::Null);
        let timeout = json.get("timeout").and_then(|v| v.as_bool()).unwrap_or(false);
        let micro = json.get("micro").and_then(|v| v.as_bool()).unwrap_or(false);
        let ticks = json.get("ticks").and_then(|v| v.as_i64()).unwrap_or(0);
        // 1.5s idle at a 200ms interval ≈ 7 ticks; 3 is a safe floor that
        // still proves sustained pumping (one post-command drain gives 0-1).
        assert!(timeout, "setTimeout must fire during the idle gap");
        assert!(micro, "promise chain must settle during the idle gap");
        assert!(ticks >= 3, "interval must keep firing while idle, got {ticks} ticks");
    }

    /// Regression (obscura #618 class): an eval whose script clicks a submit
    /// button must leave the session on the form's action URL — the click
    /// stores a pending JS navigation that the Eval command drains (same
    /// policy as click_by_index).
    #[tokio::test]
    async fn eval_submit_click_navigates_the_session() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[
            (
                "GET /form",
                "<html><body><form method='POST' action='/done'>\
                 <input name='q' value='hello'>\
                 <button type='submit' id='go'>Go</button></form></body></html>",
            ),
            ("POST /done", "<html><body>submitted ok</body></html>"),
        ]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(
            Some(&format!("http://127.0.0.1:{port}/form")),
            false,
            vec![],
        );

        let clicked = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "document.querySelector('#go').click(); 'clicked'".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(clicked.as_str().unwrap_or(""), "clicked");

        let href = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "location.href".to_string(),
                reply,
            })
            .await
            .unwrap();
        assert!(
            href.as_str().unwrap_or("").ends_with("/done"),
            "session must follow the submit navigation, got {}",
            href
        );
        assert!(
            mgr.close_and_wait(&sid).await,
            "session thread must ack close"
        );
    }

    /// list() reports every live session with an expiry budget, most recently
    /// active first, and drops to empty once sessions close.
    #[tokio::test]
    async fn list_reports_live_sessions_and_goes_empty_after_close() {
        let mut mgr = SessionManager::new();
        let a = mgr.create(Some("about:blank"), false, vec![]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let b = mgr.create(Some("about:blank"), false, vec![]);

        let entries = mgr.list();
        assert_eq!(entries.len(), 2);
        let ids: Vec<&str> = entries.iter().map(|e| e.session_id.as_str()).collect();
        assert!(ids.contains(&a.as_str()) && ids.contains(&b.as_str()));
        assert!(entries[0].idle_secs <= entries[1].idle_secs, "most recent first");
        for e in &entries {
            assert!(e.expires_in_secs <= 480, "expiry budget caps at the 8 min timeout");
        }

        assert!(mgr.close_and_wait(&a).await);
        assert!(mgr.close_and_wait(&b).await);
        assert!(mgr.list().is_empty());
    }

    /// A recorded session replays as a bash+curl script: every action type
    /// becomes a POST against $BASE, and single quotes in payloads survive
    /// shell quoting (the '\'' idiom) instead of terminating the argument.
    #[test]
    fn replay_bash_renders_all_actions_and_quotes_singles() {
        let jsonl = [
            r#"{"action":"create","url":"https://example.com/","use_proxy":false,"cookies":["sid=it's"]}"#,
            r#"{"action":"navigate","url":"https://example.com/page","ok":true}"#,
            r#"{"action":"click","index":3,"ok":true}"#,
            r#"{"action":"input","index":1,"text":"it's a test","ok":true}"#,
            r#"{"action":"scroll","direction":"down","amount":3}"#,
            r#"{"action":"eval","script":"document.querySelector('#q').value"}"#,
        ]
        .join("\n");

        let script = replay_bash(&jsonl, "http://127.0.0.1:8089");
        assert!(script.starts_with("#!/usr/bin/env bash"));
        assert!(script.contains(r#"BASE="${AGINXBROWSER_URL:-http://127.0.0.1:8089}""#));
        assert!(script.contains("POST session/create"));
        assert!(script.contains(r#"POST "session/$SID/navigate""#));
        assert!(script.contains(r#"POST "session/$SID/click""#));
        assert!(script.contains(r#"POST "session/$SID/input""#));
        assert!(script.contains(r#"POST "session/$SID/scroll""#));
        assert!(script.contains(r#"POST "session/$SID/eval""#));
        assert!(script.contains(r#"POST "session/$SID/state" '{}'"#), "ends by printing final state");
        // Single-quote escaping: it's → 'it'\''s — an unescaped quote would
        // terminate the argument and execute the rest as shell.
        assert!(script.contains(r#""sid=it'\''s"]"#));
        assert!(script.contains(r#""text":"it'\''s a test""#));
        // Command substitution appears exactly once — capturing SID from the
        // create call. Payloads are plain single-quoted strings, never
        // $(...)-wrapped (that would execute JSON as shell).
        assert_eq!(script.matches("$(").count(), 1);
    }

    /// Actions before a create record (or a log with no create at all) must
    /// not emit $SID-referencing POSTs — the script would die on an unbound
    /// variable under set -eu.
    #[test]
    fn replay_bash_without_create_skips_sid_actions() {
        let jsonl = r#"{"action":"navigate","url":"https://example.com/","ok":true}"#;
        let script = replay_bash(jsonl, "http://127.0.0.1:8089");
        assert!(!script.contains("$SID"), "no SID may be referenced without a create");
    }

    /// A live session records what it did: create params + one entry per
    /// navigate/scroll/eval command, exportable as JSONL. Reads (State,
    /// Cookies) stay out of the log — replaying them is meaningless.
    #[tokio::test]
    async fn session_records_actions_and_exports_jsonl() {
        let mut mgr = SessionManager::new();
        let sid = mgr.create(Some("about:blank"), false, vec!["k=v".to_string()]);

        let _ = mgr
            .send(&sid, |reply| SessionCommand::State { reply })
            .await
            .unwrap(); // read: not recorded
        let _ = mgr
            .send(&sid, |reply| SessionCommand::Scroll {
                direction: ScrollDirection::Down,
                amount: 2,
                reply,
            })
            .await
            .unwrap();
        let _ = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "1 + 1".to_string(),
                reply,
            })
            .await
            .unwrap();
        let jsonl = mgr
            .send(&sid, |reply| SessionCommand::Export { reply })
            .await
            .unwrap();
        assert!(mgr.close_and_wait(&sid).await, "session thread must ack close");

        let actions: Vec<Value> = jsonl.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(actions.len(), 3, "create + scroll + eval, state excluded");

        assert_eq!(actions[0]["action"], "create");
        assert_eq!(actions[0]["url"], "about:blank");
        assert_eq!(actions[0]["cookies"][0], "k=v");
        assert_eq!(actions[0]["use_proxy"], false);

        assert_eq!(actions[1]["action"], "scroll");
        assert_eq!(actions[1]["direction"], "down");
        assert_eq!(actions[1]["amount"], 2);

        assert_eq!(actions[2]["action"], "eval");
        assert_eq!(actions[2]["script"], "1 + 1");

        // The exported log must render through replay_bash without losing
        // actions (create present → SID-bound POSTs emitted).
        let script = replay_bash(&jsonl, "http://127.0.0.1:8089");
        assert!(script.contains("POST session/create"));
        assert!(script.contains(r#"POST "session/$SID/scroll""#));
        assert!(script.contains(r#"POST "session/$SID/eval""#));
    }

    /// The session sniffer: a page-side fetch() of a media URL surfaces in
    /// the Network command (media filter extracts the playback link), and
    /// Har returns a parseable HAR 1.2 document whose document entry carries
    /// its retained text body. Media elements and player iframes the engine
    /// never fetches surface as DOM candidates (via "dom"); the token-less
    /// <source> URL dedupes against its token-carrying network twin.
    #[tokio::test]
    async fn network_sniffer_and_har_surfaces() {
        let _net = crate::server::test_util::net_env_guard();
        let (port, _hits) = crate::server::test_util::recording_server(&[
            (
                "GET /watch",
                "<html><body>\
                 <video src='/v/clip.mp4' controls><source src='/v/master.m3u8'></video>\
                 <iframe src='/embed'></iframe>\
                 <script>\
                 fetch('/v/master.m3u8?token=1').then(function(r){return r.text()});\
                 </script></body></html>",
            ),
            ("GET /v/master.m3u8?token=1", "#EXTM3U"),
        ]);

        let mut mgr = SessionManager::new();
        let sid = mgr.create(Some(&format!("http://127.0.0.1:{port}/watch")), false, vec![]);

        // One pump cycle so the page's fetch() settles into the event queue.
        let _ = mgr
            .send(&sid, |reply| SessionCommand::Eval {
                script: "1".to_string(),
                reply,
            })
            .await
            .unwrap();

        let media = mgr
            .send(&sid, |reply| SessionCommand::Network { media_only: true, reply })
            .await
            .unwrap();
        let media: Value = serde_json::from_str(&media).unwrap();
        let items = media["media"].as_array().unwrap();
        assert_eq!(
            items.len(),
            3,
            "network m3u8 + dom video + dom iframe (source deduped): {media}"
        );
        assert_eq!(items[0]["kind"], "hls");
        assert_eq!(items[0]["via"], "network");
        assert!(
            items[0]["url"].as_str().unwrap().ends_with("/v/master.m3u8?token=1"),
            "playback link carries its query: {media}"
        );
        // Native video src the engine never fetched: a dom candidate.
        assert_eq!(items[1]["via"], "dom");
        assert_eq!(items[1]["tag"], "video");
        assert_eq!(items[1]["kind"], "mp4");
        assert!(
            items[1]["url"].as_str().unwrap().ends_with("/v/clip.mp4"),
            "native video src is a dom candidate: {media}"
        );
        // Player iframe: a candidate to navigate into, not a playable URL.
        assert_eq!(items[2]["via"], "dom");
        assert_eq!(items[2]["tag"], "iframe");
        assert_eq!(items[2]["kind"], "iframe");
        assert!(
            items[2]["url"].as_str().unwrap().ends_with("/embed"),
            "player iframe surfaces as a navigation candidate: {media}"
        );

        let all = mgr
            .send(&sid, |reply| SessionCommand::Network { media_only: false, reply })
            .await
            .unwrap();
        let all: Value = serde_json::from_str(&all).unwrap();
        assert!(
            all["total"].as_u64().unwrap() >= 2,
            "document + script fetch: {all}"
        );

        let har = mgr
            .send(&sid, |reply| SessionCommand::Har { reply })
            .await
            .unwrap();
        let har: Value = serde_json::from_str(&har).unwrap();
        assert_eq!(har["log"]["version"], "1.2");
        let entries = har["log"]["entries"].as_array().unwrap();
        assert!(entries.len() >= 2, "document + fetch entry: {har}");
        let doc = entries
            .iter()
            .find(|e| e["request"]["url"].as_str().unwrap().ends_with("/watch"))
            .unwrap();
        assert_eq!(doc["response"]["status"], 200);
        assert!(
            doc["response"]["content"]["text"].as_str().unwrap().contains("master.m3u8"),
            "document body retained as text"
        );

        assert!(mgr.close_and_wait(&sid).await, "session thread must ack close");
    }
}
