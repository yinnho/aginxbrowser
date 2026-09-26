//! Host→realm wiring: the setters and drains the embedding uses to hand
//! page state into the JS realm (DOM, net client, cookie jar, referrer,
//! charset, sheets, resource timings, dialog policy, interception channel)
//! and to pull settled work back out (pending navigation, console calls,
//! network events/bodies, dynamic-script in-flight counts). Split from the
//! module root (god-file ratchet); pure plumbing, no V8 entry of its own.
use super::JsRuntime;
use crate::diting_dom::DomTree;

impl JsRuntime {
    pub fn set_cookie_jar(&self, jar: std::sync::Arc<crate::diting_net::CookieJar>) {
        self.state.borrow_mut().cookie_jar = Some(jar);
    }

    /// Parse and merge an inline document import map (upstream 34373c3).
    /// Rules which would alter already-observed module resolutions are
    /// discarded while unrelated new rules remain available, matching
    /// Chromium's multiple-map model.
    pub fn add_import_map(&self, source: &str, base_url: &str) -> Result<(), String> {
        let map = crate::diting_js::import_map::ImportMap::parse(source, base_url)?;
        self.state
            .borrow()
            .import_map
            .try_borrow_mut()
            .map_err(|_| "Import map is already borrowed".to_string())?
            .merge(map);
        Ok(())
    }

    pub fn set_http_client(&self, client: std::sync::Arc<crate::diting_net::HttpClient>) {
        self.state.borrow_mut().http_client = Some(client.clone());
        self.module_loader.set_http_client(client);
    }

    pub fn set_dom(&self, dom: DomTree) {
        let mut st = self.state.borrow_mut();
        st.dom = Some(dom);
        // New document: the previous page's scroll offset and image bodies
        // mean nothing here (stale URLs would simply miss, but they'd hold
        // memory until the entry cap evicts them). Transitions must be
        // cleared outright: their node ids are per-document, and the new
        // document reuses the same id space.
        #[cfg(feature = "screenshot")]
        {
            st.scroll_offset = (0.0, 0.0);
            st.image_bytes.borrow_mut().clear();
            st.image_order.borrow_mut().clear();
            st.css_transitions.borrow_mut().clear();
        }
    }

    pub fn set_url(&self, url: &str) {
        self.state.borrow_mut().url = url.to_string();
    }

    /// Set the document's character encoding (WHATWG canonical name). Backs
    /// `document.characterSet` and the `<a>`/`<area>` URL query encoding
    /// override for legacy-charset documents.
    pub fn set_encoding(&self, encoding: &str) {
        self.state.borrow_mut().encoding = encoding.to_string();
    }

    /// Set the main response's MIME type (lowercased, no parameters). Backs
    /// `document.contentType`. Empty = no Content-Type was delivered.
    pub fn set_content_type(&self, content_type: &str) {
        self.state.borrow_mut().content_type = content_type.to_string();
    }

    pub fn set_title(&self, title: &str) {
        self.state.borrow_mut().title = title.to_string();
    }

    /// Set the source document URL exposed as `document.referrer`
    /// (navigation referrer semantics, upstream edb1785).
    pub fn set_referrer(&self, referrer: &str) {
        self.state.borrow_mut().referrer = referrer.to_string();
    }

    /// Set the Referrer Policy the main response delivered via its
    /// `Referrer-Policy` header; it establishes the document's policy
    /// outright, beating any <meta name=referrer> in the markup.
    pub fn set_referrer_policy(&self, policy: &str) {
        self.state.borrow_mut().referrer_policy_header = policy.to_string();
    }

    #[allow(dead_code)] // CDP Network.setBlockedURLs parity — no CDP client yet
    pub fn set_blocked_urls(&self, patterns: Vec<String>) {
        self.state.borrow_mut().blocked_urls = patterns;
    }

    /// External stylesheet bodies (absolute URL → CSS text) fetched during
    /// navigation. The layout pipeline joins them into the cascade
    /// (getComputedStyle/gBCR see the authored styles) and
    /// `document.styleSheets` builds its rule lists from them.
    pub fn set_ext_sheets(&self, sheets: std::collections::HashMap<String, String>) {
        *self.state.borrow_mut().ext_sheets.borrow_mut() = sheets;
    }

    /// Append real per-request fetch timings for the JS resource-timing
    /// buffer (#126). Called from the navigation pipeline as stylesheet
    /// and script fetches complete; the page's fetch/XHR shims record
    /// their own entries in-page, in the resource clock directly.
    pub fn push_resource_timings(&self, recs: Vec<crate::diting_js::ops::ResourceTimingRecord>) {
        self.state
            .borrow_mut()
            .resource_timings
            .borrow_mut()
            .extend(recs);
    }

    pub fn take_pending_navigation(&self) -> Option<(String, String, String)> {
        self.state.borrow_mut().pending_navigation.take()
    }

    /// Whether page JS ran `document.write()` since the last drain (see
    /// `JsState::pending_write_nav`).
    pub fn take_pending_write_nav(&self) -> bool {
        self.state.borrow_mut().pending_write_nav.replace(false)
    }

    /// Whether any dynamic `<script src>` fetch is still in flight. Dynamic
    /// scripts ride the op-level client cache, invisible to the page-level
    /// http_client's active_requests() counter, so the settle loop asks here
    /// before cutting a page short at its fast-path deadline (upstream
    /// a6bb741).
    pub fn has_pending_dynamic_scripts(&self) -> bool {
        self.state.borrow().dynamic_script_fetches.get() > 0
    }

    #[allow(dead_code)] // CDP Runtime.addBinding drain — emitted as bindingCalled events
    pub fn take_pending_binding_calls(&self) -> Vec<(String, String)> {
        std::mem::take(&mut self.state.borrow_mut().pending_binding_calls)
    }

    /// Drain queued console calls (level, message, page URL at log time)
    /// captured by `op_console_msg`. The CDP layer turns each into a
    /// `Runtime.consoleAPICalled` event.
    pub fn take_pending_console_calls(&self) -> Vec<(String, String, String)> {
        std::mem::take(&mut self.state.borrow_mut().pending_console_calls)
    }

    /// Session-side dialog policy for window.confirm/prompt (see
    /// `JsState::dialog_accept`). `prompt_text` is what prompt() returns when
    /// accepted; None leaves the stored text unchanged so callers can flip
    /// the answer without retyping it.
    pub fn set_dialog_policy(&self, accept: bool, prompt_text: Option<String>) {
        let mut state = self.state.borrow_mut();
        state.dialog_accept = accept;
        if let Some(t) = prompt_text {
            state.dialog_prompt_text = Some(t);
        }
    }

    /// Current dialog policy: (accept, prompt_text).
    pub fn dialog_policy(&self) -> (bool, Option<String>) {
        let state = self.state.borrow();
        (state.dialog_accept, state.dialog_prompt_text.clone())
    }

    /// Wire up the interception channel without enabling interception.
    /// Use set_intercept_enabled separately. The two were entangled before
    /// and every navigation auto-enabled interception, which made
    /// `fetch()` from page JS hang forever waiting for a CDP client to
    /// answer Fetch.requestPaused events that the client never asked for.
    pub fn set_intercept_tx(&self, tx: tokio::sync::mpsc::UnboundedSender<crate::diting_js::ops::InterceptedRequest>) {
        let mut state = self.state.borrow_mut();
        state.intercept_tx = Some(tx);
    }

    /// Enable/disable interception for the wired channel. Kept separate from
    /// `set_intercept_tx` on purpose: the two were entangled once and every
    /// navigation auto-enabled interception, which made `fetch()` from page JS
    /// hang forever waiting for a CDP client to answer Fetch.requestPaused
    /// events the client never asked for. The CDP bridge now drives both —
    /// but only after the client explicitly calls Fetch.enable.
    pub fn set_intercept_enabled(&self, enabled: bool) {
        let mut state = self.state.borrow_mut();
        state.intercept_enabled = enabled;
    }

    /// Attach the owning page's passive network-observer registry, so
    /// script-initiated fetch()/XHR requests fire its on_request/on_response
    /// callbacks (upstream #408). None detaches (bare runtimes).
    pub fn set_callbacks(&self, callbacks: std::sync::Arc<crate::diting_net::CallbackRegistry>) {
        self.state.borrow_mut().callbacks = Some(callbacks);
    }

    /// Retained response body for a script-initiated request, keyed by its
    /// `fetch-{N}` id. See `JsState::network_response_bodies`.
    pub fn get_network_response_body(
        &self,
        request_id: &str,
    ) -> Option<crate::diting_js::ops::StoredNetworkResponseBody> {
        self.state
            .borrow()
            .network_response_bodies
            .get(request_id)
            .cloned()
    }

    pub fn clear_network_response_bodies(&self) {
        let mut state = self.state.borrow_mut();
        state.network_response_bodies.clear();
        state.network_response_body_order.clear();
    }

    /// Drain network events recorded for script-initiated requests into the
    /// owning Page's event list. Idempotent (the queue is taken), so calling
    /// repeatedly never duplicates events.
    #[cfg_attr(not(test), allow(dead_code))] // drained by Page::sync_js_network_events; consumer pending
    pub fn take_js_network_events(&self) -> Vec<crate::diting_js::ops::JsNetworkEvent> {
        std::mem::take(&mut self.state.borrow_mut().js_network_events)
    }
}
