//! Script execution pipeline for the loaded document: inline/external scripts,
//! ES module graphs, module eval budgets and the env init hook. Split from page/mod.rs (ARCHITECTURE.md P2 batch 3); behavior unchanged.
use super::*;

impl Page {
    pub(super) async fn execute_scripts(&mut self) {
        tracing::info!("execute_scripts called, js runtime exists: {}", self.js.is_some());
        // Compute document base URL, respecting <base href>.
        let document_base = self.resolve_base_url();
        // Soft deadline on the fetch phase of script execution. Heavy SPAs
        // (GitHub, Linear, CodeSandbox) ship 50+ scripts and our serial
        // fetch + execute loop can blow past a 25s Puppeteer goto timeout.
        // Override via AGINXBROWSER_SCRIPT_DEADLINE_MS for slow networks.
        // The execution phase gets the same budget AGAIN, armed after the
        // fetch phase (#79) — a network stall that eats the whole fetch
        // deadline must not also behead the CPU phase.
        let script_deadline_ms: u64 = std::env::var("AGINXBROWSER_SCRIPT_DEADLINE_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10_000);
        let script_deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(script_deadline_ms);

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

        // Parser-order script visibility (#120): Chrome executes a classic
        // script before the parser has inserted any LATER script elements, so
        // self-locating loaders (KISSY reverse-scans
        // getElementsByTagName('script') for their own tag) must not see them.
        // Our pipeline parses the whole document first, so we emulate the
        // contract with a JS-side ceiling: script queries drop parser scripts
        // positioned after the running one (bootstrap.js _parserVisibleIds).
        // The order list comes from the DOM script query — every <script>
        // element in the document, including empty/foreign-type ones the
        // execution list below legitimately skips.
        let parser_order: Vec<u32> = match &self.js {
            Some(js) => js
                .with_dom(|dom| {
                    dom.query_selector_all("script")
                        .unwrap_or_default()
                        .into_iter()
                        .map(|sid| sid.raw())
                        .collect::<Vec<u32>>()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let parser_idx: std::collections::HashMap<u32, usize> = parser_order
            .iter()
            .enumerate()
            .map(|(i, nid)| (*nid, i))
            .collect();

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
                if self.url_blocked(&full_url) {
                    tracing::info!("Blocked script by Network.setBlockedURLs: {}", full_url);
                    continue;
                }
                resolved.push((i, full_url.clone()));
                fetch_tasks.push((i, full_url));
            }
        }

        let client = self.http_client.clone();
        let script_callbacks = self.callbacks.clone();
        let doc_referrer = self.url.as_ref().map(|u| u.to_string());
        let fetch_futures: Vec<_> = fetch_tasks.iter().map(|(idx, url)| {
            let client = client.clone();
            let script_callbacks = script_callbacks.clone();
            let url = url.clone();
            let idx = *idx;
            let doc_referrer = doc_referrer.clone();
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
                    return Some((idx, url, resp, None));
                }
                // #126: monotonic anchor for the live-fetch duration below.
                let rt_t0 = std::time::Instant::now();
                match client
                    .fetch_with_callbacks(&parsed, Some(script_callbacks.as_ref()), crate::diting_net::ResourceType::Script, doc_referrer.as_deref())
                    .await
                {
                    Ok(resp) => {
                        // #126: real timing around the live fetch only —
                        // data: URIs never touched the wire, so they carry
                        // no record. Epoch stamp feeds the JS resource
                        // clock; Instant gives the monotonic duration.
                        let rt_start_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs_f64() * 1000.0)
                            .unwrap_or(0.0);
                        let dur_ms = rt_t0.elapsed().as_secs_f64() * 1000.0;
                        Some((idx, url, resp, Some((rt_start_ms, dur_ms))))
                    }
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
        let external_total = fetch_futures.len();
        // #79: count arrivals so a fetch-phase overrun can say how much of
        // the page's script surface landed before the deadline — the item-
        // wise loop below keeps those bodies, and the warn names the count.
        let arrivals = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let arrivals_counter = arrivals.clone();
        let fetch_stream = futures::stream::iter(fetch_futures)
            .buffer_unordered(16)
            .inspect(move |result| {
                if result.is_some() {
                    arrivals_counter.set(arrivals_counter.get() + 1);
                }
            });
        // #79: drive the stream item-wise instead of collect() — collect is
        // all-or-nothing, so one hanging URL used to discard the bodies that
        // had already landed. On deadline we keep whatever arrived; the
        // execution phase below runs those bodies plus every inline script
        // on a fresh budget.
        let mut fetch_stream = std::pin::pin!(fetch_stream);
        let mut fetch_results: Vec<_> = Vec::with_capacity(external_total);
        while tokio::time::Instant::now() < script_deadline {
            match tokio::time::timeout(
                tokio::time::Duration::from_millis(50),
                fetch_stream.next(),
            ).await {
                Ok(Some(item)) => fetch_results.push(item),
                // Stream drained: every fetch future settled or errored out.
                Ok(None) => break,
                // Slice elapsed with work still in flight — re-check the
                // deadline at the top of the loop.
                Err(_) => {}
            }
        }
        if tokio::time::Instant::now() >= script_deadline {
            tracing::warn!(
                "execute_scripts: fetch deadline reached after {}/{} script bodies \
                 arrived; the {} arrived bodies and all inline scripts still execute \
                 on a fresh budget — raise via AGINXBROWSER_SCRIPT_DEADLINE_MS",
                arrivals.get(),
                external_total,
                arrivals.get(),
            );
        }

        let mut fetched: std::collections::HashMap<usize, (String, String, crate::diting_net::Response)> = std::collections::HashMap::new();
        let mut script_timings: Vec<crate::diting_js::ops::ResourceTimingRecord> = Vec::new();
        for result in fetch_results {
            if let Some((idx, url, resp, rt)) = result {
                // Script bodies: only the HTTP Content-Type charset matters
                // (no in-band meta-charset for JS).
                let code = crate::diting_net::decode_non_html(&resp.body, resp.content_type());
                // #126: real per-script timing — only wire fetches carry a
                // sample, and the blocking posture comes from the element
                // itself (defer/async scripts don't block the parser).
                if let Some((start_ms, dur_ms)) = rt {
                    let s = all_to_execute.get(idx);
                    script_timings.push(crate::diting_js::ops::ResourceTimingRecord {
                        name: url.clone(),
                        initiator: "script",
                        start_epoch_ms: start_ms,
                        duration_ms: dur_ms,
                        status: resp.status,
                        body_size: resp.body.len(),
                        render_blocking: !s.is_some_and(|s| s.is_defer || s.is_async),
                    });
                }
                fetched.insert(idx, (url, code, resp));
            }
        }
        if let Some(js) = &mut self.js {
            js.push_resource_timings(script_timings);
        }

        // Spec: readyState is "loading" while parser-discovered scripts execute.
        // Scripts that check readyState === 'loading' will register DOMContentLoaded
        // listeners instead of calling their callback immediately.
        if let Some(js) = &mut self.js {
            let _ = js.execute_script("<ready-state>", "globalThis.__documentReadyState__ = 'loading';");
            // #48: window named access must be live before ANY page script
            // runs — Chrome resolves id/name elements onto the global before
            // the first script, and a script's first statement may be a bare
            // identifier (`hero.focus()`), which never touches `document`.
            // The JS side is idempotent (_namedScanned), so this is a no-op
            // after the first navigation script phase.
            let _ = js.execute_script(
                "<named-boot>",
                "globalThis._namedBoot&&globalThis._namedBoot();",
            );
            // Parser-built stylesheets never pass through the JS insertion
            // hooks (js/bootstrap.js), so their load events only fire if we
            // enumerate them here — before the script loop, so inline scripts
            // that register listeners still catch the queued tasks, matching
            // Chrome's task-after-sheet-applies ordering.
            let _ = js.execute_script(
                "<initial-sheets>",
                "if (typeof __prepareInitialStylesheets === 'function') __prepareInitialStylesheets();",
            );
        }

        // CDP `Page.addScriptToEvaluateOnNewDocument` contract: preload
        // sources must run BEFORE any of the page's own scripts. This is
        // also where puppeteer's `exposeFunction` wrapper installs itself —
        // if preload runs after page scripts, every early binding call
        // hits an undefined function and silently no-ops.
        let preload_sources = self.preload_scripts.clone();
        static ENV_INIT_SCRIPT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        let env_init = ENV_INIT_SCRIPT.get_or_init(env_init_script_source);
        if let Some(js) = &mut self.js {
            for source in &preload_sources {
                if let Err(e) = js.execute_script_guarded("<preload>", source.as_str()) {
                    tracing::debug!("Preload script error: {}", e);
                }
            }
            if let Some(source) = env_init.as_deref() {
                if let Err(e) = js.execute_script_guarded("<init-script>", source) {
                    tracing::debug!("Init script error: {}", e);
                }
            }
            // #120: hand bootstrap.js the document-order parser script list.
            // The ceiling starts at -1 (nothing hidden) and each classic
            // script's prologue bumps it to its own index.
            if let Ok(order) = serde_json::to_string(&parser_order) {
                let _ = js.execute_script(
                    "<parser-order>",
                    &format!(
                        "globalThis.__parserScriptOrder={o};\
                         globalThis.__parserScriptIdx=new Map(globalThis.__parserScriptOrder.map(function(n,i){{return[n,i];}}));\
                         globalThis.__parserScriptCeiling=-1;",
                        o = order
                    ),
                );
            }
        }

        // #79: execution gets its own budget, armed AFTER the fetch phase.
        // The soft deadline bounds the serial classic loop; the watchdog is
        // the hard backstop for inline scripts, which run back-to-back with
        // no await between them, so neither the soft deadline (only checked
        // between scripts) nor the per-script guard can interrupt a page
        // burning the budget across many synchronous scripts (the real-world
        // SPA / anti-bot busy-loop hang — the xiaohongshu jsvmp-scan shape
        // that filed this issue).
        let exec_deadline =
            tokio::time::Instant::now() + tokio::time::Duration::from_millis(script_deadline_ms);
        let mut exec_wd = self
            .js
            .as_mut()
            .map(|js| js.arm_watchdog(std::time::Duration::from_millis(script_deadline_ms + 1000)));

        for (i, script) in all_to_execute.iter().enumerate() {
            // Both exit conditions matter: the wall-clock deadline stops the
            // serial classic phase from eating the whole navigation budget,
            // and a watchdog that already fired means the isolate carries a
            // pending termination — every further guarded execution in this
            // loop would instantly fail, so stop burning scripts on it.
            if tokio::time::Instant::now() >= exec_deadline
                || exec_wd.as_ref().is_some_and(|t| t.fired())
            {
                // #79: name the script that consumed the budget — it is the
                // one just before the first skipped index. "skipping N" alone
                // left a xiaohongshu jsvmp-scan overrun unattributed for an
                // evening of manual bisecting.
                let burner = if i > 0 {
                    all_to_execute
                        .get(i - 1)
                        .map(|s| match s.src.as_deref() {
                            Some(url) => format!("external {url}"),
                            None => format!("inline [nid {}, {} bytes]", s.nid, s.inline.len()),
                        })
                        .unwrap_or_else(|| "unknown".to_string())
                } else {
                    "the fetch phase alone (network)".to_string()
                };
                tracing::warn!(
                    "execute_scripts: classic budget ({}ms, AGINXBROWSER_SCRIPT_DEADLINE_MS) \
                     exhausted after {}; skipping {} remaining scripts",
                    script_deadline_ms,
                    burner,
                    all_to_execute.len() - i,
                );
                break;
            }
            if script.src.is_some() {
                if let Some((url, code, resp)) = fetched.remove(&i) {
                    tracing::info!("Executing script ({} bytes): {}", code.len(), url);
                    self.record_network_event_with_body(&url, "GET", "Script", resp.status, &resp.headers, &resp.body);
                    if let Some(js) = &mut self.js {
                        // #120: classic sync scripts run at their parser
                        // position — later parser scripts are invisible.
                        // Deferred/async run after parsing completes: ceiling
                        // -1 keeps everything visible for them.
                        let ceiling = if script.is_defer || script.is_async {
                            -1
                        } else {
                            parser_idx.get(&script.nid).map(|i| *i as i64).unwrap_or(-1)
                        };
                        let _ = js.execute_script("<current-script>", &format!(
                            "globalThis.__currentScriptNid={nid};globalThis.__parserScriptCeiling={ceiling};",
                            nid = script.nid, ceiling = ceiling));
                        if let Err(e) = js.execute_script_guarded(&url, &code) {
                            tracing::warn!("Script error ({}): {}", url, e);
                            // Chrome parity (#17): the uncaught throw must
                            // reach the window error hooks (onerror +
                            // ErrorEvent('error')) and the console, not just
                            // this host-side log line.
                            let msg = e.strip_prefix("JS error: ").unwrap_or(&e);
                            if let (Ok(m), Ok(s)) =
                                (serde_json::to_string(msg), serde_json::to_string(url.as_str()))
                            {
                                let _ = js.execute_script(
                                    "<window-error>",
                                    &format!(
                                        "globalThis.__diting_reportUncaught({m}, {s}, 0, null);"
                                    ),
                                );
                            }
                        }
                        let _ = js.execute_script("<current-script>", "globalThis.__currentScriptNid=0;");
                    }
                }
            } else if !script.inline.is_empty() {
                let doc_url = self.url_string();
                if let Some(js) = &mut self.js {
                    tracing::info!(
                        "Executing inline script ({} bytes) [nid {}]",
                        script.inline.len(),
                        script.nid
                    );
                    let _ = js.execute_script(
                        "<current-script>",
                        &format!(
                            "globalThis.__currentScriptNid={nid};globalThis.__parserScriptCeiling={ceiling};",
                            nid = script.nid,
                            ceiling = if script.is_defer || script.is_async {
                                -1
                            } else {
                                parser_idx.get(&script.nid).map(|i| *i as i64).unwrap_or(-1)
                            }
                        ),
                    );
                    if let Err(e) = js.execute_script_guarded("<inline>", &script.inline) {
                        tracing::warn!("Inline script error: {}", e);
                        // Same window-error reporting as external scripts
                        // (#17); Chrome attributes inline throws to the
                        // document URL.
                        let msg = e.strip_prefix("JS error: ").unwrap_or(&e);
                        if let (Ok(m), Ok(s)) = (
                            serde_json::to_string(msg),
                            serde_json::to_string(doc_url.as_str()),
                        ) {
                            let _ = js.execute_script(
                                "<window-error>",
                                &format!(
                                    "globalThis.__diting_reportUncaught({m}, {s}, 0, null);"
                                ),
                            );
                        }
                    }
                    let _ = js.execute_script("<current-script>", "globalThis.__currentScriptNid=0;");
                }
            }
        }

        // #120: parsing is "done" for every later phase — deferred scripts,
        // module graphs, DCL/load handlers and the settle loop all run with
        // the full document visible. Also covers the budget-break path, where
        // the last classic script's ceiling would otherwise stay armed.
        if let Some(js) = &mut self.js {
            let _ = js.execute_script("<parser-clear>", "globalThis.__parserScriptCeiling=-1;");
        }

        // Retire the classic-phase watchdog at the phase boundary. When the
        // phase overran (slow scripts, or a spinner the 5s guard killed),
        // the watchdog has already called terminate_execution() and the
        // isolate carries a pending termination: every subsequent
        // execute_script fails instantly, the module phase silently no-ops,
        // and the <load-events> script below never flips readyState or
        // fires DOMContentLoaded — the page is then wedged in "loading"
        // forever (weixin article pages; v0.3.1 Windows report P1-3).
        // disarm_watchdog cancels the termination, healing the isolate for
        // the phases that follow. Chrome semantics: a killed script ends
        // the script, never the document's load lifecycle.
        if let Some(token) = exec_wd.take() {
            if let Some(js) = self.js.as_mut() {
                if js.disarm_watchdog(token) {
                    tracing::warn!(
                        "execute_scripts: classic script phase overran the watchdog; \
                         module + load phases continue on a recovered isolate"
                    );
                }
            }
        }

        // Module phase runs on its own deadline: modules load async (their
        // eval is separately bounded by AGINXBROWSER_MODULE_EVAL_TIMEOUT_MS)
        // and must not inherit a budget already consumed by the classic
        // phase — on pages whose inline scripts legitimately take most of
        // the deadline, the app's entry module never got a chance to load.
        let module_deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(script_deadline_ms);
        for module_script in &module_scripts {
            if tokio::time::Instant::now() >= module_deadline {
                tracing::warn!(
                    "execute_scripts: module-phase deadline reached, skipping remaining module scripts"
                );
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
                    match js.load_module(&full_url, module_eval_budget_ms()).await {
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
                    if let Err(e) =
                    js.load_inline_module(&module_script.inline, &base, module_eval_budget_ms())
                        .await
                {
                        tracing::warn!("Inline ES module error: {}", e);
                    }
                }
            }
        }

        if let Some(js) = &mut self.js {
            // Spec order: readyState -> interactive, fire DOMContentLoaded on
            // both document and window. The window `load` event intentionally
            // does NOT fire here: script elements — parser-inserted and
            // dynamically-inserted alike — delay the load event until they
            // execute (#44), so load waits for the settle loop below. A
            // listener registered by a late-arriving dynamic script
            // (proxydetect-style visual suites hang their whole scheduler on
            // `window.addEventListener("load", …)`) must catch the event.
            // A page's DOMContentLoaded listener can spin exactly like a
            // script can — without a bound it would pin this synchronous
            // execute_script (and the whole session thread) forever. 5s
            // matches the per-script guard.
            let dcl_wd = js.arm_watchdog(std::time::Duration::from_secs(5));
            let _ = js.execute_script("<dcl-events>",
                "globalThis.__documentReadyState__ = 'interactive';\n\
                 try { document.dispatchEvent(new Event('DOMContentLoaded', {bubbles:false,cancelable:false})); } catch(e) {}\n\
                 try { window.dispatchEvent(new Event('DOMContentLoaded', {bubbles:false,cancelable:false})); } catch(e) {}");
            js.disarm_watchdog(dcl_wd);
        }

        // (#44) Give dynamically-inserted scripts their landing window before
        // load fires (script elements delay the load event until they
        // execute, parser-inserted or not).
        self.settle_pending_work().await;

        if let Some(js) = &mut self.js {
            // (#44) Now that the settle loop has let dynamically-inserted
            // scripts land and execute, the load lifecycle closes: readyState
            // flips to complete and load fires. The dispatch fires every
            // handler path exactly once, with a real Event: the window.onload
            // property via __windowOnHandlers, and the `<body onload="...">`
            // content attribute via the body-reflecting fallback in the
            // window dispatch wrapper (the #37 contract — byte-WAF challenge
            // pages drive their whole PoW from `<body onload="readygo()">`).
            // A load listener can spin exactly like a script can, so the 5s
            // watchdog bound carries over.
            let load_wd = js.arm_watchdog(std::time::Duration::from_secs(5));
            let _ = js.execute_script("<load-events>",
                "globalThis.__documentReadyState__ = 'complete';\n\
                 try { window.dispatchEvent(new Event('load', {bubbles:false,cancelable:false})); } catch(e) {}");
            js.disarm_watchdog(load_wd);
        }

        // Load handlers routinely kick off their own async work: the
        // byte-WAF PoW spins a setInterval from `<body onload="readygo()">`
        // that ends in a location.reload(), analytics fire fetches. Before
        // the #44 reorder this work was pumped by the settle loop above
        // (load fired first); now it needs its own bounded window or the
        // JS-triggered navigation chain started from a load handler never
        // lands before the navigation call returns.
        self.settle_pending_work().await;
    }
}
