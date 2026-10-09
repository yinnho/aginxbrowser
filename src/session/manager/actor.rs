//! The session thread: the per-session actor owning the Browser + Page,
//! executing the commands [`SessionManager`] dispatches to it. Split from
//! manager.rs (batch 281, issue #193) to keep the manager under the
//! layering audit's god-file cap; behavior unchanged.
//!
//! The loop polls the command channel *while a navigation is in flight*
//! (#193): a default-shape screenshot is answered immediately with the
//! pre-navigation frame from the band cache (Chrome shows the old frame
//! until the new document commits), and every other command parks FIFO in
//! `nav_deferred`, replayed after the load lands.

use super::*;

/// A script-initiated request older than this is not "slow", it is hung —
/// every transport the engine rides carries a 30s read timeout and a 120s
/// sync-XHR cap, so a 13-minute in-flight entry (the #203 taobao submit
/// shape) means the completion never reached JS at all. `hung: true` tells
/// the reading agent that waiting longer is pointless and the page's own
/// handler will never fire — the honest signal before a re-issue decision.
const HUNG_INFLIGHT_AFTER_MS: u64 = 300_000;

/// One `in_flight` row for the Network payload: url/method/age, plus the
/// `hung` verdict once the age crosses [`HUNG_INFLIGHT_AFTER_MS`].
fn in_flight_row(f: &diting::diting_net::InFlightScripted, now_ms: u64) -> serde_json::Value {
    let age_ms = now_ms.saturating_sub(f.dispatched_at_ms);
    json!({
        "url": f.url,
        "method": f.method,
        "age_ms": age_ms,
        "hung": age_ms >= HUNG_INFLIGHT_AFTER_MS,
    })
}

/// Inverse of diting's `parse_http_date` for the export form: epoch seconds
/// → "Wdy, DD Mon YYYY HH:MM:SS GMT". Dates outside the parser's accepted
/// year range surface as None (the entry then round-trips as a session
/// cookie — the old behavior — rather than a broken date).
fn http_date(secs: i64) -> Option<String> {
    if !(0..=253_402_300_799).contains(&secs) {
        return None;
    }
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil_from_days (Hinnant): epoch-day → (year, month, day)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mth <= 2 { y + 1 } else { y };
    // Day 0 of the epoch was a Thursday.
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let mon = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        [(mth - 1) as usize];
    Some(format!("{wd}, {d:02} {mon} {year:04} {h:02}:{m:02}:{s:02} GMT"))
}

/// Drain `pending` web-storage into the current page, origin-gated
/// (issue #141). A blob carrying an `"origin"` key (record- or
/// snapshot-sourced — captured storage always knows where it came from)
/// waits for a page of that origin: about:blank and wrong-site landings
/// leave it pending for the next successful navigation. A blob without
/// `"origin"` (hand-written session_create storage) injects on first
/// landing, the pre-existing contract. Consumed even when the injection
/// itself fails: a retry on the same origin would fail identically, and
/// the session should just start logged out, like any fresh visit.
fn try_inject_storage(page: &mut crate::page::Page, pending: &mut Option<Value>) {
    let Some(blob) = pending else { return };
    let want = blob.get("origin").and_then(Value::as_str).map(str::to_string);
    if let Some(want) = want {
        let here = crate::account::url_origin(&page.url());
        if here.as_deref() != Some(want.as_str()) {
            return;
        }
    }
    if let Some(js) = inject_storage_js(blob) {
        let v = page.evaluate(&js);
        if let Some(n) = v.as_i64() {
            tracing::info!("session: restored {n} storage entries");
        }
    }
    *pending = None;
}

#[allow(clippy::too_many_arguments)]
pub(super) fn session_thread(
    session_id: String,
    start_url: Option<String>,
    use_proxy: bool,
    cookies: Vec<String>,
    storage: Option<Value>,
    account: Option<(String, String)>,
    nav_shared: std::sync::Arc<NavFrameShared>,
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
                // Account sessions build on the account's private jar
                // (account.rs): same-account sessions share it (two tabs,
                // one profile), other accounts and the anonymous shared jar
                // never see this session's cookies. The jar is seeded from
                // the stored record, so a post-restart session_create
                // {account} starts where the last one left off. The
                // account's device persona rides along too (teach-once in
                // the record): its UA goes to the browser, its fp_seed pins
                // the page's JS hardware identity below — one stable device
                // per identity, not one shared fingerprint with two logins.
                let (browser, persona) = match &account {
                    Some((owner, name)) => {
                        let persona = crate::account::persona_for(owner, name, None);
                        (
                            crate::server::build_browser_for_account(
                                use_proxy,
                                "",
                                None,
                                crate::account::jar_for(owner, name),
                                Some(persona.user_agent.as_str()),
                            )
                            .expect("failed to build session browser"),
                            Some(persona),
                        )
                    }
                    None => (
                        crate::server::build_browser(use_proxy, "", None)
                            .expect("failed to build session browser"),
                        None,
                    ),
                };
                // Storage replay (issue #141): the account record's captured
                // local_storage/session_storage rides along when the caller
                // gave none — cookies alone can't carry a localStorage-token
                // login, so post-restart account sessions used to come back
                // logged out. Explicit caller storage always wins.
                let storage = storage.or_else(|| {
                    account
                        .as_ref()
                        .and_then(|(owner, name)| crate::account::record_storage(owner, name))
                });
                // Inject cookies before navigation so a session can start
                // already logged-in (cookies gathered from a prior session via
                // the Cookies command, or hand-exported). Mirrors /fetch. For
                // account sessions the browser's cookie store IS the account
                // jar, so first-time login imports land in the account too.
                if !cookies.is_empty() {
                    let target = start_url.as_deref().unwrap_or("");
                    crate::server::inject_cookies(&browser, &cookies, target);
                }
                let mut page = browser.new_page().await.expect("failed to create session page");

                // Pin the persona's hardware seed before the first
                // navigation: init_js re-bakes the seed into the JS runtime
                // on every navigation (#269), so a pre-goto pin holds for
                // the session's whole life — screen/dpr/GPU/canvas draw from
                // the account's identity, not a fresh random one per page.
                if let Some(persona) = &persona {
                    page.inner.set_fingerprint_seed(persona.fp_seed);
                }

                // Replay log: every page-changing action this session took,
                // in order. In-memory only, dies with the session; exported
                // explicitly via the Export command. Declared before the
                // initial navigation below (which moves start_url).
                let mut recorder: Vec<RecordedAction> = vec![RecordedAction::Create {
                    url: start_url.clone(),
                    use_proxy,
                    cookies: cookies.clone(),
                    storage: storage.clone(),
                    account: account.as_ref().map(|(_, name)| name.clone()),
                }];

                // Ring buffer of recent page console output, fed by
                // drain_console after every command (see Console command).
                let mut console_ring: std::collections::VecDeque<Value> =
                    std::collections::VecDeque::new();

                // Navigate to start URL if provided.
                // Page budget: every page this session walks counts (initial
                // navigation included). Bounds bulk walking; working one page
                // (state/scroll/eval/typing) stays free. See rate.rs.
                let mut pages_loaded: u32 = 0;
                // The last navigation error, kept until the next navigation
                // succeeds (issue #80): eval/state on a page whose navigation
                // never completed used to answer `result: null` /
                // "non-string" — reading as a broken page instead of a
                // broken navigation. Eval/state now carry this error back so
                // the caller sees the same failure navigate itself reports.
                let mut last_nav_error: Option<String> = None;
                // Document-start preload group (issue #96). Kept session-side
                // so every navigation re-applies it — the page object survives
                // navigations, but re-applying mirrors the CDP face's sync and
                // keeps the group authoritative here.
                let mut preload_scripts: Vec<String> = Vec::new();
                if let Some(url) = start_url {
                    // #84: the creation navigation rides the same builtin
                    // recipes as an explicit Navigate — an agent creating the
                    // session straight onto the site (the reporter's usage)
                    // must not land on an unsigned page while a later
                    // navigate would have been covered.
                    page.inner.set_preload_scripts(
                        crate::session::preload_recipes::effective_scripts(
                            &url,
                            &preload_scripts,
                        ),
                    );
                    match page.goto(&url).await {
                        Ok(()) => pages_loaded += 1,
                        Err(e) => {
                            last_nav_error = Some(format!("navigation failed: {}", e));
                            // session_create is fire-and-forget by design (the
                            // id returns before the thread starts navigating),
                            // so a failed initial navigation must not be
                            // silent: the agent sees it in session_console as
                            // an error entry — same observability contract as
                            // page console output (feedback ⑦ family). The
                            // session stays alive; navigating elsewhere works.
                            tracing::warn!("session: initial navigation failed: {}", e);
                            let ts_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            console_ring.push_back(serde_json::json!({
                                "ts_ms": ts_ms,
                                "level": "error",
                                "text": format!("initial navigation failed: {}", e),
                                "url": url,
                            }));
                            // A failed navigation leaves the page with no JS
                            // runtime (js: None) — every eval would silently
                            // return null while document.URL still serves the
                            // stub string, which reads as "logged-in but
                            // scriptless". Chrome keeps a working JS context
                            // on its error page; land on about:blank for the
                            // same guarantee (live-reproven 2026-09-15: an
                            // x.com session behind an unreachable network
                            // returned Null for every flow script).
                            let _ = page.goto("about:blank").await;
                        }
                    }
                } else {
                    // A real browser tab is never runtime-less: about:blank
                    // carries a full JS context. A never-navigated Page has
                    // `js: None`, and eval fell back to a stub that returned
                    // Null for every script (only literal `document.title`
                    // and `document.URL` were served from Rust state) — an
                    // agent evaluating right after session_create saw silent
                    // nulls (the 0.4.1 report's intermittent `result: null`).
                    // Land on about:blank so the runtime exists from birth.
                    let _ = page.goto("about:blank").await;
                }

                // Inject storage after landing so the entries are scoped to
                // the page's origin — this is what cookies can't carry (the
                // xinzao-class login token lives in localStorage, not the
                // cookie jar). Injection failure is non-fatal: an entry key
                // collision or a storage write error just means the session
                // starts logged out, like any fresh visit. Record-sourced
                // blobs carry their capture origin and wait for a matching
                // landing (see try_inject_storage): a start_url-less session
                // sits on about:blank first, where a one-shot injection used
                // to fire into the void (issue #141's second hole).
                let mut pending_storage = storage;
                try_inject_storage(&mut page, &mut pending_storage);

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
                //
                // After a command the slice stretches to 1.5s: async work the
                // command started (a fetch fired from a submit/click handler,
                // promise chains, timers) needs event-loop turns to progress,
                // and only evaluating synchronously would strand it. The
                // drain lives in the pump arm so it stays cancellation-safe:
                // a queued command preempts it at once. Draining inside the
                // command handler instead blocked the loop for the full 1.5s
                // on every never-idle page — back-to-back commands each paid
                // ~1505ms (measured), which is the "eval takes seconds" class
                // of report.
                let mut drain_budget: u64 = 0;
                // #193: commands that arrived while a navigation owned the
                // page (the Navigate arm serves screenshots from the
                // pre-navigation frame cache and parks everything else
                // here). Replayed in arrival order, ahead of cmd_rx — they
                // were received first.
                let mut pending_replay: std::collections::VecDeque<SessionCommand> =
                    std::collections::VecDeque::new();
                loop {
                    let cmd = if let Some(replay) = pending_replay.pop_front() {
                        replay
                    } else {
                        let cmd = tokio::select! {
                        biased;
                        cmd = cmd_rx.recv() => match cmd {
                            Some(c) => c,
                            None => break,
                        },
                        _ = page.pump_event_loop_slice(if drain_budget > 0 { 1500 } else { 200 }) => {
                            drain_console(&mut page, &mut console_ring);
                            drain_budget = 0;
                            continue;
                        }
                    };
                        cmd
                    };
                    match cmd {
                        SessionCommand::Navigate { url, reply } => {
                            // Budget first (local, free), then the per-domain
                            // rate gate — same stance as the stateless paths.
                            //
                            // #193: the load runs to completion inside this
                            // arm, but a biased select below keeps answering
                            // the command channel while it does. A
                            // default-shape screenshot poll is served the
                            // pre-navigation frame (Chrome keeps the old
                            // frame until the new document commits — a poll
                            // that instead hangs behind the load reads as a
                            // frozen browser); every other command parks in
                            // FIFO and replays after the load lands, in
                            // arrival order — the same order the cmd_rx
                            // queue gave them before this arm existed.
                            let mut nav_deferred: Vec<SessionCommand> = Vec::new();
                            let result = match crate::rate::check_page_budget(pages_loaded)
                                .and_then(|_| crate::rate::check_domain(&url))
                            {
                                Err(reason) => Err(reason),
                                Ok(()) => {
                                    // #84: builtin recipes for this host
                                    // ride ahead of the user group at
                                    // document-start; the names are
                                    // echoed in the nav response so a
                                    // caller can see what was applied.
                                    let builtin_preloads = crate::session::preload_recipes::builtin_names(&url);
                                    page.inner.set_preload_scripts(crate::session::preload_recipes::effective_scripts(&url, &preload_scripts));
                                    #[cfg(feature = "screenshot")]
                                    let nav_frame = page.band_frame_cache.clone();
                                    #[cfg(feature = "screenshot")]
                                    let nav_frame_url = page.url();
                                    // #195: publish the pre-navigation state and
                                    // raise the in-flight flag BEFORE the load
                                    // starts — from here the V8 thread may sit
                                    // inside synchronous page JS for seconds,
                                    // beyond this loop's polling reach (the
                                    // in-arm serve below still answers while
                                    // goto awaits the network; the sender side
                                    // covers the script-held stretch).
                                    #[cfg(feature = "screenshot")]
                                    nav_shared.begin(nav_frame_url.clone(), nav_frame.clone());
                                    // Scoped so the pinned goto (which holds
                                    // &mut page) drops before the post-landing
                                    // bookkeeping borrows the page again.
                                    let nav_result = {
                                        let mut goto = std::pin::pin!(page.goto(&url));
                                        loop {
                                            tokio::select! {
                                                biased;
                                                r = &mut goto => break r,
                                                incoming = cmd_rx.recv() => {
                                                    match incoming {
                                                        // Channel closed: the session is dropping.
                                                        // Finish the load so the page state is
                                                        // consistent; the replies go nowhere.
                                                        None => break goto.as_mut().await,
                                                        #[cfg(feature = "screenshot")]
                                                        Some(SessionCommand::Screenshot { width: None, height: None, full_page: false, selector: None, selector_all: false, dpr, reply }) => {
                                                            // Same shape and dpr as the cached frame:
                                                            // serve the pre-navigation PNG. Any other
                                                            // request (explicit size, full_page,
                                                            // selector, different dpr) needs the
                                                            // page and parks like everything else.
                                                            let scale = crate::session::screenshot::resolve_dpr(dpr);
                                                            match nav_frame.as_ref().filter(|f| f.sig.7 == scale) {
                                                                Some(f) => {
                                                                    let _ = reply.send(Ok(crate::session::screenshot::cached_frame_json(&nav_frame_url, f.width, f.height, &f.png)));
                                                                }
                                                                None => nav_deferred.push(SessionCommand::Screenshot { width: None, height: None, full_page: false, selector: None, selector_all: false, dpr, reply }),
                                                            }
                                                        }
                                                        Some(other) => nav_deferred.push(other),
                                                    }
                                                }
                                            }
                                        }
                                    };
                                    // #195: the load is over (landed or failed)
                                    // — sender-side old-frame answers stand
                                    // down; polls go through the channel again.
                                    nav_shared.end();
                                    match nav_result {
                                        Ok(()) => {
                                            let final_url = page.url();
                                            let title = page
                                                .evaluate("document.title")
                                                .as_str()
                                                .filter(|s| !s.is_empty())
                                                .map(|s| s.to_string());
                                            let challenge = crate::har::challenge_kind(&final_url)
                                                .map(|s| s.to_string());
                                            element_map.clear();
                                            pages_loaded += 1;
                                            last_nav_error = None;
                                            // A pending origin-gated storage
                                            // blob may belong to exactly this
                                            // landing (issue #141): the
                                            // start_url-less session that
                                            // idled on about:blank, or a
                                            // login wizard's Navigate to the
                                            // captured site.
                                            try_inject_storage(&mut page, &mut pending_storage);
                                            Ok(SessionNavResponse {
                                                url: final_url,
                                                title,
                                                challenge,
                                                redirected_from: page.inner.redirect_chain.clone(),
                                                preloads: builtin_preloads.iter().map(|s| s.to_string()).collect(),
                                            })
                                        }
                                        Err(e) => {
                                            let msg = format!("navigation failed: {}", e);
                                            last_nav_error = Some(msg.clone());
                                            Err(msg)
                                        }
                                    }
                                }
                            };
                            recorder.push(RecordedAction::Navigate {
                                ok: result.is_ok(),
                                url,
                            });
                            let _ = reply.send(result);
                            pending_replay.extend(nav_deferred);
                        }

                        SessionCommand::SetPreload { scripts, reply } => {
                            preload_scripts = scripts;
                            page.inner.set_preload_scripts(preload_scripts.clone());
                            let _ = reply.send(Ok(serde_json::json!({
                                "count": preload_scripts.len(),
                            })));
                        }

                        SessionCommand::SetContent { html, reply } => {
                            // Local bytes on the same load path as a
                            // navigation: base64 data URL (no escaping
                            // games with # , % in the HTML), decoded by
                            // the nav layer. pages_loaded stays put —
                            // nothing walked the network, so the page
                            // budget doesn't spend here.
                            use base64::{engine::general_purpose::STANDARD, Engine as _};
                            let url = format!(
                                "data:text/html;base64,{}",
                                STANDARD.encode(html.as_bytes())
                            );
                            page.inner.set_preload_scripts(preload_scripts.clone());
                            // #195: a data: URL parses inline scripts on the
                            // same V8 thread — a long script in setContent
                            // HTML holds the actor exactly like a network
                            // navigation, so publish + flag around it too.
                            #[cfg(feature = "screenshot")]
                            nav_shared.begin(page.url(), page.band_frame_cache.clone());
                            let result = match page.goto(&url).await {
                                Ok(()) => {
                                    let title = page
                                        .evaluate("document.title")
                                        .as_str()
                                        .filter(|s| !s.is_empty())
                                        .map(|s| s.to_string());
                                    element_map.clear();
                                    last_nav_error = None;
                                    Ok(serde_json::json!({
                                        "bytes": html.len(),
                                        "title": title,
                                    }))
                                }
                                Err(e) => {
                                    let msg = format!("setContent failed: {}", e);
                                    last_nav_error = Some(msg.clone());
                                    Err(msg)
                                }
                            };
                            // #195: same stand-down as the Navigate arm.
                            nav_shared.end();
                            recorder.push(RecordedAction::SetContent {
                                ok: result.is_ok(),
                                html,
                            });
                            let _ = reply.send(result);
                        }

                        SessionCommand::State { reply } => {
                            element_map.clear();
                            let result = extract_indexed_state(&mut page, &mut element_map)
                                .map_err(|e| match &last_nav_error {
                                    // #80: a failed navigation is the likely
                                    // cause of a non-string extraction (no JS
                                    // runtime on the fallback page) — say the
                                    // navigation error instead of a stringness
                                    // complaint the caller can't act on.
                                    Some(nav) => format!(
                                        "{} — the last navigation did not complete, \
                                         the session is on the fallback page \
                                         (state extraction: {})",
                                        nav, e
                                    ),
                                    None => e,
                                });
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
                                        .inspect(|resp| {
                                            if resp.url != before {
                                                pages_loaded += 1;
                                            }
                                        })
                                }
                            };
                            recorder.push(RecordedAction::Click { index, ok: result.is_ok() });
                            let _ = reply.send(result);
                        }

                        SessionCommand::Input { index, text, full_events, reply } => {
                            let result = input_by_index(&mut page, &element_map, index, &text, full_events);
                            // A filled Enter can submit the form implicitly
                            // (#194) and a change listener can navigate by
                            // itself — drain the queued navigation so the
                            // next command sees the landing page, same
                            // contract as the click path.
                            let filled = result
                                .as_ref()
                                .map(|v| v.get("filled").and_then(Value::as_bool).unwrap_or(false))
                                .unwrap_or(false);
                            if filled {
                                let _ = page.process_pending_navigation().await;
                            }
                            recorder.push(RecordedAction::Input {
                                index,
                                text,
                                ok: filled,
                            });
                            let _ = reply.send(result);
                        }

                        SessionCommand::SetFiles { selector, files, reply } => {
                            let result = set_files_by_selector(&mut page, selector.as_deref(), &files);
                            recorder.push(RecordedAction::SetFiles {
                                selector,
                                names: files
                                    .iter()
                                    .filter_map(|f| f.get("name").and_then(Value::as_str).map(str::to_owned))
                                    .collect(),
                                ok: result
                                    .as_ref()
                                    .map(|v| v.get("set").and_then(Value::as_bool).unwrap_or(false))
                                    .unwrap_or(false),
                            });
                            let _ = reply.send(result);
                        }

                        SessionCommand::ClickXY { x, y, button, click_count, reply } => {
                            // A coordinate click can navigate exactly like an
                            // indexed one, so it spends the same page budget.
                            // #236: an eval error in the dispatch chain IS the
                            // result — "coordinates accepted" no longer masquerades
                            // as "event chain actually dispatched".
                            let result = match crate::rate::check_page_budget(pages_loaded) {
                                Err(reason) => Err(reason),
                                Ok(()) => {
                                    let before = page.url();
                                    match click_xy(&mut page, x, y, &button, click_count).await {
                                        Err(stage) => Err(format!("click dispatch incomplete: {stage}")),
                                        Ok(()) => {
                                            let _ = page.process_pending_navigation().await;
                                            if page.url() != before {
                                                pages_loaded += 1;
                                            }
                                            Ok(serde_json::json!({
                                                "url": page.url(),
                                                "x": x,
                                                "y": y,
                                            }).to_string())
                                        }
                                    }
                                }
                            };
                            recorder.push(RecordedAction::ClickXY { x, y, ok: result.is_ok() });
                            let _ = reply.send(result);
                        }

                        SessionCommand::Drag { from_x, from_y, to_x, to_y, steps, delay_ms, humanize, reply } => {
                            let steps = steps.clamp(1, 200);
                            let delay_ms = delay_ms.min(1000);
                            let result = match crate::rate::check_page_budget(pages_loaded) {
                                Err(reason) => Err(reason),
                                Ok(()) => {
                                    let before = page.url();
                                    match drag_xy(&mut page, from_x, from_y, to_x, to_y, steps, delay_ms, humanize).await {
                                        Err(stage) => Err(format!("drag dispatch incomplete: {stage}")),
                                        Ok(()) => {
                                            let _ = page.process_pending_navigation().await;
                                            if page.url() != before {
                                                pages_loaded += 1;
                                            }
                                            Ok(serde_json::json!({
                                                "url": page.url(),
                                                "from": {"x": from_x, "y": from_y},
                                                "to": {"x": to_x, "y": to_y},
                                                "steps": steps,
                                                "humanized": humanize,
                                            }).to_string())
                                        }
                                    }
                                }
                            };
                            recorder.push(RecordedAction::Drag { from_x, from_y, to_x, to_y, steps });
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

                        SessionCommand::GetViewport { reply } => {
                            let _ = reply.send(Ok(page.viewport_override()));
                        }

                        SessionCommand::Viewport { width, height, mobile, reply } => {
                            // Unspecified dimensions keep the current ones
                            // (Chrome's setDeviceMetricsOverride rule), so
                            // read the live values before overriding.
                            let probe = "(function(){return [innerWidth, innerHeight]})()";
                            let current = {
                                let v = page.evaluate_with_timeout(probe, crate::page::INTERACTION_EVAL_TIMEOUT);
                                match (v.get(0).and_then(Value::as_f64), v.get(1).and_then(Value::as_f64)) {
                                    (Some(w), Some(h)) if w > 0.0 && h > 0.0 => (w as f32, h as f32),
                                    _ => (1920.0, 1000.0),
                                }
                            };
                            let w = width.map(|v| v as f32).unwrap_or(current.0);
                            let h = height.map(|v| v as f32).unwrap_or(current.1);
                            page.set_viewport_override(w, h, mobile, None);
                            let val = serde_json::json!({
                                "width": w as u32,
                                "height": h as u32,
                                "mobile": mobile,
                            });
                            recorder.push(RecordedAction::Viewport { width: Some(w as u32), height: Some(h as u32), mobile });
                            let _ = reply.send(Ok(val));
                        }

                        SessionCommand::Eval { script, timeout_ms, reply } => {
                            let outcome = if !page.inner.has_js() {
                                // #80: without a JS runtime every eval would
                                // silently answer null (only document.title /
                                // URL are stub-served) — a navigation that
                                // "succeeded" into an error page leaves the
                                // page runtime-less. Say it instead of null.
                                Err(format!(
                                    "page has no JS runtime — the last navigation did not \
                                     complete{}; navigate again",
                                    last_nav_error
                                        .as_deref()
                                        .map(|nav| format!(" ({})", nav))
                                        .unwrap_or_default()
                                ))
                            } else {
                                page.evaluate_async_checked(&script, timeout_ms).await
                            };
                            // A null result on the post-failure fallback page
                            // (about:blank after a failed goto) reads as
                            // "page is broken"; the navigation error is the
                            // actionable truth, so surface it (#80). Real
                            // values pass through untouched — the fallback
                            // page is alive and the script genuinely ran.
                            let outcome = match outcome {
                                Ok(v) if v.is_null() && last_nav_error.is_some() => Err(format!(
                                    "{} — the last navigation did not complete, \
                                     the session is on the fallback page; navigate again",
                                    last_nav_error.as_deref().unwrap_or_default()
                                )),
                                other => other,
                            };
                            recorder.push(RecordedAction::Eval { script });
                            // Drain any JS-initiated navigation the script
                            // started (location.href / form submit) so the
                            // session's current URL moves with it — same
                            // policy as click_by_index below.
                            let _ = page.process_pending_navigation().await;
                            let _ = reply.send(outcome.map_err(SessionError::Eval));
                        }

                        SessionCommand::Url { reply } => {
                            let _ = reply.send(Ok(page.url()));
                        }

                        SessionCommand::Storage { reply } => {
                            let collect = "(function(){ var ls={}, ss={}; \
                                try { for (var i=0;i<localStorage.length;i++){ var k=localStorage.key(i); ls[k]=localStorage.getItem(k); } } catch(e) {} \
                                try { for (var i=0;i<sessionStorage.length;i++){ var k=sessionStorage.key(i); ss[k]=sessionStorage.getItem(k); } } catch(e) {} \
                                return {local_storage: ls, session_storage: ss}; })()";
                            let v = page.evaluate(collect);
                            let payload = serde_json::json!({
                                "url": page.url(),
                                "local_storage": v.get("local_storage").cloned().unwrap_or(serde_json::json!({})),
                                "session_storage": v.get("session_storage").cloned().unwrap_or(serde_json::json!({})),
                            });
                            let _ = reply.send(Ok(payload.to_string()));
                        }

                        SessionCommand::Console { filter, reply } => {
                            drain_console(&mut page, &mut console_ring);
                            let total = console_ring.len();
                            let mut messages: Vec<Value> = console_ring
                                .iter()
                                .filter(|e| filter.matches(e))
                                .cloned()
                                .collect();
                            let matched = messages.len();
                            if let Some(cap) = filter.limit {
                                if messages.len() > cap {
                                    messages = messages.split_off(messages.len() - cap);
                                }
                            }
                            let payload = serde_json::json!({
                                "url": page.url(),
                                "total": total,
                                "matched": matched,
                                "messages": messages,
                            });
                            let _ = reply.send(Ok(payload.to_string()));
                        }

                        SessionCommand::Dialog { action, prompt_text, reply } => {
                            let out = match action.as_str() {
                                "list" => {
                                    drain_console(&mut page, &mut console_ring);
                                    let (accept, prompt_text) = page.inner.dialog_policy();
                                    let dialogs: Vec<Value> = console_ring
                                        .iter()
                                        .filter(|e| e["level"] == "dialog")
                                        .cloned()
                                        .collect();
                                    Ok(serde_json::json!({
                                        "policy": if accept { "accept" } else { "dismiss" },
                                        "prompt_text": prompt_text,
                                        "dialogs": dialogs,
                                    }).to_string())
                                }
                                "accept" | "dismiss" => {
                                    page.inner.set_dialog_policy(action == "accept", prompt_text);
                                    let (accept, prompt_text) = page.inner.dialog_policy();
                                    Ok(serde_json::json!({
                                        "policy": if accept { "accept" } else { "dismiss" },
                                        "prompt_text": prompt_text,
                                    }).to_string())
                                }
                                other => Err(format!(
                                    "unknown dialog action {:?} (expected list | accept | dismiss)",
                                    other
                                )),
                            };
                            let _ = reply.send(out);
                        }

                        SessionCommand::Cookies { reply } => {
                            let url_str = page.url();
                            // Full Set-Cookie form (Domain/flags included) so
                            // snapshot/clone restores cross-subdomain login
                            // state: bare name=value pairs re-anchor at
                            // whatever URL the new session opens, losing
                            // every sibling-domain cookie.
                            let cookies: Vec<String> = page
                                .context
                                .cookie_jar
                                .get_all_cookies()
                                .into_iter()
                                .map(|c| {
                                    let mut s = format!(
                                        "{}={}; Domain={}; Path={}",
                                        c.name, c.value, c.domain, c.path
                                    );
                                    if let Some(exp) = c.expires {
                                        // #203 (taobao half): the export IS
                                        // the account record / snapshot
                                        // format — dropping Expires turned
                                        // every revived session into a
                                        // replay of server-retired tokens
                                        // (sessionExpired + havana bounce).
                                        if let Some(date) = http_date(exp) {
                                            s.push_str(&format!("; Expires={date}"));
                                        }
                                    }
                                    if c.secure {
                                        s.push_str("; Secure");
                                    }
                                    if c.http_only {
                                        s.push_str("; HttpOnly");
                                    }
                                    if !c.same_site.is_empty() {
                                        s.push_str(&format!("; SameSite={}", c.same_site));
                                    }
                                    s
                                })
                                .collect();
                            let resp = serde_json::json!({ "url": url_str, "cookies": cookies });
                            let _ = reply.send(Ok(resp.to_string()));
                        }

                        SessionCommand::CookieMeta { reply } => {
                            // #102: metadata-only view — which cookies exist
                            // and how they're scoped (domain/path/flags/
                            // expiry), never the values. Cookie values are
                            // credentials; the account face already holds
                            // that line (AccountSummary), this is the same
                            // discipline one layer down.
                            let url_str = page.url();
                            let cookies = page.context.cookie_jar.get_all_cookie_metadata();
                            let resp = serde_json::json!({ "url": url_str, "cookies": cookies });
                            let _ = reply.send(Ok(resp.to_string()));
                        }

                        SessionCommand::CookieTrace { reply } => {
                            // #203: the mutation ring — the metadata-only
                            // answer to "which response deleted unb/sn",
                            // "which redirect hop minted the SSO cookie".
                            // Read-only, values never enter the rows.
                            let url_str = page.url();
                            let trace = page.context.cookie_jar.cookie_trace();
                            let resp = serde_json::json!({
                                "url": url_str,
                                "total": trace.len(),
                                "trace": trace,
                            });
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

                        SessionCommand::Network { media_only, include_bodies, include_headers, url_contains, body_max_chars, reply } => {
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
                                let body_of = |rid: &str| page.inner.get_response_body(rid);
                                let mut payload = serde_json::json!({
                                    "url": page.url(),
                                    // Current navigation generation (#101):
                                    // rows with a lower "nav" below are
                                    // earlier attempts' leftovers.
                                    "nav": page.inner.navigation_epoch,
                                    "total": events.len(),
                                    "requests": crate::har::compact_events(events, include_headers),
                                });
                                // #116: hung script-initiated fetches are
                                // invisible in the request log (entries land
                                // at completion) — surface what is still
                                // flying so a timed-out caller can tell a
                                // slow save from a dead one before re-issuing.
                                // #130: the key is always present, `[]` when
                                // quiet — an absent field is indistinguishable
                                // from a wrong endpoint / stale version for
                                // the reading agent, so "zero in flight"
                                // must read as an empty array, not silence.
                                let in_flight = page.inner.scripted_in_flight();
                                let now_ms = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);
                                payload["in_flight"] = json!(
                                    in_flight.iter().map(|f| in_flight_row(f, now_ms)).collect::<Vec<_>>()
                                );
                                // Anti-bot challenges answer 200, so they
                                // hide among successful rows — surface the
                                // count at the top level so an agent that
                                // just asks "did we get punished" doesn't
                                // have to scan every URL. Same detection as
                                // the Challenges command (URL shape + the
                                // MTop risk-control bodies), so the numbers
                                // agree. Same always-present contract as
                                // in_flight (#130).
                                let challenges =
                                    crate::har::challenge_rows(events, &body_of).len();
                                payload["challenges"] = json!(challenges);
                                if include_bodies {
                                    let filters = url_contains
                                        .as_deref()
                                        .map(|s| vec![s.to_string()])
                                        .unwrap_or_default();
                                    payload["xhr"] = json!(crate::har::xhr_bodies(
                                        events, &filters, body_max_chars, include_headers, &body_of
                                    ));
                                }
                                payload
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

                        SessionCommand::Screenshot { width, height, full_page, selector, selector_all, dpr, reply } => {
                            #[cfg(feature = "screenshot")]
                            {
                                let result = crate::session::screenshot::screenshot(
                                    &mut page,
                                    width,
                                    height,
                                    full_page,
                                    selector.as_deref(),
                                    selector_all,
                                    dpr,
                                )
                                .await;
                                let _ = reply.send(result);
                            }
                            #[cfg(not(feature = "screenshot"))]
                            {
                                let _ = (
                                    &width, &height, &full_page, &selector, &selector_all, &dpr,
                                );
                                let _ = reply.send(Err(
                                    "session screenshot requires the `screenshot` feature"
                                        .to_string(),
                                ));
                            }
                        }

                        SessionCommand::Wait { selector, predicate, timeout_ms, reply } => {
                            let result = if selector.is_some() == predicate.is_some() {
                                Err("wait takes exactly one of `selector` or `predicate`"
                                    .to_string())
                            } else {
                                crate::session::interact::wait(
                                    &mut page,
                                    selector.as_deref(),
                                    predicate.as_deref(),
                                    timeout_ms,
                                )
                                .await
                            };
                            let _ = reply.send(result);
                        }

                        SessionCommand::Challenges { reply } => {
                            page.inner.sync_js_network_events();
                            let events = &page.inner.network_events;
                            let body_of = |rid: &str| page.inner.get_response_body(rid);
                            let rows = crate::har::challenge_rows(events, &body_of);
                            let mut payload = serde_json::json!({
                                "url": page.url(),
                                "total": rows.len(),
                                "events": rows,
                            });
                            if !rows.is_empty() {
                                // Which identity got walled matters as much as
                                // the wall itself — a multi-account run wants
                                // to know whether the scraper or the publisher
                                // hit risk control.
                                if let Some((_, name)) = &account {
                                    payload["account"] = json!(name);
                                }
                                // Human handoff (taobao 0.4.1 report P1-6):
                                // the engine detects and surfaces, it does not
                                // auto-bypass. A person opens the live view
                                // (served by the binary at /live), solves the
                                // slider in this session, and the retry below
                                // rides the x5sec cookie that solving sets.
                                payload["handoff"] = json!(format!(
                                    "anti-bot wall detected — hand this session to a human: open \
                                     /live?session={session_id} in a browser (this engine's HTTP \
                                     port; clicks, drags and typing land on the real page), solve \
                                     the challenge there, then retry the same request in this session"
                                ));
                            } else {
                                // #102: zero known signatures is not an auth
                                // verdict. `total: 0` reads as "passed" to a
                                // caller who can't see the detector's scope —
                                // say in-band that it means "no wall fired",
                                // and where the login question is actually
                                // answered.
                                payload["note"] = json!(
                                    "no known challenge signatures in this session's traffic — \
                                     absence of a wall, not a login verdict; check auth state \
                                     separately (session_verdict / account_verify)"
                                );
                            }
                            let _ = reply.send(Ok(payload.to_string()));
                        }

                        SessionCommand::Verdict { reply } => {
                            // The fact sheet's inputs, all eval-free: the
                            // risk-control rows, the main document's
                            // status/size, console errors, and the current
                            // URL. The classification itself is pure code
                            // (verdict.rs) — no model, no page scripts.
                            let started = std::time::Instant::now();
                            drain_console(&mut page, &mut console_ring);
                            page.inner.sync_js_network_events();
                            let events = &page.inner.network_events;
                            let body_of = |rid: &str| page.inner.get_response_body(rid);
                            let rows = crate::har::challenge_rows(events, &body_of);
                            let doc = events
                                .iter()
                                .rev()
                                .find(|e| e.resource_type == "Document" && e.status != 0);
                            let console_errors: Vec<&Value> = console_ring
                                .iter()
                                .filter(|e| e["level"] == "error")
                                .collect();
                            let console_last: Vec<String> = console_errors
                                .iter()
                                .rev()
                                .take(3)
                                .rev()
                                .filter_map(|e| {
                                    e["text"].as_str().map(|s| {
                                        s.chars().take(160).collect::<String>()
                                    })
                                })
                                .collect();
                            let input = crate::verdict::VerdictInput {
                                url: &page.url(),
                                session_id: &session_id,
                                account: account.as_ref().map(|(_, name)| name.as_str()),
                                challenge_events: rows.len(),
                                doc_status: doc.map(|e| e.status),
                                doc_bytes: doc.map(|e| e.body_size),
                                requests: events.len(),
                                console_errors: console_errors.len(),
                                console_last,
                            };
                            let payload = crate::verdict::fact_sheet(
                                &input,
                                started.elapsed().as_millis() as u64,
                            );
                            let _ = reply.send(Ok(payload.to_string()));
                        }

                        SessionCommand::Close { reply } => {
                            let _ = reply.send(());
                            break;
                        }
                    }

                    // The post-command drain happens in the pump arm above
                    // (stretched slice, preemptible by the next command).
                    drain_console(&mut page, &mut console_ring);
                    drain_budget = 1500;
                }
            })
            .await;
    });
}


#[cfg(test)]
mod tests {
    use super::{http_date, in_flight_row, HUNG_INFLIGHT_AFTER_MS};
    use diting::diting_net::InFlightScripted;

    /// The #203 taobao submit shape: an entry aged past every engine-side
    /// timeout must read `hung: true` — the reading agent's stop-waiting
    /// signal. A young entry stays `hung: false` even though it is already
    /// surfaced (slow ≠ hung).
    #[test]
    fn in_flight_row_flags_entries_older_than_the_hung_threshold() {
        let now = 1_000_000u64;
        let young = in_flight_row(
            &InFlightScripted {
                url: "https://x/slow".into(),
                method: "POST".into(),
                dispatched_at_ms: now - 5_000,
            },
            now,
        );
        assert_eq!(young["hung"], false, "a 5s-old entry is slow, not hung");
        assert_eq!(young["age_ms"], 5_000);

        let hung = in_flight_row(
            &InFlightScripted {
                url: "https://x/submit.htm".into(),
                method: "POST".into(),
                dispatched_at_ms: now - HUNG_INFLIGHT_AFTER_MS,
            },
            now,
        );
        assert_eq!(hung["hung"], true, "age == threshold counts as hung");
        assert_eq!(hung["age_ms"], HUNG_INFLIGHT_AFTER_MS);
    }

    #[test]
    fn http_date_formats_epoch_as_rfc1123_gmt() {
        // Day 0 of the epoch was a Thursday.
        assert_eq!(http_date(0).as_deref(), Some("Thu, 01 Jan 1970 00:00:00 GMT"));
        // Day 53: Monday, 23 Feb 1970 (Jan 1 1970 = Thursday, Jan = 31 days).
        assert_eq!(
            http_date(53 * 86_400).as_deref(),
            Some("Mon, 23 Feb 1970 00:00:00 GMT")
        );
        // Leap-day boundary: 29 Feb 2000 12:34:56 = 951827696.
        assert_eq!(
            http_date(951_827_696).as_deref(),
            Some("Tue, 29 Feb 2000 12:34:56 GMT")
        );
        // Outside the parser's accepted year range → None (round-trips as a
        // session cookie instead of a broken date).
        assert_eq!(http_date(-1), None);
        assert_eq!(http_date(253_402_300_800), None);
    }
}
