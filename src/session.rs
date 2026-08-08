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
    Close,
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

    /// Close and remove a session.
    pub fn close(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.remove(session_id) {
            let _ = session.cmd_tx.send(SessionCommand::Close);
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

                // Navigate to start URL if provided.
                if let Some(url) = start_url {
                    if let Err(e) = page.goto(&url).await {
                        tracing::warn!("session: initial navigation failed: {}", e);
                    }
                }

                // Element index → _nid mapping (rebuilt on each /state call).
                let mut element_map: HashMap<usize, u64> = HashMap::new();

                // Command loop.
                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        SessionCommand::Navigate { url, reply } => {
                            let result = match page.goto(&url).await {
                                Ok(()) => {
                                    let final_url = page.url();
                                    let title = page
                                        .evaluate("document.title")
                                        .as_str()
                                        .filter(|s| !s.is_empty())
                                        .map(|s| s.to_string());
                                    element_map.clear();
                                    Ok(SessionNavResponse { url: final_url, title })
                                }
                                Err(e) => Err(format!("navigation failed: {}", e)),
                            };
                            let _ = reply.send(result);
                        }

                        SessionCommand::State { reply } => {
                            element_map.clear();
                            let result = extract_indexed_state(&mut page, &mut element_map);
                            let _ = reply.send(result);
                        }

                        SessionCommand::Click { index, reply } => {
                            let result = click_by_index(&mut page, &element_map, index).await;
                            let _ = reply.send(result);
                        }

                        SessionCommand::Input { index, text, reply } => {
                            let result = input_by_index(&mut page, &element_map, index, &text);
                            let _ = reply.send(result);
                        }

                        SessionCommand::Scroll { direction, amount, reply } => {
                            let dy = match direction {
                                ScrollDirection::Up => -(amount as i32) * 100,
                                ScrollDirection::Down => (amount as i32) * 100,
                            };
                            let js = format!("window.scrollBy(0, {})", dy);
                            page.evaluate(&js);
                            let _ = reply.send(Ok(true));
                        }

                        SessionCommand::Eval { script, reply } => {
                            let val = page.evaluate_async(&script).await;
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

                        SessionCommand::Close => {
                            break;
                        }
                    }
                }
            })
            .await;
    });
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
        var info = {
            index: idx,
            tag: el.tagName.toLowerCase(),
            text: (el.innerText || '').trim().substring(0, 100),
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
    return JSON.stringify({url: location.href, title: document.title, elements: elements});
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
    out.push_str(&format!("title={}\n\n", title));

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
                    // Truncate long class values.
                    if k == "class" && vs.len() > 50 {
                        attr_parts.push(format!("{}=\"{}…\"", k, &vs[..50]));
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

            if text.is_empty() {
                out.push_str(&format!("[{}] <{}{} />\n", idx, tag, attr_str));
            } else {
                let display_text = if text.len() > 80 {
                    format!("{}…", &text[..80])
                } else {
                    text.to_string()
                };
                out.push_str(&format!("[{}] <{}{}>{}</{}>\n", idx, tag, attr_str, display_text, tag));
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
    let result = page.evaluate(&js);
    let clicked = result.as_bool().unwrap_or(false);
    // Drain any JS-initiated navigation the click started (location.href /
    // form.submit) so the returned URL reflects the post-click page — matches
    // the firecrawl /v1/scrape click handling.
    let _ = page.process_pending_navigation().await;
    if clicked {
        page.settle(800).await;
    }
    let url = page.url();
    Ok(SessionClickResponse { url, clicked })
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
    let js = format!(
        "(function() {{ var el = globalThis._wrap && globalThis._wrap({}); if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {{ el.focus(); el.value = '{}'; el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(new Event('change', {{bubbles: true}})); return true; }} return false; }})()",
        nid, escaped
    );
    let result = page.evaluate(&js);
    Ok(result.as_bool().unwrap_or(false))
}
