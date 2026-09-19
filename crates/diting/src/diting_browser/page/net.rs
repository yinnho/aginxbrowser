//! Network bookkeeping: event recording (page-side and drained script-side
//! rows), retained response bodies, scoped on_request/on_response callbacks
//! and Network.setBlockedURLs, plus the identity overrides (extra headers,
//! user-agent override) CDP's Network/Emulation domains drive. Split from
//! page/mod.rs (ARCHITECTURE.md P2 batch 3); behavior unchanged.
use super::*;

impl Page {
    pub fn set_blocked_urls(&mut self, patterns: Vec<String>) {
        self.blocked_urls = patterns.clone();
        if let Some(js) = &self.js {
            js.set_blocked_urls(patterns);
        }
    }

    pub(super) fn record_network_event(
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
            error: None,
        });
        request_id
    }

    /// Record the event and retain the body for `get_response_body`. The
    /// text/base64 split follows the Chromium DevTools body policy (Chrome
    /// 152 verified, obscura #791).
    pub(super) fn record_network_event_with_body(
        &mut self,
        url: &str,
        method: &str,
        resource_type: &str,
        status: u16,
        response_headers: &std::collections::HashMap<String, String>,
        body: &[u8],
    ) -> String {
        let request_id = self.record_network_event(
            url,
            method,
            resource_type,
            status,
            response_headers,
            body.len(),
        );
        self.store_response_body(
            request_id.clone(),
            body,
            response_headers.get("content-type").map(|s| s.as_str()),
        );
        request_id
    }

    fn store_response_body(
        &mut self,
        request_id: String,
        body: &[u8],
        content_type: Option<&str>,
    ) {
        let max_entries = response_body_entry_limit();
        let max_bytes = response_body_byte_limit();
        if max_entries == 0 || max_bytes == 0 || body.len() > max_bytes {
            return;
        }
        let stored = match crate::diting_net::decode_devtools_body(body, content_type) {
            Some(text) => StoredResponseBody {
                body: text,
                base64_encoded: false,
            },
            None => StoredResponseBody {
                body: BASE64.encode(body),
                base64_encoded: true,
            },
        };
        self.response_bodies.insert(request_id.clone(), stored);
        self.response_body_order.push_back(request_id);
        while self.response_body_order.len() > max_entries {
            if let Some(oldest) = self.response_body_order.pop_front() {
                self.response_bodies.remove(&oldest);
            }
        }
    }

    /// Body stored for a request id: page-side (`{page}.{N}`) or
    /// script-initiated (`fetch-{N}`, retained in the JS runtime).
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
    /// The session /network and /har surfaces call this before reading.
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
                error: ev.error,
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

    /// Fan `Network.setExtraHTTPHeaders` out to every transport this page
    /// can issue requests from. A stealth page fetches its main document
    /// through the wreq stealth client (`fetch_document`) while JS
    /// fetch()/XHR keep using the plain reqwest one — writing the extras to
    /// the plain client alone silently drops them from document requests,
    /// which is exactly the header a CDP client set them to control
    /// (obscura #571).
    pub async fn set_extra_headers(&self, headers: std::collections::HashMap<String, String>) {
        self.http_client.set_extra_headers(headers.clone()).await;
        #[cfg(feature = "stealth")]
        if let Some(ref stealth) = self.stealth_client {
            stealth.set_extra_headers(headers).await;
        }
    }

    /// Apply a CDP `setUserAgentOverride` (Network + Emulation domains) to
    /// every surface that advertises an identity: both HTTP transports, and
    /// the live JS persona so navigator.userAgent / platform / fonts follow
    /// immediately instead of on the next navigation. One source of truth
    /// for the whole identity (obscura #481 class). The TLS fingerprint's
    /// OS is baked into the wreq client at construction and is deliberately
    /// not rebuilt here — matching Chrome, where a UA override does not
    /// re-handshake the TLS stack.
    ///
    /// `lang` carries the override's `acceptLanguage`, `platform` its
    /// `platform` field (navigator.platform only — obscura #777 class).
    /// Each sent field applies independently — a locale-only call must not
    /// touch the UA and vice versa (Chrome's only-sent-fields semantics).
    /// The language moves navigator.language immediately and the transport
    /// headers, but never re-pins the ICU default (process-global, sticky
    /// per-isolate — see [`crate::diting_js::runtime`]).
    pub async fn set_user_agent_override(
        &mut self,
        ua: &str,
        lang: Option<&str>,
        platform: Option<&str>,
    ) {
        if !ua.is_empty() {
            self.http_client.set_user_agent(ua).await;
            #[cfg(feature = "stealth")]
            if let Some(ref stealth) = self.stealth_client {
                stealth.set_user_agent(ua).await;
            }
            if let Some(ref mut rt) = self.js {
                rt.set_user_agent(ua);
            }
            // set_user_agent republished the persona viewport; put the
            // override back on top of it.
            self.apply_viewport_override();
            self.apply_emulated_media();
        }
        if let Some(lang) = lang.filter(|l| !l.is_empty()) {
            self.http_client.set_accept_language(lang).await;
            #[cfg(feature = "stealth")]
            if let Some(ref stealth) = self.stealth_client {
                stealth.set_accept_language(lang).await;
            }
            if let Some(ref mut rt) = self.js {
                rt.set_navigator_language(lang);
            }
        }
        if let Some(platform) = platform.filter(|p| !p.is_empty()) {
            if let Some(ref mut rt) = self.js {
                rt.set_navigator_platform(platform);
            }
        }
    }
}
