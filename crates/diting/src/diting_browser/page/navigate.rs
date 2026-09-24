//! Navigation and settle: the navigate family (document fetch, redirect
//! chains, referrer/history bookkeeping) plus the event-loop settle/pump
//! family sessions drive between actions. Split from page/mod.rs (ARCHITECTURE.md P2 batch 3); behavior unchanged.
use super::*;

impl Page {
    /// Pump the event loop until pending work settles: dynamic external
    /// script fetches land and execute, inflight XHR/fetch resolve, and
    /// timers tick. Bounded twice over — a 500ms fast path for pages with
    /// nothing pending, and `AGINXBROWSER_DYNAMIC_SCRIPT_SETTLE_MS`
    /// (default 3s) while a dynamic script fetch is still in flight, so
    /// normal pages and unrelated fetches retain the fast path (upstream
    /// a6bb741).
    pub(super) async fn settle_pending_work(&mut self) {
        let Some(js) = self.js.as_mut() else { return };
        let dynamic_settle_ms = std::env::var("AGINXBROWSER_DYNAMIC_SCRIPT_SETTLE_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(3_000)
            .max(500);
        // Watchdog follows the 批187 standard (budget + WATCHDOG_HEADROOM_MS):
        // a legitimate SPA commit synchronously mounting editors blocks the
        // loop for seconds past the settle budget (#100/#111 family), and the
        // old +250ms razor terminated it mid-commit, poisoning React's
        // executionContext. A true spin still trips at +5s; the #66
        // duty-cycle freeze remains the sustained-burn backstop.
        let settle_wd = js.arm_watchdog(std::time::Duration::from_millis(
            dynamic_settle_ms + JsRuntime::WATCHDOG_HEADROOM_MS,
        ));
        let started = tokio::time::Instant::now();
        let deadline = started + tokio::time::Duration::from_millis(500);
        let dynamic_deadline = started + tokio::time::Duration::from_millis(dynamic_settle_ms);
        let mut idle_count = 0u32;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline && (now >= dynamic_deadline || !js.has_pending_dynamic_scripts()) {
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
    pub(super) fn navigation_timeout(&self) -> tokio::time::Duration {
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

    pub(super) async fn navigate_with_wait_post_ref(
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
        if self.busy_frozen {
            // #66: the busy window already tripped for this realm. Park the
            // full slice without re-entering V8 — the storm's callbacks are
            // what's burning the core, and each one stays under every
            // watchdog budget, so termination-based defenses never see it.
            // Commands still preempt via the session select!; a Navigate or
            // SetContent rebuilds the realm and unfreezes.
            tokio::time::sleep_until(deadline).await;
            return;
        }
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
            if self.track_busy_window() {
                // Frozen mid-slice: stop feeding the loop for this slice.
                return;
            }
            if idle {
                // Quiescent: park for the rest of the slice. The session
                // command loop races this against command arrival, so a
                // new command preempts the park immediately.
                tokio::time::sleep_until(deadline).await;
                return;
            }
        }
    }

    /// #66 busy accounting for the idle pump: diff the runtime's cumulative
    /// `v8_active_ns` over a trailing window of `js_busy_limit_secs()`
    /// (default 30s, 0 disables). Freeze when ≥40% of the window's wall
    /// time was spent inside `poll_event_loop` — that is a realm whose JS
    /// never yields and never overruns, i.e. a burner tuned to sit under
    /// every watchdog budget. Returns `true` when this call tripped the
    /// freeze. Parked-waiting time (a page polling on a slow timer) doesn't
    /// count toward the budget, so ordinary self-refreshing pages stay up.
    fn track_busy_window(&mut self) -> bool {
        let limit_secs = crate::env_knobs::js_busy_limit_secs();
        if limit_secs == 0 {
            return false;
        }
        let Some(js) = &self.js else { return false };
        let now = tokio::time::Instant::now();
        let active = js.v8_active_ns();
        let Some((t0, a0)) = self.busy_mark else {
            self.busy_mark = Some((now, active));
            return false;
        };
        let elapsed = now - t0;
        if elapsed < tokio::time::Duration::from_secs(limit_secs) {
            return false;
        }
        let gained = active.saturating_sub(a0);
        // 40%, not 80: an interval burner's in-engine duty tops out around
        // 46-60% (measured — each 40ms tick lands ~78ms apart once timer
        // dispatch latency is paid), so a peak-shaped threshold never fires
        // on the very storm it exists for. The signal is SUSTAINED duty:
        // two-fifths of a 30s window inside poll_event_loop is half a core
        // burned for half a minute, which no legitimate idle page does.
        let hot = gained as u128 >= elapsed.as_nanos() * 2 / 5;
        self.busy_mark = Some((now, active));
        if hot {
            self.busy_frozen = true;
            let secs = elapsed.as_secs();
            tracing::error!(
                "#66: freezing realm after {secs}s at ≥40% V8 busy \
                 (AGINXBROWSER_JS_BUSY_LIMIT_SECS={limit_secs}); \
                 navigate/setContent to unfreeze"
            );
            if let Some(js) = &self.js {
                js.push_console_entry(
                    "error",
                    "aginxbrowser: JS frozen after sustained busy loop \
                     (see AGINXBROWSER_JS_BUSY_LIMIT_SECS); navigate or \
                     setContent to restart the page",
                );
            }
        }
        hot
    }

    /// Whether the busy window (#66) froze this realm's idle pump. Frozen
    /// realms park instead of re-entering V8; a document swap unfreezes.
    pub fn js_busy_frozen(&self) -> bool {
        self.busy_frozen
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
                    .and_then(|src| Url::parse(&next_url).ok().map(|dst| crate::diting_net::client::HttpClient::navigation_referrer(&src, &dst)))
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

        // The outgoing document's events must outlive this navigation: the
        // CDP drain that emits Network events only runs after the new
        // document settles, `network_events` is reset below, and `init_js`
        // swaps the runtime whose queue still holds script-initiated
        // fetch/XHR events. Sync those into the page-side list now and
        // carry the whole thing across (obscura #920 shape — and broader
        // than the history-jump case there: every navigation dropped them).
        self.sync_js_network_events();
        self.carried_network_url = self.url_string();
        let mut outgoing = std::mem::take(&mut self.network_events);
        self.carried_network_events.append(&mut outgoing);

        self.lifecycle = LifecycleState::Loading;
        self.url = Some(url.clone());

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
            // The submitting document initiates the POST: it is both the
            // Referer (policy-trimmed per hop) and the Origin source.
            let doc_referrer = self.url.as_ref().map(|u| u.to_string());
            self.http_client
                .post_form_with_callbacks(&url, body, Some(&self.callbacks), crate::diting_net::ResourceType::Document, doc_referrer.as_deref())
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
        // Main-response MIME (lowercased, parameters stripped) backs
        // `document.contentType`. An absent Content-Type stays None so the
        // JS layer keeps its URL-sniffing fallback.
        self.content_type = response
            .content_type()
            .map(|ct| ct.split(';').next().unwrap_or("").trim().to_lowercase())
            .filter(|ct| !ct.is_empty());
        // Referrer Policy §"Determine request's Referrer Policy": a policy
        // delivered via the response header wins outright over <meta>. Keep
        // only the last valid comma token (invalid ones are skipped).
        self.referrer_policy_header = response
            .header("referrer-policy")
            .and_then(crate::diting_js::ops::last_valid_referrer_token)
            .unwrap_or_default();
        // A bare SVG document navigated as a document (content-type
        // image/svg+xml; the html5 parser wraps it in html>body>svg):
        // Chrome sizes the root svg at 100%x100% of the initial containing
        // block with no body UA margin. Reproduce that by injecting the
        // effective viewport as width/height attributes on the root svg
        // (author attributes win — only absent ones are filled in) plus a
        // head style resetting the body margin. The replaced-leaf intrinsic
        // arm and the viewBox meet scaling (svg.rs) do the rest. Layout
        // only runs in the screenshot pipeline, so the whole branch gates
        // with it.
        #[cfg(feature = "screenshot")]
        let dom = {
            let is_svg_doc = response
                .content_type()
                .is_some_and(|ct| ct.starts_with("image/svg"));
            if is_svg_doc {
                let dom = parse_html(&format!(
                    "<style>html,body{{margin:0;padding:0}}</style>{body_text}"
                ));
                if let Some(svg_id) = dom.query_selector("svg").ok().flatten() {
                    let (vw, vh) = self.effective_viewport();
                    dom.with_node_mut(svg_id, |n| {
                        if n.get_attribute("width").is_none() {
                            n.set_attribute("width", vw.to_string());
                        }
                        if n.get_attribute("height").is_none() {
                            n.set_attribute("height", vh.to_string());
                        }
                    });
                }
                dom
            } else if self.content_type.as_deref().is_some_and(renders_as_text_document) {
                plain_text_document(&body_text)
            } else {
                parse_html(&body_text)
            }
        };
        #[cfg(not(feature = "screenshot"))]
        let dom = if self.content_type.as_deref().is_some_and(renders_as_text_document) {
            plain_text_document(&body_text)
        } else {
            parse_html(&body_text)
        };

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
            if self.url_blocked(&full_url) {
                tracing::info!("Blocked stylesheet by Network.setBlockedURLs: {}", full_url);
                continue;
            }
            css_fetch_urls.push(full_url);
        }

        let client = self.http_client.clone();
        let css_callbacks = self.callbacks.clone();
        let doc_referrer = self.url.as_ref().map(|u| u.to_string());
        let css_futures: Vec<_> = css_fetch_urls.iter().map(|full_url| {
            let client = client.clone();
            let css_callbacks = css_callbacks.clone();
            let url_str = full_url.clone();
            let doc_referrer = doc_referrer.clone();
            async move {
                let parsed = Url::parse(&url_str).unwrap_or_else(|_| Url::parse("about:blank").unwrap());
                match client
                    .fetch_with_callbacks(&parsed, Some(css_callbacks.as_ref()), crate::diting_net::ResourceType::Stylesheet, doc_referrer.as_deref())
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
        let mut css_sources: Vec<(String, String)> = Vec::new();
        for (url_str, resp) in css_results.into_iter().flatten() {
            // CSS bodies: honor the Content-Type charset; CSS @charset is
            // out of scope for the current scrape-focused pipeline.
            let css = crate::diting_net::decode_non_html(&resp.body, resp.content_type());
            self.record_network_event_with_body(&url_str, "GET", "Stylesheet", resp.status, &resp.headers, &resp.body);
            css_sources.push((url_str, css));
        }

        self.dom = Some(dom);
        self.init_js();

        // Hand the fetched sheet bodies to the JS state natively: the
        // layout run joins them into the cascade (getComputedStyle /
        // getBoundingClientRect / elementFromPoint see authored styles, not
        // initial values) and document.styleSheets builds real rule lists
        // from them. Has to happen before scripts run, regardless of
        // waitUntil, so handlers that read geometry or cssRules mid-boot
        // see the styled document.
        if let Some(js) = &mut self.js {
            let sheets: std::collections::HashMap<String, String> =
                css_sources.iter().cloned().collect();
            js.set_ext_sheets(sheets);
        }
        // Inject CSS as a global so any CSS-aware page shim can read the
        // joined text directly (window.__diting_css).
        if !css_sources.is_empty() {
            if let Some(js) = &mut self.js {
                let combined_css = css_sources
                    .iter()
                    .map(|(_, c)| c.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
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

            // Same hazard as the post-script settle, same 批187 standard: a
            // synchronous commit can pin the thread past the 5s network-idle
            // deadline, so the watchdog arms WATCHDOG_HEADROOM_MS past it
            // instead of the old +500ms razor that killed real commits
            // mid-flight (#100/#111 family).
            let netidle_wd = self.js.as_mut().map(|js| {
                js.arm_watchdog(std::time::Duration::from_millis(
                    5_000 + JsRuntime::WATCHDOG_HEADROOM_MS,
                ))
            });
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
        if let Some(js) = self.js.as_mut() {
            let calls = js.take_pending_console_calls();
            self.suspended_console.extend(calls);
        }
        self.js = None;
        self.url = Some(Url::parse("about:blank").unwrap());
        self.dom = Some(parse_html("<!DOCTYPE html><html><head></head><body></body></html>"));
        self.title = String::new();
        self.content_type = None;
        self.lifecycle = LifecycleState::Loaded;
    }

    pub fn url_string(&self) -> String {
        self.url
            .as_ref()
            .map(|u| u.to_string())
            .unwrap_or_else(|| "about:blank".to_string())
    }
}
