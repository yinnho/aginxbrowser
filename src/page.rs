use std::sync::Arc;
use std::time::Duration;

use crate::diting_browser::{BrowserContext, Page as InnerPage};
use serde_json::Value;

use crate::error::Error;

/// Watchdog budget for evaluates that dispatch into page event handlers
/// (click/input). 10s is far beyond any legitimate synchronous handler; a
/// page that exceeds it (infinite loop, MutationObserver storm) would
/// otherwise pin the session thread inside V8 forever — unreachable by
/// tokio timeouts, unclosable, leaking 100% CPU.
pub(crate) const INTERACTION_EVAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Read a DOM node id from a JS `evaluate` result. the engine serializes JS numbers
/// as f64, so `Value::as_u64` returns None for an integer-valued result; accept
/// either an integer or a non-negative finite float. null / non-numbers -> None.
fn nid_from_value(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64))
}

/// A browser tab/page.
pub struct Page {
    pub(crate) inner: InnerPage,
    pub(crate) context: Arc<BrowserContext>,
}

impl Page {
    /// Navigate to URL and wait for load.
    pub async fn goto(&mut self, url: &str) -> Result<(), Error> {
        self.inner
            .navigate(url)
            .await
            .map_err(|e| Error::Navigation(e.to_string()))
    }

    /// Get current URL.
    pub fn url(&self) -> String {
        self.inner.url_string()
    }

    /// Drain a JS-initiated navigation (location.href / form.submit recorded
    /// while running an `evaluate`) through the real navigation path. JS
    /// navigation set during an evaluate would otherwise sit unconsumed.
    /// Returns true if a pending navigation was drained.
    pub async fn process_pending_navigation(&mut self) -> Result<bool, Error> {
        self.inner
            .process_pending_navigation()
            .await
            .map_err(|e| Error::Navigation(e.to_string()))
    }

    /// Execute JS in the page.
    pub fn evaluate(&mut self, expression: &str) -> Value {
        self.inner.evaluate(expression)
    }

    /// Execute JS in the page, awaiting any returned Promise.
    ///
    /// Use this for async scripts (fetch, IIFEs returning a Promise). The
    /// result is resolved by value, so JSON-stringified objects come back
    /// as strings just like the synchronous path.
    pub async fn evaluate_async(&mut self, expression: &str) -> Value {
        let info = self
            .inner
            .evaluate_for_cdp(expression, true, true)
            .await;
        info.value.unwrap_or(Value::Null)
    }

    /// Bounded evaluate for interaction dispatch (click/input). A runaway
    /// event handler — or a MutationObserver microtask storm it triggers —
    /// pins the session thread inside V8 where tokio timeouts cannot reach;
    /// the watchdog terminates the isolate at `timeout` so the session's
    /// command loop (including Close) regains control. Returns Null when
    /// terminated.
    pub fn evaluate_with_timeout(&mut self, expression: &str, timeout: Duration) -> Value {
        self.inner.evaluate_with_timeout(expression, timeout)
    }

    /// Pin the viewport (device emulation); survives navigation.
    pub fn set_viewport_override(&mut self, w: f32, h: f32, mobile: bool) {
        self.inner.set_viewport_override(w, h, mobile);
    }

    /// Get page HTML content.
    pub fn content(&mut self) -> String {
        let val = self.evaluate("document.documentElement.outerHTML");
        val.as_str().unwrap_or("").to_string()
    }

    /// Query a single element by CSS selector.
    pub fn query_selector(&mut self, selector: &str) -> Option<Element> {
        let escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
        let js = format!(
            "(function() {{ var el = document.querySelector('{}'); return el ? el._nid : null; }})()",
            escaped
        );
        let val = self.evaluate(&js);
        nid_from_value(&val).map(|nid| Element { node_id: nid, page: self as *const Page })
    }

    /// Wait for a named cookie to appear (polls every 200ms).
    pub async fn wait_for_cookie(&self, name: &str, timeout: Duration) -> Result<(), Error> {
        let start = std::time::Instant::now();
        loop {
            let url_str = self.url();
            if let Ok(parsed) = url::Url::parse(&url_str) {
                let header = self.context.cookie_jar.get_cookie_header(&parsed);
                // Cookie header format: "name1=value1; name2=value2"
                if header
                    .split("; ")
                    .any(|pair| pair.split('=').next().map(|n| n == name).unwrap_or(false))
                {
                    return Ok(());
                }
            }
            if start.elapsed() > timeout {
                return Err(Error::Timeout(format!(
                    "wait_for_cookie({}) timed out after {}ms",
                    name,
                    timeout.as_millis()
                )));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Drive the page's JS event loop for up to `max_ms` milliseconds.
    pub async fn settle(&mut self, max_ms: u64) {
        self.inner.settle(max_ms).await
    }

    /// Drive the JS event loop until quiescent, capped at `max_ms`. See
    /// [`crate::diting_browser::Page::settle_until_idle`].
    pub async fn settle_until_idle(&mut self, max_ms: u64) -> bool {
        self.inner.settle_until_idle(max_ms).await
    }

    /// One background event-loop slice (pump, then park when quiescent).
    /// See [`crate::diting_browser::Page::pump_event_loop_slice`].
    pub async fn pump_event_loop_slice(&mut self, ms: u64) {
        self.inner.pump_event_loop_slice(ms).await
    }

    /// Scroll the page by a relative offset.
    pub fn scroll_by(&mut self, dx: i32, dy: i32) {
        self.evaluate(&format!("window.scrollBy({}, {})", dx, dy));
    }
}

/// Handle to a DOM element.
///
/// Created via [`Page::query_selector`].
pub struct Element {
    node_id: u64,
    page: *const Page,
}

impl Element {
    /// Click this element.
    pub fn click(&self) -> Result<(), Error> {
        let page = unsafe { &mut *(self.page as *mut Page) };
        // Scroll into view
        page.evaluate_with_timeout(
            &format!(
                "(function() {{ var el = globalThis._wrap && globalThis._wrap({}); if (el) el.scrollIntoView({{block:'center'}}); }})()",
                self.node_id
            ),
            INTERACTION_EVAL_TIMEOUT,
        );
        // Click
        let result = page.evaluate_with_timeout(
            &format!(
                "(function() {{ var el = globalThis._wrap && globalThis._wrap({}); if (el) {{ el.click(); return true; }} return false; }})()",
                self.node_id
            ),
            INTERACTION_EVAL_TIMEOUT,
        );
        if result.as_bool().unwrap_or(false) {
            Ok(())
        } else {
            Err(Error::ElementNotFound("click failed".into()))
        }
    }
}
